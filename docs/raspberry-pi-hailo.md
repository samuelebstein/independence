# Raspberry Pi AI HAT+ 2: Hailo VLM extension

This is an extension to [the Raspberry Pi base guide](raspberry-pi.md), not a
second Pi installation guide. It adds the AI HAT+ 2 (Hailo-10H), Hailo Apps,
one local Qwen2-VL model, and a loopback Python bridge. It does **not** use
Hailo-Ollama to serve vision.

## 0. Complete the base guide first

Complete [sections 1–2 of the base guide](raspberry-pi.md) before starting
here. They are the only place for the first local monitor/keyboard bootstrap,
2.4 GHz Wi-Fi and SSH setup, OS/firmware updates, Rust, the Independence
checkout/build, and the ordinary local Ollama text baseline.

| Intended result | Base-guide prerequisite |
| --- | --- |
| Hailo server only | Sections 1–2 |
| Voice + camera + Hailo | Complete the base guide through its section 4 hardware check |

The current checkout must contain the Hailo bridge before any Hailo package or
model is installed:

```bash
cd "$HOME/code/independence"
test -x scripts/prepare_pi_hailo_vlm.sh
test -x scripts/run_pi_hailo_vlm_bridge.sh
test -f tools/hailo_vlm_bridge.py
```

If one of those checks fails, stop and switch to the reviewed Independence
revision that contains this runbook; the historical text-only revision in the
base guide predates this extension. Until this work is merged to the default
branch, a fresh checkout can switch to its current integration branch:

```bash
cd "$HOME/code/independence"
git fetch origin research/pihat2-integration
git switch --track origin/research/pihat2-integration
git pull --ff-only
cargo test --locked
cargo build --release --locked -j 1
```

Then rerun the three file checks. Once merged, use the reviewed default branch
instead; the file checks, not the branch name, are the source of truth.

### Required power source and power gate

Keep the AI HAT+ 2 seated and actively cooled. Use a USB-C power supply that
can provide **5 V / 5 A (25 W)**, ideally the official Raspberry Pi 27 W
supply. A charger advertising more watts at other voltages is not equivalent:
the Pi uses its 5 V USB-PD mode. Do not treat an unverified battery, phone
charger, or Mac USB-C charger as accepted deployment power merely because the
Pi boots. Raspberry Pi documents the 5 V / 5 A recommendation and the reduced
peripheral power available from a 3 A supply in its
[power guide](https://www.raspberrypi.com/documentation/hardware/raspberrypi/power.html).

After every boot, run this read-only check before starting the bridge and again
immediately after the first real image request:

```bash
cd "$HOME/code/independence"
scripts/check_pi_hailo_power.sh
```

It reports the current `EXT5V_V` rail, temperature, and the firmware's
current-plus-historical throttling flags. Only `throttled=0x0` passes. A
nonzero result makes the script exit 1; it is not safe to accept a successful
model answer as a power pass. Switch to a verified source, reboot to clear the
per-boot history, and repeat both checks. Do not connect two USB-C power
sources to the Pi at once.

This exact failure signature was observed after one successful Qwen2-VL image
request on a Mac USB-C charger:

```text
2026-11-13T20:57:16+00:00
EXT5V_V volt(24)=4.99552000V
temp=51.6'C
throttled=0x50000
```

`0x50000` means **under-voltage occurred since boot** (`0x10000`) and
**throttling occurred since boot** (`0x40000`), per Raspberry Pi's
[`get_throttled` bit table](https://www.raspberrypi.com/documentation/computers/os.html#vcgencmd).
It is a failed power gate even though the instantaneous voltage and temperature
looked healthy. A battery is accepted only if it passes this same pre- and
post-inference gate.

Stop Hailo-Ollama or any other Hailo GenAI workload before Gate 3: competing
GenAI processes can make an otherwise correct bridge fail to acquire the
accelerator.

## 1. Install and identify the Hailo-10H runtime

This gate installs only dependencies not owned by the base guide:

- `hailo-h10-all` is the AI HAT+ 2 / Hailo-10H host runtime. Do not substitute
  the Hailo-8 `hailo-all` package.
- `dkms` builds the driver for the running kernel.
- `jq` formats the local API checks in Gate 3.

```bash
sudo apt install -y dkms hailo-h10-all jq
dpkg-query -W dkms hailo-h10-all jq
sudo reboot
```

After reconnecting, the first accelerator gate must identify one Hailo-10H:

```bash
hailortcli fw-control identify
```

If it does not, stop here. Check the HAT seating, PCIe ribbon, power/cooling,
and `dmesg | grep -i hailo`; do not install Hailo Apps or download a model.
This package/verification sequence follows Raspberry Pi's
[AI documentation](https://www.raspberrypi.com/documentation/computers/ai.html).

## 2. Install Hailo Apps and prepare the one supported model

The Hailo Apps installer creates its `venv_hailo_apps` environment and manages
its version-aligned dependencies. `--skip-gstreamer` omits the unused GStreamer
pipeline stack; this bridge only needs Hailo's Python GenAI API. The
Independence helper is the final dependency gate: it installs GenAI extras,
checks imports, downloads the explicit `vlm_chat` Hailo-10H resource, and
requires exactly one Qwen2-VL 2B HEF.

```bash
cd "$HOME/code"
git clone https://github.com/hailo-ai/hailo-apps.git
cd "$HOME/code/hailo-apps"
./install.sh --skip-gstreamer
test -f setup_env.sh && test -d venv_hailo_apps && printf 'Hailo Apps environment exists\n'
cd "$HOME/code/independence"
HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/prepare_pi_hailo_vlm.sh --install-python-dependencies --download-model
```

The final command must print `Hailo VLM preflight passed` and exactly one
`HAILO_VLM_HEF=` path. It is intentionally the only step that installs the
GenAI extras or downloads this model. Do not add manual `source setup_env.sh`,
`pip install`, or `hailo-download-resources` commands to this sequence.

`git clone` stops instead of overwriting an existing Hailo Apps checkout. Use
the reviewed checkout you already have, or update it deliberately. If the
installer exits nonzero or reports `Installation completed with errors`, resolve
that output before continuing. If it only warns about an optional package, the
helper's import check is the authoritative pass/fail result.

| Preflight failure | Recovery |
| --- | --- |
| `venv_hailo_apps` missing | Rerun `./install.sh --skip-gstreamer` from the Hailo Apps checkout and resolve its first error. |
| `ZSH_VERSION: unbound variable` | Update to the current Independence checkout. Its Hailo wrappers source Hailo Apps from the correct directory without propagating Bash `nounset`. |
| Zero HEFs | Rerun the helper with `--download-model`. |
| Multiple matching HEFs | Select the reviewed Qwen2-VL 2B file only for the bridge command below. |

Do not point a HEF override at Qwen3-VL or another custom model without
separately reviewing its prompt/image contract. Hailo Apps'
[installation guide](https://github.com/hailo-ai/hailo-apps/blob/main/doc/user_guide/installation.md)
is the upstream reference for this environment.

## 3. Start and prove the loopback Python bridge

In terminal A, start one local VLM instance. It binds only to
`127.0.0.1:11435`, so it is not a LAN service:

```bash
cd "$HOME/code/independence"
HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/run_pi_hailo_vlm_bridge.sh
```

If Gate 2 found multiple HEFs, replace that command in the **same terminal**
with the reviewed absolute path:

```bash
HAILO_VLM_HEF='/absolute/path/to/Qwen2-VL-2B-Instruct.hef' HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/run_pi_hailo_vlm_bridge.sh
```

Expected startup names `http://127.0.0.1:11435`, model alias
`qwen2-vl:2b`, and `capabilities: vision`. The alias is the bridge's local
API name, not an Ollama model tag. Leave terminal A running.

In terminal B, prove address, model, and image capability before involving
Independence:

```bash
curl --fail --silent http://127.0.0.1:11435/healthz | jq .
curl --fail --silent --header 'Content-Type: application/json' --data '{"model":"qwen2-vl:2b"}' http://127.0.0.1:11435/api/show | jq .
```

Health must report `ready` and show must contain exactly `vision`. Independence
uses that response to authorize camera work; do not force image support with a
setting.

Use Hailo Apps' bundled non-personal image for the first real inference test:

```bash
sample_image="$HOME/code/hailo-apps/doc/images/barcode-example.png"
test -r "$sample_image"
base64 --wrap=0 "$sample_image" | jq -Rs '{model:"qwen2-vl:2b",stream:false,messages:[{role:"user",content:"Describe this image briefly.",images:[.]}]}' | curl --fail --silent --header 'Content-Type: application/json' --data @- http://127.0.0.1:11435/api/chat | jq .
unset sample_image
```

The response must contain non-empty assistant text. Immediately rerun the
power gate from section 0; it must still exit successfully before continuing.
A bind error means another local process owns port 11435; inspect
`ss -ltnp '( sport = :11435 )'` and stop only that process, or choose another
port. A Hailo resource error means return to Gates 1–2 and check for a
concurrent Hailo GenAI process or a model/runtime mismatch.

The API accepts zero through four ordered images on the final user message.
It preserves capture order and rejects a larger request, images over 10 MiB,
or images over 10 million pixels to bound local memory and latency.

## 4. Use the Hailo profile with the base hardware adapters

For a voice/photo turn, first complete the base guide's button, microphone,
speaker, Whisper, Piper, and camera checks. In terminal B, rerun the short
`PI_TEST_MIC`, `PI_TEST_SPEAKER`, `PI_PIPER_BIN`, and `PI_BUTTON_DEVICE` blocks
from [base-guide section 4](raspberry-pi.md#4-five-to-ten-minute-hardware-check);
shell variables do not survive a new SSH session. Then keep terminal A running
and launch terminal B:

```bash
cd "$HOME/code/independence"
INDEPENDENCE_PIPER_COMMAND="$PI_PIPER_BIN" \
  ./target/release/independence --voice \
  --profile raspberry-pi-hailo \
  --input buttons \
  --input-device "$PI_BUTTON_DEVICE" \
  --audio-input "$PI_TEST_MIC" \
  --audio-output "$PI_TEST_SPEAKER"
```

The `raspberry-pi-hailo` profile selects backend `hailo-vlm`,
`http://127.0.0.1:11435`, and `qwen2-vl:2b`. If you deliberately change the
bridge port, set the matching local endpoint for Independence:

```bash
# terminal A
HAILO_VLM_PORT=11500 HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/run_pi_hailo_vlm_bridge.sh

# terminal B
INDEPENDENCE_HAILO_VLM_URL='http://127.0.0.1:11500' \
INDEPENDENCE_PIPER_COMMAND="$PI_PIPER_BIN" \
./target/release/independence --voice \
  --profile raspberry-pi-hailo \
  --input buttons \
  --input-device "$PI_BUTTON_DEVICE" \
  --audio-input "$PI_TEST_MIC" \
  --audio-output "$PI_TEST_SPEAKER"
```

Run the base guide's physical acceptance controls with this profile. For the
Hailo-specific multi-image proof, request two fresh `P` photos during one held
Talk turn and ask a comparison question. The expected pipeline is:

```text
local microphone -> local Whisper -> /api/show vision check -> fresh camera still(s)
-> 127.0.0.1 Hailo bridge -> local Hailo Qwen2-VL -> local Piper
```

The bridge forwards text and image bytes only. It does not transcribe,
translate, choose a spoken language, or save photos.

## Optional safe bridge telemetry

To opt in to local timing/count metadata only, start terminal A this way:

```bash
cd "$HOME/code/independence"
mkdir -p local-data/telemetry
HAILO_VLM_TELEMETRY_JSONL="$PWD/local-data/telemetry/hailo-vlm.jsonl" HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/run_pi_hailo_vlm_bridge.sh
```

The JSONL contains route, method/status, outcome, duration, model alias, and
message/image counts and byte counts—not prompt text, transcripts, image data,
model responses, or HEF paths. Terminal output and the operating system remain
outside that narrow guarantee.

## Completion checklist

| Gate | Required proof |
| --- | --- |
| Base | a typed local Ollama answer from the base guide |
| Power | `scripts/check_pi_hailo_power.sh` passes before and after the bundled-image chat |
| 1 | `hailortcli fw-control identify` reports Hailo-10H |
| 2 | preflight prints exactly one Qwen2-VL HEF path |
| 3 | health, show/vision, and bundled-image chat succeed |
| 4 | a fresh two-photo comparison receives a local answer |

Automated tests validate the bridge's request mapping and limits, but they do
not prove this Pi's HAT, model, camera, microphone, or speaker. Record only
the checks that actually passed on this device.
