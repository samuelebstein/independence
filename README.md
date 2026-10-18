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

It also has two explicit local microphone modes:

- `--dictate` records one short clip, transcribes it with local Whisper, and
  displays the text only. It never contacts Ollama or invokes a command.
- `--voice` uses that same bounded recording flow, displays an accepted
  transcript, then asks the local model to either answer or request one
  photograph. It supplies any accepted still photograph as context before
  considering an additional view. A requested photo names the missing visible
  detail and needs a second explicit spoken reply that the local model
  interprets in context before Independence captures anything. Spoken `/camera`
  and `/quit` text is never parsed as a terminal command.

The default observation mode (`cargo run`) does not access a microphone. No
current mode persists a conversation, streams tokens, captures a camera image
without a current permission decision, or uses a cloud model.

## Start here

Read these files in this order:

1. [`VISION.md`](VISION.md) — why the project exists.
2. [`src/main.rs`](src/main.rs) — the tiny executable entry point.
3. [`src/app.rs`](src/app.rs) — runs input, worker tasks, and shutdown.
4. [`src/dictation/mod.rs`](src/dictation/mod.rs) and
   [`src/dictation/controller.rs`](src/dictation/controller.rs) — the separate
   dictation event loop and state machine.
5. [`src/controller.rs`](src/controller.rs) — the session state machine and
   result-acceptance rule.
6. [`src/think/mod.rs`](src/think/mod.rs) and
   [`src/think/teacher.txt`](src/think/teacher.txt) — Ollama transport and
   editable teaching guidance.
7. [`LEARNING.md`](LEARNING.md) — a walkthrough of one request and the Rust
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

## Dictation and voice modes

Both microphone modes use local CPAL capture and local Whisper recognition.
They share the recording controls below, but have intentionally different
destinations for an accepted transcript. `--dictate` stops at terminal display
and has no Ollama or camera dependency at runtime. `--voice` sends an accepted
nonempty transcript to the main controller as voice input; it does not parse it
with the terminal command parser. The controller either accepts a structured
text answer or starts the narrow, permission-gated still-photo flow described
below.

### One-time local prerequisites

On Linux Mint, CPAL's Linux microphone backend needs ALSA development headers.
Run this in your own terminal, from any directory:

```bash
sudo apt-get install libasound2-dev
```

This asks your package manager to install one system-wide development package
(and may use the network); it changes the system but does not open the
microphone or alter this repository. A successful install lets
`pkg-config --modversion alsa` print an ALSA version. Independence never runs
this command itself.

Dictation also needs one local Whisper model file. The CPU starting point is
the English `ggml-base.en.bin` file published in the
[whisper.cpp model repository](https://huggingface.co/ggerganov/whisper.cpp/tree/main).
It is about 148 MiB. Download it deliberately—using a browser or the command
below—from this repository root:

```bash
mkdir -p local-data/whisper
curl -L -o local-data/whisper/ggml-base.en.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin
```

The first command creates an ignored local-data directory. The second uses the
network and writes the model there; it does not modify source code or contact
Ollama. The default path is exactly `local-data/whisper/ggml-base.en.bin`.
To keep a model elsewhere, set `INDEPENDENCE_WHISPER_MODEL` only for the launch
command. The application does not download model weights automatically.

### Launch and controls

From the repository root, after the model file exists, start transcript-only
dictation with:

```bash
cargo run -- --dictate
```

Cargo compiles the package (writing `target/`) and starts raw-key dictation
mode. At READY it prints the selected microphone and Whisper model path. CPAL
chooses the system default input by default. To select a particular input by
its exact CPAL device name, use an environment variable for one launch:

```bash
INDEPENDENCE_MICROPHONE='Exact microphone name' cargo run -- --dictate
```

That command changes neither the default device nor shell configuration. If
the requested name is unavailable, READY reports the problem and recording
does not start.

To use the speech-to-controller bridge, first ensure the same local Ollama
setup described above is running, then launch:

```bash
cargo run -- --voice
```

This starts the same raw-key recording interface but constructs the normal
local Ollama runtime as well. It still uses the default
`qwen3-vl:4b-instruct` unless `OLLAMA_MODEL` is set for that one command. For
example, this selects both an input device and a model without changing system
defaults or shell configuration:

```bash
INDEPENDENCE_MICROPHONE='Exact microphone name' \
OLLAMA_MODEL=qwen3-vl:4b-instruct \
cargo run -- --voice
```

| State | Control | Result |
| --- | --- | --- |
| READY | Enter | Opens the selected microphone and begins one recording. In `--voice`, it first cancels an outstanding model decision/answer—except when an exact photo-permission question is waiting, when it records that reply instead. |
| RECORDING | Enter | Stops and releases the microphone, then begins local transcription. |
| RECORDING | Esc | Discards the clip and waits for microphone cleanup. |
| RECORDING | 30 seconds | Automatically stops and begins transcription. |
| TRANSCRIBING | Esc | Requests cooperative cancellation; no late text is displayed. |
| READY in `--voice` | Esc | Cancels an outstanding model decision/answer or abandons a pending permission request while preserving accepted conversation. |
| Any state | Ctrl+C | Cancels owned work, restores the terminal, and exits. |

Repeated Enter presses outside READY are ignored. The raw-key reader is the
only terminal input owner in either microphone mode; the normal line reader is
not running. The microphone stream is opened only after Enter and is dropped
before resampling or Whisper inference begins. Audio lives only in bounded
memory and is discarded after transcription or cancellation; Independence
writes no recordings to disk.

In `--voice`, Whisper has no command or camera authority. After
`DictationController` accepts the current `RecordingId`, the runtime displays
the text and sends `Event::VoiceTranscript` to the main controller. If no
permission is pending, the controller gives a separate `DecisionId` to a local
structured model request: it can return a text answer or request a new photo.
If permission is pending, that same accepted text becomes a structured
permission-interpretation request—not `Input::Ask` and not a slash command.
Only the controller can consume a current `yes` result and start the existing
camera turn with the original question. A cancelled/stale recording or decision
cannot bypass those ID checks.

### Spoken photo permission in `--voice`

For a spoken question, the local Qwen model returns a schema-validated choice:
`respond` with a text answer, or `request_photo` with a short,
user-facing `needed_visual_evidence` field such as “a closer view of the leaf
edges.” The latter does **not** turn the camera on. Independence first performs
the same no-stream V4L2 inventory and selection policy used by `/camera`: it
proceeds only when exactly one usable camera can be selected without guessing a
device. It then shows a specific question naming that camera and the missing
visual detail.

The decision call receives the accepted conversation and, when one exists, the
same accepted still image. The prompt explicitly treats that image as a past
still—not live video—and asks the model to answer from it first. A later
`request_photo` means an additional/different view, not a duplicate or a
replacement. The model is also told that Independence has no system clock,
calendar, GPS, web, or live sensors: a general “what time and day is it?”
should receive an honest limitation rather than a speculative camera request.
These are local-model instructions, not a proof that every model response will
be wise; the controller still enforces the capture/permission boundary.

Record the reply with Enter → speak → Enter. A second schema-constrained local
model call receives the exact shown question, the proposed one-still action,
the selected camera, and the accepted reply. It returns `yes`, `no`, or
`unclear`; Independence validates that result again before acting. This is not
a phrase list: arbitrary natural language is model-interpreted in that exact
context. Conditional replies, questions, or a change of subject are supposed
to become `unclear`, which repeats the permission question without capturing.

`yes` authorizes one capture now and starts the existing staged
camera-and-answer path with the **original** question. `no` asks the normal
local model to continue without a new photo. A malformed, failed, cancelled,
or stale decision never activates the camera. The app only controls its own
HTTP task; cancelling it cannot prove that the separate Ollama server stopped
computing immediately.

Try this manual sequence:

1. Start `cargo run -- --dictate`, press Enter, say “The creek is loud after
   last night's rain,” then press Enter. Only the resulting transcript should
   appear.
2. Press Enter again, speak a few words, then press Esc. No text from that
   recording should appear.
3. Press Enter a third time, say a different short sentence, then press Enter.
   Only that third transcript should appear after the earlier successful one.

For `--voice`, try this separate sequence:

1. Start `cargo run -- --voice`, press Enter, ask “What is the primary gas in
   Earth’s atmosphere?”, then press Enter. The transcript appears first, then
   the local model's text answer.
2. At READY, press Enter again, ask “Why does that matter for life?”, then
   press Enter. This is a follow-up using the accepted conversation.
3. While the second answer is still pending, press Enter to begin a third
   recording. The old answer must not appear. Press Esc to discard that third
   recording; the first accepted exchanges remain intact.

To try the permission loop, first use normal `cargo run` and `/cameras` to
confirm that exactly one usable camera is listed. Then start `cargo run --
--voice`, ask about an object in front of the camera, and wait. The model may
answer from text alone; if it instead asks to take a photo, the camera should
still be off. Press Enter, say an unambiguous natural reply such as “I approve
that one picture now,” and press Enter. It should capture one still, release
the camera, and answer the original question. Try again with “I would rather
continue without a new photo” to verify that no capture occurs, or ask a
condition/question to see the clarification path.

After a successful voice capture, ask “What was in the photo?” and then “What
visible feature should I inspect first?” Each follow-up should be answered from
the accepted still and should **not** activate the camera again. If the model
does ask for another photo, the permission question should say that it already
has a still and name the different visual detail it needs. A response such as
“I guess so” or “You could if you want” remains deliberately conservative
`unclear`; say plainly that you authorize one still now to approve.

Current limits: English `base.en` recognition runs on CPU; no device list UI,
voice activity detection, streaming partial text, saved audio, microphone
permissions workflow, text-to-speech, or continuous recording exists. The
voice photo flow has no spoken camera-selection/disambiguation UI: if none or
more than one usable V4L2 camera is found, it reports that it cannot safely
propose a capture. Voice questions cannot use typed `/look` or `/camera`
commands; keep using the default keyboard mode for those. Whisper cancellation
is cooperative: Esc prevents a result from being accepted, while the native
recognizer exits at its next supported abort callback point.

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
git diff --check
```

`fmt --check` verifies formatting without changing source. `check` type-checks
without producing a runnable binary. `test` uses Wiremock loopback servers,
synthetic image fixtures, synthetic audio, and test doubles—no GPU, installed
speech model, microphone, real photo, camera, or remote server is required.
`clippy` treats lint warnings as failures. `git diff --check` detects whitespace
errors in this working change. Cargo may need network access the first time it
downloads declared crates; subsequent runs normally use its local cache.

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
    dictation.rs raw-key adapter used by --dictate and --voice
  dictation/     recording-ID controller, CPAL capture, Whisper, accepted-text boundary
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

- Default observation mode uses keyboard input only. `--dictate` is
  transcript-only; `--voice` can use local structured decisions to request one
  consented still photo, but has no wake words, text-to-speech, camera-selection
  dialogue, or continuous recording.
- One explicit JPEG/PNG or one explicitly consented `--voice` camera still at a
  time; no live view, continuous vision, background capture, or unrestricted
  camera/tool use.
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

## Teach Sam About the Latest Milestone

> **Maintenance convention:** After each completed Independence milestone,
> update this section alongside the relevant README and learning documentation.
> Keep it grounded in the current code, show what changed, and distinguish the
> implemented system from the next proposed milestone.

This is a map of what exists **now**. It shows three ways into the same local
learning system: typed questions, transcript-only dictation, and integrated
voice questions.

```text
                         cargo run
                             |
                             v
       keyboard line -> listen/mod.rs -> Controller -> TurnRuntime
                                              |             |
                                              |             v
                                              |      see + think workers
                                              |             |
                                              v             v
                                      accepted state <- TurnFinished
                                              |
                                              v
                                          speak/mod.rs
```

### Choose a mode

```mermaid
flowchart TD
    Main[src/main.rs] --> Typed["cargo run"]
    Main --> Dictate["cargo run -- --dictate"]
    Main --> Voice["cargo run -- --voice"]

    Typed --> TypedApp["app::run()"]
    Dictate --> DictationApp["app::run_dictation()"]
    Voice --> VoiceApp["app::run_voice()"]

    TypedApp --> LineInput["listen/mod.rs\nstdin line reader"]
    DictationApp --> RawInput["listen/dictation.rs\nraw Enter / Esc / Ctrl+C"]
    VoiceApp --> RawInput
```

| Mode | What it does | What it deliberately does not do |
| --- | --- | --- |
| `cargo run` | Typed questions, `/look`, `/camera`, accepted photo follow-ups | Use microphone input |
| `cargo run -- --dictate` | Record → local Whisper → display transcript | Contact Ollama or execute spoken commands |
| `cargo run -- --voice` | Record → local Whisper → structured local answer-or-photo decision → text answer, or one permission-gated still photo | Parse spoken commands, guess among cameras, or capture without a current model-interpreted approval |

### Typed question path

```mermaid
sequenceDiagram
    participant K as Keyboard
    participant L as listen/mod.rs
    participant C as controller.rs
    participant R as app.rs TurnRuntime
    participant T as think/mod.rs
    participant S as speak/mod.rs

    K->>L: "Why is the sky blue?"
    L->>C: Event::Input(Input::Ask)
    C->>R: Effect::StartTurn(TurnRequest + TurnId)
    R->>T: OllamaClient::answer(history, question, image?)
    T-->>R: answer or error
    R->>C: Event::TurnFinished(TurnId, result)
    C->>S: Effect::Display(accepted output)
```

The important boundary is in [`src/controller.rs`](src/controller.rs): workers
cannot print answers or mutate history. They return a result, and the
controller accepts it only if its `TurnId` is still the active turn.

```text
new question, /stop, /reset, EOF, or Ctrl+C
                    |
                    v
             active TurnId is removed
                    |
                    v
      old TurnFinished arrives later -> ignored safely
```

### Photograph and camera path

```text
/look path/to/photo.jpg | question
        |
        v
listen/mod.rs parses a literal path; it never invokes a shell
        |
        v
Controller stages a proposed new scene
        |
        v
app.rs loads + validates Photo in src/see/mod.rs
        |
        v
think/mod.rs base64-encodes bytes for Ollama /api/chat
        |
        v
answer succeeds and is still the active TurnId
        |
        v
Controller replaces accepted photo + starts that scene's history
```

```text
/camera [id] | question
        |
        v
Controller asks TurnRuntime to preflight selection
        |
        v
src/see/camera.rs lists/selects one V4L2 device
        |
        v
one bounded capture -> validated in-memory JPEG -> device released
        |
        v
same Ollama request and accepted-scene rule as /look
```

A failed, cancelled, missing, or invalid new photo never replaces the last
accepted scene. Ordinary follow-up questions retain that accepted photo without
reopening a camera.

### Dictation-only path

```mermaid
flowchart LR
    Key["Enter / Esc / Ctrl+C"] --> Raw["listen/dictation.rs"]
    Raw --> DC["dictation/controller.rs\nDictationController"]
    DC --> Mic["dictation/audio.rs\nCPAL recording"]
    Mic --> Resample["Rubato: mono 16 kHz PCM"]
    Resample --> Whisper["dictation/transcribe.rs\nwhisper-rs / whisper.cpp"]
    Whisper --> Ready["TranscriptReady { RecordingId, text }"]
    Ready --> DC
    DC --> Accepted["AcceptedTranscript"]
    Accepted --> Display["speak::emit_dictation"]
```

`RecordingId` protects the microphone/Whisper side. A completion is accepted
only when it belongs to the current recording. A cancelled or stale result is
discarded rather than displayed.

In `--dictate`, the path stops at **Display**. There is no Ollama client, main
controller, command parser, or camera service in that mode.

### Integrated voice path

`--voice` reuses the same microphone and transcription machinery, then gives
the main controller—not Whisper or the model worker—the decision and permission
state:

```mermaid
flowchart TD
    A["Whisper completion"] --> B["DictationController\nvalidates RecordingId"]
    B --> C["DictationSession\nAcceptedTranscript { text }"]
    C --> D["VoiceRuntime in app.rs"]
    D --> E["Event::VoiceTranscript(text)"]
    E --> F["Controller allocates DecisionId\nStartDecision: ChoosePhoto"]
    F --> G["OllamaClient::decide_photo\naccepted history + accepted still image?\nJSON-schema result"]
    G --> H{"respond or request_photo?"}
    H -->|respond| I["Controller accepts text/history\nraw-mode-safe output"]
    H -->|request_photo + needed_visual_evidence| J["No-stream V4L2 selection\nno capture yet"]
    J --> K["PermissionQuestion\nexact action + selected camera + missing detail"]
    K --> L["next accepted spoken transcript"]
    L --> M["StartDecision: InterpretPermission"]
    M --> N{"yes / no / unclear"}
    N -->|yes| O["StartTurn original question\nProposeCamera"]
    O --> P["capture once -> Ollama answer\naccept new scene only on success"]
    N -->|no| Q["StartTurn original question\nwithout a new photo"]
    N -->|unclear| K
```

An accepted `Photo` is cloned as an `Arc`-backed snapshot, so each voice
follow-up reaches `think::OllamaClient::decide_photo` with the same bytes; it
does not reopen the camera. The decision prompt distinguishes that accepted
still from a live view and asks Qwen to answer from it first. If Qwen still
requests a photo, its schema must name `needed_visual_evidence`; the controller
then describes the action as one **additional** still, not a replacement of the
accepted scene.

`VoiceRuntime` deliberately constructs `Event::VoiceTranscript(text)`, not
`Input::Ask(text)`. This lets the controller distinguish a new spoken question
from a reply to an exact pending permission question, while retaining the
dictation controller's `RecordingId` protection:

```text
spoken "/camera 0 | take a photo"
        |
        v
untrusted transcript data sent to the controller/model
        |
        +--> never passed to listen::parse_line
        +--> never activates camera hardware
        +--> never acts like /quit or /reset
```

While a local decision or final answer is thinking, dictation is already back
at READY. Enter normally starts another question recording and cancels old
model work first. When an exact permission question is pending, Enter instead
preserves that request and records its reply. Esc at READY abandons the pending
request or cancels model work; Esc during recording discards only that clip.

### State ownership at a glance

```text
Controller (src/controller.rs)
  owns: accepted history/photo, active TurnId and DecisionId,
        camera preflight, and the exact pending permission request
  decides: whether a decision, capture, or model result can alter state or print

DictationController (src/dictation/controller.rs)
  owns: recording/transcription phase and current RecordingId
  decides: whether a Whisper completion is current enough to become text

TurnRuntime (src/app.rs)
  owns: active answer/decision tasks and their CancellationTokens
  performs: photo loading, camera work, structured decision requests,
            ordinary Ollama requests, and task cleanup

DictationSession (src/dictation/mod.rs)
  owns: one recording handle and one Whisper worker
  performs: microphone/recognizer lifecycle and cleanup
```

The controllers make decisions; runtimes perform I/O. Neither worker is allowed
to independently update conversation history or print an unapproved answer.

### Relevant file structure

```text
src/
  main.rs                  selects typed, --dictate, or --voice mode
  app.rs                   application event loops and worker lifetimes
  protocol.rs              typed events, effects, TurnId/DecisionId, and requests/results
  controller.rs            accepted scene/history, decision state, and output authorization

  listen/
    mod.rs                 typed terminal-line parsing for normal mode
    dictation.rs           raw-key input for --dictate and --voice

  dictation/
    mod.rs                 DictationSession and standalone dictation runtime
    controller.rs          RecordingId state machine
    audio.rs               CPAL capture, downmixing, resampling, bounds
    transcribe.rs          local whisper-rs / whisper.cpp wrapper

  see/
    mod.rs                 explicit JPEG/PNG validation and Photo
    camera.rs              Linux V4L2 discovery and one-still capture

  think/
    mod.rs                 loopback-only Ollama /api/chat client and schema validation
    teacher.txt            editable model teaching instructions

  speak/
    mod.rs                 normal and raw-mode-safe terminal output
```

### Deliberately not present yet

There is no microphone transcription in normal typed mode, TTS, wake word,
continuous vision, persistent memory, cloud fallback, arbitrary agent/tool
framework, or open-ended autonomous camera use. `--voice` can only request one
still photo through the narrow current-question/current-permission flow above.
