#!/usr/bin/env bash
# Start Independence on a Raspberry Pi 5 with the Hailo AI HAT+ 2 in one step:
# check the setup, start the local Hailo VLM bridge, wait until it is ready,
# then run Independence's voice mode with the `raspberry-pi-hailo` profile.
#
# The bridge is stopped when Independence exits (Ctrl+C, an error, or quit).
# Every default below can be overridden with an environment variable, e.g.
#   PI_BUTTON_DEVICE=/dev/input/event3 scripts/run-raspberry-pi-hailo.sh
#
#   scripts/run-raspberry-pi-hailo.sh --check
# checks everything this script needs, reports every problem at once, and
# exits without starting anything.
#
# Nothing here contacts the network: the bridge binds loopback only and the
# health check uses curl against that loopback address.
set -euo pipefail

check_only=0
case "$*" in
  "") ;;
  --check) check_only=1 ;;
  *)
    printf '%s\n' 'usage: scripts/run-raspberry-pi-hailo.sh [--check]   (configure with environment variables; see the top of this file)' >&2
    exit 2
    ;;
esac

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --- Configuration (environment variable overrides win) ---------------------
: "${HAILO_VLM_HEF:=$HOME/models/hailo10h/Qwen3-VL-2B-Instruct.hef}"
: "${HAILO_APPS_DIR:=$HOME/code/hailo-apps-26.03.1}"
: "${HAILO_VLM_PYTHON:=$HOME/.venvs/hailo-qwen3-smoke/bin/python}"
: "${INDEPENDENCE_PIPER_COMMAND:=$HOME/.local/share/independence/piper-venv/bin/piper}"
# The Piper voice. Its config defaults to the same path plus `.json`, so
# changing only INDEPENDENCE_TTS_MODEL switches voices.
: "${INDEPENDENCE_TTS_MODEL:=$repository_root/local-data/piper/hal/hal.onnx}"
: "${INDEPENDENCE_TTS_CONFIG:=$INDEPENDENCE_TTS_MODEL.json}"
# What Whisper listens for: `en`, another language code such as `es`, or
# `auto`. Anything but `en` needs a multilingual model (ggml-base.bin).
: "${INDEPENDENCE_WHISPER_MODEL:=$repository_root/local-data/whisper/ggml-base.en.bin}"
: "${INDEPENDENCE_WHISPER_LANGUAGE:=en}"
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
# Every check runs, so one attempt lists everything that needs fixing.
problems=()
check() { # check <message if it fails> <test command...>
  local message="$1"
  shift
  if "$@"; then
    ((check_only)) && printf '  ok       %s\n' "${message%%:*}"
  else
    problems+=("$message")
    ((check_only)) && printf '  PROBLEM  %s\n' "$message"
  fi
  return 0
}
has_command() { command -v "$1" >/dev/null 2>&1; }
# English-only Whisper models are named *.en.bin and cannot hear other languages.
whisper_model_fits_language() {
  [[ "$INDEPENDENCE_WHISPER_LANGUAGE" == en || "$INDEPENDENCE_WHISPER_MODEL" != *.en.bin ]]
}
camera_is_listed() {
  local camera_list
  camera_list="$("$RPICAM_STILL" --list-cameras 2>&1 || true)"
  grep -Eq '^[[:space:]]*[0-9]+[[:space:]]*:' <<<"$camera_list"
}
bridge_port_is_free() {
  ! curl --silent --output /dev/null --max-time 2 "$bridge_url/healthz"
}

((check_only)) && printf '%s\n' 'Checking the Raspberry Pi Hailo setup:'
check "Independence release build: $INDEPENDENCE_BIN is not built. Run: cargo build --release" \
  test -x "$INDEPENDENCE_BIN"
check "bridge launcher: $BRIDGE_LAUNCHER is missing or not executable" test -x "$BRIDGE_LAUNCHER"
check "Hailo model: file not found: $HAILO_VLM_HEF (set HAILO_VLM_HEF)" test -r "$HAILO_VLM_HEF"
check "Hailo Apps: not found: $HAILO_APPS_DIR (set HAILO_APPS_DIR)" test -d "$HAILO_APPS_DIR"
check "bridge Python: not found: $HAILO_VLM_PYTHON (set HAILO_VLM_PYTHON)" test -x "$HAILO_VLM_PYTHON"
check "Piper: not found: $INDEPENDENCE_PIPER_COMMAND (set INDEPENDENCE_PIPER_COMMAND)" \
  test -x "$INDEPENDENCE_PIPER_COMMAND"
check "voice model: not found: $INDEPENDENCE_TTS_MODEL (set INDEPENDENCE_TTS_MODEL)" \
  test -r "$INDEPENDENCE_TTS_MODEL"
check "voice config: not found: $INDEPENDENCE_TTS_CONFIG (download the voice's .onnx.json next to it)" \
  test -r "$INDEPENDENCE_TTS_CONFIG"
check "Whisper model: not found: $INDEPENDENCE_WHISPER_MODEL (set INDEPENDENCE_WHISPER_MODEL)" \
  test -r "$INDEPENDENCE_WHISPER_MODEL"
check "Whisper language: \`$INDEPENDENCE_WHISPER_LANGUAGE\` needs a multilingual model, but $INDEPENDENCE_WHISPER_MODEL is English-only. Download ggml-base.bin and set INDEPENDENCE_WHISPER_MODEL" \
  whisper_model_fits_language
check "button device: $PI_BUTTON_DEVICE does not exist. List devices with: ls -l /dev/input/by-id/ (then set PI_BUTTON_DEVICE)" \
  test -e "$PI_BUTTON_DEVICE"
check "curl: required for the bridge health check" has_command curl
check "rpicam-still: $RPICAM_STILL was not found; install rpicam-apps" has_command "$RPICAM_STILL"
# The raspberry-pi-hailo profile requires a camera. Fail now, before the
# bridge spends time loading the model. Independence repeats this check.
if has_command "$RPICAM_STILL"; then
  check "camera: no camera detected. Power off, reseat both ends of the camera ribbon, power on, and check: $RPICAM_STILL --list-cameras" \
    camera_is_listed
fi
# Refuse to share the port with a bridge (or anything else) this script did
# not start: it would be ambiguous which process answers and who stops it.
if has_command curl; then
  check "bridge port: something is already listening at $bridge_url. Stop the other Hailo bridge first (it may be running in another terminal)." \
    bridge_port_is_free
fi

if ((${#problems[@]})); then
  if ((check_only)); then
    printf '%s problem(s) found. Fix them, then run this check again.\n' "${#problems[@]}" >&2
  else
    for problem in "${problems[@]}"; do
      printf 'Cannot start Independence: %s\n' "$problem" >&2
    done
    ((${#problems[@]} > 1)) &&
      printf '%s\n' 'Run scripts/run-raspberry-pi-hailo.sh --check for the full list.' >&2
  fi
  exit 2
fi
if ((check_only)); then
  printf 'All checks passed. Voice: %s; Whisper: %s (language: %s).\n' \
    "$(basename "$INDEPENDENCE_TTS_MODEL")" "$(basename "$INDEPENDENCE_WHISPER_MODEL")" \
    "$INDEPENDENCE_WHISPER_LANGUAGE"
  printf '%s\n' 'Start Independence with: scripts/run-raspberry-pi-hailo.sh'
  exit 0
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
export INDEPENDENCE_PIPER_COMMAND INDEPENDENCE_TTS_MODEL INDEPENDENCE_TTS_CONFIG INDEPENDENCE_LOG \
  INDEPENDENCE_WHISPER_MODEL INDEPENDENCE_WHISPER_LANGUAGE
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
