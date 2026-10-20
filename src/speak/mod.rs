//! Terminal logging and the prototype's user-facing dialogue boundary.
//!
//! Every line Independence writes to the terminal is a structured `tracing`
//! event. The application decides which text is dialogue and which is a
//! process diagnostic; this module formats it safely for ordinary and raw-key
//! terminal modes. A later speech worker receives dialogue through an
//! explicit call site, never by scraping these logs.

use std::{
    io::{self, Write},
    path::Path,
    sync::Once,
};

use tracing::{error, info, warn};
use tracing_subscriber::fmt::writer::MakeWriter;

use crate::protocol::{DictationOutput, Output};

pub mod playback;
pub mod tts;
pub mod worker;

pub use worker::{
    SpeechEvent, SpeechHandle, SpeechOutcome, SpeechOwner, SpeechService, UtteranceId,
};

pub const HELP: &str = r#"Independence local observation prototype

Type a question to ask the local Ollama model.
/look <path> | <question>  load one explicit JPEG/PNG and ask about it
/cameras                    list usable local V4L2 camera identifiers
/camera | <question>        capture from the only listed camera and ask
/camera <id> | <question>   capture from one explicitly selected camera
/stop                      cancel the active application turn
/reset                     cancel active work and clear accepted scene/history
/help                      show this help
/quit                      exit

Paths are literal text, never shell commands. Relative paths resolve from the
directory where Independence was started; spaces are allowed. `|` separates a
path from its question, so a path containing `|` is not supported.

Only the last successfully accepted scene is used for follow-ups. A /look or
/camera photo replaces that scene only after its answer succeeds. Camera
selection is checked before it supersedes active work. If multiple cameras are
listed, /camera requires an ID; it never guesses a /dev/video node. A camera
capture briefly opens the selected device, discards up to two warm-up frames,
keeps one validated JPEG in memory, then releases the device before Ollama is
contacted. Photos must be JPEG or PNG and at most 10 MiB. History is in memory
only and caps at six accepted exchanges; use /reset or /look to begin a new
scene.

Camera captures are not saved by Independence, but the original source file
for /look remains where it was.
This observation mode has keyboard input and terminal output only: no
microphone, speech synthesis, persistent memory, continuous vision, streaming,
or autonomous tools. /stop cancels this application's request task, but cannot
prove that a separate Ollama server has stopped its own computation immediately.

For the deliberately separate local microphone experiment, start
`cargo run -- --dictate`. Its transcript is displayed only; it never becomes a
question, slash command, camera request, or Ollama request.

For integrated voice questions, start `cargo run -- --voice`. It uses the
same local microphone and Whisper setup, displays each accepted transcript,
then sends that text as a literal question to local Ollama. Spoken slash-like
text is never parsed as a terminal command.
"#;

static LOGGING: Once = Once::new();

/// Installs the one terminal logger used by every Independence mode.
///
/// The custom writer uses CRLF and flushes each event. That matters while
/// Crossterm has placed the terminal in raw mode: logs remain readable without
/// relying on `println!`'s normal line-buffered behavior.
pub fn init_logging() {
    LOGGING.call_once(|| {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_target(true)
            .with_level(true)
            .with_writer(TerminalMakeWriter)
            .init();
    });
}

#[derive(Clone, Copy)]
struct TerminalMakeWriter;

impl<'writer> MakeWriter<'writer> for TerminalMakeWriter {
    type Writer = TerminalWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        TerminalWriter
    }
}

/// A raw-mode-aware `tracing` writer. `Write::write` must report how much of
/// the caller's buffer it consumed even though LF becomes CRLF on the terminal.
struct TerminalWriter;

impl Write for TerminalWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut stdout = io::stdout().lock();
        let mut start = 0;

        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                stdout.write_all(&bytes[start..index])?;
                if index > 0 && bytes[index - 1] == b'\r' {
                    stdout.write_all(b"\n")?;
                } else {
                    stdout.write_all(b"\r\n")?;
                }
                start = index + 1;
            }
        }

        stdout.write_all(&bytes[start..])?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().lock().flush()
    }
}

/// Records a normal application answer as dialogue. The caller that owns a
/// speech worker may enqueue this same text separately.
pub fn independence_says(text: &str) {
    info!(target: "dialogue", role = "independence", text = ?text, "authorized speech");
}

/// Records recognized microphone text. It is intentional user-visible data,
/// unlike audio samples, which are never logged.
pub fn user_transcript(stage: &'static str, text: &str) {
    info!(target: "dialogue", role = "user", transcript = stage, text = ?text, "recognized speech");
}

/// Records a final voice submission without copying it into a free-form
/// diagnostic string. `photos` is a count only; image bytes are never logged.
pub fn voice_submission(text: &str, photos: usize) {
    info!(target: "dialogue", role = "user", transcript = "submitted", text = ?text, photos, "voice request authorized");
}

/// A normal state transition or timing diagnostic.
pub fn process(event: &'static str, detail: impl std::fmt::Display) {
    info!(target: "process", event, detail = %detail, "state");
}

/// A recoverable condition. The application stays usable, but the terminal
/// makes clear that a requested operation did not happen.
pub fn warning(event: &'static str, detail: impl std::fmt::Display) {
    warn!(target: "process", event, detail = %detail, "warning");
}

/// Lists audio output devices without opening a stream or producing sound.
/// The exact identifiers can be passed to `--audio-output` or
/// `INDEPENDENCE_AUDIO_OUTPUT` for a later voice launch.
pub fn list_audio_outputs() -> Result<(), playback::PlaybackError> {
    let devices = playback::list_output_devices()?;
    if devices.is_empty() {
        warning(
            "audio_output_unavailable",
            "no audio output devices were reported by the local CPAL backend",
        );
        return Ok(());
    }

    process("audio_output_list", format!("{} device(s)", devices.len()));
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

/// Records an application-level error. It remains a terminal log, rather than
/// an unstructured `eprintln!`, even before any microphone mode starts.
pub fn application_error(detail: impl std::fmt::Display) {
    error!(target: "process", event = "application_error", detail = %detail, "error");
}

pub fn usage(detail: &str) {
    error!(target: "process", event = "usage", detail, "error");
}

pub fn welcome() {
    process("ready", "Independence is ready; type /help for commands");
}

/// Emits main-application output as structured terminal logs.
pub fn emit(output: Output) {
    match output {
        Output::Answer(answer) => independence_says(&answer),
        Output::Diagnostic(message) => warning("diagnostic", message),
        Output::Cameras(cameras) => {
            if cameras.is_empty() {
                warning(
                    "camera_unavailable",
                    "no usable V4L2 cameras found; connect one, check permissions, then run /cameras",
                );
            } else {
                process("camera_list", format!("{} usable camera(s)", cameras.len()));
                for camera in cameras {
                    info!(
                        target: "process",
                        event = "camera_available",
                        id = %camera.id(),
                        name = camera.name(),
                        device = %format!("/dev/video{}", camera.id()),
                        "camera"
                    );
                }
            }
        }
        Output::Help => {
            for line in HELP.lines() {
                info!(target: "help", line = %line, "help");
            }
        }
    }
}

/// Compatibility entry point for callers that are in raw terminal mode. The
/// installed writer itself is raw-mode-safe, so logging needs no separate
/// printing path.
pub fn emit_raw(output: Output) {
    emit(output);
}

/// Diagnostic terminal text for button-controlled voice mode.
pub fn voice_log(message: &str) {
    process("voice", message);
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
        "Enter starts/stops recording; Esc discards/cancels; Ctrl+C exits",
    );
    process(
        "privacy",
        "transcripts are displayed only and never submitted to Ollama",
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
        "release Enter to submit; P captures a photo while Enter is held; Esc cancels; Ctrl+C exits",
    );
    process(
        "safety",
        "transcripts are literal question text and cannot execute terminal commands",
    );
}

/// Emits standalone dictation lifecycle events as logs.
pub fn emit_dictation(output: DictationOutput) {
    match output {
        DictationOutput::Ready => process("dictation_ready", "Enter starts recording"),
        DictationOutput::Recording { elapsed } => process(
            "recording",
            format!(
                "elapsed={:02}:{:02}; Enter finishes; Esc discards",
                elapsed.as_secs() / 60,
                elapsed.as_secs() % 60
            ),
        ),
        DictationOutput::Discarding => process("recording_discard", "releasing microphone"),
        DictationOutput::Transcribing => process(
            "transcribing",
            "microphone released; Esc cancels the pending transcript",
        ),
        DictationOutput::Transcript(text) => user_transcript("final", &text),
        DictationOutput::Diagnostic(message) => warning("dictation", message),
    }
}
