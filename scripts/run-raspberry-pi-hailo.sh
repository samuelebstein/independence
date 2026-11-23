#!/usr/bin/env bash
# Start Independence on a Raspberry Pi 5 with the Hailo AI HAT+ 2 in one step:
# check the setup, start the local Hailo VLM bridge, wait until it is ready,
# then run Independence's voice mode with the `raspberry-pi-hailo` profile.
#
# The bridge is stopped when Independence exits (Ctrl+C, an error, or quit).
# Every default below can be overridden with an environment variable, e.g.
#   PI_BUTTON_DEVICE=/dev/input/event3 scripts/run-raspberry-pi-hailo.sh
#
# Nothing here contacts the network: the bridge binds loopback only and the
# health check uses curl against that loopback address.
set -euo pipefail

if [[ $# -ne 0 ]]; then
  printf '%s\n' 'usage: scripts/run-raspberry-pi-hailo.sh   (configure with environment variables; see the top of this file)' >&2
  exit 2
fi

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --- Configuration (environment variable overrides win) ---------------------
: "${HAILO_VLM_HEF:=$HOME/models/hailo10h/Qwen3-VL-2B-Instruct.hef}"
: "${HAILO_APPS_DIR:=$HOME/code/hailo-apps-26.03.1}"
: "${HAILO_VLM_PYTHON:=$HOME/.venvs/hailo-qwen3-smoke/bin/python}"
: "${INDEPENDENCE_PIPER_COMMAND:=$HOME/.local/share/independence/piper-venv/bin/piper}"
: "${INDEPENDENCE_TTS_MODEL:=$repository_root/local-data/piper/hal/hal.onnx}"
: "${INDEPENDENCE_TTS_CONFIG:=$repository_root/local-data/piper/hal/hal.onnx.json}"
: "${PI_BUTTON_DEVICE:=/dev/input/event2}"
: "${PI_AUDIO_INPUT:=alsa:plughw:CARD=Device,DEV=0}"
: "${PI_AUDIO_OUTPUT:=alsa:plughw:CARD=Device,DEV=0}"
# Bridge generation settings (Milestone 8 owner decisions: cap 100, 0.3).
: "${BRIDGE_MAX_GENERATED_TOKENS:=100}"
: "${BRIDGE_TEMPERATURE:=0.3}"
# 1 keeps the voice system prompt loaded between questions; 0 re-reads it.
: "${BRIDGE_SYSTEM_PROMPT_CACHE:=1}"
: "${BRIDGE_READY_TIMEOUT_SECONDS:=180}"
: "${INDEPENDENCE_LOG:=debug}"
: "${INDEPENDENCE_BIN:=$repository_root/target/release/independence}"
: "${BRIDGE_LAUNCHER:=$repository_root/scripts/run_pi_hailo_vlm_bridge.sh}"
: "${RPICAM_STILL:=rpicam-still}"
: "${LOG_DIRECTORY:=$repository_root/local-data/logs}"
bridge_url="${INDEPENDENCE_HAILO_VLM_URL:-http://127.0.0.1:11435}"

fail() {
  printf 'Cannot start Independence: %s\n' "$1" >&2
  exit 2
}

# --- Pre-flight checks (before anything slow starts) ------------------------
[[ -x "$INDEPENDENCE_BIN" ]] ||
  fail "$INDEPENDENCE_BIN is not built. Run: cargo build --release"
[[ -x "$BRIDGE_LAUNCHER" ]] || fail "bridge launcher $BRIDGE_LAUNCHER is missing or not executable"
[[ -r "$HAILO_VLM_HEF" ]] || fail "Hailo model file not found: $HAILO_VLM_HEF (set HAILO_VLM_HEF)"
[[ -d "$HAILO_APPS_DIR" ]] || fail "Hailo Apps not found: $HAILO_APPS_DIR (set HAILO_APPS_DIR)"
[[ -x "$HAILO_VLM_PYTHON" ]] || fail "bridge Python not found: $HAILO_VLM_PYTHON (set HAILO_VLM_PYTHON)"
[[ -x "$INDEPENDENCE_PIPER_COMMAND" ]] ||
  fail "Piper not found: $INDEPENDENCE_PIPER_COMMAND (set INDEPENDENCE_PIPER_COMMAND)"
[[ -r "$INDEPENDENCE_TTS_MODEL" ]] || fail "voice model not found: $INDEPENDENCE_TTS_MODEL"
[[ -r "$INDEPENDENCE_TTS_CONFIG" ]] || fail "voice config not found: $INDEPENDENCE_TTS_CONFIG"
[[ -e "$PI_BUTTON_DEVICE" ]] ||
  fail "button device $PI_BUTTON_DEVICE does not exist. List devices with: ls -l /dev/input/by-id/ (then set PI_BUTTON_DEVICE)"
command -v curl >/dev/null 2>&1 || fail "curl is required for the bridge health check"
command -v "$RPICAM_STILL" >/dev/null 2>&1 || fail "$RPICAM_STILL was not found; install rpicam-apps"

# The raspberry-pi-hailo profile requires a camera. Fail now, before the
# bridge spends time loading the model. Independence repeats this check.
camera_list="$("$RPICAM_STILL" --list-cameras 2>&1 || true)"
if ! grep -Eq '^[[:space:]]*[0-9]+[[:space:]]*:' <<<"$camera_list"; then
  fail "no camera detected. Power off, reseat both ends of the camera ribbon, power on, and check: $RPICAM_STILL --list-cameras"
fi

# Refuse to share the port with a bridge (or anything else) this script did
# not start: it would be ambiguous which process answers and who stops it.
if curl --silent --output /dev/null --max-time 2 "$bridge_url/healthz"; then
  fail "something is already listening at $bridge_url. Stop the other Hailo bridge first (it may be running in another terminal)."
fi

# --- Start the bridge ------------------------------------------------------
mkdir -p "$LOG_DIRECTORY"
timestamp="$(date +%Y%m%d-%H%M%S)"
bridge_log="$LOG_DIRECTORY/hailo-bridge-$timestamp.log"
independence_log="$LOG_DIRECTORY/raspberry-pi-hailo-$timestamp.log"

bridge_arguments=()
[[ -n "$BRIDGE_MAX_GENERATED_TOKENS" ]] &&
  bridge_arguments+=(--max-generated-tokens "$BRIDGE_MAX_GENERATED_TOKENS")
[[ -n "$BRIDGE_TEMPERATURE" ]] && bridge_arguments+=(--temperature "$BRIDGE_TEMPERATURE")
[[ "$BRIDGE_SYSTEM_PROMPT_CACHE" == 0 ]] && bridge_arguments+=(--no-system-prompt-cache)

bridge_pid=""
stop_bridge() {
  if [[ -n "$bridge_pid" ]] && kill -0 "$bridge_pid" 2>/dev/null; then
    printf '%s\n' 'Stopping the Hailo bridge...'
    # SIGTERM triggers the bridge's orderly shutdown (it releases the Hailo
    # device). Force it only if it has not exited after ten seconds.
    kill -TERM "$bridge_pid" 2>/dev/null || true
    for _ in {1..100}; do
      kill -0 "$bridge_pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL "$bridge_pid" 2>/dev/null || true
    wait "$bridge_pid" 2>/dev/null || true
  fi
}
trap stop_bridge EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

printf 'Starting the Hailo bridge (log: %s)...\n' "$bridge_log"
HAILO_VLM_HEF="$HAILO_VLM_HEF" \
  HAILO_APPS_DIR="$HAILO_APPS_DIR" \
  HAILO_VLM_PYTHON="$HAILO_VLM_PYTHON" \
  "$BRIDGE_LAUNCHER" "${bridge_arguments[@]}" >"$bridge_log" 2>&1 &
bridge_pid=$!

deadline=$((SECONDS + BRIDGE_READY_TIMEOUT_SECONDS))
printf '%s' 'Waiting for the model to load'
while true; do
  health="$(curl --silent --max-time 2 "$bridge_url/healthz" 2>/dev/null || true)"
  if [[ "$health" == *'"status":"ready"'* ]]; then
    printf ' ready.\n'
    break
  fi
  if ! kill -0 "$bridge_pid" 2>/dev/null; then
    printf '\n'
    fail "the Hailo bridge exited during startup. See $bridge_log"
  fi
  if ((SECONDS >= deadline)); then
    printf '\n'
    fail "the Hailo bridge was not ready after ${BRIDGE_READY_TIMEOUT_SECONDS}s (set BRIDGE_READY_TIMEOUT_SECONDS to wait longer). See $bridge_log"
  fi
  printf '.'
  sleep 1
done

# --- Run Independence in the foreground --------------------------------------
printf 'Diagnostics: %s\n' "$independence_log"
export INDEPENDENCE_PIPER_COMMAND INDEPENDENCE_TTS_MODEL INDEPENDENCE_TTS_CONFIG INDEPENDENCE_LOG
set +e
"$INDEPENDENCE_BIN" \
  --voice \
  --profile raspberry-pi-hailo \
  --input buttons \
  --input-device "$PI_BUTTON_DEVICE" \
  --audio-input "$PI_AUDIO_INPUT" \
  --audio-output "$PI_AUDIO_OUTPUT" \
  2>"$independence_log"
status=$?
set -e
exit "$status"
