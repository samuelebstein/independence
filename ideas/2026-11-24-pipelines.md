# choose your pipeline

*Idea, not an approved design. Written down 2026-11-24 while making the Pi's
`--check` explain itself.*

## the idea

Independence is turning into a set of pipelines: a chain of local models and
devices that together do one thing well. Hearing → thinking → speaking is one
chain. Spanish tutoring is the same chain with a different Whisper model, a
different voice, and (eventually) a different prompt. Photo questions add the
camera and a vision model.

So what if the interface were literally "choose your pipeline"?

- **Your current installation.** A screen (a TUI first) that shows what this
  machine has: which models are downloaded, which devices are attached, what
  each one is for, and whether it works right now. `--check` is the seed of
  this: green for working, red for what's missing, and a sentence on what each
  piece does.
- **Pipelines as goals.** "Tutor Spanish", "explain what I'm looking at",
  "dictate a note". Each goal is a chain of models and devices. Pick one and
  Independence runs it.
- **Know the hardware.** Every model has system requirements (memory, CPU/GPU/
  NPU, speed on a given board). Independence knows them, so it can say "this
  Pi + Hailo can run Spanish tutoring" or "this needs the laptop".
- **Everything on the image, not everything at once.** A bootable image could
  carry all the models and code. Only some fit in memory or run fast enough at
  the same time, and that's fine: the screen shows which pipelines this
  hardware can run *now* and which it could run on a better device.
- **Many devices.** One device is great at speech, another has the
  accelerator for vision. Pipelines could span devices.
- **The map.** Eventually a matrix: open models on one side, goals on the
  other, coloured lines between them for "good at this", "works", "too slow
  here". Interactive. A picture of what open models can do for you, offline,
  on hardware you own.

## what exists today that points this way

- Pipelines: `transcribe-and-speak` and `audio-to-audio` (`--pipeline`).
- Backends: `ollama`, `hailo-vlm`, `fixture` (`--backend`).
- Hardware profiles: `raspberry-pi`, `raspberry-pi-hailo` (`--profile`).
- Swappable parts: `--tts-model`, `--whisper-model`, `--whisper-language`.
- `scripts/run-raspberry-pi-hailo.sh --check`: an installation report for one
  hardware profile.

## open questions

- What is a pipeline, exactly: a named file listing its parts and
  requirements? How small can that be before it becomes a framework?
- Where do requirements and measured speeds come from? Measured on real
  devices (like Milestone 8's tokens/s), not copied from model cards.
- Privacy: pipelines that span devices send audio, photos, or text between
  them. That needs the same care as any other network path.
- The one-boot setup: the same report could drive downloads ("Spanish
  tutoring needs ggml-base.bin and a Spanish voice; get them?").
