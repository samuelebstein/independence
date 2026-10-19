//! The sole raw-key terminal owner for the microphone modes.
//!
//! It is intentionally not started by the normal line-oriented application.
//! Crossterm raw mode turns Ctrl+C into an ordinary key event, so this adapter
//! maps it into a typed interrupt and restores the terminal when it stops.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossterm::{
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement},
};
use tokio::sync::mpsc::Sender;

use crate::protocol::{DictationEvent, DictationInput};

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Owns both the input thread and the process-global raw-mode setting.
pub struct DictationKeyReader {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    raw_mode: Option<RawModeGuard>,
}

impl DictationKeyReader {
    /// Stops polling, joins the key reader, and restores normal terminal mode.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Drop after the reader is gone, so no code calls Crossterm's event
        // reader after normal line editing has been restored.
        self.raw_mode.take();
    }
}

impl Drop for DictationKeyReader {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts the raw-key adapter used by `cargo run -- --dictate` and
/// `cargo run -- --voice`.
pub fn spawn_dictation_key_reader(
    sender: Sender<DictationEvent>,
) -> io::Result<DictationKeyReader> {
    let raw_mode = RawModeGuard::enable_legacy()?;
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let thread = thread::Builder::new()
        .name("independence-dictation-keys".into())
        .spawn(move || read_keys(sender, worker_stop, classify_key))?;

    Ok(DictationKeyReader {
        stop,
        thread: Some(thread),
        raw_mode: Some(raw_mode),
    })
}

/// Starts the real press/release reader used only by hold-to-talk voice mode.
///
/// This deliberately refuses terminals that cannot report keyboard event
/// types. A timeout cannot distinguish a still-held Enter key from a release.
pub fn spawn_voice_key_reader(sender: Sender<DictationEvent>) -> io::Result<DictationKeyReader> {
    let raw_mode = RawModeGuard::enable_hold_to_talk()?;
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let thread = thread::Builder::new()
        .name("independence-voice-keys".into())
        .spawn(move || read_keys(sender, worker_stop, classify_voice_key))?;

    Ok(DictationKeyReader {
        stop,
        thread: Some(thread),
        raw_mode: Some(raw_mode),
    })
}

fn read_keys(
    sender: Sender<DictationEvent>,
    stop: Arc<AtomicBool>,
    classify: fn(KeyEvent) -> Option<DictationInput>,
) {
    while !stop.load(Ordering::Acquire) {
        match event::poll(POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    let Some(input) = classify(key) else {
                        continue;
                    };
                    let interrupt = input == DictationInput::Interrupt;
                    if sender.blocking_send(DictationEvent::Input(input)).is_err() || interrupt {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = sender.blocking_send(DictationEvent::Input(DictationInput::EndOfInput));
                    break;
                }
            },
            Ok(false) => {}
            Err(_) => {
                let _ = sender.blocking_send(DictationEvent::Input(DictationInput::EndOfInput));
                break;
            }
        }
    }
}

/// Maps a press event into dictation controls. Key repeat and release events
/// are purposefully ignored, so holding Enter cannot queue recordings.
pub(crate) fn classify_key(key: KeyEvent) -> Option<DictationInput> {
    if key.kind != KeyEventKind::Press {
        return None;
    }

    match key.code {
        KeyCode::Enter => Some(DictationInput::Enter),
        KeyCode::Esc => Some(DictationInput::Escape),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(DictationInput::Interrupt)
        }
        _ => None,
    }
}

/// Maps Kitty/CSI-u key event types into the button-controlled voice inputs.
/// Repeat is intentionally ignored for Enter and P, so a held key does not
/// start another draft or queue accidental photos.
pub(crate) fn classify_voice_key(key: KeyEvent) -> Option<DictationInput> {
    match (key.code, key.kind) {
        (KeyCode::Enter, KeyEventKind::Press) => Some(DictationInput::HoldStart),
        (KeyCode::Enter, KeyEventKind::Release) => Some(DictationInput::HoldEnd),
        (KeyCode::Char('p' | 'P'), KeyEventKind::Press) => Some(DictationInput::Photo),
        (KeyCode::Esc, KeyEventKind::Press) => Some(DictationInput::Escape),
        (KeyCode::Char('c'), KeyEventKind::Press)
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(DictationInput::Interrupt)
        }
        _ => None,
    }
}

struct RawModeGuard {
    keyboard_enhancements: bool,
}

impl RawModeGuard {
    fn enable_legacy() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self {
            keyboard_enhancements: false,
        })
    }

    fn enable_hold_to_talk() -> io::Result<Self> {
        if !supports_keyboard_enhancement()? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this terminal cannot report key release events; --voice requires Kitty keyboard event reporting",
            ));
        }

        enable_raw_mode()?;
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        if let Err(error) = execute!(std::io::stdout(), PushKeyboardEnhancementFlags(flags)) {
            let _ = disable_raw_mode();
            return Err(error);
        }

        Ok(Self {
            keyboard_enhancements: true,
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.keyboard_enhancements {
            let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use crate::protocol::DictationInput;

    use super::{classify_key, classify_voice_key};

    #[test]
    fn only_press_events_control_dictation() {
        assert_eq!(
            classify_key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )),
            Some(DictationInput::Enter)
        );
        assert_eq!(
            classify_key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            None
        );
        assert_eq!(
            classify_key(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            None
        );
    }

    #[test]
    fn control_c_is_an_interrupt() {
        assert_eq!(
            classify_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(DictationInput::Interrupt)
        );
    }

    #[test]
    fn hold_voice_uses_real_enter_release_and_distinct_p_presses() {
        assert_eq!(
            classify_voice_key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )),
            Some(DictationInput::HoldStart)
        );
        assert_eq!(
            classify_voice_key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            Some(DictationInput::HoldEnd)
        );
        assert_eq!(
            classify_voice_key(KeyEvent::new_with_kind(
                KeyCode::Char('p'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )),
            Some(DictationInput::Photo)
        );
        assert_eq!(
            classify_voice_key(KeyEvent::new_with_kind(
                KeyCode::Char('p'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            None
        );
    }
}
