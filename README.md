# Independence

Independence is an experiment in a local-first computer for noticing and
learning about the physical world. This repository currently contains a narrow
terminal prototype—not a finished camera device or general agent.

It can:

- take keyboard input as its `listen` capability;
- send a text question, or one explicitly selected JPEG/PNG plus a question,
  to a local Ollama model as `think`;
- list local Linux V4L2 cameras and explicitly capture one still image as
  `see` input;
- retain the last successfully accepted photograph and conversation for
  follow-up questions as `see` context; and
- print an accepted answer as `speak`.

It does not access a microphone, persist a conversation, stream tokens, call
tools autonomously, or use a cloud model.

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
terminal app. It uses `http://127.0.0.1:11434` and the local non-thinking
vision model `qwen3-vl:4b-instruct` unless you set environment variables for
that one command:

```bash
OLLAMA_URL=http://127.0.0.1:11434 OLLAMA_MODEL=qwen3-vl:4b-instruct cargo run
```

This does not change shell configuration or Ollama settings. The model must
already be available from a running local Ollama server. If it is not, the app
reports the local connection failure rather than falling back to a cloud API.

`qwen3-vl:4b-instruct` retains text and image input but is a separate
non-thinking model variant. On this machine it answered a one-word prompt with
two generated tokens, while the installed `qwen3-vl:4b` thinking variant spent
hundreds of tokens reasoning first. To deliberately use that slower reasoning
variant again, set `OLLAMA_MODEL=qwen3-vl:4b` for one `cargo run` command.

If you need to install or download a model, do that deliberately outside this
project. For example, `ollama pull qwen3-vl:4b-instruct` uses the network and
consumes local disk space; it is not run automatically by this application.
Starting `ollama serve` starts a local service and binds its configured local
endpoint.

The camera code adds the Rust `v4l` crate for Linux V4L2 devices. Cargo may
download and compile it on the first build; this current Linux Mint machine
already compiled it using its existing toolchain. The application does not
install packages, alter `/dev/video*` permissions, or run as root. On another
machine, a build error about the C/Clang toolchain is a system-setup issue to
resolve deliberately outside this repository.

## Use the terminal prototype

```text
What is the primary gas in Earth's atmosphere?

/look data/tree.jpg | What might this tree be?
What detail would help distinguish it from an oak?

/cameras
/camera 0 | What visible features should I notice?
What should I inspect next?

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

### Camera commands (Linux V4L2)

Run `/cameras` first. It inspects `/dev/video*` nodes for the capture and
streaming capabilities this prototype supports, but does **not** start a
capture stream. It prints the numeric identifier and a kernel-provided name:

```text
independence: usable cameras:
  0  Laptop Camera: Laptop Camera  (/dev/video0)
```

Then use one of these literal commands:

```text
/camera | What visible features should I notice?
/camera 0 | What visible features should I notice?
```

The no-ID form works only when exactly one usable camera is listed. With two
or more entries, explicitly choose one; the application never guesses the
first `/dev/video*` node. A malformed ID, an ID not currently listed, no
camera, or ambiguous no-ID command reports an error before capture begins and
does not interrupt an unrelated active turn.

For an accepted camera turn, Independence requests a conservative MJPEG or
YUYV format at up to 1280×720, validates the format actually negotiated with
the driver, discards up to two warm-up frames, and keeps one final still in
memory. It emits the negotiated dimensions after capture, closes the V4L2
stream and device, restores the prior format when possible, and only then
contacts Ollama. YUYV frames are encoded as JPEG in memory; MJPEG frames are
fully validated as JPEG. No capture is written to disk or logged as payload
data.

Like `/look`, `/camera` stages a proposed new scene. The camera image and its
new history replace the accepted scene only after Ollama returns an accepted
answer. A failure or cancellation retains the previously accepted photo and
history. Follow-up questions use the last accepted camera image; they do not
activate the camera again.

`/stop` makes the application immediately reject the camera turn. A running
blocking V4L2 frame read cannot be safely killed by Tokio, so it is allowed to
finish its bounded three-second capture window and release its handle in the
background. A one-permit gate prevents repeated cancellations from piling up
camera workers or opening the device twice. The three-second bound applies to
the poll-based frame waits; V4L2 open/configuration ioctls are driver calls
without a separate timeout exposed by this backend, so a broken kernel driver
can still misbehave there. Independence does not claim to forcibly stop that
kind of driver failure.

### Small manual experiments

Start the app with `cargo run` from the repository root. For an explicit file
photo and follow-up, type:

```text
/look /home/samebstein/Downloads/testtree.jpeg | What visible features can help identify this tree?
What part of the image should I inspect next?
```

For a camera still, use the ID `/cameras` reported, then type:

```text
/camera 0 | What visible features should I notice?
What observation would reduce uncertainty?
```

To test cancellation, submit a camera question and type `/stop` while the
capture or local inference is active. The application should not print an
accepted answer or add an exchange. The camera may remain busy for up to the
three-second bounded acquisition window while its worker closes. To test an
error that must not alter an accepted scene, use a deliberately invalid file:

```text
/look /definitely/not/a/photo.jpeg | What is this?
```

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

The ordinary test suite intentionally does not touch a real camera. To test
enumeration, acquisition, validation, release, and negotiated dimensions on
this computer **without Ollama**, first identify an explicit ID with
`/cameras`, then run this from the repository root:

```bash
INDEPENDENCE_CAMERA_ID=0 cargo test manual_v4l_capture_without_ollama -- --ignored --nocapture
```

This intentionally opens the selected local camera for one bounded capture,
prints its dimensions/format, and releases it. It does not save a photo or
make an HTTP request. Replace `0` with the ID you chose. Do not run it as root
or change device permissions merely to make it pass.

## Structure

```text
src/
  main.rs        starts the Tokio runtime
  app.rs         owns worker-task lifecycle and application shutdown
  protocol.rs    typed facts (events) and requested side effects (effects)
  controller.rs  owns accepted scene/history and authorizes results
  listen/        blocking keyboard adapter and command parser
  see/           bounded, validated file-photo and camera-image handling
    camera.rs    V4L2 inventory, capture, conversion, and capture gate
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

- Keyboard input only; no microphone, wake word, or text-to-speech.
- One explicit JPEG/PNG or one explicit camera still at a time; no live view,
  continuous vision, background capture, or autonomous camera use.
- Camera support is Linux V4L2 single-plane MJPEG/YUYV only; unusual drivers or
  multi-plane-only devices may not be listed as usable.
- Non-streaming Ollama responses; no partial answer is printed.
- Six accepted exchanges per scene/session; this is a simple context cap, not
  exact token budgeting. Start a new scene with `/look` or use `/reset`.
- No persistent memory, database, vector store, agent framework, plugins,
  containers, or cloud fallback.
- The client uses a 2-second connect timeout and a 120-second request timeout,
  disables redirects and ambient HTTP proxy use, and never logs image payloads
  or full requests.
