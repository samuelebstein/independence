# Current architecture

This document describes the current working tree, including Milestone 6.1's
non-cryptographic security foundation. It separates implemented behavior from
the future encryption/authentication design in
[security/ARCHITECTURE.md](security/ARCHITECTURE.md).

## Entry points and modes

`src/main.rs` initializes telemetry and the opt-in diagnostic subscriber, parses arguments in
`src/launch.rs`, then starts exactly one application mode.

| Invocation | Entry function | Current purpose | Does not do |
| --- | --- | --- | --- |
| `cargo run` | `app::run_with_mode` | Typed **Ephemeral Mode** question, `/camera`, and temporary follow-ups | Open microphone or speaker; load local files with `/look`; access private storage |
| `cargo run -- --dictate [--audio-input <exact device id>]` | `app::run_dictation` / `dictation::run` | One local ephemeral recording → Whisper transcript → terminal display | Call Ollama, run a spoken command, or open camera |
| `cargo run -- --voice` | `app::run_voice_with_options` | Hold-to-talk → local Whisper → main controller → local Ollama; optional Piper/Rodio speech | Automatic listening, command parsing from speech, or echo cancellation |
| `--voice --pipeline audio-to-audio` | `audio_to_audio::run` | Isolated recording-to-WAV plumbing experiment | Whisper, Piper, camera, text history, or a real audio model |
| list/test commands | `app::list_*`, `app::run_speech_test` | Inspect devices or test local speech output | Start a normal interaction |

The command parser accepts `--mode ephemeral|private`, `--profile
raspberry-pi|raspberry-pi-hailo`, `--input terminal|buttons`, selected
input/output device IDs, model/backend/pipeline choices, and an explicit
button device. `src/profile.rs` resolves explicit command-line values over
environment values and profile defaults. A Raspberry Pi profile is a
configuration choice, not a separate application.

`Ephemeral` is the default. `Private` is deliberately rejected before model or
hardware setup because Milestone 6.1 has no authentication implementation.

## Core ownership model

```text
SecurityManager (src/security.rs)
  owns lifecycle state and grants an opaque Ephemeral Mode lease
          |
          v
input adapters / hardware workers
          | facts (Events)
          v
src/controller.rs: Controller
  owns accepted history, accepted photos, active TurnId, pending camera request
          | requested side effects (Effects)
          v
src/app.rs: TurnRuntime / VoiceRuntime
  owns cancellable tasks and performs I/O
          | results (Events)
          v
Controller accepts only the still-current result
```

`SecurityManager` is separate from the controller. It models
`Uninitialized`, `SetupInProgress`, `Locked`, `Unlocked`, and
`RecoveryRequired`, but current production startup always begins
`Uninitialized`: no actual setup, PIN, recovery phrase, key, database, or
persisted lifecycle state exists. Only an Ephemeral Mode lease can be acquired
by production code. An opaque future private-storage capability cannot be
constructed by the controller.

`Controller` in `src/controller.rs` owns the only accepted **ephemeral**
`Session`: up to six
accepted conversation turns and ordered accepted `Photo` values. A worker gets
an owned snapshot and cannot mutate the session directly. `TurnId` prevents a
late result from printing or changing history after a new question, `/stop`,
`/reset`, EOF, or Ctrl+C. `RecordingId` performs the analogous job for a
microphone recording. `src/protocol.rs` holds those typed boundaries.

There is no database, account, authentication implementation, encryption key,
recovery phrase, or persistent conversation memory in the current controller.
Private Mode has no production entry point.

## Listen: terminal and microphone input

### Typed terminal input

`src/listen/mod.rs` reads terminal lines and parses typed commands. Paths are
literal data, not shell commands. In the default Ephemeral Mode, `/look` is
rejected before file I/O so an arbitrary local file cannot become temporary
model context. `/camera` requests one fresh temporary scene; `/stop`, `/reset`,
and `/quit` go through the controller.

### Local dictation and integrated voice

`src/listen/dictation.rs` owns raw terminal key input; the Linux button mode
instead uses `src/listen/evdev.rs`. Both are adapted to the same talk/photo/
cancel events. In button mode the selected evdev device is exclusively grabbed
and terminal input is not started.

`src/dictation/audio.rs` opens a CPAL stream only for an explicit recording.
Its callback performs sample-format conversion, mono downmixing, and bounded
in-memory buffering; it does not run recognition or controller logic. After
capture, Rubato prepares Whisper's 16 kHz mono input. `src/dictation/transcribe.rs`
uses `whisper-rs`/whisper.cpp with cooperative cancellation. The
`DictationController` accepts a `TranscriptReady` event only for the current
`RecordingId`.

In `--dictate`, accepted text ends at terminal display. In `--voice`,
`VoiceRuntime` receives the already accepted transcript and constructs literal
question text for the main controller. A spoken string such as `/camera` or
`/quit` is never given terminal-command authority. Voice mode has optional
display-only partial recognition while a button is held; the final accepted
transcript is the submitted text.

## See: photo files and explicit camera capture

`src/see/mod.rs` validates explicit JPEG or PNG data with bounded reads,
dimensions/allocation, and in-memory `Photo` bytes. That file-loading path is
currently exercised only by private-controller test fixtures: production
Ephemeral Mode rejects `/look` before any path is opened. The camera path uses
the same validation for a fresh in-memory still.

`src/see/camera.rs` offers a shared one-shot camera service. The standard Linux
path inventories and captures from V4L2; it picks a conservative supported
format, discards a bounded number of warm-up frames, validates one JPEG, and
releases the camera before model inference. The Raspberry Pi adapter invokes
the installed `rpicam-still` executable directly (not through a shell), reads a
bounded JPEG from stdout, and kills/reaps the process on cancellation or
timeout. Camera use is always explicit: typed `/camera` or a `P` button press
while an approved voice recording is active.

For typed `/camera`, a proposed photo remains staged until the associated
answer succeeds. Failure or cancellation preserves the accepted scene.
Voice-captured photos are retained only after the associated answer is
accepted. Follow-up questions receive the ordered accepted photos for this
one Ephemeral session.

## Think: local model integration

`src/think/mod.rs` contains `OllamaClient` and the `/api/chat` request shape.
Despite its historical name, it is a strict loopback-only client for the small
Ollama-compatible protocol Independence uses. It sends an editable system
prompt from `src/think/teacher.txt`, accepted history, current question, and
base64 image bytes when present. Responses are non-streaming. The ordinary
default is `http://127.0.0.1:11434` and `qwen3-vl:4b-instruct`; it disables
redirects and ambient proxy routing, and has explicit 2-second connect and
120-second request timeouts.

At voice startup, the Ollama adapter obtains and caches model capabilities via
`/api/show`. Image capture is rejected when metadata is absent or does not
advertise image input. This is not a general security attestation of a model.

An SSH tunnel can make a remote owner-controlled Ollama server appear on a
local loopback port, but it is still a connection to another machine. There is
no cloud fallback in the application.

`ModelBackend::HailoVlm` reuses that deliberately narrow client contract but
selects `tools/hailo_vlm_bridge.py` at `127.0.0.1:11435` by default. The bridge
is a separate local Python process: it validates the same request, advertises
`vision`, decodes up to four current images in capture order, converts each for
Hailo Qwen2-VL, and returns the same non-streaming response shape. It owns neither
conversation history nor camera access, clears Hailo context after each
generation, and binds numeric loopback only. Its optional `hailo_vlm.*` JSONL
metadata excludes prompts, image data, and answers. See
[raspberry-pi-hailo.md](raspberry-pi-hailo.md) for installation and the
hardware verification boundary.

## Terminal interface, diagnostics, and optional local audio

`src/interface.rs` owns all human-facing stdout rendering. It writes concise
status, instructions, final transcripts, authorized answers, device/camera
lists, and actionable errors without timestamps, log levels, IDs, or tracing
detail. Its raw-mode-safe writer flushes each message. Final transcripts and
answers are intentionally visible in terminal scrollback and redirected stdout;
that remains an important privacy boundary.

`src/speak/mod.rs` configures a separate opt-in diagnostic subscriber. It
writes sanitized operational metadata to stderr only when
`INDEPENDENCE_LOG=error|warn|info|debug` is selected. It has no dialogue
target and does not receive text, prompts, responses, transcripts, tool data,
memory contents, or media. See [logging.md](logging.md).

In voice mode, `src/speak/worker.rs` owns one bounded, ordered synthesis and
playback queue. It tags work with a generation and `TurnId`/draft owner so stale
audio cannot play after cancellation. `src/speak/tts.rs` invokes a configured
local Piper CLI without a shell, passes text by stdin, and receives a WAV in
memory. `src/speak/playback.rs` uses Rodio/CPAL to play that WAV through the
explicitly selected or system-default output device. There is no echo
cancellation: a speaker can be heard by a microphone.

## Observability and stored material

`src/telemetry.rs` creates a correlation/timing record before a voice turn is
assigned a `TurnId`. It can write redacted JSON Lines when
`INDEPENDENCE_TELEMETRY_JSONL` is set and can export OpenTelemetry only when an
OTLP endpoint is explicitly configured. Its intended schema omits transcript,
prompt, answer, image, audio, credential, and path fields. Redaction/filtering
is defense in depth, not a substitute for an encrypted private store. Telemetry
files are optional and are not currently encrypted or rotated by the
application.

Current data lifetime:

| Data | Current location/lifetime | Important limit |
| --- | --- | --- |
| Accepted history and accepted photos | Process memory in Ephemeral `Controller` | Starts empty and is cleared on orderly runtime shutdown or `/reset`; not encrypted while process runs |
| Captured microphone samples and Piper WAV | Bounded process memory | Released after work/cancellation; no audio file written by the app |
| `/look` source | No production Ephemeral Mode access | File loading is blocked before I/O; future Private Mode is not implemented |
| Camera still | Process memory | App does not save it, but the model request contains its bytes |
| Whisper/Piper/Ollama/Hailo model artifacts | User-managed `local-data` or external runtime locations | Not application-owned private data storage |
| Terminal interface | Terminal scrollback/stdout/redirection | Current dialogue text is deliberately shown and can be retained outside app control |
| Opt-in diagnostics | stderr or a developer-selected redirected file | Contains operational metadata only; it is not an archive of dialogue |
| Optional JSONL/OTLP telemetry | Explicitly configured destination | Application redacts content, but files/exporters are not encrypted or retention-managed by the app |

This is why the security milestone does **not** describe the current system as
locked or encrypted. See [security/THREAT_MODEL.md](security/THREAT_MODEL.md).

## Platform-specific integrations

| Concern | Laptop/Linux default | Raspberry Pi profile | Raspberry Pi Hailo profile |
| --- | --- | --- | --- |
| Interaction input | Crossterm raw-key terminal controls | Explicit selected Linux evdev keyboard used as buttons; no stdin reader; five-second Talk hold gates Ephemeral Mode entry | Same physical-button adapter and entry gate |
| Microphone/output | CPAL/Rodio selected device or system default | Same libraries and device selectors; usually explicit USB device IDs | Same libraries and selectors |
| Camera | V4L2 one-shot still capture | `rpicam-still` one-shot JPEG adapter | Same `rpicam-still` adapter; bridge allows up to four current images in capture order |
| Default model runtime | local Ollama at `11434` | `gemma3:1b` for text-only experiments | Hailo VLM bridge at `127.0.0.1:11435`, alias `qwen2-vl:2b` |

The profile does not prove hardware availability, a secure enclave, a TPM,
trusted boot, or model suitability. Device capability and permissions are
runtime facts. Both platforms now use the same process-local `SecurityManager`
policy, but neither has authentication, secure hardware, or protected private
storage yet.

## Tests and verification

The normal Rust suite uses fakes and controlled completions rather than a live
camera, microphone, Ollama model, Piper executable, or GPU. It covers controller
authorization/stale results, staged scenes, request shape and model errors,
photo validation, dictation state and resampling, voice cancellation, speech
queue order, telemetry redaction/timing, audio-to-audio fixture plumbing, and
platform adapters. Python bridge contract tests use a fake VLM and do not load
Hailo Apps or a HEF. A few real hardware captures are intentionally ignored
manual tests. Standard repository checks are documented in the README:
`cargo fmt --check`, `cargo check`, `cargo test`,
`cargo clippy --all-targets -- -D warnings`, and `git diff --check`.

## Planned, not implemented

The following do not exist in the application at this revision:

- user identity, PIN entry, biometric or hardware-key authentication;
- a production transition to `Unlocked`, persistent lifecycle state, or key
  clearing lifecycle;
- encrypted conversation/history/photo/audio storage;
- a master data-encryption key, recovery phrase, encrypted backups, or
  cross-device restoration;
- persistent personal memory;
- automatic camera permission or continuous listening;
- a guarantee that local processes, the OS, a model, a terminal, or a
  configured exporter cannot observe private material.
