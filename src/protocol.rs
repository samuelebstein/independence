//! Typed boundaries between terminal input, session decisions, and side effects.

use std::{fmt, path::PathBuf, time::Duration};

use crate::{
    see::{
        Photo,
        camera::{CameraDescriptor, CameraId, CameraPhotoSource},
    },
    think::ConversationTurn,
};

/// A unique identifier for one proposed model turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TurnId(u64);

impl TurnId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    /// A stable process-local value for correlation records.
    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

/// A unique identifier for a camera-command preflight.
///
/// Camera selection happens before a turn exists, so invalid hardware choices
/// cannot supersede unrelated accepted work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CameraRequestId(u64);

impl CameraRequestId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }
}

/// A unique identifier for one in-memory dictation recording.
///
/// It is intentionally separate from `TurnId`: dictation does not start an
/// Ollama turn, and its transcript has no command or tool authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordingId(u64);

impl RecordingId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    /// A stable process-local value for correlation records.
    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

/// A bounded, validated microphone recording: mono f32 PCM at the actual
/// capture sample rate.
///
/// CPAL converts the device's sample representation and downmixes channels at
/// the callback boundary, but it deliberately preserves the negotiated sample
/// rate here.  Whisper's 16 kHz conversion happens later at its own boundary;
/// an audio-to-audio backend can therefore request its own conversion instead
/// of receiving audio that was silently tailored for Whisper.
///
/// The samples remain opaque at this protocol boundary. Debug output reports
/// format and count only, so diagnostics cannot leak a recording's contents.
#[derive(Clone, PartialEq)]
pub struct CapturedAudio {
    samples: Vec<f32>,
    sample_rate_hz: u32,
}

// `CapturedAudio` is constructed only after rejecting non-finite samples, so
// bitwise f32 equality cannot encounter NaN's non-reflexive behaviour here.
impl Eq for CapturedAudio {}

impl CapturedAudio {
    /// Test/default constructor for already-normalized Whisper fixtures.
    #[cfg(test)]
    pub(crate) fn new(samples: Vec<f32>) -> Self {
        Self {
            samples,
            sample_rate_hz: 16_000,
        }
    }

    /// Builds a recording from the mono f32 representation produced by the
    /// CPAL adapter. Callers must retain the actual negotiated sample rate.
    pub(crate) fn with_sample_rate(samples: Vec<f32>, sample_rate_hz: u32) -> Self {
        Self {
            samples,
            sample_rate_hz,
        }
    }

    pub(crate) fn samples(&self) -> &[f32] {
        &self.samples
    }

    pub(crate) fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    pub(crate) fn into_parts(self) -> (Vec<f32>, u32) {
        (self.samples, self.sample_rate_hz)
    }

    #[cfg(test)]
    pub(crate) fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

impl fmt::Debug for CapturedAudio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedAudio")
            .field("sample_count", &self.samples.len())
            .field("sample_rate_hz", &self.sample_rate_hz)
            .finish()
    }
}

/// Raw-key input understood by the microphone modes (`--dictate` and
/// `--voice`), never by the normal line-oriented terminal interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictationInput {
    /// The legacy tap-to-start/tap-to-finish control used by `--dictate`.
    Enter,
    /// A real Enter key press in the hold-to-talk `--voice` mode.
    HoldStart,
    /// A real Enter key release in the hold-to-talk `--voice` mode.
    HoldEnd,
    /// One distinct `P` press while a voice draft is being recorded.
    Photo,
    Escape,
    Interrupt,
    EndOfInput,
}

/// A fact that reached the dictation controller.
///
/// This is deliberately distinct from `Event`: it prevents a transcript from
/// accidentally becoming an `Input::Ask` or slash command for the main app.
#[derive(Debug)]
pub enum DictationEvent {
    Input(DictationInput),
    RecordingStarted {
        recording_id: RecordingId,
    },
    RecordingElapsed {
        recording_id: RecordingId,
        elapsed: Duration,
    },
    /// A bounded snapshot of an ongoing recording, prepared for local
    /// transcription. It never changes the final capture/transcription
    /// lifecycle; voice mode uses it only for display-only partial text.
    RecordingSnapshot {
        recording_id: RecordingId,
        audio: CapturedAudio,
    },
    /// A requested live snapshot had no usable audio yet. This is normal for
    /// an early quiet hold; it lets voice mode request a later snapshot.
    RecordingSnapshotUnavailable {
        recording_id: RecordingId,
    },
    RecordingLimitReached {
        recording_id: RecordingId,
    },
    RecordingFinished {
        recording_id: RecordingId,
        audio: CapturedAudio,
    },
    RecordingCancelled {
        recording_id: RecordingId,
    },
    RecordingFailed {
        recording_id: RecordingId,
        message: String,
    },
    /// The listening/transcription boundary promised by this milestone.
    TranscriptReady {
        recording_id: RecordingId,
        text: String,
    },
    TranscriptionCancelled {
        recording_id: RecordingId,
    },
    TranscriptionFailed {
        recording_id: RecordingId,
        message: String,
    },
}

/// A side effect requested by the dictation controller.
#[derive(Debug)]
pub enum DictationEffect {
    StartRecording {
        recording_id: RecordingId,
    },
    FinishRecording {
        recording_id: RecordingId,
    },
    CancelRecording {
        recording_id: RecordingId,
    },
    StartTranscription {
        recording_id: RecordingId,
        audio: CapturedAudio,
    },
    CancelTranscription {
        recording_id: RecordingId,
    },
    /// A current, nonempty transcript accepted by the dictation controller.
    ///
    /// The runtime chooses whether this remains display-only (`--dictate`) or
    /// becomes literal question text for the integrated voice mode.
    AcceptedTranscript {
        recording_id: RecordingId,
        text: String,
    },
    Display(DictationOutput),
    Exit,
}

/// Terminal output authorized by the dictation controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DictationOutput {
    Ready,
    Recording { elapsed: Duration },
    Discarding,
    Transcribing,
    Transcript(String),
    Diagnostic(String),
}

/// A fact that reached the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Input(Input),
    CamerasListed(Result<Vec<CameraDescriptor>, String>),
    CameraPrepared {
        id: CameraRequestId,
        result: Result<CameraDescriptor, String>,
    },
    CameraCaptureStarted {
        id: TurnId,
        camera: CameraDescriptor,
    },
    CameraCaptureReady {
        id: TurnId,
        source: CameraPhotoSource,
    },
    TurnFinished(TurnFinished),
    AudioTurnFinished(AudioTurnFinished),
}

/// A parsed terminal input or terminal lifecycle signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Ask(String),
    /// A completed microphone recording for the deliberately separate
    /// audio-to-audio experiment. This does not create text conversation
    /// history or give the recording terminal-command authority.
    AskAudio {
        recording_id: RecordingId,
        audio: CapturedAudio,
    },
    /// A literal question accompanied by already validated, in-memory photos.
    ///
    /// This is constructed only by the hold-to-talk voice coordinator. It is
    /// deliberately not part of the line parser, so a transcript cannot gain
    /// terminal-command authority.
    AskWithPhotos {
        question: String,
        photos: Vec<Photo>,
    },
    Look {
        path: PathBuf,
        question: String,
    },
    Cameras,
    Camera {
        requested: Option<CameraId>,
        question: String,
    },
    Stop,
    Reset,
    Help,
    Quit,
    Blank,
    Malformed(String),
    EndOfInput,
    Interrupt,
}

/// A request the controller has authorized a worker to perform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnRequest {
    pub id: TurnId,
    pub question: String,
    pub history: Vec<ConversationTurn>,
    pub visual: VisualRequest,
}

/// A controller-authorized direct-audio request. The existing `TurnId`
/// remains the single authorization generation across text and audio work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTurnRequest {
    pub id: TurnId,
    pub recording_id: RecordingId,
    pub audio: CapturedAudio,
}

/// A direct-audio worker completion. As with text turns, a worker only
/// returns bytes; the controller determines whether they remain current and
/// may reach playback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTurnFinished {
    pub id: TurnId,
    pub result: AudioTurnResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioTurnResult {
    Audio { wav: Vec<u8> },
    Failed { message: String },
    Cancelled,
}

/// Whether a turn continues the accepted scene or stages a proposed replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualRequest {
    /// Send the current accepted image context with this turn, in order.
    UseAccepted(Vec<Photo>),
    ProposeNew(PathBuf),
    ProposeCamera(CameraDescriptor),
}

/// A no-stream validation request for `/camera`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraPreparation {
    pub id: CameraRequestId,
    pub requested: Option<CameraId>,
}

/// A worker result. Workers never print or alter accepted session state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnFinished {
    pub id: TurnId,
    pub result: TurnResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnResult {
    Answer {
        answer: String,
        proposed_scene: Option<Photo>,
    },
    Failed {
        message: String,
    },
    Cancelled,
}

/// A side effect the controller asks the runtime to carry out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    StartTurn(TurnRequest),
    StartAudioTurn(AudioTurnRequest),
    PrepareCamera(CameraPreparation),
    ListCameras,
    CancelTurn(TurnId),
    /// The controller accepted this current direct-audio result. The runtime
    /// sends it to its existing playback adapter; the controller never opens
    /// a device or plays audio itself.
    PlayAudio {
        id: TurnId,
        wav: Vec<u8>,
    },
    Display(Output),
    Exit,
}

/// Terminal output authorized by the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Answer(String),
    Diagnostic(String),
    Cameras(Vec<CameraDescriptor>),
    Help,
}
