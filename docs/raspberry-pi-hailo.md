# Raspberry Pi + AI HAT+ 2 Hailo VLM bridge

This is the one installation and verification path for running Independence's
local vision path on a Raspberry Pi 5 with an AI HAT+ 2 (Hailo-10H).
It uses Hailo Apps' Python VLM API behind a small loopback HTTP bridge; it does
not use Hailo-Ollama to serve vision.

Run one numbered gate at a time. A gate is passed only when its final check
succeeds; do not compensate for a failed earlier gate by retrying a later one.
That order separates operating-system, Hailo-device, model-installation,
server, and Independence failures.

## Before starting

- Use a Pi 5 with 64-bit Raspberry Pi OS Trixie, the AI HAT+ 2 seated and
  cooled, and reliable power.
- On this home's router, connect the Pi to the **2.4 GHz** Wi-Fi SSID. Its
  5 GHz option has not been reliable here; a failed 5 GHz connection is a
  network issue, not a Hailo or Independence failure.
- These steps download packages, source code, and one model only when the
  command explicitly says so. The bridge does not download a model at startup
  or send prompts/photos off the Pi.
- Stop Hailo-Ollama or any other Hailo GenAI workload before Gate 5. Two GenAI
  processes competing for the accelerator can make an otherwise correct
  bridge fail to start or infer.

The completed pieces are:

| Piece | What it does | Success signal |
| --- | --- | --- |
| Independence | Builds the local application and its normal request path | release executable exists |
| Hailo runtime | Lets Linux see the Hailo-10H | `hailortcli` identifies one device |
| Hailo Apps + HEF | Installs the VLM runtime and local Qwen2-VL model | preflight prints one HEF path |
| Python bridge | Translates Independence's Ollama-shaped vision request | `/healthz` and `/api/show` succeed on loopback |
| Hailo profile | Connects the existing voice/photo controls to that bridge | spoken/photo test receives an answer |

## 1. Prepare and prove the base Pi

This gate installs Independence's native build dependencies before Cargo runs.
Gate 4's Hailo installer owns its version-aligned Hailo Apps dependencies. The
firmware/kernel update needs a reboot before the new Hailo driver is installed.

```bash
sudo apt update
sudo apt full-upgrade -y
sudo rpi-eeprom-update -a
sudo apt install -y git curl ca-certificates build-essential pkg-config cmake clang libclang-dev libasound2-dev zstd python3-venv jq rpicam-apps
sudo reboot
```

After reconnecting, confirm the supported OS/architecture and installed command
set. `uname -m` must print `aarch64` and the `grep` command must print the
Trixie codename.

```bash
uname -m
grep '^VERSION_CODENAME=trixie$' /etc/os-release
command -v git curl cmake clang jq rpicam-still
```

If either OS check fails, re-image or update the Pi before continuing. If a
command is absent, rerun the package installation and resolve APT's error; do
not attempt a partial Cargo or Hailo installation.

Install the pinned Rust toolchain used by the base-Pi guide, then clone and
build a fresh checkout. `-j 1` keeps peak memory lower on smaller Pis.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
. "$HOME/.cargo/env"
rustup toolchain install 1.99.1 --profile minimal --component rustfmt
rustup default 1.99.1
rustc --version
cargo --version
mkdir -p "$HOME/code"
git clone https://github.com/samuelebstein/independence.git "$HOME/code/independence"
cd "$HOME/code/independence"
cargo test --locked
cargo build --release --locked -j 1
test -x ./target/release/independence
```

`git clone` deliberately stops if that destination already exists; do not
overwrite an existing checkout. To update one later, use `git pull --ff-only`
inside it, review the result, and rebuild.

These checks only inspect local hardware; neither needs Hailo. A missing
camera blocks only the later photo test, not the bridge itself.

```bash
./target/release/independence --input-devices
rpicam-still --list-cameras
```

## 2. Recommended no-accelerator baseline

This optional diagnostic proves the Pi, local networking, Rust build, and
ordinary local text path before Hailo is involved. It is standard Ollama, not
Hailo-Ollama, and does not validate the accelerator.

```bash
curl -fsSL https://ollama.com/install.sh | sh
sudo systemctl enable --now ollama
systemctl is-active --quiet ollama
ollama pull gemma3:1b
OLLAMA_MODEL=gemma3:1b ./target/release/independence
```

Ask one typed question and use `/quit` to exit. If the service check fails,
inspect `journalctl -u ollama -n 50 --no-pager` before proceeding. If this
gate passes but a later Hailo gate fails, the problem is narrowed to Hailo or
the bridge rather than the base application.

## 3. Install and identify the AI HAT+ 2

The AI HAT+ 2 is Hailo-10H. Install `hailo-h10-all`, not the Hailo-8
`hailo-all` package. `dkms` ensures the driver is built for the running kernel.

```bash
sudo apt install -y dkms hailo-h10-all
sudo reboot
```

After reconnecting, run the first accelerator gate:

```bash
hailortcli fw-control identify
```

It must identify one Hailo-10H device. If it does not, stop here: check the
HAT seating, PCIe ribbon, cooling/power, OS update, and `dmesg | grep -i hailo`.
Do not install Hailo Apps or download a model until this command works. The
package and verification sequence follows Raspberry Pi's
[AI documentation](https://www.raspberrypi.com/documentation/computers/ai.html).

## 4. Install Hailo Apps and explicitly prepare the model

`./install.sh --skip-gstreamer` creates Hailo Apps' `venv_hailo_apps` virtual
environment and runs its version-aligned dependency setup. `--skip-gstreamer`
omits only the unused GStreamer pipeline stack; this bridge needs the Python
GenAI API. The checked-in Independence helper is the final dependency gate: it
activates that environment, installs the GenAI Python extras, verifies imports,
downloads the `vlm_chat` Hailo-10H resource, and requires exactly one supported
Qwen2-VL 2B HEF.

```bash
cd "$HOME/code"
git clone https://github.com/hailo-ai/hailo-apps.git
cd "$HOME/code/hailo-apps"
./install.sh --skip-gstreamer
test -f setup_env.sh && test -d venv_hailo_apps && printf 'Hailo Apps environment exists\n'
cd "$HOME/code/independence"
HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/prepare_pi_hailo_vlm.sh --install-python-dependencies --download-model
```

The final command must end with `Hailo VLM preflight passed` and one
`HAILO_VLM_HEF=` path. It is intentionally the only step that installs the
GenAI extras or downloads the VLM. Do not manually run `source setup_env.sh`,
`pip install`, or `hailo-download-resources` as duplicate setup steps.
If the Hailo installer exits nonzero or reports `Installation completed with
errors`, stop and resolve that output; if it only warns about an optional
package, the helper's import check remains the authoritative pass/fail check.
As with the Independence checkout, `git clone` stops rather than overwriting
an existing Hailo Apps checkout; use the reviewed existing checkout or update
it deliberately.

If `venv_hailo_apps` is missing, `./install.sh --skip-gstreamer` did not finish: rerun it from
the Hailo Apps checkout and resolve its first error. If preflight reports zero
HEFs, rerun the final command with `--download-model`. If it reports multiple
matching HEFs, select the reviewed one when starting the bridge:

```bash
export HAILO_VLM_HEF='/absolute/path/to/Qwen2-VL-2B-Instruct.hef'
```

Do not point that variable at a Qwen3-VL or other custom HEF without separately
reviewing its prompt and image contract. Hailo Apps' own
[installation guide](https://github.com/hailo-ai/hailo-apps/blob/main/doc/user_guide/installation.md)
is the upstream reference for this environment. Keep that export in the same
terminal that starts the bridge; a newly opened terminal needs it again.

## 5. Start and prove the local Python VLM bridge

Keep the bridge in terminal A. The wrapper activates Hailo Apps and starts one
local VLM instance; the bridge binds only to `127.0.0.1:11435`, so it is not a
LAN service.

```bash
cd "$HOME/code/independence"
HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/run_pi_hailo_vlm_bridge.sh
```

Its expected startup lines name `http://127.0.0.1:11435`,
`qwen2-vl:2b`, and `capabilities: vision`. Leave terminal A running. In
terminal B, verify the server's address, model alias, and capability before
launching Independence:

```bash
curl --fail --silent http://127.0.0.1:11435/healthz | jq .
curl --fail --silent --header 'Content-Type: application/json' --data '{"model":"qwen2-vl:2b"}' http://127.0.0.1:11435/api/show | jq .
```

The health response must say `ready` and the show response must contain exactly
the `vision` capability. Independence uses that response to decide whether a
camera photo is permitted; do not bypass it with a configuration flag.

Use Hailo Apps' bundled, non-personal image for the first real inference test.
This tests image bytes in the same base64 field Independence uses, without
needing a camera or a private photo.

```bash
sample_image="$HOME/code/hailo-apps/doc/images/barcode-example.png"
test -r "$sample_image"
image_base64="$(base64 --wrap=0 "$sample_image")"
jq -n --arg image "$image_base64" '{model:"qwen2-vl:2b",stream:false,messages:[{role:"user",content:"Describe this image briefly.",images:[$image]}]}' | curl --fail --silent --header 'Content-Type: application/json' --data @- http://127.0.0.1:11435/api/chat | jq .
unset image_base64 sample_image
```

The response must contain non-empty assistant text. A bind error means another
local process owns port 11435; inspect
`ss -ltnp '( sport = :11435 )'` and stop only the identified conflicting
process, or choose another port. A Hailo resource error usually means a
concurrent Hailo GenAI process, an unrecognized device, or a model/runtime
version mismatch—return to Gates 3 and 4 rather than changing the client.

The API accepts zero through four ordered images on the final user message.
The bridge keeps the first captured image first, rather than merging images.
It deliberately rejects a larger request, images over 10 MiB, or images over
10 million pixels to bound local memory and latency.

## 6. Connect the existing Independence voice/photo flow

Gate 5 is sufficient to prove the Hailo server. This gate additionally needs
the hardware-specific parts of [the base Pi guide](rasberry-pi.md):

1. List and select the physical button keyboard, microphone, and speaker:

   ```bash
   ./target/release/independence --input-devices
   ./target/release/independence --audio-input-devices
   ./target/release/independence --audio-devices
   ```

2. Create a **device-specific** udev rule for only that keyboard; do not run
   Independence as root or grant access to all input devices. The exact
   inspection and rule template are in the base guide's
   [keyboard-access section](rasberry-pi.md#give-access-only-to-the-selected-keyboard).
3. Install local Whisper and Piper, then separately test the selected
   microphone and speaker using the base guide's
   [voice checks](rasberry-pi.md#verify-microphone-and-speaker-separately).
4. Confirm `rpicam-still --list-cameras` sees the intended camera.

Those values are intentionally not hard-coded here because they identify your
own USB devices. With terminal A's bridge still running, launch terminal B
using the exact IDs reported by those checks:

```bash
cd "$HOME/code/independence"
./target/release/independence --voice --profile raspberry-pi-hailo --input buttons --input-device /dev/input/independence/button-kbd --audio-input 'Exact CPAL input id' --audio-output 'Exact CPAL output id'
```

The `raspberry-pi-hailo` profile selects backend `hailo-vlm`,
`http://127.0.0.1:11435`, and model alias `qwen2-vl:2b`. If you deliberately
change the bridge port, make both sides agree:

```bash
# terminal A
HAILO_VLM_PORT=11500 HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/run_pi_hailo_vlm_bridge.sh

# terminal B
INDEPENDENCE_HAILO_VLM_URL='http://127.0.0.1:11500' ./target/release/independence --voice --profile raspberry-pi-hailo --input buttons --input-device /dev/input/independence/button-kbd
```

For the physical acceptance test, hold Enter for five seconds and release to
enter Ephemeral Mode. Then hold Enter again, wait for the listening cue, press
`P` while still holding Enter to request a fresh in-memory photo, ask about
the scene, and release Enter. For the ordered multi-image check, press `P`
twice before release and ask a comparison question. A successful path is:

```text
local microphone -> local Whisper -> /api/show vision check -> fresh camera still(s)
-> 127.0.0.1 Hailo bridge -> local Hailo Qwen2-VL -> local Piper
```

No photo is saved by this path. The bridge receives text plus image bytes and
does not transcribe, translate, or choose the spoken language. The stock
Whisper model is English-only; a different local Whisper model must be
installed and tested separately before relying on another language.

## Optional safe bridge telemetry

To retain only local API timing/count metadata, opt in with an existing
directory when starting terminal A:

```bash
cd "$HOME/code/independence"
mkdir -p local-data/telemetry
HAILO_VLM_TELEMETRY_JSONL="$PWD/local-data/telemetry/hailo-vlm.jsonl" HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/run_pi_hailo_vlm_bridge.sh
```

The JSONL records route, method/status, outcome, duration, model alias, and
message/image counts and byte counts. It does not record prompt text,
transcripts, image data, model responses, or HEF paths. Normal terminal output
and the operating system remain outside that narrow telemetry guarantee.

## Completion checklist

| Gate | Required proof |
| --- | --- |
| 1 | `cargo test --locked` and the release build pass |
| 2 | one local typed Ollama answer, if the recommended baseline was run |
| 3 | `hailortcli fw-control identify` reports Hailo-10H |
| 4 | preflight prints one Qwen2-VL HEF path |
| 5 | health, show/vision, and bundled-image chat all return successfully |
| 6 | an explicit physical photo turn receives a local spoken answer |

Only mark hardware integration complete after the relevant Pi-local checks
actually pass. Automated tests validate the bridge's request mapping and
limits, but they do not prove this Pi's HAT, camera, microphone, speaker, or
downloaded model.
