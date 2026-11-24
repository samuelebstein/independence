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
    interface,
    launch::{ModelBackend, VoiceLaunchOptions, VoicePipeline},
    listen::{
        self,
        controls::{VoiceControlEvent, VoiceInputEvent, VoiceInputSource},
        evdev::{self, EvdevButtonInputSource},
    },
    profile::{self, CameraBackendSelection, ResolvedVoiceInput, ResolvedVoiceProfile},
    protocol::{
        CameraPreparation, CapturedAudio, DictationEvent, DictationInput, DictationOutput, Effect,
        Event, Input, RecordingId, TurnFinished, TurnId, TurnRequest, TurnResult, VisualRequest,
    },
    security::{EphemeralSession, SecurityError, SecurityManager},
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
const MAX_QUEUED_PHOTO_CAPTURES: usize = 1;
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
    /// Shows and (in voice) queues one streamed sentence. Each sentence is a
    /// separate utterance in the same ordered queue, so synthesis of the next
    /// overlaps playback of the current one.
    fn emit_sentence(
        &self,
        sentence: String,
        first: bool,
        speech_owner: speak::SpeechOwner,
    ) -> AnswerSpeechEmission {
        speak::independence_says_sentence(&sentence, first);
        match self {
            Self::Line => AnswerSpeechEmission::None,
            Self::Voice {
                speech,
                interaction,
            } => match speech.say_with_interaction(speech_owner, sentence, interaction.clone()) {
                Some(utterance_id) => AnswerSpeechEmission::Queued(utterance_id),
                None => AnswerSpeechEmission::Unavailable,
            },
        }
    }

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
                    let spoken_diagnostic = match &output {
                        crate::protocol::Output::Diagnostic(message) => {
                            voice_diagnostic_speech(message)
                        }
                        _ => None,
                    };
                    speak::emit(output);
                    if let Some(message) = spoken_diagnostic {
                        // Keep Piper on a fixed, non-sensitive status phrase.
                        // The detailed local diagnostic remains terminal-only.
                        let _ = speech.say(speak::SpeechOwner::System, message);
                    }
                    AnswerSpeechEmission::None
                }
            },
        }
    }
}

fn voice_diagnostic_speech(message: &str) -> Option<&'static str> {
    if message.starts_with("local inference failed: ") {
        if message.contains("Hailo VLM") || message.contains("health=degraded") {
            Some("The local model could not complete that request. Check the Hailo bridge.")
        } else {
            Some("The local model could not complete that request. Check the local model service.")
        }
    } else if message.starts_with("could not capture camera: ") {
        Some("The camera could not capture a photo. Check the camera and try again.")
    } else {
        None
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
///
/// Ephemeral Mode is the only mode offered. Private Mode has no launch option
/// until Ephemeral Mode is mature and an authenticator exists.
pub async fn run() -> Result<(), AppError> {
    let mut security = SecurityManager::new();
    announce_security_lifecycle(&security);
    let session = enter_ephemeral_mode(&mut security)?;

    let result = async {
        let client = OllamaClient::from_environment()?;
        discover_model_capabilities(&client, ModelBackend::Ollama).await;
        let (event_sender, mut event_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        listen::spawn_stdin_reader(event_sender.clone()).map_err(AppError::InputAdapter)?;

        let mut controller = Controller::new_ephemeral();
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
        controller.clear_ephemeral_session();
        Ok(())
    }
    .await;

    finish_ephemeral_session(&mut security, session)?;
    result
}

/// Emits only lifecycle/mode facts, never personal content.  In this
/// milestone lifecycle state is intentionally process-local: protected setup
/// storage does not exist yet, so persisting a fake state would be misleading.
fn announce_security_lifecycle(security: &SecurityManager) {
    speak::process(
        "security_lifecycle",
        format!(
            "{}; no authenticated private session is available",
            security.state()
        ),
    );
}

/// Acquires the only production session type that exists. Starting the
/// program is the explicit action that begins Ephemeral Mode; there is no
/// separate entry gesture. Keeping this in `SecurityManager` means no adapter
/// can obtain any other kind of session.
fn enter_ephemeral_mode(security: &mut SecurityManager) -> Result<EphemeralSession, AppError> {
    let session = security.enter_ephemeral().map_err(AppError::Security)?;
    // Logged now, when the session begins. The terminal says "Ephemeral Mode
    // is active" later, together with the ready line, so the person sees it
    // only once input will actually be accepted.
    speak::process(
        "ephemeral_mode_entered",
        "Ephemeral Mode active — temporary context only; private data is unavailable",
    );
    Ok(session)
}

fn finish_ephemeral_session(
    security: &mut SecurityManager,
    session: EphemeralSession,
) -> Result<(), AppError> {
    security
        .end_ephemeral(session)
        .map_err(AppError::Security)?;
    speak::process(
        "ephemeral_session_cleared",
        "temporary conversation and image buffers were released",
    );
    Ok(())
}

/// Obtains one cached `/api/show` record for the selected local model. A
/// failed probe does not stop text-only startup, but leaves camera and any
/// future direct-audio capability gated as explicitly unknown.
async fn discover_model_capabilities(client: &OllamaClient, backend: ModelBackend) {
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
        // Every Hailo request, text included, goes through the bridge, so an
        // unreachable bridge must not be described as "text still works".
        Err(error) if backend == ModelBackend::HailoVlm => speak::warning(
            "hailo_bridge_unavailable",
            format!(
                "could not reach the Hailo bridge for `{}`: {error}",
                client.model()
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
pub async fn run_dictation(audio_input: Option<String>) -> Result<(), AppError> {
    let mut security = SecurityManager::new();
    announce_security_lifecycle(&security);
    let session = enter_ephemeral_mode(&mut security)?;
    let result = dictation::run(audio_input)
        .await
        .map_err(AppError::Dictation);
    finish_ephemeral_session(&mut security, session)?;
    result
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
    interface::audio_inputs(
        &devices
            .iter()
            .map(|device| (device.id.clone(), device.name.clone(), device.is_default))
            .collect::<Vec<_>>(),
    );
    info!(target: "process", event = "audio_input_listed", count = devices.len(), "audio input devices listed");
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
    let interface_lines = devices
        .iter()
        .map(|device| match device.access_error.as_ref() {
            Some(_) => format!(
                "- {} — unavailable (check device permissions)",
                device.path.display()
            ),
            None => {
                let controls = device.controls.expect("open device has controls");
                let name = device.name.as_deref().unwrap_or("unnamed device");
                format!(
                    "- {} — {name}; ENTER: {}, P: {}, ESC: {}{}",
                    device.path.display(),
                    if controls.enter { "yes" } else { "no" },
                    if controls.photo { "yes" } else { "no" },
                    if controls.cancel { "yes" } else { "no" },
                    if device.supports_controller() {
                        " (button-controller compatible)"
                    } else {
                        ""
                    }
                )
            }
        })
        .collect::<Vec<_>>();
    interface::input_devices(&interface_lines);
    info!(target: "process", event = "input_device_listed", count = devices.len(), "Linux input devices listed");
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

    // Profile resolution reads only command-line/environment configuration.
    // Ephemeral Mode begins immediately at launch in every input mode; the
    // local model client, input device, Whisper, and audio output are created
    // only after the session lease exists, and the microphone opens only while
    // Talk/ENTER is held.
    let mut security = SecurityManager::new();
    announce_security_lifecycle(&security);
    let session = enter_ephemeral_mode(&mut security)?;

    let result = async {
        match (resolved.pipeline, resolved.backend) {
            (VoicePipeline::TranscribeAndSpeak, ModelBackend::Ollama | ModelBackend::HailoVlm) => {
                // Voice answers are spoken, so the system message asks for
                // short spoken sentences (typed mode keeps the plain prompt).
                let client = configured_ollama_client(&resolved)?
                    .with_instructions(crate::think::voice_instructions());
                discover_model_capabilities(&client, resolved.backend).await;
                if resolved.backend == ModelBackend::HailoVlm {
                    prime_voice_instructions(&client).await;
                }
                let camera = camera_service_for(resolved.camera_backend);
                if resolved.require_camera {
                    // Checked before the microphone, speaker, or Whisper are
                    // opened: on the Hailo Pi, a missing camera is a setup
                    // fault to fix now, not something to discover on `P`.
                    require_working_camera(&camera).await?;
                }
                run_transcribe_and_speak(resolved, client, camera).await
            }
            (VoicePipeline::TranscribeAndSpeak, backend) => Err(AppError::UnsupportedPipeline {
                backend: backend.label(),
                pipeline: VoicePipeline::TranscribeAndSpeak.label(),
            }),
            (VoicePipeline::AudioToAudio, backend) => {
                // The fixture backend is specifically a microphone→backend→speaker
                // plumbing check. It must not start an Ollama client or probe a
                // model. The explicit `ollama` backend receives cached metadata
                // first and will reject unsupported/unknown direct-audio support.
                let client = match backend {
                    ModelBackend::Ollama => {
                        let client = configured_ollama_client(&resolved)?;
                        discover_model_capabilities(&client, resolved.backend).await;
                        Some(client)
                    }
                    // The Hailo bridge is intentionally visual/text only in
                    // this milestone. Do not invent an audio request format
                    // or open a microphone for it.
                    ModelBackend::HailoVlm => {
                        return Err(AppError::UnsupportedPipeline {
                            backend: ModelBackend::HailoVlm.label(),
                            pipeline: VoicePipeline::AudioToAudio.label(),
                        });
                    }
                    ModelBackend::Fixture => None,
                };
                crate::audio_to_audio::run(resolved, client)
                    .await
                    .map_err(AppError::AudioToAudio)
            }
        }
    }
    .await;

    // This always runs after an entered session, even if model or hardware
    // startup failed.  It revokes the mode lease; individual runtimes clear
    // their application-owned buffers during their own shutdown.
    finish_ephemeral_session(&mut security, session)?;
    result
}

/// A model file's name without its directory, for logs that should not
/// record where on the machine (or under whose home directory) it lives.
fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Asks the Hailo bridge to load the (fixed, non-private) voice system
/// message once now, so no question has to wait for it to be read again.
/// Failure only costs speed: the bridge then reads it with each question.
async fn prime_voice_instructions(client: &OllamaClient) {
    match client.prime_instructions().await {
        Ok(true) => speak::process(
            "model_system_prompt_cached",
            "the voice system prompt is loaded once and reused for every question",
        ),
        Ok(false) => speak::background_warning("model_system_prompt_not_cached"),
        Err(_) => speak::background_warning("model_system_prompt_prime_failed"),
    }
}

fn camera_service_for(selection: CameraBackendSelection) -> CameraService {
    match selection {
        CameraBackendSelection::V4l => CameraService::v4l(),
        CameraBackendSelection::RpiCamStill => CameraService::rpicam(),
    }
}

/// Lists cameras once and fails unless at least one is usable. Listing opens
/// no stream and captures nothing (`rpicam-still --list-cameras` on the Pi).
async fn require_working_camera(camera: &CameraService) -> Result<(), AppError> {
    match camera.list().await {
        Ok(cameras) if !cameras.is_empty() => {
            speak::process(
                "camera_ready",
                format!("{} camera(s) detected", cameras.len()),
            );
            Ok(())
        }
        Ok(_) => Err(AppError::CameraRequired {
            detail: "no camera was listed".into(),
        }),
        Err(error) => Err(AppError::CameraRequired {
            detail: error.to_string(),
        }),
    }
}

/// Creates the strict local Ollama-compatible client only after runtime
/// pipeline selection chose a compatible adapter. Profile resolution
/// intentionally does not decide whether a model can perform the selected
/// operation.
fn configured_ollama_client(resolved: &ResolvedVoiceProfile) -> Result<OllamaClient, AppError> {
    let config = resolved
        .ollama
        .clone()
        .ok_or(AppError::MissingBackendConfiguration { backend: "ollama" })?;
    Ok(OllamaClient::new(config)?)
}

/// The established transcript → text-model → Piper path. It receives already
/// resolved configuration so `run_voice_with_options` can select the isolated
/// direct-audio path without starting Whisper or Piper first.
async fn run_transcribe_and_speak(
    resolved: ResolvedVoiceProfile,
    client: OllamaClient,
    camera: CameraService,
) -> Result<(), AppError> {
    let (main_event_sender, mut main_event_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (dictation_event_sender, mut dictation_event_receiver) =
        mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (voice_aux_sender, mut voice_aux_receiver) = mpsc::channel(VOICE_AUX_CHANNEL_CAPACITY);
    let (speech_event_sender, mut speech_event_receiver) =
        mpsc::channel(VOICE_AUX_CHANNEL_CAPACITY);
    let (control_sender, mut control_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let tts_model = resolved.piper.model_path().to_path_buf();
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
    speak::process("voice_pipeline", resolved.pipeline.label());
    speak::process("model_backend", resolved.backend.label());
    speak::process("voice_camera_backend", resolved.camera_backend.label());
    speak::process(
        "voice_model",
        format!(
            "{} (image_input={})",
            client.model(),
            client.capabilities().image_input.label()
        ),
    );

    // Configuration only (no speech): which language Whisper listens for and
    // which Piper voice answers, so a log shows how a session was set up.
    info!(
        target: "process",
        event = "voice_language_configured",
        whisper_language = resolved.whisper_language.code(),
        whisper_model = %file_name(&resolved.whisper_model),
        tts_model = %file_name(&tts_model),
        "voice language configured"
    );
    let dictation = DictationSession::with_services(
        Arc::new(CpalRecorder::with_requested_name(resolved.microphone)),
        Arc::new(
            WhisperTranscriber::new(resolved.whisper_model)
                .with_language(resolved.whisper_language),
        ),
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

    let live_partial_transcription = resolved.live_partial_transcription;
    speak::process(
        "voice_live_partials",
        if live_partial_transcription {
            "live partial transcription enabled"
        } else {
            "live partial transcription disabled for this profile; final transcript only"
        },
    );
    let mut runtime = VoiceRuntime::new(
        // A photo is sent only with the question that took it, and the
        // conversation keeps its last exchanges instead of needing /reset,
        // which cannot be typed in voice mode.
        Controller::new_ephemeral()
            .with_photo_only_on_capturing_turn()
            .with_rolling_history(),
        TurnRuntime::new(client, camera, main_event_sender),
        dictation,
        voice_aux_sender,
        speech,
    );
    runtime.live_partial_transcription = live_partial_transcription;
    // Voice answers arrive and are spoken sentence by sentence.
    runtime.turns.stream_answers = true;
    let mut partial_tick = tokio::time::interval(PARTIAL_TRANSCRIPTION_INTERVAL);
    partial_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Everything above is set up; the next press is heard.
    speak::voice_ready();

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
            _ = partial_tick.tick(), if live_partial_transcription => VoiceEvent::PartialTick,
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
        Event::TurnPartial { id, .. } => speak::SpeechOwner::Turn(*id),
        _ => speak::SpeechOwner::System,
    };
    if let Event::TurnFinished(finished) = &event {
        runtime.retire(finished.id).await;
    }

    let effects = controller.handle(event);
    let accepted_answer = effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Display(
                crate::protocol::Output::Answer(_) | crate::protocol::Output::AnswerStreamed
            )
        )
    });
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
    /// Whether starting a model request prints "Thinking...". Typed mode
    /// needs it; voice mode prints its own "Asking the local model..." when
    /// it sends the request.
    announce_thinking: bool,
    /// Whether answers are requested as a stream and delivered sentence by
    /// sentence (voice). Typed mode shows the whole answer at the end.
    stream_answers: bool,
    /// The streamed answer currently being shown/spoken, if any.
    streamed_answer: Option<StreamedAnswer>,
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

struct StreamedAnswer {
    owner: speak::SpeechOwner,
    last_speech: AnswerSpeechEmission,
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
            announce_thinking: true,
            stream_answers: false,
            streamed_answer: None,
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
            // Direct-audio effects are consumed exclusively by the isolated
            // audio-to-audio runtime. Keeping this branch explicit prevents
            // the established text runtime from accidentally initializing a
            // second pipeline or treating audio bytes as dialogue.
            Effect::StartAudioTurn(_) | Effect::PlayAudio { .. } => {
                speak::warning(
                    "unexpected_direct_audio_effect",
                    "direct-audio effect reached the text runtime and was ignored",
                );
            }
            Effect::PrepareCamera(preparation) => self.prepare_camera(preparation),
            Effect::ListCameras => self.list_cameras(),
            Effect::CancelTurn(id) => self.cancel(id).await,
            Effect::Display(crate::protocol::Output::AnswerSentence(sentence)) => {
                let first = !matches!(
                    &self.streamed_answer,
                    Some(streamed) if streamed.owner == speech_owner
                );
                let speech = output_mode.emit_sentence(sentence, first, speech_owner);
                self.streamed_answer = Some(StreamedAnswer {
                    owner: speech_owner,
                    last_speech: speech,
                });
                return RuntimeEffectResult::continued();
            }
            Effect::Display(crate::protocol::Output::AnswerStreamed) => {
                // The answer is accepted; its completion follows the speech
                // outcome of its last sentence.
                let answer_speech = match self.streamed_answer.take() {
                    Some(streamed) if streamed.owner == speech_owner => streamed.last_speech,
                    _ => AnswerSpeechEmission::None,
                };
                return RuntimeEffectResult {
                    keep_running: true,
                    answer_speech,
                };
            }
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
        // A model request has been authorized and is about to begin. This is
        // deliberately user-interface state, not a tracing message.
        if self.announce_thinking {
            interface::status("Thinking...");
        }
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let client = self.client.clone();
        let camera = self.camera.clone();
        let event_sender = self.event_sender.clone();
        let worker_interaction = interaction.clone();
        let stream = self.stream_answers;
        let handle = tokio::spawn(async move {
            let result = run_turn_with_interaction(
                client,
                camera,
                event_sender.clone(),
                request,
                worker_cancellation.clone(),
                worker_interaction,
                stream,
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
    /// Whether partial ticks may ask for a live snapshot. When false (the
    /// Hailo Pi profile), the recording is never copied or transcribed until
    /// Talk/ENTER is released.
    live_partial_transcription: bool,
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
    /// Whisper has started on the final recording (no transcript yet).
    transcribing: bool,
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
            transcribing: false,
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

    fn has_captured_photo(&self) -> bool {
        self.photos
            .values()
            .any(|photo| matches!(photo, DraftPhoto::Captured(_)))
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
        let mut turns = turns;
        // Voice mode shows "Transcribing..." and "Asking the local
        // model..." itself, at the right moments.
        turns.announce_thinking = false;
        Self {
            controller,
            turns,
            dictation,
            aux_sender,
            speech,
            draft: None,
            live_partial_transcription: true,
            active_photo_capture: None,
            live_transcription: None,
            pending_snapshot: None,
            cancelled_recordings: HashMap::new(),
            pending_answer_speech: HashMap::new(),
            next_draft_id: 1,
            next_partial_sequence: 1,
        }
    }

    /// Prints "Ready" only when nothing for the person is still in progress:
    /// no held or pending draft, no model turn, and dictation idle. This keeps
    /// the prompt after the answer or failure instead of before it.
    fn announce_ready_if_idle(&self) {
        if self.draft.is_none() && !self.controller.has_active_turn() && self.dictation.is_ready() {
            speak::voice_log("ready — hold enter to start recording/transcription");
        }
    }

    async fn handle_main_event(&mut self, event: Event) -> bool {
        let turn_finished = matches!(event, Event::TurnFinished(_));
        let interaction = match &event {
            Event::TurnFinished(finished) => self.turns.active_interaction(finished.id),
            Event::TurnPartial { id, .. } => self.turns.active_interaction(*id),
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
        if turn_finished && result.keep_running {
            // The answer or the model failure has now been displayed.
            self.announce_ready_if_idle();
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
                // Capture starts immediately on press. There is no spoken cue
                // to wait for, so the first word after pressing is recorded.
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
            DictationEvent::RecordingTooShort { recording_id } => {
                // A tap or sticky button. Drop the draft (including any photo
                // request) quietly; the dictation controller shows the
                // "hold longer" notice. Nothing is transcribed or sent.
                if self.draft_matches_recording(*recording_id) {
                    self.cancel_draft();
                }
                Some((*recording_id, "too_short"))
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
                // No words were heard: nothing to ask. The dictation
                // controller shows "No speech heard".
                self.cancel_draft();
                Some((*recording_id, "no_speech"))
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

                // "You: ..." is printed when the request is actually sent
                // (see `try_submit`), so it always follows "Photo captured"
                // even if the photo finishes after Whisper.
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
            // The dictation controller is ready again as soon as a transcript
            // is accepted, but the person's turn is not over until the answer
            // (or a failure) has been shown. "Ready" is printed only then.
            DictationOutput::Ready => self.announce_ready_if_idle(),
            DictationOutput::Recording { .. } => {}
            // Voice mode has already shown "Cancelled." for this Escape; a
            // second "Cancelling recording..." line would appear after it.
            DictationOutput::Discarding => {
                info!(target: "process", event = "voice_recording_discarding", "releasing microphone after cancellation");
            }
            DictationOutput::Transcribing => {
                // Dictation emits this only after the CPAL recording owner has
                // produced `RecordingFinished`, so a too-short or failed
                // recording never shows "Transcribing...". Status only; never
                // spoken.
                if let Some(draft) = self.draft.as_mut() {
                    draft.transcribing = true;
                    draft
                        .interaction
                        .mark("whisper.final_transcription_started");
                }
                self.announce_transcribing_if_ready();
            }
            // `AcceptedTranscript` below provides the exact, one-time final
            // transcript log. Avoid printing it once here and once there.
            DictationOutput::Transcript(_) => {}
            DictationOutput::TooShort => speak::voice_log(speak::TOO_SHORT_VOICE_LOG),
            DictationOutput::NoSpeech => speak::voice_log(speak::NO_SPEECH_VOICE_LOG),
            DictationOutput::Diagnostic(message) => speak::warning("dictation", message),
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
        // "Listening..." is terminal status only. The microphone opens right
        // away; nothing is synthesized or played before capture.
        speak::voice_log("listening — microphone opening");
        self.start_capture(id).await
    }

    /// Handles a worker fact. The speech worker never manipulates microphone
    /// or draft state; it only reports how an accepted answer's audio ended.
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

        // Only accepted answers and fixed error notices are spoken now, so
        // no other speech fact affects microphone or draft state.
        true
    }

    /// Starts the pre-existing DictationSession for the newly pressed draft.
    /// This explicitly reuses its recording-ID lifecycle instead of inventing
    /// another microphone owner.
    async fn start_capture(&mut self, draft_id: u64) -> bool {
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
            info!(target: "process", event = "voice_recording_released", "recording released; waiting for microphone cleanup");
        }
    }

    /// Shows "Transcribing..." while Whisper works on the released
    /// recording, but only after any requested photo has finished, so the
    /// terminal reads in order: listening, photo, transcribing, question,
    /// model. If Whisper already finished while the photo was still being
    /// captured, the line is skipped rather than shown late.
    fn announce_transcribing_if_ready(&self) {
        let ready = self.draft.as_ref().is_some_and(|draft| {
            draft.released
                && draft.transcribing
                && draft.final_transcript.is_none()
                && !draft.has_pending_photos()
                && !draft.cancellation.is_cancelled()
        });
        if ready {
            speak::voice_log("transcribing — whisper is running on the recording");
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
            // `P` is meaningful only while actual capture is open: a press in
            // the brief moment before the microphone reports it is recording
            // is ignored rather than attached to nothing.
            if !draft.held
                || !draft.recording_started
                || draft.released
                || draft.cancellation.is_cancelled()
            {
                return;
            }
            if draft.has_captured_photo() {
                speak::voice_log(
                    "photo capture request rejected — Qwen3 accepts one photo per request; release ENTER and start a new request for another photo",
                );
                return;
            }
            if draft.pending_photo_count() >= MAX_QUEUED_PHOTO_CAPTURES {
                speak::voice_log(
                    "photo capture request rejected — one photo capture is already waiting for the camera",
                );
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
        if !self.live_partial_transcription {
            return;
        }
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
                self.announce_transcribing_if_ready();
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
        if failure.is_some() {
            // Live partials are opportunistic display. A failure here (often
            // just an early, quiet clip) says nothing about the final
            // transcript, so it is never shown as a terminal warning.
            speak::background_warning("partial_transcription_failed");
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
        let failed_without_photo = self.draft.as_ref().is_some_and(|draft| {
            draft.released
                && draft.final_transcript.is_some()
                && !draft.has_pending_photos()
                && !draft.has_captured_photo()
                && draft
                    .photos
                    .values()
                    .any(|photo| matches!(photo, DraftPhoto::Failed))
        });
        if failed_without_photo {
            if let Some(draft) = self.draft.take() {
                draft.cancellation.cancel();
                draft
                    .interaction
                    .complete("photo_capture_failed_no_request", telemetry::Fields::new());
            }
            speak::voice_log(
                "photo capture failed — no local model request was sent; hold ENTER and try again",
            );
            self.announce_ready_if_idle();
            return true;
        }

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

        speak::user_transcript("final", &transcript);
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
        let keep_running = self.handle_main_event(Event::Input(input)).await;
        if self.controller.has_active_turn() {
            // The request really went to the local model; this is where the
            // long wait is, so say so separately from transcription. (If the
            // controller refused it, its message was shown instead.)
            speak::voice_log("model request started");
        }
        keep_running
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
        self.active_photo_capture = None;
        if let Some(draft) = self.draft.take() {
            draft.cancellation.cancel();
            draft.interaction.mark("cancellation.requested");
            if let Some(recording_id) = draft.recording_id {
                self.cancelled_recordings
                    .insert(recording_id, draft.interaction);
            } else {
                // A draft whose recording was never requested (for example,
                // the microphone could not be started) has no microphone or
                // Whisper handle to wait for.
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
            if matches!(outcome, "cancelled" | "too_short" | "no_speech") {
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
        // Voice Mode always runs an Ephemeral controller in Milestone 6.1.
        // Workers above have been cancelled/joined before these references are
        // dropped, so a late result cannot repopulate the cleared session.
        self.controller.clear_ephemeral_session();
    }
}

/// Streams the answer and sends each complete sentence to the controller as
/// soon as it exists. The final (possibly unpunctuated) remainder is sent
/// before this returns, so every sentence precedes `TurnFinished`.
async fn stream_answer_sentences(
    client: &OllamaClient,
    history: &[crate::think::ConversationTurn],
    question: &str,
    image_bytes: &[&[u8]],
    interaction: &Interaction,
    id: TurnId,
    event_sender: &mpsc::Sender<Event>,
) -> Result<crate::think::OllamaAnswer, ThinkError> {
    let (piece_sender, mut pieces) = mpsc::unbounded_channel::<String>();
    let ask = async move {
        let mut forward = move |piece: &str| {
            let _ = piece_sender.send(piece.to_owned());
        };
        client
            .answer_streaming(
                history,
                question,
                image_bytes,
                Some(interaction),
                &mut forward,
            )
            .await
    };
    let deliver = async {
        let mut splitter = speak::sentences::SentenceSplitter::default();
        let mut first = true;
        while let Some(piece) = pieces.recv().await {
            for sentence in splitter.push(&piece) {
                if first {
                    first = false;
                    interaction.mark_first("answer.first_sentence_ready");
                }
                let _ = event_sender
                    .send(Event::TurnPartial { id, text: sentence })
                    .await;
            }
        }
        if let Some(rest) = splitter.finish() {
            let _ = event_sender
                .send(Event::TurnPartial { id, text: rest })
                .await;
        }
    };
    let (answer, ()) = tokio::join!(ask, deliver);
    answer
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
        false,
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
    stream: bool,
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

    let answer = if stream {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                // Dropping the stream closes the connection, which makes the
                // Hailo bridge stop generating right away.
                return TurnResult::Cancelled;
            },
            answer = stream_answer_sentences(
                &client,
                &history,
                &question,
                &image_bytes,
                &interaction,
                id,
                &event_sender,
            ) => answer,
        }
    } else {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return TurnResult::Cancelled;
            },
            answer = client.answer_with_telemetry(&history, &question, &image_bytes, Some(&interaction)) => answer,
        }
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
                streamed: stream,
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

    #[error("security lifecycle rejected the requested operation: {0}")]
    Security(#[source] SecurityError),

    #[error(
        "no working camera was found ({detail}). The raspberry-pi-hailo profile requires one: power off, reseat both ends of the camera ribbon, power on, and confirm with `rpicam-still --list-cameras`"
    )]
    CameraRequired { detail: String },

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

    #[error("could not run the direct audio-to-audio experiment: {0}")]
    AudioToAudio(#[source] crate::audio_to_audio::AudioToAudioError),

    #[error("backend `{backend}` does not implement pipeline `{pipeline}`")]
    UnsupportedPipeline {
        backend: &'static str,
        pipeline: &'static str,
    },

    #[error("backend `{backend}` did not provide its required local configuration")]
    MissingBackendConfiguration { backend: &'static str },
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
        think::{OllamaClient, OllamaConfig, TEACHER_INSTRUCTIONS},
    };

    use super::{
        AppError, TurnRuntime, VoiceRuntime, enter_ephemeral_mode, finish_ephemeral_session,
        run_turn, run_voice_with_options, voice_diagnostic_speech,
    };
    use crate::launch::{ModelBackend, VoiceLaunchOptions, VoicePipeline};
    use crate::security::{OperatingMode, SecurityManager, SecurityState};
    use crate::speak::{SpeechEvent, SpeechOutcome, SpeechOwner, SpeechService};

    fn client(server: &MockServer) -> Arc<OllamaClient> {
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "test-model").unwrap()).unwrap();
        client.set_capabilities_for_test(&["vision"]);
        Arc::new(client)
    }

    #[test]
    fn application_launch_enters_ephemeral_mode_once_without_any_gesture() {
        crate::interface::test_terminal::start();
        let mut security = SecurityManager::new();
        let session = enter_ephemeral_mode(&mut security)
            .expect("launching the application enters the temporary mode immediately");
        assert!(
            crate::interface::test_terminal::lines().is_empty(),
            "the session starts at launch, but the terminal announces it only \
             with the ready line, once input is accepted"
        );
        assert_eq!(security.state(), SecurityState::Uninitialized);
        assert_eq!(security.active_mode(), Some(OperatingMode::Ephemeral));
        assert!(
            enter_ephemeral_mode(&mut security).is_err(),
            "a second lease cannot be acquired while one is active"
        );
        finish_ephemeral_session(&mut security, session).unwrap();
        assert_eq!(security.active_mode(), None);
    }

    #[test]
    fn voice_model_failures_use_a_fixed_non_sensitive_spoken_notice() {
        assert_eq!(
            voice_diagnostic_speech(
                "local inference failed: Hailo VLM inference failed [health=degraded]"
            ),
            Some("The local model could not complete that request. Check the Hailo bridge.")
        );
        assert_eq!(
            voice_diagnostic_speech("local inference failed: HTTP 503"),
            Some("The local model could not complete that request. Check the local model service.")
        );
        assert_eq!(voice_diagnostic_speech("camera capture failed"), None);
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

        /// Handles streamed sentences and then the finished turn.
        async fn accept_streamed_answer(&mut self) {
            loop {
                let event = within_test_timeout(self.main_events.recv())
                    .await
                    .expect("mock streamed completion");
                let finished = matches!(event, Event::TurnFinished(_));
                assert!(self.runtime.handle_main_event(event).await);
                if finished {
                    return;
                }
            }
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

    struct EmptyCameraBackend;

    impl CameraBackend for EmptyCameraBackend {
        fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
            Ok(Vec::new())
        }

        fn capture(
            &self,
            _camera: &CameraDescriptor,
            _cancellation: &CancellationToken,
        ) -> Result<Photo, CameraError> {
            Err(CameraError::NoCameras)
        }
    }

    #[tokio::test]
    async fn required_camera_check_passes_only_when_a_camera_is_listed() {
        assert!(
            super::require_working_camera(&photo_camera()).await.is_ok(),
            "a listed camera satisfies the requirement"
        );

        let empty = CameraService::with_backend(Arc::new(EmptyCameraBackend));
        let error = super::require_working_camera(&empty)
            .await
            .expect_err("an empty camera list is fatal");
        assert!(matches!(error, AppError::CameraRequired { .. }));
        assert!(error.to_string().contains("rpicam-still --list-cameras"));

        let list_calls = Arc::new(AtomicUsize::new(0));
        let capture_calls = Arc::new(AtomicUsize::new(0));
        let failing = CameraService::with_backend(Arc::new(UnexpectedCameraBackend {
            list_calls: list_calls.clone(),
            capture_calls: capture_calls.clone(),
        }));
        assert!(matches!(
            super::require_working_camera(&failing).await,
            Err(AppError::CameraRequired { .. })
        ));
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            capture_calls.load(Ordering::SeqCst),
            0,
            "the startup check never captures a photo"
        );
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
    async fn unsupported_pipeline_backend_is_reported_at_runtime_before_device_setup() {
        let result = run_voice_with_options(VoiceLaunchOptions {
            backend: Some(ModelBackend::Fixture),
            pipeline: Some(VoicePipeline::TranscribeAndSpeak),
            ..VoiceLaunchOptions::default()
        })
        .await;

        assert!(matches!(
            result,
            Err(AppError::UnsupportedPipeline {
                backend: "fixture",
                pipeline: "transcribe-and-speak",
            })
        ));
    }

    #[tokio::test]
    async fn hailo_direct_audio_is_rejected_before_model_or_microphone_setup() {
        let result = run_voice_with_options(VoiceLaunchOptions {
            backend: Some(ModelBackend::HailoVlm),
            pipeline: Some(VoicePipeline::AudioToAudio),
            ..VoiceLaunchOptions::default()
        })
        .await;

        assert!(matches!(
            result,
            Err(AppError::UnsupportedPipeline {
                backend: "hailo-vlm",
                pipeline: "audio-to-audio",
            })
        ));
    }

    #[tokio::test]
    async fn model_without_advertised_vision_rejects_camera_preparation_before_device_access() {
        let server = MockServer::start().await;
        let client =
            OllamaClient::new(OllamaConfig::new(&server.uri(), "text-model").unwrap()).unwrap();
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
        let mut controller = Controller::new_private_for_test();
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
    async fn disabled_live_partials_never_copy_or_transcribe_while_held() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(
            &server,
            ["only the final transcript".to_owned()],
            unused_camera_service(),
        );
        harness.runtime.live_partial_transcription = false;
        let recording_id = harness.start_hold().await;

        for _ in 0..3 {
            harness.runtime.request_partial_snapshot();
        }
        tokio::task::yield_now().await;
        assert!(
            harness.dictation_events.try_recv().is_err(),
            "no snapshot may be requested from the recorder"
        );
        assert_eq!(harness.transcriber.calls.load(Ordering::SeqCst), 0);

        let text = harness.release_hold(recording_id).await;
        assert_eq!(text, "only the final transcript");
        assert_eq!(harness.transcriber.calls.load(Ordering::SeqCst), 1);
    }

    async fn mock_answer(server: &MockServer, answer: &str) {
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": answer},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(server)
            .await;
    }

    fn photo_camera() -> CameraService {
        let (camera, photo) = fake_camera();
        CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera,
            photo,
            fail: false,
            captures: Arc::new(AtomicUsize::new(0)),
        }))
    }

    #[tokio::test]
    async fn terminal_shows_a_text_turn_once_and_in_order() {
        let server = MockServer::start().await;
        mock_answer(&server, "an answer").await;
        let mut harness =
            voice_harness(&server, ["a question".to_owned()], unused_camera_service());
        crate::interface::test_terminal::start();

        let _ = harness.submit_next_transcript().await;
        harness.accept_next_answer().await;

        assert_eq!(
            crate::interface::test_terminal::lines(),
            [
                "Listening...",
                "Transcribing...",
                "You: a question",
                "Asking the local model...",
                "Independence: an answer",
                "Ready. Hold ENTER to speak.",
            ]
        );
    }

    #[tokio::test]
    async fn terminal_shows_a_photo_turn_once_and_in_order() {
        let server = MockServer::start().await;
        mock_answer(&server, "a photo answer").await;
        let mut harness = voice_harness(&server, ["what is this".to_owned()], photo_camera());
        crate::interface::test_terminal::start();

        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        let photo_finished = harness.next_aux_event().await;
        assert!(harness.runtime.handle_aux_event(photo_finished).await);
        let _ = harness.release_hold(recording_id).await;
        harness.accept_next_answer().await;

        assert_eq!(
            crate::interface::test_terminal::lines(),
            [
                "Listening...",
                "Photo captured. 1 photo attached.",
                "Transcribing...",
                "You: what is this",
                "Asking the local model...",
                "Independence: a photo answer",
                "Ready. Hold ENTER to speak.",
            ]
        );
    }

    #[tokio::test]
    async fn terminal_keeps_photo_first_when_release_comes_before_the_photo() {
        // Reproduces the Pi runs: releasing during capture printed the
        // processing status before "Photo captured" (and once, twice). The
        // photo line must come first and nothing may repeat.
        let server = MockServer::start().await;
        mock_answer(&server, "a fridge answer").await;
        let (camera_service, open_gate) = gated_camera(false);
        let mut harness = voice_harness(
            &server,
            ["what is on the fridge".to_owned()],
            camera_service,
        );
        crate::interface::test_terminal::start();

        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        let _ = harness.release_hold(recording_id).await;
        open_gate.send(()).unwrap();
        let photo_finished = harness.next_aux_event().await;
        assert!(harness.runtime.handle_aux_event(photo_finished).await);
        harness.accept_next_answer().await;

        // Whisper finished while the camera was still busy, so
        // "Transcribing..." is skipped rather than shown after the fact.
        let lines = crate::interface::test_terminal::lines();
        assert_eq!(
            lines,
            [
                "Listening...",
                "Photo captured. 1 photo attached.",
                "You: what is on the fridge",
                "Asking the local model...",
                "Independence: a fridge answer",
                "Ready. Hold ENTER to speak.",
            ]
        );
    }

    #[tokio::test]
    async fn terminal_shows_a_too_short_press_as_guidance_then_ready() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(&server, ["unused".to_owned()], unused_camera_service());
        crate::interface::test_terminal::start();

        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldEnd))
                .await
        );
        let _ = harness.next_dictation_event().await; // synthetic finished audio
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::RecordingTooShort { recording_id })
                .await
        );

        assert_eq!(
            crate::interface::test_terminal::lines(),
            [
                "Listening...",
                "Recording too short. Hold ENTER while you speak.",
                "Ready. Hold ENTER to speak.",
            ]
        );
    }

    #[tokio::test]
    async fn terminal_shows_no_speech_and_sends_nothing_when_no_words_were_heard() {
        let server = MockServer::start().await;
        // Whisper's markers such as "[BLANK_AUDIO]" are removed before the
        // transcript reaches the runtime, leaving an empty transcript.
        let mut harness = voice_harness(&server, [String::new()], unused_camera_service());
        crate::interface::test_terminal::start();

        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldEnd))
                .await
        );
        let finished = harness.next_dictation_event().await;
        assert!(harness.runtime.handle_dictation_event(finished).await);
        let transcript = harness.next_dictation_event().await;
        assert!(matches!(
            &transcript,
            DictationEvent::TranscriptReady { recording_id: id, .. } if *id == recording_id
        ));
        assert!(harness.runtime.handle_dictation_event(transcript).await);

        assert_eq!(
            crate::interface::test_terminal::lines(),
            [
                "Listening...",
                "Transcribing...",
                "No speech heard. Hold ENTER, speak, then release.",
                "Ready. Hold ENTER to speak.",
            ]
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(harness.runtime.controller.accepted_history().is_empty());
    }

    #[tokio::test]
    async fn terminal_shows_escape_once_then_ready() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(&server, ["unused".to_owned()], unused_camera_service());
        crate::interface::test_terminal::start();

        let _recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
                .await
        );
        let cancelled = harness.next_dictation_event().await;
        assert!(harness.runtime.handle_dictation_event(cancelled).await);

        assert_eq!(
            crate::interface::test_terminal::lines(),
            ["Listening...", "Cancelled.", "Ready. Hold ENTER to speak."]
        );
    }

    #[tokio::test]
    async fn terminal_shows_a_failed_photo_once_and_ready_only_at_the_end() {
        let server = MockServer::start().await;
        let (camera, photo) = fake_camera();
        let camera_service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera,
            photo,
            fail: true,
            captures: Arc::new(AtomicUsize::new(0)),
        }));
        let mut harness = voice_harness(&server, ["what is here".to_owned()], camera_service);
        crate::interface::test_terminal::start();

        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        let photo_finished = harness.next_aux_event().await;
        assert!(harness.runtime.handle_aux_event(photo_finished).await);
        let _ = harness.release_hold(recording_id).await;

        let lines = crate::interface::test_terminal::lines();
        assert_eq!(lines.first().map(String::as_str), Some("Listening..."));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("Ready. Hold ENTER to speak.")
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.as_str() == "Transcribing...")
                .count(),
            1
        );
        assert!(
            !lines.iter().any(|line| line == "Asking the local model..."),
            "a failed photo never reaches the model: {lines:?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("Ready."))
                .count(),
            1,
            "{lines:?}"
        );
        assert!(!lines.iter().any(|line| line.starts_with("Independence:")));
    }

    fn ndjson(lines: &[serde_json::Value]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    #[tokio::test]
    async fn voice_answer_streams_sentence_by_sentence_to_screen_and_speech() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ndjson(&[
                json!({"message": {"role": "assistant", "content": "The fridge has"}, "done": false}),
                json!({"message": {"role": "assistant", "content": " magnets. Some"}, "done": false}),
                json!({"message": {"role": "assistant", "content": " are round."}, "done": false}),
                json!({"message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "stop"}),
            ])))
            .mount(&server)
            .await;
        let mut harness = voice_harness(
            &server,
            ["what is on it".to_owned()],
            unused_camera_service(),
        );
        harness.runtime.turns.stream_answers = true;
        crate::interface::test_terminal::start();

        let _ = harness.submit_next_transcript().await;
        harness.accept_streamed_answer().await;

        assert_eq!(
            crate::interface::test_terminal::lines(),
            [
                "Listening...",
                "Transcribing...",
                "You: what is on it",
                "Asking the local model...",
                "Independence: The fridge has magnets.",
                "              Some are round.",
                "Ready. Hold ENTER to speak.",
            ]
        );
        assert_eq!(
            harness.runtime.speech.handle().utterances_requested(),
            2,
            "each sentence is its own utterance"
        );
        let history = harness.runtime.controller.accepted_history();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].assistant,
            "The fridge has magnets. Some are round."
        );

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn a_new_press_during_a_streamed_answer_keeps_it_out_of_history() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(ndjson(&[
                        json!({"message": {"role": "assistant", "content": "Too late."}, "done": false}),
                        json!({"message": {"role": "assistant", "content": ""}, "done": true}),
                    ]))
                    .set_delay(Duration::from_millis(500)),
            )
            .mount(&server)
            .await;
        let mut harness = voice_harness(
            &server,
            ["first question".to_owned(), "unused".to_owned()],
            unused_camera_service(),
        );
        harness.runtime.turns.stream_answers = true;

        let _ = harness.submit_next_transcript().await;
        assert!(harness.runtime.controller.has_active_turn());
        // Pressing ENTER again stops the in-flight answer.
        let _second = harness.start_hold().await;
        assert!(!harness.runtime.controller.has_active_turn());
        tokio::time::sleep(Duration::from_millis(700)).await;
        while let Ok(event) = harness.main_events.try_recv() {
            assert!(harness.runtime.handle_main_event(event).await);
        }
        assert!(harness.runtime.controller.accepted_history().is_empty());
        assert_eq!(harness.runtime.speech.handle().utterances_requested(), 0);
    }

    #[tokio::test]
    async fn voice_follow_up_after_a_photo_does_not_resend_the_photo() {
        let server = MockServer::start().await;
        mock_answer(&server, "an answer").await;
        let mut harness = voice_harness(
            &server,
            ["what is this".to_owned(), "what color is it".to_owned()],
            photo_camera(),
        );
        harness.runtime.controller = Controller::new().with_photo_only_on_capturing_turn();

        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        let photo_finished = harness.next_aux_event().await;
        assert!(harness.runtime.handle_aux_event(photo_finished).await);
        let _ = harness.release_hold(recording_id).await;
        harness.accept_next_answer().await;
        let _ = harness.submit_next_transcript().await;
        harness.accept_next_answer().await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let image_count = |index: usize| {
            let body: serde_json::Value = serde_json::from_slice(&requests[index].body).unwrap();
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| message["images"].as_array().map_or(0, Vec::len))
                .sum::<usize>()
        };
        assert_eq!(
            image_count(0),
            1,
            "the photo goes with the turn that took it"
        );
        assert_eq!(image_count(1), 0, "the follow-up sends text only");
    }

    #[tokio::test]
    async fn press_opens_the_microphone_immediately_without_any_spoken_cue() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(
            &server,
            ["immediate question".to_owned()],
            unused_camera_service(),
        );

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldStart))
                .await
        );
        // Capture is requested synchronously by the press itself; nothing
        // waits for Piper or playback first.
        assert_eq!(harness.recorder.starts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            harness.next_dictation_event().await,
            DictationEvent::RecordingStarted { .. }
        ));
        assert_eq!(harness.runtime.speech.handle().utterances_requested(), 0);
    }

    #[tokio::test]
    async fn status_changes_through_transcription_are_never_spoken() {
        let server = MockServer::start().await;
        let mut harness =
            voice_harness(&server, ["a question".to_owned()], unused_camera_service());
        let recording_id = harness.start_hold().await;
        // Release → microphone released → transcribing → accepted transcript.
        // None of these status changes may queue speech; only an accepted
        // model answer may.
        let _ = harness.release_hold(recording_id).await;
        assert_eq!(harness.runtime.speech.handle().utterances_requested(), 0);
    }

    #[tokio::test]
    async fn release_before_the_microphone_reports_started_still_finishes_that_recording() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(&server, ["unused".to_owned()], unused_camera_service());

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldStart))
                .await
        );
        let started = harness.next_dictation_event().await;
        // A very short press: the release arrives before the runtime has
        // processed the microphone's started event.
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldEnd))
                .await
        );
        assert!(harness.runtime.handle_dictation_event(started).await);
        assert_eq!(harness.recorder.finishes.load(Ordering::SeqCst), 1);
        assert_eq!(harness.recorder.cancels.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn escape_right_after_press_cancels_the_new_recording() {
        let server = MockServer::start().await;
        let mut harness = voice_harness(&server, ["unused".to_owned()], unused_camera_service());
        let recording_id = harness.start_hold().await;

        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
                .await
        );
        let cancelled = harness.next_dictation_event().await;
        assert!(matches!(
            &cancelled,
            DictationEvent::RecordingCancelled { recording_id: id } if *id == recording_id
        ));
        assert!(harness.runtime.handle_dictation_event(cancelled).await);
        assert!(harness.runtime.draft.is_none());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn p_before_the_microphone_reports_recording_does_not_activate_a_camera() {
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
    async fn repeated_p_presses_keep_qwen3_voice_requests_to_one_photo() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "one-photo answer"},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let (camera, photo) = fake_camera();
        let captures = Arc::new(AtomicUsize::new(0));
        let camera_service = CameraService::with_backend(Arc::new(SequencedCameraBackend {
            camera,
            photos: Mutex::new(VecDeque::from([photo])),
            captures: captures.clone(),
        }));

        let mut harness = voice_harness(&server, ["describe this".to_owned()], camera_service);
        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        let completion = harness.next_aux_event().await;
        assert!(matches!(
            &completion,
            super::VoiceAuxEvent::PhotoCaptureFinished { result: Ok(_), .. }
        ));
        assert!(harness.runtime.handle_aux_event(completion).await);

        // A second P press is rejected after the first image is captured; it
        // cannot become a second image in the eventual request.
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(harness.release_hold(recording_id).await, "describe this");
        harness.accept_next_answer().await;
        assert_eq!(harness.runtime.controller.accepted_photos().len(), 1);
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["messages"][1]["images"].as_array().unwrap().len(), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn failed_voice_photo_does_not_fall_back_to_a_text_only_request() {
        let server = MockServer::start().await;
        let (camera, photo) = fake_camera();
        let captures = Arc::new(AtomicUsize::new(0));
        let camera_service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera,
            photo,
            fail: true,
            captures: captures.clone(),
        }));
        let mut harness = voice_harness(&server, ["what is here".to_owned()], camera_service);
        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        let completion = harness.next_aux_event().await;
        assert!(matches!(
            &completion,
            super::VoiceAuxEvent::PhotoCaptureFinished { result: Err(_), .. }
        ));
        assert!(harness.runtime.handle_aux_event(completion).await);

        assert_eq!(harness.release_hold(recording_id).await, "what is here");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), harness.main_events.recv())
                .await
                .is_err(),
            "a failed explicit photo must not submit a text-only model request"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        assert_eq!(captures.load(Ordering::SeqCst), 1);
    }

    /// A camera whose capture blocks until the test opens its gate, so a
    /// release can deterministically happen while the photo is in progress.
    struct GatedCameraBackend {
        camera: CameraDescriptor,
        photo: Photo,
        fail: bool,
        gate: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl CameraBackend for GatedCameraBackend {
        fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
            Ok(vec![self.camera.clone()])
        }

        fn capture(
            &self,
            _camera: &CameraDescriptor,
            _cancellation: &CancellationToken,
        ) -> Result<Photo, CameraError> {
            let _ = self
                .gate
                .lock()
                .expect("synthetic camera gate lock")
                .recv_timeout(Duration::from_secs(3));
            if self.fail {
                Err(CameraError::NoCameras)
            } else {
                Ok(self.photo.clone())
            }
        }
    }

    fn gated_camera(fail: bool) -> (CameraService, std::sync::mpsc::Sender<()>) {
        let (camera, photo) = fake_camera();
        let (open_gate, gate) = std::sync::mpsc::channel();
        (
            CameraService::with_backend(Arc::new(GatedCameraBackend {
                camera,
                photo,
                fail,
                gate: Mutex::new(gate),
            })),
            open_gate,
        )
    }

    #[tokio::test]
    async fn release_during_photo_capture_waits_and_sends_transcript_with_the_photo() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "a photo answer"},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;
        let (camera_service, open_gate) = gated_camera(false);
        let mut harness = voice_harness(&server, ["what is this".to_owned()], camera_service);
        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );

        // Release immediately: the camera is still capturing. The recording
        // finishes normally and the transcript is accepted, but nothing is
        // sent until the photo is ready.
        assert_eq!(harness.release_hold(recording_id).await, "what is this");
        assert!(
            harness.runtime.draft.is_some(),
            "the draft waits for its photo"
        );
        assert!(server.received_requests().await.unwrap().is_empty());

        open_gate.send(()).unwrap();
        let photo_finished = harness.next_aux_event().await;
        assert!(matches!(
            &photo_finished,
            super::VoiceAuxEvent::PhotoCaptureFinished { result: Ok(_), .. }
        ));
        assert!(harness.runtime.handle_aux_event(photo_finished).await);
        harness.accept_next_answer().await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "exactly one request with transcript and photo"
        );
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let last = body["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["content"], "what is this");
        assert_eq!(last["images"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn release_during_a_photo_that_then_fails_sends_nothing() {
        let server = MockServer::start().await;
        let (camera_service, open_gate) = gated_camera(true);
        let mut harness = voice_harness(&server, ["what is this".to_owned()], camera_service);
        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        assert_eq!(harness.release_hold(recording_id).await, "what is this");

        open_gate.send(()).unwrap();
        let photo_finished = harness.next_aux_event().await;
        assert!(harness.runtime.handle_aux_event(photo_finished).await);
        assert!(harness.runtime.draft.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), harness.main_events.recv())
                .await
                .is_err(),
            "a failed photo must not become a text-only request"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn escape_while_waiting_for_a_photo_sends_nothing() {
        let server = MockServer::start().await;
        let (camera_service, open_gate) = gated_camera(false);
        let mut harness = voice_harness(&server, ["what is this".to_owned()], camera_service);
        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Photo))
                .await
        );
        assert_eq!(harness.release_hold(recording_id).await, "what is this");
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::Escape))
                .await
        );
        assert!(harness.runtime.draft.is_none());

        // A late photo belongs to a cancelled draft. Whether or not its
        // worker still reports, it must stay inert.
        open_gate.send(()).unwrap();
        if let Ok(Some(photo_finished)) =
            tokio::time::timeout(Duration::from_millis(200), harness.aux_events.recv()).await
        {
            assert!(harness.runtime.handle_aux_event(photo_finished).await);
        }
        assert!(harness.runtime.draft.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), harness.main_events.recv())
                .await
                .is_err()
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn too_short_recording_drops_the_draft_without_transcribing_or_asking() {
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
        let recording_id = harness.start_hold().await;
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::Input(DictationInput::HoldEnd))
                .await
        );
        // The synthetic recorder always reports finished audio; real CPAL
        // capture reports a too-short recording instead. Discard the former
        // and deliver the latter.
        assert!(matches!(
            harness.next_dictation_event().await,
            DictationEvent::RecordingFinished { .. }
        ));
        assert!(
            harness
                .runtime
                .handle_dictation_event(DictationEvent::RecordingTooShort { recording_id })
                .await
        );
        assert!(harness.runtime.draft.is_none());
        assert!(harness.runtime.dictation.is_ready());
        assert_eq!(harness.transcriber.calls.load(Ordering::SeqCst), 0);
        assert!(server.received_requests().await.unwrap().is_empty());

        // The next press starts a fresh, normal recording.
        let next = harness.start_hold().await;
        assert_ne!(next, recording_id);
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

        // The accepted exchange exists for this one live Ephemeral session,
        // then runtime shutdown drops application-owned context. A fresh
        // launch has no controller history to inherit.
        harness.runtime.shutdown().await;
        assert!(harness.runtime.controller.accepted_history().is_empty());
        assert!(harness.runtime.controller.accepted_photos().is_empty());
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
                        streamed: false,
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
                        streamed: false,
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
