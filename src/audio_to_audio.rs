//! Isolated hold-to-talk audio-to-audio experiment.
//!
//! This mode deliberately shares the existing physical controls, CPAL capture
//! adapter, controller turn authorization, cancellation tokens, telemetry,
//! and Rodio output adapter. It does **not** construct Whisper, Piper, HAL,
//! partial transcription, a camera service, or text conversation history.
//! One released recording becomes one direct-audio request and, only if the
//! controller still authorizes its result, one WAV playback request.

use std::{collections::HashMap, io, sync::Arc};

use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    controller::Controller,
    dictation::audio::{CpalRecorder, Recorder, RecordingHandle},
    launch::ModelBackend,
    listen::{
        self,
        controls::{VoiceControlEvent, VoiceInputEvent, VoiceInputSource},
        evdev::EvdevButtonInputSource,
    },
    profile::{ResolvedVoiceInput, ResolvedVoiceProfile},
    protocol::{
        AudioTurnFinished, AudioTurnRequest, AudioTurnResult, CapturedAudio, DictationEvent,
        Effect, Event, Input, RecordingId, TurnId,
    },
    speak::{
        self,
        audio::{DirectPlaybackEvent, DirectPlaybackOutcome, DirectPlaybackService},
        playback::OutputSelection,
    },
    telemetry::{self, Interaction},
    think::{CapabilityStatus, ModelCapabilities, OllamaClient},
};

const EVENT_CHANNEL_CAPACITY: usize = 32;

/// Runs the deliberately limited direct-audio experiment after profile
/// resolution and capability discovery. The caller supplies an `OllamaClient`
/// only for its cached metadata; fixture mode never sends it a request.
pub(crate) async fn run(
    resolved: ResolvedVoiceProfile,
    client: Option<OllamaClient>,
) -> Result<(), AudioToAudioError> {
    let configured_model = resolved.model.clone();
    let backend: Arc<dyn AudioAnswerBackend> = match resolved.backend {
        ModelBackend::Fixture => Arc::new(FixtureAudioBackend),
        ModelBackend::Ollama => {
            let client =
                client.expect("Ollama backend was initialized before audio runtime launch");
            backend_for_ollama(client.model(), client.capabilities())?
        }
    };
    let (main_sender, mut main_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (capture_sender, mut capture_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (control_sender, mut control_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (playback_sender, mut playback_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let mut playback = DirectPlaybackService::with_configuration(
        OutputSelection::from_requested_id(resolved.audio_output),
        playback_sender,
    )
    .map_err(AudioToAudioError::PlaybackWorker)?;
    let source_name = resolved.input.label();
    let mut input: Box<dyn VoiceInputSource> = match resolved.input {
        ResolvedVoiceInput::Terminal => Box::new(
            listen::dictation::spawn_terminal_voice_input(control_sender)
                .map_err(AudioToAudioError::InputAdapter)?,
        ),
        ResolvedVoiceInput::Buttons(path) => Box::new(
            EvdevButtonInputSource::open(path, control_sender)
                .map_err(AudioToAudioError::ButtonInput)?,
        ),
    };

    let recorder: Arc<dyn Recorder> =
        Arc::new(CpalRecorder::with_requested_name(resolved.microphone));
    let microphone = recorder.selected_microphone().map(|device| device.label());
    speak::process("voice_input_source", source_name);
    if let Some(profile) = resolved.profile {
        speak::process("hardware_profile", profile.label());
    }
    speak::process("voice_pipeline", "audio-to-audio");
    let model_label = match resolved.backend {
        ModelBackend::Fixture => format!("{configured_model} (not used by fixture)"),
        ModelBackend::Ollama => configured_model,
    };
    speak::process("voice_model", model_label);
    speak::process(
        "model_backend",
        format!(
            "{} (fixture is a plumbing test; Ollama must advertise a documented bidirectional API)",
            resolved.backend.label()
        ),
    );
    match microphone {
        Ok(label) => speak::voice_log(&format!(
            "audio-to-audio ready — microphone: {label}; hold Enter to record, release to submit; no Whisper, Piper, camera, or listening/thinking cue is used"
        )),
        Err(error) => speak::warning(
            "audio_to_audio_microphone_unavailable",
            format!("recording will fail until a microphone is available: {error}"),
        ),
    }

    let mut runtime = AudioToAudioRuntime::new(
        Controller::new(),
        recorder,
        backend,
        capture_sender,
        main_sender,
        playback.handle(),
    );

    loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => AudioEvent::Control(VoiceInputEvent::Interrupt),
            event = control_receiver.recv() => match event {
                Some(event) => AudioEvent::Control(event),
                None => AudioEvent::ControlChannelClosed,
            },
            event = capture_receiver.recv() => match event {
                Some(event) => AudioEvent::Capture(event),
                None => AudioEvent::CaptureChannelClosed,
            },
            event = main_receiver.recv() => match event {
                Some(event) => AudioEvent::Main(event),
                None => AudioEvent::MainChannelClosed,
            },
            event = playback_receiver.recv() => match event {
                Some(event) => AudioEvent::Playback(event),
                None => AudioEvent::PlaybackChannelClosed,
            },
        };

        let keep_running = match event {
            AudioEvent::Control(event) => runtime.handle_control(event).await,
            AudioEvent::Capture(event) => runtime.handle_capture_event(event).await,
            AudioEvent::Main(event) => runtime.handle_main_event(event).await,
            AudioEvent::Playback(event) => runtime.handle_playback_event(event),
            AudioEvent::ControlChannelClosed => {
                runtime
                    .source_failed(source_name, "control channel closed unexpectedly")
                    .await
            }
            AudioEvent::CaptureChannelClosed => {
                runtime
                    .source_failed(
                        "microphone capture",
                        "capture event channel closed unexpectedly",
                    )
                    .await
            }
            AudioEvent::MainChannelClosed | AudioEvent::PlaybackChannelClosed => {
                runtime
                    .source_failed(
                        "audio-to-audio worker",
                        "worker event channel closed unexpectedly",
                    )
                    .await
            }
        };
        if !keep_running {
            break;
        }
    }

    input.shutdown();
    runtime.shutdown().await;
    playback.shutdown();
    Ok(())
}

enum AudioEvent {
    Control(VoiceInputEvent),
    Capture(DictationEvent),
    Main(Event),
    Playback(DirectPlaybackEvent),
    ControlChannelClosed,
    CaptureChannelClosed,
    MainChannelClosed,
    PlaybackChannelClosed,
}

/// A deliberately narrow blocking backend seam. It accepts a real completed
/// recording and returns a complete WAV. No transcript, prompt, or terminal
/// output flows through this type.
pub(crate) trait AudioAnswerBackend: Send + Sync {
    fn answer(
        &self,
        request: AudioTurnRequest,
        cancellation: &CancellationToken,
        interaction: Option<&Interaction>,
    ) -> Result<Vec<u8>, AudioBackendError>;
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum AudioBackendError {
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

fn backend_for_ollama(
    model: &str,
    capabilities: ModelCapabilities,
) -> Result<Arc<dyn AudioAnswerBackend>, AudioToAudioError> {
    let input = capability_reason("audio input", &capabilities.audio_input);
    let output = capability_reason("audio output", &capabilities.audio_output);
    if matches!(capabilities.audio_input, CapabilityStatus::Supported)
        && matches!(capabilities.audio_output, CapabilityStatus::Supported)
    {
        // A future server might advertise two names before publishing a
        // request/response schema. Do not guess that schema with private
        // recordings; the real backend remains a deliberate later change.
        return Err(AudioToAudioError::OllamaAudioProtocolUndocumented {
            model: model.to_owned(),
        });
    }
    Err(AudioToAudioError::OllamaAudioUnsupported {
        model: model.to_owned(),
        input,
        output,
    })
}

/// Clearly labeled deterministic backend used only to prove plumbing. It
/// validates that a bounded real capture reached the backend, then returns a
/// short generated WAV test tone—not an intelligent answer.
struct FixtureAudioBackend;

impl AudioAnswerBackend for FixtureAudioBackend {
    fn answer(
        &self,
        request: AudioTurnRequest,
        cancellation: &CancellationToken,
        interaction: Option<&Interaction>,
    ) -> Result<Vec<u8>, AudioBackendError> {
        if cancellation.is_cancelled() {
            return Err(AudioBackendError::Cancelled);
        }
        validate_backend_audio(&request.audio)?;
        if let Some(interaction) = interaction {
            interaction.event(
                "audio_to_audio.fixture",
                "received_capture",
                telemetry::fields([
                    (
                        "sample_rate_hz",
                        serde_json::json!(request.audio.sample_rate_hz()),
                    ),
                    (
                        "sample_count",
                        serde_json::json!(request.audio.samples().len()),
                    ),
                ]),
            );
        }
        if cancellation.is_cancelled() {
            return Err(AudioBackendError::Cancelled);
        }
        Ok(fixture_wav())
    }
}

fn capability_reason(kind: &str, status: &CapabilityStatus) -> String {
    match status {
        CapabilityStatus::Supported => format!("{kind} is advertised"),
        CapabilityStatus::Unsupported => format!("{kind} is not advertised"),
        CapabilityStatus::Unknown { reason } => format!("{kind} is unknown ({reason})"),
    }
}

fn validate_backend_audio(audio: &CapturedAudio) -> Result<(), AudioBackendError> {
    if audio.sample_rate_hz() == 0 || audio.samples().is_empty() {
        return Err(AudioBackendError::Failed(
            "capture was missing audio format metadata or samples".into(),
        ));
    }
    if !audio.samples().iter().all(|sample| sample.is_finite()) {
        return Err(AudioBackendError::Failed(
            "capture contained invalid non-finite samples".into(),
        ));
    }
    Ok(())
}

/// Generates a tiny valid 16-bit mono WAV for fixture-only plumbing tests.
/// It is created in memory, contains no person’s recording, and is never
/// persisted.
fn fixture_wav() -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    const SAMPLES: usize = 1_600;
    let mut pcm = Vec::with_capacity(SAMPLES * 2);
    for index in 0..SAMPLES {
        let phase = index as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32;
        let sample = (phase.sin() * i16::MAX as f32 * 0.10) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    let data_length = u32::try_from(pcm.len()).expect("fixture WAV length fits u32");
    let file_length = 36_u32
        .checked_add(data_length)
        .expect("fixture WAV length fits u32");
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_length.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_length.to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

struct RunningCapture {
    id: RecordingId,
    handle: Box<dyn RecordingHandle>,
}

struct AudioDraft {
    id: u64,
    recording_id: RecordingId,
    held: bool,
    released: bool,
    cancellation: CancellationToken,
    interaction: Interaction,
}

struct RunningAudioTurn {
    id: TurnId,
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
    interaction: Interaction,
}

struct AudioTurnRuntime {
    backend: Arc<dyn AudioAnswerBackend>,
    event_sender: mpsc::Sender<Event>,
    active: Option<RunningAudioTurn>,
    pending_interaction: Option<Interaction>,
    completed: HashMap<TurnId, Interaction>,
    cleanup: Vec<JoinHandle<()>>,
}

impl AudioTurnRuntime {
    fn new(backend: Arc<dyn AudioAnswerBackend>, event_sender: mpsc::Sender<Event>) -> Self {
        Self {
            backend,
            event_sender,
            active: None,
            pending_interaction: None,
            completed: HashMap::new(),
            cleanup: Vec::new(),
        }
    }

    fn set_pending_interaction(&mut self, interaction: Interaction) {
        if let Some(previous) = self.pending_interaction.replace(interaction) {
            previous.complete("superseded_before_audio_dispatch", telemetry::Fields::new());
        }
    }

    async fn apply(&mut self, effect: Effect) -> Option<Effect> {
        match effect {
            Effect::StartAudioTurn(request) => {
                self.start(request).await;
                None
            }
            Effect::CancelTurn(id) => {
                self.cancel(id).await;
                None
            }
            // The direct-audio runtime handles this effect itself so the
            // existing text runtime never needs to initialize this backend.
            other => Some(other),
        }
    }

    async fn start(&mut self, request: AudioTurnRequest) {
        self.stop_active().await;
        let id = request.id;
        let interaction = self
            .pending_interaction
            .take()
            .unwrap_or_else(|| telemetry::start_interaction("audio-to-audio", None));
        interaction.bind_turn(id);
        interaction.bind_recording(request.recording_id);
        interaction.mark("audio_to_audio.request_dispatched");
        interaction.set_input(telemetry::fields([
            ("request_type", serde_json::json!("audio_to_audio")),
            (
                "audio_sample_rate_hz",
                serde_json::json!(request.audio.sample_rate_hz()),
            ),
            (
                "audio_sample_count",
                serde_json::json!(request.audio.samples().len()),
            ),
        ]));
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let backend = self.backend.clone();
        let event_sender = self.event_sender.clone();
        let worker_interaction = interaction.clone();
        let handle = tokio::spawn(async move {
            let stage =
                worker_interaction.stage("audio_to_audio.backend", telemetry::Fields::new());
            let cancellation_for_worker = worker_cancellation.clone();
            let result = tokio::task::spawn_blocking(move || {
                backend.answer(request, &cancellation_for_worker, Some(&worker_interaction))
            })
            .await;
            let result = if worker_cancellation.is_cancelled() {
                stage.finish("cancelled", telemetry::Fields::new());
                AudioTurnResult::Cancelled
            } else {
                match result {
                    Ok(Ok(wav)) => {
                        stage.finish(
                            "completed",
                            telemetry::fields([(
                                "returned_audio_bytes",
                                serde_json::json!(wav.len()),
                            )]),
                        );
                        AudioTurnResult::Audio { wav }
                    }
                    Ok(Err(AudioBackendError::Cancelled)) => {
                        stage.finish("cancelled", telemetry::Fields::new());
                        AudioTurnResult::Cancelled
                    }
                    Ok(Err(AudioBackendError::Failed(message))) => {
                        stage.finish("failed", telemetry::Fields::new());
                        AudioTurnResult::Failed { message }
                    }
                    Err(error) => {
                        stage.finish("worker_failure", telemetry::Fields::new());
                        AudioTurnResult::Failed {
                            message: format!("audio backend worker ended unexpectedly: {error}"),
                        }
                    }
                }
            };
            if !worker_cancellation.is_cancelled() && !matches!(result, AudioTurnResult::Cancelled)
            {
                let _ = event_sender
                    .send(Event::AudioTurnFinished(AudioTurnFinished { id, result }))
                    .await;
            }
        });
        self.active = Some(RunningAudioTurn {
            id,
            cancellation,
            handle,
            interaction,
        });
    }

    async fn cancel(&mut self, id: TurnId) {
        if self.active.as_ref().map(|active| active.id) == Some(id) {
            self.stop_active().await;
        }
    }

    async fn retire(&mut self, id: TurnId) {
        if self.active.as_ref().map(|active| active.id) != Some(id) {
            return;
        }
        let active = self.active.take().expect("active audio turn was checked");
        let _ = active.handle.await;
        self.completed.insert(id, active.interaction);
    }

    async fn stop_active(&mut self) {
        self.reap_cleanup().await;
        let Some(active) = self.active.take() else {
            return;
        };
        active.interaction.mark("cancellation.requested");
        active.cancellation.cancel();
        // `spawn_blocking` cannot be force-stopped safely. The backend gets
        // this token and its completion is retained in `cleanup`, preventing
        // a cancelled worker from becoming invisible or accumulating.
        self.cleanup.push(tokio::spawn(async move {
            let _ = active.handle.await;
            active
                .interaction
                .mark("cancellation.local_cleanup_completed");
            active
                .interaction
                .complete("cancelled", telemetry::Fields::new());
        }));
    }

    async fn reap_cleanup(&mut self) {
        let mut pending = Vec::new();
        for task in std::mem::take(&mut self.cleanup) {
            if task.is_finished() {
                let _ = task.await;
            } else {
                pending.push(task);
            }
        }
        self.cleanup = pending;
    }

    async fn shutdown(&mut self) {
        self.stop_active().await;
        for task in std::mem::take(&mut self.cleanup) {
            let _ = task.await;
        }
        for (_, interaction) in self.completed.drain() {
            interaction.complete("accepted_audio_not_played", telemetry::Fields::new());
        }
    }

    fn take_completed_interaction(&mut self, id: TurnId) -> Option<Interaction> {
        self.completed.remove(&id)
    }

    fn complete_completed_interaction(&mut self, id: TurnId, outcome: &'static str) {
        if let Some(interaction) = self.completed.remove(&id) {
            interaction.complete(outcome, telemetry::Fields::new());
        }
    }
}

/// State owner for the isolated direct-audio interaction. The main
/// [`Controller`] still authorizes which backend result can play; this runtime
/// only owns the hardware capture and audio-specific worker lifecycle.
struct AudioToAudioRuntime {
    controller: Controller,
    recorder: Arc<dyn Recorder>,
    capture_sender: mpsc::Sender<DictationEvent>,
    capture: Option<RunningCapture>,
    draft: Option<AudioDraft>,
    cancelled_captures: HashMap<RecordingId, Interaction>,
    next_recording_id: u64,
    next_draft_id: u64,
    turns: AudioTurnRuntime,
    playback: crate::speak::audio::DirectPlaybackHandle,
    pending_playback: HashMap<crate::speak::audio::DirectPlaybackId, Interaction>,
}

impl AudioToAudioRuntime {
    fn new(
        controller: Controller,
        recorder: Arc<dyn Recorder>,
        backend: Arc<dyn AudioAnswerBackend>,
        capture_sender: mpsc::Sender<DictationEvent>,
        main_sender: mpsc::Sender<Event>,
        playback: crate::speak::audio::DirectPlaybackHandle,
    ) -> Self {
        Self {
            controller,
            recorder,
            capture_sender,
            capture: None,
            draft: None,
            cancelled_captures: HashMap::new(),
            next_recording_id: 1,
            next_draft_id: 1,
            turns: AudioTurnRuntime::new(backend, main_sender),
            playback,
            pending_playback: HashMap::new(),
        }
    }

    async fn handle_control(&mut self, event: VoiceInputEvent) -> bool {
        match event {
            VoiceInputEvent::Control(VoiceControlEvent::TalkPressed) => self.begin().await,
            VoiceInputEvent::Control(VoiceControlEvent::TalkReleased) => self.release(),
            VoiceInputEvent::Control(VoiceControlEvent::PhotoPressed) => {
                speak::warning(
                    "audio_to_audio_photo_ignored",
                    "direct audio-to-audio mode never activates a camera",
                );
                true
            }
            VoiceInputEvent::Control(VoiceControlEvent::Cancel) => {
                self.cancel_unfinished("cancel").await;
                speak::voice_log("direct-audio draft cancelled");
                true
            }
            VoiceInputEvent::Interrupt => {
                self.cancel_unfinished("interrupt").await;
                false
            }
            VoiceInputEvent::SourceFailed { source, message } => {
                self.source_failed(source, &message).await
            }
            VoiceInputEvent::SourceLost { source, message } => {
                speak::warning(
                    "audio_to_audio_input_lost",
                    format!(
                        "{source} was lost: {message}; cancelling unfinished work and waiting for recovery"
                    ),
                );
                self.cancel_unfinished("input_source_lost").await;
                true
            }
            VoiceInputEvent::SourceRecovered { source } => {
                speak::process(
                    "voice_input_source_recovered",
                    format!("{source} was reacquired; hold Enter for a fresh recording"),
                );
                true
            }
        }
    }

    async fn source_failed(&mut self, source: &str, message: &str) -> bool {
        speak::warning(
            "audio_to_audio_input_failed",
            format!("{source} failed: {message}; cancelling unfinished work"),
        );
        self.cancel_unfinished("source_failure").await;
        false
    }

    async fn begin(&mut self) -> bool {
        // A new press invalidates every unfinished audio result and stops any
        // already playing response before opening the microphone again.
        self.cancel_unfinished("new_talk_press").await;
        self.playback.cancel_all("new_talk_press");

        let draft_id = self.next_draft_id;
        self.next_draft_id = self
            .next_draft_id
            .checked_add(1)
            .expect("audio draft identifiers exhausted");
        let recording_id = RecordingId::new(self.next_recording_id);
        self.next_recording_id = self
            .next_recording_id
            .checked_add(1)
            .expect("audio recording identifiers exhausted");
        let interaction = telemetry::start_interaction("audio-to-audio", Some(draft_id));
        interaction.mark("voice.enter_pressed");
        self.draft = Some(AudioDraft {
            id: draft_id,
            recording_id,
            held: true,
            released: false,
            cancellation: CancellationToken::new(),
            interaction: interaction.clone(),
        });

        self.start_capture_if_pending();
        true
    }

    fn start_capture_if_pending(&mut self) {
        if self.capture.is_some() {
            speak::process(
                "audio_to_audio_capture_waiting",
                "previous microphone capture is releasing before the new hold begins",
            );
            return;
        }
        let Some((draft_id, recording_id, interaction)) = self.draft.as_ref().and_then(|draft| {
            (draft.held && !draft.released && !draft.cancellation.is_cancelled())
                .then(|| (draft.id, draft.recording_id, draft.interaction.clone()))
        }) else {
            return;
        };
        match self
            .recorder
            .start(recording_id, self.capture_sender.clone(), Some(interaction))
        {
            Ok(handle) => {
                self.capture = Some(RunningCapture {
                    id: recording_id,
                    handle,
                });
                speak::process(
                    "audio_to_audio_capture_requested",
                    format!("draft_id={draft_id} recording_id={recording_id:?}"),
                );
            }
            Err(message) => {
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| draft.recording_id == recording_id)
                {
                    let draft = self.draft.take().expect("draft was checked");
                    draft
                        .interaction
                        .complete("failed", telemetry::Fields::new());
                }
                speak::warning("audio_to_audio_capture_failed", message);
            }
        }
    }

    fn release(&mut self) -> bool {
        let recording_id = self.draft.as_mut().and_then(|draft| {
            if draft.held && !draft.released {
                draft.held = false;
                draft.released = true;
                draft.interaction.mark("voice.enter_released");
                Some(draft.recording_id)
            } else {
                None
            }
        });
        if let Some(recording_id) = recording_id {
            if let Some(capture) = self.capture.as_ref()
                && capture.id == recording_id
            {
                capture.handle.finish();
                speak::voice_log("recording released; waiting for direct audio backend");
            } else {
                // A previous microphone was still releasing when this new
                // hold ended. Do not start a delayed stream after release.
                if let Some(draft) = self.draft.take() {
                    draft.cancellation.cancel();
                    draft
                        .interaction
                        .complete("cancelled_before_capture_started", telemetry::Fields::new());
                }
                speak::voice_log(
                    "released before the microphone became available; no recording started",
                );
            }
        }
        true
    }

    async fn handle_capture_event(&mut self, event: DictationEvent) -> bool {
        match event {
            DictationEvent::RecordingStarted { recording_id } => {
                if let Some(draft) = self
                    .draft
                    .as_ref()
                    .filter(|draft| draft.recording_id == recording_id)
                {
                    draft.interaction.bind_recording(recording_id);
                    draft.interaction.mark("microphone.recording_started_event");
                    speak::process(
                        "microphone_capture_started",
                        format!(
                            "direct-audio draft_id={} recording_id={recording_id:?}",
                            draft.id
                        ),
                    );
                }
            }
            DictationEvent::RecordingElapsed { .. }
            | DictationEvent::RecordingSnapshot { .. }
            | DictationEvent::RecordingSnapshotUnavailable { .. }
            | DictationEvent::TranscriptReady { .. }
            | DictationEvent::TranscriptionCancelled { .. }
            | DictationEvent::TranscriptionFailed { .. } => {}
            DictationEvent::RecordingLimitReached { recording_id } => {
                if let Some(capture) = self
                    .capture
                    .as_ref()
                    .filter(|capture| capture.id == recording_id)
                {
                    capture.handle.finish();
                    speak::warning(
                        "audio_to_audio_recording_limit",
                        "30-second capture limit reached; finishing this direct-audio request",
                    );
                }
            }
            DictationEvent::RecordingFinished {
                recording_id,
                audio,
            } => {
                self.retire_capture(recording_id).await;
                if self.finish_cancelled_capture(recording_id, "cancelled") {
                    self.start_capture_if_pending();
                    return true;
                }
                let accepted = self.draft.as_ref().is_some_and(|draft| {
                    draft.recording_id == recording_id
                        && draft.released
                        && !draft.cancellation.is_cancelled()
                });
                if !accepted {
                    return true;
                }
                let draft = self.draft.take().expect("accepted audio draft exists");
                draft.interaction.mark("microphone.recording_finished");
                self.turns.set_pending_interaction(draft.interaction);
                self.apply_controller(Event::Input(Input::AskAudio {
                    recording_id,
                    audio,
                }))
                .await;
                self.start_capture_if_pending();
            }
            DictationEvent::RecordingCancelled { recording_id } => {
                self.retire_capture(recording_id).await;
                if self.finish_cancelled_capture(recording_id, "cancelled") {
                    self.start_capture_if_pending();
                    return true;
                }
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| draft.recording_id == recording_id)
                {
                    self.draft.take();
                    speak::voice_log("direct-audio recording cancelled; microphone released");
                }
                self.start_capture_if_pending();
            }
            DictationEvent::RecordingFailed {
                recording_id,
                message,
            } => {
                self.retire_capture(recording_id).await;
                if self.finish_cancelled_capture(recording_id, "failed") {
                    self.start_capture_if_pending();
                    return true;
                }
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| draft.recording_id == recording_id)
                {
                    self.draft.take();
                    speak::warning("audio_to_audio_recording_failed", message);
                }
                self.start_capture_if_pending();
            }
            DictationEvent::Input(_) => {}
        }
        true
    }

    async fn retire_capture(&mut self, recording_id: RecordingId) {
        if self.capture.as_ref().map(|capture| capture.id) != Some(recording_id) {
            return;
        }
        let capture = self.capture.take().expect("capture was checked");
        capture.handle.wait().await;
    }

    fn finish_cancelled_capture(
        &mut self,
        recording_id: RecordingId,
        outcome: &'static str,
    ) -> bool {
        let Some(interaction) = self.cancelled_captures.remove(&recording_id) else {
            return false;
        };
        interaction.mark("cancellation.local_cleanup_completed");
        interaction.complete(outcome, telemetry::Fields::new());
        true
    }

    async fn apply_controller(&mut self, event: Event) {
        for effect in self.controller.handle(event) {
            self.apply_effect(effect).await;
        }
    }

    async fn apply_effect(&mut self, effect: Effect) {
        let Some(effect) = self.turns.apply(effect).await else {
            return;
        };
        match effect {
            Effect::PlayAudio { id, wav } => {
                let interaction = self.turns.take_completed_interaction(id);
                // A dedicated interaction is only a defensive fallback for a
                // malformed runtime event; normal direct-audio completions
                // retain their original Enter→capture→backend correlation.
                let interaction = interaction.or_else(|| {
                    Some(telemetry::start_interaction(
                        "audio-to-audio-playback",
                        None,
                    ))
                });
                if let Some(playback_id) = self.playback.play(id, wav, interaction.clone()) {
                    if let Some(interaction) = interaction {
                        self.pending_playback.insert(playback_id, interaction);
                    }
                } else {
                    speak::warning(
                        "audio_to_audio_playback_unavailable",
                        "controller accepted audio, but it could not be queued for local playback",
                    );
                }
            }
            Effect::Display(output) => speak::emit(output),
            Effect::Exit => {}
            // These effects belong to the text/camera runtime and cannot be
            // produced from `AskAudio`; do not create hardware fallbacks.
            Effect::StartTurn(_) | Effect::PrepareCamera(_) | Effect::ListCameras => {
                warn!(
                    target: "process",
                    event = "audio_to_audio_unexpected_text_effect",
                    "direct-audio controller produced an unexpected text/camera effect"
                );
            }
            Effect::StartAudioTurn(_) | Effect::CancelTurn(_) => {
                // `AudioTurnRuntime::apply` consumed these above.
                unreachable!("audio effect should have been consumed by AudioTurnRuntime")
            }
        }
    }

    async fn handle_main_event(&mut self, event: Event) -> bool {
        let completion = match &event {
            Event::AudioTurnFinished(finished) => Some((
                finished.id,
                matches!(finished.result, AudioTurnResult::Audio { .. }),
                matches!(finished.result, AudioTurnResult::Failed { .. }),
            )),
            _ => None,
        };
        if let Some((id, _, _)) = completion {
            self.turns.retire(id).await;
        }
        self.apply_controller(event).await;
        if let Some((id, accepted_audio, failed)) = completion {
            if failed {
                self.turns.complete_completed_interaction(id, "failed");
            } else if !accepted_audio {
                self.turns.complete_completed_interaction(id, "cancelled");
            }
        }
        true
    }

    fn handle_playback_event(&mut self, event: DirectPlaybackEvent) -> bool {
        let Some(interaction) = self.pending_playback.remove(&event.id) else {
            return true;
        };
        match event.outcome {
            DirectPlaybackOutcome::Played => {
                interaction.mark("audio_to_audio.playback_completed");
                interaction.complete("accepted_audio_played", telemetry::Fields::new());
            }
            DirectPlaybackOutcome::Cancelled => {
                interaction.mark("audio_to_audio.playback_cancelled");
                interaction.complete(
                    "accepted_audio_playback_cancelled",
                    telemetry::Fields::new(),
                );
            }
            DirectPlaybackOutcome::Failed => {
                interaction.complete("accepted_audio_playback_failed", telemetry::Fields::new());
                speak::warning(
                    "audio_to_audio_playback_failed",
                    "direct audio was accepted but local speaker playback failed",
                );
            }
        }
        true
    }

    async fn cancel_unfinished(&mut self, reason: &'static str) {
        if let Some(capture) = self.capture.as_ref() {
            capture.handle.cancel();
        }
        if let Some(draft) = self.draft.take() {
            draft.cancellation.cancel();
            draft.interaction.mark("cancellation.requested");
            if self
                .capture
                .as_ref()
                .is_some_and(|capture| capture.id == draft.recording_id)
            {
                self.cancelled_captures
                    .insert(draft.recording_id, draft.interaction);
            } else {
                draft
                    .interaction
                    .mark("cancellation.local_cleanup_completed");
                draft
                    .interaction
                    .complete("cancelled", telemetry::Fields::new());
            }
        }
        self.playback.cancel_all(reason);
        self.apply_controller(Event::Input(Input::Stop)).await;
    }

    async fn shutdown(&mut self) {
        self.cancel_unfinished("audio_to_audio_shutdown").await;
        if let Some(capture) = self.capture.take() {
            capture.handle.cancel();
            capture.handle.wait().await;
        }
        for (_, interaction) in self.cancelled_captures.drain() {
            interaction.mark("cancellation.local_cleanup_completed");
            interaction.complete("cancelled", telemetry::Fields::new());
        }
        self.turns.shutdown().await;
        for (_, interaction) in self.pending_playback.drain() {
            interaction.complete(
                "accepted_audio_playback_cancelled",
                telemetry::Fields::new(),
            );
        }
    }
}

#[derive(Debug, Error)]
pub enum AudioToAudioError {
    #[error("could not start terminal voice controls: {0}")]
    InputAdapter(#[source] io::Error),
    #[error("could not start Linux button controls: {0}")]
    ButtonInput(#[source] crate::listen::evdev::InputDeviceError),
    #[error("could not start direct WAV playback: {0}")]
    PlaybackWorker(#[source] io::Error),
    #[error(
        "Ollama model `{model}` cannot run direct audio-to-audio: {input}; {output}. No microphone was opened and no audio was sent. Use --backend fixture to test local plumbing."
    )]
    OllamaAudioUnsupported {
        model: String,
        input: String,
        output: String,
    },
    #[error(
        "Ollama model `{model}` advertised audio input and output, but Independence has no documented direct-audio request/response schema for this server. No microphone was opened and no audio was sent."
    )]
    OllamaAudioProtocolUndocumented { model: String },
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::Future,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use super::{
        AudioAnswerBackend, AudioBackendError, AudioToAudioError, AudioToAudioRuntime,
        FixtureAudioBackend, backend_for_ollama, fixture_wav, validate_backend_audio,
    };
    use crate::{
        controller::Controller,
        dictation::audio::{MicrophoneDescription, Recorder, RecordingHandle},
        listen::controls::{VoiceControlEvent, VoiceInputEvent},
        protocol::{
            AudioTurnFinished, AudioTurnRequest, AudioTurnResult, CapturedAudio, DictationEvent,
            Effect, Event, Input, RecordingId,
        },
        speak::{
            audio::{DirectPlaybackEvent, DirectPlaybackOutcome, DirectPlaybackService},
            playback::{Playback, PlaybackError, PlaybackRequest, PlaybackResult},
        },
        think::{OllamaClient, OllamaConfig},
    };

    /// A controllable capture adapter that sends exactly the events a real
    /// CPAL recording would send, without touching an input device.  The
    /// queued samples deliberately retain their requested sample rate so the
    /// test catches an accidental Whisper-only conversion in this pipeline.
    struct SyntheticRecorder {
        captures: Mutex<VecDeque<CapturedAudio>>,
        starts: Arc<AtomicUsize>,
        finishes: Arc<AtomicUsize>,
        cancels: Arc<AtomicUsize>,
    }

    impl SyntheticRecorder {
        fn with_captures(captures: impl IntoIterator<Item = CapturedAudio>) -> Self {
            Self {
                captures: Mutex::new(captures.into_iter().collect()),
                starts: Arc::new(AtomicUsize::new(0)),
                finishes: Arc::new(AtomicUsize::new(0)),
                cancels: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Recorder for SyntheticRecorder {
        fn selected_microphone(&self) -> Result<MicrophoneDescription, String> {
            Ok(MicrophoneDescription::new(
                "synthetic direct-audio microphone".into(),
                48_000,
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
            Ok(Box::new(SyntheticRecordingHandle {
                recording_id,
                event_sender,
                captures: self.captures.lock().unwrap().pop_front(),
                finishes: self.finishes.clone(),
                cancels: self.cancels.clone(),
            }))
        }
    }

    struct SyntheticRecordingHandle {
        recording_id: RecordingId,
        event_sender: mpsc::Sender<DictationEvent>,
        captures: Option<CapturedAudio>,
        finishes: Arc<AtomicUsize>,
        cancels: Arc<AtomicUsize>,
    }

    impl RecordingHandle for SyntheticRecordingHandle {
        fn finish(&self) {
            self.finishes.fetch_add(1, Ordering::SeqCst);
            let event = match self.captures.clone() {
                Some(audio) => DictationEvent::RecordingFinished {
                    recording_id: self.recording_id,
                    audio,
                },
                None => DictationEvent::RecordingFailed {
                    recording_id: self.recording_id,
                    message: "synthetic capture had no configured audio".into(),
                },
            };
            let _ = self.event_sender.try_send(event);
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

    /// Captures the exact request that reached the direct-audio boundary and
    /// returns a predetermined complete WAV.  It deliberately has no text,
    /// Whisper, Piper, camera, or HTTP dependency.
    struct CapturingBackend {
        requests: Arc<Mutex<Vec<AudioTurnRequest>>>,
        result: Result<Vec<u8>, AudioBackendError>,
    }

    impl CapturingBackend {
        fn returning(wav: Vec<u8>) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                result: Ok(wav),
            }
        }
    }

    impl AudioAnswerBackend for CapturingBackend {
        fn answer(
            &self,
            request: AudioTurnRequest,
            cancellation: &CancellationToken,
            _interaction: Option<&crate::telemetry::Interaction>,
        ) -> Result<Vec<u8>, AudioBackendError> {
            self.requests.lock().unwrap().push(request);
            if cancellation.is_cancelled() {
                Err(AudioBackendError::Cancelled)
            } else {
                self.result.clone()
            }
        }
    }

    /// Simulates a native backend that cannot observe cancellation until it
    /// returns.  The runtime must still prevent its late WAV from playing.
    struct HeldBackend {
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        wav: Vec<u8>,
    }

    impl AudioAnswerBackend for HeldBackend {
        fn answer(
            &self,
            _request: AudioTurnRequest,
            _cancellation: &CancellationToken,
            _interaction: Option<&crate::telemetry::Interaction>,
        ) -> Result<Vec<u8>, AudioBackendError> {
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
            let _ = self.release.lock().unwrap().recv();
            Ok(self.wav.clone())
        }
    }

    #[derive(Clone)]
    struct RecordingPlayback {
        seen: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Playback for RecordingPlayback {
        fn play(
            &mut self,
            request: PlaybackRequest,
            _current_generation: &std::sync::atomic::AtomicU64,
        ) -> Result<PlaybackResult, PlaybackError> {
            self.seen.lock().unwrap().push(request.wav);
            Ok(PlaybackResult::Completed)
        }
    }

    struct RuntimeHarness {
        runtime: AudioToAudioRuntime,
        capture_events: mpsc::Receiver<DictationEvent>,
        main_events: mpsc::Receiver<Event>,
        playback_events: mpsc::Receiver<DirectPlaybackEvent>,
        playback: DirectPlaybackService,
        played_wavs: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl RuntimeHarness {
        fn new(recorder: Arc<SyntheticRecorder>, backend: Arc<dyn AudioAnswerBackend>) -> Self {
            let (capture_sender, capture_events) = mpsc::channel(8);
            let (main_sender, main_events) = mpsc::channel(8);
            let (playback_sender, playback_events) = mpsc::channel(8);
            let played_wavs = Arc::new(Mutex::new(Vec::new()));
            let playback = DirectPlaybackService::test_parts(
                playback_sender,
                RecordingPlayback {
                    seen: played_wavs.clone(),
                },
            );
            let runtime = AudioToAudioRuntime::new(
                Controller::new(),
                recorder.clone(),
                backend,
                capture_sender,
                main_sender,
                playback.handle(),
            );
            Self {
                runtime,
                capture_events,
                main_events,
                playback_events,
                playback,
                played_wavs,
            }
        }

        async fn deliver_capture(&mut self) {
            let event = tokio::time::timeout(Duration::from_secs(1), self.capture_events.recv())
                .await
                .expect("synthetic capture event timed out")
                .expect("synthetic capture event channel closed");
            assert!(self.runtime.handle_capture_event(event).await);
        }

        async fn begin_and_release(&mut self) {
            assert!(
                self.runtime
                    .handle_control(VoiceInputEvent::Control(VoiceControlEvent::TalkPressed))
                    .await
            );
            self.deliver_capture().await;
            assert!(
                self.runtime
                    .handle_control(VoiceInputEvent::Control(VoiceControlEvent::TalkReleased))
                    .await
            );
            self.deliver_capture().await;
        }

        async fn next_main_event(&mut self) -> Event {
            tokio::time::timeout(Duration::from_secs(1), self.main_events.recv())
                .await
                .expect("direct audio backend event timed out")
                .expect("direct audio backend event channel closed")
        }

        async fn deliver_playback(&mut self) -> DirectPlaybackEvent {
            let event = tokio::time::timeout(Duration::from_secs(1), self.playback_events.recv())
                .await
                .expect("direct playback event timed out")
                .expect("direct playback event channel closed");
            assert!(self.runtime.handle_playback_event(event));
            event
        }

        async fn shutdown(&mut self) {
            self.runtime.shutdown().await;
            self.playback.shutdown();
        }
    }

    #[test]
    fn fixture_backend_receives_capture_format_and_returns_a_valid_wav() {
        let request = AudioTurnRequest {
            id: crate::protocol::TurnId::new(3),
            recording_id: crate::protocol::RecordingId::new(9),
            audio: CapturedAudio::with_sample_rate(vec![0.1; 480], 48_000),
        };
        let wav = FixtureAudioBackend
            .answer(request, &CancellationToken::new(), None)
            .expect("fixture backend should accept valid capture");
        assert!(wav.starts_with(b"RIFF"));
        assert!(wav.windows(4).any(|chunk| chunk == b"data"));
    }

    #[test]
    fn unsupported_ollama_is_rejected_before_the_direct_runtime_can_open_a_microphone() {
        let client = OllamaClient::new(
            OllamaConfig::new("http://127.0.0.1:11434", "metadata-only-model").unwrap(),
        )
        .unwrap();
        // `speech` describes Ollama's documented text-to-speech output. It
        // is not evidence of a documented audio-input conversation endpoint.
        client.set_capabilities_for_test(&["speech"]);

        assert!(matches!(
            backend_for_ollama(client.model(), client.capabilities()),
            Err(AudioToAudioError::OllamaAudioUnsupported { .. })
        ));
    }

    #[test]
    fn malformed_capture_is_rejected_by_direct_backend_validation() {
        let audio = CapturedAudio::with_sample_rate(vec![f32::NAN], 16_000);
        assert!(matches!(
            validate_backend_audio(&audio),
            Err(AudioBackendError::Failed(_))
        ));
    }

    #[test]
    fn controller_authorizes_only_current_audio_result_and_preserves_history() {
        let mut controller = Controller::new();
        let effects = controller.handle(Event::Input(Input::AskAudio {
            recording_id: crate::protocol::RecordingId::new(1),
            audio: CapturedAudio::new(vec![0.1; 160]),
        }));
        let request = match effects.as_slice() {
            [Effect::StartAudioTurn(request)] => request.clone(),
            _ => panic!("expected direct audio request"),
        };
        let stale = controller.handle(Event::AudioTurnFinished(AudioTurnFinished {
            id: crate::protocol::TurnId::new(request.id.value() + 1),
            result: AudioTurnResult::Audio { wav: fixture_wav() },
        }));
        assert!(stale.is_empty());
        let accepted = controller.handle(Event::AudioTurnFinished(AudioTurnFinished {
            id: request.id,
            result: AudioTurnResult::Audio { wav: fixture_wav() },
        }));
        assert!(matches!(accepted.as_slice(), [Effect::PlayAudio { .. }]));
        assert!(controller.accepted_history().is_empty());
    }

    #[tokio::test]
    async fn direct_pipeline_preserves_capture_format_and_plays_the_returned_wav_without_whisper_or_piper()
     {
        let recorder = Arc::new(SyntheticRecorder::with_captures([
            CapturedAudio::with_sample_rate(vec![0.1; 480], 48_000),
        ]));
        let backend = Arc::new(CapturingBackend::returning(fixture_wav()));
        let mut harness = RuntimeHarness::new(recorder.clone(), backend.clone());

        harness.begin_and_release().await;
        let event = harness.next_main_event().await;
        harness.runtime.handle_main_event(event).await;
        let playback = harness.deliver_playback().await;

        assert_eq!(playback.outcome, DirectPlaybackOutcome::Played);
        assert_eq!(recorder.starts.load(Ordering::SeqCst), 1);
        assert_eq!(recorder.finishes.load(Ordering::SeqCst), 1);
        {
            let requests = backend.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].audio.sample_rate_hz(), 48_000);
            assert_eq!(requests[0].audio.samples().len(), 480);
        }
        assert_eq!(*harness.played_wavs.lock().unwrap(), vec![fixture_wav()]);

        // `AudioToAudioRuntime` receives only Recorder, direct backend, and
        // direct Playback dependencies. This exercise therefore cannot
        // construct WhisperTranscriber, SpeechService/Piper, or CameraService.
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn escape_makes_an_already_returned_audio_result_stale_before_playback() {
        let recorder = Arc::new(SyntheticRecorder::with_captures([
            CapturedAudio::with_sample_rate(vec![0.1; 480], 48_000),
        ]));
        let backend = Arc::new(CapturingBackend::returning(fixture_wav()));
        let mut harness = RuntimeHarness::new(recorder, backend);

        harness.begin_and_release().await;
        let stale = harness.next_main_event().await;
        assert!(
            harness
                .runtime
                .handle_control(VoiceInputEvent::Control(VoiceControlEvent::Cancel))
                .await
        );
        harness.runtime.handle_main_event(stale).await;

        assert!(harness.played_wavs.lock().unwrap().is_empty());
        assert!(harness.playback_events.try_recv().is_err());
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn a_new_talk_press_rejects_a_late_audio_result_and_starts_a_fresh_capture() {
        let recorder = Arc::new(SyntheticRecorder::with_captures([
            CapturedAudio::with_sample_rate(vec![0.1; 480], 48_000),
            CapturedAudio::with_sample_rate(vec![0.2; 240], 48_000),
        ]));
        let backend = Arc::new(CapturingBackend::returning(fixture_wav()));
        let mut harness = RuntimeHarness::new(recorder.clone(), backend);

        harness.begin_and_release().await;
        let stale = harness.next_main_event().await;
        assert!(
            harness
                .runtime
                .handle_control(VoiceInputEvent::Control(VoiceControlEvent::TalkPressed))
                .await
        );
        harness.deliver_capture().await;
        harness.runtime.handle_main_event(stale).await;

        assert_eq!(recorder.starts.load(Ordering::SeqCst), 2);
        assert!(harness.played_wavs.lock().unwrap().is_empty());
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn a_noncooperative_backend_returning_after_new_talk_never_plays_stale_audio() {
        let recorder = Arc::new(SyntheticRecorder::with_captures([
            CapturedAudio::with_sample_rate(vec![0.1; 480], 48_000),
            CapturedAudio::with_sample_rate(vec![0.2; 240], 48_000),
        ]));
        let (started_sender, started_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let backend: Arc<dyn AudioAnswerBackend> = Arc::new(HeldBackend {
            started: Mutex::new(Some(started_sender)),
            release: Mutex::new(release_receiver),
            wav: fixture_wav(),
        });
        let mut harness = RuntimeHarness::new(recorder.clone(), backend);

        harness.begin_and_release().await;
        tokio::time::timeout(Duration::from_secs(1), started_receiver)
            .await
            .expect("held backend did not receive the captured audio")
            .expect("held backend start signal was dropped");
        assert!(
            harness
                .runtime
                .handle_control(VoiceInputEvent::Control(VoiceControlEvent::TalkPressed))
                .await
        );
        harness.deliver_capture().await;
        release_sender
            .send(())
            .expect("held backend release receiver should remain open");
        harness.shutdown().await;

        assert_eq!(recorder.starts.load(Ordering::SeqCst), 2);
        assert!(harness.main_events.try_recv().is_err());
        assert!(harness.played_wavs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_capture_reaches_no_playback_path() {
        let recorder = Arc::new(SyntheticRecorder::with_captures([
            CapturedAudio::with_sample_rate(vec![f32::NAN], 48_000),
        ]));
        let backend: Arc<dyn AudioAnswerBackend> = Arc::new(FixtureAudioBackend);
        let mut harness = RuntimeHarness::new(recorder, backend);

        harness.begin_and_release().await;
        let event = harness.next_main_event().await;
        assert!(matches!(
            event,
            Event::AudioTurnFinished(AudioTurnFinished {
                result: AudioTurnResult::Failed { .. },
                ..
            })
        ));
        harness.runtime.handle_main_event(event).await;

        assert!(harness.played_wavs.lock().unwrap().is_empty());
        assert!(harness.playback_events.try_recv().is_err());
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn photo_control_is_ignored_without_opening_capture_or_a_camera_path() {
        let recorder = Arc::new(SyntheticRecorder::with_captures([]));
        let backend: Arc<dyn AudioAnswerBackend> = Arc::new(FixtureAudioBackend);
        let mut harness = RuntimeHarness::new(recorder.clone(), backend);

        assert!(
            harness
                .runtime
                .handle_control(VoiceInputEvent::Control(VoiceControlEvent::PhotoPressed))
                .await
        );

        assert_eq!(recorder.starts.load(Ordering::SeqCst), 0);
        assert!(harness.capture_events.try_recv().is_err());
        assert!(harness.main_events.try_recv().is_err());
        assert!(harness.played_wavs.lock().unwrap().is_empty());
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn cancellation_token_prevents_fixture_answer() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let request = AudioTurnRequest {
            id: crate::protocol::TurnId::new(1),
            recording_id: crate::protocol::RecordingId::new(1),
            audio: CapturedAudio::new(vec![0.1; 16]),
        };
        assert!(matches!(
            FixtureAudioBackend.answer(request, &cancellation, None),
            Err(AudioBackendError::Cancelled)
        ));
    }
}
