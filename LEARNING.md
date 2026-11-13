# Learning walkthrough

This is a guide to the small amount of Rust architecture in the current
prototype. It explains why each boundary exists, not just what its file is
called.

## Ephemeral Mode and a `/look` request

```text
keyboard line
  -> listen parses `/look path | question` into Input::Look
  -> default Ephemeral Controller rejects it before file I/O
  -> diagnostic reaches the terminal; no photo/model worker starts
```

This is a deliberate security boundary, not a parser limitation. An arbitrary
path may point at a saved personal photograph or file. Until real Private Mode
exists, normal production launches must not load it merely because a user typed
`/look`. The bounded JPEG/PNG validator and proposed-scene path remain useful
code and test fixtures for future authenticated storage work, but are not a
production Ephemeral capability.

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
  -> interface::answer(text)               human-facing terminal output
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

The terminal interface, diagnostics, and structured telemetry are intentionally
separate. `interface.rs` shows recognized final speech and authorized answers
on stdout. Diagnostics are disabled by default; a developer can use
`INDEPENDENCE_LOG=error|warn|info|debug` to send sanitized operational metadata
to stderr. Set `INDEPENDENCE_TELEMETRY_JSONL` to opt into a bounded background
JSONL writer; set standard `OTEL_EXPORTER_OTLP_*` endpoint variables only when
an OTLP collector is deliberately available. See [docs/logging.md](docs/logging.md)
for exact commands and privacy limits.

## Why the controller owns state

The model worker gets an owned snapshot of history, question, and photographs.
It cannot print and it cannot mutate the session. The controller stores:

- ordered accepted `Photo` values, if any;
- accepted user/assistant exchanges only; and
- at most one active `TurnId`.

The key invariant is simple: a result is accepted only when its `TurnId` is
still active. Starting a new valid question, `/stop`, `/reset`, EOF, and Ctrl+C
remove the old ID first. A late completion then becomes a harmless no-op.

A future authenticated `/look` is different from a regular question. Its photo
will be only a proposal until both loading and inference succeed. On success it
would replace the accepted scene and start that new scene's history with the
accepted exchange. In current Ephemeral Mode, it stops at the earlier
file-isolation diagnostic.

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
normal terminal settings. `--voice` uses the raw-mode-safe interface writer
for model answers and status text. It writes CRLF and flushes each message
instead of assuming ordinary `println!` behaves correctly while raw mode is
active.

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
path; Ephemeral Mode should report its file-isolation policy without opening
the path or starting inference.

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
does not open a microphone, contact Ollama, or capture a photo. If the device
list names a different desired output, rerun it with `--audio-output 'Exact id'`.

For `--voice`, hold Enter and wait for **Listening...** before asking “What is
the primary gas in Earth's atmosphere?”; then release Enter. The microphone is
not open during that cue. The final transcript appears once, followed by
**Thinking...** and the text answer. Repeat while pressing `P`
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

## Previous milestone: Raspberry Pi hardware adapters and keyboard-as-buttons

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
image capability: determined at runtime by Ollama /api/show
camera backend: rpicam-still
```

`TurnRuntime::prepare_camera`, the voice `P` path, and
`OllamaClient::answer_with_telemetry` all reject image work before camera
enumeration, image encoding, or HTTP unless the selected Ollama model's cached
`/api/show` metadata advertises `vision`. To intentionally use a verified
vision model, select it with `--model` or `OLLAMA_MODEL`. This prevents an
unverified Pi text model from receiving a photo or pretending to understand
one, without an application-side image-support setting.

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
[`docs/raspberry-pi.md`](docs/raspberry-pi.md#8-optional-hardware-adapters-attached-keyboard-as-buttons).

## Latest milestone: local Hailo VLM bridge

The Hailo integration adds a **transport adapter**, not another chat
application. Hailo Apps owns the accelerator, VDevice, model loading, image
preprocessing requirements, and VLM generation. Independence continues to own
explicit camera intent, in-memory photo lifetime, accepted history, cancellation
authorization, and voice output. That split matters because a Hailo inference
exception must not get authority to capture a camera frame or append a turn.

```text
existing Controller / VoiceRuntime
  owns: accepted history, TurnId, microphone, explicit P photo request
  sends: existing stream=false /api/chat shape
                       |
                       v
ModelBackend::HailoVlm + OllamaClient transport
  owns: strict loopback endpoint and /api/show capability discovery
                       |
                       v
tools/hailo_vlm_bridge.py at 127.0.0.1:11435
  owns: request validation, ordered base64 image decode, Hailo prompt/frame
        conversion, serialized VLM generation, clear_context(), safe API metadata
                       |
                       v
Hailo Apps VDevice + Qwen2-VL HEF
  owns: accelerator/model execution
```

The existing `OllamaClient` name describes the narrow compatibility contract,
not exclusive ownership by the Ollama executable. Both local Ollama and the
bridge answer `POST /api/show` and non-streaming `POST /api/chat`, so the Rust
capability logic does not need an unsafe special case. `hailo-vlm` is explicit
in `ModelBackend`; it shares the client only because it implements the same
documented contract. The `raspberry-pi-hailo` profile selects that backend,
`qwen2-vl:2b`, `127.0.0.1:11435`, physical buttons, and `rpicam-still`.
Command-line backend/model choices still win. `INDEPENDENCE_HAILO_VLM_URL`
can override that profile endpoint; the legacy compatible `OLLAMA_URL` remains
the higher-precedence loopback override.

The bridge's image boundary is intentionally bounded. It accepts up to four
base64 images on the final user message, preserves their capture order, turns
each into Hailo's RGB 336×336 central crop, and emits a matching Hailo image
placeholder for each frame. It rejects a fifth image rather than making a
collage or silently throwing one away. It clears the Hailo VLM context after
every request; Independence resends its accepted text history in the next
request. No prompt, image, base64 value, transcript, model response, or HEF
path goes into bridge stdout or its optional JSONL telemetry. The latter records
only safe operational metadata such as route, status, duration, count, and byte
count.

The Hailo bridge is numeric-loopback-only. It needs an explicitly downloaded
HEF because model auto-download during a server start would create an invisible
network dependency. `scripts/prepare_pi_hailo_vlm.sh` makes Hailo Apps package
installation and model download opt-in flags; `scripts/run_pi_hailo_vlm_bridge.sh`
activates the Hailo Apps environment and reports the exact server address.

The Python contract tests use a fake VLM, so they establish the ordered
multi-image API mapping, vision capability record, four-image boundary, and
telemetry redaction. They do not establish that the Pi sees the HAT, that the
HEF matches HailoRT, that text-only VLM requests work on a particular release,
or that the camera/speech pipeline is physically connected. Follow
[raspberry-pi-hailo.md](docs/raspberry-pi-hailo.md) for those device tests.

## Previous milestone: separate pipeline, backend, and model choices

The new path adds a second **pipeline**, not a second application or
controller. This follow-up keeps the model runtime separate from that pipeline:

```text
TalkPressed -> CpalRecorder -> CapturedAudio { mono f32, actual rate }
TalkReleased -> Controller::Input::AskAudio -> AudioTurnRequest + TurnId
             -> AudioAnswerBackend -> AudioTurnFinished
             -> Controller authorization -> Effect::PlayAudio
             -> DirectPlaybackService -> existing RodioPlayback
```

This is intentionally unlike the default voice route:

```text
transcribe-and-speak: capture -> prepare_for_whisper(16 kHz) -> Whisper
                       -> text Controller/Ollama -> Piper -> Rodio

audio-to-audio:       capture -> direct backend -> complete WAV -> Rodio
```

The three launch choices answer different questions:

```text
--pipeline  transcribe-and-speak or audio-to-audio
--backend   ollama, hailo-vlm (Pi Hailo profile), or fixture (direct-audio test)
--model     the backend's selected model
```

The laptop/base Pi profiles default to Ollama. The `raspberry-pi-hailo` profile
selects the documented local `hailo-vlm` bridge but still does not select a
different pipeline. The fixture is deliberately model-less and is valid only
for the direct-audio route; Hailo VLM rejects that route before microphone
startup because it has no documented recording-to-WAV API.

`CapturedAudio` now retains the actual capture sample rate after CPAL has
converted/downmixed samples. That solves a small but important abstraction
problem: Whisper gets its required 16 kHz conversion at the Whisper boundary,
while a direct-audio backend can receive honest source-format metadata and make
its own documented conversion choice.

`AudioToAudioRuntime` owns the direct capture/backend/playback lifecycle.
`Controller` still owns the active `TurnId`, so workers merely return WAV bytes
and cannot play stale output themselves. Escape and a new Talk press issue
cancellation, remove controller authorization, and invalidate the direct
playback generation. The direct path has no text history, photo service,
Whisper, Piper, HAL cue, or automatic listening behavior.

For now use the fixture route:

```bash
cargo run --release -- --voice \
  --pipeline audio-to-audio \
  --backend fixture
```

It returns a known short WAV tone, proving capture → backend → playback. The
ordinary tests use synthetic audio and a fake playback device to confirm format
preservation, malformed-audio rejection, and stale-result cancellation. See
[`docs/audio-to-audio.md`](docs/audio-to-audio.md) for controls, Pi/button use,
configuration precedence, and why a backend must verify both a model's
capabilities and the request/response format before direct audio can run.

## Latest milestone: security foundation and Ephemeral Mode

Milestone 6.1 turns one security design idea into a small, testable code
boundary without pretending encryption exists. The new
[`src/security.rs`](src/security.rs) owns lifecycle policy; the controller
does not get to decide that an operating mode is authenticated.

```text
SecurityState (process-local today)        OperatingMode (current session)

Uninitialized                              Ephemeral
SetupInProgress                              - temporary
Locked                                       - allowed from Uninitialized,
Unlocked                                       Locked, RecoveryRequired
RecoveryRequired                             - does not change lifecycle

Private
  - requires Unlocked
  - no production way to enter it yet
```

The important Rust idea here is an **opaque capability**. `SecurityManager`
can return an `EphemeralSession` lease to the runtime, but no production code
can construct the future `PrivateSession` or `PrivateStorageAccess` type. That
does not encrypt anything today; it prevents a controller, UI, or command-line
flag from smuggling a fake “authenticated” session into future storage code.

Every normal application path therefore does this:

```text
main / app launch
  -> SecurityManager::new()               Uninitialized, process-local
  -> enter_ephemeral()                    opaque EphemeralSession lease
  -> Controller::new_ephemeral()          empty history + no photos
  -> normal typed/voice/camera work
  -> cancel/join owned workers
  -> Controller::clear_ephemeral_session()
  -> SecurityManager::end_ephemeral()
```

The clearing step drops application-owned references. It cannot honestly erase
all copies from the allocator, OS memory, swap, crash dumps, terminal
scrollback, device drivers, or local Ollama internals. It does, however,
prevent the *next application controller request* from receiving old history:
Ollama chat requests carry history explicitly, and a fresh/cleared controller
has none to send.

Ephemeral Mode's most visible data-isolation choice is `/look`: it is rejected
before it opens a local path. An explicit fresh `/camera` still works and
remains in memory only. Terminal transcript/answer output remains visible to
the person using the terminal; diagnostics and JSONL telemetry keep their
separate content-redaction boundaries.

On the Raspberry Pi button profile, the existing Talk/Enter evdev input is a
physical entry gesture: hold for five seconds and release before the normal
voice runtime starts. The gate opens no microphone/camera/model/Piper resource
and consumes that release, so a fresh press is required to record. This is
intentional entry, not authentication.

Try this from the repository root:

```bash
cargo run
```

You should see an uninitialized lifecycle message and then Ephemeral Mode.
Ask a question and a follow-up, exit with `/quit`, restart, and ask whether it
knows the prior question. The application sends no prior context. Then run:

```bash
cargo run -- --mode private
```

It should refuse before starting model/device work. That refusal is safer and
more honest than a placeholder PIN. The next milestone remains **6.2 only
after owner approval** of the key/recovery-format choices in the security
documents.
