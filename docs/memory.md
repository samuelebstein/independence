# Development memory and context management

> **Development-only warning:** selected memories are stored in an ordinary,
> unencrypted SQLite database. This is not Private Mode and does not protect
> data from filesystem access, backups, or a compromised operating system.
> Do not use the database for sensitive personal information.

Independence has two separate kinds of context:

1. Current-session history is accepted user/assistant conversation kept in
   process memory for one run.
2. Selected long-term memories are concise facts, observations, decisions,
   corrections, and evidence chosen by a tool-capable local model.

Default Ephemeral Mode never opens, creates, reads, or changes a memory
database. Private Mode remains unavailable because authentication and encrypted
storage do not exist yet.

## Run the experiment

Choose an explicit directory. `local-data/` is ignored by Git, but it is not
encrypted.

```bash
cargo run -- \
  --mode development-memory \
  --memory-dir ./local-data/development-memory
```

For hold-to-talk voice, add the same mode and directory selection:

```bash
cargo run -- --voice \
  --mode development-memory \
  --memory-dir ./local-data/development-memory \
  --input terminal \
  --pipeline transcribe-and-speak \
  --backend ollama \
  --model <tool-capable-local-model>
```

The selected model must advertise `tools` through `/api/show`; Independence
refuses to open the database otherwise. `--mode development-memory` always
requires `--memory-dir`, and that directory option is rejected by Ephemeral and
Private modes.

The chosen directory contains `memory.db` and may contain SQLite WAL side files
(`memory.db-wal` and `memory.db-shm`) while the app runs. Keep them together.

## More detail

See the source-level public API and standalone-library notes in
[`crates/independence-memory/README.md`](../crates/independence-memory/README.md).

## Memory tools and user progress

In development-memory mode only, Independence supplies three
Ollama-compatible tools to the local model:

| Tool | Effect | Terminal progress |
| --- | --- | --- |
| `remember` | Persists a concise current memory and selected evidence | `Saving a memory...` |
| `recall` | Searches current memory content with SQLite FTS5 | `Searching memory...` |
| `correct` | Supersedes an old memory and creates a corrected current memory | `Updating a memory...` |

The model makes the usefulness judgment. It may retain a durable observation,
experiment, project decision, plan, instruction, or correction, and should
skip trivial/transient remarks. It is not forced to save every message.
Independence validates tool names and JSON arguments, applies identifier and
size limits, and returns success to the model only after SQLite confirms it.

The tools never automatically archive a whole conversation, recording, or
photo. Evidence is short selected text with a source label, optional source
timestamp, and optional opaque reference. Diagnostics and telemetry omit tool
arguments, memory content, evidence, full prompts, and tool results.

FTS5 is full-text/keyword retrieval, not semantic/vector search. An empty
result is a successful search with no match; a database failure is a separate
failure. A search returns at most 20 current memories.

## Tool-call continuity

The controller remains the sole owner of accepted session history and `TurnId`
result authorization. The memory crate has no model, controller, UI, audio, or
camera dependency.

```text
accepted history + current question
              |
              v
  context manager prepares a safe request snapshot
              |
              v
Ollama /api/chat + development-only tool definitions
              |
   answer  or  tool calls
              |
              v
validate -> progress -> SQLite -> tool result -> next model request
              |
              v
   controller-authorized final answer
```

Each tool round remains attached to the accepted exchange that caused it. The
assistant tool-call message precedes its matching tool result(s), so the next
model request receives the prior session and tool continuity. A turn has at
most four model tool-call iterations. Unknown tools, malformed arguments,
invalid identifiers, database/model errors, cancellation, stale turns, and
iteration exhaustion fail clearly rather than becoming unchecked side effects.

Each local SQLite tool operation also has a 10-second deadline. A timeout ends
the turn with an explicit failure rather than making the model infer success.

Progress wording is authored by Independence and travels through the normal
controller event path. It is terminal status, not arbitrary model text or
speech output.

Cancellation prevents a late model/tool result from being accepted by a newer
turn. An SQLite operation already running in a blocking worker cannot be
forcefully rolled back from another thread; it may finish its short transaction
after cancellation while its result is discarded. This is an explicit
development-experiment limitation, not a promise of cancelable durable writes.

## Context capacity and trimming

Before every model request, including a request after a tool result,
Independence prepares a new context snapshot. It distinguishes:

- `/api/show` metadata (`*.context_length`), which is theoretical advertised
  model capacity when present;
- the explicit runtime `num_ctx` selected by Independence (default 8,192); and
- estimated input usage for the request being assembled.

The usable input budget is `runtime context - reserved output - safety margin`.
Independence does not yet bundle every model's exact tokenizer, so its count is
a deliberately conservative UTF-8-byte estimate with message envelopes,
tool-schema JSON, tool messages/results, and a fixed allowance per image. It
is an estimate, not an exact token measurement.

If a request is too large, Independence removes the oldest complete accepted
exchange, recalculates, and repeats. It never removes system instructions, the
latest user request, a tool result without its tool call, or a tool call without
its matching results. Controller history and persistent memories are not
deleted: only the outgoing snapshot is reduced. If current required material
still cannot fit, the request fails with an actionable context-budget error.

When trimming occurs, the terminal shows `Reducing older conversation
context...`. The timeline records capacity, reserve, estimates before/after,
message/exchange trim counts, estimator type, and overflow class without
recording conversation or memory content.

## Database library

The standalone [`independence-memory`](../crates/independence-memory/README.md)
crate exposes `MemoryStore::open`, `remember`, `recall`, and `correct`.

```rust
use independence_memory::{CorrectInput, Evidence, MemoryInput, MemoryStore};

let mut store = MemoryStore::open("./chosen-directory")?;
let memory = store.remember(MemoryInput::new("A concise selected memory"))?;
let _matches = store.recall("selected memory", 5)?;
let _correction = store.correct(CorrectInput {
    memory_id: memory.id,
    replacement: MemoryInput {
        content: "The corrected memory".into(),
        confidence: None,
        evidence: vec![Evidence::new("Why it changed", "correction")],
        related_memories: vec![],
    },
})?;
# Ok::<(), independence_memory::MemoryError>(())
```

It uses bundled SQLite/FTS5, versioned schema migrations, foreign-key
enforcement, WAL, and `synchronous=FULL`. Schema version 2 also indexes the
exact current-content duplicate check, so repeated durable writes do not
become a full-table scan as a database grows. There is no SQLite server,
Docker container, network service, or separately installed SQLite requirement.
Writes and corrections use transactions. Exact duplicate current content is
reused to limit retries; this is not semantic deduplication. Corrections retain
the original as `superseded`, create a new `current` record, and add a
`corrects` relationship.

## Observability, tests, and benchmarks

Structured `tracing` spans/events cover database opening/migrations, FTS
search, remember/correct transaction duration, tool iterations, model request
time, actual SQLite mutex-gate wait, and context trimming. They contain IDs,
counts, timing, and error classes, not content or tool JSON.

Run ordinary tests:

```bash
cargo test
cargo test -p independence-memory
```

Two real-work benchmark harnesses are intentionally ignored by normal tests:

```bash
cargo test -p independence-memory --test benchmark -- --ignored --nocapture
cargo test think::context::tests::benchmarks_large_history_assembly_and_chronological_trimming -- --ignored --nocapture
```

The SQLite benchmark uses temporary directories and reports commit/FTS timings
at 100, 1,000, 10,000, and 100,000 rows. Run the same command on the Raspberry
Pi for target-specific numbers. Results depend on CPU, filesystem, compiler
profile, and SQLite build, so no number is claimed until measured on a named
machine.

## Manual development check

Use non-sensitive test data only.

1. Start the typed development experiment with the first command in this
   document and confirm the terminal warns about unencrypted storage and names
   the selected database path.
2. Tell a tool-capable local model a durable test observation, such as: “I
   tested Example Model on a test machine and it took 40 seconds. Please retain
   that result for later.” If the model selects `remember`, Independence first
   prints `Saving a memory...`, then the final answer may state what it did.
   A model is allowed to decide that a statement is not worth saving; that is
   why deterministic commit behavior is covered by the automated tool tests.
3. Exit with `/quit`, restart with the same `--memory-dir`, and ask a direct
   question such as “Search your saved memory for the Example Model test.” If
   the model selects `recall`, the terminal prints `Searching memory...` before
   it answers from the tool result.
4. State a correction, for example “That was the 4B model, not the 8B model.”
   A correct model action prints `Updating a memory...`; old content remains
   superseded rather than silently rewritten.
5. Start ordinary `cargo run` without development mode. It must not open the
   selected database or offer memory tools. Passing `--memory-dir` without
   `--mode development-memory` is rejected at argument parsing.

This is a functional experiment, not a protection test. Do not treat a correct
terminal response as proof that the database is encrypted, that a model will
always make ideal memory decisions, or that an operating system cannot inspect
the data.

## Security boundaries

- This milestone does not implement PINs, authentication, encryption, LUKS,
  recovery phrases, secure deletion, encrypted backups, cloud storage,
  embeddings, semantic retrieval, or production Private Mode.
- Terminal dialogue can remain in terminal scrollback or redirected stdout.
  Redacted telemetry does not encrypt OS memory, swap, crash dumps, model
  internals, or the database file.
- A future protected-storage milestone must move this database behind the
  SecurityManager private-storage boundary. This experiment does not bypass it
  or claim personal-data protection.
