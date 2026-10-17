# Independence

Independence is an experiment in a local-first computer for noticing and
learning about the physical world. This repository currently contains a narrow
terminal prototype—not a camera device or general agent.

It can:

- take keyboard input as its `listen` capability;
- send a text question, or one explicitly selected JPEG/PNG plus a question,
  to a local Ollama model as `think`;
- retain the last successfully accepted photograph and conversation for
  follow-up questions as `see` context; and
- print an accepted answer as `speak`.

It does not access a microphone or camera, persist a conversation, stream
tokens, call tools autonomously, or use a cloud model.

## Start here

Read these files in this order:

1. [`VISION.md`](VISION.md) — why the project exists.
2. [`src/main.rs`](src/main.rs) — the tiny executable entry point.
3. [`src/app.rs`](src/app.rs) — runs input, worker tasks, and shutdown.
4. [`src/controller.rs`](src/controller.rs) — the session state machine and
   result-acceptance rule.
5. [`src/think/mod.rs`](src/think/mod.rs) and
   [`src/think/teacher.txt`](src/think/teacher.txt) — Ollama transport and
   editable teaching guidance.
6. [`LEARNING.md`](LEARNING.md) — a walkthrough of one request and the Rust
   choices behind it.

## Run it on the same laptop

Run these commands from this repository root
(`/home/samebstein/code/independence` in the current checkout).

```bash
cargo run
```

This compiles the Rust package (writing only `target/`) and starts the
terminal app. It uses `http://127.0.0.1:11434` and `qwen3-vl:4b` unless you
set environment variables for that one command:

```bash
OLLAMA_URL=http://127.0.0.1:11434 OLLAMA_MODEL=qwen3-vl:4b cargo run
```

This does not change shell configuration or Ollama settings. The model must
already be available from a running local Ollama server. If it is not, the app
reports the local connection failure rather than falling back to a cloud API.

If you need to install or download a model, do that deliberately outside this
project. For example, `ollama pull qwen3-vl:4b` uses the network and consumes
local disk space; it is not run automatically by this application. Starting
`ollama serve` starts a local service and binds its configured local endpoint.

## Use the terminal prototype

```text
What is the primary gas in Earth's atmosphere?

/look data/tree.jpg | What might this tree be?
What detail would help distinguish it from an oak?

/stop
/reset
/help
/quit
```

`/look` takes literal text before the first `|` as a path; it is never sent to
a shell. Spaces in paths work. A relative path is resolved from the directory
where Independence started. A literal `|` in the path is not supported because
it is the command delimiter.

The app reads at most 10 MiB, recognizes JPEG/PNG by content, and fully
decode-validates the image before any model request. It keeps the bytes only
in memory. A failed, cancelled, missing, oversized, or unsupported `/look`
does not replace the last accepted photograph or history.

`/stop` cancels the application-owned request task without adding a model
answer to history. It cannot prove that the independent Ollama server stopped
all of its computation immediately. `/reset` also cancels active work and
clears this application's in-memory scene/history; it does not erase the
source image, terminal scrollback, or server-side logs.

## Use your own server through SSH

The client intentionally accepts only loopback URLs. If `mrfoxybrown` is a
hostname your SSH configuration can resolve, create a tunnel in a separate
terminal:

```bash
ssh -N -L 127.0.0.1:11435:127.0.0.1:11434 mrfoxybrown
```

That command opens a persistent SSH connection and maps this laptop's
`127.0.0.1:11435` to the server's own loopback Ollama port. It requires a
network connection to that server and does not expose Ollama publicly. Leave
that terminal open, then run Independence from this repository root:

```bash
OLLAMA_URL=http://127.0.0.1:11435 cargo run
```

Inference in this setup runs on your server, not the laptop, even though the
application still talks only to a local loopback port. No hostname resolution
or old private address is assumed by the application.

## Verify the code

Run each command from this repository root:

```bash
cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
```

`fmt --check` verifies formatting without changing source. `check` type-checks
without producing a runnable binary. `test` uses Wiremock loopback servers and
synthetic temporary image fixtures—no GPU, installed model, real photo, or
remote server is required. `clippy` treats lint warnings as failures. Cargo may
need network access the first time it downloads declared crates; subsequent
runs normally use its local cache.

## Structure

```text
src/
  main.rs        starts the Tokio runtime
  app.rs         owns worker-task lifecycle and application shutdown
  protocol.rs    typed facts (events) and requested side effects (effects)
  controller.rs  owns accepted scene/history and authorizes results
  listen/        blocking keyboard adapter and command parser
  see/           bounded, validated explicit photo loading
  think/         Ollama /api/chat client and teacher prompt
  speak/         terminal answers, diagnostics, and help
```

The controller holds the only accepted scene and conversation. Each model turn
gets a unique ID plus an owned snapshot. A completion may print or update
history only if its ID is still the active one. Therefore an old result cannot
revive state after `/stop`, `/reset`, a newer question, EOF, or Ctrl+C.

The system prompt asks the model to be a careful teacher, distinguish
observation from inference, and avoid overconfidence. Those are model
instructions, not guarantees. File validation, request formatting, bounded
history, cancellation, and result authorization are the behaviors enforced by
application code.

## Current limits

- Keyboard input only; no microphone, wake word, camera, or text-to-speech.
- One explicit JPEG/PNG at a time; no live view or automatic capture.
- Non-streaming Ollama responses; no partial answer is printed.
- Six accepted exchanges per scene/session; this is a simple context cap, not
  exact token budgeting. Start a new scene with `/look` or use `/reset`.
- No persistent memory, database, vector store, agent framework, plugins,
  containers, or cloud fallback.
- The client uses a 2-second connect timeout and a 120-second request timeout,
  disables redirects and ambient HTTP proxy use, and never logs image payloads
  or full requests.
