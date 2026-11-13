#!/usr/bin/env bash
# Report Raspberry Pi power/thermal flags before and after a Hailo VLM test.
set -euo pipefail

if [[ $# -ne 0 ]]; then
  printf '%s\n' 'usage: scripts/check_pi_hailo_power.sh' >&2
  exit 2
fi

if ! command -v vcgencmd >/dev/null 2>&1; then
  printf '%s\n' 'Raspberry Pi vcgencmd was not found; run this check on the Pi.' >&2
  exit 2
fi

if ! voltage_output="$(vcgencmd pmic_read_adc EXT5V_V)"; then
  printf '%s\n' 'Could not read the Raspberry Pi external 5 V rail.' >&2
  exit 2
fi
if ! temperature_output="$(vcgencmd measure_temp)"; then
  printf '%s\n' 'Could not read the Raspberry Pi temperature.' >&2
  exit 2
fi
if ! throttled_output="$(vcgencmd get_throttled)"; then
  printf '%s\n' 'Could not read Raspberry Pi throttling flags.' >&2
  exit 2
fi

if [[ ! "$throttled_output" =~ ^throttled=0x([[:xdigit:]]+)$ ]]; then
  printf 'Unexpected vcgencmd throttling output: %s\n' "$throttled_output" >&2
  exit 2
fi

throttled_hex="${BASH_REMATCH[1]}"
throttled_value=$((16#$throttled_hex))

printf 'Pi Hailo power check\n%s\n%s\n%s\n' \
  "$voltage_output" "$temperature_output" "$throttled_output"

warnings=()
(( throttled_value & 0x00001 )) && warnings+=('under-voltage is active now')
(( throttled_value & 0x00002 )) && warnings+=('ARM frequency capping is active now')
(( throttled_value & 0x00004 )) && warnings+=('throttling is active now')
(( throttled_value & 0x00008 )) && warnings+=('the soft temperature limit is active now')
(( throttled_value & 0x10000 )) && warnings+=('under-voltage occurred since boot')
(( throttled_value & 0x20000 )) && warnings+=('ARM frequency capping occurred since boot')
(( throttled_value & 0x40000 )) && warnings+=('throttling occurred since boot')
(( throttled_value & 0x80000 )) && warnings+=('the soft temperature limit occurred since boot')

if (( throttled_value != 0 )); then
  printf '%s\n' 'FAIL: power or thermal flags were recorded during this boot.'
  if (( ${#warnings[@]} == 0 )); then
    printf '  - unrecognised flag bits: 0x%X\n' "$throttled_value"
  else
    for warning in "${warnings[@]}"; do
      printf '  - %s\n' "$warning"
    done
  fi
  printf '%s\n' 'Use a verified 5 V / 5 A source, reboot, then repeat the pre- and post-inference checks.'
  exit 1
fi

printf '%s\n' 'PASS: no current or historical power/thermal flags were recorded in this boot.'
