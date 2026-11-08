use std::path::PathBuf;

use thiserror::Error;

/// Errors returned by the persistent-memory boundary.
///
/// Error variants intentionally describe a stage or validation category. They
/// never include a memory's content or supporting evidence.
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory data directory must not be empty")]
    EmptyDataDirectory,

    #[error("could not create memory data directory `{path}`: {source}")]
    CreateDataDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("memory data path `{path}` is not a directory")]
    DataPathNotDirectory { path: PathBuf },

    #[error("could not resolve memory data directory `{path}`: {source}")]
    ResolveDataDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not open memory database `{path}`: {source}")]
    OpenDatabase {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("could not configure or migrate the memory database")]
    DatabaseSetup(#[source] rusqlite::Error),

    #[error(
        "memory database schema version {found} is newer than this library supports ({supported})"
    )]
    UnsupportedSchemaVersion { found: i64, supported: i64 },

    #[error("could not read the local clock for memory timestamps")]
    ClockBeforeUnixEpoch,

    #[error("memory content must not be empty")]
    EmptyMemoryContent,

    #[error("memory content exceeds the {maximum} byte limit")]
    MemoryContentTooLarge { maximum: usize },

    #[error("memory evidence exceeds the maximum of {maximum} entries")]
    TooManyEvidenceEntries { maximum: usize },

    #[error("supporting evidence must not be empty")]
    EmptyEvidenceContent,

    #[error("supporting evidence exceeds the {maximum} byte limit")]
    EvidenceContentTooLarge { maximum: usize },

    #[error("evidence source kind must not be empty")]
    EmptyEvidenceSourceKind,

    #[error("evidence source kind exceeds the {maximum} byte limit")]
    EvidenceSourceKindTooLarge { maximum: usize },

    #[error("evidence source reference exceeds the {maximum} byte limit")]
    EvidenceSourceReferenceTooLarge { maximum: usize },

    #[error("confidence must be a finite number from 0.0 through 1.0")]
    InvalidConfidence,

    #[error("a memory may link to at most {maximum} related memories")]
    TooManyRelationships { maximum: usize },

    #[error("memory identifier {0} is invalid")]
    InvalidMemoryId(i64),

    #[error("memory identifier {0} was not found")]
    MemoryNotFound(i64),

    #[error("memory identifier {0} is already superseded and cannot be corrected again")]
    MemoryNotCurrent(i64),

    #[error("recall query must contain at least one searchable term")]
    EmptyRecallQuery,

    #[error("recall limit must be between 1 and {maximum}")]
    InvalidRecallLimit { maximum: usize },

    #[error("memory database operation failed")]
    Database(#[source] rusqlite::Error),
}
