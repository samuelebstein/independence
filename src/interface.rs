//! The human-facing terminal interface.
//!
//! This module is deliberately separate from `tracing`: it is the only
//! application path that renders conversation text, microphone transcripts,
//! instructions, and actionable messages for the person using Independence.
//! Diagnostic logging must never call these helpers or receive their text.

use std::{
    io::{self, Write},
    sync::{Mutex, OnceLock},
};

use crate::protocol::{DictationOutput, Output};

pub const HELP: &str = r#"Independence — Ephemeral Mode

Type a question to ask the local model.
/cameras                    list usable local cameras
/camera | <question>        capture from the only listed camera and ask
/camera <id> | <question>   capture from one explicitly selected camera
/stop                       cancel the current request
/reset                      clear this temporary conversation
/help                       show this help
/quit                       exit

Ephemeral Mode keeps this conversation and any captured photos only in memory
until Independence exits or you use /reset. Terminal scrollback may retain
what is displayed here.
"#;

static LAST_STATUS: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// Displays an authorized assistant response. It is intentionally not a log
/// event: answers are private conversation data, not diagnostics.
pub fn answer(text: &str) {
    write_terminal(|writer| write_prefixed_text(writer, "Independence: ", text));
    clear_status();
}

/// Displays one accepted final microphone transcript. Partial transcripts are
/// deliberately not rendered or retained by this interface.
pub fn final_transcript(text: &str) {
    write_terminal(|writer| write_prefixed_text(writer, "You: ", text));
    clear_status();
}

/// Displays a short, deduplicated status message.
pub fn status(message: &str) {
    let lock = LAST_STATUS.get_or_init(|| Mutex::new(None));
    let mut last = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if last.as_deref() == Some(message) {
        return;
    }
    write_terminal(|writer| write_text(writer, message));
    *last = Some(message.to_owned());
}

/// Displays an informational user-facing message that should not be deduped.
pub fn notice(message: &str) {
    write_terminal(|writer| write_text(writer, message));
    clear_status();
}

/// Displays an actionable warning for the person using Independence.
pub fn warning(message: &str) {
    write_terminal(|writer| write_prefixed_text(writer, "Warning: ", message));
    clear_status();
}

/// Displays an actionable operational warning without exposing an arbitrary
/// backend or device error string. The caller's detail remains neither a
/// terminal diagnostic nor a tracing field.
pub fn operational_warning(event: &str) {
    warning(operational_warning_message(event));
}

fn operational_warning_message(event: &str) -> &'static str {
    match event {
        "microphone_unavailable"
        | "audio_input_unavailable"
        | "audio_to_audio_microphone_unavailable" => {
            "No microphone is available. Connect or select one, then try again."
        }
        "audio_output_unavailable"
        | "audio_to_audio_playback_unavailable"
        | "audio_to_audio_playback_failed" => {
            "Audio playback is unavailable. Check the selected speaker, then try again."
        }
        "input_device_unavailable"
        | "input_device_inaccessible"
        | "ephemeral_entry_input_lost"
        | "voice_input_source_failed"
        | "voice_input_source_lost"
        | "audio_to_audio_input_failed"
        | "audio_to_audio_input_lost" => {
            "The selected button input is unavailable. Check its connection and permissions, then try again."
        }
        "ollama_model_capabilities_unknown" => {
            "The model's capabilities could not be checked. Text chat is available, but camera and direct voice features are unavailable."
        }
        "photo_capture_unavailable" => {
            "The selected model cannot interpret photos. Choose a vision-capable model to use the camera."
        }
        "audio_to_audio_photo_ignored" => "Camera capture is not available in direct voice mode.",
        "dictation" | "partial_transcription_failed" => {
            "Voice recognition could not be completed. Please try again."
        }
        "audio_to_audio_capture_failed" | "audio_to_audio_recording_failed" => {
            "Voice capture could not be completed. Please try again."
        }
        "audio_to_audio_recording_limit" => {
            "The recording reached the 30-second limit and is being finished."
        }
        "listening_cue_unavailable" | "listening_cue_failed" => {
            "The listening sound could not play. Recording will still start."
        }
        _ => "Independence could not complete that step. Please try again.",
    }
}

/// Displays an actionable error for the person using Independence.
pub fn error(message: &str) {
    write_terminal(|writer| write_prefixed_text(writer, "Error: ", message));
    clear_status();
}

/// Renders one controller-authorized terminal output.
pub fn emit(output: Output) {
    match output {
        Output::Answer(answer) => self::answer(&answer),
        Output::Diagnostic(message) => diagnostic(&message),
        Output::Cameras(cameras) => {
            if cameras.is_empty() {
                warning(
                    "No usable cameras were found. Connect one, check permissions, then run /cameras again.",
                );
                return;
            }

            write_terminal(|writer| {
                write_text(writer, "Available cameras:")?;
                for camera in cameras {
                    write_text(
                        writer,
                        &format!(
                            "- {}: {} ({}, {})",
                            camera.id(),
                            camera.name(),
                            camera.backend(),
                            camera.device_path()
                        ),
                    )?;
                }
                Ok(())
            });
            clear_status();
        }
        Output::Help => notice(HELP),
    }
}

/// Renders controller diagnostics as person-facing messages. The controller
/// owns which condition occurred; this boundary prevents backend error text
/// and implementation names from becoming the terminal interface.
fn diagnostic(message: &str) {
    if message.starts_with("activating camera ") {
        status("Capturing a photo...");
    } else if message.starts_with("captured camera ") {
        status("Photo captured. Thinking...");
    } else if message.starts_with("could not list cameras")
        || message.starts_with("could not prepare camera")
        || message.starts_with("could not capture camera")
    {
        warning("The camera is unavailable. Check its connection and permissions, then try again.");
    } else if message.starts_with("camera/photo request was not started") {
        warning(
            "The selected model cannot interpret photos. Choose a vision-capable model to use the camera.",
        );
    } else if message.starts_with("could not use photo") {
        warning("The photo could not be used. Choose a supported image and try again.");
    } else if message.starts_with("local inference failed") {
        warning(
            "The local model could not complete that request. Check that it is running, then try again.",
        );
    } else if message.starts_with("direct audio request failed") {
        warning("The voice request could not be completed. Please try again.");
    } else if message.starts_with("worker returned")
        || message.starts_with("photo loading task ended unexpectedly")
    {
        warning("Independence could not complete that request. Please try again.");
    } else {
        warning(message);
    }
}

/// Renders standalone dictation state without treating transcripts as
/// diagnostic data.
pub fn emit_dictation(output: DictationOutput) {
    match output {
        DictationOutput::Ready => status("Ready. Press ENTER to start recording."),
        DictationOutput::Recording { .. } => status("Listening..."),
        DictationOutput::Discarding => status("Cancelling recording..."),
        DictationOutput::Transcribing => status("Transcribing..."),
        DictationOutput::Transcript(text) => final_transcript(&text),
        DictationOutput::Diagnostic(_) => operational_warning("dictation"),
    }
}

/// Maps existing process state names to concise terminal interface language.
/// Unknown operational state is intentionally diagnostic-only.
pub fn process_status(event: &str, detail: &str) {
    let message = match event {
        "ready" => Some("Independence is ready. Type /help for commands."),
        "operating_mode" => Some("Ephemeral Mode is active. Your conversation is temporary."),
        "ephemeral_session_cleared" => Some("Temporary conversation cleared."),
        "voice_prompt" => Some("Hold ENTER to speak."),
        "dictation_ready" => Some("Ready. Press ENTER to start recording."),
        "recording" | "microphone_capture_started" | "listening_cue_completed" => {
            Some("Listening...")
        }
        "transcribing" => Some("Transcribing..."),
        "mode" => match detail {
            value if value.starts_with("dictation") => Some("Dictation mode is ready."),
            value if value.starts_with("voice") => Some("Voice mode is ready."),
            _ => None,
        },
        "controls" => Some(detail),
        "privacy" => Some("Your transcript is displayed here and is not sent to Ollama."),
        "ephemeral_entry_waiting" => Some(
            "Hold the Talk/ENTER button for five seconds, then release it to enter Ephemeral Mode.",
        ),
        "ephemeral_entry_not_confirmed" => {
            Some("Hold the Talk/ENTER button for five full seconds to enter Ephemeral Mode.")
        }
        "ephemeral_entry_cancelled" => Some("Ephemeral Mode entry was cancelled."),
        "ephemeral_entry_confirmed" => Some("Ephemeral Mode is active. Hold ENTER to speak."),
        "ephemeral_entry_input_recovered" | "voice_input_source_recovered" => {
            Some("The button input is available again. Hold ENTER to speak.")
        }
        "speech_test_completed" => Some("Speech test completed."),
        _ => None,
    };

    if let Some(message) = message {
        status(message);
    }
}

/// Maps the fixed voice lifecycle messages into normal terminal language.
pub fn voice_status(message: &str) {
    if let Some(count) = message
        .strip_prefix("photo captured — ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|count| count.parse::<usize>().ok())
    {
        let label = if count == 1 { "photo" } else { "photos" };
        status(&format!("Photo captured. {count} {label} attached."));
        return;
    }

    if message.starts_with("photo capture request rejected") {
        warning("Too many photo captures are already waiting. Please wait for them to finish.");
        return;
    }

    if message.starts_with("photo capture rejected") {
        warning("The photo could not be attached because the temporary photo limit was reached.");
        return;
    }

    if message.starts_with("photo capture failed") {
        warning("The photo could not be captured. Check the camera and try again.");
        return;
    }

    match message {
        "ready — hold enter to start recording/transcription" => {
            status("Ready. Hold ENTER to speak.")
        }
        "microphone released; finishing local transcription" => status("Thinking..."),
        "recording released; waiting for microphone cleanup" => status("Finishing recording..."),
        "recording released; waiting for direct audio backend" => status("Thinking..."),
        "discarding recording and releasing microphone" => status("Cancelling recording..."),
        "draft cancelled"
        | "direct-audio draft cancelled"
        | "direct-audio recording cancelled; microphone released" => notice("Cancelled."),
        "released before the microphone became available; no recording started" => {
            notice("Recording was not started.")
        }
        "Enter released during listening cue; no microphone capture started" => {
            notice("Recording was not started.")
        }
        value if value.starts_with("audio-to-audio ready") => {
            status("Direct voice mode is ready. Hold ENTER to speak.")
        }
        // Callers can add a new internal state without accidentally exposing
        // it as terminal implementation detail. Add a deliberate mapping
        // above when the state needs a person-facing explanation.
        _ => status("Working..."),
    }
}

/// Announces a fixed spoken cue without echoing it as an assistant answer.
pub fn voice_cue(text: &str) {
    match text {
        "i'm listening" => status("Listening..."),
        "i'm thinking" => status("Thinking..."),
        _ => status("Working..."),
    }
}

pub fn audio_outputs(devices: &[(String, String, bool)]) {
    write_terminal(|writer| {
        write_text(writer, "Audio output devices:")?;
        for (id, name, is_default) in devices {
            let marker = if *is_default { " (default)" } else { "" };
            write_text(writer, &format!("- {name}{marker}\n  ID: {id}"))?;
        }
        Ok(())
    });
    clear_status();
}

pub fn audio_inputs(devices: &[(String, String, bool)]) {
    write_terminal(|writer| {
        write_text(writer, "Audio input devices:")?;
        for (id, name, is_default) in devices {
            let marker = if *is_default { " (default)" } else { "" };
            write_text(writer, &format!("- {name}{marker}\n  ID: {id}"))?;
        }
        Ok(())
    });
    clear_status();
}

pub fn input_devices(lines: &[String]) {
    write_terminal(|writer| {
        write_text(writer, "Linux input devices:")?;
        for line in lines {
            write_text(writer, line)?;
        }
        Ok(())
    });
    clear_status();
}

fn clear_status() {
    let lock = LAST_STATUS.get_or_init(|| Mutex::new(None));
    *lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

fn write_terminal(render: impl FnOnce(&mut dyn Write) -> io::Result<()>) {
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    // The terminal may be in Crossterm raw mode. Write CRLF ourselves and
    // flush every message so status text remains readable in that mode.
    let _ = render(&mut writer).and_then(|()| writer.flush());
}

fn write_text(writer: &mut dyn Write, text: &str) -> io::Result<()> {
    for line in text.split('\n') {
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\r\n")?;
    }
    Ok(())
}

fn write_prefixed_text(writer: &mut dyn Write, prefix: &str, text: &str) -> io::Result<()> {
    let mut lines = text.split('\n');
    if let Some(first) = lines.next() {
        write_text(writer, &format!("{prefix}{first}"))?;
    }
    for line in lines {
        write_text(writer, line)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_answer_to(writer: &mut dyn Write, text: &str) -> io::Result<()> {
    write_prefixed_text(writer, "Independence: ", text)
}

#[cfg(test)]
pub(crate) fn write_transcript_to(writer: &mut dyn Write, text: &str) -> io::Result<()> {
    write_prefixed_text(writer, "You: ", text)
}

#[cfg(test)]
mod tests {
    use super::{operational_warning_message, write_answer_to, write_transcript_to};

    #[test]
    fn conversation_text_renders_only_as_terminal_interface_text() {
        let mut output = Vec::new();
        write_transcript_to(&mut output, "USER_SENTINEL_8d1e").expect("transcript writes");
        write_answer_to(&mut output, "ASSISTANT_SENTINEL_4a72").expect("answer writes");
        let output = String::from_utf8(output).expect("terminal text is UTF-8");

        assert!(output.contains("You: USER_SENTINEL_8d1e"));
        assert!(output.contains("Independence: ASSISTANT_SENTINEL_4a72"));
    }

    #[test]
    fn multiline_answers_keep_the_prefix_only_on_the_first_line() {
        let mut output = Vec::new();
        write_answer_to(&mut output, "first\nsecond").expect("answer writes");
        assert_eq!(
            String::from_utf8(output).expect("terminal text is UTF-8"),
            "Independence: first\r\nsecond\r\n"
        );
    }

    #[test]
    fn operational_warnings_hide_backend_details_but_remain_actionable() {
        assert_eq!(
            operational_warning_message("dictation"),
            "Voice recognition could not be completed. Please try again."
        );
        assert_eq!(
            operational_warning_message("input_device_inaccessible"),
            "The selected button input is unavailable. Check its connection and permissions, then try again."
        );
        assert_eq!(
            operational_warning_message("unknown"),
            "Independence could not complete that step. Please try again."
        );
    }
}
