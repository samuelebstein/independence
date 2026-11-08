# Raspberry Pi + Hailo VLM bridge runbook

This runbook connects a Raspberry Pi with a Hailo-10H-backed AI HAT+ 2 to
Independence's existing voice-and-photo path. It is an implemented, local-only
integration—not a claim that `hailo-ollama` can serve a vision model.

The bridge is deliberately small:

```text
Pi Camera still + spoken question
          |
          v
Independence CameraService + Whisper
          |
          | existing non-streaming Ollama-shaped /api/chat request
          | (UTF-8 text + up to four ordered base64 images)
          v
127.0.0.1:11435  tools/hailo_vlm_bridge.py
          |
          | decode -> RGB central crop -> Hailo VLM API
          v
one local Qwen2-VL HEF on the Hailo accelerator
          |
          v
same /api/chat-shaped response -> existing Controller -> Piper
```

`Controller` still owns accepted history and turn authorization. `VoiceRuntime`
still owns the microphone/camera draft. The Python process only owns one Hailo
VLM instance and its local HTTP translation. There is no cloud fallback.

## What runs where

| Component | Process | Address / ownership |
| --- | --- | --- |
| Hailo VLM bridge | `tools/hailo_vlm_bridge.py` | **`http://127.0.0.1:11435`** by default; numeric loopback only |
| Capability discovery | bridge `POST /api/show` | returns `{"capabilities":["vision"]}` |
| Inference | Hailo Apps Python `VLM` API | one pre-downloaded Qwen2-VL HEF; context cleared after each request |
| Main application | Independence | profile `raspberry-pi-hailo`, backend `hailo-vlm` |

Port `11435` is intentionally different from ordinary Ollama's `11434`.
The bridge prints its exact address, model alias, and capability at startup.
Use `GET /healthz` to verify the same values without sending a prompt or image.

The profile's logical model name is `qwen2-vl:2b`. It is a bridge alias, not
an Ollama tag: the bridge maps it to the local Hailo Qwen2-VL 2B HEF selected
by `HAILO_VLM_HEF`.

## Prerequisites and hardware check

This bridge needs a Hailo-10H runtime, Hailo Apps' GenAI dependencies, and a
downloaded VLM model. It does not use Hailo-Ollama for vision. Hailo's current
GenAI documentation lists VLMs under the Python/C++ API rather than its
Ollama-serving path; [Hailo Apps' GenAI guide](https://github.com/hailo-ai/hailo-apps/blob/main/hailo_apps/python/gen_ai_apps/README.md)
and [VLM example](https://github.com/hailo-ai/hailo-apps/blob/main/hailo_apps/python/gen_ai_apps/simple_vlm_chat/simple_vlm_chat.py)
are the upstream references this bridge follows.

Before installing anything, verify that the Pi sees the attached Hailo device:

```bash
hailortcli fw-control identify
```

The output must identify a Hailo-10H-capable device. Check cooling, power, and
the HAT installation if it does not. Do not start Hailo-Ollama and this VLM
bridge as two GenAI workloads at once: Hailo documents GenAI + GenAI as an
unsupported concurrent combination. Stop the other GenAI process before
starting the bridge. Ordinary Hailo vision pipelines use the shared virtual
device group, but throughput still needs a real device test.

## One-time Hailo Apps setup

The following networked steps are explicit. They clone Hailo Apps, install its
dependencies, and download a model only because you run the commands; neither
Independence nor the bridge silently downloads a model at request time.

```bash
cd "$HOME/code"
git clone https://github.com/hailo-ai/hailo-apps.git
cd hailo-apps
./install.sh
source setup_env.sh
python3 -m pip install -e '.[gen-ai]'
hailo-download-resources --group vlm_chat --arch hailo10h
```

If `source setup_env.sh` says `venv_hailo_apps` is missing, as in the setup
attempt that prompted this work, `./install.sh` did not finish or was not run
from the Hailo Apps checkout. Run it first, then source `setup_env.sh` again.
The `pip` command installs the GenAI Python extras that `vlm_chat` and this
bridge need.

From a freshly pulled Independence checkout, the checked-in helper performs a
safe preflight. It changes Python dependencies or downloads model data only
when its named flags are present:

```bash
cd "$HOME/code/independence"
HAILO_APPS_DIR="$HOME/code/hailo-apps" \
  scripts/prepare_pi_hailo_vlm.sh \
  --install-python-dependencies \
  --download-model
```

Without flags it only verifies the environment:

```bash
HAILO_APPS_DIR="$HOME/code/hailo-apps" scripts/prepare_pi_hailo_vlm.sh
```

The helper expects exactly one Qwen2-VL 2B HEF under
`/usr/local/hailo/resources/models/hailo10h` (or
`HAILO_RESOURCES_DIR`). If you installed more than one matching VLM, select
the intended model explicitly rather than letting the script guess:

```bash
export HAILO_VLM_HEF='/absolute/path/to/Qwen2-VL-2B-Instruct.hef'
```

The bridge deliberately requires that file path. It does **not** call Hailo
Apps' auto-download resolver, because starting an inference server should not
silently create network traffic or install a model.

This first bridge is implemented and documented for Hailo's Qwen2-VL 2B
resource. `HAILO_VLM_MODEL` changes only the API alias that Independence sends;
it does not switch the HEF or make another VLM's preprocessing compatible. Do
not point `HAILO_VLM_HEF` at Qwen3-VL or another custom HEF until its image
format, prompt contract, and device test have been reviewed.

## Start and verify the bridge

Run the service in a dedicated terminal on the Pi:

```bash
cd "$HOME/code/independence"
scripts/run_pi_hailo_vlm_bridge.sh
```

The wrapper activates `$HAILO_APPS_DIR/setup_env.sh`, discovers one downloaded
Qwen2-VL HEF, and starts the server. Its expected startup lines are:

```text
Hailo VLM bridge listening on http://127.0.0.1:11435
Health: http://127.0.0.1:11435/healthz | model: qwen2-vl:2b | capabilities: vision
```

Verify its address, model, and capability locally:

```bash
curl --fail --silent http://127.0.0.1:11435/healthz | jq .
curl --fail --silent \
  --header 'Content-Type: application/json' \
  --data '{"model":"qwen2-vl:2b"}' \
  http://127.0.0.1:11435/api/show | jq .
ss -ltnp '( sport = :11435 )'
```

The second response must include exactly the `vision` capability Independence
requires. It is not an application-side image override: the normal runtime
will still refuse camera capture if this lookup fails or omits `vision`.

For a direct multi-image API smoke test, use two non-sensitive local JPEGs with
visibly different contents. This is the same base64 image field Independence
already sends to a local Ollama vision model:

```bash
first_image_base64="$(base64 --wrap=0 ./known-test-image-1.jpg)"
second_image_base64="$(base64 --wrap=0 ./known-test-image-2.jpg)"
jq -n --arg first "$first_image_base64" --arg second "$second_image_base64" '{
  model: "qwen2-vl:2b",
  stream: false,
  messages: [
    {role: "system", content: "Answer briefly."},
    {role: "user", content: "Compare these two images.", images: [$first, $second]}
  ]
}' | curl --fail --silent \
  --header 'Content-Type: application/json' \
  --data @- \
  http://127.0.0.1:11435/api/chat | jq .
```

The bridge accepts zero through four images on the final user message and
preserves their order: the first captured `P` photo is the first Hailo frame.
It emits one Hailo image placeholder per frame rather than combining photos.
The four-photo limit and the existing 14 MiB total JSON-request limit bound Pi
memory, accelerator latency, and local API exposure; each decoded image also
has its existing 10 MiB and 10-million-pixel limit. Text-only messages are
still sent as text-only VLM requests; test that path on the installed Hailo
Apps/HailoRT version before relying on it for normal conversation.

To use a different local port or model alias, set these before the wrapper:

```bash
export HAILO_VLM_PORT=11500
export HAILO_VLM_MODEL='qwen2-vl:2b'
export HAILO_VLM_HEF='/absolute/path/to/Qwen2-VL-2B-Instruct.hef'
scripts/run_pi_hailo_vlm_bridge.sh
```

When either port or alias differs from the profile defaults, give Independence
the matching local endpoint and alias for its own launch:

```bash
INDEPENDENCE_HAILO_VLM_URL='http://127.0.0.1:11500' \
INDEPENDENCE_MODEL='qwen2-vl:2b' \
./target/release/independence --voice \
  --profile raspberry-pi-hailo \
  --input buttons \
  --input-device /dev/input/by-id/usb-Example-event-kbd
```

Only `127.0.0.1` or `::1` are accepted for `HAILO_VLM_HOST`; `0.0.0.0`, other
LAN/loopback addresses, and hostnames are rejected. This is deliberate: a
photo/prompt inference API should not accidentally become a network service,
and Independence's own client accepts those same two endpoint hosts.

## Run Independence against it

Build Independence on the Pi, keep the bridge terminal running, then launch
the existing physical-button experience:

```bash
cargo build --release
./target/release/independence --voice \
  --profile raspberry-pi-hailo \
  --input buttons \
  --input-device /dev/input/by-id/usb-Example-event-kbd \
  --audio-input 'Exact CPAL input id' \
  --audio-output 'Exact CPAL output id'
```

`raspberry-pi-hailo` resolves these defaults:

| Setting | Default |
| --- | --- |
| backend | `hailo-vlm` |
| endpoint | `http://127.0.0.1:11435` |
| model alias | `qwen2-vl:2b` |
| input | physical buttons (requires the explicit device path) |
| camera backend | `rpicam-still` |
| pipeline | existing `transcribe-and-speak` |

The normal precedence remains explicit CLI choice, then environment, then
profile default. `--backend hailo-vlm` may also be selected without the
profile. `INDEPENDENCE_HAILO_VLM_URL` is the bridge-specific endpoint override;
the compatible legacy `OLLAMA_URL` has higher precedence and remains useful for
an intentional one-launch endpoint override. Both must be **loopback** URLs.
For example, this runs the Hailo backend with an explicit endpoint/model:

```bash
OLLAMA_URL='http://127.0.0.1:11435' \
./target/release/independence --voice \
  --profile raspberry-pi \
  --backend hailo-vlm \
  --model qwen2-vl:2b \
  --input buttons \
  --input-device /dev/input/by-id/usb-Example-event-kbd
```

Hold the physical Talk/Enter button to record, press `P` once while still
holding it to request one fresh in-memory camera still, then release Talk.
Independence transcribes locally, verifies the bridge's `/api/show` result,
encodes the still, and sends the normal request to the loopback bridge.

The bridge forwards UTF-8 question text unchanged; it does not transcribe,
translate, choose a spoken language, or send audio to Hailo. The stock
Independence Whisper model is `ggml-base.en.bin`, so default speech recognition
is English-only. A separately installed multilingual local Whisper model can
be selected with `INDEPENDENCE_WHISPER_MODEL`; verify that model locally before
depending on another spoken language. There is no remote translation fallback.

`--pipeline audio-to-audio --backend hailo-vlm` is intentionally unsupported
in this milestone. The bridge offers a documented text/vision contract only;
Independence refuses before it opens a microphone for a made-up direct-audio
API.

## Safe API telemetry

Independence already records its existing request-preparation and HTTP timing
metadata. The bridge can additionally write local JSONL metadata when you
choose a path in an existing directory:

```bash
mkdir -p local-data/telemetry
HAILO_VLM_TELEMETRY_JSONL="$PWD/local-data/telemetry/hailo-vlm.jsonl" \
  scripts/run_pi_hailo_vlm_bridge.sh
```

Each API-call event has only the route, HTTP method/status, outcome, duration,
model alias, message count, image count, and image byte count. Generation time
is included when available. It never contains question text, transcript,
system prompt, base64 image, decoded image, model response, or HEF path.
Normal stdout still shows final Independence answers and transcripts, and the
operating system, other local processes, dependencies, removable storage, and
backups remain outside this bridge's privacy boundary.

The existing Rust transport telemetry retains `ollama.*` names because the
bridge deliberately speaks that narrow request/response protocol. Its server
port (`11435`) and model alias distinguish this bridge; the Python records use
the `hailo_vlm.*` event names.

## Troubleshooting

| Symptom | Check / recovery |
| --- | --- |
| `venv_hailo_apps not found` | Run `./install.sh` inside Hailo Apps, then source `setup_env.sh`; do not source an uncreated environment. |
| `Hailo Apps Python dependencies are unavailable` | Run `scripts/prepare_pi_hailo_vlm.sh --install-python-dependencies`. |
| no HEF / zero or many matches | Run `--download-model`; if there are multiple VLM HEFs, set `HAILO_VLM_HEF` to the exact intended file. |
| `could not bind http://127.0.0.1:11435` | Inspect `ss -ltnp '( sport = :11435 )'`, stop only the identified conflicting local process, or choose a different `HAILO_VLM_PORT`. |
| capability remains unknown in Independence | Check the bridge is running, then call `POST /api/show` with `qwen2-vl:2b`; do not force camera support. |
| Hailo device or resource error | Stop the concurrent Hailo-Ollama/other GenAI process, rerun `hailortcli fw-control identify`, and confirm the Hailo Apps/HailoRT versions match the downloaded model. |
| a request with more than four photos is rejected | The bridge has a deliberate four-photo limit. Split the comparison into separate turns or reduce the selected photos. |

## Physical acceptance sequence

The automated tests validate request shape, ordered multi-image mapping,
capability advertisement, the four-photo boundary, and telemetry redaction
without Hailo hardware. On the Pi, complete these checks before treating the
integration as verified:

1. Run the bridge and confirm `/healthz` and `/api/show` on port `11435`.
2. Send the known, non-sensitive JPEG smoke test and confirm a non-empty local
   response.
3. Launch `raspberry-pi-hailo`, complete the five-second physical Ephemeral
   Mode entry gesture, then perform one spoken turn with two `P` photos and a
   comparison question. Confirm the answer distinguishes their capture order.
4. Confirm `local-data/telemetry/hailo-vlm.jsonl`, if enabled, has counts and
   timings but none of the known question/image/answer strings.
5. Test a text-only spoken turn and any non-English Whisper configuration you
   plan to use. Record the installed Hailo Apps/HailoRT/model versions with the
   result.

The bridge does not claim a successful Hailo device, camera, speech, or model
test until those device-local steps have actually passed.
