# Independence

Independence is a voice-first operating system for exploring the physical
world. Read the [original vision](VISION.md) for the longer explanation of
why it exists.

It is meant to help you become more capable and better able to help the people
and community around you. Your head should be up: watching, listening,
learning—not down at another screen.

Independence will never require an account, subscription, or cloud service. It
is designed to remain local, with conversations temporary and in memory by
default. Encrypted, user-controlled memory may be added later. In the future,
it may be made physically incapable of contacting the outside world—possibly
without any network adapter at all—but that hardware boundary is still
undecided and is not part of the current system.

## What exists today

This is an experimental prototype, not a finished operating system. The Rust
application can:

- hold a local voice conversation using Whisper, a local model, and Piper;
- capture a photograph only after explicit camera authorization;
- ask a local vision model about that photograph; and
- run on a Raspberry Pi with buttons, microphone, speaker, Camera Module 3,
  and Hailo-10H accelerated Qwen3 vision.

The current user session is Ephemeral Mode: unauthenticated, in-memory, and
discarded when the process ends.

## Teach Sam About the Latest Milestone

Milestone 8 attacks latency on the Hailo Pi. The model was already running at
Hailo's advertised ~4.9 tokens/s; the wait came from what we asked of it. Voice
answers now stream: each sentence is printed and spoken as soon as the model
finishes it, with the next sentence synthesized while the current one plays.
Escape or a new question stops the Hailo immediately instead of letting it
finish an abandoned answer. Voice asks for one to three short sentences, a
photo is sent only with the question that took it, the system prompt is loaded
into the bridge once at startup instead of every turn, and Whisper processes a
window sized to your question instead of 30 seconds. The bridge log now shows
prompt tokens, time to first token, output tokens, and tokens/s for every
question. See [the milestone](docs/milestones/MILESTONE_8_PI_VOICE_LATENCY.md)
and [the learning notes](LEARNING.md#latest-milestone-where-the-pi-voice-latency-went-milestone-8).

Milestone 7 made the Pi voice loop reliable and starts it immediately.
Ephemeral Mode begins at launch (no five-second hold, no `--mode` option), and
pressing ENTER opens the microphone at once: status lines such as
"Listening...", "Transcribing...", and "Asking the local model..." are shown
on screen only, never spoken. The microphone callback now hands audio
to the recording thread through a lock-free ring buffer, and the Hailo Pi
profile no longer re-transcribes while you hold ENTER, so releasing during a
photo capture no longer loses the recording; the turn waits for the photo and
sends both. Presses under 300 ms say "Recording too short", and each status
line appears once, in order, with "Ready" after the answer. On the Pi,
`scripts/run-raspberry-pi-hailo.sh` now checks the setup, starts the Hailo
bridge, and runs Independence in one command; a missing camera is a fatal
startup error for this profile. See
[the milestone](docs/milestones/MILESTONE_7_PI_VOICE_RELIABILITY.md) and
[the learning notes](LEARNING.md#latest-milestone-pi-voice-reliability-and-immediate-start-milestone-7).

## Try it locally

From the repository root, with a local Ollama server and model already
installed:

```bash
cargo run
```

For local hold-to-talk mode:

```bash
cargo run -- --voice --input terminal
```

Independence never downloads a model or falls back to a cloud service for
either command.

## Raspberry Pi setup

The intended complete setup is the Raspberry Pi with the Hailo accelerator:

1. [Base Raspberry Pi setup](docs/raspberry-pi.md) — OS, Wi-Fi, SSH, and first boot.
2. [Independence Pi setup](docs/raspberry-pi-independence.md) — build the
   application and verify buttons, audio, and Camera Module 3.
3. [HailoRT 5.3 and Qwen3 setup](docs/raspberry-pi-hailo-qwen3.md) — install
   the Hailo-10H stack, load Qwen3-VL-2B-Instruct, and test one image.
4. Run it: `cargo build --release && scripts/run-raspberry-pi-hailo.sh`
   starts the Hailo bridge and Independence together
   ([details](docs/raspberry-pi-hailo-qwen3.md#start-everything-with-one-command)).

The Qwen3 guide is the supported Hailo path. The older
[`raspberry-pi-hailo.md`](docs/raspberry-pi-hailo.md) file is retained as
historical reference for the previous Qwen2 experiment.

## Technical shape

Rust owns the application, permissions, camera lifecycle, conversation state,
and user-facing output. Local Python services are used only where the Hailo
runtime requires them. Camera images remain local and are sent to a local
inference service as in-memory bytes.

## Read next

- [Project purpose and boundaries](docs/PROJECT.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Security and privacy threat model](docs/security/THREAT_MODEL.md)
- [Learning guide](LEARNING.md)
- [Roadmap](docs/ROADMAP.md)
- [Development and milestone rules](AGENTS.md)
