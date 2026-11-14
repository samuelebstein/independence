# Raspberry Pi 5: Independence hardware setup

Start after [first boot](raspberry-pi.md). This guide builds Independence,
installs local speech assets, and proves the attached speaker, microphone,
keyboard buttons, and Camera Module 3. It does not install Hailo or any
language/vision model.

## Overview

| Step | Done when |
| --- | --- |
| 1. Host packages | The Pi can build Independence and use local audio/camera tools |
| 2. Build | `target/release/independence` exists |
| 3. Local speech | Piper, the HAL voice, and Whisper are on the Pi |
| 4. Hardware check | Speaker, microphone, buttons, and camera each pass separately |

## 1. Install host packages

This installs the compiler, audio headers, camera command, and Python virtual
environment needed by the local application and speech tools.

```bash
sudo apt update
sudo apt install -y \
  git curl ca-certificates \
  build-essential pkg-config cmake clang libclang-dev libasound2-dev \
  python3-venv rpicam-apps zstd
command -v git curl cmake clang rpicam-still
```

## 2. Build Independence

This installs the pinned Rust toolchain for the normal Pi user, clones the
repository, runs its tests, and makes the release binary with one build job.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain none
. "$HOME/.cargo/env"
rustup toolchain install 1.99.1 --profile minimal --component rustfmt
rustup default 1.99.1

mkdir -p "$HOME/code"
git clone https://github.com/samuelebstein/independence.git \
  "$HOME/code/independence"
cd "$HOME/code/independence"
cargo test --locked
cargo build --release --locked -j 1
test -x ./target/release/independence
```

## 3. Install local speech assets

This one-time setup downloads Piper, the HAL voice, and Whisper. The downloads
use the network now; voice tests and normal inference afterward stay local to
the Pi.

```bash
cd "$HOME/code/independence"
mkdir -p "$HOME/.local/share/independence"
python3 -m venv "$HOME/.local/share/independence/piper-venv"
"$HOME/.local/share/independence/piper-venv/bin/pip" install 'piper-tts==1.8.0'

mkdir -p local-data/piper/hal local-data/whisper
curl -fL -o local-data/piper/hal/hal.onnx \
  https://huggingface.co/campwill/HAL-9000-Piper-TTS/resolve/main/hal.onnx
curl -fL -o local-data/piper/hal/hal.onnx.json \
  https://huggingface.co/campwill/HAL-9000-Piper-TTS/resolve/main/hal.onnx.json
curl -fL -o local-data/whisper/ggml-base.en.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin

test -x "$HOME/.local/share/independence/piper-venv/bin/piper"
test -r local-data/piper/hal/hal.onnx
test -r local-data/piper/hal/hal.onnx.json
test -r local-data/whisper/ggml-base.en.bin
```

## 4. Hardware check

These tests do not start Ollama, Hailo, or a vision model. First list the
devices so the variables match this Pi; event numbers can change after reboot.

```bash
cd "$HOME/code/independence"
./target/release/independence --input-devices
./target/release/independence --audio-input-devices
./target/release/independence --audio-devices
```

For the confirmed USB PnP Audio Device and Magic Keyboard, use the values
below. On another device, replace the audio IDs with the listed `ID` values and
the button path with the keyboard line that reports `ENTER: yes`, `P: yes`, and
`ESC: yes`; no udev rule is needed for this setup.

```bash
PI_TEST_MIC='alsa:plughw:CARD=Device,DEV=0'
PI_TEST_SPEAKER='alsa:plughw:CARD=Device,DEV=0'
PI_PIPER_BIN="$HOME/.local/share/independence/piper-venv/bin/piper"
PI_BUTTON_DEVICE='/dev/input/event2'
test -r "$PI_BUTTON_DEVICE"
```

### Speaker

This plays a fixed sentence through the selected speaker.

```bash
INDEPENDENCE_PIPER_COMMAND="$PI_PIPER_BIN" \
  ./target/release/independence --speak-test \
  --audio-output "$PI_TEST_SPEAKER"
```

You should hear “Hello, Steve. I'm here to help.” If not, replace only
`PI_TEST_SPEAKER` with the exact ID from `--audio-devices` and rerun it.

### Microphone

This records and transcribes locally with Whisper.

```bash
./target/release/independence --dictate --audio-input "$PI_TEST_MIC"
```

Press **Enter** once, speak a short sentence, then press **Enter** again. The
transcript should print; use **Ctrl+C** to exit. If it cannot find the device,
replace only `PI_TEST_MIC` with the exact ID from `--audio-input-devices`.

### Buttons, microphone, and speaker

This fixture records one bounded clip and returns a generated tone; it does not
use Whisper, Piper, a camera, or a model.

```bash
./target/release/independence --voice \
  --input buttons \
  --input-device "$PI_BUTTON_DEVICE" \
  --audio-input "$PI_TEST_MIC" \
  --audio-output "$PI_TEST_SPEAKER" \
  --pipeline audio-to-audio \
  --backend fixture
```

Use the physically attached keyboard, not SSH keys: hold **Enter** for five
seconds and release to enter Ephemeral Mode; then hold **Enter** again, make a
short sound, and release. A test tone should play. Physical **Escape** cancels
unfinished work, and **Ctrl+C** exits the terminal process.

### Camera Module 3 Wide

Power the Pi off completely before connecting or reseating the ribbon cable.
The narrow **22-pin** end goes into the Pi 5 `CAM/DISP0` or `CAM/DISP1` socket;
the wide **15-pin** end goes into the camera. At both ends the gold contacts
face **away from the moving flap**: toward the Ethernet/USB ports on the Pi,
and toward the green camera PCB on the module. The blue/plain side faces the
flap at both ends.

This first captures a JPEG with the operating-system camera tool, then proves
the Independence adapter can capture an in-memory validated JPEG.

```bash
rpicam-still --list-cameras
rpicam-still --nopreview --camera 0 --timeout 2000 \
  --width 1280 --height 720 --encoding jpg \
  --output /tmp/independence-camera-check.jpg
test -s /tmp/independence-camera-check.jpg
rm -f /tmp/independence-camera-check.jpg

INDEPENDENCE_RPICAM_CAMERA=0 \
  cargo test manual_rpicam_capture_without_ollama -- --ignored --nocapture
```

Expect `captured ... as validated JPEG` and `test result: ok`. A headless
session may report an unavailable preview; that is expected with `--nopreview`.
If the camera identifies as `imx708` but then reports a frontend timeout,
power off and reseat both cable ends before trying another Pi camera connector
or cable.

## Next

- Continue to [HailoRT 5.3 + Qwen3-VL-2B-Instruct](raspberry-pi-hailo-qwen3.md)
  for the new-card Qwen3 setup.
- Or use the existing [Qwen2-VL Hailo bridge](raspberry-pi-hailo.md) for the
  currently implemented Independence vision backend.
