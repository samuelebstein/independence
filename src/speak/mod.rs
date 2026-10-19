//! Terminal output for the prototype's `speak` capability.

use std::{io::Write, path::Path};

use crate::protocol::{DictationOutput, Output};

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

pub fn welcome() {
    eprintln!("Independence is ready. Type /help for commands.");
}

pub fn emit(output: Output) {
    match output {
        Output::Answer(answer) => println!("independence: {answer}"),
        Output::Diagnostic(message) => eprintln!("independence: {message}"),
        Output::Cameras(cameras) => {
            if cameras.is_empty() {
                eprintln!(
                    "independence: no usable V4L2 cameras found; connect one, check permissions, then run /cameras"
                );
            } else {
                println!("independence: usable cameras:");
                for camera in cameras {
                    println!(
                        "  {}  {}  (/dev/video{})",
                        camera.id(),
                        camera.name(),
                        camera.id()
                    );
                }
            }
        }
        Output::Help => println!("{HELP}"),
    }
}

/// Emits main-application output while Crossterm raw mode is active.
///
/// Voice mode owns raw terminal input, so it must not rely on `println!` for
/// controller-approved answers or diagnostics. This uses the same explicit
/// write-and-flush path as the dictation UI.
pub fn emit_raw(output: Output) {
    match output {
        Output::Answer(answer) => raw_prefixed_lines("independence: ", &answer),
        Output::Diagnostic(message) => raw_prefixed_lines("independence: ", &message),
        Output::Cameras(cameras) => {
            if cameras.is_empty() {
                dictation_line(
                    "independence: no usable V4L2 cameras found; connect one, check permissions, then run /cameras",
                );
            } else {
                dictation_line("independence: usable cameras:");
                for camera in cameras {
                    dictation_line(&format!(
                        "  {}  {}  (/dev/video{})",
                        camera.id(),
                        camera.name(),
                        camera.id()
                    ));
                }
            }
        }
        Output::Help => raw_block(HELP),
    }
}

/// Displays the non-sensitive setup information before dictation enters READY.
pub fn dictation_welcome(microphone: Result<&str, &str>, model_path: &Path, model_available: bool) {
    dictation_line("Independence dictation — local transcription only");
    match microphone {
        Ok(microphone) => dictation_line(&format!("Microphone: {microphone}")),
        Err(message) => dictation_line(&format!("Microphone: unavailable ({message})")),
    }
    let availability = if model_available {
        "available"
    } else {
        "missing; recording will not start"
    };
    dictation_line(&format!(
        "Whisper model: {} ({availability})",
        model_path.display()
    ));
    dictation_line("Enter starts/stops recording; Esc discards/cancels; Ctrl+C exits.");
    dictation_line("Transcripts are displayed as text only and are never submitted to Ollama.");
}

/// Displays the setup information for integrated voice questions.
pub fn voice_welcome(microphone: Result<&str, &str>, model_path: &Path, model_available: bool) {
    dictation_line("Independence voice — local transcription and local Ollama");
    match microphone {
        Ok(microphone) => dictation_line(&format!("Microphone: {microphone}")),
        Err(message) => dictation_line(&format!("Microphone: unavailable ({message})")),
    }
    let availability = if model_available {
        "available"
    } else {
        "missing; recording will not start"
    };
    dictation_line(&format!(
        "Whisper model: {} ({availability})",
        model_path.display()
    ));
    dictation_line("Enter starts/stops recording; Esc cancels; Ctrl+C exits.");
    dictation_line(
        "Accepted transcripts are displayed, then sent as literal questions to local Ollama.",
    );
    dictation_line("Spoken text such as /camera or /quit cannot execute a terminal command.");
}

/// Emits raw-mode-safe dictation UI. Ordinary `println!` is not used here:
/// Crossterm raw mode requires explicit writes and flushing for predictable
/// terminal output.
pub fn emit_dictation(output: DictationOutput) {
    match output {
        DictationOutput::Ready => dictation_line("READY — Enter starts recording."),
        DictationOutput::Recording { elapsed } => dictation_line(&format!(
            "RECORDING {:02}:{:02} — Enter finishes; Esc discards.",
            elapsed.as_secs() / 60,
            elapsed.as_secs() % 60
        )),
        DictationOutput::Discarding => {
            dictation_line("Discarding recording and releasing microphone…")
        }
        DictationOutput::Transcribing => {
            dictation_line("TRANSCRIBING — microphone released; Esc cancels the pending text.")
        }
        DictationOutput::Transcript(text) => dictation_line(&format!("Transcript: {text}")),
        DictationOutput::Diagnostic(message) => {
            dictation_line(&format!("independence dictation: {message}"))
        }
    }
}

fn dictation_line(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.write_all(b"\r\n");
    let _ = stdout.flush();
}

fn raw_prefixed_lines(prefix: &str, text: &str) {
    let mut lines = text.lines();
    match lines.next() {
        Some(first) => {
            dictation_line(&format!("{prefix}{first}"));
            for line in lines {
                dictation_line(line);
            }
        }
        None => dictation_line(prefix.trim_end()),
    }
}

fn raw_block(text: &str) {
    for line in text.lines() {
        dictation_line(line);
    }
}
