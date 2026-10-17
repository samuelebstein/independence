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

## Follow one `/camera` request

```text
keyboard line
  -> listen parses `/camera [id] | question` into Input::Camera
  -> controller creates a CameraRequestId, but does not cancel an active turn
  -> app inventories usable V4L2 devices without starting a stream
  -> controller receives CameraPrepared
  -> only a valid selection becomes a new TurnRequest with a TurnId
  -> see::camera acquires the single capture gate and opens that device
  -> it discards up to two warm-up frames, validates one final still, and closes
  -> think encodes the in-memory JPEG and POSTs /api/chat to Ollama
  -> controller accepts or ignores the result by TurnId
```

This two-step start is intentional. A command such as `/camera 99 | ...` is
syntactically well-formed, but the ID is invalid only after checking current
hardware. The `CameraRequestId` makes that check stale if a newer valid command,
`/stop`, or `/reset` arrives first. An error therefore leaves a separate active
question, accepted scene, and history alone. Listing devices only opens nodes
for capability inspection; it does not start a capture stream or take a frame.

`Photo` now has two pieces of information: validated image bytes and source
metadata. A file source has a literal path. A camera source has a camera ID,
name, negotiated dimensions, and input format—not a made-up filesystem path.
Both become the same base64 image field in the existing Ollama request.

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

`/camera` has those same accepted-scene rules. It is never a live view: a
follow-up uses the successfully accepted still photo and does not re-open the
camera. Capture or inference failure leaves the old scene untouched.

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

Camera acquisition adds one important qualification. Tokio can abort an async
task, but it cannot safely kill a `spawn_blocking` task that is already waiting
inside a kernel camera call. `see::camera` therefore uses V4L2's timed polling:
each frame wait is bounded and the total acquisition window is three seconds.
It also gives the blocking closure an owned Tokio `Semaphore` permit. If the
async caller is cancelled, the closure keeps the permit until it has dropped
the V4L2 stream and device. A second camera request waits rather than starting
another worker against a still-closing device. This is a bounded cleanup
strategy, not a claim that cancellation interrupts a sensor instantly. The
three-second bound covers frame polling; V4L2 setup ioctls themselves are
driver calls without a separate timeout exposed by this backend, so a broken
driver can still misbehave there.

### Serde and reqwest versus our code

`reqwest` performs HTTP, timeout handling, proxy/redirect policy, and body
transfer. `serde` serializes/deserializes JSON. `base64` encodes image bytes.
`image` recognizes and decodes image data. Our code chooses the allowed
formats/limits, builds the application message sequence, and decides whether a
completed response still belongs to the session.

The new `v4l` crate is the Linux V4L2 binding. It enumerates `/dev/video*`,
queries capabilities, negotiates a pixel format, manages MMAP buffers, and
performs the bounded poll used by one-frame capture. `image` still does the
JPEG validation and the YUYV-to-JPEG encoding step. Our code chooses the small
format/resolution policy, protects allocation sizes, restores the prior device
format, and owns the application-level cancellation and scene rules.

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

For the local camera, first type `/cameras`. If it prints more than one entry,
choose one numeric ID explicitly:

```text
/camera 0 | What visible features should I notice?
What should I inspect next?
```

The first command should report camera activation, then the actual dimensions
and that the camera was released before local inference starts. To test the
hardware path alone, without Ollama, run from the repository root after
choosing an ID:

```bash
INDEPENDENCE_CAMERA_ID=0 cargo test manual_v4l_capture_without_ollama -- --ignored --nocapture
```

That ignored test deliberately opens the selected camera, captures one bounded
still, prints its dimensions, and releases it. It is excluded from ordinary
tests because a real camera is external hardware. Try `/stop` during a camera
turn as well: no answer should be accepted, though the device can take up to
three seconds to finish its bounded cleanup.

## What tests establish—and what they cannot

The test suite exercises real controller transitions, real request
serialization against a local mock HTTP server, bounded image loading, and
controlled race completions. It demonstrates software properties such as
“a stale completion cannot enter history.” It cannot prove that a model is
accurate, that it followed the teacher prompt, or that a visual interpretation
is safe. Those remain reasons to inspect, verify, and improve the system.

Camera tests use fake backends to cover selection, failure-before-inference,
image-plus-question request construction, scene follow-ups, cancellation,
and the one-worker capture gate. They do not prove that every physical V4L2
driver behaves well; the ignored manual capture check is the separate hardware
exercise for that.
