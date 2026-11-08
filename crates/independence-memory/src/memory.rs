use crate::{Evidence, MemoryError};

/// Stable SQLite identifier for one memory record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoryId(i64);

impl MemoryId {
    /// Returns the database identifier for display, tool arguments, or a
    /// caller-owned reference. It is not a secret.
    pub const fn value(self) -> i64 {
        self.0
    }

    pub(crate) const fn from_database(value: i64) -> Self {
        Self(value)
    }
}

impl TryFrom<i64> for MemoryId {
    type Error = MemoryError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value > 0 {
            Ok(Self(value))
        } else {
            Err(MemoryError::InvalidMemoryId(value))
        }
    }
}

/// Whether a memory is the current version or historical context replaced by
/// a later correction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryStatus {
    Current,
    Superseded,
}

impl MemoryStatus {
    pub(crate) const fn as_database(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Superseded => "superseded",
        }
    }

    pub(crate) fn from_database(value: &str) -> Option<Self> {
        match value {
            "current" => Some(Self::Current),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }
}

/// One non-taxonomic relationship retained alongside a memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedMemory {
    pub memory_id: MemoryId,
    pub relationship: String,
}

/// Input used to create either a new memory or the new version in a
/// correction. The library validates sizes and references; it never silently
/// truncates selected information.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryInput {
    pub content: String,
    pub confidence: Option<f64>,
    pub evidence: Vec<Evidence>,
    pub related_memories: Vec<MemoryId>,
}

impl MemoryInput {
    /// Starts a memory with selected content and no evidence. Callers may add
    /// evidence before persisting it.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            confidence: None,
            evidence: Vec::new(),
            related_memories: Vec::new(),
        }
    }
}

/// One durable memory plus bounded provenance returned by the store.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryRecord {
    pub id: MemoryId,
    pub content: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub confidence: Option<f64>,
    pub status: MemoryStatus,
    pub supersedes: Option<MemoryId>,
    pub evidence: Vec<Evidence>,
    pub related_memories: Vec<RelatedMemory>,
}

/// Input for a correction that preserves the prior memory as historical
/// context instead of overwriting it.
#[derive(Clone, Debug, PartialEq)]
pub struct CorrectInput {
    pub memory_id: MemoryId,
    pub replacement: MemoryInput,
}

/// The preserved historical memory and its new current replacement.
#[derive(Clone, Debug, PartialEq)]
pub struct CorrectedMemory {
    pub previous: MemoryRecord,
    pub current: MemoryRecord,
}
