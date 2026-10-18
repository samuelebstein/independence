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
}

/// A unique identifier for a structured local-model decision.
///
/// Decisions are separate from answer turns: a decision can be cancelled or
/// become stale before it is allowed to ask for permission or begin a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DecisionId(u64);

impl DecisionId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
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
}

/// Validated audio for the local recognizer: mono f32 PCM at 16 kHz.
///
/// The samples are deliberately opaque at the protocol boundary. Debug output
/// reports only a count, so diagnostics cannot leak a recording's contents.
pub struct CapturedAudio {
    samples: Vec<f32>,
}

impl CapturedAudio {
    pub(crate) fn new(samples: Vec<f32>) -> Self {
        Self { samples }
    }

    pub(crate) fn samples(&self) -> &[f32] {
        &self.samples
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
            .finish()
    }
}

/// Raw-key input understood by the microphone modes (`--dictate` and
/// `--voice`), never by the normal line-oriented terminal interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictationInput {
    Enter,
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
    /// becomes an accepted voice input for the integrated mode. The main
    /// controller—not dictation—then decides its meaning.
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
    /// An accepted Whisper transcript entering the integrated voice workflow.
    ///
    /// It is deliberately not `Input::Ask`: the controller decides whether it
    /// starts a new question or replies to a currently pending permission.
    VoiceTranscript(String),
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
    DecisionFinished {
        id: DecisionId,
        result: DecisionResult,
    },
    TurnFinished(TurnFinished),
}

/// A parsed terminal input or terminal lifecycle signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Ask(String),
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

/// A local-model decision requested by the controller.
///
/// The worker can only return a typed result. It cannot capture a photograph,
/// print a response, or update the accepted conversation itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionRequest {
    /// Decide whether to answer the current spoken question or ask to take a
    /// new photograph. The accepted scene, if any, is available only as input
    /// to the model; it remains owned by the controller.
    ChoosePhoto {
        id: DecisionId,
        question: String,
        history: Vec<ConversationTurn>,
        scene: Option<Photo>,
    },
    /// Interpret a reply to one exact, still-current permission request.
    InterpretPermission {
        id: DecisionId,
        proposed_action: String,
        selected_camera: CameraDescriptor,
        permission_question: String,
        reply: String,
    },
}

impl DecisionRequest {
    pub(crate) fn id(&self) -> DecisionId {
        match self {
            Self::ChoosePhoto { id, .. } | Self::InterpretPermission { id, .. } => *id,
        }
    }
}

/// A validated structured answer from a photo-use decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhotoDecision {
    Answer {
        answer: String,
    },
    /// A request for one new still image, with the concrete visible detail
    /// that is missing from the available context.
    ///
    /// This is deliberately an operational, user-facing purpose—not hidden
    /// model reasoning. When there is already an accepted scene, it means a
    /// different or closer view, never a silent replacement of that scene.
    RequestPhoto {
        needed_visual_evidence: String,
    },
}

/// A validated structured interpretation of one permission reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Yes,
    No,
    Unclear,
}

/// A decision worker result. Like `TurnResult`, it has no direct output or
/// session-state authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionResult {
    Photo(PhotoDecision),
    Permission(PermissionDecision),
    Failed { message: String },
    Cancelled,
}

/// Whether a turn continues the accepted scene or stages a proposed replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualRequest {
    UseAccepted(Option<Photo>),
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
    StartDecision(DecisionRequest),
    PrepareCamera(CameraPreparation),
    ListCameras,
    CancelTurn(TurnId),
    CancelDecision(DecisionId),
    Display(Output),
    Exit,
}

/// Terminal output authorized by the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Answer(String),
    Diagnostic(String),
    PermissionQuestion(String),
    PermissionClarification(String),
    Cameras(Vec<CameraDescriptor>),
    Help,
}
