//! Runtime glue for the four current capabilities.
//!
//! `listen` sends typed input events, the controller authorizes effects,
//! `see` and `think` run in a cancellable worker, and `speak` emits only
//! controller-approved output.

use std::{io, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{
    controller::Controller,
    dictation::{self, DictationAction, DictationSession},
    listen,
    listen::vad::{VadMonitor, VadMonitorAction},
    protocol::{
        CameraPreparation, DecisionId, DecisionRequest, DecisionResult, DictationEvent,
        DictationInput, Effect, Event, Input, TurnFinished, TurnId, TurnRequest, TurnResult,
        VisualRequest,
    },
    see::{
        Photo, PhotoSource,
        camera::{CameraRunError, CameraService, select_camera},
    },
    speak,
    think::{OllamaClient, ThinkError},
};

const EVENT_CHANNEL_CAPACITY: usize = 32;

#[derive(Clone, Copy)]
enum OutputMode {
    Line,
    Raw,
}

impl OutputMode {
    fn emit(self, output: crate::protocol::Output) {
        match self {
            Self::Line => speak::emit(output),
            Self::Raw => speak::emit_raw(output),
        }
    }
}

/// Runs the interactive terminal application until `/quit`, EOF, or Ctrl+C.
pub async fn run() -> Result<(), AppError> {
    let client = OllamaClient::from_environment()?;
    let (event_sender, mut event_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    listen::spawn_stdin_reader(event_sender.clone()).map_err(AppError::InputAdapter)?;

    let mut controller = Controller::new();
    let mut runtime = TurnRuntime::new(client, CameraService::v4l(), event_sender);
    speak::welcome();

    loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => Event::Input(Input::Interrupt),
            event = event_receiver.recv() => event.unwrap_or(Event::Input(Input::EndOfInput)),
        };

        if !apply_main_event(&mut controller, &mut runtime, event, OutputMode::Line).await {
            break;
        }
    }

    runtime.shutdown().await;
    Ok(())
}

/// Runs standalone local dictation without constructing Ollama or camera
/// services. Its transcript deliberately stops at terminal display.
pub async fn run_dictation() -> Result<(), AppError> {
    dictation::run().await.map_err(AppError::Dictation)
}

/// Runs local dictation connected to the voice decision-and-answer controller.
///
/// This owns the raw-key input adapter, while `DictationSession` continues to
/// own microphone and Whisper lifetimes. Only an accepted transcript crosses
/// into the main controller, which decides whether it starts a question or
/// replies to one still-current photo permission request.
pub async fn run_voice() -> Result<(), AppError> {
    let client = OllamaClient::from_environment()?;
    let (main_event_sender, mut main_event_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (dictation_event_sender, mut dictation_event_receiver) =
        mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let mut input = listen::dictation::spawn_dictation_key_reader(dictation_event_sender.clone())
        .map_err(AppError::InputAdapter)?;

    let dictation = DictationSession::from_environment(dictation_event_sender);
    let microphone_label = dictation.microphone_label();
    let microphone_for_display = match &microphone_label {
        Ok(label) => Ok(label.as_str()),
        Err(message) => Err(message.as_str()),
    };
    speak::voice_welcome(
        microphone_for_display,
        dictation.model_path(),
        dictation.model_is_available(),
    );
    speak::emit_dictation(crate::protocol::DictationOutput::Ready);

    let mut runtime = VoiceRuntime::new(
        Controller::new(),
        TurnRuntime::new(client, CameraService::v4l(), main_event_sender),
        dictation,
    );

    loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => VoiceEvent::Dictation(DictationEvent::Input(DictationInput::Interrupt)),
            event = dictation_event_receiver.recv() => VoiceEvent::Dictation(event.unwrap_or(DictationEvent::Input(DictationInput::EndOfInput))),
            event = main_event_receiver.recv() => VoiceEvent::Main(event.unwrap_or(Event::Input(Input::Interrupt))),
        };

        let keep_running = match event {
            VoiceEvent::Dictation(event) => runtime.handle_dictation_event(event).await,
            VoiceEvent::Main(event) => runtime.handle_main_event(event).await,
        };
        if !keep_running {
            break;
        }
    }

    // This adapter owns raw mode. Stop and join it before restoring normal
    // terminal settings, then release both native dictation work and the
    // application-owned inference task.
    input.shutdown();
    runtime.shutdown().await;
    Ok(())
}

/// Runs continuous local acoustic activity diagnostics only.
///
/// This intentionally constructs no Whisper transcriber, Ollama client,
/// application controller, camera service, or permission workflow. It owns the
/// one raw-key reader while active and leaves the monitor's microphone stream
/// under explicit M mute/unmute control.
pub async fn run_vad_monitor() -> Result<(), AppError> {
    let (key_sender, mut key_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (monitor_sender, mut monitor_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let mut monitor = VadMonitor::from_environment(monitor_sender).map_err(AppError::VadMonitor)?;
    let mut input = listen::dictation::spawn_microphone_key_reader(key_sender)
        .map_err(AppError::InputAdapter)?;

    let microphone_label = monitor.microphone_label();
    let microphone_for_display = match &microphone_label {
        Ok(label) => Ok(label.as_str()),
        Err(message) => Err(message.as_str()),
    };
    speak::vad_monitor_welcome(microphone_for_display, &monitor.settings_summary());

    let mut keep_running = apply_vad_monitor_actions(monitor.start());
    let mut health_tick = tokio::time::interval(Duration::from_millis(100));
    health_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    while keep_running {
        let actions = tokio::select! {
            _ = tokio::signal::ctrl_c() => monitor.handle_key(DictationInput::Interrupt),
            input = key_receiver.recv() => monitor.handle_key(input.unwrap_or(DictationInput::EndOfInput)),
            event = monitor_receiver.recv() => match event {
                Some(event) => monitor.handle_event(event),
                None => {
                    keep_running = false;
                    monitor.worker_channel_closed()
                }
            },
            _ = health_tick.tick() => monitor.poll(),
        };
        if !apply_vad_monitor_actions(actions) {
            keep_running = false;
        }
    }

    // The raw reader owns raw mode, so stop it before terminal settings are
    // restored. The monitor then drops CPAL's stream, clears queued samples,
    // asks its one worker to exit, and awaits that cleanup.
    input.shutdown();
    // The worker may be delivering a final bounded status. Closing its
    // receiver makes that send fail promptly before shutdown waits for it.
    drop(monitor_receiver);
    monitor.shutdown().await;
    Ok(())
}

fn apply_vad_monitor_actions(actions: Vec<VadMonitorAction>) -> bool {
    for action in actions {
        match action {
            VadMonitorAction::Display(output) => speak::emit_vad_monitor(output),
            VadMonitorAction::Exit => return false,
        }
    }
    true
}

enum VoiceEvent {
    Dictation(DictationEvent),
    Main(Event),
}

/// Applies facts at the main controller boundary for both line-input and voice
/// runtimes. The controller still decides whether a worker completion may
/// become output or history; this helper only chooses raw-safe rendering.
async fn apply_main_event(
    controller: &mut Controller,
    runtime: &mut TurnRuntime,
    event: Event,
    output_mode: OutputMode,
) -> bool {
    if let Event::TurnFinished(finished) = &event {
        runtime.retire(finished.id).await;
    }
    if let Event::DecisionFinished { id, .. } = &event {
        runtime.retire_decision(*id).await;
    }

    for effect in controller.handle(event) {
        if !runtime.apply(effect, output_mode).await {
            return false;
        }
    }
    true
}

struct TurnRuntime {
    client: Arc<OllamaClient>,
    camera: CameraService,
    event_sender: mpsc::Sender<Event>,
    active: Option<RunningTurn>,
    active_decision: Option<RunningDecision>,
}

struct RunningTurn {
    id: TurnId,
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
}

struct RunningDecision {
    id: DecisionId,
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
}

impl TurnRuntime {
    fn new(client: OllamaClient, camera: CameraService, event_sender: mpsc::Sender<Event>) -> Self {
        Self {
            client: Arc::new(client),
            camera,
            event_sender,
            active: None,
            active_decision: None,
        }
    }

    async fn apply(&mut self, effect: Effect, output_mode: OutputMode) -> bool {
        match effect {
            Effect::StartTurn(request) => self.start(request).await,
            Effect::StartDecision(request) => self.start_decision(request).await,
            Effect::PrepareCamera(preparation) => self.prepare_camera(preparation),
            Effect::ListCameras => self.list_cameras(),
            Effect::CancelTurn(id) => self.cancel(id).await,
            Effect::CancelDecision(id) => self.cancel_decision(id).await,
            Effect::Display(output) => output_mode.emit(output),
            Effect::Exit => {
                self.shutdown().await;
                return false;
            }
        }

        true
    }

    async fn start(&mut self, request: TurnRequest) {
        // The controller always emits CancelTurn before StartTurn, but retain a
        // defensive cleanup here so this runtime never keeps two owned tasks.
        // Unlike application shutdown, this does not wait for a previously
        // cancelled camera's bounded cleanup: a new text turn must stay
        // responsive while that closure releases its gate.
        self.stop_active_turn().await;

        let id = request.id;
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let client = self.client.clone();
        let camera = self.camera.clone();
        let event_sender = self.event_sender.clone();
        let handle = tokio::spawn(async move {
            let result = run_turn(
                client,
                camera,
                event_sender.clone(),
                request,
                worker_cancellation.clone(),
            )
            .await;

            // A cancelled worker normally sends nothing. If cancellation races
            // with completion, the controller's turn ID check remains the final
            // output authorization step.
            if !worker_cancellation.is_cancelled() && !matches!(result, TurnResult::Cancelled) {
                let _ = event_sender
                    .send(Event::TurnFinished(TurnFinished { id, result }))
                    .await;
            }
        });

        self.active = Some(RunningTurn {
            id,
            cancellation,
            handle,
        });
    }

    async fn start_decision(&mut self, request: DecisionRequest) {
        // Only one structured decision may be eligible at a time. The
        // controller also authorizes results by ID, so a late task is inert.
        self.stop_active_decision().await;

        let id = request.id();
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let client = self.client.clone();
        let event_sender = self.event_sender.clone();
        let handle = tokio::spawn(async move {
            let result = run_decision(client, request, worker_cancellation.clone()).await;
            if !worker_cancellation.is_cancelled() && !matches!(result, DecisionResult::Cancelled) {
                let _ = event_sender
                    .send(Event::DecisionFinished { id, result })
                    .await;
            }
        });

        self.active_decision = Some(RunningDecision {
            id,
            cancellation,
            handle,
        });
    }

    fn list_cameras(&self) {
        let camera = self.camera.clone();
        let event_sender = self.event_sender.clone();
        tokio::spawn(async move {
            let result = camera.list().await.map_err(|error| error.to_string());
            let _ = event_sender.send(Event::CamerasListed(result)).await;
        });
    }

    fn prepare_camera(&self, preparation: CameraPreparation) {
        let camera = self.camera.clone();
        let event_sender = self.event_sender.clone();
        tokio::spawn(async move {
            let result = camera
                .list()
                .await
                .and_then(|cameras| select_camera(&cameras, preparation.requested))
                .map_err(|error| error.to_string());
            let _ = event_sender
                .send(Event::CameraPrepared {
                    id: preparation.id,
                    result,
                })
                .await;
        });
    }

    async fn cancel(&mut self, id: TurnId) {
        if self.active.as_ref().map(|active| active.id) != Some(id) {
            return;
        }

        self.stop_active_turn().await;
    }

    async fn cancel_decision(&mut self, id: DecisionId) {
        if self.active_decision.as_ref().map(|active| active.id) != Some(id) {
            return;
        }

        self.stop_active_decision().await;
    }

    async fn stop_active_turn(&mut self) {
        let RunningTurn {
            cancellation,
            handle,
            ..
        } = match self.active.take() {
            Some(active) => active,
            None => return,
        };
        cancellation.cancel();
        handle.abort();
        let _ = handle.await;
    }

    async fn stop_active_decision(&mut self) {
        let RunningDecision {
            cancellation,
            handle,
            ..
        } = match self.active_decision.take() {
            Some(active) => active,
            None => return,
        };
        cancellation.cancel();
        handle.abort();
        let _ = handle.await;
    }

    async fn retire(&mut self, id: TurnId) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.id != id {
            return;
        }

        let RunningTurn { handle, .. } = self.active.take().expect("active turn was checked above");
        let _ = handle.await;
    }

    async fn retire_decision(&mut self, id: DecisionId) {
        let Some(active) = self.active_decision.as_ref() else {
            return;
        };
        if active.id != id {
            return;
        }

        let RunningDecision { handle, .. } = self
            .active_decision
            .take()
            .expect("active decision was checked above");
        let _ = handle.await;
    }

    async fn shutdown(&mut self) {
        self.stop_active_turn().await;
        self.stop_active_decision().await;
        // A capture cancelled earlier may already be detached from its outer
        // async task. It still owns this gate until V4L2 has released its
        // buffers and device handle, so wait before process shutdown.
        self.camera.wait_for_idle().await;
    }
}

/// Coordinates two independent controllers for `--voice`.
///
/// `DictationSession` remains responsible for deciding whether a Whisper
/// completion is current. The main `Controller` remains responsible for turn
/// ownership, history, and accepted output. This small coordinator only
/// defines the user-interface policy that a READY Enter/Esc cancels an
/// outstanding answer before starting or returning to the next recording.
struct VoiceRuntime {
    controller: Controller,
    turns: TurnRuntime,
    dictation: DictationSession,
}

impl VoiceRuntime {
    fn new(controller: Controller, turns: TurnRuntime, dictation: DictationSession) -> Self {
        Self {
            controller,
            turns,
            dictation,
        }
    }

    async fn handle_main_event(&mut self, event: Event) -> bool {
        apply_main_event(
            &mut self.controller,
            &mut self.turns,
            event,
            OutputMode::Raw,
        )
        .await
    }

    async fn handle_dictation_event(&mut self, event: DictationEvent) -> bool {
        let cancel_main_work = match &event {
            // While a permission question is waiting, Enter begins the one
            // recording that supplies its reply. It must preserve the pending
            // original question and selected camera. In every other READY
            // state, Enter starts a new question and invalidates old work.
            DictationEvent::Input(DictationInput::Enter) => {
                self.dictation.is_ready() && !self.controller.awaits_photo_permission()
            }
            // Esc in READY explicitly abandons a pending permission request
            // (or cancels a current model/decision task).
            DictationEvent::Input(DictationInput::Escape) => self.dictation.is_ready(),
            _ => false,
        };
        if cancel_main_work && !self.handle_main_event(Event::Input(Input::Stop)).await {
            return false;
        }

        for action in self.dictation.handle(event).await {
            match action {
                DictationAction::Display(output) => speak::emit_dictation(output),
                DictationAction::AcceptedTranscript { text } => {
                    // The controller decides whether this accepted transcript
                    // begins a new question or replies to a still-current
                    // permission request. It is never parsed as a terminal
                    // command.
                    if !self.handle_main_event(Event::VoiceTranscript(text)).await {
                        return false;
                    }
                }
                DictationAction::Exit => {
                    // Dictation shutdown owns microphone/recognizer cleanup;
                    // the main controller cancels its owned model turn using
                    // its ordinary generation-safe exit path.
                    let _ = self.handle_main_event(Event::Input(Input::Interrupt)).await;
                    return false;
                }
            }
        }

        true
    }

    async fn shutdown(&mut self) {
        self.dictation.shutdown().await;
        self.turns.shutdown().await;
    }
}

async fn run_turn(
    client: Arc<OllamaClient>,
    camera_service: CameraService,
    event_sender: mpsc::Sender<Event>,
    request: TurnRequest,
    cancellation: CancellationToken,
) -> TurnResult {
    let TurnRequest {
        id,
        question,
        history,
        visual,
        ..
    } = request;

    let (image, proposed_scene) = match visual {
        VisualRequest::UseAccepted(scene) => (scene, None),
        VisualRequest::ProposeNew(path) => {
            let mut load_task = tokio::task::spawn_blocking(move || Photo::load(path));
            let photo = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    load_task.abort();
                    return TurnResult::Cancelled;
                }
                result = &mut load_task => match result {
                    Ok(Ok(photo)) => photo,
                    Ok(Err(error)) => {
                        return TurnResult::Failed {
                            message: format!("could not use photo: {error}"),
                        };
                    }
                    Err(error) => {
                        return TurnResult::Failed {
                            message: format!("photo loading task ended unexpectedly: {error}"),
                        };
                    }
                },
            };

            if cancellation.is_cancelled() {
                return TurnResult::Cancelled;
            }

            (Some(photo.clone()), Some(photo))
        }
        VisualRequest::ProposeCamera(camera) => {
            if !send_camera_event(
                &event_sender,
                &cancellation,
                Event::CameraCaptureStarted {
                    id,
                    camera: camera.clone(),
                },
            )
            .await
            {
                return TurnResult::Cancelled;
            }

            let photo = match camera_service.capture(camera, cancellation.clone()).await {
                Ok(photo) => photo,
                Err(CameraRunError::Cancelled) => return TurnResult::Cancelled,
                Err(CameraRunError::Failed(error)) => {
                    return TurnResult::Failed {
                        message: format!("could not capture camera: {error}"),
                    };
                }
            };

            if cancellation.is_cancelled() {
                return TurnResult::Cancelled;
            }

            let PhotoSource::Camera(source) = photo.source() else {
                return TurnResult::Failed {
                    message: "camera worker returned a non-camera photo; no answer was accepted"
                        .into(),
                };
            };
            if !send_camera_event(
                &event_sender,
                &cancellation,
                Event::CameraCaptureReady {
                    id,
                    source: source.clone(),
                },
            )
            .await
            {
                return TurnResult::Cancelled;
            }

            (Some(photo.clone()), Some(photo))
        }
    };

    let answer = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return TurnResult::Cancelled,
        answer = client.answer(&history, &question, image.as_ref().map(Photo::bytes)) => answer,
    };

    if cancellation.is_cancelled() {
        return TurnResult::Cancelled;
    }

    match answer {
        Ok(answer) => TurnResult::Answer {
            answer,
            proposed_scene,
        },
        Err(error) => TurnResult::Failed {
            message: format!("local inference failed: {error}"),
        },
    }
}

async fn run_decision(
    client: Arc<OllamaClient>,
    request: DecisionRequest,
    cancellation: CancellationToken,
) -> DecisionResult {
    let result = match request {
        DecisionRequest::ChoosePhoto {
            question,
            history,
            scene,
            ..
        } => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return DecisionResult::Cancelled,
                result = client.decide_photo(&history, &question, scene.as_ref().map(Photo::bytes)) => {
                    result.map(DecisionResult::Photo)
                }
            }
        }
        DecisionRequest::InterpretPermission {
            proposed_action,
            selected_camera,
            permission_question,
            reply,
            ..
        } => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return DecisionResult::Cancelled,
                result = client.interpret_permission(
                    &proposed_action,
                    &selected_camera,
                    &permission_question,
                    &reply,
                ) => result.map(DecisionResult::Permission),
            }
        }
    };

    if cancellation.is_cancelled() {
        return DecisionResult::Cancelled;
    }

    match result {
        Ok(result) => result,
        Err(error) => DecisionResult::Failed {
            message: error.to_string(),
        },
    }
}

async fn send_camera_event(
    event_sender: &mpsc::Sender<Event>,
    cancellation: &CancellationToken,
    event: Event,
) -> bool {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        result = event_sender.send(event) => result.is_ok(),
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("could not configure local inference: {0}")]
    Configuration(#[from] ThinkError),

    #[error("could not start terminal input: {0}")]
    InputAdapter(#[source] io::Error),

    #[error("could not run local dictation: {0}")]
    Dictation(#[from] dictation::DictationError),

    #[error("could not start local VAD monitor: {0}")]
    VadMonitor(String),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        future::Future,
        io::Cursor,
        path::{Path, PathBuf},
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use base64::{Engine, engine::general_purpose::STANDARD};
    use image::{DynamicImage, ImageFormat};
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, Request as WiremockRequest, Respond, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    use crate::{
        controller::Controller,
        dictation::{
            DictationSession,
            audio::{MicrophoneDescription, Recorder, RecordingHandle},
            transcribe::{Transcriber, TranscriptionError},
        },
        protocol::{
            CapturedAudio, DecisionId, DecisionResult, DictationEvent, DictationInput, Effect,
            Event, Input, PhotoDecision, RecordingId, TurnFinished, TurnId, TurnRequest,
            TurnResult, VisualRequest,
        },
        see::{
            MAX_PHOTO_BYTES, Photo, PhotoSource,
            camera::{
                CameraBackend, CameraDescriptor, CameraError, CameraId, CameraPhotoSource,
                CameraService,
            },
        },
        think::{OllamaClient, OllamaConfig, TEACHER_INSTRUCTIONS},
    };

    use super::{TurnRuntime, VoiceRuntime, run_turn};

    fn client(server: &MockServer) -> Arc<OllamaClient> {
        Arc::new(
            OllamaClient::new(OllamaConfig::new(&server.uri(), "test-model").unwrap()).unwrap(),
        )
    }

    async fn within_test_timeout<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .expect("worker test exceeded its three-second timeout")
    }

    fn ollama_response(content: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "message": {"role": "assistant", "content": content},
            "done": true,
            "done_reason": "stop"
        }))
    }

    /// Makes a deterministic sequence of model outcomes available to one
    /// integration test without a live model or timing guesses.
    #[derive(Clone)]
    struct SequentialResponder {
        responses: Arc<Mutex<VecDeque<ResponseTemplate>>>,
    }

    impl SequentialResponder {
        fn new(responses: impl IntoIterator<Item = ResponseTemplate>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            }
        }
    }

    impl Respond for SequentialResponder {
        fn respond(&self, _request: &WiremockRequest) -> ResponseTemplate {
            self.responses
                .lock()
                .expect("mock response queue lock")
                .pop_front()
                .unwrap_or_else(|| ResponseTemplate::new(500))
        }
    }

    fn request_for_photo(path: PathBuf) -> TurnRequest {
        TurnRequest {
            id: TurnId::new(1),
            question: "What is this?".into(),
            history: Vec::new(),
            visual: VisualRequest::ProposeNew(path),
        }
    }

    fn start_camera_request(
        controller: &mut Controller,
        camera: CameraDescriptor,
        question: &str,
    ) -> TurnRequest {
        let preparation = controller
            .handle(Event::Input(Input::Camera {
                requested: Some(camera.id()),
                question: question.into(),
            }))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::PrepareCamera(preparation) => Some(preparation),
                _ => None,
            })
            .expect("camera command should request a preflight");
        controller
            .handle(Event::CameraPrepared {
                id: preparation.id,
                result: Ok(camera),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("validated camera should start a turn")
    }

    fn unused_camera_service() -> CameraService {
        // These file-photo tests never invoke camera capture. Constructing the
        // backend is inert and does not enumerate or open hardware.
        CameraService::v4l()
    }

    fn unused_event_sender() -> mpsc::Sender<Event> {
        let (sender, _receiver) = mpsc::channel(4);
        sender
    }

    /// A synthetic capture boundary for the integrated voice tests. It sends
    /// the same typed events as CPAL, but never opens a physical microphone.
    #[derive(Default)]
    struct VoiceRecorder {
        starts: Arc<AtomicUsize>,
        finishes: Arc<AtomicUsize>,
        cancels: Arc<AtomicUsize>,
    }

    impl Recorder for VoiceRecorder {
        fn selected_microphone(&self) -> Result<MicrophoneDescription, String> {
            Ok(MicrophoneDescription::new(
                "synthetic voice microphone".into(),
                16_000,
                1,
            ))
        }

        fn start(
            &self,
            recording_id: RecordingId,
            event_sender: mpsc::Sender<DictationEvent>,
        ) -> Result<Box<dyn RecordingHandle>, String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let _ = event_sender.try_send(DictationEvent::RecordingStarted { recording_id });
            Ok(Box::new(VoiceRecordingHandle {
                recording_id,
                event_sender,
                finishes: self.finishes.clone(),
                cancels: self.cancels.clone(),
            }))
        }
    }

    struct VoiceRecordingHandle {
        recording_id: RecordingId,
        event_sender: mpsc::Sender<DictationEvent>,
        finishes: Arc<AtomicUsize>,
        cancels: Arc<AtomicUsize>,
    }

    impl RecordingHandle for VoiceRecordingHandle {
        fn finish(&self) {
            self.finishes.fetch_add(1, Ordering::SeqCst);
            let _ = self
                .event_sender
                .try_send(DictationEvent::RecordingFinished {
                    recording_id: self.recording_id,
                    audio: CapturedAudio::new(vec![0.1; 160]),
                });
        }

        fn cancel(&self) {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            let _ = self
                .event_sender
                .try_send(DictationEvent::RecordingCancelled {
                    recording_id: self.recording_id,
                });
        }

        fn wait(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async {})
        }
    }

    /// A queue of preselected recognizer results. It exercises the production
    /// blocking-worker path without loading a Whisper model or using audio
    /// hardware.
    struct VoiceTranscriber {
        results: Mutex<VecDeque<String>>,
        calls: AtomicUsize,
        model_path: PathBuf,
    }

    impl VoiceTranscriber {
        fn new(results: impl IntoIterator<Item = String>) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
                calls: AtomicUsize::new(0),
                model_path: PathBuf::from("synthetic-whisper-model.bin"),
            }
        }
    }

    impl Transcriber for VoiceTranscriber {
        fn model_path(&self) -> &Path {
            &self.model_path
        }

        fn model_is_available(&self) -> bool {
            true
        }

        fn transcribe(
            &self,
            _audio: CapturedAudio,
            cancelled: Arc<AtomicBool>,
        ) -> Result<String, TranscriptionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if cancelled.load(Ordering::Acquire) {
                return Err(TranscriptionError::Cancelled);
            }
            self.results
                .lock()
                .expect("synthetic transcript queue lock")
                .pop_front()
                .ok_or_else(|| {
                    TranscriptionError::Failed(
                        "synthetic test did not provide a transcript result".into(),
                    )
                })
        }
    }

    struct VoiceHarness {
        runtime: VoiceRuntime,
        dictation_events: mpsc::Receiver<DictationEvent>,
        main_events: mpsc::Receiver<Event>,
        recorder: Arc<VoiceRecorder>,
        transcriber: Arc<VoiceTranscriber>,
    }

    impl VoiceHarness {
        async fn next_dictation_event(&mut self) -> DictationEvent {
            within_test_timeout(self.dictation_events.recv())
                .await
                .expect("synthetic dictation event")
        }

        async fn submit_next_transcript(&mut self) -> (RecordingId, String) {
            assert!(
                self.runtime
                    .handle_dictation_event(DictationEvent::Input(DictationInput::Enter))
                    .await
            );
            let recording_id = match self.next_dictation_event().await {
                DictationEvent::RecordingStarted { recording_id } => recording_id,
                event => panic!("expected synthetic recording start, got {event:?}"),
            };
            assert!(
                self.runtime
                    .handle_dictation_event(DictationEvent::RecordingStarted { recording_id })
                    .await
            );
            assert!(
                self.runtime
                    .handle_dictation_event(DictationEvent::Input(DictationInput::Enter))
                    .await
            );
            let finished = self.next_dictation_event().await;
            assert!(matches!(
                &finished,
                DictationEvent::RecordingFinished {
                    recording_id: finished_id,
                    ..
                } if *finished_id == recording_id
            ));
            assert!(self.runtime.handle_dictation_event(finished).await);

            let transcript = self.next_dictation_event().await;
            let text = match &transcript {
                DictationEvent::TranscriptReady {
                    recording_id: transcript_id,
                    text,
                } if *transcript_id == recording_id => text.clone(),
                event => panic!("expected synthetic transcript, got {event:?}"),
            };
            assert!(self.runtime.handle_dictation_event(transcript).await);
            (recording_id, text)
        }

        async fn next_main_event(&mut self) -> Event {
            within_test_timeout(self.main_events.recv())
                .await
                .expect("mock Ollama or camera event")
        }

        async fn accept_next_answer(&mut self) {
            let event = self.next_main_event().await;
            assert!(matches!(
                event,
                Event::TurnFinished(_) | Event::DecisionFinished { .. }
            ));
            assert!(self.runtime.handle_main_event(event).await);
        }
    }

    fn voice_harness(
        server: &MockServer,
        transcripts: impl IntoIterator<Item = String>,
        camera: CameraService,
    ) -> VoiceHarness {
        let (main_sender, main_events) = mpsc::channel(super::EVENT_CHANNEL_CAPACITY);
        let (dictation_sender, dictation_events) = mpsc::channel(super::EVENT_CHANNEL_CAPACITY);
        let recorder = Arc::new(VoiceRecorder::default());
        let transcriber = Arc::new(VoiceTranscriber::new(transcripts));
        let dictation = DictationSession::with_services(
            recorder.clone(),
            transcriber.clone(),
            dictation_sender,
        );
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "test-model").unwrap()).unwrap();

        VoiceHarness {
            runtime: VoiceRuntime::new(
                Controller::new(),
                TurnRuntime::new(client, camera, main_sender),
                dictation,
            ),
            dictation_events,
            main_events,
            recorder,
            transcriber,
        }
    }

    fn tiny_png() -> Vec<u8> {
        let image = DynamicImage::new_rgba8(1, 1);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn tiny_jpeg() -> Vec<u8> {
        let image = DynamicImage::new_rgb8(1, 1);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Jpeg).unwrap();
        bytes.into_inner()
    }

    #[derive(Clone)]
    struct FakeCameraBackend {
        camera: CameraDescriptor,
        photo: Photo,
        fail: bool,
        captures: Arc<AtomicUsize>,
    }

    impl CameraBackend for FakeCameraBackend {
        fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
            Ok(vec![self.camera.clone()])
        }

        fn capture(&self, _camera: &CameraDescriptor) -> Result<Photo, CameraError> {
            self.captures.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(CameraError::NoCameras)
            } else {
                Ok(self.photo.clone())
            }
        }
    }

    fn fake_camera() -> (CameraDescriptor, Photo) {
        let camera = CameraDescriptor::new(CameraId::new(7), "Mock Camera");
        let photo = Photo::from_camera_jpeg(
            CameraPhotoSource::new(camera.clone(), 1, 1, "MJPEG"),
            tiny_jpeg(),
        )
        .unwrap();
        (camera, photo)
    }

    fn start_request(effects: Vec<Effect>) -> TurnRequest {
        effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("controller should start a turn")
    }

    #[tokio::test]
    async fn rejected_photo_inputs_never_reach_ollama() {
        let server = MockServer::start().await;
        let client = client(&server);
        let directory = tempdir().unwrap();

        let missing = directory.path().join("missing.png");
        let unsupported = directory.path().join("notes.txt");
        fs::write(&unsupported, "not an image").unwrap();
        let oversized = directory.path().join("large.png");
        fs::write(&oversized, vec![0; MAX_PHOTO_BYTES + 1]).unwrap();

        for path in [missing, unsupported, oversized] {
            let result = within_test_timeout(run_turn(
                client.clone(),
                unused_camera_service(),
                unused_event_sender(),
                request_for_photo(path),
                CancellationToken::new(),
            ))
            .await;
            assert!(matches!(result, TurnResult::Failed { .. }));
        }

        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn accepted_photo_follow_up_reaches_ollama_with_history_and_same_image() {
        let server = MockServer::start().await;
        let directory = tempdir().unwrap();
        let photo_path = directory.path().join("tree image.png");
        let image_bytes = tiny_png();
        fs::write(&photo_path, &image_bytes).unwrap();
        let image_base64 = STANDARD.encode(&image_bytes);

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "What might this tree be?", "images": [image_base64.clone()]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "A first observation."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "What might this tree be?"},
                    {"role": "assistant", "content": "A first observation."},
                    {"role": "user", "content": "What should I observe next?", "images": [image_base64]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "Notice the leaf arrangement."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let client = client(&server);
        let mut controller = Controller::new();
        let first = start_request(controller.handle(Event::Input(Input::Look {
            path: photo_path,
            question: "What might this tree be?".into(),
        })));
        let first_id = first.id;
        let first_result = within_test_timeout(run_turn(
            client.clone(),
            unused_camera_service(),
            unused_event_sender(),
            first,
            CancellationToken::new(),
        ))
        .await;
        controller.handle(Event::TurnFinished(TurnFinished {
            id: first_id,
            result: first_result,
        }));

        let follow_up = start_request(controller.handle(Event::Input(Input::Ask(
            "What should I observe next?".into(),
        ))));
        let follow_up_id = follow_up.id;
        let follow_up_result = within_test_timeout(run_turn(
            client,
            unused_camera_service(),
            unused_event_sender(),
            follow_up,
            CancellationToken::new(),
        ))
        .await;
        let effects = controller.handle(Event::TurnFinished(TurnFinished {
            id: follow_up_id,
            result: follow_up_result,
        }));

        assert!(matches!(effects.as_slice(), [Effect::Display(_)]));
        assert_eq!(controller.accepted_history().len(), 2);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn camera_capture_failure_never_reaches_ollama() {
        let server = MockServer::start().await;
        let (camera, photo) = fake_camera();
        let captures = Arc::new(AtomicUsize::new(0));
        let service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera: camera.clone(),
            photo,
            fail: true,
            captures: captures.clone(),
        }));
        let (event_sender, _event_receiver) = mpsc::channel(4);
        let result = within_test_timeout(run_turn(
            client(&server),
            service,
            event_sender,
            TurnRequest {
                id: TurnId::new(1),
                question: "What is this?".into(),
                history: Vec::new(),
                visual: VisualRequest::ProposeCamera(camera),
            },
            CancellationToken::new(),
        ))
        .await;

        assert!(
            matches!(result, TurnResult::Failed { message } if message.contains("could not capture camera"))
        );
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn camera_image_and_question_reach_ollama_and_follow_up_keeps_the_scene() {
        let server = MockServer::start().await;
        let (camera, photo) = fake_camera();
        let image_base64 = STANDARD.encode(photo.bytes());

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "What is visible?", "images": [image_base64.clone()]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "A visible observation."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "What is visible?"},
                    {"role": "assistant", "content": "A visible observation."},
                    {"role": "user", "content": "What should I inspect next?", "images": [image_base64]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "Inspect the bark texture."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let captures = Arc::new(AtomicUsize::new(0));
        let service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera: camera.clone(),
            photo,
            fail: false,
            captures: captures.clone(),
        }));
        let (event_sender, mut event_receiver) = mpsc::channel(4);
        let mut controller = Controller::new();
        let first = start_camera_request(&mut controller, camera.clone(), "What is visible?");
        let first_id = first.id;
        let first_result = within_test_timeout(run_turn(
            client(&server),
            service.clone(),
            event_sender.clone(),
            first,
            CancellationToken::new(),
        ))
        .await;

        assert!(matches!(
            within_test_timeout(event_receiver.recv()).await,
            Some(Event::CameraCaptureStarted { camera: started, .. }) if started == camera
        ));
        assert!(matches!(
            within_test_timeout(event_receiver.recv()).await,
            Some(Event::CameraCaptureReady { source, .. })
                if source.camera() == &camera
                    && source.width() == 1
                    && source.height() == 1
        ));
        controller.handle(Event::TurnFinished(TurnFinished {
            id: first_id,
            result: first_result,
        }));
        assert!(matches!(
            controller.accepted_scene().unwrap().source(),
            PhotoSource::Camera(source) if source.camera() == &camera
        ));

        let follow_up = start_request(controller.handle(Event::Input(Input::Ask(
            "What should I inspect next?".into(),
        ))));
        let follow_up_id = follow_up.id;
        let follow_up_result = within_test_timeout(run_turn(
            client(&server),
            service,
            event_sender,
            follow_up,
            CancellationToken::new(),
        ))
        .await;
        controller.handle(Event::TurnFinished(TurnFinished {
            id: follow_up_id,
            result: follow_up_result,
        }));

        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(controller.accepted_history().len(), 2);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn accepted_voice_transcript_submits_one_literal_question() {
        let server = MockServer::start().await;
        let question = "What makes a fern different from a moss?";
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_response(
                r#"{"decision":"respond","answer":"Ferns make spores and have vascular tissue."}"#,
            ))
            .mount(&server)
            .await;

        let mut harness = voice_harness(&server, [question.to_owned()], unused_camera_service());
        let (recording_id, text) = harness.submit_next_transcript().await;
        assert_eq!(text, question);

        // The dictation controller is already READY after accepting the
        // first completion, so the duplicate cannot cross the bridge again.
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::TranscriptReady { recording_id, text })
                .await
        );
        harness.accept_next_answer().await;

        assert_eq!(harness.transcriber.calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.runtime.controller.accepted_history().len(), 1);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(body["format"].is_object());
        assert_eq!(
            body["messages"].as_array().unwrap().last().unwrap()["content"],
            question
        );
    }

    #[tokio::test]
    async fn voice_follow_up_uses_the_existing_accepted_conversation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_response(
                r#"{"decision":"respond","answer":"It is a partnership between a fungus and a photosynthetic partner."}"#,
            ))
            .mount(&server)
            .await;

        let mut harness = voice_harness(
            &server,
            [
                "What is lichen?".to_owned(),
                "What should I look for on tree bark?".to_owned(),
            ],
            unused_camera_service(),
        );
        harness.submit_next_transcript().await;
        harness.accept_next_answer().await;
        harness.submit_next_transcript().await;
        harness.accept_next_answer().await;

        assert_eq!(harness.runtime.controller.accepted_history().len(), 2);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(second["messages"][1]["content"], "What is lichen?");
        assert_eq!(
            second["messages"][2]["content"],
            "It is a partnership between a fungus and a photosynthetic partner."
        );
        assert_eq!(
            second["messages"][3]["content"],
            "What should I look for on tree bark?"
        );
    }

    #[tokio::test]
    async fn slash_looking_voice_text_cannot_activate_a_camera() {
        let server = MockServer::start().await;
        let question = "/camera 7 | take a photo";
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_response(
                r#"{"decision":"respond","answer":"I will treat that as words, not an action."}"#,
            ))
            .mount(&server)
            .await;

        let (camera, photo) = fake_camera();
        let captures = Arc::new(AtomicUsize::new(0));
        let camera_service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera,
            photo,
            fail: false,
            captures: captures.clone(),
        }));
        let mut harness = voice_harness(&server, [question.to_owned()], camera_service);
        harness.submit_next_transcript().await;
        harness.accept_next_answer().await;

        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn approved_voice_photo_permission_captures_once_and_many_follow_ups_reuse_the_scene() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SequentialResponder::new([
                ollama_response(
                    r#"{"decision":"request_photo","needed_visual_evidence":"the object's broad shape and surface details"}"#,
                ),
                ollama_response(r#"{"decision":"yes"}"#),
                ollama_response("The object has a broad green leaf and a visible central vein."),
                ollama_response(
                    r#"{"decision":"respond","answer":"The photo shows a broad green leaf and a central vein."}"#,
                ),
                ollama_response(
                    r#"{"decision":"respond","answer":"The visible leaf edge and central vein are the clearest features."}"#,
                ),
                ollama_response(
                    r#"{"decision":"respond","answer":"The far side of the object remains outside this still image."}"#,
                ),
            ]))
            .mount(&server)
            .await;

        let (camera, photo) = fake_camera();
        let image_base64 = STANDARD.encode(photo.bytes());
        let captures = Arc::new(AtomicUsize::new(0));
        let camera_service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera: camera.clone(),
            photo,
            fail: false,
            captures: captures.clone(),
        }));
        let original_question = "What visible features should I notice?";
        let reply = "I am comfortable with that one photo now.";
        let follow_ups = [
            (
                "What was in the photo?",
                "The photo shows a broad green leaf and a central vein.",
            ),
            (
                "Which visible feature should I inspect first?",
                "The visible leaf edge and central vein are the clearest features.",
            ),
            (
                "What remains uncertain from this view?",
                "The far side of the object remains outside this still image.",
            ),
        ];
        let mut harness = voice_harness(
            &server,
            std::iter::once(original_question.to_owned())
                .chain(std::iter::once(reply.to_owned()))
                .chain(
                    follow_ups
                        .iter()
                        .map(|(question, _)| (*question).to_owned()),
                ),
            camera_service,
        );

        harness.submit_next_transcript().await;
        let photo_decision = harness.next_main_event().await;
        assert!(matches!(
            &photo_decision,
            Event::DecisionFinished {
                result:
                    DecisionResult::Photo(PhotoDecision::RequestPhoto {
                        needed_visual_evidence,
                    }),
                ..
            } if needed_visual_evidence == "the object's broad shape and surface details"
        ));
        assert!(harness.runtime.handle_main_event(photo_decision).await);

        let prepared = harness.next_main_event().await;
        assert!(matches!(
            &prepared,
            Event::CameraPrepared {
                result: Ok(selected),
                ..
            } if selected == &camera
        ));
        assert!(harness.runtime.handle_main_event(prepared).await);
        assert!(harness.runtime.controller.awaits_photo_permission());
        assert_eq!(captures.load(Ordering::SeqCst), 0);

        harness.submit_next_transcript().await;
        let permission_decision = harness.next_main_event().await;
        assert!(matches!(
            &permission_decision,
            Event::DecisionFinished {
                result: DecisionResult::Permission(crate::protocol::PermissionDecision::Yes),
                ..
            }
        ));
        assert!(harness.runtime.handle_main_event(permission_decision).await);

        let started = harness.next_main_event().await;
        assert!(matches!(
            &started,
            Event::CameraCaptureStarted { camera: started_camera, .. } if started_camera == &camera
        ));
        assert!(harness.runtime.handle_main_event(started).await);
        let ready = harness.next_main_event().await;
        assert!(matches!(
            &ready,
            Event::CameraCaptureReady { source, .. } if source.camera() == &camera
        ));
        assert!(harness.runtime.handle_main_event(ready).await);
        let answer = harness.next_main_event().await;
        assert!(matches!(&answer, Event::TurnFinished(_)));
        assert!(harness.runtime.handle_main_event(answer).await);

        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(harness.runtime.controller.accepted_history().len(), 1);
        assert_eq!(
            harness.runtime.controller.accepted_history()[0].user,
            original_question
        );
        assert!(matches!(
            harness.runtime.controller.accepted_scene().unwrap().source(),
            PhotoSource::Camera(source) if source.camera() == &camera
        ));

        for (question, expected_answer) in follow_ups {
            let (_, transcript) = harness.submit_next_transcript().await;
            assert_eq!(transcript, question);
            let decision = harness.next_main_event().await;
            assert!(matches!(
                &decision,
                Event::DecisionFinished {
                    result: DecisionResult::Photo(PhotoDecision::Answer { answer }),
                    ..
                } if answer == expected_answer
            ));
            assert!(harness.runtime.handle_main_event(decision).await);
            assert_eq!(captures.load(Ordering::SeqCst), 1);
            assert!(matches!(
                harness.runtime.controller.accepted_scene().unwrap().source(),
                PhotoSource::Camera(source) if source.camera() == &camera
            ));
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3 + follow_ups.len());
        let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let third: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert!(first["format"].is_object());
        assert_eq!(
            first["messages"].as_array().unwrap().last().unwrap()["content"],
            original_question
        );
        assert!(second["format"].is_object());
        let permission_context: serde_json::Value = serde_json::from_str(
            second["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(permission_context["reply"], reply);
        assert_eq!(permission_context["selected_camera"]["id"], "7");
        assert!(third.get("format").is_none());
        assert_eq!(
            third["messages"].as_array().unwrap().last().unwrap()["content"],
            original_question
        );
        assert_eq!(
            third["messages"].as_array().unwrap().last().unwrap()["images"],
            json!([image_base64.clone()])
        );
        for (index, (question, _)) in follow_ups.iter().enumerate() {
            let body: serde_json::Value =
                serde_json::from_slice(&requests[3 + index].body).unwrap();
            let messages = body["messages"].as_array().unwrap();
            assert!(body["format"].is_object());
            assert_eq!(messages.last().unwrap()["content"], *question);
            assert_eq!(messages.last().unwrap()["images"], json!([image_base64]));
            assert_eq!(messages.len(), 4 + index * 2);
        }
        assert_eq!(
            harness.runtime.controller.accepted_history().len(),
            1 + follow_ups.len()
        );
    }

    #[tokio::test]
    async fn cancelled_stale_and_empty_voice_transcripts_make_no_request() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(&server, [String::new()], unused_camera_service());

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Enter))
                .await
        );
        let cancelled_id = match harness.next_dictation_event().await {
            DictationEvent::RecordingStarted { recording_id } => recording_id,
            event => panic!("expected synthetic recording start, got {event:?}"),
        };
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::RecordingStarted {
                    recording_id: cancelled_id,
                })
                .await
        );
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
                .await
        );
        let cancelled = harness.next_dictation_event().await;
        assert!(matches!(
            &cancelled,
            DictationEvent::RecordingCancelled { recording_id } if *recording_id == cancelled_id
        ));
        assert!(harness.runtime.handle_dictation_event(cancelled).await);
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::TranscriptReady {
                    recording_id: cancelled_id,
                    text: "late cancelled text".into(),
                })
                .await
        );

        // Put a new recording into TRANSCRIBING, then deliver a completion
        // from the old recording before the new recognizer's empty result.
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Enter))
                .await
        );
        let current_id = match harness.next_dictation_event().await {
            DictationEvent::RecordingStarted { recording_id } => recording_id,
            event => panic!("expected synthetic recording start, got {event:?}"),
        };
        assert_ne!(current_id, cancelled_id);
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::RecordingStarted {
                    recording_id: current_id,
                })
                .await
        );
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Enter))
                .await
        );
        let finished = harness.next_dictation_event().await;
        assert!(harness.runtime.handle_dictation_event(finished).await);
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::TranscriptReady {
                    recording_id: cancelled_id,
                    text: "late stale text".into(),
                })
                .await
        );
        let empty = harness.next_dictation_event().await;
        assert!(matches!(
            &empty,
            DictationEvent::TranscriptReady { recording_id, text }
                if *recording_id == current_id && text.is_empty()
        ));
        assert!(harness.runtime.handle_dictation_event(empty).await);

        assert_eq!(harness.transcriber.calls.load(Ordering::SeqCst), 1);
        assert!(!harness.runtime.controller.has_active_turn());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn new_voice_recording_invalidates_an_old_model_result() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_response(
                r#"{"decision":"respond","answer":"old answer"}"#,
            ))
            .mount(&server)
            .await;
        let mut harness = voice_harness(
            &server,
            ["first voice question".to_owned()],
            unused_camera_service(),
        );
        harness.submit_next_transcript().await;
        assert!(harness.runtime.controller.has_active_decision());

        // READY Enter cancels the old decision before it opens the next
        // recording. A controlled late completion cannot restore history.
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Enter))
                .await
        );
        assert!(!harness.runtime.controller.has_active_decision());
        assert!(
            harness
                .runtime
                .handle_main_event(Event::DecisionFinished {
                    id: DecisionId::new(1),
                    result: DecisionResult::Photo(PhotoDecision::Answer {
                        answer: "late old answer".into(),
                    }),
                })
                .await
        );
        assert!(harness.runtime.controller.accepted_history().is_empty());

        // The synthetic recorder is active for the new recording; explicit
        // shutdown must ask it to release instead of leaving a worker behind.
        harness.runtime.shutdown().await;
        assert_eq!(harness.recorder.starts.load(Ordering::SeqCst), 2);
        assert_eq!(harness.recorder.cancels.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn voice_escape_cancels_an_answer_and_late_result_stays_inert() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_response(
                r#"{"decision":"respond","answer":"answer that should be cancelled"}"#,
            ))
            .mount(&server)
            .await;
        let mut harness = voice_harness(
            &server,
            ["cancel this answer".to_owned()],
            unused_camera_service(),
        );
        harness.submit_next_transcript().await;
        assert!(harness.runtime.controller.has_active_decision());

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
                .await
        );
        assert!(!harness.runtime.controller.has_active_decision());
        assert!(
            harness
                .runtime
                .handle_main_event(Event::DecisionFinished {
                    id: DecisionId::new(1),
                    result: DecisionResult::Photo(PhotoDecision::Answer {
                        answer: "late cancelled answer".into(),
                    }),
                })
                .await
        );
        assert!(harness.runtime.controller.accepted_history().is_empty());
        harness.runtime.shutdown().await;
    }

    #[test]
    fn tiny_fixture_is_a_png_for_the_integration_test() {
        let bytes = tiny_png();
        assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::Png);
        assert!(Path::new("tree image.png").to_string_lossy().contains(' '));
    }
}
