# Raspberry Pi 5: text-only local Independence

This is one complete installation path for running Independence with a
keyboard, terminal text output, a local Ollama server, and a small local text
model. It intentionally does **not** set up a microphone, speaker, camera,
Piper, HAL voice files, or Whisper weights.

## Reported working configuration

These are the reported working conditions for this guide, not universal
minimum requirements or a guarantee for every Raspberry Pi installation.

| Item | Reported working value |
| --- | --- |
| Computer | Raspberry Pi 5 with 4 GiB RAM and an approximately 32 GB microSD card |
| Operating system | Debian GNU/Linux 13 (trixie), ARM64/aarch64 |
| Kernel | `6.18.50+rpt-rpi-2712` |
| Independence revision | `156c506` |
| Rust compiler | `rustc 1.99.1 (58cf8f9c3 2026-10-13)` |
| Rust build tool | `cargo 1.99.1 (5f94df478 2026-08-27)` |
| Ollama | `0.40.10` |
| Text model | `gemma3:1b`, reported ID `97558784bdc9`, recorded download size 806 MB |

The recorded model download size is not its total memory requirement while it
is loaded and answering. This guide has not been run as a fresh-card acceptance
test; it records the successful configuration above and the current source
configuration at revision `156c506`.

## 1. Install system prerequisites

Run these commands in the Pi terminal. `sudo apt update` refreshes APT's
catalog of available packages. It does **not** upgrade the whole operating
system. The one `apt install` command then installs the complete known native
build prerequisite set before the first Cargo build.

```bash
sudo apt update
sudo apt install \
  git curl ca-certificates \
  build-essential pkg-config cmake \
  clang libclang-dev libasound2-dev \
  zstd
```

The installation changes system software and may download packages. Each item
is present for a specific reason:

- `git` obtains the repository and records which committed revision is checked
  out.
- `curl` downloads the Rust and Ollama installers.
- `ca-certificates` provides the trusted certificate store used to verify those
  HTTPS downloads.
- `build-essential` provides the standard C/C++ compilers, `make`, and
  development files used to build and link native dependencies.
- `pkg-config` tells Rust build scripts where installed native libraries live
  and which compiler/linker settings they require.
- `cmake` configures and coordinates the bundled whisper.cpp native build.
- `clang` installs C/C++ compiler tools alongside the binding toolchain.
- `libclang-dev` supplies the libclang library that `bindgen` uses to generate
  Rust declarations from C/C++ headers.
- `libasound2-dev` provides the ALSA development files used by the Linux audio
  dependencies, even though this text-only run will not open an audio device.
- `zstd` extracts the compressed Ollama distribution used by its Linux
  installer.

The demonstrated binding requirement is **libclang**. The standalone `clang`
compiler is installed as part of the coherent toolchain, but this guide does
not claim that it necessarily compiles whisper.cpp; the native build may use
GCC from `build-essential` instead. This is the consolidated working setup,
not a claim that every package is part of a mathematically minimal install.

## 2. Install Rust and rustfmt

Rustup is Rust's per-user toolchain manager. It installs the Rust compiler
(`rustc`), Cargo, and optional tools under your home directory. Run it as your
normal user—**not** with `sudo`.

The checked-out Independence revision has no `rust-toolchain.toml` or
`rust-toolchain` file that selects a repository-specific version. To reproduce
the reported configuration, install the explicit tested `1.99.1` toolchain and
add `rustfmt` before building:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain none
. "$HOME/.cargo/env"
rustup toolchain install 1.99.1 --profile minimal --component rustfmt
rustup default 1.99.1
rustc --version
cargo --version
rustfmt --version
```

The first command downloads and runs Rust's official rustup installer, without
selecting a moving default toolchain. The line beginning with `.` loads
`$HOME/.cargo/env` into this terminal session; it adds Rustup's user-local
`$HOME/.cargo/bin` directory to `PATH` so the next commands can find `rustup`,
`rustc`, Cargo, and rustfmt. `rustup toolchain install` downloads exactly
`1.99.1` and its `rustfmt` component. `rustup default` changes the normal
user's default Rust toolchain to that explicit version. The three version
commands should report Rust/Cargo 1.99.1 and a rustfmt version.

`rustc` compiles Rust source, Cargo resolves/builds the package and its locked
dependencies, and rustfmt formats Rust source. A moving `stable` download is
not described here as a way to guarantee the recorded 1.99.1 version.

## 3. Clone Independence

Create a conventional code directory, clone committed source from GitHub, and
select the recorded working revision:

```bash
mkdir -p ~/code
git clone https://github.com/samuelebstein/independence.git \
  ~/code/independence
cd ~/code/independence
git checkout 156c506
git rev-parse --short HEAD
```

`mkdir -p` creates `~/code` if it does not already exist. `git clone` retrieves
the repository's committed code and history into `~/code/independence`; it does
not install system packages. `cd` makes that repository the working directory.
`git checkout 156c506` selects the reported working commit, and
`git rev-parse --short HEAD` should print `156c506`. Keep this directory as the
place to run the remaining commands.

## 4. Build Independence

From `~/code/independence`, build the optimized release executable:

```bash
cargo build --release --locked -j 1
```

`--release` builds optimized code. `--locked` tells Cargo to preserve the
dependency resolution recorded in `Cargo.lock`, rather than updating it.
`-j 1` limits Cargo to one build job at a time; it can lower peak contention on
a small Pi, but it is not a hard memory limit. Cargo may download source
dependencies during this first build.

The resulting executable is:

```text
target/release/independence
```

`Cargo.toml` states the package and direct dependency requirements;
`Cargo.lock` records the exact resolved dependency graph used by this revision.
`cargo run` builds if necessary and then launches the program. Launching
`./target/release/independence` directly runs the already-built executable and
does not start another build.

The build compiles the Rust integrations for audio and local Whisper/native
code. That does not download Whisper speech-model weights, require Piper, or
need a physical audio device for the text-only run below.

## 5. Install and start Ollama

Ollama is separate system software that serves the local model. Unlike Rustup,
which installs under the current user's home directory, Ollama's Linux
installer places system software and creates its service. The
[official Linux documentation](https://docs.ollama.com/linux#installing-specific-versions)
documents `OLLAMA_VERSION` for selecting a specific release. Use it here so
the command requests the tested `0.40.10`, rather than the installer’s current
latest release:

```bash
curl -fsSL https://ollama.com/install.sh | OLLAMA_VERSION=0.40.10 sh
sudo systemctl enable --now ollama
systemctl is-active ollama
ollama --version
```

The installer downloads and installs Ollama and may request elevated access for
system locations. `sudo systemctl enable --now ollama` starts the Ollama system
service now and enables it to start at boot. `systemctl is-active ollama` should
print `active`. `ollama --version` should report `0.40.10`. Independence uses
the service through its existing local default endpoint; do not also start a
separate `ollama serve` process for this guide.

## 6. Download and test the text model

Ollama is the inference software. Gemma is the separately downloaded model
that Ollama will run. Download the recorded small text model, inspect the local
catalog, then ask it one direct test question:

```bash
ollama pull gemma3:1b
ollama list
ollama run gemma3:1b "Say hello in one short sentence."
```

`ollama pull` downloads `gemma3:1b`; the reported successful download was
806 MB. `ollama list` should show the local model, with reported ID
`97558784bdc9` in the recorded configuration. `ollama run` loads the model and
prints a short local response. This is the first command in the guide that
performs model inference.

## 7. Run Independence

Return to the repository root if needed, then launch the release binary with a
one-command model override:

```bash
OLLAMA_MODEL=gemma3:1b ./target/release/independence
```

The equivalent development command is:

```bash
OLLAMA_MODEL=gemma3:1b cargo run --release --locked -j 1
```

`OLLAMA_MODEL=gemma3:1b` applies only to that one launch. It does not change
Independence's source default, which remains `qwen3-vl:4b-instruct`, or change
Ollama's global configuration. The application uses its existing local Ollama
endpoint default.

At the Independence prompt, try a typed question and then a follow-up:

```text
What is the primary gas in Earth's atmosphere?
Why is it more common than oxygen?
/quit
```

The first two lines should receive local text answers using the same in-memory
conversation. `/quit` is the application’s supported normal exit command.

## Verification boundary

This guide was checked against the current repository configuration at
`156c506`: its `OLLAMA_MODEL` override, default local endpoint, release build
path, and `/quit` command. The documented Ollama version-selection mechanism
was checked against the official Linux documentation. It was not executed as a
fresh Raspberry Pi installation during this documentation change.

A future clean-install acceptance test is simple: follow these steps in order,
receive a local answer from Independence, ask the follow-up, and exit normally
with `/quit`.

## 8. Optional hardware adapters: attached keyboard as buttons

This section is an additional voice-and-camera path. It does not replace the
text-only run above. The one Independence executable still owns the same
controller, accepted history, cancellation rules, and terminal logs.
It describes the hardware-adapter source changes after the reported `156c506`
text-only baseline; it is not a claim that those adapters existed or were
hardware-tested at that recorded revision.

In button mode, a keyboard physically connected to the Pi becomes a tiny
controller rather than a text keyboard:

| Physical key | Linux button event | Independence behavior |
| --- | --- | --- |
| Hold Enter | `TalkPressed`, then `TalkReleased` | After the `i'm listening` cue, records while held; release finishes the draft and submits it once transcription/photos are ready. |
| Press `P` while Enter is held and capture has begun | `PhotoPressed` | Requests one explicit still photograph to attach to that draft. |
| Press Escape | `Cancel` | Discards unfinished recording/photo/model/speech work while preserving accepted conversation history. |

The adapter reads Linux `evdev` key codes directly. It ignores unrelated keys
and Linux autorepeat events; it does not assemble characters, use a keyboard
layout, invoke the line-command parser, or treat the keyboard as a terminal.

### List devices without starting Independence media work

From the repository root, run:

```bash
./target/release/independence --input-devices
./target/release/independence --audio-input-devices
./target/release/independence --audio-devices
```

The first command opens Linux input *nodes only long enough to inspect their
names and supported Enter/P/Escape controls*. It does not grab a keyboard,
open a microphone, load Whisper/Piper/Ollama, access a camera, or run
inference. Prefer a stable path it reports under `/dev/input/by-id/`, such as
`/dev/input/by-id/usb-Example_Button_Keyboard-event-kbd`, rather than an
unstable `/dev/input/eventN` number.

The second command lists CPAL microphone identifiers/names without starting a
recording. The third lists Rodio/CPAL output identifiers without opening a
speaker stream. Copy an exact logged `id` when selecting a USB microphone or
speaker; Independence accepts either CPAL's exact input ID or its exact name
for `INDEPENDENCE_MICROPHONE`, but the ID is less ambiguous.

### Give access only to the selected keyboard

Do **not** run Independence as root and do not add the user to a broad group
that can read every keyboard. On Linux, `/dev/input/event*` access is normally
controlled by udev permissions. Create a dedicated group once:

```bash
sudo groupadd --system independence-input
sudo usermod -a -G independence-input "$USER"
```

Those commands change local account/group configuration. Log out and back in
before expecting the new group membership in an SSH session.

Then inspect the chosen stable keyboard path and create a **device-specific**
udev rule. These inspection commands make no changes:

```bash
udevadm info --query=property \
  --name=/dev/input/by-id/usb-Example_Button_Keyboard-event-kbd
udevadm info --attribute-walk \
  --name=/dev/input/by-id/usb-Example_Button_Keyboard-event-kbd
```

Use a unique property from that output, ideally the keyboard's serial number
or physical port path. The following is a template, not a universal rule: it
must use values from *your selected keyboard* before it is installed.

```text
# /etc/udev/rules.d/70-independence-button-keyboard.rules
SUBSYSTEM=="input", KERNEL=="event*", ENV{ID_INPUT_KEYBOARD}=="1", ENV{ID_PATH}=="<copied-unique-ID_PATH>", GROUP="independence-input", MODE="0660", SYMLINK+="independence/button-kbd"
```

After replacing the placeholder, install the file with your editor and reload
rules:

```bash
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=input
```

Reconnect the selected keyboard if the new `/dev/input/independence/button-kbd`
symlink does not appear. This rule grants the dedicated group access only to
the matching keyboard event node. Do not use a wildcard rule that grants access
to all `event*` nodes.

### Launch over SSH

The Pi profile defaults to the button adapter, `rpicam-still`, and
`gemma3:1b`. It requires an explicit device path so Independence never guesses
which keyboard to grab:

```bash
./target/release/independence --voice --profile raspberry-pi \
  --input buttons \
  --input-device /dev/input/independence/button-kbd \
  --audio-input 'Exact CPAL input id' \
  --audio-output 'Exact CPAL output id'
```

`--input buttons`, `--input-device`, `--audio-input`, and `--audio-output`
are explicit command-line choices and win over profile defaults and the
corresponding environment variables. `INDEPENDENCE_MICROPHONE` and
`INDEPENDENCE_AUDIO_OUTPUT` remain useful per-launch selectors when no matching
command-line option is supplied. The profile supplies defaults only where
neither is set. `OLLAMA_MODEL`,
`INDEPENDENCE_WHISPER_MODEL`, `INDEPENDENCE_TTS_MODEL`,
`INDEPENDENCE_TTS_CONFIG`, `INDEPENDENCE_PIPER_COMMAND`, and
`INDEPENDENCE_CAMERA_BACKEND` remain per-launch environment overrides.

In this mode Independence starts **no** stdin reader and enables **no**
Crossterm raw-key reader. Enter, `P`, and Escape typed in the SSH terminal do
nothing to recording, photos, submission, or cancellation. The SSH terminal
still displays timestamped logs, and Ctrl+C/SIGINT still performs orderly
shutdown.

When the selected keyboard starts, Independence exclusively grabs its evdev
node, preventing its button keys from also reaching the Pi's local console.
Dropping/shutting down the adapter releases that grab. If the keyboard is
unplugged or the Linux event stream loses synchronization, Independence cancels
unfinished work and releases the microphone; it does not submit a possibly
truncated recording. The adapter retries the exact selected path, and a fresh
Enter press is required after recovery.

### Verify microphone and speaker separately

The Pi profile does not assume the laptop's Whisper/Piper files or Python
environment exist. Install/download those local components deliberately using
the current [voice setup in the README](../README.md#dictation-and-voice-modes),
then check each capability separately:

```bash
# Whisper/microphone check using terminal controls; does not call Ollama.
INDEPENDENCE_MICROPHONE='Exact CPAL input id' \
  ./target/release/independence --dictate

# Piper/selected speaker check; does not open a microphone, model, or camera.
./target/release/independence --speak-test \
  --audio-output 'Exact CPAL output id'
```

The first command needs the local `ggml-base.en.bin` file; the second needs the
Piper executable plus the HAL ONNX/config pair. If either is missing,
Independence keeps terminal logging usable and reports the local failure. These
commands do not prove that a particular USB device is suitable until you speak
or listen through it on the Pi.

### Raspberry Pi Camera Module 3 Wide

The Pi profile selects the `rpicam-still` backend. It keeps the existing V4L2
backend available for a USB webcam (`INDEPENDENCE_CAMERA_BACKEND=v4l2`). The Pi
adapter uses the Raspberry Pi-supported `rpicam-still` tool directly—without a
shell—with `--nopreview`, a 1280×720 JPEG request, and stdout as its in-memory
image transport. Its stderr is never interpreted as image data or logged as
an image payload.

First verify the externally installed camera tool and inspect camera IDs:

```bash
rpicam-still --list-cameras
```

`rpicam-still` must be installed and the Camera Module 3 Wide connected/configured
by the Pi operating system before this can succeed. The app bounds the child
process to five seconds, bounds JPEG output to 10 MiB, validates the returned
JPEG, and kills/reaps its child process on cancellation. It does not save the
still to disk. [Raspberry Pi's camera software documentation](https://www.raspberrypi.com/documentation/computers/camera_software.html)
lists `rpicam-still` among its supported camera applications; it is the
appropriate camera stack for current Pi camera modules rather than the
unsupported legacy `raspistill` stack.

To test only the adapter—no Whisper, Piper, Ollama, or language-model request—
run the ignored hardware test from the repository root, using an ID printed by
the preceding command:

```bash
INDEPENDENCE_RPICAM_CAMERA=0 \
  cargo test manual_rpicam_capture_without_ollama -- --ignored --nocapture
```

This intentionally activates the selected camera once, prints dimensions and
format, and releases it. It was not run as part of this code change.

`gemma3:1b` remains the Pi profile's text-test model and is deliberately marked
text-only. Pressing `P` with that configuration reports that camera capture
remains off; Independence does not pretend the text model can inspect a photo.
After deliberately installing and verifying a local vision model, make both
facts explicit for one launch:

```bash
OLLAMA_MODEL='qwen3-vl:4b-instruct' \
INDEPENDENCE_IMAGE_MODEL_SUPPORT=vision \
./target/release/independence --voice --profile raspberry-pi \
  --input buttons \
  --input-device /dev/input/independence/button-kbd
```

Only then can a `P` press request the Camera Module still for the held draft.
This configuration does not download a model or alter Ollama globally.

### End-to-end physical acceptance test

After the independent checks pass, launch the button command over SSH. Type
Enter, `P`, and Escape into the SSH terminal first: logs should not show any
interaction change. Then, on the physically attached keyboard:

1. Hold Enter and wait for the logged/listening cue; recording starts only
   after that cue completes.
2. While still holding Enter, press `P` once. With a verified vision model,
   one still is requested; with `gemma3:1b`, the app reports text-only support
   and leaves the camera off.
3. Release Enter. The draft submits only after its current transcript and
   requested photo capture are ready.
4. Hold Enter again during unfinished work to replace that unfinished draft,
   or press Escape to cancel it without erasing accepted history.
5. Unplug the selected keyboard while recording. The microphone should stop
   and no incomplete question should submit. Reconnect it, wait for recovery,
   and use a fresh Enter press.

These real hardware checks are pending on a Raspberry Pi with the selected USB
devices and Camera Module attached. The ordinary Rust tests use synthetic
controls, recorder/transcriber/camera fakes, mock Ollama, and fixture
subprocesses instead; they do not claim to validate Pi hardware.
