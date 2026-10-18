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

## Follow one dictation recording or spoken question

Both `--dictate` and `--voice` select the raw-key listener instead of the
normal line reader. They share the local recording/transcription path:

```text
raw Enter key
  -> listen::dictation sends DictationInput::Enter
  -> DictationController allocates a RecordingId and requests capture
  -> CPAL opens the selected microphone only for that recording
  -> callback downmixes to bounded mono f32 samples
  -> Enter (or 30 seconds) drops the stream and releases the microphone
  -> Rubato resamples the retained clip to 16 kHz
  -> whisper-rs runs whisper.cpp on a blocking worker
  -> worker sends TranscriptReady { recording_id, text }
  -> DictationController checks the current RecordingId
  -> DictationSession returns AcceptedTranscript { text }
```

`--dictate` stops there: its terminal loop displays the transcript and never
constructs an Ollama client, an application `Controller`, or a camera service.
A transcript that looks like `/camera`, `/quit`, or any other command remains
displayed data.

`--voice` has a narrow, explicit bridge, but it no longer turns every
transcript directly into `Input::Ask`:

```text
accepted DictationSession transcript
  -> VoiceRuntime constructs Event::VoiceTranscript(text)
  -> Controller either starts a new DecisionId or recognizes a pending permission reply
  -> TurnRuntime asks local Ollama for a JSON-schema-validated result
  -> Controller alone accepts, rejects, or starts later work from that result
```

It deliberately does **not** call `listen::parse_line`. Spoken `/camera` or
`/quit` text therefore has no command or hardware authority. The dictation and
main controllers remain separate: `RecordingId` decides whether speech is
current; `DecisionId` decides whether a structured choice is current; and
`TurnId` decides whether a final answer/camera turn is current.

## Follow the continuous VAD monitor

`cargo run -- --voice --vad-monitor` is deliberately a fourth mode, separate
from both dictation paths. It uses the raw-key reader because it needs `M`, Esc,
and Ctrl+C immediately, but it creates no `DictationSession`, Whisper
transcriber, Ollama client, main `Controller`, camera service, transcript, or
turn request.

```text
M unmute
  -> VadMonitorSession allocates CaptureGeneration N
  -> CPAL opens exactly one selected input stream
  -> callback converts sample format + downmixes into a two-second bounded queue
  -> one blocking VAD worker drains that queue
  -> persistent Rubato Fft resampler converts actual rate -> 16 kHz mono
  -> exact 512-sample frames enter one stateful Silero VAD instance
  -> VadMonitorEvent { CaptureGeneration N, activity } returns to the session
  -> session accepts only N and asks speak::emit_vad_monitor to display it
```

Silero VAD v5 accepts only 16 kHz frames of 512 samples (32 ms), so the code
does not pass arbitrary CPAL callback buffers to the detector. A callback may
contain any number of interleaved samples and may arrive at the device’s actual
rate, such as 48 kHz stereo. CPAL's `FromSample` conversion and a channel
average make it mono f32; a persistent Rubato `Fft` preserves resampling state
as it accumulates exactly the next input chunk. The detector also preserves its
recurrent state across frames. Resetting either one per callback would produce
incorrect, jittery stream behavior.

The callback is intentionally a producer, not a worker: it never runs ONNX,
prints, blocks on a message send, or appends to an unbounded recording. If the
preallocated queue is full or temporarily locked by the worker, it counts the
dropped mono frames. The monitor reports that condition rather than silently
treating missing samples as silence. If CPAL delivers no callbacks for two
seconds, the monitor reports that it is waiting/stalled; it does not emit a
fake “no speech” period.

`VadSettings` keeps the classifier readable rather than flickery: the default
requires two consecutive frames at or above 0.50 to start/resume speech and
eight below it to pause. It reports elapsed non-speech at most once per second.
Those environment-configurable values are acoustic smoothing only. A pause is
explicitly **not** a future conversation-completion rule.

Mute is a state transition, not an indicator change. The controller first
marks generation N obsolete, then drops CPAL's stream, clears its queue, and
asks the long-lived worker to deactivate N. A late event from N cannot print
after muting or a new unmute because the monitor accepts only the current
generation. The worker is created once so repeated mute/unmute does not pile
up native ONNX workers. The native detector call has no cooperative cancellation
hook; shutdown releases CPAL immediately and the worker observes shutdown
between short frames. This is a cleanup strategy, not a claim that arbitrary
native code can be force-killed safely.

`voice_activity_detector 0.2.1` embeds the Silero ONNX model. Its default
`ort` dependency downloads a prebuilt ONNX Runtime bundle during the first
Cargo build; this Linux Mint build selected static `onnxruntime` linking from
the ORT cache rather than requiring a system-wide shared library. No separate
VAD model path is needed. That build-time network/native dependency is
different from the local Whisper weight file, which remains needed only for
`--dictate` and ordinary `--voice`.

## Follow one spoken photo-permission request

This flow exists only in `--voice`; normal keyboard questions and explicit
`/camera` commands keep their established paths.

```text
accepted spoken question
  -> Controller snapshots accepted history + accepted Photo into DecisionRequest::ChoosePhoto
  -> think::OllamaClient::decide_photo returns JSON: respond or request_photo + needed_visual_evidence
  -> respond: controller accepts its answer/history
  -> request_photo: app inventories/selects one camera without a capture stream
  -> Controller shows one exact question with the missing visible detail,
     and stores original question + action + camera
  -> next accepted spoken text becomes DecisionRequest::InterpretPermission
  -> Ollama sees exact question, action, selected camera, and reply; returns yes/no/unclear
  -> yes: controller starts VisualRequest::ProposeCamera using the original question
  -> no: controller starts the original question without a new photo
  -> unclear: controller re-shows the exact permission question, with no capture
```

`ChoosePhoto` is not a live-camera operation. If there is an accepted scene,
`TurnRuntime::run_decision` passes its in-memory bytes to
`OllamaClient::decide_photo` alongside the accepted conversation and current
spoken question. The routing prompt says that this is an existing still image,
so the model should answer questions about it first. A `request_photo` in that
case means an additional/different view, not another copy of the same scene.
The structured `needed_visual_evidence` field is a short, user-facing purpose
such as “the other side of the label”; it is not a displayed hidden-reasoning
trace. Empty or malformed purposes are rejected before camera preflight.

The decision prompt also establishes capability limits: this prototype has no
system clock, calendar, GPS, web, live video, or other sensors. A generic
question about the current time/date should receive that limitation, not a
speculative request to photograph the room. This is a local-model instruction;
the test suite verifies the prompt and protocol boundary, not Qwen's wisdom on
every live question.

The words in the reply are not matched against a list. Qwen receives structured
context and produces a schema-constrained classification; Serde validates it
again. The controller checks that the `DecisionId` is still current and moves
the pending request out of state before a `yes` can start capture. Therefore a
late, failed, malformed, duplicate, reset, or cancelled decision cannot turn
on the camera.

Only a safely selected single V4L2 camera can reach the permission question.
If no camera exists or selection is ambiguous, the prototype reports that it
cannot make this request; it does not guess a `/dev/video*` device. Selection
inspects hardware but does not start a stream. The actual capture begins only
after the controller accepts a current `yes`, then immediately uses the
existing bounded `VisualRequest::ProposeCamera` path.

`DictationController` has READY, recording, finishing, transcribing, and two
internal cancelling states. The extra cancelling states solve a real ownership
problem: pressing Esc must not say READY while CPAL still owns a device or
while native recognition might still return text. Once a cancel changes the
active phase, a completion with that ID is either used only for cleanup or
ignored; it cannot display or submit a stale transcript. In voice mode, Enter
at READY first stops active decision/answer work before it starts the next
question recording. The exception is a visible permission question: Enter
preserves that request and records its reply. Esc at READY abandons the
permission request or stops model work without clearing accepted history.

## Why the controller owns state

Model workers get owned snapshots; they cannot print and they cannot mutate the
session. The controller stores:

- the accepted `Photo`, if any;
- accepted user/assistant exchanges only; and
- at most one active `TurnId`, one active `DecisionId`, and one pending
  permission request.

The key invariant is simple: a result is accepted only when its `TurnId` or
`DecisionId` is still active. Starting a new valid question, `/stop`, `/reset`,
EOF, and Ctrl+C remove old IDs and pending permission first. A late completion
then becomes a harmless no-op.

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

Dictation and voice input need individual keys rather than submitted lines, so
they do not start the normal reader. `listen::dictation` enables Crossterm raw
mode and owns one separate polling thread. It maps only `KeyEventKind::Press`:
Enter, Esc, Ctrl+C, and `M`. Repeat and release events are ignored, so holding
Enter cannot queue a second recording and holding `M` cannot repeatedly toggle
the monitor. `M` is deliberately ignored by `--dictate` and ordinary `--voice`;
it is the explicit mute/unmute control only in `--vad-monitor`. Its short poll
interval lets shutdown join the thread before the raw-mode guard restores normal
terminal settings. `--voice` and the monitor use raw-mode-safe output; ordinary
`println!` is not assumed to behave correctly while raw mode is active.

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

Dictation has a similar but narrower qualification. CPAL's callback runs on a
platform audio thread, so it only converts sample format, averages channels,
and appends to a preallocated bounded buffer. It does not wait on Tokio or run
recognition. On Enter, Esc, or the cap, the capture owner drops CPAL's stream
before resampling or transcription. The command loop polls every 100 ms and
the cap is enforced by the capture owner, not merely displayed by the UI.

Whisper runs in `spawn_blocking`, which keeps native CPU work off Tokio's
event-loop threads. The application installs whisper.cpp's abort callback with
a pointer into an `Arc<AtomicBool>` that stays alive for the synchronous
recognition call. Esc sets that flag; it does **not** use `JoinHandle::abort`
to claim that foreign native computation died instantly. The outer task waits
for Whisper to reach a supported abort callback point, then drops the audio
and ignores any obsolete result. This is why dictation does not accept a new
recording while a cancelled worker is still cleaning up.

### Serde and reqwest versus our code

`reqwest` performs HTTP, timeout handling, proxy/redirect policy, and body
transfer. `serde` serializes/deserializes JSON. `base64` encodes image bytes.
`image` recognizes and decodes image data. For the photo and permission steps,
Ollama's `format` field carries a JSON Schema and our Serde enums validate the
returned JSON again. Our code chooses the allowed formats/limits, builds the
application message sequence, and decides whether a completed response still
belongs to the session.

The new `v4l` crate is the Linux V4L2 binding. It enumerates `/dev/video*`,
queries capabilities, negotiates a pixel format, manages MMAP buffers, and
performs the bounded poll used by one-frame capture. `image` still does the
JPEG validation and the YUYV-to-JPEG encoding step. Our code chooses the small
format/resolution policy, protects allocation sizes, restores the prior device
format, and owns the application-level cancellation and scene rules.

For dictation, CPAL discovers and opens the operating system's input stream;
it tells us the real channel count, sample format, and sample rate. Our code
uses CPAL's `FromSample` conversion and averages each input frame to mono.
Whisper requires 16 kHz mono f32 PCM, so Rubato's offline FFT resampler changes
the actual device rate instead of merely relabeling samples. `whisper-rs`
builds on whisper.cpp and performs the English CPU transcription. Our code
chooses explicit limits, manages stream/task lifetimes, and decides whether a
result is still current. `--dictate` displays accepted text only; `--voice`
hands accepted text to `Event::VoiceTranscript`, without interpreting it as an
application command. The main controller then decides whether it is a new
question or a reply to a pending permission prompt.

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

For dictation, after completing the setup in `README.md`, run this from the
repository root:

```bash
cargo run -- --dictate
```

At READY, press Enter, say one short sentence, then press Enter again. The
microphone indicator should become TRANSCRIBING only after the stream has been
released. Press Enter for a second recording and Esc to discard it; no text
from it should appear. Finally record a third, distinct sentence. The first
and third texts may appear, but never the cancelled second text. If model
loading or recognition takes longer than expected, Esc is still processed by
the raw-key thread; the UI waits for cooperative native cleanup before it
returns to READY.

For integrated voice decisions, use a running local Ollama server and launch:

```bash
cargo run -- --voice
```

Press Enter, ask “What is the primary gas in Earth's atmosphere?”, and press
Enter again. The accepted transcript should appear before the text answer.
Repeat with “Why does that matter for life?” to observe that the normal
controller receives the accepted history. While that second decision/answer is
pending, press Enter to begin another recording: the old result must become
ineligible. Press Esc to discard this new recording.

For the permission loop, use normal `cargo run` and `/cameras` first to check
that exactly one usable camera is available. Then use `--voice`, ask about an
object in front of the camera, and wait. If the model requests a photo, the
camera remains off while its exact question is displayed. Press Enter, say an
unambiguous reply such as “I approve that one still image now,” then press
Enter. The program should capture once, release the device, and answer the
original question. Repeat with “I would rather continue without a new photo”:
there should be no capture. Try a conditional reply or a question to exercise
the `unclear` path; it should re-ask without capturing. `cargo run --
--dictate` remains the separate transcript-only check; it should display speech
but never contact Ollama.

After an accepted voice capture, ask “What was in the photo?”, then “Which
visible feature should I inspect first?”, and then “What remains uncertain from
this view?” Each decision request should receive the same accepted still and
no second capture should begin. If a genuinely new view is requested, its
permission question should explicitly say that Independence already has one
accepted still and should name the missing visual detail. To approve, speak a
plain authorization such as “Yes, capture that one still now”; hedged or
conditional replies remain `unclear` under the deliberately conservative
privacy rule.

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

Dictation tests use synthetic PCM for conversion/resampling and fake recorder
and transcriber boundaries for lifecycle events. Voice-bridge tests use those
same fakes plus a local mock Ollama server; they cover accepted versus
cancelled/stale/empty transcripts, duplicate completions, follow-up history,
literal slash-looking text, structured decision serialization, permission
context, no/unclear/failure/stale outcomes, malformed photo-request purposes,
and one approved fake capture followed by three separate voice follow-ups that
reuse the same image bytes without another capture. They do not prove that the
physical microphone works, that ALSA/PipeWire picks the desired device, that
Whisper recognizes a particular voice, that a real local model follows the
structured prompt, or that a real camera is released on this machine. Those are
the separate manual checks above; no model weight, microphone, Ollama server,
or camera is required for the ordinary suite.
