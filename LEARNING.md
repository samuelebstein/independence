# Learning walkthrough

This is a guide to the small amount of Rust architecture in the current
prototype. It explains why each boundary exists, not just what its file is
called.

## Follow one `/look` request

```text
keyboard line
  -> listen parses `/look path | question` into Input::Look
  -> controller stages a TurnRequest, retaining old accepted state
  -> app worker loads and validates the selected photo
  -> think encodes its bytes and POSTs /api/chat to Ollama
  -> worker returns TurnFinished
  -> controller either accepts it or ignores it by turn ID
  -> speak prints only an accepted answer
```

The arrow labels are deliberately different:

- `Event` means something already happened: input arrived or work finished.
- `Effect` means something should happen: start/cancel a turn, display output,
  or exit.

Most code stays ordinary function calls. The controller only uses events at
boundaries where arrival order matters.

## Why the controller owns state

The model worker gets an owned snapshot of history, question, and photograph.
It cannot print and it cannot mutate the session. The controller stores:

- the accepted `Photo`, if any;
- accepted user/assistant exchanges only; and
- at most one active `TurnId`.

The key invariant is simple: a result is accepted only when its `TurnId` is
still active. Starting a new valid question, `/stop`, `/reset`, EOF, and Ctrl+C
remove the old ID first. A late completion then becomes a harmless no-op.

A `/look` is different from a regular question. Its photo is only a proposal
until both loading and inference succeed. On success it replaces the accepted
scene and starts that new scene's history with the accepted exchange. On a
failure or cancellation, the prior scene and history remain intact.

## Rust concepts appearing here

### `async` and Tokio

`async fn` describes work that can pause at `.await`. The HTTP request pauses
while the local Ollama server works, letting the main event loop receive
`/stop` or the next question. Tokio supplies the executor, bounded `mpsc`
channel, task handles, and Ctrl+C signal handling. We use its library
mechanisms rather than write our own scheduler or queue.

The terminal's `read_line` is blocking, so `listen` runs it on one dedicated
standard thread. That is intentionally outside Tokio executor threads. It
sends parsed input into the bounded channel. A blocked standard input read
cannot be portably forced awake, so the thread is not joined on shutdown; it
does not own an inference task and exits naturally on EOF, closed channel, or
process exit.

### Ownership snapshots and `Arc`

`TurnRequest` owns the data a worker needs, which avoids shared mutable session
state. `Photo` stores bytes in `Arc<[u8]>`: cloning a photo for a follow-up
clones a small reference count, not up to 10 MiB of image data. The controller
keeps the original accepted copy alive while the worker uses its snapshot.

### Cancellation is two layers

`CancellationToken` asks a worker to stop and drops its pending HTTP future.
`JoinHandle::abort` makes shutdown prompt for the application task. Neither is
a claim that the separate Ollama server immediately halted its own internal
inference. The turn-ID invariant supplies the stronger user-visible guarantee:
an obsolete result cannot become an answer or history entry.

### Serde and reqwest versus our code

`reqwest` performs HTTP, timeout handling, proxy/redirect policy, and body
transfer. `serde` serializes/deserializes JSON. `base64` encodes image bytes.
`image` recognizes and decodes image data. Our code chooses the allowed
formats/limits, builds the application message sequence, and decides whether a
completed response still belongs to the session.

## Useful experiments

Run these from the repository root.

```bash
cargo run
```

Ask a text question, then type another before the first finishes. Only the
newer turn is allowed to answer. Type `/stop` during a slow turn; there should
be no accepted answer from it. Type `/reset`, then give an invalid `/look`
path; the diagnostic should appear without restoring or erasing other state.

For a photo, use a path you intentionally selected:

```text
/look data/tree.jpg | What features can I observe before trying to identify this tree?
What other angle would help?
```

The first line stages the image; the second uses it only after the first answer
was accepted. The model's answer is not proof of an identification—use the
suggested observations and independent references to verify it.

## What tests establish—and what they cannot

The test suite exercises real controller transitions, real request
serialization against a local mock HTTP server, bounded image loading, and
controlled race completions. It demonstrates software properties such as
“a stale completion cannot enter history.” It cannot prove that a model is
accurate, that it followed the teacher prompt, or that a visual interpretation
is safe. Those remain reasons to inspect, verify, and improve the system.
