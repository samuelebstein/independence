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
  -> speak logs only an accepted answer (and --voice may queue it for local audio)
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

On the default laptop/terminal path, both `--dictate` and `--voice` select a
raw-key listener instead of the normal line reader. `--dictate` retains a
tap-to-start/tap-to-finish interaction. `--voice` uses a terminal that can
report real press and release events, so it does not guess when Enter was
released:

```text
hold-to-talk Enter press
  -> VoiceRuntime queues the fixed "i'm listening" cue to SpeechService
  -> after that exact cue plays (or audio fails visibly), VoiceRuntime asks
     DictationController to allocate a RecordingId and request capture
  -> CPAL opens the selected microphone only for that recording
  -> callback downmixes to bounded mono f32 samples
  -> periodic bounded snapshots run display-only local recognition
  -> real Enter release (or 30 seconds) drops the stream and releases the microphone
  -> Rubato resamples the retained clip to 16 kHz
  -> whisper-rs runs whisper.cpp on a blocking worker
  -> worker sends TranscriptReady { recording_id, text }
  -> DictationController checks the current RecordingId
  -> DictationSession returns AcceptedTranscript { recording_id, text }
```

`--dictate` stops there: its terminal loop displays the transcript and never
constructs an Ollama client, an application `Controller`, or a camera service.
A transcript that looks like `/camera`, `/quit`, or any other command remains
displayed data.

`--voice` keeps an unfinished draft around that final lifecycle:

```text
P press while Enter is held
  -> VoiceRuntime queues one in-memory V4L2 capture in press order
  -> each completed full partial replaces, rather than appends to, draft text
accepted final DictationSession transcript + all requested capture completions
  -> VoiceRuntime constructs one Input::Ask(text) or Input::AskWithPhotos
  -> existing Controller assigns a new TurnId and starts the normal Ollama turn
  -> existing controller accepts the result by TurnId before raw-mode-safe output
```

It deliberately does **not** call `listen::parse_line`. Spoken `/camera` or
`/quit` text therefore has no command or hardware authority. The dictation and
main controllers remain separate: `RecordingId` decides whether speech is
current; `TurnId` decides whether a model answer is current.

`DictationController` has READY, recording, finishing, transcribing, and two
internal cancelling states. The extra cancelling states solve a real ownership
problem: pressing Esc must not say READY while CPAL still owns a device or
while native recognition might still return text. Once a cancel changes the
active phase, a completion with that ID is either used only for cleanup or
ignored; it cannot display or submit a stale transcript. In voice mode, a new
Enter press at READY first stops any active model turn and speech playback,
then queues a new listening cue. Capture begins only when that particular cue
has completed. Releasing Enter or pressing Esc while it is still playing clears
the draft first, so its late completion cannot open the microphone. Esc also
cancels the microphone, partial/final transcription, photo work, synthesis,
playback, and inference without clearing accepted history.

## Local speech output is a separate worker

The speech implementation is intentionally downstream from the controller. A
model worker still returns text; it does not know about speakers. The
controller first authorizes `Output::Answer`, then `OutputMode::Voice` in
[`src/app.rs`](src/app.rs) performs two explicit actions with that exact text:

```text
accepted Output::Answer(text)
  -> speak::independence_says(text)       timestamped terminal log now
  -> SpeechHandle::say(TurnId, text)      bounded async queue
  -> PiperCli::synthesize(text)           local subprocess, WAV stays in memory
  -> RodioPlayback::play(WAV)              selected output device
```

[`src/speak/worker.rs`](src/speak/worker.rs) owns one ordered queue (capacity
8), so an acknowledgement or answer cannot overlap or overtake an earlier
utterance. Its requests carry both an utterance ID and the current cancellation
generation. Escape and a new Enter press increment that generation before they
cancel the unfinished interaction; the worker then kills an in-progress Piper
child or stops the current Rodio player at its next check, and stale generated
audio is never started later.

There are two fixed cues: `i'm listening` before capture begins and `i'm
thinking` only after CPAL has released the microphone. The worker caches their
generated WAV bytes after the first successful synthesis, so repeating a
normal control does not repeatedly invoke Piper for those short phrases. Piper
itself is used through its documented CLI rather than reimplemented in Rust;
Rodio provides output-device access and WAV decoding/playback. Our code owns
queue ordering, cancellation identity, privacy limits, and the rule that the
microphone must wait for the listening cue.

This is **not echo cancellation**. Speaker audio can reach a nearby
microphone; use a selectable headset when that matters. If Piper or an output
device is unavailable, the text log remains authoritative and recording starts
with a visible warning instead of pretending audio played.

## Observability follows ownership, not dialogue

[`src/telemetry.rs`](src/telemetry.rs) adds a fifth, deliberately passive
boundary: `telemetry::Interaction`. It starts for voice mode in
`VoiceDraft::new` at the actual Enter press—before a `RecordingId` or `TurnId`
exists. Typed turns start one at dispatch. The interaction is then cloned into
the existing owners, rather than recovered from whatever happens to be active
when a worker finishes:

```text
VoiceDraft::new / TurnRuntime::start
  -> Interaction { run ID, interaction ID, root tracing span, monotonic clock }
  -> DictationSession + CpalRecorder + WhisperTranscriber
  -> VoiceRuntime photo task / CameraService capture gate
  -> TurnRuntime + OllamaClient
  -> SpeechHandle + one Piper/Rodio worker request
```

That is intentional Rust ownership, not magic task-local context. A spawned
Tokio task or `spawn_blocking` closure receives an explicit clone. The clone
contains IDs, timestamps, sizes, bounded outcome labels, and a `tracing` span;
it does **not** contain a transcript, answer, prompt, audio, image bytes, or a
path. This gives a human or exporter a coherent trace while preserving the
same controller rule: telemetry cannot submit input, authorize output, mutate
accepted history, or restart hardware.

`Interaction::stage` records a named execution window. It uses `Instant` for
durations and UTC epoch milliseconds only for cross-process correlation. Its
summary keeps stage start offsets instead of adding durations, because camera,
audio, and worker scheduling can overlap. `state.stage_union_covered_ms` is a
merged union of completed or abandoned stage windows;
`state.unattributed_elapsed_ms` is the honest remainder, not a made-up “server
queue” explanation.

The named milestones calculate only intervals with both endpoints. In voice
mode those include Enter→actual microphone recording, release→Ollama dispatch,
release→accepted text, release→final-answer software playback start, and
cancellation request→local resource cleanup. The thinking cue is its own
speech request and cannot satisfy the final-answer playback milestone.

The controller still accepts text before speech delivery completes. For
observability, `VoiceRuntime` keeps that accepted interaction indexed by the
specific `UtteranceId` until Piper/Rodio reports played, cancelled, synthesis
failure, or playback failure. Thus a text answer accepted into history and a
failed spoken delivery are distinguishable instead of being falsely reported
as the same outcome.

Ollama's `/api/chat` response may carry server statistics. `think::OllamaAnswer`
keeps `total_duration`, `load_duration`, prompt/eval counts and durations, and
`done_reason` as optional values. The server's units are nanoseconds; the
telemetry layer converts them explicitly to seconds for OpenTelemetry metrics
and milliseconds in JSONL. `eval_count / eval_duration` is emitted only when
the duration is positive. A non-streaming request has no truthful first-token
latency, and the local API does not expose a server queue depth, so both are
reported unavailable rather than inferred by subtraction.

The readable terminal logger and structured output are intentionally separate.
The logger shows recognized/authorized dialogue on stdout. Set
`INDEPENDENCE_TELEMETRY_JSONL` to opt into a bounded background JSONL writer;
set standard `OTEL_EXPORTER_OTLP_*` endpoint variables only when an OTLP
collector is deliberately available. See README's “Observe one interaction”
section for exact commands, fields, a synthetic trace, and privacy limits.

## Why the controller owns state

The model worker gets an owned snapshot of history, question, and photographs.
It cannot print and it cannot mutate the session. The controller stores:

- ordered accepted `Photo` values, if any;
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

Dictation and voice input need individual keys rather than submitted lines, so
they do not start the normal reader. `listen::dictation` enables Crossterm raw
mode and owns one separate polling thread. `--dictate` maps press events only.
`--voice` first negotiates keyboard enhancement reporting, then maps Enter
press to `HoldStart`, Enter release to `HoldEnd`, and distinct `P` presses to
photo requests; repeats are ignored. If the terminal cannot report event
types, voice mode fails rather than using a release timeout. Its short poll
interval lets shutdown join the thread before the raw-mode guard restores
normal terminal settings. `--voice` uses raw-mode-safe output for model
answers too; the shared `tracing` writer flushes timestamped logs to stdout
with CRLF handling instead of assuming ordinary `println!` behaves correctly
while raw mode is active.

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
another worker against a still-closing device. The Raspberry Pi adapter has a
different bounded cleanup mechanism: its `rpicam-still` child observes the
same cancellation token, then Independence kills and reaps that child. Neither
path claims that a sensor stops at an exact instant. The three-second V4L2
bound covers frame polling; V4L2 setup ioctls themselves are driver calls
without a separate timeout exposed by this backend, so a broken driver can
still misbehave there.

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

Speech has a related but separate limit. Piper runs as an app-owned local child
process. A cancellation-generation change makes the worker kill that child if
it is still synthesizing; it polls Rodio playback about every 10 ms and stops a
stale player there. That is a bounded application-owned cleanup strategy, not
a claim that an operating-system audio driver halts at an exact instant.

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

For dictation, CPAL discovers and opens the operating system's input stream;
it tells us the real channel count, sample format, and sample rate. Our code
uses CPAL's `FromSample` conversion and averages each input frame to mono.
Whisper requires 16 kHz mono f32 PCM, so Rubato's offline FFT resampler changes
the actual device rate instead of merely relabeling samples. `whisper-rs`
builds on whisper.cpp and performs the English CPU transcription. Our code
chooses explicit limits, manages stream/task lifetimes, and decides whether a
result is still current. `--dictate` displays accepted text only; `--voice`
hands the already accepted final text directly to `Input::Ask` or
`Input::AskWithPhotos`, without interpreting it as an application command.

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

For the integrated bridge, use a running local Ollama server and launch:

```bash
cargo run -- --voice
```

After completing the separate Piper/HAL setup in `README.md`, first run:

```bash
cargo run -- --audio-devices
cargo run -- --speak-test
```

The first command only lists outputs. The second deliberately speaks one fixed
sentence through the system-default device. It is the real speaker check and
does not open a microphone, contact Ollama, or capture a photo. If the log
names a different desired output, rerun it with `--audio-output 'Exact id'`.

For `--voice`, hold Enter and wait for `i'm listening` before asking “What is
the primary gas in Earth's atmosphere?”; then release Enter. The microphone is
not open during that cue. Partial log lines may appear before the final
transcript, `i'm thinking`, and the text answer. Repeat while pressing `P`
twice after capture begins and before release to attach two stills in press
order. While an answer is pending, hold Enter again: its old model result and
old speech must become ineligible. Press Esc during the listening cue, a held
draft, final recognition, synthesis/playback, or inference to discard that
work. `cargo run -- --dictate` remains the separate transcript-only check; it
should display speech but never contact Ollama or speakers.

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
cancelled/stale/empty transcripts, real hold/release transitions, replaceable
partial text, ordered multi-photo submission, late photo/partial results,
follow-up history, literal slash-looking text, and old-answer invalidation when
another recording starts. They do not prove that the physical microphone works, that
ALSA/PipeWire picks the desired device, that Whisper recognizes a particular
voice, or that a real local model answers quickly. Those are the separate
manual checks above; no model weight, microphone, Ollama server, or camera is
required for the ordinary suite.

Speech-worker tests use fake synthesizers and fake players. They verify queue
order, fixed-cue caching, synthesis/playback failure reporting, and generation
cancellation without Piper, a model file, or a real speaker. Voice-runtime
tests separately verify that recording does not open before the listening cue,
that a late cue after release/Escape cannot reopen it, and that `P` during the
cue cannot activate a camera. They do not prove that a physical speaker works;
only `cargo run -- --speak-test` can do that on the selected machine.

## Latest milestone: Raspberry Pi hardware adapters and keyboard-as-buttons

This milestone changes **how physical intent enters** the existing voice
runtime, not who owns conversation state. `Controller` still owns accepted
history and `TurnId` authorization. `VoiceRuntime` still owns one unfinished
hold-to-talk draft. The new small boundary lives in
[`src/listen/controls.rs`](src/listen/controls.rs):

```text
one physical event
  -> VoiceControlEvent
       TalkPressed | TalkReleased | PhotoPressed | Cancel
  -> VoiceInputEvent (adds source-loss/recovery facts)
  -> VoiceRuntime::handle_control_event
  -> existing DictationInput / existing voice draft lifecycle
```

There are now two adapters that produce those same facts:

```text
terminal profile/default                  raspberry-pi button profile
------------------------                  ---------------------------
listen/dictation.rs                       listen/evdev.rs
Crossterm raw terminal                    selected /dev/input node
Kitty key press/release                   Linux EV_KEY values
        |                                         |
        +------------ identical events -----------+
```

`TerminalVoiceInputSource` is deliberately still the only raw-terminal owner
for ordinary `--voice`. `EvdevButtonInputSource` does not create a terminal
reader at all. It opens the exact path passed to `--input-device`, checks that
Enter, `P`, and Escape are supported, sets nonblocking I/O, and takes an
exclusive Linux grab. That makes an attached keyboard a controller rather than
a text keyboard. It ignores autorepeat and all unrelated keys. On
`SYN_DROPPED` or device loss, it sends `SourceLost`; `VoiceRuntime` follows its
ordinary Escape cancellation path, so an uncertain held recording cannot
submit. The adapter retries only the same explicit path, emits
`SourceRecovered` if it comes back, and requires a fresh press.

This is why SSH is safe in button mode:

```text
SSH terminal keys  --x--> no Crossterm reader, no stdin reader
attached keyboard  -----> evdev source -----> VoiceRuntime
SIGINT / Ctrl+C    -----> Tokio signal handler -----> normal shutdown
```

The exact input node must be chosen rather than guessed. These commands are
read-only; they do not load Whisper/Piper/Ollama, start a microphone, grab a
keyboard, or capture a photograph:

```bash
cargo run --release -- --input-devices
cargo run --release -- --audio-input-devices
cargo run --release -- --audio-devices
```

[`src/launch.rs`](src/launch.rs) parses literal options; it does not touch
hardware. [`src/profile.rs`](src/profile.rs) applies a narrow `raspberry-pi`
profile. Its precedence is:

```text
--input / --input-device / --audio-input / --audio-output
    > corresponding environment variables
    > raspberry-pi profile defaults
    > ordinary laptop defaults
```

The Pi defaults are intentionally conservative:

```text
input adapter: buttons (but an explicit --input-device is required)
Ollama model:  gemma3:1b
image contract: text-only
camera backend: rpicam-still
```

“Text-only” is an application safety contract, not a guess based on a model
name. `TurnRuntime::prepare_camera`, the voice `P` path, and
`OllamaClient::answer_with_telemetry` all reject image work before camera
enumeration, image encoding, or HTTP when it is selected. To intentionally use
a verified vision model, set both `OLLAMA_MODEL` and
`INDEPENDENCE_IMAGE_MODEL_SUPPORT=vision` for that launch. This prevents an
unverified Pi text model from receiving a photo or pretending to understand
one.

The Pi camera remains behind the existing `CameraService` interface:

```text
PhotoPressed while actual recording is active
  -> VoiceRuntime queues an ordered draft photo
  -> CameraService's one-permit capture gate
  -> RpiCamStillBackend
  -> direct rpicam-still --nopreview ... --output - child (no shell)
  -> bounded stdout JPEG + full validation
  -> child/handle released before an Ollama request can begin
  -> existing ordered in-memory Photo collection
```

The V4L2 webcam backend remains the normal laptop/default backend. For Pi
hardware, `rpicam-still` is selected only by the profile or the explicit
`INDEPENDENCE_CAMERA_BACKEND=rpicam-still` override. The adapter uses a
five-second child-process bound, a 10 MiB in-memory JPEG bound, no preview, no
disk file, and no shell. Cancellation kills/reaps the app-owned child; that is
stronger than simply aborting its async wrapper, but it still says nothing
about any unrelated camera service the operating system might own.

The ordinary test suite uses a fake event source, fake microphone/transcriber,
fake camera backends, a direct fixture subprocess, and mock Ollama. It proves
key mappings, repeat suppression, talk/photo/cancel routing, source-loss
cancellation, explicit text-only camera rejection, direct subprocess arguments,
bounded output, and child cancellation/reaping. It does **not** prove an
attached Pi keyboard, USB microphone/speaker, ARM64 Whisper/Piper installation,
or Camera Module 3 Wide. Those remain separate, documented hardware checks in
[`docs/rasberry-pi.md`](docs/rasberry-pi.md#8-optional-hardware-adapters-attached-keyboard-as-buttons).
