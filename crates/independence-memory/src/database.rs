use std::{
    fs,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tracing::{info, info_span};

use crate::{
    CorrectInput, CorrectedMemory, Evidence, MemoryError, MemoryId, MemoryInput, MemoryRecord,
    MemoryStatus, RelatedMemory,
};

const SCHEMA_VERSION: i64 = 2;
const DATABASE_FILE_NAME: &str = "memory.db";
pub(crate) const MAX_MEMORY_CONTENT_BYTES: usize = 16 * 1024;
pub(crate) const MAX_EVIDENCE_ENTRIES: usize = 8;
pub(crate) const MAX_EVIDENCE_CONTENT_BYTES: usize = 8 * 1024;
pub(crate) const MAX_EVIDENCE_SOURCE_KIND_BYTES: usize = 128;
pub(crate) const MAX_EVIDENCE_SOURCE_REFERENCE_BYTES: usize = 512;
pub(crate) const MAX_RELATIONSHIPS: usize = 32;

/// A local SQLite memory database chosen explicitly by its caller.
///
/// The store owns one SQLite connection. Create it at application startup or
/// in another Rust program, then keep it behind that program's own
/// synchronization boundary if multiple tasks need it.
pub struct MemoryStore {
    connection: Connection,
    data_directory: PathBuf,
    database_path: PathBuf,
}

impl MemoryStore {
    /// Opens or creates `memory.db` inside an explicit local directory.
    ///
    /// This crate never chooses a home-directory, temporary-directory, or
    /// network location on a caller's behalf.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let opened_at = Instant::now();
        let requested_directory = data_directory.as_ref();
        if requested_directory.as_os_str().is_empty() {
            return Err(MemoryError::EmptyDataDirectory);
        }

        let span = info_span!(
            "independence_memory.database.open",
            data_directory = %requested_directory.display(),
        );
        let _entered = span.enter();
        fs::create_dir_all(requested_directory).map_err(|source| {
            MemoryError::CreateDataDirectory {
                path: requested_directory.to_path_buf(),
                source,
            }
        })?;
        let metadata = fs::metadata(requested_directory).map_err(|source| {
            MemoryError::ResolveDataDirectory {
                path: requested_directory.to_path_buf(),
                source,
            }
        })?;
        if !metadata.is_dir() {
            return Err(MemoryError::DataPathNotDirectory {
                path: requested_directory.to_path_buf(),
            });
        }
        let data_directory = fs::canonicalize(requested_directory).map_err(|source| {
            MemoryError::ResolveDataDirectory {
                path: requested_directory.to_path_buf(),
                source,
            }
        })?;
        let database_path = data_directory.join(DATABASE_FILE_NAME);
        let connection =
            Connection::open(&database_path).map_err(|source| MemoryError::OpenDatabase {
                path: database_path.clone(),
                source,
            })?;
        let mut store = Self {
            connection,
            data_directory,
            database_path,
        };
        store.configure_and_migrate()?;
        info!(
            target: "independence_memory",
            event = "database_opened",
            duration_ms = opened_at.elapsed().as_millis() as u64,
            "memory database opened"
        );
        Ok(store)
    }

    /// The explicitly configured directory which owns this database and any
    /// SQLite journal files.
    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    /// The ordinary local SQLite database file.
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Persists one selected memory and its optional evidence atomically.
    pub fn remember(&mut self, input: MemoryInput) -> Result<MemoryRecord, MemoryError> {
        validate_memory_input(&input)?;
        let span = info_span!(
            "independence_memory.remember",
            evidence_count = input.evidence.len(),
            relationship_count = input.related_memories.len(),
        );
        let _entered = span.enter();
        let operation_started = Instant::now();
        let now = timestamp_ms()?;
        let transaction_started = Instant::now();
        let transaction = self
            .connection
            .transaction()
            .map_err(MemoryError::Database)?;
        // Exact current-content deduplication prevents a model retry from
        // creating an unbounded pile of identical memories. This intentionally
        // does not claim semantic deduplication: related-but-differently-worded
        // information still needs model judgment and, when appropriate, the
        // explicit correction operation.
        let existing_id = transaction
            .query_row(
                "SELECT id FROM memories WHERE status = ?1 AND content = ?2 LIMIT 1",
                params![MemoryStatus::Current.as_database(), input.content],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(MemoryError::Database)?;
        if let Some(existing_id) = existing_id {
            let existing_id = MemoryId::from_database(existing_id);
            let existing = load_memory(&transaction, existing_id)?
                .ok_or(MemoryError::MemoryNotFound(existing_id.value()))?;
            transaction.commit().map_err(MemoryError::Database)?;
            info!(
                target: "independence_memory",
                event = "memory_deduplicated",
                memory_id = existing.id.value(),
                duration_ms = operation_started.elapsed().as_millis() as u64,
                "identical current memory already persisted"
            );
            return Ok(existing);
        }
        let record = insert_memory(&transaction, &input, now, None)?;
        transaction.commit().map_err(MemoryError::Database)?;
        info!(
            target: "independence_memory",
            event = "memory_remembered",
            memory_id = record.id.value(),
            transaction_duration_ms = transaction_started.elapsed().as_millis() as u64,
            duration_ms = operation_started.elapsed().as_millis() as u64,
            "memory persisted"
        );
        Ok(record)
    }

    /// Creates a new current memory while preserving the old current memory as
    /// a superseded historical record.
    pub fn correct(&mut self, input: CorrectInput) -> Result<CorrectedMemory, MemoryError> {
        validate_memory_input(&input.replacement)?;
        let span = info_span!(
            "independence_memory.correct",
            memory_id = input.memory_id.value(),
            evidence_count = input.replacement.evidence.len(),
        );
        let _entered = span.enter();
        let operation_started = Instant::now();
        let now = timestamp_ms()?;
        let transaction_started = Instant::now();
        let transaction = self
            .connection
            .transaction()
            .map_err(MemoryError::Database)?;

        let previous = load_memory(&transaction, input.memory_id)?
            .ok_or(MemoryError::MemoryNotFound(input.memory_id.value()))?;
        if previous.status != MemoryStatus::Current {
            return Err(MemoryError::MemoryNotCurrent(input.memory_id.value()));
        }

        transaction
            .execute(
                "UPDATE memories SET status = ?1, updated_at_ms = ?2 WHERE id = ?3",
                params![
                    MemoryStatus::Superseded.as_database(),
                    now,
                    input.memory_id.value()
                ],
            )
            .map_err(MemoryError::Database)?;
        let created = insert_memory(&transaction, &input.replacement, now, Some(input.memory_id))?;
        transaction
            .execute(
                "INSERT INTO memory_relationships (from_memory_id, to_memory_id, relationship, created_at_ms)
                 VALUES (?1, ?2, 'corrects', ?3)",
                params![created.id.value(), input.memory_id.value(), now],
            )
            .map_err(MemoryError::Database)?;
        let current = load_memory(&transaction, created.id)?
            .ok_or(MemoryError::MemoryNotFound(created.id.value()))?;
        let previous = load_memory(&transaction, input.memory_id)?
            .ok_or(MemoryError::MemoryNotFound(input.memory_id.value()))?;
        transaction.commit().map_err(MemoryError::Database)?;
        info!(
            target: "independence_memory",
            event = "memory_corrected",
            previous_memory_id = input.memory_id.value(),
            current_memory_id = current.id.value(),
            transaction_duration_ms = transaction_started.elapsed().as_millis() as u64,
            duration_ms = operation_started.elapsed().as_millis() as u64,
            "memory correction persisted"
        );
        Ok(CorrectedMemory { previous, current })
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    fn configure_and_migrate(&mut self) -> Result<(), MemoryError> {
        let span = info_span!("independence_memory.database.migrate");
        let _entered = span.enter();
        let started = Instant::now();
        self.connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 CREATE TABLE IF NOT EXISTS schema_migrations (
                    version INTEGER PRIMARY KEY
                 );",
            )
            .map_err(MemoryError::DatabaseSetup)?;
        let current: i64 = self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(MemoryError::DatabaseSetup)?;
        if current > SCHEMA_VERSION {
            return Err(MemoryError::UnsupportedSchemaVersion {
                found: current,
                supported: SCHEMA_VERSION,
            });
        }
        if current < 1 {
            let transaction = self
                .connection
                .transaction()
                .map_err(MemoryError::DatabaseSetup)?;
            transaction
                .execute_batch(
                    "CREATE TABLE memories (
                        id INTEGER PRIMARY KEY,
                        content TEXT NOT NULL,
                        created_at_ms INTEGER NOT NULL,
                        updated_at_ms INTEGER NOT NULL,
                        confidence REAL,
                        status TEXT NOT NULL CHECK (status IN ('current', 'superseded')),
                        supersedes_memory_id INTEGER REFERENCES memories(id)
                    );
                    CREATE INDEX memories_status_created_idx
                        ON memories(status, created_at_ms DESC);

                    CREATE TABLE evidence (
                        id INTEGER PRIMARY KEY,
                        memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                        content TEXT NOT NULL,
                        source_kind TEXT NOT NULL,
                        source_timestamp_ms INTEGER,
                        source_reference TEXT,
                        created_at_ms INTEGER NOT NULL
                    );
                    CREATE INDEX evidence_memory_idx ON evidence(memory_id, id);

                    CREATE TABLE memory_relationships (
                        id INTEGER PRIMARY KEY,
                        from_memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                        to_memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                        relationship TEXT NOT NULL,
                        created_at_ms INTEGER NOT NULL,
                        UNIQUE(from_memory_id, to_memory_id, relationship)
                    );
                    CREATE INDEX memory_relationships_from_idx
                        ON memory_relationships(from_memory_id, id);

                    CREATE VIRTUAL TABLE memory_fts USING fts5(content);
                    CREATE TRIGGER memories_fts_after_insert
                    AFTER INSERT ON memories BEGIN
                        INSERT INTO memory_fts(rowid, content) VALUES (new.id, new.content);
                    END;
                    CREATE TRIGGER memories_fts_after_delete
                    AFTER DELETE ON memories BEGIN
                        INSERT INTO memory_fts(memory_fts, rowid, content)
                        VALUES ('delete', old.id, old.content);
                    END;
                    CREATE TRIGGER memories_fts_after_update_content
                    AFTER UPDATE OF content ON memories BEGIN
                        INSERT INTO memory_fts(memory_fts, rowid, content)
                        VALUES ('delete', old.id, old.content);
                        INSERT INTO memory_fts(rowid, content) VALUES (new.id, new.content);
                    END;",
                )
                .map_err(MemoryError::DatabaseSetup)?;
            transaction
                .execute("INSERT INTO schema_migrations (version) VALUES (1)", [])
                .map_err(MemoryError::DatabaseSetup)?;
            transaction.commit().map_err(MemoryError::DatabaseSetup)?;
        }
        // `remember` performs an exact current-content lookup before inserting
        // so a model retry cannot create a pile of duplicates. Keep that
        // bounded as a database grows instead of turning a durable operation
        // into a full-table scan. This is intentionally a non-unique index:
        // an older database might already contain duplicates and migration
        // must preserve records rather than reject or rewrite them.
        if current < 2 {
            let transaction = self
                .connection
                .transaction()
                .map_err(MemoryError::DatabaseSetup)?;
            transaction
                .execute(
                    "CREATE INDEX IF NOT EXISTS memories_status_content_idx
                     ON memories(status, content)",
                    [],
                )
                .map_err(MemoryError::DatabaseSetup)?;
            transaction
                .execute("INSERT INTO schema_migrations (version) VALUES (2)", [])
                .map_err(MemoryError::DatabaseSetup)?;
            transaction.commit().map_err(MemoryError::DatabaseSetup)?;
        }
        info!(
            target: "independence_memory",
            event = "schema_migration_completed",
            schema_version = current.max(SCHEMA_VERSION),
            duration_ms = started.elapsed().as_millis() as u64,
            "memory schema migration checked"
        );
        Ok(())
    }
}

fn timestamp_ms() -> Result<i64, MemoryError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| MemoryError::ClockBeforeUnixEpoch)?
        .as_millis();
    i64::try_from(millis).map_err(|_| MemoryError::ClockBeforeUnixEpoch)
}

fn validate_memory_input(input: &MemoryInput) -> Result<(), MemoryError> {
    if input.content.trim().is_empty() {
        return Err(MemoryError::EmptyMemoryContent);
    }
    if input.content.len() > MAX_MEMORY_CONTENT_BYTES {
        return Err(MemoryError::MemoryContentTooLarge {
            maximum: MAX_MEMORY_CONTENT_BYTES,
        });
    }
    if input.evidence.len() > MAX_EVIDENCE_ENTRIES {
        return Err(MemoryError::TooManyEvidenceEntries {
            maximum: MAX_EVIDENCE_ENTRIES,
        });
    }
    if input.related_memories.len() > MAX_RELATIONSHIPS {
        return Err(MemoryError::TooManyRelationships {
            maximum: MAX_RELATIONSHIPS,
        });
    }
    if input
        .confidence
        .is_some_and(|confidence| !confidence.is_finite() || !(0.0..=1.0).contains(&confidence))
    {
        return Err(MemoryError::InvalidConfidence);
    }
    for evidence in &input.evidence {
        if evidence.content.trim().is_empty() {
            return Err(MemoryError::EmptyEvidenceContent);
        }
        if evidence.content.len() > MAX_EVIDENCE_CONTENT_BYTES {
            return Err(MemoryError::EvidenceContentTooLarge {
                maximum: MAX_EVIDENCE_CONTENT_BYTES,
            });
        }
        if evidence.source_kind.trim().is_empty() {
            return Err(MemoryError::EmptyEvidenceSourceKind);
        }
        if evidence.source_kind.len() > MAX_EVIDENCE_SOURCE_KIND_BYTES {
            return Err(MemoryError::EvidenceSourceKindTooLarge {
                maximum: MAX_EVIDENCE_SOURCE_KIND_BYTES,
            });
        }
        if evidence
            .source_reference
            .as_ref()
            .is_some_and(|reference| reference.len() > MAX_EVIDENCE_SOURCE_REFERENCE_BYTES)
        {
            return Err(MemoryError::EvidenceSourceReferenceTooLarge {
                maximum: MAX_EVIDENCE_SOURCE_REFERENCE_BYTES,
            });
        }
    }
    Ok(())
}

fn insert_memory(
    transaction: &Transaction<'_>,
    input: &MemoryInput,
    now: i64,
    supersedes: Option<MemoryId>,
) -> Result<MemoryRecord, MemoryError> {
    transaction
        .execute(
            "INSERT INTO memories
                (content, created_at_ms, updated_at_ms, confidence, status, supersedes_memory_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                input.content,
                now,
                now,
                input.confidence,
                MemoryStatus::Current.as_database(),
                supersedes.map(MemoryId::value),
            ],
        )
        .map_err(MemoryError::Database)?;
    let memory_id = MemoryId::from_database(transaction.last_insert_rowid());
    for evidence in &input.evidence {
        transaction
            .execute(
                "INSERT INTO evidence
                    (memory_id, content, source_kind, source_timestamp_ms, source_reference, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    memory_id.value(),
                    evidence.content,
                    evidence.source_kind,
                    evidence.source_timestamp_ms,
                    evidence.source_reference,
                    now,
                ],
            )
            .map_err(MemoryError::Database)?;
    }
    for related in &input.related_memories {
        transaction
            .execute(
                "INSERT INTO memory_relationships
                    (from_memory_id, to_memory_id, relationship, created_at_ms)
                 VALUES (?1, ?2, 'related', ?3)",
                params![memory_id.value(), related.value(), now],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(_, _) => {
                    MemoryError::MemoryNotFound(related.value())
                }
                other => MemoryError::Database(other),
            })?;
    }
    load_memory(transaction, memory_id)?.ok_or(MemoryError::MemoryNotFound(memory_id.value()))
}

pub(crate) fn load_memory(
    connection: &Connection,
    memory_id: MemoryId,
) -> Result<Option<MemoryRecord>, MemoryError> {
    let memory = connection
        .query_row(
            "SELECT id, content, created_at_ms, updated_at_ms, confidence, status, supersedes_memory_id
             FROM memories WHERE id = ?1",
            [memory_id.value()],
            |row| {
                let status: String = row.get(5)?;
                let status = MemoryStatus::from_database(&status).ok_or_else(|| {
                    rusqlite::Error::InvalidColumnType(
                        5,
                        "status".into(),
                        rusqlite::types::Type::Text,
                    )
                })?;
                Ok(MemoryRecord {
                    id: MemoryId::from_database(row.get(0)?),
                    content: row.get(1)?,
                    created_at_ms: row.get(2)?,
                    updated_at_ms: row.get(3)?,
                    confidence: row.get(4)?,
                    status,
                    supersedes: row.get::<_, Option<i64>>(6)?.map(MemoryId::from_database),
                    evidence: Vec::new(),
                    related_memories: Vec::new(),
                })
            },
        )
        .optional()
        .map_err(MemoryError::Database)?;
    let Some(mut memory) = memory else {
        return Ok(None);
    };
    memory.evidence = load_evidence(connection, memory.id)?;
    memory.related_memories = load_relationships(connection, memory.id)?;
    Ok(Some(memory))
}

fn load_evidence(
    connection: &Connection,
    memory_id: MemoryId,
) -> Result<Vec<Evidence>, MemoryError> {
    let mut statement = connection
        .prepare(
            "SELECT content, source_kind, source_timestamp_ms, source_reference
             FROM evidence WHERE memory_id = ?1 ORDER BY id ASC",
        )
        .map_err(MemoryError::Database)?;
    let rows = statement
        .query_map([memory_id.value()], |row| {
            Ok(Evidence {
                content: row.get(0)?,
                source_kind: row.get(1)?,
                source_timestamp_ms: row.get(2)?,
                source_reference: row.get(3)?,
            })
        })
        .map_err(MemoryError::Database)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(MemoryError::Database)
}

fn load_relationships(
    connection: &Connection,
    memory_id: MemoryId,
) -> Result<Vec<RelatedMemory>, MemoryError> {
    let mut statement = connection
        .prepare(
            "SELECT to_memory_id, relationship
             FROM memory_relationships WHERE from_memory_id = ?1 ORDER BY id ASC",
        )
        .map_err(MemoryError::Database)?;
    let rows = statement
        .query_map([memory_id.value()], |row| {
            Ok(RelatedMemory {
                memory_id: MemoryId::from_database(row.get(0)?),
                relationship: row.get(1)?,
            })
        })
        .map_err(MemoryError::Database)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(MemoryError::Database)
}
