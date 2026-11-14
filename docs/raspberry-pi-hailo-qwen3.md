# Raspberry Pi AI HAT+ 2: HailoRT 5.3 + Qwen3-VL-2B-Instruct

Use this on a fresh Raspberry Pi OS card after [first boot](raspberry-pi.md).
It installs one consistent HailoRT 5.3 stack, proves the Hailo-10H is visible,
and downloads the Qwen3-VL-2B-Instruct HEF. It does **not** claim that the
current Independence Qwen2 bridge can serve Qwen3.

## Overview

| Gate | Done when |
| --- | --- |
| 1. Hailo host stack | The AI HAT+ 2 reports one Hailo-10H running 5.3.0 |
| 2. Model artifact | The Qwen3 HEF is stored locally and non-empty |
| 3. Integration boundary | The model's one-image contract is understood before code changes |

Use a well-cooled Pi and a verified 5 V / 5 A USB-C supply. After a cold boot,
run `vcgencmd get_throttled`; it must report `throttled=0x0`. A nonzero value
means under-voltage or throttling occurred during that boot, even if the Pi
currently appears healthy.

## 1. Install the exact 5.3 system stack

Complete the pinned external guide through **Option B: Install 5.3.0 from Hailo
dev-public**, then reboot and run its verification step:

[AI HAT+ 2 System Setup for Raspberry Pi 5 — pinned commit
`cad57e0`](https://github.com/gregm123456/raspberry_pi_hailo_ai_services/blob/cad57e09a4b454cebecd28dfe2df9c8fb66ad3c3/reference_documentation/system_setup.md)

That guide owns the OS/firmware update and the three matching 5.3.0 packages:
the PCIe driver, HailoRT, and TAPPAS core. Do **not** follow its older Option A
(`hailo-h10-all`) on this card, and do not mix 5.1 packages from the Qwen2
runbook with this 5.3 setup.

Back on the Pi, prove the installed stack before downloading a 3 GiB model:

```bash
hailortcli fw-control identify
dpkg-query -W -f='${Package}\t${Version}\n' \
  hailort hailort-pcie-driver hailo-tappas-core
dmesg | grep -i hailo
```

`hailortcli` must identify `Device Architecture: HAILO10H` and report firmware
`5.3.0`. If it does not, stop here and fix the physical HAT, power, cooling, or
the external guide's first failing command. Do not install Hailo Apps or
download a HEF to diagnose a missing accelerator.

### Why this is marked experimental

Hailo's v5.3 release notes say that Qwen3-VL-2B-Instruct was added in v5.3.0,
but the current model table labels its version compatibility `> 5.3.0`. That is
not a clean compatibility promise. This guide deliberately uses the matching
v5.3 artifact below and requires a separate inference proof before any
Independence integration; do not silently substitute a 5.4 or 5.5 component.

## 2. Download the Qwen3 model artifact

The HEF is a precompiled Hailo executable, not an Ollama model. It is roughly
3 GiB on disk, so check free space before starting. The command is resumable if
the connection drops.

```bash
sudo apt install -y wget
df -h "$HOME"

mkdir -p "$HOME/models/hailo10h"
cd "$HOME/models/hailo10h"
wget --continue --show-progress \
  https://dev-public.hailo.ai/v5.3.0/blob/Qwen3-VL-2B-Instruct.hef

QWEN3_VL_HEF="$HOME/models/hailo10h/Qwen3-VL-2B-Instruct.hef"
test -s "$QWEN3_VL_HEF"
du -h "$QWEN3_VL_HEF"
```

Do not begin the download with less than 4 GiB available in the `Avail` column.
The final two commands prove that a non-empty local artifact exists; they do
not prove that the model can infer, and this guide does not provide a checksum
to compare.

The model and its download are local after this step. No photograph, prompt,
or generated response is sent to Hailo merely by storing the HEF.

## 3. Model contract for the future adapter

| Property | Qwen3-VL-2B-Instruct v5.3 behavior |
| --- | --- |
| Input | One still image per request |
| Video | Not supported in this v5.3 release |
| Image preprocessing | RGB, exactly 512 × 288 pixels |
| Runtime API | Hailo C++ or Python API |
| Hailo-Ollama | Not supported for VLMs |

The single-frame/no-video limit comes from Hailo's v5.3 release notes. Hailo
community testing, acknowledged by Hailo staff, found that this HEF requires
`512 × 288` rather than Qwen2's `336 × 336` preprocessing. Do not reuse the
Qwen2 size or send multiple photos.

Hailo-Ollama is intentionally not part of this path: Hailo documents its
Ollama-compatible REST API as LLM-only, not usable for vision-language models.

## 4. What to do after the download

At this point Gates 1–2 are complete: the Pi has a matching host stack and the
model file. Do **not** point the existing commands below at the new HEF:

```bash
scripts/run_pi_hailo_vlm_bridge.sh
tools/hailo_vlm_bridge.py
```

They are current Qwen2-only code: they preprocess square 336 × 336 images and
accept up to four images. Overriding `HAILO_VLM_HEF` there would be a misleading
test, not a Qwen3 deployment.

The next work is a small, explicit Qwen3 adapter milestone: load this exact
HEF through the Hailo Python API, resize one photo to 512 × 288 RGB, use the
model's prompt contract, prove one bundled non-personal image, and then expose
its actual capabilities to Independence (`vision`, `max_images=1`, and
`video=false`). That work needs an owner-approved implementation plan and a
real-device test before it replaces the working Qwen2 path.

The Hailo Apps simple VLM example is useful Qwen2 reference code, but its
current preprocessing is also 336 × 336. Treat it as a reference, not a
drop-in Qwen3 command or a reason to edit untracked upstream files.

## Sources

- [Pinned 5.3 system setup](https://github.com/gregm123456/raspberry_pi_hailo_ai_services/blob/cad57e09a4b454cebecd28dfe2df9c8fb66ad3c3/reference_documentation/system_setup.md)
- [Hailo GenAI v5.3 release notes](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/v5.3.0/docs/CHANGELOG.rst)
- [Hailo model table](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/main/docs/MODELS.rst)
- [Hailo-Ollama API limits](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/main/docs/USAGE.rst)
- [512 × 288 Qwen3-VL discussion with Hailo staff](https://community.hailo.ai/t/qwen3-vl-2b-instruct-hef-fails-with-hailo-invalid-operation-6-on-hailort-5-3-0-generate-impl-fails-despite-successful-model-load/19104)
