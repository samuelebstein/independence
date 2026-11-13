#!/usr/bin/env bash
# Preflight Hailo Apps for Independence's local vision bridge.
# Network/download and dependency installation are opt-in flags.
set -euo pipefail

install_python_dependencies=false
download_model=false
for argument in "$@"; do
  case "$argument" in
    --install-python-dependencies) install_python_dependencies=true ;;
    --download-model) download_model=true ;;
    --help)
      printf '%s\n' 'usage: scripts/prepare_pi_hailo_vlm.sh [--install-python-dependencies] [--download-model]'
      exit 0
      ;;
    *)
      printf 'Unknown option: %s\n' "$argument" >&2
      exit 2
      ;;
  esac
done

hailo_apps_dir="${HAILO_APPS_DIR:-$HOME/code/hailo-apps}"
resources_dir="${HAILO_RESOURCES_DIR:-/usr/local/hailo/resources/models/hailo10h}"

if [[ ! -f "$hailo_apps_dir/setup_env.sh" ]]; then
  printf 'Hailo Apps was not found at %s. Clone/install it first; see docs/raspberry-pi-hailo.md.\n' "$hailo_apps_dir" >&2
  exit 2
fi
if [[ ! -d "$hailo_apps_dir/venv_hailo_apps" ]]; then
  printf 'Hailo Apps virtual environment is missing. Run ./install.sh from %s first.\n' "$hailo_apps_dir" >&2
  exit 2
fi

source_hailo_apps_environment() {
  # Hailo Apps' setup script expects its own directory as $PWD and expands
  # optional shell variables without defaults. Source it outside nounset, then
  # restore this wrapper's safety setting before continuing.
  local nounset_was_enabled=false
  local setup_status=0

  case $- in
    *u*)
      nounset_was_enabled=true
      set +u
      ;;
  esac

  pushd "$hailo_apps_dir" >/dev/null
  # shellcheck source=/dev/null
  if source ./setup_env.sh; then
    setup_status=0
  else
    setup_status=$?
  fi
  popd >/dev/null

  if [[ "$nounset_was_enabled" == true ]]; then
    set -u
  fi
  return "$setup_status"
}

if ! source_hailo_apps_environment; then
  printf 'Hailo Apps environment setup failed in %s. Resolve its output before continuing.\n' "$hailo_apps_dir" >&2
  exit 2
fi

if "$install_python_dependencies"; then
  (
    cd "$hailo_apps_dir"
    python3 -m pip install -e '.[gen-ai]'
  )
fi

if "$download_model"; then
  hailo-download-resources --group vlm_chat --arch hailo10h
fi

if ! python3 -c 'from hailo_platform.genai import VLM; import cv2; import numpy' >/dev/null 2>&1; then
  printf 'The Hailo VLM Python dependencies are missing. Re-run with --install-python-dependencies.\n' >&2
  exit 2
fi

if [[ ! -d "$resources_dir" ]]; then
  printf 'No Hailo model directory at %s. Re-run with --download-model to make the explicit model download.\n' "$resources_dir" >&2
  exit 2
fi
mapfile -t hef_candidates < <(find "$resources_dir" -type f -iname '*qwen2*vl*2b*.hef' -print | sort)
if [[ ${#hef_candidates[@]} -ne 1 ]]; then
  printf 'Expected exactly one Qwen2-VL 2B HEF below %s; found %s. Set HAILO_VLM_HEF explicitly when starting the bridge.\n' "$resources_dir" "${#hef_candidates[@]}" >&2
  exit 2
fi

printf 'Hailo VLM preflight passed. The bridge will use:\nHAILO_VLM_HEF=%q\n' "${hef_candidates[0]}"
printf 'Start it with: scripts/run_pi_hailo_vlm_bridge.sh\n'
