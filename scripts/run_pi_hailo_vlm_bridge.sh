#!/usr/bin/env bash
# Start the loopback-only Hailo VLM bridge after activating Hailo Apps.
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
hailo_apps_dir="${HAILO_APPS_DIR:-$HOME/code/hailo-apps}"
resources_dir="${HAILO_RESOURCES_DIR:-/usr/local/hailo/resources/models/hailo10h}"

if [[ ! -f "$hailo_apps_dir/setup_env.sh" ]]; then
  printf 'Hailo Apps was not found at %s. Set HAILO_APPS_DIR or follow docs/raspberry-pi-hailo.md.\n' "$hailo_apps_dir" >&2
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

if [[ -z "${HAILO_VLM_HEF:-}" ]]; then
  if [[ ! -d "$resources_dir" ]]; then
    printf 'No Hailo model directory at %s. Run scripts/prepare_pi_hailo_vlm.sh --download-model first.\n' "$resources_dir" >&2
    exit 2
  fi
  mapfile -t hef_candidates < <(find "$resources_dir" -type f -iname '*qwen2*vl*2b*.hef' -print | sort)
  if [[ ${#hef_candidates[@]} -ne 1 ]]; then
    printf 'Expected exactly one Qwen2-VL 2B HEF below %s; found %s. Set HAILO_VLM_HEF explicitly.\n' "$resources_dir" "${#hef_candidates[@]}" >&2
    exit 2
  fi
  export HAILO_VLM_HEF="${hef_candidates[0]}"
fi

exec python3 "$repository_root/tools/hailo_vlm_bridge.py" "$@"
