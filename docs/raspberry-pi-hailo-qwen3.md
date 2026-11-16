# Raspberry Pi AI HAT+ 2: HailoRT 5.3 + Qwen3-VL-2B-Instruct

Use this on a fresh Raspberry Pi OS card after [first boot](raspberry-pi.md).
It installs one consistent HailoRT 5.3 stack, proves the Hailo-10H is visible,
downloads the Qwen3-VL-2B-Instruct HEF, and proves one local image request.
It now includes the migrated Independence Qwen3 bridge path after the
standalone acceptance test.

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

## 1. Install the verified 5.3 system stack

The installation instructions in this section were taken from the pinned
external [AI HAT+ 2 system setup guide — commit
`cad57e0`](https://github.com/gregm123456/raspberry_pi_hailo_ai_services/blob/cad57e09a4b454cebecd28dfe2df9c8fb66ad3c3/reference_documentation/system_setup.md),
then modified to produce a successful current Raspberry Pi OS Trixie
installation.
Its plain HailoRT 5.3 PCIe-driver package does not compile on the current Pi
6.18 kernel because it calls the removed `del_timer_sync` kernel function.
This runbook applies Hailo's narrowly scoped upstream compatibility fix before
finishing that package.

Do **not** install the external guide's older Option A (`hailo-h10-all`) on
this card, and do not mix its 5.1 packages with this 5.3 setup.

### 1.1 Update the Pi first

Update Raspberry Pi OS and firmware before choosing the kernel headers used to
build the driver, then reconnect after the reboot.

```bash
sudo apt-get update
sudo apt-get full-upgrade -y
sudo rpi-eeprom-update -a
sudo reboot
```

After reconnecting, confirm that this is a 64-bit Trixie Pi kernel with
matching headers available:

```bash
uname -r
test -d "/lib/modules/$(uname -r)/build"
```

The second command must exit successfully. The verified path below used
`6.18.60+rpt-rpi-2712`.

### 1.2 Install build prerequisites and download matching packages

DKMS builds the PCIe kernel module for the exact running kernel. The three
packages below are the matching HailoRT 5.3.0 driver, runtime, and TAPPAS core.

```bash
sudo apt-get install -y build-essential dkms git wget \
  "linux-headers-$(uname -r)"
command -v dkms

HAILO_5_3_DIR="$HOME/downloads/hailo-5.3.0"
mkdir -p "$HAILO_5_3_DIR"
cd "$HAILO_5_3_DIR"
wget --continue --show-progress \
  https://dev-public.hailo.ai/2026_04/Hailo10/hailort-pcie-driver_5.3.0_all.deb
wget --continue --show-progress \
  https://dev-public.hailo.ai/2026_04/Hailo10/hailort_5.3.0_arm64.deb
wget --continue --show-progress \
  https://dev-public.hailo.ai/2026_04/Hailo10/hailo-tappas-core_5.3.0_arm64.deb

test -s "$HAILO_5_3_DIR/hailort-pcie-driver_5.3.0_all.deb"
test -s "$HAILO_5_3_DIR/hailort_5.3.0_arm64.deb"
test -s "$HAILO_5_3_DIR/hailo-tappas-core_5.3.0_arm64.deb"
```

`command -v dkms` must print a path. A local-package warning about the `_apt`
user being unable to read a file in your home directory is not the driver
failure; apt falls back to root to read the local file you explicitly supplied.

### 1.3 Unpack the known-broken driver source once

On kernel 6.18, the following driver-install command is expected to fail with
`del_timer_sync` and leave `hailort-pcie-driver` half-configured. That failure
unpacks the source and firmware under `/usr/src`, which is what the next step
repairs. Do not run `dpkg --configure -a` yet.

```bash
sudo apt-get install -y "$HAILO_5_3_DIR/hailort-pcie-driver_5.3.0_all.deb"

sudo grep -n 'del_timer_sync' /var/log/hailort-pcie-driver.deb.log
dpkg -l hailort-pcie-driver
```

Proceed only if the log shows the expected compiler error and the package status
starts with `iF`. A different error means stop and diagnose that error rather
than applying this kernel-specific fix.

### 1.4 Apply Hailo's kernel compatibility fix and finish the driver

Hailo's open upstream PR `#52`, commit `bf5b66b`, changes the compatibility
helper and VDMA monitor source while retaining driver version 5.3.0. These
commands confirm both source trees are 5.3.0, back up the original package
source, replace only those two files, and let `dpkg` complete the package.

```bash
mkdir -p "$HOME/code"
HAILO_DRIVER_FIX_DIR="$HOME/code/hailort-drivers-pr52"
test ! -e "$HAILO_DRIVER_FIX_DIR"
git clone https://github.com/hailo-ai/hailort-drivers.git "$HAILO_DRIVER_FIX_DIR"
git -C "$HAILO_DRIVER_FIX_DIR" fetch origin pull/52/head:pr-52
git -C "$HAILO_DRIVER_FIX_DIR" checkout --detach pr-52
git -C "$HAILO_DRIVER_FIX_DIR" rev-parse --short HEAD

grep -E '^#define HAILO_DRV_VER_(MAJOR|MINOR|REVISION)' \
  "$HAILO_DRIVER_FIX_DIR/common/hailo_ioctl_common.h"
sudo grep -E '^#define HAILO_DRV_VER_(MAJOR|MINOR|REVISION)' \
  /usr/src/hailort-pcie-driver/common/hailo_ioctl_common.h

sudo test ! -e /usr/src/hailort-pcie-driver.before-kernel-6.18-fix
sudo cp -a /usr/src/hailort-pcie-driver \
  /usr/src/hailort-pcie-driver.before-kernel-6.18-fix
sudo install -m 0644 \
  "$HAILO_DRIVER_FIX_DIR/linux/utils/compact.h" \
  /usr/src/hailort-pcie-driver/linux/utils/compact.h
sudo install -m 0644 \
  "$HAILO_DRIVER_FIX_DIR/linux/vdma/monitor.c" \
  /usr/src/hailort-pcie-driver/linux/vdma/monitor.c

sudo dpkg --configure hailort-pcie-driver
```

The checkout must identify `bf5b66b`, and both version checks must print
major `5`, minor `3`, revision `0`. The final command must print `Hailo PCIe
Driver Installed!`. The backup is intentional: keep it until the full
acceptance gate passes. Do not run the final `dpkg --configure` command again;
an `already installed and configured` response only means there is nothing left
to configure.

### 1.5 Install the runtime, reboot, and prove the whole stack

Install the two remaining matching packages, confirm no package remains
half-configured, then reboot so the new DKMS module and Hailo firmware load.

```bash
sudo apt-get install -y "$HAILO_5_3_DIR/hailort_5.3.0_arm64.deb"
sudo apt-get install -y "$HAILO_5_3_DIR/hailo-tappas-core_5.3.0_arm64.deb"
sudo dpkg --audit
sudo reboot
```

After reconnecting, run every check:

```bash
dpkg -l hailort hailort-pcie-driver hailo-tappas-core
dkms status
modinfo hailo1x_pci | grep -E '^(filename|version):'
hailortcli fw-control identify
vcgencmd get_throttled
```

This exact gate was successfully verified on a Raspberry Pi 5 / AI HAT+ 2
running kernel `6.18.60+rpt-rpi-2712`. Pass only when all of these are true:

| Check | Required result |
| --- | --- |
| Packages | `ii` for `hailort`, `hailort-pcie-driver`, and `hailo-tappas-core`, all at 5.3.0 |
| DKMS | `hailo1x_pci/5.3.0, 6.18.60+rpt-rpi-2712, aarch64: installed` (substitute the current kernel if it differs) |
| Module | A filename below `/lib/modules/<kernel>/updates/dkms/` and `version: 5.3.0` |
| Device | `Firmware Version: 5.3.0` and `Device Architecture: HAILO10H` |
| Power | `throttled=0x0` |

Do not download the model or proceed to the Python test if any check fails.

### What the driver repair means

`hailort-pcie-driver` provides both Hailo firmware and the Linux PCIe kernel
module. DKMS compiles that module when the package is configured and installs
it under `/lib/modules/<kernel>/updates/dkms/`. Linux 6.18 rejects the old
`del_timer_sync` call; the Hailo fix maps it to the supported current kernel
function without changing the driver ABI, so the resulting module, HailoRT,
and firmware all remain version 5.3.0.

This is experimental because PR #52 is not merged into a Hailo release. Do not
run `apt reinstall hailort-pcie-driver` without repeating this repair: a
reinstall restores the broken source. After a future kernel upgrade, run the
five acceptance commands above before relying on the HAT.

### Why Qwen3 remains experimental

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

sudo apt-get install -y python3-venv python3-opencv python3-numpy

git clone --depth 1 --branch "$HAILO_APPS_VERSION" \
  https://github.com/hailo-ai/hailo-apps.git "$HAILO_APPS_DIR"
git -C "$HAILO_APPS_DIR" describe --tags --exact-match

python3 -m venv --system-site-packages "$HAILO_SMOKE_VENV"
"$HAILO_SMOKE_VENV/bin/python" --version
"$HAILO_SMOKE_VENV/bin/python" -m pip install \
  python-dotenv \
  https://dev-public.hailo.ai/2026_04/Hailo10/hailort-5.3.0-cp313-cp313-linux_aarch64.whl
"$HAILO_SMOKE_VENV/bin/python" -c \
  'from dotenv import load_dotenv; from hailo_platform import VDevice; from hailo_platform.genai import VLM; import cv2, numpy; print("Hailo Apps and Hailo VLM imports OK")'
```

`git describe` must print `26.03.1`, and the virtual environment must report
Python 3.13.x; the HailoRT wheel is built for that Python version. The wheel
belongs in `$HAILO_SMOKE_VENV`, not Independence's Piper environment. This
test runs the upstream source file directly, so it intentionally does not call
Hailo Apps' broader `install.sh` or `setup_env.sh`. The final import command
must print `Hailo Apps and Hailo VLM imports OK`. `python-dotenv` is required
because the Hailo Apps example imports Hailo Apps' shared environment loader;
installing only the HailoRT wheel does not install Hailo Apps' Python
dependencies. If you already created the venv and reached
`ModuleNotFoundError: No module named 'dotenv'`, install the missing package
without recreating the venv, then rerun the import check:

```bash
"$HAILO_SMOKE_VENV/bin/python" -m pip install python-dotenv
"$HAILO_SMOKE_VENV/bin/python" -c \
  'from dotenv import load_dotenv; from hailo_platform import VDevice; from hailo_platform.genai import VLM; import cv2, numpy; print("Hailo Apps and Hailo VLM imports OK")'
```

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

## 5. Use the migrated Qwen3 bridge

After Gates 1–3, the Pi has a matching host stack, the model file, and proof
that Qwen3 generated a response from one image. The Independence bridge now
uses the same Qwen3 implementation: direct BGR-to-RGB conversion, resize to
512 × 288, the Hailo Apps prompt structure, and `generate_all` with the smoke
test's generation parameters.

```bash
scripts/run_pi_hailo_vlm_bridge.sh
tools/hailo_vlm_bridge.py
```

Start the bridge with the explicit Qwen3 HEF:

```bash
HAILO_VLM_HEF="$QWEN3_VL_HEF" \
  HAILO_APPS_DIR="$HOME/code/hailo-apps" \
  scripts/run_pi_hailo_vlm_bridge.sh
```

It serves `qwen3-vl:2b-instruct` on loopback port `11435`, advertises vision,
and rejects requests containing more than one image. The existing Rust camera
permission and JPEG transport remain in use.

The smoke-test generation settings are startup-configurable. Defaults remain
`temperature=0.1`, `seed=42`, and `max-generated-tokens=200`:

```bash
HAILO_VLM_HEF="$QWEN3_VL_HEF" \
  HAILO_APPS_DIR="$HOME/code/hailo-apps" \
  scripts/run_pi_hailo_vlm_bridge.sh \
  --temperature 0.2 --seed 42 --max-generated-tokens 128
```

Temperature accepts `0` through `2`; token count must be positive. These values
affect generation only and are reported nowhere with prompt or image content.

The Hailo Apps test remains a deliberately temporary upstream edit, not the
Independence implementation. Restore that source file after the smoke test;
the migrated bridge contains the Qwen3-specific implementation locally.

## Sources

- [Source 5.3 system setup guide (adapted above)](https://github.com/gregm123456/raspberry_pi_hailo_ai_services/blob/cad57e09a4b454cebecd28dfe2df9c8fb66ad3c3/reference_documentation/system_setup.md)
- [Hailo kernel-6.18 driver compatibility fix, PR #52 / commit bf5b66b](https://github.com/hailo-ai/hailort-drivers/pull/52)
- [Hailo GenAI v5.3 release notes](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/v5.3.0/docs/CHANGELOG.rst)
- [Hailo model table](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/main/docs/MODELS.rst)
- [Hailo-Ollama API limits](https://github.com/hailo-ai/hailo_model_zoo_genai/blob/main/docs/USAGE.rst)
- [Hailo Apps 26.03.1 release](https://github.com/hailo-ai/hailo-apps/releases/tag/26.03.1)
- [Hailo Apps simple VLM example at 26.03.1](https://github.com/hailo-ai/hailo-apps/blob/26.03.1/hailo_apps/python/gen_ai_apps/simple_vlm_chat/simple_vlm_chat.py)
- [512 × 288 Qwen3-VL discussion with Hailo staff](https://community.hailo.ai/t/qwen3-vl-2b-instruct-hef-fails-with-hailo-invalid-operation-6-on-hailort-5-3-0-generate-impl-fails-despite-successful-model-load/19104)
