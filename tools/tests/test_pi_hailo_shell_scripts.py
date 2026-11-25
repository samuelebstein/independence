"""Shell-wrapper tests that do not need Hailo hardware or Hailo Apps."""

from __future__ import annotations

import json
import os
import socket
import subprocess
import tempfile
import textwrap
import threading
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
PREPARE_SCRIPT = REPOSITORY_ROOT / "scripts" / "prepare_pi_hailo_vlm.sh"
LAUNCHER_SCRIPT = REPOSITORY_ROOT / "scripts" / "run_pi_hailo_vlm_bridge.sh"
POWER_CHECK_SCRIPT = REPOSITORY_ROOT / "scripts" / "check_pi_hailo_power.sh"
PI_HAILO_RUN_SCRIPT = REPOSITORY_ROOT / "scripts" / "run-raspberry-pi-hailo.sh"


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
        (self.resources / "Qwen3-VL-2B-Instruct.hef").touch()

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


def _free_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


FAKE_BRIDGE = """\
#!/usr/bin/env python3
# Stands in for scripts/run_pi_hailo_vlm_bridge.sh: records its arguments,
# optionally serves /healthz, and records how it was stopped.
import json, os, signal, sys, time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

record = Path(os.environ["FAKE_RECORD_DIR"])
(record / "bridge-args.json").write_text(json.dumps(sys.argv[1:]))
mode = os.environ.get("FAKE_BRIDGE_MODE", "ready")
if mode == "exit":
    sys.exit(3)

def stop(_signum, _frame):
    (record / "bridge-stopped").write_text("SIGTERM")
    sys.exit(0)

signal.signal(signal.SIGTERM, stop)
if mode == "never-ready":
    while True:
        time.sleep(0.1)

class Health(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"status":"ready"}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass

HTTPServer(("127.0.0.1", int(os.environ["FAKE_BRIDGE_PORT"])), Health).serve_forever()
"""

FAKE_INDEPENDENCE = """\
#!/usr/bin/env bash
# Stands in for target/release/independence: lists fake audio devices (as the
# real program does, with CRLF line ends), or records how it was launched.
case "$1" in
  --audio-input-devices)
    printf 'Audio input devices:\\r\\n- USB PnP Audio Device (default)\\r\\n  ID: %s\\r\\n' \\
      "${FAKE_LISTED_MICROPHONE:-${PI_AUDIO_INPUT:-alsa:plughw:CARD=Device,DEV=0}}"
    exit 0
    ;;
  --audio-devices)
    printf 'Audio output devices:\\r\\n- USB PnP Audio Device\\r\\n  ID: %s\\r\\n' \\
      "${PI_AUDIO_OUTPUT:-alsa:plughw:CARD=Device,DEV=0}"
    exit 0
    ;;
esac
printf '%s\\n' "$@" > "$FAKE_RECORD_DIR/independence-args"
printf '%s|%s|%s|%s\\n' "$INDEPENDENCE_PIPER_COMMAND" "$INDEPENDENCE_TTS_MODEL" \\
  "$INDEPENDENCE_TTS_CONFIG" "$INDEPENDENCE_LOG" > "$FAKE_RECORD_DIR/independence-env"
printf '%s|%s\\n' "$INDEPENDENCE_WHISPER_MODEL" "$INDEPENDENCE_WHISPER_LANGUAGE" \\
  > "$FAKE_RECORD_DIR/independence-whisper"
test -e "$FAKE_RECORD_DIR/bridge-stopped" && echo "bridge stopped too early" >&2 && exit 9
printf 'diagnostic line\\n' >&2
exit "${FAKE_INDEPENDENCE_STATUS:-0}"
"""


class RaspberryPiHailoRunScriptTests(unittest.TestCase):
    """The one-command Pi launcher, with fake bridge, camera tool, and app."""

    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary_directory.name)
        self.record = self.root / "record"
        self.record.mkdir()
        self.logs = self.root / "logs"
        self.port = _free_loopback_port()

        for name in (
            "model.hef",
            "hal.onnx",
            "hal.onnx.json",
            "es_MX-claude-high.onnx",
            "es_MX-claude-high.onnx.json",
            "ggml-base.en.bin",
            "ggml-base.bin",
            "event2",
        ):
            (self.root / name).touch()
        (self.root / "es_MX-claude-high.onnx.json").write_text(
            '{"language": {"code": "es_MX"}, "espeak": {"voice": "es"}}', encoding="utf-8"
        )
        (self.root / "hailo-apps").mkdir()
        for name in ("python", "piper"):
            self._executable(self.root / name, "#!/usr/bin/env bash\nexit 0\n")
        self._executable(self.root / "bridge", FAKE_BRIDGE)
        self._executable(self.root / "independence", FAKE_INDEPENDENCE)
        self._camera(present=True)

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def _executable(self, path: Path, contents: str) -> None:
        path.write_text(contents, encoding="utf-8")
        path.chmod(0o755)

    def _camera(self, present: bool) -> None:
        output = (
            "Available cameras\\n-----------------\\n0 : imx708_wide [4608x2592 10-bit RGGB]\\n"
            if present
            else "No cameras available!\\n"
        )
        self._executable(
            self.root / "rpicam-still",
            f"#!/usr/bin/env bash\nprintf '{output}'\n",
        )

    def _environment(self, **overrides: str) -> dict[str, str]:
        environment = os.environ.copy()
        environment.update(
            {
                "HAILO_VLM_HEF": str(self.root / "model.hef"),
                "HAILO_APPS_DIR": str(self.root / "hailo-apps"),
                "HAILO_VLM_PYTHON": str(self.root / "python"),
                "INDEPENDENCE_PIPER_COMMAND": str(self.root / "piper"),
                "INDEPENDENCE_TTS_MODEL": str(self.root / "hal.onnx"),
                "INDEPENDENCE_TTS_CONFIG": str(self.root / "hal.onnx.json"),
                "INDEPENDENCE_WHISPER_MODEL": str(self.root / "ggml-base.en.bin"),
                "PI_BUTTON_DEVICE": str(self.root / "event2"),
                "INDEPENDENCE_BIN": str(self.root / "independence"),
                "BRIDGE_LAUNCHER": str(self.root / "bridge"),
                "RPICAM_STILL": str(self.root / "rpicam-still"),
                "LOG_DIRECTORY": str(self.logs),
                "INDEPENDENCE_HAILO_VLM_URL": f"http://127.0.0.1:{self.port}",
                "BRIDGE_READY_TIMEOUT_SECONDS": "10",
                "FAKE_BRIDGE_PORT": str(self.port),
                "FAKE_RECORD_DIR": str(self.record),
            }
        )
        environment.pop("INDEPENDENCE_LOG", None)
        environment.pop("BRIDGE_TEMPERATURE", None)
        environment.pop("BRIDGE_MAX_GENERATED_TOKENS", None)
        environment.pop("BRIDGE_SYSTEM_PROMPT_CACHE", None)
        environment.pop("INDEPENDENCE_WHISPER_LANGUAGE", None)
        environment.update(overrides)
        return {key: value for key, value in environment.items() if value != "<unset>"}

    def _run(self, *arguments: str, **overrides: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(PI_HAILO_RUN_SCRIPT), *arguments],
            cwd=REPOSITORY_ROOT,
            env=self._environment(**overrides),
            text=True,
            capture_output=True,
            timeout=60,
        )

    def test_starts_bridge_waits_runs_independence_then_stops_bridge(self) -> None:
        result = self._run()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads((self.record / "bridge-args.json").read_text()),
            ["--max-generated-tokens", "100", "--temperature", "0.3"],
        )
        arguments = (self.record / "independence-args").read_text().split()
        self.assertEqual(
            arguments,
            [
                "--voice",
                "--profile",
                "raspberry-pi-hailo",
                "--input",
                "buttons",
                "--input-device",
                str(self.root / "event2"),
                "--audio-input",
                "alsa:plughw:CARD=Device,DEV=0",
                "--audio-output",
                "alsa:plughw:CARD=Device,DEV=0",
            ],
        )
        self.assertEqual(
            (self.record / "independence-env").read_text().strip(),
            f"{self.root / 'piper'}|{self.root / 'hal.onnx'}|{self.root / 'hal.onnx.json'}|debug",
        )
        self.assertEqual((self.record / "bridge-stopped").read_text(), "SIGTERM")
        independence_logs = list(self.logs.glob("raspberry-pi-hailo-*.log"))
        self.assertEqual(len(independence_logs), 1)
        self.assertEqual(independence_logs[0].read_text(), "diagnostic line\n")
        self.assertEqual(len(list(self.logs.glob("hailo-bridge-*.log"))), 1)

    def test_overrides_reach_the_bridge_and_independence(self) -> None:
        result = self._run(
            BRIDGE_MAX_GENERATED_TOKENS="60",
            BRIDGE_TEMPERATURE="0.4",
            PI_AUDIO_INPUT="alsa:mic",
            PI_AUDIO_OUTPUT="alsa:speaker",
            INDEPENDENCE_LOG="info",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads((self.record / "bridge-args.json").read_text()),
            ["--max-generated-tokens", "60", "--temperature", "0.4"],
        )
        arguments = (self.record / "independence-args").read_text().split()
        self.assertIn("alsa:mic", arguments)
        self.assertIn("alsa:speaker", arguments)
        self.assertTrue((self.record / "independence-env").read_text().strip().endswith("|info"))

    def test_whisper_defaults_to_english_and_reaches_independence(self) -> None:
        result = self._run()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.record / "independence-whisper").read_text().strip(),
            f"{self.root / 'ggml-base.en.bin'}|en",
        )

    def test_spanish_voice_and_whisper_need_only_their_paths_and_language(self) -> None:
        result = self._run(
            INDEPENDENCE_TTS_MODEL=str(self.root / "es_MX-claude-high.onnx"),
            INDEPENDENCE_TTS_CONFIG="<unset>",
            INDEPENDENCE_WHISPER_MODEL=str(self.root / "ggml-base.bin"),
            INDEPENDENCE_WHISPER_LANGUAGE="es",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.record / "independence-env").read_text().strip().split("|")[1:3],
            [
                str(self.root / "es_MX-claude-high.onnx"),
                str(self.root / "es_MX-claude-high.onnx.json"),
            ],
            "the voice's own config follows the voice",
        )
        self.assertEqual(
            (self.record / "independence-whisper").read_text().strip(),
            f"{self.root / 'ggml-base.bin'}|es",
        )

    def test_another_language_with_the_english_only_whisper_model_fails_before_the_bridge(
        self,
    ) -> None:
        result = self._run(INDEPENDENCE_WHISPER_LANGUAGE="es")
        self.assertEqual(result.returncode, 2)
        self.assertIn("English-only", result.stderr)
        self.assertIn("ggml-base.bin", result.stderr)
        self.assertFalse((self.record / "bridge-args.json").exists())

    def test_check_reports_success_and_starts_nothing(self) -> None:
        result = self._run("--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("All checks passed", result.stdout)
        self.assertIn("Whisper: ggml-base.en.bin (language: en)", result.stdout)
        self.assertFalse((self.record / "bridge-args.json").exists())
        self.assertFalse((self.record / "independence-args").exists())

    def test_check_explains_each_item_and_what_it_found(self) -> None:
        result = self._run(
            "--check",
            INDEPENDENCE_TTS_MODEL=str(self.root / "es_MX-claude-high.onnx"),
            INDEPENDENCE_TTS_CONFIG="<unset>",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        output = result.stdout
        for section in ("Program", "Thinking", "Hearing", "Speaking", "Controls and camera"):
            self.assertIn(section, output)
        self.assertIn("✓ Microphone", output)
        self.assertIn("alsa:plughw:CARD=Device,DEV=0 (USB PnP Audio Device)", output)
        self.assertIn("Records your voice while ENTER is held.", output)
        self.assertIn("0 : imx708_wide", output, "the detected camera is shown")
        self.assertIn("pronounces text as es_MX", output)
        self.assertNotIn("\x1b[", output, "no colour codes when not a terminal")

    def test_check_colours_results_when_forced(self) -> None:
        result = self._run("--check", FORCE_COLOR="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("\x1b[32m✓\x1b[0m Microphone", result.stdout)

    def test_check_flags_a_microphone_independence_cannot_see(self) -> None:
        result = self._run("--check", FAKE_LISTED_MICROPHONE="alsa:something-else")
        self.assertEqual(result.returncode, 2)
        self.assertIn("✗ Microphone", result.stdout)
        self.assertIn("--audio-input-devices", result.stdout)
        self.assertFalse((self.record / "independence-args").exists())

    def _pinyin_voice(self, with_g2pw: bool) -> Path:
        voice = self.root / "zh_CN-chaowen-medium"
        voice.mkdir()
        (voice / "zh_CN-chaowen-medium.onnx").touch()
        (voice / "zh_CN-chaowen-medium.onnx.json").write_text(
            '{"language": {"code": "zh_CN"}, "phoneme_type": "pinyin"}', encoding="utf-8"
        )
        if with_g2pw:
            (voice / "g2pW").mkdir()
            (voice / "g2pW" / "g2pw.onnx").touch()
        return voice / "zh_CN-chaowen-medium.onnx"

    def test_check_ignores_g2pw_for_espeak_voices(self) -> None:
        result = self._run("--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("g2pW", result.stdout)

    def test_pinyin_voice_without_g2pw_fails_before_the_bridge(self) -> None:
        model = self._pinyin_voice(with_g2pw=False)
        result = self._run("--check", INDEPENDENCE_TTS_MODEL=str(model), INDEPENDENCE_TTS_CONFIG="<unset>")
        self.assertEqual(result.returncode, 2)
        self.assertIn("✗ Pinyin model (g2pW)", result.stdout)
        self.assertIn(f"{model.parent / 'g2pW' / 'g2pw.onnx'} (missing)", result.stdout)
        self.assertIn("Download g2pw.tar.gz into", result.stdout)

        started = self._run(INDEPENDENCE_TTS_MODEL=str(model), INDEPENDENCE_TTS_CONFIG="<unset>")
        self.assertEqual(started.returncode, 2)
        self.assertFalse((self.record / "bridge-args.json").exists())
        self.assertFalse((self.record / "independence-args").exists())

    def test_pinyin_voice_with_g2pw_and_a_new_enough_piper_passes(self) -> None:
        model = self._pinyin_voice(with_g2pw=True)
        result = self._run("--check", INDEPENDENCE_TTS_MODEL=str(model), INDEPENDENCE_TTS_CONFIG="<unset>")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("✓ Pinyin model (g2pW)", result.stdout)
        self.assertIn("✓ Piper pinyin support", result.stdout)

    def test_pinyin_voice_with_an_old_piper_explains_the_upgrade(self) -> None:
        model = self._pinyin_voice(with_g2pw=True)
        old_piper = self.root / "old-piper-venv" / "bin"
        old_piper.mkdir(parents=True)
        self._executable(old_piper / "piper", "#!/usr/bin/env bash\nexit 0\n")
        self._executable(old_piper / "python", "#!/usr/bin/env bash\nexit 1\n")
        result = self._run(
            "--check",
            INDEPENDENCE_TTS_MODEL=str(model),
            INDEPENDENCE_TTS_CONFIG="<unset>",
            INDEPENDENCE_PIPER_COMMAND=str(old_piper / "piper"),
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("✗ Piper pinyin support", result.stdout)
        self.assertIn("pip install --upgrade piper-tts", result.stdout)

    def test_check_lists_every_problem_at_once(self) -> None:
        self._camera(present=False)
        result = self._run(
            "--check",
            INDEPENDENCE_BIN=str(self.root / "not-built"),
            INDEPENDENCE_WHISPER_MODEL=str(self.root / "missing-whisper.bin"),
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("cargo build --release", result.stdout)
        self.assertIn("missing-whisper.bin", result.stdout)
        self.assertIn("no camera detected", result.stdout)
        self.assertIn("3 problem(s) found", result.stderr)
        self.assertFalse((self.record / "bridge-args.json").exists())

    def test_a_normal_start_reports_every_problem_too(self) -> None:
        result = self._run(
            INDEPENDENCE_BIN=str(self.root / "not-built"),
            PI_BUTTON_DEVICE=str(self.root / "event99"),
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("cargo build --release", result.stderr)
        self.assertIn("/dev/input/by-id", result.stderr)
        self.assertIn("--check", result.stderr)

    def test_system_prompt_cache_can_be_turned_off(self) -> None:
        result = self._run(BRIDGE_SYSTEM_PROMPT_CACHE="0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "--no-system-prompt-cache",
            json.loads((self.record / "bridge-args.json").read_text()),
        )

    def test_independence_failure_still_stops_the_bridge_and_returns_its_status(self) -> None:
        result = self._run(FAKE_INDEPENDENCE_STATUS="1")

        self.assertEqual(result.returncode, 1)
        self.assertEqual((self.record / "bridge-stopped").read_text(), "SIGTERM")

    def test_missing_camera_is_fatal_before_the_bridge_starts(self) -> None:
        self._camera(present=False)
        result = self._run()

        self.assertEqual(result.returncode, 2)
        self.assertIn("no camera detected", result.stderr)
        self.assertIn("reseat", result.stderr)
        self.assertFalse((self.record / "bridge-args.json").exists())
        self.assertFalse((self.record / "independence-args").exists())

    def test_missing_release_binary_names_the_build_command(self) -> None:
        result = self._run(INDEPENDENCE_BIN=str(self.root / "not-built"))

        self.assertEqual(result.returncode, 2)
        self.assertIn("cargo build --release", result.stderr)
        self.assertFalse((self.record / "bridge-args.json").exists())

    def test_missing_button_device_explains_how_to_find_it(self) -> None:
        result = self._run(PI_BUTTON_DEVICE=str(self.root / "event99"))

        self.assertEqual(result.returncode, 2)
        self.assertIn("/dev/input/by-id", result.stderr)

    def test_refuses_when_something_already_listens_on_the_bridge_port(self) -> None:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as existing:
            existing.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            existing.bind(("127.0.0.1", self.port))
            existing.listen()
            server = threading.Thread(target=self._answer_once, args=(existing,), daemon=True)
            server.start()
            result = self._run()

        self.assertEqual(result.returncode, 2)
        self.assertIn("already listening", result.stderr)
        self.assertFalse((self.record / "bridge-args.json").exists())

    @staticmethod
    def _answer_once(listener: socket.socket) -> None:
        connection, _ = listener.accept()
        with connection:
            connection.recv(4096)
            connection.sendall(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")

    def test_bridge_that_exits_during_startup_is_reported(self) -> None:
        result = self._run(FAKE_BRIDGE_MODE="exit")

        self.assertEqual(result.returncode, 2)
        self.assertIn("exited during startup", result.stderr)
        self.assertFalse((self.record / "independence-args").exists())

    def test_bridge_that_never_becomes_ready_times_out_and_is_stopped(self) -> None:
        result = self._run(FAKE_BRIDGE_MODE="never-ready", BRIDGE_READY_TIMEOUT_SECONDS="2")

        self.assertEqual(result.returncode, 2)
        self.assertIn("not ready after 2s", result.stderr)
        self.assertFalse((self.record / "independence-args").exists())
        self.assertEqual((self.record / "bridge-stopped").read_text(), "SIGTERM")

    def test_rejects_arguments(self) -> None:
        result = subprocess.run(
            [str(PI_HAILO_RUN_SCRIPT), "--mode", "private"],
            cwd=REPOSITORY_ROOT,
            env=self._environment(),
            text=True,
            capture_output=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage", result.stderr)


if __name__ == "__main__":
    unittest.main()
