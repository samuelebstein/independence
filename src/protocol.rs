//! Typed boundaries between terminal input, session decisions, and side effects.

use std::path::PathBuf;

use crate::{see::Photo, think::ConversationTurn};

/// A unique identifier for one proposed model turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TurnId(u64);

impl TurnId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }
}

/// A fact that reached the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Input(Input),
    TurnFinished(TurnFinished),
}

/// A parsed terminal input or terminal lifecycle signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Ask(String),
    Look { path: PathBuf, question: String },
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

/// Whether a turn continues the accepted scene or stages a proposed replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualRequest {
    UseAccepted(Option<Photo>),
    ProposeNew(PathBuf),
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
    CancelTurn(TurnId),
    Display(Output),
    Exit,
}

/// Terminal output authorized by the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Answer(String),
    Diagnostic(String),
    Help,
}
