//! Device-independent controls for the button-controlled voice interaction.
//!
//! This module deliberately contains no terminal, evdev, microphone, camera,
//! or controller code.  It names the four physical intentions which both the
//! terminal and a directly attached Linux keyboard can report.

/// One physical control accepted by the hold-to-talk voice interaction.
///
/// These are facts about a button, not commands or text.  In particular, a
/// button source never feeds characters into the terminal command parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceControlEvent {
    TalkPressed,
    TalkReleased,
    PhotoPressed,
    Cancel,
}

/// Events delivered by a concrete voice-control adapter to the application.
///
/// `Interrupt` remains separate because it represents process-level shutdown
/// (for example Ctrl+C), rather than an input-device button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceInputEvent {
    Control(VoiceControlEvent),
    Interrupt,
    /// A source which cannot safely continue (the terminal reader, for
    /// example) has stopped.  The application cancels and exits rather than
    /// leaving a microphone or turn alive.
    SourceFailed {
        source: &'static str,
        message: String,
    },
    /// A recoverable physical device fault. The active interaction must be
    /// cancelled, but a source may later report that it was reacquired.
    SourceLost {
        source: &'static str,
        message: String,
    },
    /// A physical source has been reopened after a loss.  No held state is
    /// carried across that boundary; only a fresh press can start recording.
    SourceRecovered {
        source: &'static str,
    },
}

/// Owns the operating-system resource for one voice-control source.
///
/// The runtime invokes this before dictation and model cleanup, so raw
/// terminal state or an exclusive evdev grab cannot outlive the application.
pub trait VoiceInputSource: Send {
    fn shutdown(&mut self);
}

/// A controllable source used by runtime tests.  It has no terminal or Linux
/// device dependency; tests inject the same events that production adapters
/// would send.
#[cfg(test)]
pub(crate) struct FakeVoiceInputSource {
    sender: tokio::sync::mpsc::Sender<VoiceInputEvent>,
    shutdown: bool,
}

#[cfg(test)]
impl FakeVoiceInputSource {
    pub(crate) fn new(sender: tokio::sync::mpsc::Sender<VoiceInputEvent>) -> Self {
        Self {
            sender,
            shutdown: false,
        }
    }

    pub(crate) async fn emit(&self, event: VoiceInputEvent) {
        self.sender.send(event).await.unwrap();
    }

    pub(crate) fn was_shutdown(&self) -> bool {
        self.shutdown
    }
}

#[cfg(test)]
impl VoiceInputSource for FakeVoiceInputSource {
    fn shutdown(&mut self) {
        self.shutdown = true;
    }
}
