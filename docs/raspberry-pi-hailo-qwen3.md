# Raspberry Pi AI HAT+ 2: HailoRT 5.3 + Qwen3-VL-2B-Instruct

Use this on a fresh Raspberry Pi OS card after [first boot](raspberry-pi.md).
It installs one consistent HailoRT 5.3 stack, proves the Hailo-10H is visible,
downloads the Qwen3-VL-2B-Instruct HEF, and proves one local image request.
It does **not** claim that the current Independence Qwen2 bridge can serve
Qwen3.

## Overview

| Gate | Done when |
| --- | --- |
| 1. Hailo host stack | The AI HAT+ 2 reports one Hailo-10H running 5.3.0 |
| 2. Model artifact | The Qwen3 HEF is stored locally and non-empty |
| 3. Standalone inference | One bundled image produces non-empty Qwen3 response text |
| 4. Integration boundary | The model's one-image contract is understood before code changes |

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

## 3. Prove one standalone Qwen3 image request

This is the required acceptance test. It creates no Independence script and
does not start a server: it uses Hailo Apps' existing simple VLM example,
passes the explicit Qwen3 HEF, and uses Hailo Apps' bundled non-personal
barcode image. The image, prompt, and response stay on the Pi.

The commands pin Hailo Apps to release `26.03.1`, which explicitly supports
HailoRT 5.3.0. Its simple VLM example is labelled Qwen2 and hard-codes Qwen2's
`336 × 336` preprocessing. They create a dedicated Python virtual environment,
make a backed-up **temporary** Qwen3 size change, run exactly one request, then
restore the original source file. Do not run this while another Hailo GenAI
model or bridge is running.

```bash
cd "$HOME/code"
HAILO_APPS_VERSION='26.03.1'
HAILO_APPS_DIR="$HOME/code/hailo-apps-$HAILO_APPS_VERSION"
HAILO_SMOKE_VENV="$HOME/.venvs/hailo-qwen3-smoke"
test ! -e "$HAILO_APPS_DIR"
mkdir -p "$HOME/.venvs"
test ! -e "$HAILO_SMOKE_VENV"

sudo apt-get update
sudo apt-get install -y git python3-venv python3-opencv python3-numpy

git clone --depth 1 --branch "$HAILO_APPS_VERSION" \
  https://github.com/hailo-ai/hailo-apps.git "$HAILO_APPS_DIR"
git -C "$HAILO_APPS_DIR" describe --tags --exact-match

python3 -m venv --system-site-packages "$HAILO_SMOKE_VENV"
"$HAILO_SMOKE_VENV/bin/python" --version
"$HAILO_SMOKE_VENV/bin/python" -m pip install \
  https://dev-public.hailo.ai/2026_04/Hailo10/hailort-5.3.0-cp313-cp313-linux_aarch64.whl
"$HAILO_SMOKE_VENV/bin/python" -c \
  'from hailo_platform import VDevice; from hailo_platform.genai import VLM; import cv2, numpy; print("Hailo VLM imports OK")'
```

`git describe` must print `26.03.1`, and the virtual environment must report
Python 3.13.x; the HailoRT wheel is built for that Python version. The wheel
belongs in `$HAILO_SMOKE_VENV`, not Independence's Piper environment. This
test runs the upstream source file directly, so it intentionally does not call
Hailo Apps' broader `install.sh` or `setup_env.sh`. The final import command
must print `Hailo VLM imports OK`.

The next block modifies only the upstream example file after first confirming
that it has no existing local changes. Keep using the same terminal so the
paths defined above remain available. If that `git diff` command prints a
diff, stop instead of overwriting somebody else's Hailo Apps change.

```bash
QWEN3_VL_HEF="$HOME/models/hailo10h/Qwen3-VL-2B-Instruct.hef"
test -r "$QWEN3_VL_HEF"
SMOKE_RELATIVE="hailo_apps/python/gen_ai_apps/simple_vlm_chat/simple_vlm_chat.py"
SMOKE_FILE="$HAILO_APPS_DIR/hailo_apps/python/gen_ai_apps/simple_vlm_chat/simple_vlm_chat.py"
git -C "$HAILO_APPS_DIR" diff --exit-code -- "$SMOKE_RELATIVE"
test ! -e "$SMOKE_FILE.qwen3-smoke-backup"

cp "$SMOKE_FILE" "$SMOKE_FILE.qwen3-smoke-backup"
sed -i \
  -e 's/(336, 336)/(512, 288)/' \
  -e 's/336x336/512x288/g' \
  "$SMOKE_FILE"
grep -nE '512, 288|512x288' "$SMOKE_FILE"

"$HAILO_SMOKE_VENV/bin/python" "$SMOKE_FILE" \
  --hef-path "$QWEN3_VL_HEF"
```

Pass only if the output says `Hailo device initialized`, `Model loaded
successfully`, and `Example completed successfully`, with non-empty text below
`Response received`. The bundled test picture contains one person, so a
response that identifies one person is a useful sanity check, although its
exact wording can vary. This is a real Hailo Qwen3 image request, not merely a
file-existence check.

Restore the Hailo Apps example after the request—whether it passed or failed—
then confirm power remained healthy:

```bash
mv "$SMOKE_FILE.qwen3-smoke-backup" "$SMOKE_FILE"
git -C "$HAILO_APPS_DIR" diff --exit-code -- "$SMOKE_RELATIVE"
vcgencmd get_throttled
```

The last command must report `throttled=0x0`. A response with a nonzero
throttling value is not an accepted hardware test; correct the power source,
cold-boot, and rerun Gate 3.

## 4. Model contract for the future adapter

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

## 5. What to do after the standalone test

After Gates 1–3, the Pi has a matching host stack, the model file, and proof
that Qwen3 generated a response from one image. Do **not** point the existing
commands below at the new HEF:

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

The Hailo Apps test is a deliberately temporary upstream edit, not the
Independence implementation. The backup/restore step keeps its working tree
clean; do not make the changed file a permanent local fork.

## Sources

- [Pinned 5.3 system setup](https://github.com/gregm123456/raspberry_pi_hailo_ai_services/blob/cad57e09a4b454cebecd28dfe2df9c8fb66ad3c3/reference_documentation/system_setup.md)
- [Hailo GenAI v5.3 release notes](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/v5.3.0/docs/CHANGELOG.rst)
- [Hailo model table](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/main/docs/MODELS.rst)
- [Hailo-Ollama API limits](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/main/docs/USAGE.rst)
- [Hailo Apps 26.03.1 release](https://github.com/hailo-ai/hailo-apps/releases/tag/26.03.1)
- [Hailo Apps simple VLM example at 26.03.1](https://github.com/hailo-ai/hailo-apps/blob/26.03.1/hailo_apps/python/gen_ai_apps/simple_vlm_chat/simple_vlm_chat.py)
- [512 × 288 Qwen3-VL discussion with Hailo staff](https://community.hailo.ai/t/qwen3-vl-2b-instruct-hef-fails-with-hailo-invalid-operation-6-on-hailort-5-3-0-generate-impl-fails-despite-successful-model-load/19104)
