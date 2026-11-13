"""Shell-wrapper tests that do not need Hailo hardware or Hailo Apps."""

from __future__ import annotations

import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
PREPARE_SCRIPT = REPOSITORY_ROOT / "scripts" / "prepare_pi_hailo_vlm.sh"
LAUNCHER_SCRIPT = REPOSITORY_ROOT / "scripts" / "run_pi_hailo_vlm_bridge.sh"
POWER_CHECK_SCRIPT = REPOSITORY_ROOT / "scripts" / "check_pi_hailo_power.sh"


class PiHailoShellScriptTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        root = Path(self.temporary_directory.name)
        self.hailo_apps = root / "hailo-apps"
        self.resources = root / "resources" / "models" / "hailo10h"
        self.bin_directory = root / "bin"
        (self.hailo_apps / "venv_hailo_apps" / "bin").mkdir(parents=True)
        self.resources.mkdir(parents=True)
        self.bin_directory.mkdir()
        (self.resources / "Qwen2-VL-2B-Instruct.hef").touch()

        self._write_executable(
            self.hailo_apps / "setup_env.sh",
            """\
            #!/usr/bin/env bash
            VENV_NAME="venv_hailo_apps"
            is_sourced() {
                if [ -n "$ZSH_VERSION" ]; then
                    [[ -o sourced ]]
                elif [ -n "$BASH_VERSION" ]; then
                    [[ "${BASH_SOURCE[0]}" != "$0" ]]
                else
                    return 1
                fi
            }
            is_sourced
            PROJECT_ROOT=$(pwd)
            export PYTHONPATH="${PROJECT_ROOT}:${PYTHONPATH}"
            export GST_PLUGIN_PATH="/tmp/test-gstreamer:${GST_PLUGIN_PATH}"
            source "$VENV_NAME/bin/activate"
            """,
        )
        self._write_executable(
            self.hailo_apps / "venv_hailo_apps" / "bin" / "activate",
            """\
            export HAILO_TEST_ENV=active
            """,
        )
        self._write_executable(
            self.bin_directory / "python3",
            """\
            #!/usr/bin/env bash
            set -eu
            test "${HAILO_TEST_ENV:-}" = active
            case "${PYTHONPATH:-}" in
                "${HAILO_APPS_EXPECTED}:"*) ;;
                *)
                    echo "Hailo Apps setup used the wrong working directory" >&2
                    exit 17
                    ;;
            esac
            printf 'fake python completed\\n'
            """,
        )

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def _write_executable(self, path: Path, contents: str) -> None:
        path.write_text(textwrap.dedent(contents), encoding="utf-8")
        path.chmod(0o755)

    def _environment(self) -> dict[str, str]:
        environment = os.environ.copy()
        environment.pop("ZSH_VERSION", None)
        environment.pop("PYTHONPATH", None)
        environment.pop("GST_PLUGIN_PATH", None)
        environment.pop("HAILO_VLM_HEF", None)
        environment["HAILO_APPS_DIR"] = str(self.hailo_apps)
        environment["HAILO_RESOURCES_DIR"] = str(self.resources)
        environment["HAILO_APPS_EXPECTED"] = str(self.hailo_apps)
        environment["PATH"] = f"{self.bin_directory}:{environment['PATH']}"
        return environment

    def _run(self, script: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(script)],
            cwd=REPOSITORY_ROOT,
            env=self._environment(),
            text=True,
            capture_output=True,
            check=False,
        )

    def _run_power_check(self, throttled: str) -> subprocess.CompletedProcess[str]:
        self._write_executable(
            self.bin_directory / "vcgencmd",
            f"""\
            #!/usr/bin/env bash
            set -eu
            case "$1" in
                pmic_read_adc)
                    test "$2" = EXT5V_V
                    printf '     EXT5V_V volt(24)=5.01160000V\\n'
                    ;;
                measure_temp)
                    printf "temp=51.6'C\\n"
                    ;;
                get_throttled)
                    printf 'throttled=0x{throttled}\\n'
                    ;;
                *)
                    exit 64
                    ;;
            esac
            """,
        )
        return self._run(POWER_CHECK_SCRIPT)

    def test_prepare_sources_hailo_apps_without_nounset_or_working_directory_failures(self) -> None:
        result = self._run(PREPARE_SCRIPT)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Hailo VLM preflight passed", result.stdout)

    def test_launcher_sources_hailo_apps_without_nounset_or_working_directory_failures(self) -> None:
        result = self._run(LAUNCHER_SCRIPT)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("fake python completed", result.stdout)

    def test_power_check_passes_without_current_or_historical_flags(self) -> None:
        result = self._run_power_check("0")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("EXT5V_V volt(24)=5.01160000V", result.stdout)
        self.assertIn("PASS: no current or historical power/thermal flags", result.stdout)

    def test_power_check_rejects_the_recorded_undervoltage_and_throttling_signature(self) -> None:
        result = self._run_power_check("50000")

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("under-voltage occurred since boot", result.stdout)
        self.assertIn("throttling occurred since boot", result.stdout)
        self.assertIn("Use a verified 5 V / 5 A source", result.stdout)


if __name__ == "__main__":
    unittest.main()
