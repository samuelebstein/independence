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
# Every check runs, so one attempt lists everything that needs fixing. With
# --check each item is printed with what it is and what was found; a normal
# start stays quiet unless something is wrong.

# Colour only for a terminal (NO_COLOR turns it off, FORCE_COLOR forces it).
if [[ -n "${FORCE_COLOR:-}" ]] || [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  green=$'\e[32m' red=$'\e[31m' bold=$'\e[1m' dim=$'\e[2m' reset=$'\e[0m'
else
  green='' red='' bold='' dim='' reset=''
fi

problems=()
section() {
  if ((check_only)); then printf '\n%s%s%s\n' "$bold" "$1" "$reset"; fi
  return 0
}
# check <name> <what was found> <what it is> <fix if it fails> <test command...>
check() {
  local name="$1" found="$2" about="$3" fix="$4"
  shift 4
  if "$@"; then
    if ((check_only)); then
      printf '  %s✓%s %-20s %s\n' "$green" "$reset" "$name" "$found"
      printf '    %s%s%s\n' "$dim" "$about" "$reset"
    fi
  else
    problems+=("$name: $fix")
    if ((check_only)); then
      printf '  %s✗ %-20s%s %s\n' "$red" "$name" "$reset" "$found"
      printf '    %s%s%s\n' "$dim" "$about" "$reset"
      printf '    %sFix: %s%s\n' "$red" "$fix" "$reset"
    fi
  fi
  return 0
}
has_command() { command -v "$1" >/dev/null 2>&1; }
# A path with $HOME shortened to ~, plus the file's size when it exists (a
# truncated download shows up as a suspiciously small size).
describe_file() {
  local shown="${1/#$HOME/\~}"
  if [[ -f "$1" ]]; then
    printf '%s (%s)' "$shown" "$(du -h "$1" | cut -f1)"
  else
    printf '%s (missing)' "$shown"
  fi
}
describe_path() { printf '%s' "${1/#$HOME/\~}"; }
# The language a Piper voice was trained for, from its .onnx.json.
voice_language() {
  [[ -r "$1" ]] || return 0
  sed -n 's/.*"code"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1" | head -n 1
}
# How a Piper voice turns text into sounds, from its .onnx.json: "espeak"
# (built in) or "pinyin" (Mandarin; needs the separate g2pW model).
voice_phoneme_type() {
  [[ -r "$1" ]] || return 0
  sed -n 's/.*"phoneme_type"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1" | head -n 1
}
# Piper's own Python, next to the piper command in the same virtual environment.
piper_python() { printf '%s/python' "$(dirname "$INDEPENDENCE_PIPER_COMMAND")"; }
piper_knows_pinyin() {
  "$(piper_python)" -c 'from piper.config import PhonemeType; PhonemeType.PINYIN' >/dev/null 2>&1
}
# English-only Whisper models are named *.en.bin and cannot hear other languages.
whisper_model_fits_language() {
  [[ "$INDEPENDENCE_WHISPER_LANGUAGE" == en || "$INDEPENDENCE_WHISPER_MODEL" != *.en.bin ]]
}
describe_language() {
  case "$INDEPENDENCE_WHISPER_LANGUAGE" in
    en) printf 'en (English)' ;;
    auto) printf 'auto (detected for each recording)' ;;
    *) printf '%s' "$INDEPENDENCE_WHISPER_LANGUAGE" ;;
  esac
}
# The audio devices Independence itself can open, from its own listing.
audio_listing() { # audio_listing --audio-input-devices|--audio-devices
  "$INDEPENDENCE_BIN" "$1" 2>/dev/null | tr -d '\r' || true
}
# The human-readable name listed just above `ID: <id>`, if the id is listed.
audio_device_name() { # audio_device_name <listing> <id>
  awk -v id="ID: $2" '
    /^- / { name = substr($0, 3) }
    { sub(/^[[:space:]]+/, "") }
    $0 == id { print name; found = 1; exit }
    END { exit !found }
  ' <<<"$1"
}
# The kernel's name for a /dev/input device, e.g. "Magic Keyboard".
input_device_name() {
  local event
  event="$(basename "$(readlink -f "$1")")"
  [[ -r "/sys/class/input/$event/device/name" ]] && cat "/sys/class/input/$event/device/name"
  return 0
}
camera_line() {
  grep -Em1 '^[[:space:]]*[0-9]+[[:space:]]*:' <<<"$camera_list" | sed 's/^[[:space:]]*//'
}
bridge_port_is_free() {
  ! curl --silent --output /dev/null --max-time 2 "$bridge_url/healthz"
}

if ((check_only)); then
  printf '%sIndependence setup check: Raspberry Pi 5 + Hailo-10H%s\n' "$bold" "$reset"
  printf '%s%s%s\n' "$dim" "Nothing is started. Each line shows what it is and what was found." "$reset"
fi

section "Program"
check "Independence" "$(describe_path "$INDEPENDENCE_BIN")" \
  "The compiled Independence program (release build) that this script runs." \
  "it is not built. Run: cargo build --release" \
  test -x "$INDEPENDENCE_BIN"

section "Thinking: the local model on the Hailo"
check "Bridge launcher" "$(describe_path "$BRIDGE_LAUNCHER")" \
  "Starts the Python bridge that serves the Hailo model to Independence on this Pi only (127.0.0.1)." \
  "$BRIDGE_LAUNCHER is missing or not executable" \
  test -x "$BRIDGE_LAUNCHER"
check "Hailo model" "$(describe_file "$HAILO_VLM_HEF")" \
  "Qwen3-VL 2B compiled for the Hailo-10H: reads your question and photo and writes the answer." \
  "file not found: $HAILO_VLM_HEF (set HAILO_VLM_HEF)" \
  test -r "$HAILO_VLM_HEF"
check "Hailo Apps" "$(describe_path "$HAILO_APPS_DIR")" \
  "Hailo's Python application code that the bridge uses to run the model." \
  "not found: $HAILO_APPS_DIR (set HAILO_APPS_DIR)" \
  test -d "$HAILO_APPS_DIR"
check "Bridge Python" "$(describe_path "$HAILO_VLM_PYTHON")" \
  "The Python environment with Hailo's runtime installed; it runs the bridge." \
  "not found: $HAILO_VLM_PYTHON (set HAILO_VLM_PYTHON)" \
  test -x "$HAILO_VLM_PYTHON"

section "Hearing: speech to text"
# Audio devices are checked against Independence's own device list, so they
# need the program; without it the "Independence" problem above says why.
if [[ -x "$INDEPENDENCE_BIN" ]]; then
  microphones="$(audio_listing --audio-input-devices)"
  microphone_name="$(audio_device_name "$microphones" "$PI_AUDIO_INPUT" || true)"
  check "Microphone" "$PI_AUDIO_INPUT${microphone_name:+ ($microphone_name)}" \
    "Records your voice while ENTER is held." \
    "not in Independence's microphone list. List them with: $INDEPENDENCE_BIN --audio-input-devices (then set PI_AUDIO_INPUT)" \
    test -n "$microphone_name"
fi
check "Whisper model" "$(describe_file "$INDEPENDENCE_WHISPER_MODEL")" \
  "Turns the recording into text on the Pi's CPU. Names ending in .en.bin understand English only." \
  "not found: $INDEPENDENCE_WHISPER_MODEL (set INDEPENDENCE_WHISPER_MODEL)" \
  test -r "$INDEPENDENCE_WHISPER_MODEL"
check "Whisper language" "$(describe_language)" \
  "The language Whisper listens for (INDEPENDENCE_WHISPER_LANGUAGE)." \
  "\`$INDEPENDENCE_WHISPER_LANGUAGE\` needs a multilingual model, but $INDEPENDENCE_WHISPER_MODEL is English-only. Download ggml-base.bin and set INDEPENDENCE_WHISPER_MODEL" \
  whisper_model_fits_language

section "Speaking: text to speech"
check "Piper" "$(describe_path "$INDEPENDENCE_PIPER_COMMAND")" \
  "The text-to-speech program; it turns each answer sentence into audio." \
  "not found: $INDEPENDENCE_PIPER_COMMAND (set INDEPENDENCE_PIPER_COMMAND)" \
  test -x "$INDEPENDENCE_PIPER_COMMAND"
spoken_language="$(voice_language "$INDEPENDENCE_TTS_CONFIG")"
check "Voice" "$(describe_file "$INDEPENDENCE_TTS_MODEL")" \
  "The voice Piper speaks with${spoken_language:+; it pronounces text as $spoken_language}." \
  "not found: $INDEPENDENCE_TTS_MODEL (set INDEPENDENCE_TTS_MODEL)" \
  test -r "$INDEPENDENCE_TTS_MODEL"
check "Voice config" "$(describe_file "$INDEPENDENCE_TTS_CONFIG")" \
  "The voice's settings (sample rate, pronunciation language); it sits next to the voice." \
  "not found: $INDEPENDENCE_TTS_CONFIG (download the voice's .onnx.json next to it)" \
  test -r "$INDEPENDENCE_TTS_CONFIG"
# Pinyin voices (Mandarin chaowen, xiao_ya) need g2pW next to the voice. Without
# it Piper would download g2pW from the internet the first time it speaks, so
# Independence refuses to start; this says so before launch.
if [[ "$(voice_phoneme_type "$INDEPENDENCE_TTS_CONFIG")" == pinyin ]]; then
  pronunciation_model="$(dirname "$INDEPENDENCE_TTS_MODEL")/g2pW/g2pw.onnx"
  check "Pinyin model (g2pW)" "$(describe_file "$pronunciation_model")" \
    "This voice reads Chinese characters as pinyin using g2pW, a second model that must already be on the Pi." \
    "missing. Download g2pw.tar.gz into $(dirname "$pronunciation_model") (see docs/voice-and-language.md)" \
    test -r "$pronunciation_model"
  if [[ -x "$(piper_python)" ]]; then
    check "Piper pinyin support" "$(describe_path "$INDEPENDENCE_PIPER_COMMAND")" \
      "Older Piper versions cannot speak pinyin voices." \
      "this Piper does not know pinyin voices. Upgrade it: $(describe_path "$(piper_python)") -m pip install --upgrade piper-tts" \
      piper_knows_pinyin
  fi
fi
if [[ -x "$INDEPENDENCE_BIN" ]]; then
  speakers="$(audio_listing --audio-devices)"
  speaker_name="$(audio_device_name "$speakers" "$PI_AUDIO_OUTPUT" || true)"
  check "Speaker" "$PI_AUDIO_OUTPUT${speaker_name:+ ($speaker_name)}" \
    "Plays the spoken answers." \
    "not in Independence's speaker list. List them with: $INDEPENDENCE_BIN --audio-devices (then set PI_AUDIO_OUTPUT)" \
    test -n "$speaker_name"
fi

section "Controls and camera"
button_name="$(input_device_name "$PI_BUTTON_DEVICE")"
check "Buttons" "$PI_BUTTON_DEVICE${button_name:+ ($button_name)}" \
  "The keyboard Independence reads: hold ENTER to talk, P for a photo, ESC to cancel." \
  "$PI_BUTTON_DEVICE does not exist. List devices with: ls -l /dev/input/by-id/ (then set PI_BUTTON_DEVICE)" \
  test -e "$PI_BUTTON_DEVICE"
check "rpicam-still" "$(describe_path "$(command -v "$RPICAM_STILL" || printf '%s' "$RPICAM_STILL")")" \
  "The Raspberry Pi camera tool Independence uses to take a photo when you press P." \
  "$RPICAM_STILL was not found; install rpicam-apps" \
  has_command "$RPICAM_STILL"
# The raspberry-pi-hailo profile requires a camera. Fail now, before the
# bridge spends time loading the model. Independence repeats this check.
camera_list=""
if has_command "$RPICAM_STILL"; then
  camera_list="$("$RPICAM_STILL" --list-cameras 2>&1 || true)"
  camera="$(camera_line || true)"
  check "Camera" "${camera:-none listed}" \
    "The camera module itself; this profile will not start without one." \
    "no camera detected. Power off, reseat both ends of the camera ribbon, power on, and check: $RPICAM_STILL --list-cameras" \
    test -n "$camera"
fi

section "Connection to the bridge"
check "curl" "$(describe_path "$(command -v curl || printf 'curl')")" \
  "Asks the bridge whether the model has finished loading." \
  "required for the bridge health check" \
  has_command curl
# Refuse to share the port with a bridge (or anything else) this script did
# not start: it would be ambiguous which process answers and who stops it.
if has_command curl; then
  check "Bridge port" "$bridge_url" \
    "The local-only address the bridge will use. It must be free, so the bridge that answers is the one this script starts and stops." \
    "something is already listening at $bridge_url. Stop the other Hailo bridge first (it may be running in another terminal)." \
    bridge_port_is_free
fi

if ((${#problems[@]})); then
  if ((check_only)); then
    printf '\n%s%s problem(s) found.%s Fix them, then run this check again.\n' \
      "$red" "${#problems[@]}" "$reset" >&2
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
  printf '\n%sAll checks passed.%s Voice: %s; Whisper: %s (language: %s).\n' \
    "$green" "$reset" "$(basename "$INDEPENDENCE_TTS_MODEL")" \
    "$(basename "$INDEPENDENCE_WHISPER_MODEL")" "$INDEPENDENCE_WHISPER_LANGUAGE"
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
