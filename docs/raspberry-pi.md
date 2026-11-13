# Raspberry Pi 5 base setup

Use this guide before the [AI HAT+ 2 / Hailo extension](raspberry-pi-hailo.md).
It sets up the Pi, local Ollama, and Independence's keyboard buttons, USB
audio, and Camera Module 3. Hailo installation and vision-model setup live
only in the extension.

> **Home-network note:** this Pi is reliable on the router's separate **2.4
> GHz** Wi-Fi SSID. Use that SSID rather than its 5 GHz option.

## Overview

| Section | When to use it | Done when |
| --- | --- | --- |
| 1. First boot | Once, with a monitor attached | Wi-Fi and SSH both work |
| 2. Base software | Once | Independence answers a typed local question |
| 3. Voice files | Once | Whisper, Piper, and the HAL voice are local |
| 4. Hardware check | Every new Pi; about 5–10 minutes after sections 1–3 | Speaker, microphone, buttons, and camera each pass |
| 5. Hailo | Only after this guide passes | Continue in the separate Hailo guide |

Start with a monitor and wired keyboard attached. Do not begin headless: use
the local console to prove Wi-Fi and SSH once, then manage the Pi over SSH.

## 1. First boot: Wi-Fi and SSH

Connect the Pi to the home's 2.4 GHz SSID. `--ask` keeps the Wi-Fi password out
of shell history.

```bash
nmcli radio wifi on
nmcli dev wifi list
sudo nmcli --ask dev wifi connect 'YOUR-2.4-GHZ-SSID'
nmcli device status
hostname -I
```

If the 2.4 GHz SSID does not appear, run `sudo raspi-config`, set
`Localisation Options → WLAN Country`, reboot, and repeat this block.

Enable SSH and confirm that it is running before disconnecting the monitor or
Ethernet.

```bash
sudo systemctl enable --now ssh
systemctl is-enabled ssh
systemctl is-active ssh
sudo ss -ltnp | grep ':22'
```

The status lines must say `enabled` and `active`. From the development
computer, use the IPv4 address printed by `hostname -I`:

```bash
ssh samebstein@PI-IP-ADDRESS
```

Unplug Ethernet only after that SSH connection works over Wi-Fi. If SSH says
`connection refused`, return to the Pi's monitor and rerun the SSH block above.

## 2. Base software and local text test

This guide assumes **Raspberry Pi OS (64-bit), Trixie**. Confirm it first; an
Ubuntu/Noble result or an empty codename means reflash the card before
continuing.

```bash
. /etc/os-release
printf 'PRETTY_NAME=%s\nVERSION_CODENAME=%s\n' "$PRETTY_NAME" "$VERSION_CODENAME"
uname -m
```

The expected values include `VERSION_CODENAME=trixie` and `aarch64`.

Install the operating-system packages used to build Independence, use ALSA,
run the Pi camera tool, and create the Piper virtual environment.

```bash
sudo apt update
sudo apt full-upgrade -y
sudo rpi-eeprom-update -a
sudo apt install -y \
  git curl ca-certificates \
  build-essential pkg-config cmake \
  clang libclang-dev libasound2-dev \
  python3-venv rpicam-apps zstd
sudo reboot
```

After reconnecting, confirm the main build tools are present:

```bash
command -v git curl cmake clang
```

Install Rust for the normal Pi user, then clone and build Independence. The
build and tests prove the checkout is usable before any hardware is involved.

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
git rev-parse --short HEAD
cargo test --locked
cargo build --release --locked -j 1
test -x ./target/release/independence
```

Install Ollama, start its local service, download the small text model, and
prove that Independence can answer one typed question locally.

```bash
curl -fsSL https://ollama.com/install.sh | sh
sudo systemctl enable --now ollama
systemctl is-active ollama
ollama pull gemma3:1b
OLLAMA_MODEL=gemma3:1b ./target/release/independence --mode ephemeral
```

At the prompt, ask a short question, then enter `/quit`. This is a text-only
check; `gemma3:1b` cannot inspect camera photos.

## 3. One-time local voice files

Run this section from the repository root once. It downloads local Whisper and
Piper files; it does not start a cloud speech service or send recordings away.

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

## 4. Five-to-ten-minute hardware check

Run this section from the repository root after sections 1–3. The device
inventory commands do not start recording, playback, a camera, or a model.

```bash
cd "$HOME/code/independence"
./target/release/independence --input-devices
./target/release/independence --audio-input-devices
./target/release/independence --audio-devices
```

For the confirmed USB PnP Audio Device, use the exact IDs below. Replace them
if the inventory on another Pi reports different IDs; run this short block in
each new SSH shell before the tests.

```bash
PI_TEST_MIC='alsa:plughw:CARD=Device,DEV=0'
PI_TEST_SPEAKER='alsa:plughw:CARD=Device,DEV=0'
PI_PIPER_BIN="$HOME/.local/share/independence/piper-venv/bin/piper"
```

### Speaker output

This plays a fixed sentence through the selected speaker and does not open the
microphone, camera, or Ollama.

```bash
INDEPENDENCE_PIPER_COMMAND="$PI_PIPER_BIN" \
  ./target/release/independence --speak-test \
  --audio-output "$PI_TEST_SPEAKER"
```

You should hear: “Hello, Steve. I'm here to help.” If not, choose the exact
`ID` from `--audio-devices` and repeat this one command.

### Microphone input

This records and transcribes locally without calling Ollama. It uses the same
explicit `--audio-input` parameter as the full voice command below.

```bash
./target/release/independence --dictate \
  --audio-input "$PI_TEST_MIC"
```

When it says it is ready, press **Enter once**, speak a short sentence, then
press **Enter again**. The transcript should print in the terminal; use
**Ctrl+C** to exit. If the microphone cannot be found, recopy its exact ID
from `--audio-input-devices`.

### Camera Module 3 Wide

Power the Pi off completely before connecting or reseating the ribbon cable.
The two cable ends are not interchangeable:

| Connection | Cable end | Gold-contact direction |
| --- | --- | --- |
| Pi 5 `CAM/DISP0` or `CAM/DISP1` | Narrow **22-pin** end | Away from the flap, toward the Ethernet/USB ports; blue/plain side toward micro-HDMI |
| Camera Module 3 Wide | Wide **15-pin** end | Away from the flap and toward the green camera PCB; blue/plain side toward the flap |

Boot after locking both flaps. This is the standard Pi 5 15-pin-to-22-pin
camera cable arrangement; either Pi camera/display connector can host the
camera. See Raspberry Pi's [camera setup guide](https://www.raspberrypi.com/documentation/accessories/camera-software.html)
for the connector illustrations.

First prove that the operating system can capture a JPEG, then remove the
temporary photo when finished.

```bash
rpicam-still --list-cameras
rpicam-still --nopreview --camera 0 --timeout 2000 \
  --width 1280 --height 720 --encoding jpg \
  --output /tmp/independence-camera-check.jpg
file /tmp/independence-camera-check.jpg
rm -f /tmp/independence-camera-check.jpg
```

`file` should identify a JPEG image. A headless SSH session may say that a
preview is unavailable; that is expected with `--nopreview`. If it identifies
an `imx708` camera but reports a camera-frontend timeout, power off and reseat
the cable at both ends, then try the other Pi camera connector or another
known-good cable.

Now prove the Independence camera adapter itself. This captures one in-memory
JPEG only; it does not call Ollama, Whisper, Piper, or write a photo to disk.

```bash
INDEPENDENCE_RPICAM_CAMERA=0 \
  cargo test manual_rpicam_capture_without_ollama -- --ignored --nocapture
```

Expect a `captured ... as validated JPEG` line and `test result: ok`.

### Keyboard buttons and the complete voice path

Run `--input-devices` again and choose the directly attached keyboard entry
that says `ENTER: yes`, `P: yes`, and `ESC: yes`. On the confirmed Magic
Keyboard it was `/dev/input/event2`; event numbers can change after a reboot,
so replace the value below with the current matching path. No udev rule is
needed for this working setup.

```bash
PI_BUTTON_DEVICE='/dev/input/event2'
```

This is the explicit button test and full local voice test. The app grabs the
physical keyboard while it is running; the SSH terminal remains only a display
and **Ctrl+C** remains the normal exit.

```bash
INDEPENDENCE_PIPER_COMMAND="$PI_PIPER_BIN" \
  ./target/release/independence --voice \
  --mode ephemeral \
  --profile raspberry-pi \
  --input buttons \
  --input-device "$PI_BUTTON_DEVICE" \
  --audio-input "$PI_TEST_MIC" \
  --audio-output "$PI_TEST_SPEAKER" \
  --backend ollama \
  --pipeline transcribe-and-speak \
  --model gemma3:1b
```

Use the **physical attached keyboard**, not SSH keys:

1. Hold **Enter** for five seconds, then release it to enter Ephemeral Mode.
   That first release does not record.
2. Hold **Enter** again, wait for the listening cue, speak, then release
   Enter. A local transcript, Gemma response, and Piper speech should follow.
3. On another held Enter turn, press physical **P** after the listening cue.
   Gemma should report that a photo is unavailable rather than capture one.
4. Press physical **Escape** during unfinished work to cancel it.

The Hailo extension supplies the verified vision-model path for a real
photo-and-question turn.

## 5. Continue to Hailo

After the text, speaker, microphone, camera, and button checks pass, continue
with [Raspberry Pi AI HAT+ 2: Hailo VLM extension](raspberry-pi-hailo.md).
That guide installs the accelerator runtime, starts the loopback vision server,
and changes only the inference backend; it does not repeat this base setup.
