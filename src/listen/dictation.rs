//! The sole raw-key terminal owner for standalone dictation mode.
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
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
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

/// Starts the raw-key adapter used only by `cargo run -- --dictate`.
pub fn spawn_dictation_key_reader(
    sender: Sender<DictationEvent>,
) -> io::Result<DictationKeyReader> {
    let raw_mode = RawModeGuard::enable()?;
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let thread = thread::Builder::new()
        .name("independence-dictation-keys".into())
        .spawn(move || read_keys(sender, worker_stop))?;

    Ok(DictationKeyReader {
        stop,
        thread: Some(thread),
        raw_mode: Some(raw_mode),
    })
}

fn read_keys(sender: Sender<DictationEvent>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match event::poll(POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    let Some(input) = classify_key(key) else {
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

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use crate::protocol::DictationInput;

    use super::classify_key;

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
}
