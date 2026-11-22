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

/// Shown when a press is too short to contain a question (a tap, or a sticky
/// button). This is guidance, not a failure.
pub const TOO_SHORT_MESSAGE: &str = "Recording too short. Hold ENTER while you speak.";

static LAST_STATUS: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// Runs `update` with the most recently displayed status line. Tests use a
/// per-thread copy so parallel tests cannot deduplicate each other's lines.
fn with_last_status<R>(update: impl FnOnce(&mut Option<String>) -> R) -> R {
    #[cfg(test)]
    {
        let _ = &LAST_STATUS;
        test_terminal::LAST_STATUS.with(|last| update(&mut last.borrow_mut()))
    }
    #[cfg(not(test))]
    {
        let lock = LAST_STATUS.get_or_init(|| Mutex::new(None));
        let mut last = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        update(&mut last)
    }
}

/// Displays an authorized assistant response. It is intentionally not a log
/// event: answers are private conversation data, not diagnostics.
pub fn answer(text: &str) {
    write_terminal(|writer| write_prefixed_text(writer, "Independence: ", text));
    clear_status();
}

/// Displays one accepted final microphone transcript. Partial transcripts are
/// deliberately not rendered or retained by this interface.
///
/// The current status is intentionally kept: in voice mode "Thinking..." is
/// shown on release, before the transcript, and must not be repeated after it.
pub fn final_transcript(text: &str) {
    write_terminal(|writer| write_prefixed_text(writer, "You: ", text));
}

/// Displays a short, deduplicated status message.
pub fn status(message: &str) {
    with_last_status(|last| {
        if last.as_deref() == Some(message) {
            return;
        }
        write_terminal(|writer| write_text(writer, message));
        *last = Some(message.to_owned());
    });
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

/// Renders controller diagnostics as person-facing messages. Camera and local
/// inference workers provide bounded, request-free failure details here so a
/// person can diagnose the device without exposing prompts or image bytes.
fn diagnostic(message: &str) {
    if message.starts_with("activating camera ") {
        status("Capturing a photo...");
    } else if message.starts_with("captured camera ") {
        status("Photo captured. Thinking...");
    } else if let Some(detail) = message.strip_prefix("could not list cameras: ") {
        warning(&format!("Camera listing failed: {detail}"));
    } else if let Some(detail) = message.strip_prefix("could not prepare camera: ") {
        warning(&format!("Camera preparation failed: {detail}"));
    } else if let Some(detail) = message.strip_prefix("could not capture camera: ") {
        warning(&format!("Camera capture failed: {detail}"));
    } else if message.starts_with("camera/photo request was not started") {
        warning(
            "The selected model cannot interpret photos. Choose a vision-capable model to use the camera.",
        );
    } else if message.starts_with("could not use photo") {
        warning("The photo could not be used. Choose a supported image and try again.");
    } else if let Some(detail) = message.strip_prefix("local inference failed: ") {
        warning(&format!("Local model request failed: {detail}"));
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
        DictationOutput::TooShort => notice(TOO_SHORT_MESSAGE),
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
        "recording" | "microphone_capture_started" => Some("Listening..."),
        "transcribing" => Some("Transcribing..."),
        "mode" => match detail {
            value if value.starts_with("dictation") => Some("Dictation mode is ready."),
            value if value.starts_with("voice") => Some("Voice mode is ready."),
            _ => None,
        },
        "controls" => Some(detail),
        "privacy" => Some("Your transcript is displayed here and is not sent to Ollama."),
        "voice_input_source_recovered" => {
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
        warning(message.trim_start_matches("photo capture request rejected — "));
        return;
    }

    if message.starts_with("photo capture rejected") {
        warning("The photo could not be attached because the temporary photo limit was reached.");
        return;
    }

    if let Some(detail) = message.strip_prefix("photo capture failed — ") {
        warning(&format!("Photo capture failed: {detail}"));
        return;
    }

    match message {
        "ready — hold enter to start recording/transcription" => {
            status("Ready. Hold ENTER to speak.")
        }
        "microphone released; finishing local transcription" => status("Thinking..."),
        // Release is the moment the person stops talking; "Thinking..." is
        // shown once here and not repeated by later processing steps.
        "recording released; waiting for microphone cleanup" => status("Thinking..."),
        "recording released; waiting for direct audio backend" => status("Thinking..."),
        "discarding recording and releasing microphone" => status("Cancelling recording..."),
        "draft cancelled"
        | "direct-audio draft cancelled"
        | "direct-audio recording cancelled; microphone released" => notice("Cancelled."),
        "listening — microphone opening" => status("Listening..."),
        crate::speak::TOO_SHORT_VOICE_LOG
        | "released before the microphone became available; no recording started" => {
            notice(TOO_SHORT_MESSAGE)
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
    with_last_status(|last| *last = None);
}

fn write_terminal(render: impl FnOnce(&mut dyn Write) -> io::Result<()>) {
    #[cfg(test)]
    test_terminal::record(render);
    #[cfg(not(test))]
    {
        let stdout = io::stdout();
        let mut writer = stdout.lock();
        // The terminal may be in Crossterm raw mode. Write CRLF ourselves and
        // flush every message so status text remains readable in that mode.
        let _ = render(&mut writer).and_then(|()| writer.flush());
    }
}

/// A per-thread record of what the interface would have written, so tests can
/// assert that each status line appears once and in order.
#[cfg(test)]
pub(crate) mod test_terminal {
    use std::cell::RefCell;
    use std::io::{self, Write};

    thread_local! {
        pub(super) static LAST_STATUS: RefCell<Option<String>> = const { RefCell::new(None) };
        static LINES: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    }

    /// Starts recording terminal output on this thread, discarding earlier
    /// lines and status state.
    pub(crate) fn start() {
        LAST_STATUS.with(|last| *last.borrow_mut() = None);
        LINES.with(|lines| *lines.borrow_mut() = Some(Vec::new()));
    }

    /// Returns the recorded lines since `start`.
    pub(crate) fn lines() -> Vec<String> {
        LINES.with(|lines| lines.borrow().clone().unwrap_or_default())
    }

    /// Writes into the recording if one is active; otherwise writes to
    /// standard output like production does.
    pub(super) fn record(render: impl FnOnce(&mut dyn Write) -> io::Result<()>) {
        let mut buffer = Vec::new();
        let _ = render(&mut buffer);
        let text = String::from_utf8_lossy(&buffer).into_owned();
        let recorded = LINES.with(|lines| match lines.borrow_mut().as_mut() {
            Some(lines) => {
                lines.extend(
                    text.split("\r\n")
                        .filter(|line| !line.is_empty())
                        .map(str::to_owned),
                );
                true
            }
            None => false,
        });
        if !recorded {
            let stdout = io::stdout();
            let mut writer = stdout.lock();
            let _ = writer.write_all(&buffer).and_then(|()| writer.flush());
        }
    }
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
