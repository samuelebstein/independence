//! A small, local, SQLite-backed memory store.
//!
//! `independence-memory` intentionally has no model, UI, audio, camera, or
//! application-session dependency.  It persists concise selected memories,
//! their evidence, and correction history; it is not a conversation archive.
//!
//! The caller chooses the directory explicitly. [`MemoryStore::open`] creates
//! `memory.db` there, initializes versioned schema migrations, and verifies
//! SQLite FTS5 before reporting success.

mod database;
mod error;
mod evidence;
mod memory;
mod search;

pub use database::MemoryStore;
pub use error::MemoryError;
pub use evidence::Evidence;
pub use memory::{
    CorrectInput, CorrectedMemory, MemoryId, MemoryInput, MemoryRecord, MemoryStatus, RelatedMemory,
};
