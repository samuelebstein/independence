#!/usr/bin/env bash
# Start the loopback-only Hailo VLM bridge after activating Hailo Apps.
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
hailo_apps_dir="${HAILO_APPS_DIR:-$HOME/code/hailo-apps}"
resources_dir="${HAILO_RESOURCES_DIR:-/usr/local/hailo/resources/models/hailo10h}"
hailo_python="${HAILO_VLM_PYTHON:-}"

if [[ ! -f "$hailo_apps_dir/setup_env.sh" ]]; then
  printf 'Hailo Apps was not found at %s. Set HAILO_APPS_DIR or follow docs/raspberry-pi-hailo.md.\n' "$hailo_apps_dir" >&2
  exit 2
fi

if [[ -n "$hailo_python" ]]; then
  if [[ ! -x "$hailo_python" ]]; then
    printf 'HAILO_VLM_PYTHON is not executable: %s\n' "$hailo_python" >&2
    exit 2
  fi
  # The dedicated Qwen3 smoke-test venv contains HailoRT, OpenCV, NumPy, and
  # python-dotenv; expose the checked-out Hailo Apps package without sourcing
  # its optional venv_hailo_apps environment.
  export PYTHONPATH="$hailo_apps_dir${PYTHONPATH:+:$PYTHONPATH}"
else
  hailo_python='python3'
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

if [[ -z "${HAILO_VLM_PYTHON:-}" ]]; then
  if ! source_hailo_apps_environment; then
    printf 'Hailo Apps environment setup failed in %s. Set HAILO_VLM_PYTHON to the proven Qwen3 smoke-test Python environment or resolve this output.\n' "$hailo_apps_dir" >&2
    exit 2
  fi
fi

if [[ -z "${HAILO_VLM_HEF:-}" ]]; then
  if [[ ! -d "$resources_dir" ]]; then
    printf 'No Hailo model directory at %s. Run scripts/prepare_pi_hailo_vlm.sh --download-model first.\n' "$resources_dir" >&2
    exit 2
  fi
  mapfile -t hef_candidates < <(find "$resources_dir" -type f -iname '*qwen3*vl*2b*.hef' -print | sort)
  if [[ ${#hef_candidates[@]} -ne 1 ]]; then
    printf 'Expected exactly one Qwen3-VL 2B HEF below %s; found %s. Set HAILO_VLM_HEF explicitly.\n' "$resources_dir" "${#hef_candidates[@]}" >&2
    exit 2
  fi
  export HAILO_VLM_HEF="${hef_candidates[0]}"
fi

exec "$hailo_python" "$repository_root/tools/hailo_vlm_bridge.py" "$@"
