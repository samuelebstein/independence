//! Runtime glue for the four current capabilities.
//!
//! `listen` sends typed input events, the controller authorizes effects,
//! `see` and `think` run in a cancellable worker, and `speak` emits only
//! controller-approved output.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    controller::Controller,
    dictation::{self, DictationAction, DictationSession},
    dictation::{
        audio::CpalRecorder,
        transcribe::{Transcriber, TranscriptionError, WhisperTranscriber},
    },
    launch::VoiceLaunchOptions,
    listen::{
        self,
        controls::{VoiceControlEvent, VoiceInputEvent, VoiceInputSource},
        evdev::{self, EvdevButtonInputSource},
    },
    profile::{self, CameraBackendSelection, ResolvedVoiceInput},
    protocol::{
        CameraPreparation, CapturedAudio, DictationEvent, DictationInput, DictationOutput, Effect,
        Event, Input, RecordingId, TurnFinished, TurnId, TurnRequest, TurnResult, VisualRequest,
    },
    see::{
        Photo, PhotoSource,
        camera::{CameraRunError, CameraService, select_camera},
    },
    speak,
    telemetry::{self, Interaction},
    think::{ModelCapabilities, OllamaClient, ThinkError},
};

const EVENT_CHANNEL_CAPACITY: usize = 32;
const VOICE_AUX_CHANNEL_CAPACITY: usize = 64;
const PARTIAL_TRANSCRIPTION_INTERVAL: Duration = Duration::from_millis(750);
const MAX_QUEUED_PHOTO_CAPTURES: usize = 32;
const MAX_DRAFT_PHOTO_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
enum OutputMode<'a> {
    Line,
    Voice {
        speech: &'a speak::SpeechHandle,
        interaction: Option<Interaction>,
    },
}

/// What a controller-authorized answer did at the terminal/speech boundary.
///
/// Accepting answer text and successfully delivering it through Piper are
/// separate facts. The controller owns the former; `VoiceRuntime` observes
/// the latter through the speech worker's completion event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnswerSpeechEmission {
    None,
    Queued(speak::UtteranceId),
    Unavailable,
}

impl OutputMode<'_> {
    fn emit(
        &self,
        output: crate::protocol::Output,
        speech_owner: speak::SpeechOwner,
    ) -> AnswerSpeechEmission {
        match self {
            Self::Line => {
                speak::emit(output);
                AnswerSpeechEmission::None
            }
            Self::Voice {
                speech,
                interaction,
            } => match output {
                crate::protocol::Output::Answer(answer) => {
                    // The same authorized text is logged before it is sent to
                    // the asynchronous worker. The worker cannot create or
                    // print dialogue on its own.
                    speak::independence_says(&answer);
                    match speech.say_with_interaction(speech_owner, answer, interaction.clone()) {
                        Some(utterance_id) => AnswerSpeechEmission::Queued(utterance_id),
                        None => AnswerSpeechEmission::Unavailable,
                    }
                }
                output => {
                    speak::emit(output);
                    AnswerSpeechEmission::None
                }
            },
        }
    }
}

/// The only extra fact the shared controller loop needs to give voice mode:
/// an accepted answer reached the terminal, but its spoken delivery has not
/// yet reached a terminal outcome.
struct MainEventResult {
    keep_running: bool,
    pending_answer_speech: Option<(speak::UtteranceId, Interaction)>,
}

impl MainEventResult {
    fn continue_without_speech() -> Self {
        Self {
            keep_running: true,
            pending_answer_speech: None,
        }
    }
}

/// Runs the interactive terminal application until `/quit`, EOF, or Ctrl+C.
pub async fn run() -> Result<(), AppError> {
    let client = OllamaClient::from_environment()?;
    discover_model_capabilities(&client).await;
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

        if !apply_main_event(&mut controller, &mut runtime, event, OutputMode::Line)
            .await
            .keep_running
        {
            break;
        }
    }

    runtime.shutdown().await;
    Ok(())
}

/// Obtains one cached `/api/show` record for the selected local model. A
/// failed probe does not stop text-only startup, but leaves camera and any
/// future direct-audio capability gated as explicitly unknown.
async fn discover_model_capabilities(client: &OllamaClient) {
    match client.discover_capabilities().await {
        Ok(ModelCapabilities {
            image_input,
            audio_input,
            audio_output,
            ..
        }) => speak::process(
            "ollama_model_capabilities",
            format!(
                "model={} image_input={} audio_input={} audio_output={}",
                client.model(),
                image_input.label(),
                audio_input.label(),
                audio_output.label()
            ),
        ),
        Err(error) => speak::warning(
            "ollama_model_capabilities_unknown",
            format!(
                "could not discover capabilities for model `{}`: {error}; text-only requests remain available, but camera/direct-audio support is disabled until a later launch can verify metadata",
                client.model()
            ),
        ),
    }
}

/// Runs standalone local dictation without constructing Ollama or camera
/// services. Its transcript deliberately stops at terminal display.
pub async fn run_dictation() -> Result<(), AppError> {
    dictation::run().await.map_err(AppError::Dictation)
}

/// Runs local dictation connected to the existing text-question controller.
///
/// This owns the raw-key input adapter, while `DictationSession` continues to
/// own microphone and Whisper lifetimes. Only an accepted transcript crosses
/// into the main controller as literal `Input::Ask` text.
pub async fn run_voice() -> Result<(), AppError> {
    run_voice_with_options(VoiceLaunchOptions::default()).await
}

/// Lists local audio output devices without creating a Piper worker, opening
/// an output stream, or touching microphone/model/camera capabilities.
pub fn list_audio_outputs() -> Result<(), AppError> {
    speak::list_audio_outputs().map_err(AppError::AudioOutput)
}

/// Lists microphone devices through CPAL without opening a stream, loading
/// Whisper, or constructing a voice/controller runtime.
pub fn list_audio_inputs() -> Result<(), AppError> {
    let devices = dictation::audio::list_input_devices().map_err(AppError::AudioInput)?;
    if devices.is_empty() {
        speak::warning(
            "audio_input_unavailable",
            "no microphone devices were reported by the local CPAL backend",
        );
        return Ok(());
    }
    speak::process("audio_input_list", format!("{} device(s)", devices.len()));
    for device in devices {
        info!(
            target: "process",
            event = "audio_input_available",
            default = device.is_default,
            id = %device.id,
            device = ?device.name,
            "audio input"
        );
    }
    Ok(())
}

/// Lists Linux input nodes without opening a microphone, model, camera, or
/// output stream.  The selected button-controller path must be supplied
/// explicitly to `--input-device`; this command never selects one itself.
pub fn list_input_devices() -> Result<(), AppError> {
    let devices = evdev::list_input_devices().map_err(AppError::InputDevices)?;
    if devices.is_empty() {
        speak::warning(
            "input_device_unavailable",
            "no /dev/input/event* devices were found",
        );
        return Ok(());
    }
    speak::process("input_device_list", format!("{} node(s)", devices.len()));
    for device in devices {
        match device.access_error {
            Some(error) => speak::warning(
                "input_device_inaccessible",
                format!("{}: {error}", device.path.display()),
            ),
            None => {
                let controls = device.controls.expect("open device has controls");
                info!(
                    target: "process",
                    event = "input_device_available",
                    path = %device.path.display(),
                    stable_path = ?device.stable_path.as_ref().map(|path| path.display().to_string()),
                    device = ?device.name,
                    physical_path = ?device.physical_path,
                    enter = controls.enter,
                    photo_p = controls.photo,
                    escape = controls.cancel,
                    controller_compatible = device.supports_controller(),
                    "Linux input device"
                );
            }
        }
    }
    Ok(())
}

/// Deliberately exercises exactly one local synthesis/playback request. This
/// is separate from `--voice` so someone can verify the engine, model, and
/// selected speakers before involving a microphone or Ollama.
pub async fn run_speech_test(audio_output_id: Option<String>) -> Result<(), AppError> {
    const TEST_SENTENCE: &str = "Hello, Steve. I'm here to help.";
    const TEST_TIMEOUT: Duration = Duration::from_secs(90);

    let (event_sender, mut event_receiver) = mpsc::channel(4);
    let mut speech = speak::SpeechService::with_output_id(audio_output_id, event_sender)
        .map_err(AppError::Speech)?;
    speak::independence_says(TEST_SENTENCE);
    let Some(utterance_id) = speech
        .handle()
        .say(speak::SpeechOwner::System, TEST_SENTENCE)
    else {
        speech.shutdown();
        return Err(AppError::SpeechTestUnavailable);
    };

    let result = tokio::time::timeout(TEST_TIMEOUT, event_receiver.recv()).await;
    speech.shutdown();
    match result {
        Ok(Some(event)) if event.utterance_id == utterance_id => match event.outcome {
            speak::SpeechOutcome::Played => {
                speak::process(
                    "speech_test_completed",
                    "local Piper synthesis and playback completed",
                );
                Ok(())
            }
            outcome => Err(AppError::SpeechTestFailed(outcome)),
        },
        Ok(Some(_)) => Err(AppError::SpeechTestUnexpectedEvent),
        Ok(None) => Err(AppError::SpeechTestUnavailable),
        Err(_) => Err(AppError::SpeechTestTimedOut),
    }
}

/// The button-controlled voice prototype with an optional exact CPAL
/// audio-output device identifier from the command line. `None` keeps the
/// configured/default output selection behavior.
pub async fn run_voice_with_output(audio_output_id: Option<String>) -> Result<(), AppError> {
    run_voice_with_options(VoiceLaunchOptions {
        audio_output: audio_output_id,
        ..VoiceLaunchOptions::default()
    })
    .await
}

/// Runs hold-to-talk voice with a terminal or directly attached physical
/// button source.  Both sources send the same `VoiceControlEvent` values into
/// one `VoiceRuntime`; neither gets a special controller path.
pub async fn run_voice_with_options(options: VoiceLaunchOptions) -> Result<(), AppError> {
    let resolved = profile::resolve_voice_profile(&options).map_err(AppError::Profile)?;
    let client = OllamaClient::new(resolved.ollama)?;
    discover_model_capabilities(&client).await;
    let (main_event_sender, mut main_event_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (dictation_event_sender, mut dictation_event_receiver) =
        mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (voice_aux_sender, mut voice_aux_receiver) = mpsc::channel(VOICE_AUX_CHANNEL_CAPACITY);
    let (speech_event_sender, mut speech_event_receiver) =
        mpsc::channel(VOICE_AUX_CHANNEL_CAPACITY);
    let (control_sender, mut control_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let speech = speak::SpeechService::with_configuration(
        resolved.piper,
        speak::playback::OutputSelection::from_requested_id(resolved.audio_output),
        speech_event_sender,
    )
    .map_err(AppError::Speech)?;
    let source_name = resolved.input.label();
    let mut input: Box<dyn VoiceInputSource> = match resolved.input {
        ResolvedVoiceInput::Terminal => Box::new(
            listen::dictation::spawn_terminal_voice_input(control_sender)
                .map_err(AppError::InputAdapter)?,
        ),
        ResolvedVoiceInput::Buttons(path) => Box::new(
            EvdevButtonInputSource::open(path, control_sender).map_err(AppError::ButtonInput)?,
        ),
    };
    speak::process("voice_input_source", source_name);
    if let Some(profile) = resolved.profile {
        speak::process("hardware_profile", profile.label());
    }
    speak::process("voice_camera_backend", resolved.camera_backend.label());
    speak::process(
        "voice_model",
        format!(
            "{} ({})",
            client.model(),
            client.image_input_support().label()
        ),
    );

    let dictation = DictationSession::with_services(
        Arc::new(CpalRecorder::with_requested_name(resolved.microphone)),
        Arc::new(WhisperTranscriber::new(resolved.whisper_model)),
        dictation_event_sender,
    );
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

    let mut runtime = VoiceRuntime::new(
        Controller::new(),
        TurnRuntime::new(
            client,
            match resolved.camera_backend {
                CameraBackendSelection::V4l => CameraService::v4l(),
                CameraBackendSelection::RpiCamStill => CameraService::rpicam(),
            },
            main_event_sender,
        ),
        dictation,
        voice_aux_sender,
        speech,
    );
    let mut partial_tick = tokio::time::interval(PARTIAL_TRANSCRIPTION_INTERVAL);
    partial_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => VoiceEvent::Control(VoiceInputEvent::Interrupt),
            event = control_receiver.recv() => match event {
                Some(event) => VoiceEvent::Control(event),
                None => VoiceEvent::ControlChannelClosed,
            },
            event = dictation_event_receiver.recv() => VoiceEvent::Dictation(event.unwrap_or(DictationEvent::Input(DictationInput::EndOfInput))),
            event = main_event_receiver.recv() => VoiceEvent::Main(event.unwrap_or(Event::Input(Input::Interrupt))),
            event = voice_aux_receiver.recv() => match event {
                Some(event) => VoiceEvent::Aux(event),
                None => VoiceEvent::AuxChannelClosed,
            },
            event = speech_event_receiver.recv() => match event {
                Some(event) => VoiceEvent::Speech(event),
                None => VoiceEvent::SpeechChannelClosed,
            },
            _ = partial_tick.tick() => VoiceEvent::PartialTick,
        };

        let keep_running = match event {
            VoiceEvent::Control(event) => runtime.handle_control_event(event).await,
            VoiceEvent::ControlChannelClosed => {
                runtime
                    .handle_control_source_failure(
                        source_name,
                        "control channel closed unexpectedly".into(),
                    )
                    .await
            }
            VoiceEvent::Dictation(event) => runtime.handle_dictation_event(event).await,
            VoiceEvent::Main(event) => runtime.handle_main_event(event).await,
            VoiceEvent::Aux(event) => runtime.handle_aux_event(event).await,
            VoiceEvent::Speech(event) => runtime.handle_speech_event(event).await,
            VoiceEvent::PartialTick => {
                runtime.request_partial_snapshot();
                true
            }
            VoiceEvent::AuxChannelClosed => true,
            VoiceEvent::SpeechChannelClosed => true,
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

enum VoiceEvent {
    Control(VoiceInputEvent),
    ControlChannelClosed,
    Dictation(DictationEvent),
    Main(Event),
    Aux(VoiceAuxEvent),
    Speech(speak::SpeechEvent),
    PartialTick,
    AuxChannelClosed,
    SpeechChannelClosed,
}

/// Applies facts at the main controller boundary for both line-input and voice
/// runtimes. The controller still decides whether a worker completion may
/// become output or history; this helper only chooses raw-safe rendering.
async fn apply_main_event(
    controller: &mut Controller,
    runtime: &mut TurnRuntime,
    event: Event,
    output_mode: OutputMode<'_>,
) -> MainEventResult {
    let completed_turn = match &event {
        Event::TurnFinished(finished) => Some((
            runtime.active_interaction(finished.id),
            match &finished.result {
                TurnResult::Answer { .. } => "answer_returned",
                TurnResult::Failed { .. } => "failed",
                TurnResult::Cancelled => "cancelled",
            },
        )),
        _ => None,
    };
    let speech_owner = match &event {
        Event::TurnFinished(finished) => speak::SpeechOwner::Turn(finished.id),
        _ => speak::SpeechOwner::System,
    };
    if let Event::TurnFinished(finished) = &event {
        runtime.retire(finished.id).await;
    }

    let effects = controller.handle(event);
    let accepted_answer = effects
        .iter()
        .any(|effect| matches!(effect, Effect::Display(crate::protocol::Output::Answer(_))));
    let mut answer_speech = AnswerSpeechEmission::None;
    for effect in effects {
        let result = runtime
            .apply(effect, output_mode.clone(), speech_owner)
            .await;
        if !result.keep_running {
            return MainEventResult {
                keep_running: false,
                pending_answer_speech: None,
            };
        }
        if result.answer_speech != AnswerSpeechEmission::None {
            answer_speech = result.answer_speech;
        }
    }
    let mut result = MainEventResult::continue_without_speech();
    if let Some((Some(interaction), worker_outcome)) = completed_turn {
        match (worker_outcome, accepted_answer) {
            ("answer_returned", true) => {
                interaction.mark("controller.answer_accepted");
                match answer_speech {
                    AnswerSpeechEmission::Queued(utterance_id) => {
                        // Wait for the worker's specific final outcome. The
                        // terminal text is already accepted, but playback is
                        // still independently observable and may fail.
                        result.pending_answer_speech = Some((utterance_id, interaction));
                    }
                    AnswerSpeechEmission::Unavailable => {
                        interaction.event(
                            "speech.final_answer",
                            "queue_unavailable",
                            telemetry::Fields::new(),
                        );
                        interaction
                            .complete("accepted_text_speech_unavailable", telemetry::Fields::new());
                    }
                    AnswerSpeechEmission::None => {
                        interaction.complete("accepted_text", telemetry::Fields::new());
                    }
                }
            }
            ("answer_returned", false) => {
                interaction.complete(
                    "rejected",
                    telemetry::fields([("rejection_stage", serde_json::json!("controller"))]),
                );
            }
            ("failed", _) => interaction.complete("failed", telemetry::Fields::new()),
            ("cancelled", _) => interaction.complete("cancelled", telemetry::Fields::new()),
            _ => {}
        }
    }
    result
}

struct TurnRuntime {
    client: Arc<OllamaClient>,
    camera: CameraService,
    event_sender: mpsc::Sender<Event>,
    active: Option<RunningTurn>,
    pending_interaction: Option<Interaction>,
    /// Cancellation returns control immediately, but an already-running
    /// bounded V4L2 closure can still own the camera gate. These background
    /// monitors finish telemetry only after that app-owned resource is idle;
    /// shutdown joins them so they cannot outlive the application.
    cleanup_tasks: Vec<JoinHandle<()>>,
}

struct RunningTurn {
    id: TurnId,
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
    interaction: Interaction,
}

struct RuntimeEffectResult {
    keep_running: bool,
    answer_speech: AnswerSpeechEmission,
}

impl RuntimeEffectResult {
    fn continued() -> Self {
        Self {
            keep_running: true,
            answer_speech: AnswerSpeechEmission::None,
        }
    }
}

impl TurnRuntime {
    fn new(client: OllamaClient, camera: CameraService, event_sender: mpsc::Sender<Event>) -> Self {
        Self {
            client: Arc::new(client),
            camera,
            event_sender,
            active: None,
            pending_interaction: None,
            cleanup_tasks: Vec::new(),
        }
    }

    /// Supplies the correlation created at a voice Enter press. The next
    /// controller-authorized `StartTurn` consumes it; typed turns create their
    /// timeline at dispatch instead.
    fn set_pending_interaction(&mut self, interaction: Interaction) {
        if let Some(previous) = self.pending_interaction.replace(interaction) {
            previous.complete("superseded_before_dispatch", telemetry::Fields::new());
        }
    }

    fn active_interaction(&self, id: TurnId) -> Option<Interaction> {
        self.active
            .as_ref()
            .filter(|active| active.id == id)
            .map(|active| active.interaction.clone())
    }

    fn supports_images(&self) -> bool {
        self.client.supports_images()
    }

    async fn apply(
        &mut self,
        effect: Effect,
        output_mode: OutputMode<'_>,
        speech_owner: speak::SpeechOwner,
    ) -> RuntimeEffectResult {
        match effect {
            Effect::StartTurn(request) => self.start(request).await,
            Effect::PrepareCamera(preparation) => self.prepare_camera(preparation),
            Effect::ListCameras => self.list_cameras(),
            Effect::CancelTurn(id) => self.cancel(id).await,
            Effect::Display(output) => {
                return RuntimeEffectResult {
                    keep_running: true,
                    answer_speech: output_mode.emit(output, speech_owner),
                };
            }
            Effect::Exit => {
                self.shutdown().await;
                return RuntimeEffectResult {
                    keep_running: false,
                    answer_speech: AnswerSpeechEmission::None,
                };
            }
        }

        RuntimeEffectResult::continued()
    }

    async fn start(&mut self, request: TurnRequest) {
        // The controller always emits CancelTurn before StartTurn, but retain a
        // defensive cleanup here so this runtime never keeps two owned tasks.
        // Unlike application shutdown, this does not wait for a previously
        // cancelled camera's bounded cleanup: a new text turn must stay
        // responsive while that closure releases its gate.
        self.stop_active_turn().await;

        let id = request.id;
        let interaction = self
            .pending_interaction
            .take()
            .unwrap_or_else(|| telemetry::start_interaction("typed", None));
        interaction.bind_turn(id);
        interaction.set_photo_count(match &request.visual {
            VisualRequest::UseAccepted(photos) => photos.len(),
            VisualRequest::ProposeNew(_) | VisualRequest::ProposeCamera(_) => 1,
        });
        interaction.mark("turn.request_dispatched");
        let request_uses_images = match &request.visual {
            VisualRequest::UseAccepted(photos) => !photos.is_empty(),
            VisualRequest::ProposeNew(_) | VisualRequest::ProposeCamera(_) => true,
        };
        if request_uses_images && !self.client.supports_images() {
            interaction.event(
                "turn.image_support",
                "rejected",
                telemetry::fields([("reason", serde_json::json!("configured_text_only_model"))]),
            );
            interaction.complete("failed", telemetry::Fields::new());
            let _ = self
                .event_sender
                .send(Event::TurnFinished(TurnFinished {
                    id,
                    result: TurnResult::Failed {
                        message: format!(
                            "camera/photo request was not started: {}",
                            self.client.image_input_restriction()
                        ),
                    },
                }))
                .await;
            return;
        }
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let client = self.client.clone();
        let camera = self.camera.clone();
        let event_sender = self.event_sender.clone();
        let worker_interaction = interaction.clone();
        let handle = tokio::spawn(async move {
            let result = run_turn_with_interaction(
                client,
                camera,
                event_sender.clone(),
                request,
                worker_cancellation.clone(),
                worker_interaction,
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
            interaction,
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
        if !self.client.supports_images() {
            let event_sender = self.event_sender.clone();
            let reason = self.client.image_input_restriction();
            tokio::spawn(async move {
                let _ = event_sender
                    .send(Event::CameraPrepared {
                        id: preparation.id,
                        result: Err(format!("camera capture remains off: {reason}")),
                    })
                    .await;
            });
            return;
        }
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

    async fn stop_active_turn(&mut self) {
        self.reap_finished_cleanup_tasks().await;
        let RunningTurn {
            cancellation,
            handle,
            interaction,
            ..
        } = match self.active.take() {
            Some(active) => active,
            None => return,
        };
        interaction.mark("cancellation.requested");
        let cleanup = interaction.stage("interaction.local_cleanup", telemetry::Fields::new());
        cancellation.cancel();
        handle.abort();
        let _ = handle.await;
        let camera = self.camera.clone();
        let cleanup_interaction = interaction.clone();
        self.cleanup_tasks.push(tokio::spawn(async move {
            // This waits only for Independence's local camera gate. It does
            // not claim to wait for Ollama's independent server-side work.
            camera.wait_for_idle().await;
            cleanup.finish("completed", telemetry::Fields::new());
            cleanup_interaction.mark("cancellation.local_cleanup_completed");
            cleanup_interaction.complete("cancelled", telemetry::Fields::new());
        }));
    }

    async fn reap_finished_cleanup_tasks(&mut self) {
        let mut active = Vec::with_capacity(self.cleanup_tasks.len());
        for task in std::mem::take(&mut self.cleanup_tasks) {
            if task.is_finished() {
                let _ = task.await;
            } else {
                active.push(task);
            }
        }
        self.cleanup_tasks = active;
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

    async fn shutdown(&mut self) {
        self.stop_active_turn().await;
        // A capture cancelled earlier may already be detached from its outer
        // async task. It still owns this gate until V4L2 has released its
        // buffers and device handle, so wait before process shutdown.
        self.camera.wait_for_idle().await;
        for task in std::mem::take(&mut self.cleanup_tasks) {
            let _ = task.await;
        }
    }
}

/// Coordinates button-controlled microphone, camera, and model lifecycles
/// for `--voice`.
///
/// The dictation controller still exclusively owns final recording and
/// transcription. This coordinator owns only an unfinished draft: replaceable
/// partial text, ordered camera requests, and the rule that all of it becomes
/// one literal controller input after Enter is released.
struct VoiceRuntime {
    controller: Controller,
    turns: TurnRuntime,
    dictation: DictationSession,
    aux_sender: mpsc::Sender<VoiceAuxEvent>,
    speech: speak::SpeechService,
    draft: Option<VoiceDraft>,
    /// Capture starts only after this exact listening cue completed or failed.
    /// A release/Escape clears it first, so a late speech event cannot activate
    /// the microphone after the user changed their mind.
    pending_listening_cue: Option<PendingListeningCue>,
    /// At most one camera request is dispatched at a time, in `P` press
    /// order. The camera service has its own hardware gate too; this queue
    /// preserves user-visible attachment order before work reaches that gate.
    active_photo_capture: Option<(u64, u64)>,
    live_transcription: Option<LiveTranscription>,
    pending_snapshot: Option<(RecordingId, CapturedAudio)>,
    cancelled_recordings: HashMap<RecordingId, Interaction>,
    /// Accepted text answers whose matching Piper request still has to report
    /// playback. This is deliberately keyed by worker utterance ID, rather
    /// than by the current draft or turn, because a new draft may begin after
    /// text acceptance while old audio is being cancelled.
    pending_answer_speech: HashMap<speak::UtteranceId, Interaction>,
    next_draft_id: u64,
    next_partial_sequence: u64,
}

struct VoiceDraft {
    id: u64,
    recording_id: Option<RecordingId>,
    recording_started: bool,
    held: bool,
    released: bool,
    snapshot_requested: bool,
    cancellation: CancellationToken,
    photos: BTreeMap<u64, DraftPhoto>,
    next_photo_request: u64,
    attached_photo_bytes: usize,
    last_partial: Option<String>,
    last_partial_error: Option<String>,
    final_transcript: Option<String>,
    submitted: bool,
    interaction: Interaction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingListeningCue {
    draft_id: u64,
    utterance_id: speak::UtteranceId,
}

impl VoiceDraft {
    fn new(id: u64) -> Self {
        let interaction = telemetry::start_interaction("voice", Some(id));
        interaction.mark("voice.enter_pressed");
        Self {
            id,
            recording_id: None,
            recording_started: false,
            held: true,
            released: false,
            snapshot_requested: false,
            cancellation: CancellationToken::new(),
            photos: BTreeMap::new(),
            next_photo_request: 1,
            attached_photo_bytes: 0,
            last_partial: None,
            last_partial_error: None,
            final_transcript: None,
            submitted: false,
            interaction,
        }
    }

    fn pending_photo_count(&self) -> usize {
        self.photos
            .values()
            .filter(|photo| matches!(photo, DraftPhoto::Queued | DraftPhoto::Capturing))
            .count()
    }

    fn has_pending_photos(&self) -> bool {
        self.photos
            .values()
            .any(|photo| matches!(photo, DraftPhoto::Queued | DraftPhoto::Capturing))
    }

    fn captured_photos(&self) -> Vec<Photo> {
        self.photos
            .values()
            .filter_map(|photo| match photo {
                DraftPhoto::Captured(photo) => Some(photo.clone()),
                DraftPhoto::Queued | DraftPhoto::Capturing | DraftPhoto::Failed => None,
            })
            .collect()
    }
}

enum DraftPhoto {
    Queued,
    Capturing,
    Captured(Photo),
    Failed,
}

struct LiveTranscription {
    recording_id: RecordingId,
    sequence: u64,
    cancelled: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

#[derive(Debug)]
enum VoiceAuxEvent {
    PartialTranscript {
        recording_id: RecordingId,
        sequence: u64,
        result: Result<String, String>,
    },
    PhotoCaptureFinished {
        draft_id: u64,
        request_id: u64,
        result: Result<Photo, String>,
    },
}

impl VoiceRuntime {
    fn new(
        controller: Controller,
        turns: TurnRuntime,
        dictation: DictationSession,
        aux_sender: mpsc::Sender<VoiceAuxEvent>,
        speech: speak::SpeechService,
    ) -> Self {
        Self {
            controller,
            turns,
            dictation,
            aux_sender,
            speech,
            draft: None,
            pending_listening_cue: None,
            active_photo_capture: None,
            live_transcription: None,
            pending_snapshot: None,
            cancelled_recordings: HashMap::new(),
            pending_answer_speech: HashMap::new(),
            next_draft_id: 1,
            next_partial_sequence: 1,
        }
    }

    async fn handle_main_event(&mut self, event: Event) -> bool {
        let interaction = match &event {
            Event::TurnFinished(finished) => self.turns.active_interaction(finished.id),
            _ => None,
        };
        let speech = self.speech.handle();
        let result = apply_main_event(
            &mut self.controller,
            &mut self.turns,
            event,
            OutputMode::Voice {
                speech: &speech,
                interaction,
            },
        )
        .await;
        if let Some((utterance_id, interaction)) = result.pending_answer_speech {
            self.pending_answer_speech.insert(utterance_id, interaction);
        }
        result.keep_running
    }

    /// Applies a device-independent physical control through the same paths
    /// the terminal adapter used before controls were separated out.
    ///
    /// This is intentionally a translation layer, not a second button state
    /// machine: DictationSession still owns microphone/transcription state and
    /// the main Controller still owns model-turn cancellation and acceptance.
    async fn handle_control_event(&mut self, event: VoiceInputEvent) -> bool {
        match event {
            VoiceInputEvent::Control(control) => {
                let input = match control {
                    VoiceControlEvent::TalkPressed => DictationInput::HoldStart,
                    VoiceControlEvent::TalkReleased => DictationInput::HoldEnd,
                    VoiceControlEvent::PhotoPressed => DictationInput::Photo,
                    VoiceControlEvent::Cancel => DictationInput::Escape,
                };
                self.handle_dictation_event(DictationEvent::Input(input))
                    .await
            }
            VoiceInputEvent::Interrupt => {
                self.handle_dictation_event(DictationEvent::Input(DictationInput::Interrupt))
                    .await
            }
            VoiceInputEvent::SourceFailed { source, message } => {
                self.handle_control_source_failure(source, message).await
            }
            VoiceInputEvent::SourceLost { source, message } => {
                self.handle_control_source_lost(source, message).await
            }
            VoiceInputEvent::SourceRecovered { source } => {
                speak::process(
                    "voice_input_source_recovered",
                    format!(
                        "{source} was reacquired; press Enter again to start a fresh recording"
                    ),
                );
                true
            }
        }
    }

    /// A physical button source must never fail while leaving an active
    /// recording or authorized model turn alive.  We first use the ordinary
    /// interruption path, then leave the voice loop so its owner can release
    /// the source and all native work.
    async fn handle_control_source_failure(&mut self, source: &str, message: String) -> bool {
        speak::warning(
            "voice_input_source_failed",
            format!("{source} failed: {message}; cancelling the active interaction"),
        );
        let _ = self
            .handle_dictation_event(DictationEvent::Input(DictationInput::Interrupt))
            .await;
        false
    }

    /// A USB keyboard can disappear and later return.  Its last release may
    /// never have reached us, so treat loss as an ordinary cancellation rather
    /// than submitting the potentially incomplete held recording.  The evdev
    /// adapter reports recovery separately; no key state survives the loss.
    async fn handle_control_source_lost(&mut self, source: &str, message: String) -> bool {
        speak::warning(
            "voice_input_source_lost",
            format!(
                "{source} was lost: {message}; cancelling unfinished work and waiting for recovery"
            ),
        );
        self.handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
            .await
    }

    async fn handle_dictation_event(&mut self, event: DictationEvent) -> bool {
        if let DictationEvent::RecordingSnapshot {
            recording_id,
            audio,
        } = event
        {
            self.handle_recording_snapshot(recording_id, audio);
            // Snapshots are intentionally not part of DictationSession's
            // final lifecycle, so do not give them controller authority.
            return true;
        }
        if let DictationEvent::RecordingSnapshotUnavailable { recording_id } = event {
            if let Some(draft) = self.draft.as_mut()
                && draft.recording_id == Some(recording_id)
            {
                draft.snapshot_requested = false;
            }
            return true;
        }

        let terminal_recording = match &event {
            DictationEvent::RecordingSnapshot { .. } => {
                unreachable!("recording snapshots return before dictation handling")
            }
            DictationEvent::RecordingSnapshotUnavailable { .. } => {
                unreachable!("unavailable snapshots return before dictation handling")
            }
            DictationEvent::Input(DictationInput::HoldStart) => {
                // Capture is deliberately delayed until the listening cue has
                // played. The cue completion comes back as `SpeechEvent`.
                return self.begin_draft().await;
            }
            // `Enter` is not emitted by the hold-to-talk adapter. Retaining
            // this narrow fallback keeps programmatic callers and the old
            // dictation test seam usable without altering real key handling.
            DictationEvent::Input(DictationInput::Enter) => {
                if self.dictation.is_ready() {
                    return self.begin_draft().await;
                } else {
                    self.release_draft();
                }
                None
            }
            DictationEvent::Input(DictationInput::HoldEnd) => {
                self.release_draft();
                None
            }
            DictationEvent::Input(DictationInput::Photo) => {
                self.request_photo().await;
                None
            }
            DictationEvent::Input(DictationInput::Escape) => {
                self.cancel_draft();
                self.speech.handle().cancel_all("escape");
                if !self.handle_main_event(Event::Input(Input::Stop)).await {
                    return false;
                }
                speak::voice_log("draft cancelled");
                None
            }
            DictationEvent::Input(DictationInput::Interrupt | DictationInput::EndOfInput) => {
                self.cancel_draft();
                self.speech.handle().cancel_all("input_ended");
                None
            }
            DictationEvent::RecordingStarted { recording_id } => {
                if let Some(draft) = self.draft.as_mut()
                    && draft.recording_id == Some(*recording_id)
                {
                    draft.recording_started = true;
                    draft.interaction.bind_recording(*recording_id);
                    draft.interaction.mark("microphone.recording_started_event");
                    speak::process(
                        "microphone_capture_started",
                        format!("draft_id={} recording_id={recording_id:?}", draft.id),
                    );
                }
                None
            }
            DictationEvent::RecordingCancelled { recording_id }
            | DictationEvent::TranscriptionCancelled { recording_id } => {
                if self.draft_matches_recording(*recording_id) {
                    self.cancel_draft();
                    self.speech.handle().cancel_all("dictation_failed");
                }
                Some((*recording_id, "cancelled"))
            }
            DictationEvent::RecordingFailed { recording_id, .. }
            | DictationEvent::TranscriptionFailed { recording_id, .. } => {
                if self.draft_matches_recording(*recording_id) {
                    self.cancel_draft();
                    self.speech.handle().cancel_all("dictation_failed");
                }
                Some((*recording_id, "failed"))
            }
            DictationEvent::TranscriptReady { recording_id, text }
                if text.trim().is_empty() && self.draft_matches_recording(*recording_id) =>
            {
                self.cancel_draft();
                self.speech.handle().cancel_all("empty_transcript");
                Some((*recording_id, "failed"))
            }
            DictationEvent::RecordingLimitReached { recording_id }
                if self.draft_matches_recording(*recording_id) =>
            {
                // The bounded capture owner ended this hold. Treat that as a
                // release so its authoritative final transcript can submit.
                self.release_draft();
                None
            }
            DictationEvent::RecordingFinished { recording_id, .. }
                if self.draft_matches_recording(*recording_id) =>
            {
                if let Some(draft) = self.draft.as_ref() {
                    draft.interaction.mark("microphone.recording_finished");
                }
                None
            }
            DictationEvent::RecordingFinished { .. }
            | DictationEvent::RecordingElapsed { .. }
            | DictationEvent::RecordingLimitReached { .. }
            | DictationEvent::TranscriptReady { .. } => None,
        };

        for action in self.dictation.handle(event).await {
            if !self.handle_dictation_action(action).await {
                return false;
            }
        }

        if let Some((recording_id, outcome)) = terminal_recording {
            self.complete_pending_recording(recording_id, outcome);
        }

        true
    }

    async fn handle_dictation_action(&mut self, action: DictationAction) -> bool {
        match action {
            DictationAction::RecordingRequested { recording_id } => {
                if let Some(draft) = self.draft.as_mut()
                    && draft.recording_id.is_none()
                {
                    draft.recording_id = Some(recording_id);
                }
            }
            DictationAction::Display(output) => self.display_dictation_log(output),
            DictationAction::AcceptedTranscript { recording_id, text } => {
                let accepted = self.draft.as_ref().is_some_and(|draft| {
                    draft.recording_id == Some(recording_id)
                        && draft.released
                        && !draft.cancellation.is_cancelled()
                });
                if !accepted {
                    return true;
                }

                speak::user_transcript("final", &text);
                if let Some(draft) = self.draft.as_mut() {
                    draft.interaction.mark("whisper.final_transcript_accepted");
                    draft.final_transcript = Some(text);
                }
                return self.try_submit().await;
            }
            DictationAction::Exit => {
                self.cancel_draft();
                self.speech.handle().cancel_all("dictation_exit");
                // Dictation shutdown owns microphone/recognizer cleanup; the
                // main controller owns its model turn through its ordinary
                // generation-safe exit path.
                let _ = self.handle_main_event(Event::Input(Input::Interrupt)).await;
                return false;
            }
        }

        true
    }

    fn display_dictation_log(&mut self, output: DictationOutput) {
        match output {
            DictationOutput::Ready => {
                speak::voice_log("ready — hold enter to start recording/transcription")
            }
            DictationOutput::Recording { .. } => {}
            DictationOutput::Discarding => {
                speak::voice_log("discarding recording and releasing microphone")
            }
            DictationOutput::Transcribing => {
                // Dictation emits this only after the CPAL recording owner has
                // produced `RecordingFinished`, which means its stream is
                // already released. Speaking now avoids recording our cue.
                speak::voice_log("microphone released; finishing local transcription");
                if let Some(draft) = self.draft.as_ref() {
                    draft
                        .interaction
                        .mark("whisper.final_transcription_started");
                    let _ = self.say(speak::SpeechOwner::Draft(draft.id), "i'm thinking");
                }
            }
            // `AcceptedTranscript` below provides the exact, one-time final
            // transcript log. Avoid printing it once here and once there.
            DictationOutput::Transcript(_) => {}
            DictationOutput::Diagnostic(message) => speak::voice_log(&message),
        }
    }

    async fn begin_draft(&mut self) -> bool {
        if !self.dictation.is_ready() {
            return true;
        }

        // A new press starts a fresh draft. Any already accepted context
        // remains in `controller`; only unfinished audio/photos are dropped.
        self.cancel_draft();
        self.speech.handle().cancel_all("new_recording");
        if !self.handle_main_event(Event::Input(Input::Stop)).await {
            return false;
        }

        let id = self.next_draft_id;
        self.next_draft_id = self
            .next_draft_id
            .checked_add(1)
            .expect("voice draft identifiers exhausted");
        self.draft = Some(VoiceDraft::new(id));
        if let Some(draft) = self.draft.as_ref() {
            draft.interaction.mark("speech.listening_cue_queued");
        }

        match self.say(speak::SpeechOwner::Draft(id), "i'm listening") {
            Some(utterance_id) => {
                self.pending_listening_cue = Some(PendingListeningCue {
                    draft_id: id,
                    utterance_id,
                });
            }
            None => {
                // Queue failure is explicit and privacy-safe. There will be
                // no later cue event, so capture may start without a cue.
                speak::warning(
                    "listening_cue_unavailable",
                    "starting microphone capture without the listening cue",
                );
                return self.start_capture_after_cue(id).await;
            }
        }
        true
    }

    /// Logs and queues an already-authorized spoken utterance. The controller
    /// remains the only source of model answers; this helper is used only for
    /// fixed interaction cues owned by this runtime.
    fn say(&self, owner: speak::SpeechOwner, text: &'static str) -> Option<speak::UtteranceId> {
        speak::independence_says(text);
        let interaction = match owner {
            speak::SpeechOwner::Draft(draft_id) => self
                .draft
                .as_ref()
                .filter(|draft| draft.id == draft_id)
                .map(|draft| draft.interaction.clone()),
            speak::SpeechOwner::Turn(turn_id) => self.turns.active_interaction(turn_id),
            speak::SpeechOwner::System => None,
        };
        self.speech
            .handle()
            .say_with_interaction(owner, text, interaction)
    }

    /// Handles a worker fact, rather than allowing the speech worker to
    /// manipulate microphone state directly. Only the exact listening cue for
    /// the currently-held draft may open capture.
    async fn handle_speech_event(&mut self, event: speak::SpeechEvent) -> bool {
        if let Some(interaction) = self.pending_answer_speech.remove(&event.utterance_id) {
            // The controller has already accepted and displayed the answer.
            // This terminal worker fact only finishes its independently
            // correlated speech-delivery outcome; it cannot alter history.
            match event.outcome {
                speak::SpeechOutcome::Played => {
                    interaction.mark("speech.final_answer.playback_completed");
                    interaction.complete("accepted_text_spoken", telemetry::Fields::new());
                }
                speak::SpeechOutcome::SynthesisFailed | speak::SpeechOutcome::PlaybackFailed => {
                    interaction.event(
                        "speech.final_answer",
                        "failed",
                        telemetry::fields([(
                            "delivery_stage",
                            serde_json::json!(match event.outcome {
                                speak::SpeechOutcome::SynthesisFailed => "synthesis",
                                speak::SpeechOutcome::PlaybackFailed => "playback",
                                speak::SpeechOutcome::Played | speak::SpeechOutcome::Cancelled => {
                                    unreachable!("the surrounding match excluded this outcome")
                                }
                            }),
                        )]),
                    );
                    interaction.complete("accepted_text_speech_failed", telemetry::Fields::new());
                }
                speak::SpeechOutcome::Cancelled => {
                    interaction.mark("speech.final_answer.cleanup_completed");
                    interaction
                        .complete("accepted_text_speech_cancelled", telemetry::Fields::new());
                }
            }
            return true;
        }

        let Some(pending) = self.pending_listening_cue else {
            return true;
        };
        if pending.utterance_id != event.utterance_id
            || pending.draft_id
                != match event.owner {
                    speak::SpeechOwner::Draft(draft_id) => draft_id,
                    speak::SpeechOwner::Turn(_) | speak::SpeechOwner::System => return true,
                }
        {
            return true;
        }

        // Clear before any await. A release or Escape that races with this
        // event therefore cannot leave a second path able to open capture.
        self.pending_listening_cue = None;
        match event.outcome {
            speak::SpeechOutcome::Played => {
                speak::process("listening_cue_completed", "opening microphone capture");
                self.start_capture_after_cue(pending.draft_id).await
            }
            speak::SpeechOutcome::SynthesisFailed | speak::SpeechOutcome::PlaybackFailed => {
                // Spoken output is optional: a missing Piper install or output
                // device does not make the established voice input unusable.
                speak::warning(
                    "listening_cue_failed",
                    "starting microphone capture without the listening cue",
                );
                self.start_capture_after_cue(pending.draft_id).await
            }
            speak::SpeechOutcome::Cancelled => {
                // Cancellation is intentional (new press, release, Escape,
                // or shutdown). It must never open the microphone later.
                speak::process(
                    "listening_cue_cancelled",
                    "microphone capture was not started",
                );
                true
            }
        }
    }

    /// Starts the pre-existing DictationSession only after the cue boundary.
    /// This explicitly reuses its recording-ID lifecycle instead of inventing
    /// another microphone owner for speech output.
    async fn start_capture_after_cue(&mut self, draft_id: u64) -> bool {
        let current = self.draft.as_ref().is_some_and(|draft| {
            draft.id == draft_id
                && draft.held
                && !draft.released
                && !draft.cancellation.is_cancelled()
                && draft.recording_id.is_none()
        });
        if !current {
            return true;
        }

        if let Some(draft) = self.draft.as_ref() {
            draft.interaction.mark("speech.listening_cue_completed");
            self.dictation
                .set_pending_interaction(draft.interaction.clone());
        }

        for action in self
            .dictation
            .handle(DictationEvent::Input(DictationInput::HoldStart))
            .await
        {
            if !self.handle_dictation_action(action).await {
                return false;
            }
        }
        true
    }

    fn release_draft(&mut self) {
        if self.pending_listening_cue.take().is_some() {
            // The person released during the cue. There is no recording to
            // finish, and the late completion must not open a microphone.
            self.cancel_draft();
            self.speech
                .handle()
                .cancel_all("enter_released_during_listening_cue");
            speak::voice_log("Enter released during listening cue; no microphone capture started");
            return;
        }

        let should_announce = if let Some(draft) = self.draft.as_mut() {
            if draft.held && !draft.released {
                draft.held = false;
                draft.released = true;
                draft.snapshot_requested = false;
                true
            } else {
                false
            }
        } else {
            false
        };

        if should_announce {
            if let Some(draft) = self.draft.as_ref() {
                draft.interaction.mark("voice.enter_released");
            }
            self.cancel_live_transcription();
            // The thinking cue is deliberately delayed until DictationOutput::
            // Transcribing, after the microphone has actually been released.
            speak::voice_log("recording released; waiting for microphone cleanup");
        }
    }

    async fn request_photo(&mut self) {
        if !self.turns.supports_images() {
            speak::warning(
                "photo_capture_unavailable",
                self.turns.client.image_input_restriction(),
            );
            return;
        }
        let queued = {
            let Some(draft) = self.draft.as_mut() else {
                return;
            };
            // `P` is meaningful only while actual capture is open. In
            // particular, it cannot activate a camera while the listening cue
            // is still playing before microphone capture begins.
            if !draft.held
                || !draft.recording_started
                || draft.released
                || draft.cancellation.is_cancelled()
            {
                return;
            }
            if draft.pending_photo_count() >= MAX_QUEUED_PHOTO_CAPTURES {
                speak::voice_log(&format!(
                    "photo capture request rejected — {MAX_QUEUED_PHOTO_CAPTURES} captures are already waiting for the camera"
                ));
                return;
            }

            let request_id = draft.next_photo_request;
            draft.next_photo_request = draft
                .next_photo_request
                .checked_add(1)
                .expect("voice photo request identifiers exhausted");
            draft.photos.insert(request_id, DraftPhoto::Queued);
            draft.interaction.event(
                "photo.request",
                "queued",
                telemetry::fields([("draft_photo_id", serde_json::json!(request_id))]),
            );
            true
        };

        if queued {
            self.start_next_photo_capture();
        }
    }

    fn start_next_photo_capture(&mut self) {
        if self.active_photo_capture.is_some() {
            return;
        }
        let request = self.draft.as_mut().and_then(|draft| {
            if draft.cancellation.is_cancelled() {
                return None;
            }
            let request_id = draft.photos.iter().find_map(|(request_id, photo)| {
                matches!(photo, DraftPhoto::Queued).then_some(*request_id)
            })?;
            *draft
                .photos
                .get_mut(&request_id)
                .expect("queued photo request was just found") = DraftPhoto::Capturing;
            Some((
                draft.id,
                request_id,
                draft.cancellation.clone(),
                draft.interaction.clone(),
            ))
        });
        let Some((draft_id, request_id, cancellation, interaction)) = request else {
            return;
        };

        self.active_photo_capture = Some((draft_id, request_id));
        let camera_service = self.turns.camera.clone();
        let event_sender = self.aux_sender.clone();
        tokio::spawn(async move {
            let result = match camera_service.list().await {
                Ok(cameras) => match select_camera(&cameras, None) {
                    Ok(camera) => {
                        match camera_service
                            .capture_with_telemetry(camera, cancellation.clone(), Some(interaction))
                            .await
                        {
                            Ok(photo) => Ok(photo),
                            Err(CameraRunError::Cancelled) => return,
                            Err(CameraRunError::Failed(error)) => Err(error.to_string()),
                        }
                    }
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(error.to_string()),
            };

            if cancellation.is_cancelled() {
                return;
            }
            let _ = event_sender
                .send(VoiceAuxEvent::PhotoCaptureFinished {
                    draft_id,
                    request_id,
                    result,
                })
                .await;
        });
    }

    fn request_partial_snapshot(&mut self) {
        let recording_id = self.draft.as_mut().and_then(|draft| {
            if draft.held
                && !draft.released
                && draft.recording_started
                && !draft.snapshot_requested
                && self.live_transcription.is_none()
                && self.pending_snapshot.is_none()
            {
                draft.snapshot_requested = true;
                draft.recording_id
            } else {
                None
            }
        });
        if let Some(recording_id) = recording_id {
            self.dictation.request_snapshot(recording_id);
        }
    }

    fn handle_recording_snapshot(&mut self, recording_id: RecordingId, audio: CapturedAudio) {
        let current = self.draft.as_mut().is_some_and(|draft| {
            if draft.recording_id == Some(recording_id) {
                draft.snapshot_requested = false;
                draft.held && !draft.released && !draft.cancellation.is_cancelled()
            } else {
                false
            }
        });
        if !current {
            return;
        }

        if self.live_transcription.is_some() {
            // Keep only the newest full snapshot. Each result replaces prior
            // partial text, so retaining every intermediate clip would waste
            // CPU without adding information.
            self.pending_snapshot = Some((recording_id, audio));
            return;
        }
        self.start_partial_transcription(recording_id, audio);
    }

    fn start_partial_transcription(&mut self, recording_id: RecordingId, audio: CapturedAudio) {
        let sequence = self.next_partial_sequence;
        self.next_partial_sequence = self
            .next_partial_sequence
            .checked_add(1)
            .expect("partial transcription identifiers exhausted");
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let transcriber: Arc<dyn Transcriber> = self.dictation.transcriber();
        let event_sender = self.aux_sender.clone();
        let interaction = self.draft.as_ref().and_then(|draft| {
            (draft.recording_id == Some(recording_id)).then(|| draft.interaction.clone())
        });
        let queued_at = std::time::Instant::now();
        let in_flight = interaction
            .as_ref()
            .map(|interaction| interaction.in_flight_guard("whisper"));
        let handle = tokio::spawn(async move {
            let _in_flight = in_flight;
            let recognition_cancelled = worker_cancelled.clone();
            let worker_interaction = interaction.clone();
            let result = tokio::task::spawn_blocking(move || {
                if let Some(interaction) = &worker_interaction {
                    interaction.queue_wait("whisper", queued_at.elapsed(), 0, 1);
                }
                transcriber.transcribe(
                    audio,
                    recognition_cancelled,
                    crate::dictation::transcribe::TranscriptionKind::Partial,
                    worker_interaction.as_ref(),
                )
            })
            .await;
            if worker_cancelled.load(Ordering::Acquire) {
                return;
            }
            let result = match result {
                Ok(Ok(text)) => Ok(text),
                Ok(Err(TranscriptionError::Cancelled)) => return,
                Ok(Err(TranscriptionError::Failed(message))) => Err(message),
                Err(error) => Err(format!(
                    "local partial transcription worker ended unexpectedly: {error}"
                )),
            };
            let _ = event_sender
                .send(VoiceAuxEvent::PartialTranscript {
                    recording_id,
                    sequence,
                    result,
                })
                .await;
        });
        self.live_transcription = Some(LiveTranscription {
            recording_id,
            sequence,
            cancelled,
            handle,
        });
    }

    async fn handle_aux_event(&mut self, event: VoiceAuxEvent) -> bool {
        match event {
            VoiceAuxEvent::PartialTranscript {
                recording_id,
                sequence,
                result,
            } => self.handle_partial_transcript(recording_id, sequence, result),
            VoiceAuxEvent::PhotoCaptureFinished {
                draft_id,
                request_id,
                result,
            } => {
                self.handle_photo_capture_finished(draft_id, request_id, result);
                self.start_next_photo_capture();
                self.try_submit().await
            }
        }
    }

    fn handle_partial_transcript(
        &mut self,
        recording_id: RecordingId,
        sequence: u64,
        result: Result<String, String>,
    ) -> bool {
        let Some(live) = self.live_transcription.take() else {
            return true;
        };
        if live.recording_id != recording_id || live.sequence != sequence {
            self.live_transcription = Some(live);
            return true;
        }
        drop(live.handle);

        let mut partial = None;
        let mut failure = None;
        if let Some(draft) = self.draft.as_mut()
            && draft.recording_id == Some(recording_id)
            && draft.held
            && !draft.released
            && !draft.cancellation.is_cancelled()
        {
            match result {
                Ok(text) if draft.last_partial.as_deref() != Some(text.as_str()) => {
                    draft.last_partial = Some(text.clone());
                    draft.last_partial_error = None;
                    partial = Some(text);
                }
                Ok(_) => {}
                Err(error) if draft.last_partial_error.as_deref() != Some(error.as_str()) => {
                    draft.last_partial_error = Some(error.clone());
                    failure = Some(error);
                }
                Err(_) => {}
            }
        }
        if let Some(text) = partial {
            speak::user_transcript("partial", &text);
        }
        if let Some(error) = failure {
            speak::warning("partial_transcription_failed", error);
        }

        if let Some((pending_id, audio)) = self.pending_snapshot.take() {
            self.start_partial_transcription(pending_id, audio);
        }
        true
    }

    fn handle_photo_capture_finished(
        &mut self,
        draft_id: u64,
        request_id: u64,
        result: Result<Photo, String>,
    ) {
        let message = {
            if self.active_photo_capture != Some((draft_id, request_id)) {
                return;
            }
            self.active_photo_capture = None;
            let Some(draft) = self.draft.as_mut() else {
                return;
            };
            if draft.id != draft_id || draft.cancellation.is_cancelled() {
                return;
            }
            let Some(slot) = draft.photos.get_mut(&request_id) else {
                return;
            };
            if !matches!(slot, DraftPhoto::Capturing) {
                return;
            }

            match result {
                Ok(photo) => {
                    let total = match draft.attached_photo_bytes.checked_add(photo.bytes().len()) {
                        Some(total) => total,
                        None => {
                            *slot = DraftPhoto::Failed;
                            MAX_DRAFT_PHOTO_BYTES + 1
                        }
                    };
                    if total > MAX_DRAFT_PHOTO_BYTES {
                        *slot = DraftPhoto::Failed;
                        format!(
                            "photo capture rejected — attached photos would exceed the {} MiB in-memory draft limit",
                            MAX_DRAFT_PHOTO_BYTES / (1024 * 1024)
                        )
                    } else {
                        draft.attached_photo_bytes = total;
                        *slot = DraftPhoto::Captured(photo);
                        let count = draft
                            .photos
                            .values()
                            .filter(|photo| matches!(photo, DraftPhoto::Captured(_)))
                            .count();
                        format!("photo captured — {count} photos attached")
                    }
                }
                Err(error) => {
                    *slot = DraftPhoto::Failed;
                    format!("photo capture failed — {error}")
                }
            }
        };
        speak::voice_log(&message);
    }

    async fn try_submit(&mut self) -> bool {
        let submission = self.draft.as_mut().and_then(|draft| {
            if !draft.released
                || draft.final_transcript.is_none()
                || draft.has_pending_photos()
                || draft.submitted
                || draft.cancellation.is_cancelled()
            {
                return None;
            }
            draft.submitted = true;
            Some((
                draft
                    .final_transcript
                    .clone()
                    .expect("transcript was checked"),
                draft.captured_photos(),
                draft.interaction.clone(),
            ))
        });
        let Some((transcript, photos, interaction)) = submission else {
            return true;
        };

        speak::voice_submission(&transcript, photos.len());
        let input = if photos.is_empty() {
            Input::Ask(transcript)
        } else {
            Input::AskWithPhotos {
                question: transcript,
                photos,
            }
        };
        self.turns.set_pending_interaction(interaction);
        self.draft = None;
        self.handle_main_event(Event::Input(input)).await
    }

    fn draft_matches_recording(&self, recording_id: RecordingId) -> bool {
        self.draft
            .as_ref()
            .is_some_and(|draft| draft.recording_id == Some(recording_id))
    }

    fn cancel_live_transcription(&mut self) {
        self.pending_snapshot = None;
        if let Some(live) = self.live_transcription.take() {
            live.cancelled.store(true, Ordering::Release);
            live.handle.abort();
        }
    }

    fn cancel_draft(&mut self) {
        self.cancel_live_transcription();
        self.pending_listening_cue = None;
        self.active_photo_capture = None;
        if let Some(draft) = self.draft.take() {
            draft.cancellation.cancel();
            draft.interaction.mark("cancellation.requested");
            if let Some(recording_id) = draft.recording_id {
                self.cancelled_recordings
                    .insert(recording_id, draft.interaction);
            } else {
                // A cue-only draft has no microphone/Whisper handle to wait
                // for. Its local cleanup is complete once the generation was
                // invalidated above.
                draft
                    .interaction
                    .mark("cancellation.local_cleanup_completed");
                draft
                    .interaction
                    .complete("cancelled", telemetry::Fields::new());
            }
        }
    }

    fn complete_pending_recording(&mut self, recording_id: RecordingId, outcome: &'static str) {
        if let Some(interaction) = self.cancelled_recordings.remove(&recording_id) {
            if outcome == "cancelled" {
                interaction.mark("cancellation.local_cleanup_completed");
            } else {
                interaction.mark("dictation.cleanup_completed_after_failure");
            }
            interaction.complete(outcome, telemetry::Fields::new());
        }
    }

    async fn shutdown(&mut self) {
        self.speech.handle().cancel_all("voice_shutdown");
        self.cancel_draft();
        self.dictation.shutdown().await;
        for (_, interaction) in self.cancelled_recordings.drain() {
            interaction.mark("cancellation.local_cleanup_completed");
            interaction.complete("cancelled", telemetry::Fields::new());
        }
        self.turns.shutdown().await;
        self.speech.shutdown();
        // The worker is joined above, so any remaining final-answer request
        // has completed its local cancellation cleanup even though the event
        // loop is no longer running to receive its `SpeechEvent`.
        for (_, interaction) in self.pending_answer_speech.drain() {
            interaction.mark("speech.final_answer.cleanup_completed");
            interaction.complete("accepted_text_speech_cancelled", telemetry::Fields::new());
        }
    }
}

#[cfg(test)]
async fn run_turn(
    client: Arc<OllamaClient>,
    camera_service: CameraService,
    event_sender: mpsc::Sender<Event>,
    request: TurnRequest,
    cancellation: CancellationToken,
) -> TurnResult {
    let interaction = telemetry::start_interaction("direct", None);
    interaction.bind_turn(request.id);
    run_turn_with_interaction(
        client,
        camera_service,
        event_sender,
        request,
        cancellation,
        interaction,
    )
    .await
}

async fn run_turn_with_interaction(
    client: Arc<OllamaClient>,
    camera_service: CameraService,
    event_sender: mpsc::Sender<Event>,
    request: TurnRequest,
    cancellation: CancellationToken,
    interaction: Interaction,
) -> TurnResult {
    let TurnRequest {
        id,
        question,
        history,
        visual,
        ..
    } = request;

    let (images, proposed_scene) = match visual {
        VisualRequest::UseAccepted(photos) => (photos, None),
        VisualRequest::ProposeNew(path) => {
            interaction.event(
                "photo.request",
                "explicit_file_selected",
                telemetry::Fields::new(),
            );
            let mut loading = Some(interaction.stage(
                "photo.file.load_validate",
                telemetry::fields([("source", serde_json::json!("explicit_file"))]),
            ));
            let mut load_task = tokio::task::spawn_blocking(move || Photo::load(path));
            let photo = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    load_task.abort();
                    if let Some(stage) = loading.take() {
                        stage.finish("cancelled", telemetry::Fields::new());
                    }
                    return TurnResult::Cancelled;
                }
                result = &mut load_task => match result {
                    Ok(Ok(photo)) => {
                        if let Some(stage) = loading.take() {
                            stage.finish(
                                "completed",
                                telemetry::fields([("encoded_image_bytes", serde_json::json!(photo.bytes().len()))]),
                            );
                        }
                        photo
                    },
                    Ok(Err(error)) => {
                        if let Some(stage) = loading.take() {
                            stage.finish(
                                "failed",
                                telemetry::fields([(
                                    "failure_class",
                                    serde_json::json!("photo_read_or_validation_failure"),
                                )]),
                            );
                        }
                        return TurnResult::Failed {
                            message: format!("could not use photo: {error}"),
                        };
                    }
                    Err(error) => {
                        if let Some(stage) = loading.take() {
                            stage.finish(
                                "worker_failure",
                                telemetry::fields([(
                                    "failure_class",
                                    serde_json::json!("blocking_worker_failure"),
                                )]),
                            );
                        }
                        return TurnResult::Failed {
                            message: format!("photo loading task ended unexpectedly: {error}"),
                        };
                    }
                },
            };

            if cancellation.is_cancelled() {
                return TurnResult::Cancelled;
            }

            (vec![photo.clone()], Some(photo))
        }
        VisualRequest::ProposeCamera(camera) => {
            interaction.event(
                "photo.request",
                "camera_capture_requested",
                telemetry::fields([("camera_id", serde_json::json!(camera.id().to_string()))]),
            );
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

            let photo = match camera_service
                .capture_with_telemetry(camera, cancellation.clone(), Some(interaction.clone()))
                .await
            {
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

            (vec![photo.clone()], Some(photo))
        }
    };

    let image_bytes = images.iter().map(Photo::bytes).collect::<Vec<_>>();
    interaction.set_photo_count(image_bytes.len());
    interaction.mark("ollama.request_dispatch_started");
    let _ollama_in_flight = interaction.in_flight_guard("ollama");

    let answer = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            return TurnResult::Cancelled;
        },
        answer = client.answer_with_telemetry(&history, &question, &image_bytes, Some(&interaction)) => answer,
    };

    if cancellation.is_cancelled() {
        return TurnResult::Cancelled;
    }

    match answer {
        Ok(answer) => {
            interaction.mark("ollama.response_received");
            TurnResult::Answer {
                answer: answer.text,
                proposed_scene,
            }
        }
        Err(error) => {
            interaction.event(
                "ollama.request.failed",
                error.classification(),
                telemetry::Fields::new(),
            );
            TurnResult::Failed {
                message: format!("local inference failed: {error}"),
            }
        }
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

    #[error("could not inspect Linux input devices: {0}")]
    InputDevices(#[source] evdev::InputDeviceError),

    #[error("could not start the Linux button controller: {0}")]
    ButtonInput(#[source] evdev::InputDeviceError),

    #[error("could not resolve the hardware profile: {0}")]
    Profile(#[source] profile::ProfileError),

    #[error("could not start the local speech worker: {0}")]
    Speech(#[source] io::Error),

    #[error("could not inspect local audio output devices: {0}")]
    AudioOutput(#[source] speak::playback::PlaybackError),

    #[error("could not inspect local audio input devices: {0}")]
    AudioInput(String),

    #[error("the local speech worker could not queue the test sentence")]
    SpeechTestUnavailable,

    #[error("the local speech test timed out before playback completed")]
    SpeechTestTimedOut,

    #[error("the local speech test received an unexpected worker completion")]
    SpeechTestUnexpectedEvent,

    #[error("the local speech test did not complete successfully: {0:?}")]
    SpeechTestFailed(speak::SpeechOutcome),

    #[error("could not run local dictation: {0}")]
    Dictation(#[from] dictation::DictationError),
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
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    use crate::{
        controller::Controller,
        dictation::{
            DictationSession,
            audio::{MicrophoneDescription, Recorder, RecordingHandle},
            transcribe::{Transcriber, TranscriptionError},
        },
        listen::controls::{
            FakeVoiceInputSource, VoiceControlEvent, VoiceInputEvent, VoiceInputSource,
        },
        protocol::{
            CameraPreparation, CameraRequestId, CapturedAudio, DictationEvent, DictationInput,
            Effect, Event, Input, RecordingId, TurnFinished, TurnId, TurnRequest, TurnResult,
            VisualRequest,
        },
        see::{
            MAX_PHOTO_BYTES, Photo, PhotoSource,
            camera::{
                CameraBackend, CameraDescriptor, CameraError, CameraId, CameraPhotoSource,
                CameraService,
            },
        },
        think::{ImageInputSupport, OllamaClient, OllamaConfig, TEACHER_INSTRUCTIONS},
    };

    use super::{TurnRuntime, VoiceRuntime, run_turn};
    use crate::speak::{SpeechEvent, SpeechOutcome, SpeechOwner, SpeechService};

    fn client(server: &MockServer) -> Arc<OllamaClient> {
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "test-model").unwrap()).unwrap();
        client.set_capabilities_for_test(&["vision"]);
        Arc::new(client)
    }

    async fn within_test_timeout<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .expect("worker test exceeded its three-second timeout")
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
            _interaction: Option<crate::telemetry::Interaction>,
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

        fn snapshot(&self) {
            let _ = self
                .event_sender
                .try_send(DictationEvent::RecordingSnapshot {
                    recording_id: self.recording_id,
                    audio: CapturedAudio::new(vec![0.1; 160]),
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
            _kind: crate::dictation::transcribe::TranscriptionKind,
            _interaction: Option<&crate::telemetry::Interaction>,
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
        aux_events: mpsc::Receiver<super::VoiceAuxEvent>,
        recorder: Arc<VoiceRecorder>,
        transcriber: Arc<VoiceTranscriber>,
    }

    impl VoiceHarness {
        async fn next_dictation_event(&mut self) -> DictationEvent {
            within_test_timeout(self.dictation_events.recv())
                .await
                .expect("synthetic dictation event")
        }

        async fn next_aux_event(&mut self) -> super::VoiceAuxEvent {
            within_test_timeout(self.aux_events.recv())
                .await
                .expect("synthetic voice auxiliary event")
        }

        async fn start_hold(&mut self) -> RecordingId {
            assert!(
                self.runtime
                    .handle_dictation_event(DictationEvent::Input(DictationInput::HoldStart))
                    .await
            );
            let cue = self
                .runtime
                .pending_listening_cue
                .expect("holding Enter should queue a listening cue");
            assert!(
                self.runtime
                    .handle_speech_event(SpeechEvent {
                        utterance_id: cue.utterance_id,
                        owner: SpeechOwner::Draft(cue.draft_id),
                        outcome: SpeechOutcome::Played,
                    })
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
            recording_id
        }

        async fn release_hold(&mut self, recording_id: RecordingId) -> String {
            assert!(
                self.runtime
                    .handle_dictation_event(DictationEvent::Input(DictationInput::HoldEnd))
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
            text
        }

        async fn submit_next_transcript(&mut self) -> (RecordingId, String) {
            let recording_id = self.start_hold().await;
            let text = self.release_hold(recording_id).await;
            (recording_id, text)
        }

        async fn accept_next_answer(&mut self) {
            let event = within_test_timeout(self.main_events.recv())
                .await
                .expect("mock Ollama completion");
            assert!(matches!(event, Event::TurnFinished(_)));
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
        let (aux_sender, aux_events) = mpsc::channel(super::VOICE_AUX_CHANNEL_CAPACITY);
        let recorder = Arc::new(VoiceRecorder::default());
        let transcriber = Arc::new(VoiceTranscriber::new(transcripts));
        let dictation = DictationSession::with_services(
            recorder.clone(),
            transcriber.clone(),
            dictation_sender,
        );
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "test-model").unwrap()).unwrap();
        client.set_capabilities_for_test(&["vision"]);

        VoiceHarness {
            runtime: VoiceRuntime::new(
                Controller::new(),
                TurnRuntime::new(client, camera, main_sender),
                dictation,
                aux_sender,
                SpeechService::test_stub(),
            ),
            dictation_events,
            main_events,
            aux_events,
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

    fn tiny_colored_jpeg(red: u8, green: u8, blue: u8) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([red, green, blue]));
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, ImageFormat::Jpeg)
            .unwrap();
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

        fn capture(
            &self,
            _camera: &CameraDescriptor,
            _cancellation: &CancellationToken,
        ) -> Result<Photo, CameraError> {
            self.captures.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(CameraError::NoCameras)
            } else {
                Ok(self.photo.clone())
            }
        }
    }

    struct SequencedCameraBackend {
        camera: CameraDescriptor,
        photos: Mutex<VecDeque<Photo>>,
        captures: Arc<AtomicUsize>,
    }

    impl CameraBackend for SequencedCameraBackend {
        fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
            Ok(vec![self.camera.clone()])
        }

        fn capture(
            &self,
            _camera: &CameraDescriptor,
            _cancellation: &CancellationToken,
        ) -> Result<Photo, CameraError> {
            self.captures.fetch_add(1, Ordering::SeqCst);
            self.photos
                .lock()
                .expect("synthetic camera sequence lock")
                .pop_front()
                .ok_or(CameraError::NoCameras)
        }
    }

    struct UnexpectedCameraBackend {
        list_calls: Arc<AtomicUsize>,
        capture_calls: Arc<AtomicUsize>,
    }

    impl CameraBackend for UnexpectedCameraBackend {
        fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            Err(CameraError::NoCameras)
        }

        fn capture(
            &self,
            _camera: &CameraDescriptor,
            _cancellation: &CancellationToken,
        ) -> Result<Photo, CameraError> {
            self.capture_calls.fetch_add(1, Ordering::SeqCst);
            Err(CameraError::NoCameras)
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

    #[tokio::test]
    async fn fake_button_source_uses_the_same_talk_press_and_release_path() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(
            &server,
            ["literal button question".to_owned()],
            unused_camera_service(),
        );
        let (sender, mut receiver) = mpsc::channel(super::EVENT_CHANNEL_CAPACITY);
        let mut source = FakeVoiceInputSource::new(sender);

        source
            .emit(VoiceInputEvent::Control(VoiceControlEvent::TalkPressed))
            .await;
        let event = receiver.recv().await.expect("fake press event");
        assert!(harness.runtime.handle_control_event(event).await);
        let cue = harness
            .runtime
            .pending_listening_cue
            .expect("button press should queue the existing listening cue");
        assert!(
            harness
                .runtime
                .handle_speech_event(SpeechEvent {
                    utterance_id: cue.utterance_id,
                    owner: SpeechOwner::Draft(cue.draft_id),
                    outcome: SpeechOutcome::Played,
                })
                .await
        );
        let started = harness.next_dictation_event().await;
        let recording_id = match started {
            DictationEvent::RecordingStarted { recording_id } => recording_id,
            event => panic!("expected recording start from button press, got {event:?}"),
        };
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::RecordingStarted { recording_id })
                .await
        );

        source
            .emit(VoiceInputEvent::Control(VoiceControlEvent::TalkReleased))
            .await;
        let event = receiver.recv().await.expect("fake release event");
        assert!(harness.runtime.handle_control_event(event).await);
        assert_eq!(harness.recorder.finishes.load(Ordering::SeqCst), 1);

        source.shutdown();
        assert!(source.was_shutdown());
    }

    #[tokio::test]
    async fn button_photo_and_cancel_controls_use_the_common_voice_runtime_path() {
        let server = MockServer::start().await;
        let (camera, photo) = fake_camera();
        let captures = Arc::new(AtomicUsize::new(0));
        let camera_service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera,
            photo,
            fail: false,
            captures: captures.clone(),
        }));
        let mut harness = voice_harness(
            &server,
            ["a draft that must be cancelled".to_owned()],
            camera_service,
        );
        let (sender, mut receiver) = mpsc::channel(super::EVENT_CHANNEL_CAPACITY);
        let mut source = FakeVoiceInputSource::new(sender);

        source
            .emit(VoiceInputEvent::Control(VoiceControlEvent::TalkPressed))
            .await;
        assert!(
            harness
                .runtime
                .handle_control_event(receiver.recv().await.expect("button press event"))
                .await
        );
        let cue = harness
            .runtime
            .pending_listening_cue
            .expect("talk press should start the established listening cue");
        assert!(
            harness
                .runtime
                .handle_speech_event(SpeechEvent {
                    utterance_id: cue.utterance_id,
                    owner: SpeechOwner::Draft(cue.draft_id),
                    outcome: SpeechOutcome::Played,
                })
                .await
        );
        let recording_id = match harness.next_dictation_event().await {
            DictationEvent::RecordingStarted { recording_id } => recording_id,
            event => panic!("expected recording start, got {event:?}"),
        };
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::RecordingStarted { recording_id })
                .await
        );

        // A `P` press is a separate control event, not typed text. It uses
        // the same ordered photo path as the terminal adapter once recording
        // is actually open.
        source
            .emit(VoiceInputEvent::Control(VoiceControlEvent::PhotoPressed))
            .await;
        assert!(
            harness
                .runtime
                .handle_control_event(receiver.recv().await.expect("photo press event"))
                .await
        );
        let photo_finished = harness.next_aux_event().await;
        assert!(matches!(
            &photo_finished,
            super::VoiceAuxEvent::PhotoCaptureFinished { result: Ok(_), .. }
        ));
        assert!(harness.runtime.handle_aux_event(photo_finished).await);
        assert_eq!(captures.load(Ordering::SeqCst), 1);

        source
            .emit(VoiceInputEvent::Control(VoiceControlEvent::Cancel))
            .await;
        assert!(
            harness
                .runtime
                .handle_control_event(receiver.recv().await.expect("cancel event"))
                .await
        );
        let cancelled = harness.next_dictation_event().await;
        assert!(matches!(
            &cancelled,
            DictationEvent::RecordingCancelled { recording_id: id } if *id == recording_id
        ));
        assert!(harness.runtime.handle_dictation_event(cancelled).await);
        assert!(harness.runtime.draft.is_none());
        assert!(!harness.runtime.controller.has_active_turn());
        assert!(server.received_requests().await.unwrap().is_empty());

        source.shutdown();
        assert!(source.was_shutdown());
    }

    #[tokio::test]
    async fn button_source_loss_cancels_incomplete_capture_and_recovery_needs_a_fresh_press() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(
            &server,
            ["should never submit".to_owned()],
            unused_camera_service(),
        );
        let _recording_id = harness.start_hold().await;

        assert!(
            harness
                .runtime
                .handle_control_event(VoiceInputEvent::SourceLost {
                    source: "synthetic buttons",
                    message: "device disconnected".into(),
                })
                .await
        );
        let cancelled = harness.next_dictation_event().await;
        assert!(matches!(
            cancelled,
            DictationEvent::RecordingCancelled { .. }
        ));
        assert!(harness.runtime.handle_dictation_event(cancelled).await);
        assert_eq!(harness.recorder.cancels.load(Ordering::SeqCst), 1);
        assert!(harness.runtime.draft.is_none());

        assert!(
            harness
                .runtime
                .handle_control_event(VoiceInputEvent::SourceRecovered {
                    source: "synthetic buttons",
                })
                .await
        );
        assert!(harness.main_events.try_recv().is_err());
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
    async fn text_only_model_rejects_camera_preparation_before_device_access() {
        let server = MockServer::start().await;
        let client = OllamaClient::new(
            OllamaConfig::new_with_image_support(
                &server.uri(),
                "text-only-model",
                ImageInputSupport::TextOnly,
            )
            .unwrap(),
        )
        .unwrap();
        client.set_capabilities_for_test(&[]);
        let list_calls = Arc::new(AtomicUsize::new(0));
        let capture_calls = Arc::new(AtomicUsize::new(0));
        let camera = CameraService::with_backend(Arc::new(UnexpectedCameraBackend {
            list_calls: list_calls.clone(),
            capture_calls: capture_calls.clone(),
        }));
        let (sender, mut receiver) = mpsc::channel(super::EVENT_CHANNEL_CAPACITY);
        let runtime = TurnRuntime::new(client, camera, sender);

        runtime.prepare_camera(CameraPreparation {
            id: CameraRequestId::new(1),
            requested: None,
        });
        let event = within_test_timeout(receiver.recv())
            .await
            .expect("text-only camera rejection");
        assert!(matches!(
            event,
            Event::CameraPrepared {
                result: Err(message),
                ..
            } if message.contains("does not advertise vision")
        ));
        assert_eq!(list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(capture_calls.load(Ordering::SeqCst), 0);
        assert!(server.received_requests().await.unwrap().is_empty());
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
    async fn hold_release_runs_live_partial_transcription_without_duplicate_draft_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "final spoken question"}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "final answer"},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let mut harness = voice_harness(
            &server,
            [
                "partial tree words".to_owned(),
                "partial tree words".to_owned(),
                "final spoken question".to_owned(),
            ],
            unused_camera_service(),
        );
        let recording_id = harness.start_hold().await;

        // Repeated press does not allocate another recording while held.
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldStart))
                .await
        );
        assert_eq!(harness.recorder.starts.load(Ordering::SeqCst), 1);
        assert!(
            harness
                .runtime
                .draft
                .as_ref()
                .and_then(|draft| draft.final_transcript.as_ref())
                .is_none()
        );
        assert!(harness.runtime.controller.accepted_history().is_empty());

        for _ in 0..2 {
            harness.runtime.request_partial_snapshot();
            let snapshot = harness.next_dictation_event().await;
            assert!(matches!(
                &snapshot,
                DictationEvent::RecordingSnapshot { recording_id: id, .. } if *id == recording_id
            ));
            assert!(harness.runtime.handle_dictation_event(snapshot).await);
            let partial = harness.next_aux_event().await;
            assert!(matches!(
                &partial,
                super::VoiceAuxEvent::PartialTranscript {
                    recording_id: id,
                    result: Ok(_),
                    ..
                } if *id == recording_id
            ));
            assert!(harness.runtime.handle_aux_event(partial).await);
        }

        // Each partial is a full replacement, never an append of the same
        // words from the previous snapshot.
        assert_eq!(
            harness
                .runtime
                .draft
                .as_ref()
                .and_then(|draft| draft.last_partial.as_deref()),
            Some("partial tree words")
        );

        assert_eq!(
            harness.release_hold(recording_id).await,
            "final spoken question"
        );
        harness.accept_next_answer().await;
        assert_eq!(harness.recorder.finishes.load(Ordering::SeqCst), 1);
        assert_eq!(harness.transcriber.calls.load(Ordering::SeqCst), 3);
        assert_eq!(harness.runtime.controller.accepted_history().len(), 1);
    }

    #[tokio::test]
    async fn microphone_stays_closed_until_the_listening_cue_completes() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(&server, ["unused".to_owned()], unused_camera_service());

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldStart))
                .await
        );
        let cue = harness
            .runtime
            .pending_listening_cue
            .expect("hold should queue a listening cue");
        assert_eq!(harness.recorder.starts.load(Ordering::SeqCst), 0);
        assert!(harness.dictation_events.try_recv().is_err());

        assert!(
            harness
                .runtime
                .handle_speech_event(SpeechEvent {
                    utterance_id: cue.utterance_id,
                    owner: SpeechOwner::Draft(cue.draft_id),
                    outcome: SpeechOutcome::Played,
                })
                .await
        );
        assert_eq!(harness.recorder.starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn release_or_escape_during_listening_cue_cannot_open_a_late_microphone() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(&server, ["unused".to_owned()], unused_camera_service());

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldStart))
                .await
        );
        let released_cue = harness
            .runtime
            .pending_listening_cue
            .expect("hold should queue a listening cue");
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldEnd))
                .await
        );
        assert!(
            harness
                .runtime
                .handle_speech_event(SpeechEvent {
                    utterance_id: released_cue.utterance_id,
                    owner: SpeechOwner::Draft(released_cue.draft_id),
                    outcome: SpeechOutcome::Played,
                })
                .await
        );
        assert_eq!(harness.recorder.starts.load(Ordering::SeqCst), 0);

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldStart))
                .await
        );
        let escaped_cue = harness
            .runtime
            .pending_listening_cue
            .expect("second hold should queue a listening cue");
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
                .await
        );
        assert!(
            harness
                .runtime
                .handle_speech_event(SpeechEvent {
                    utterance_id: escaped_cue.utterance_id,
                    owner: SpeechOwner::Draft(escaped_cue.draft_id),
                    outcome: SpeechOutcome::Played,
                })
                .await
        );
        assert_eq!(harness.recorder.starts.load(Ordering::SeqCst), 0);
        assert!(harness.runtime.draft.is_none());
    }

    #[tokio::test]
    async fn a_failed_listening_cue_leaves_voice_input_usable_without_speaking() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(&server, ["unused".to_owned()], unused_camera_service());

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldStart))
                .await
        );
        let cue = harness
            .runtime
            .pending_listening_cue
            .expect("hold should queue a listening cue");
        assert!(
            harness
                .runtime
                .handle_speech_event(SpeechEvent {
                    utterance_id: cue.utterance_id,
                    owner: SpeechOwner::Draft(cue.draft_id),
                    outcome: SpeechOutcome::PlaybackFailed,
                })
                .await
        );

        assert!(matches!(
            harness.next_dictation_event().await,
            DictationEvent::RecordingStarted { .. }
        ));
        assert_eq!(harness.recorder.starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn p_during_the_listening_cue_does_not_activate_a_camera() {
        let server = MockServer::start().await;
        let (camera, photo) = fake_camera();
        let captures = Arc::new(AtomicUsize::new(0));
        let camera_service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera,
            photo,
            fail: false,
            captures: captures.clone(),
        }));
        let mut harness = voice_harness(&server, ["unused".to_owned()], camera_service);

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldStart))
                .await
        );
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        tokio::task::yield_now().await;
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert!(harness.runtime.active_photo_capture.is_none());
    }

    #[tokio::test]
    async fn repeated_p_presses_submit_four_photos_in_press_order_with_one_request() {
        let server = MockServer::start().await;
        let camera = CameraDescriptor::new(CameraId::new(11), "Sequenced Camera");
        let image_bytes = [
            tiny_colored_jpeg(255, 0, 0),
            tiny_colored_jpeg(0, 255, 0),
            tiny_colored_jpeg(0, 0, 255),
            tiny_colored_jpeg(255, 255, 0),
        ];
        let expected_images = image_bytes
            .iter()
            .map(|bytes| STANDARD.encode(bytes))
            .collect::<Vec<_>>();
        let photos = image_bytes
            .iter()
            .map(|bytes| {
                Photo::from_camera_jpeg(
                    CameraPhotoSource::new(camera.clone(), 1, 1, "MJPEG"),
                    (*bytes).clone(),
                )
                .unwrap()
            })
            .collect::<VecDeque<_>>();
        let captures = Arc::new(AtomicUsize::new(0));
        let camera_service = CameraService::with_backend(Arc::new(SequencedCameraBackend {
            camera,
            photos: Mutex::new(photos),
            captures: captures.clone(),
        }));

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "compare the four views", "images": expected_images.clone()}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "four-view answer"},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let mut harness = voice_harness(
            &server,
            ["compare the four views".to_owned()],
            camera_service,
        );
        let recording_id = harness.start_hold().await;
        for _ in 0..4 {
            assert!(
                harness
                    .runtime
                    .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                    .await
            );
        }
        for _ in 0..4 {
            let completion = harness.next_aux_event().await;
            assert!(matches!(
                &completion,
                super::VoiceAuxEvent::PhotoCaptureFinished { result: Ok(_), .. }
            ));
            assert!(harness.runtime.handle_aux_event(completion).await);
        }
        assert_eq!(captures.load(Ordering::SeqCst), 4);

        assert_eq!(
            harness.release_hold(recording_id).await,
            "compare the four views"
        );
        harness.accept_next_answer().await;
        assert_eq!(harness.runtime.controller.accepted_photos().len(), 4);
        assert_eq!(
            harness
                .runtime
                .controller
                .accepted_photos()
                .iter()
                .map(|photo| STANDARD.encode(photo.bytes()))
                .collect::<Vec<_>>(),
            expected_images
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn escape_discards_photo_and_partial_draft_results_before_submission() {
        let server = MockServer::start().await;
        let (camera, photo) = fake_camera();
        let captures = Arc::new(AtomicUsize::new(0));
        let camera_service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera,
            photo: photo.clone(),
            fail: false,
            captures,
        }));
        let mut harness = voice_harness(
            &server,
            ["final text that must not submit".to_owned()],
            camera_service,
        );
        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        let draft_id = harness.runtime.draft.as_ref().expect("active draft").id;

        // Begin an actual live-recognition worker, then cancel before its
        // completion is allowed to update or submit the draft.
        harness.runtime.request_partial_snapshot();
        let snapshot = harness.next_dictation_event().await;
        assert!(harness.runtime.handle_dictation_event(snapshot).await);
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
                .await
        );

        // Controlled late auxiliary completions cannot resurrect the draft.
        assert!(
            harness
                .runtime
                .handle_aux_event(super::VoiceAuxEvent::PartialTranscript {
                    recording_id,
                    sequence: 1,
                    result: Ok("late partial".into()),
                })
                .await
        );
        assert!(
            harness
                .runtime
                .handle_aux_event(super::VoiceAuxEvent::PhotoCaptureFinished {
                    draft_id,
                    request_id: 1,
                    result: Ok(photo),
                })
                .await
        );
        assert!(harness.runtime.draft.is_none());
        assert!(!harness.runtime.controller.has_active_turn());
        assert!(harness.runtime.controller.accepted_history().is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn escape_during_final_transcription_keeps_late_text_out_of_the_controller() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(
            &server,
            ["late final transcript".to_owned()],
            unused_camera_service(),
        );
        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldEnd))
                .await
        );
        let finished = harness.next_dictation_event().await;
        assert!(matches!(
            &finished,
            DictationEvent::RecordingFinished { recording_id: id, .. } if *id == recording_id
        ));
        assert!(harness.runtime.handle_dictation_event(finished).await);

        // Deliver Escape before the queued native-recognizer completion. The
        // controller must treat either completion shape as cleanup only.
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
                .await
        );
        let late = harness.next_dictation_event().await;
        assert!(matches!(
            &late,
            DictationEvent::TranscriptReady { recording_id: id, .. }
                | DictationEvent::TranscriptionCancelled { recording_id: id }
                if *id == recording_id
        ));
        assert!(harness.runtime.handle_dictation_event(late).await);
        assert!(harness.runtime.draft.is_none());
        assert!(!harness.runtime.controller.has_active_turn());
        assert!(harness.runtime.controller.accepted_history().is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn accepted_voice_transcript_submits_one_literal_question() {
        let server = MockServer::start().await;
        let question = "What makes a fern different from a moss?";
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": question}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "Ferns make spores and have vascular tissue."},
                "done": true,
                "done_reason": "stop"
            })))
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
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn accepted_voice_text_waits_for_its_own_speech_delivery_outcome() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "A spoken answer."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let mut harness = voice_harness(
            &server,
            ["question with a final speech result".to_owned()],
            unused_camera_service(),
        );
        harness.submit_next_transcript().await;
        let event = harness
            .main_events
            .recv()
            .await
            .expect("mock Ollama completion");
        assert!(harness.runtime.handle_main_event(event).await);

        // Text acceptance remains controller-owned and immediate, while a
        // distinct worker completion keeps the matching spoken outcome tied
        // to this exact answer rather than a later draft.
        assert_eq!(harness.runtime.controller.accepted_history().len(), 1);
        let utterance_id = *harness
            .runtime
            .pending_answer_speech
            .keys()
            .next()
            .expect("accepted voice answer queues one speech request");
        assert!(
            harness
                .runtime
                .handle_speech_event(SpeechEvent {
                    utterance_id,
                    owner: SpeechOwner::Turn(TurnId::new(1)),
                    outcome: SpeechOutcome::PlaybackFailed,
                })
                .await
        );
        assert!(harness.runtime.pending_answer_speech.is_empty());
        assert_eq!(harness.runtime.controller.accepted_history().len(), 1);
        harness.runtime.shutdown().await;
    }

    #[tokio::test]
    async fn voice_follow_up_uses_the_existing_accepted_conversation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "What is lichen?"}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "It is a partnership between a fungus and a photosynthetic partner."},
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
                    {"role": "user", "content": "What is lichen?"},
                    {"role": "assistant", "content": "It is a partnership between a fungus and a photosynthetic partner."},
                    {"role": "user", "content": "What should I look for on tree bark?"}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "Look for leaflike, crusty, or branching patches."},
                "done": true,
                "done_reason": "stop"
            })))
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
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn slash_looking_voice_text_cannot_activate_a_camera() {
        let server = MockServer::start().await;
        let question = "/camera 7 | take a photo";
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": question}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "I will treat that as words, not an action."},
                "done": true,
                "done_reason": "stop"
            })))
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
    async fn cancelled_stale_and_empty_voice_transcripts_make_no_request() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(&server, [String::new()], unused_camera_service());

        let cancelled_id = harness.start_hold().await;
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
        let current_id = harness.start_hold().await;
        assert_ne!(current_id, cancelled_id);
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldEnd))
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
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "old answer"},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;
        let mut harness = voice_harness(
            &server,
            ["first voice question".to_owned()],
            unused_camera_service(),
        );
        harness.submit_next_transcript().await;
        assert!(harness.runtime.controller.has_active_turn());

        // A new Enter hold cancels the old model turn before it opens the next
        // recording. A controlled late completion cannot restore history.
        let _next_recording = harness.start_hold().await;
        assert!(!harness.runtime.controller.has_active_turn());
        assert!(
            harness
                .runtime
                .handle_main_event(Event::TurnFinished(TurnFinished {
                    id: TurnId::new(1),
                    result: TurnResult::Answer {
                        answer: "late old answer".into(),
                        proposed_scene: None,
                    },
                }))
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
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "answer that should be cancelled"},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;
        let mut harness = voice_harness(
            &server,
            ["cancel this answer".to_owned()],
            unused_camera_service(),
        );
        harness.submit_next_transcript().await;
        assert!(harness.runtime.controller.has_active_turn());

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
                .await
        );
        assert!(!harness.runtime.controller.has_active_turn());
        assert!(
            harness
                .runtime
                .handle_main_event(Event::TurnFinished(TurnFinished {
                    id: TurnId::new(1),
                    result: TurnResult::Answer {
                        answer: "late cancelled answer".into(),
                        proposed_scene: None,
                    },
                }))
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
