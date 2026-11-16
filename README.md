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
