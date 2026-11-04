//! Local speech plus the diagnostic boundary.
//!
//! Human-facing terminal text belongs to [`crate::interface`]. This module
//! owns optional speech services and emits only sanitized operational metadata
//! through `tracing`; it never sends conversation text to diagnostics.

use std::{env, path::Path, sync::Once};

use tracing::{Level, Metadata, error, info, warn};
use tracing_subscriber::{Layer, filter::filter_fn, layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    interface,
    protocol::{DictationOutput, Output},
};

pub mod audio;
pub mod playback;
pub mod tts;
pub mod worker;

pub use crate::interface::HELP;
pub use worker::{
    SpeechEvent, SpeechHandle, SpeechOutcome, SpeechOwner, SpeechService, UtteranceId,
};

static LOGGING: Once = Once::new();

/// The four diagnostic severities Independence supports. The terminal UI does
/// not enable this sink by default; a developer opts in with
/// `INDEPENDENCE_LOG=error|warn|info|debug` and can redirect stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagnosticVerbosity {
    Off,
    Error,
    Warn,
    Info,
    Debug,
}

impl DiagnosticVerbosity {
    fn from_environment() -> Self {
        env::var("INDEPENDENCE_LOG")
            .ok()
            .as_deref()
            .and_then(Self::parse)
            .unwrap_or(Self::Off)
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "off" | "" => Some(Self::Off),
            // `trace` is deliberately unsupported: Independence exposes only
            // the four levels above, and dependency traces are not an
            // appropriate terminal user interface.
            "trace" => Some(Self::Off),
            _ => None,
        }
    }

    fn accepts(self, level: &Level) -> bool {
        match self {
            Self::Off => false,
            Self::Error => *level == Level::ERROR,
            Self::Warn => matches!(*level, Level::ERROR | Level::WARN),
            Self::Info => matches!(*level, Level::ERROR | Level::WARN | Level::INFO),
            Self::Debug => matches!(
                *level,
                Level::ERROR | Level::WARN | Level::INFO | Level::DEBUG
            ),
        }
    }
}

/// Installs the diagnostic subscriber used by every Independence mode.
///
/// User-facing output goes through `crate::interface` on stdout. Diagnostics
/// are opt-in and go to stderr, so `stdout` can be redirected without copying
/// developer logging and `stderr` can be redirected without copying dialogue.
pub fn init_logging() {
    LOGGING.call_once(|| {
        let verbosity = DiagnosticVerbosity::from_environment();
        let terminal = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_level(true)
            .with_writer(std::io::stderr)
            .with_filter(filter_fn(move |metadata| {
                diagnostic_accepts_metadata(metadata, verbosity)
            }));
        // Telemetry's custom JSONL sink is deliberately separate from this
        // layer. Both paths receive only redacted operational metadata.
        let otel = tracing_opentelemetry::layer()
            .with_tracer(opentelemetry::global::tracer("independence"))
            .with_filter(filter_fn(telemetry_accepts_metadata));
        tracing_subscriber::registry()
            .with(terminal)
            .with(otel)
            .init();
    });
}

/// Filters the opt-in stderr sink to Independence-owned targets and the four
/// supported diagnostic levels. This prevents dependency trace/debug chatter
/// from becoming part of the person-facing terminal experience.
fn diagnostic_accepts_metadata(metadata: &Metadata<'_>, verbosity: DiagnosticVerbosity) -> bool {
    independence_diagnostic_target(metadata.target()) && verbosity.accepts(metadata.level())
}

/// OTLP is an explicit external-destination boundary. Keep trace-level
/// dependency chatter and any future `dialogue` target out of it. Application
/// events carry only redacted metadata; conversation text never enters this
/// layer in the first place.
fn telemetry_accepts_metadata(metadata: &Metadata<'_>) -> bool {
    independence_diagnostic_target(metadata.target()) && *metadata.level() != Level::TRACE
}

fn independence_diagnostic_target(target: &str) -> bool {
    matches!(
        target,
        "process" | "speech" | "telemetry" | "independence_memory"
    )
}

/// Displays an authorized answer without sending it to diagnostics.
pub fn independence_says(text: &str) {
    interface::answer(text);
}

/// Displays only a final recognized microphone transcript. Partial text stays
/// out of both diagnostics and the terminal interface.
pub fn user_transcript(stage: &'static str, text: &str) {
    if stage == "final" {
        interface::final_transcript(text);
    }
}

/// Announces that an accepted voice request is being processed. Its text and
/// photo details remain outside diagnostics.
pub fn voice_submission(_text: &str, _photos: usize) {
    interface::status("Thinking...");
}

/// Emits a safe operational event and, when appropriate, a human-facing
/// status. `detail` is intentionally never passed to `tracing`.
pub fn process(event: &'static str, detail: impl std::fmt::Display) {
    let detail = detail.to_string();
    interface::process_status(event, &detail);
    info!(target: "process", event, "operational state changed");
}

/// Emits a safe recoverable-condition event and an actionable user-facing
/// warning. Arbitrary backend detail is deliberately kept out of both
/// diagnostics and the normal terminal interface.
pub fn warning(event: &'static str, _detail: impl std::fmt::Display) {
    interface::operational_warning(event);
    warn!(target: "process", event, "recoverable operational condition");
}

/// Lists audio output devices without opening a stream or producing sound.
/// The exact identifiers can be passed to `--audio-output` or
/// `INDEPENDENCE_AUDIO_OUTPUT` for a later voice launch.
pub fn list_audio_outputs() -> Result<(), playback::PlaybackError> {
    let devices = playback::list_output_devices()?;
    if devices.is_empty() {
        warning(
            "audio_output_unavailable",
            "No audio output devices were reported by the local audio system.",
        );
        return Ok(());
    }

    interface::audio_outputs(
        &devices
            .iter()
            .map(|device| (device.id.clone(), device.name.clone(), device.is_default))
            .collect::<Vec<_>>(),
    );
    info!(target: "speech", event = "audio_output_listed", count = devices.len(), "audio output devices listed");
    for device in devices {
        info!(
            target: "speech",
            event = "output_available",
            default = device.is_default,
            id = %device.id,
            device = ?device.name,
            "audio output"
        );
    }
    Ok(())
}

/// Displays a top-level application error and records only its classification.
pub fn application_error(detail: impl std::fmt::Display) {
    interface::error(&detail.to_string());
    error!(target: "process", event = "application_error", "application failed");
}

pub fn usage(detail: &str) {
    interface::error(detail);
    error!(target: "process", event = "usage", "invalid command-line usage");
}

pub fn welcome() {
    process("ready", "Independence is ready; type /help for commands");
}

/// Renders controller-authorized terminal output and records only its category.
pub fn emit(output: Output) {
    let kind = match &output {
        Output::Answer(_) => "answer",
        Output::Diagnostic(_) => "notice",
        Output::Cameras(_) => "camera_list",
        Output::Help => "help",
    };
    interface::emit(output);
    info!(target: "process", event = "interface_output_displayed", output_kind = kind, "terminal interface output displayed");
}

/// Compatibility entry point for callers that are in raw terminal mode. The
/// interface writer itself is raw-mode-safe, so it needs no second path.
pub fn emit_raw(output: Output) {
    emit(output);
}

/// User-facing lifecycle text for button-controlled voice mode. The text is
/// never copied into diagnostic output.
pub fn voice_log(message: &str) {
    interface::voice_status(message);
    info!(target: "process", event = "voice_status_changed", "voice status changed");
}

/// Displays a fixed voice cue as status rather than as a logged answer.
pub fn voice_cue(text: &str) {
    interface::voice_cue(text);
    info!(target: "speech", event = "voice_cue_displayed", "voice cue displayed");
}

/// Displays non-sensitive setup information before standalone dictation
/// enters READY.
pub fn dictation_welcome(microphone: Result<&str, &str>, model_path: &Path, model_available: bool) {
    process("mode", "dictation — local transcription only");
    match microphone {
        Ok(microphone) => process("microphone", microphone),
        Err(message) => warning("microphone_unavailable", message),
    }
    let availability = if model_available {
        "available"
    } else {
        "missing; recording will not start"
    };
    process(
        "whisper_model",
        format!("{} ({availability})", model_path.display()),
    );
    process(
        "controls",
        "Press ENTER to start or finish recording. Press ESC to discard. Press Ctrl+C to exit.",
    );
    process(
        "privacy",
        "Your transcript is displayed only and is never sent to Ollama.",
    );
}

/// Displays setup information for button-controlled voice questions.
pub fn voice_welcome(microphone: Result<&str, &str>, model_path: &Path, model_available: bool) {
    process(
        "voice_prompt",
        "hold down Enter to start recording/transcription",
    );
    process("mode", "voice — local transcription and local Ollama");
    match microphone {
        Ok(microphone) => process("microphone", microphone),
        Err(message) => warning("microphone_unavailable", message),
    }
    let availability = if model_available {
        "available"
    } else {
        "missing; recording will not start"
    };
    process(
        "whisper_model",
        format!("{} ({availability})", model_path.display()),
    );
    process(
        "controls",
        "Release ENTER to submit. Press P while holding ENTER to take a photo. Press ESC to cancel.",
    );
    process(
        "safety",
        "Spoken command-like text is treated as a question, not a terminal command.",
    );
}

/// Renders standalone dictation lifecycle output without logging a transcript.
pub fn emit_dictation(output: DictationOutput) {
    let kind = match &output {
        DictationOutput::Ready => "ready",
        DictationOutput::Recording { .. } => "recording",
        DictationOutput::Discarding => "discarding",
        DictationOutput::Transcribing => "transcribing",
        DictationOutput::Transcript(_) => "transcript",
        DictationOutput::Diagnostic(_) => "notice",
    };
    interface::emit_dictation(output);
    info!(target: "process", event = "dictation_interface_output_displayed", output_kind = kind, "dictation interface output displayed");
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use tracing::{debug, error, info, warn};
    use tracing_subscriber::{
        Layer,
        filter::filter_fn,
        fmt::{self, writer::MakeWriter},
        layer::SubscriberExt,
    };

    use super::{DiagnosticVerbosity, diagnostic_accepts_metadata};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    struct CaptureWriter(Capture);

    impl Write for CaptureWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .0
                .lock()
                .expect("capture lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for Capture {
        type Writer = CaptureWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            CaptureWriter(self.clone())
        }
    }

    #[test]
    fn only_four_diagnostic_levels_are_supported() {
        assert_eq!(
            DiagnosticVerbosity::parse("error"),
            Some(DiagnosticVerbosity::Error)
        );
        assert_eq!(
            DiagnosticVerbosity::parse("warn"),
            Some(DiagnosticVerbosity::Warn)
        );
        assert_eq!(
            DiagnosticVerbosity::parse("info"),
            Some(DiagnosticVerbosity::Info)
        );
        assert_eq!(
            DiagnosticVerbosity::parse("debug"),
            Some(DiagnosticVerbosity::Debug)
        );
        assert_eq!(
            DiagnosticVerbosity::parse("trace"),
            Some(DiagnosticVerbosity::Off)
        );
        assert_eq!(DiagnosticVerbosity::parse("verbose"), None);
    }

    #[test]
    fn conversation_shaped_details_do_not_enter_captured_diagnostics() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_writer(capture.clone())
                .with_filter(filter_fn(|metadata| {
                    diagnostic_accepts_metadata(metadata, DiagnosticVerbosity::Debug)
                })),
        );

        let mut interface_output = Vec::new();
        crate::interface::write_transcript_to(&mut interface_output, "USER_SENTINEL_7a1f")
            .expect("transcript renders in the interface");
        crate::interface::write_answer_to(&mut interface_output, "ASSISTANT_SENTINEL_9c2d")
            .expect("answer renders in the interface");
        crate::interface::write_answer_to(
            &mut interface_output,
            "TOOL_ARGUMENTS_SENTINEL_1e4b MEMORY_CONTENT_SENTINEL_5f8a",
        )
        .expect("private test text renders in the interface");
        let interface_output = String::from_utf8(interface_output).expect("interface is UTF-8");
        assert!(interface_output.contains("USER_SENTINEL_7a1f"));
        assert!(interface_output.contains("ASSISTANT_SENTINEL_9c2d"));

        tracing::subscriber::with_default(subscriber, || {
            super::voice_submission("USER_SENTINEL_7a1f", 1);
            super::warning(
                "dictation",
                "TOOL_ARGUMENTS_SENTINEL_1e4b MEMORY_CONTENT_SENTINEL_5f8a",
            );
            error!(target: "process", event = "test_error", "safe error metadata");
            warn!(target: "process", event = "test_warn", "safe warning metadata");
            info!(target: "process", event = "test_info", "safe info metadata");
            debug!(target: "process", event = "test_debug", "safe debug metadata");
        });

        let output = String::from_utf8(capture.0.lock().expect("capture lock").clone())
            .expect("diagnostics are UTF-8");
        assert!(output.contains("safe error metadata"));
        assert!(output.contains("safe warning metadata"));
        assert!(output.contains("safe info metadata"));
        assert!(output.contains("safe debug metadata"));
        assert!(!output.contains("USER_SENTINEL_7a1f"));
        assert!(!output.contains("ASSISTANT_SENTINEL_9c2d"));
        assert!(!output.contains("TOOL_ARGUMENTS_SENTINEL_1e4b"));
        assert!(!output.contains("MEMORY_CONTENT_SENTINEL_5f8a"));
    }
}
