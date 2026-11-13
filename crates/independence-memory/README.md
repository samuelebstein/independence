# `independence-memory`

`independence-memory` is a small, reusable Rust library for selected local
memories. It owns an ordinary SQLite database, SQLite FTS5 search, concise
evidence, and correction history. It deliberately does not know about Ollama,
Independence's controller, UI, audio, cameras, sessions, or authentication.

It is not a conversation archive, semantic/vector search engine, or security
product. In Independence's current development experiment, the database is
plain-text SQLite storage and must not be used for sensitive information.

## Storage and dependencies

The crate links SQLite into the Rust binary through `rusqlite`'s `bundled` and
`modern_sqlite` features. It does not need a SQLite server, Docker, network
connection, or separately installed database package. The caller must provide
an explicit directory:

```rust
use independence_memory::{Evidence, MemoryInput, MemoryStore};

let mut store = MemoryStore::open("./development-memory")?;

let mut input = MemoryInput::new("The Raspberry Pi 5 test used the 4B model and took 40 seconds.");
input.evidence.push(Evidence::new(
    "The user reported the measurement and later corrected the model size.",
    "user_statement",
));
let memory = store.remember(input)?;

let matches = store.recall("Raspberry Pi model", 5)?;
println!("stored {} and found {} match(es)", memory.id.value(), matches.len());
# Ok::<(), independence_memory::MemoryError>(())
```

`MemoryStore::open` creates `<chosen-directory>/memory.db` when needed and
opens the same file on later calls. It never chooses a home-directory,
temporary-directory, or network location itself. SQLite may also create
`memory.db-wal` and `memory.db-shm` while write-ahead logging is active; those
files are part of the same database and need to be kept together for copying
or backup.

## Public API

The intentionally small API is:

- `MemoryStore::open(directory)` opens or initializes the versioned schema.
- `remember(MemoryInput)` atomically stores one selected current memory with
  optional `Evidence` and related-memory IDs.
- `recall(query, limit)` searches current memories through FTS5 and returns a
  bounded `Vec<MemoryRecord>`; an empty vector is a successful no-match.
- `correct(CorrectInput)` atomically marks a prior current memory
  `Superseded`, creates a new current memory, and records the `corrects`
  relationship.

`MemoryInput` accepts concise content, optional confidence in the inclusive
`0.0..=1.0` range, selected textual evidence, and related memory IDs.
`Evidence` retains short supporting text, a caller-selected source label, an
optional source timestamp, and an optional opaque source reference. The crate
does not interpret source labels or impose a taxonomy.

For example, a correction retains the original rather than overwriting it:

```rust
use independence_memory::{CorrectInput, Evidence, MemoryInput, MemoryStore};

# let mut store = MemoryStore::open("./development-memory")?;
# let original = store.remember(MemoryInput::new("The test used an 8B model."))?;
let corrected = store.correct(CorrectInput {
    memory_id: original.id,
    replacement: MemoryInput {
        content: "The test used a 4B model.".into(),
        confidence: None,
        evidence: vec![Evidence::new(
            "The user corrected 8B to 4B.",
            "correction",
        )],
        related_memories: Vec::new(),
    },
})?;
assert_eq!(corrected.previous.id, original.id);
# Ok::<(), independence_memory::MemoryError>(())
```

Inputs are bounded and validated rather than silently truncated. `remember`
also avoids an exact duplicate of an existing current content string. That is
retry protection only, not semantic deduplication: callers remain responsible
for deciding whether related facts should be a correction or a distinct memory.

## Schema and migration behavior

The current schema is migration version `2` and contains:

- `memories`: current and superseded memory records;
- `evidence`: selected textual support for a memory;
- `memory_relationships`: `related` and `corrects` links; and
- `memory_fts`: an FTS5 index over memory content, maintained by SQLite
  triggers.

Migration `2` adds the ordinary `(status, content)` index used by the exact
duplicate check. It keeps repeated durable `remember` calls from degrading
into a full-table scan as the development database grows; it is not semantic
deduplication or a relevance-ranking index.

Each opening checks the `schema_migrations` table before use. A database newer
than the library understands is rejected rather than silently changed. Writes
and corrections use SQLite transactions; a successful call returns only after
the transaction commits. The connection uses foreign-key enforcement, WAL, and
`synchronous=FULL`.

FTS retrieval is keyword/full-text matching, not semantic search. The crate
turns input into literal terms before issuing the FTS query, returns only
current memories, and accepts a limit from 1 through 20. It does not store raw
audio, images, or complete conversations unless an application deliberately
places such content into a selected text field—which Independence must not do.

## Observability and safety limits

The crate emits structured `tracing` spans/events for database open/migration,
remember, recall/FTS search, and correction. They include counts, IDs, and
durations, but not memory content or evidence text.

The crate is synchronous because SQLite owns one connection. A concurrent host
application must provide its own synchronization boundary; Independence uses a
small mutex around one `MemoryStore`. This crate does not encrypt the database,
authenticate callers, make deletion secure, or protect against a compromised
operating system.

## Tests and local benchmarks

Run focused library tests from the repository root:

```bash
cargo test -p independence-memory
```

The repeatable benchmark harness is intentionally ignored in normal tests
because it inserts up to 100,000 rows and its numbers depend on the machine
and filesystem:

```bash
cargo test -p independence-memory --test benchmark -- --ignored --nocapture
```

It uses temporary directories and prints commit/FTS timings for 100, 1,000,
10,000, and 100,000 memories. Run the same command on a Raspberry Pi to obtain
Pi-specific numbers; do not compare those results directly with a desktop
without recording storage, CPU, release/debug profile, and SQLite build.
