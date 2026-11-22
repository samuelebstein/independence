"""Contract tests for the Hailo bridge that do not need Hailo hardware."""

from __future__ import annotations

import base64
import io
import json
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from urllib.request import Request, urlopen


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY_ROOT))

from tools.hailo_vlm_bridge import (  # noqa: E402
    BridgeApplication,
    BridgeConfig,
    BridgeHttpServer,
    BridgeRequestHandler,
    BridgeResponse,
    BackendInferenceError,
    HailoVlmBackend,
    MAX_IMAGES,
    SafeTelemetry,
    _loopback_host,
    _stop_on_terminate,
    install_termination_handler,
)


class FakeBackend:
    def __init__(self) -> None:
        self.calls: list[tuple[list[dict[str, object]], list[bytes]]] = []

    def generate(self, prompt: list[dict[str, object]], images: list[bytes]) -> str:
        self.calls.append((prompt, images))
        return "a local answer"

    def close(self) -> None:
        return


class TerminationHandlerTests(unittest.TestCase):
    def test_sigterm_uses_the_same_orderly_shutdown_as_ctrl_c(self) -> None:
        import signal

        previous = signal.getsignal(signal.SIGTERM)
        try:
            install_termination_handler()
            self.assertIs(signal.getsignal(signal.SIGTERM), _stop_on_terminate)
            with self.assertRaises(KeyboardInterrupt):
                _stop_on_terminate(signal.SIGTERM, None)
        finally:
            signal.signal(signal.SIGTERM, previous)


class HailoVlmBridgeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.backend = FakeBackend()
        self.config = BridgeConfig(
            host="127.0.0.1",
            port=11435,
            model="qwen3-vl:2b-instruct",
            hef_path=Path("/not-used-by-fake.hef"),
            max_generated_tokens=200,
        )

    def app(self, telemetry_path: Path | None = None) -> BridgeApplication:
        return BridgeApplication(
            self.config,
            self.backend,
            SafeTelemetry(str(telemetry_path) if telemetry_path else None),
        )

    def test_show_advertises_vision_and_health_names_the_loopback_endpoint(self) -> None:
        application = self.app()
        show = application.handle(
            "POST",
            "/api/show",
            json.dumps({"model": "qwen3-vl:2b-instruct"}).encode(),
        )
        health = application.handle("GET", "/healthz")

        self.assertEqual(show.status, 200)
        self.assertEqual(show.body["capabilities"], ["vision"])
        self.assertEqual(health.status, 200)
        self.assertEqual(health.body["status"], "ready")
        self.assertEqual(health.body["endpoint"], "http://127.0.0.1:11435")
        self.assertEqual(health.body["model"], "qwen3-vl:2b-instruct")

    def test_backend_failure_marks_health_degraded_and_exposes_bounded_reason(self) -> None:
        class FailingBackend(FakeBackend):
            def generate(self, prompt: list[dict[str, object]], images: list[bytes]) -> str:
                raise BackendInferenceError(
                    "Hailo VLM inference failed",
                    "hailo_inference_failed",
                    "RuntimeError: device queue stopped",
                )

        application = BridgeApplication(
            self.config,
            FailingBackend(),
            SafeTelemetry(None),
        )
        request = {
            "model": "qwen3-vl:2b-instruct",
            "stream": False,
            "messages": [{"role": "user", "content": "look", "images": []}],
        }

        response = application.handle("POST", "/api/chat", json.dumps(request).encode())
        health = application.handle("GET", "/healthz")

        self.assertEqual(response.status, 503)
        self.assertEqual(response.body["code"], "hailo_inference_failed")
        self.assertEqual(response.body["health"], "degraded")
        self.assertIn("device queue stopped", response.body["error"])
        self.assertEqual(health.status, 503)
        self.assertEqual(health.body["status"], "degraded")
        self.assertEqual(health.body["error_code"], "hailo_inference_failed")

        show = application.handle(
            "POST",
            "/api/show",
            json.dumps({"model": "qwen3-vl:2b-instruct"}).encode(),
        )
        self.assertEqual(show.status, 503)
        self.assertEqual(show.body["health"], "degraded")

    def test_successful_generation_recovers_degraded_health(self) -> None:
        application = self.app()
        application._health.mark_failure(  # type: ignore[attr-defined]
            BackendInferenceError("Hailo VLM inference failed", "hailo_inference_failed", "device stopped")
        )
        request = {
            "model": "qwen3-vl:2b-instruct",
            "stream": False,
            "messages": [{"role": "user", "content": "look", "images": []}],
        }

        response = application.handle("POST", "/api/chat", json.dumps(request).encode())
        health = application.handle("GET", "/healthz")

        self.assertEqual(response.status, 200)
        self.assertEqual(health.status, 200)
        self.assertEqual(health.body["status"], "ready")

    def test_client_disconnect_during_response_is_logged_without_traceback(self) -> None:
        class DisconnectedWriter:
            def write(self, _body: bytes) -> int:
                raise BrokenPipeError("client closed the loopback socket")

        handler = object.__new__(BridgeRequestHandler)
        handler.wfile = DisconnectedWriter()  # type: ignore[assignment]
        handler.send_response = lambda _status: None  # type: ignore[method-assign]
        handler.send_header = lambda _name, _value: None  # type: ignore[method-assign]
        handler.end_headers = lambda: None  # type: ignore[method-assign]

        stderr = io.StringIO()
        with redirect_stderr(stderr):
            handler._respond(BridgeResponse(200, {"status": "ready"}))

        self.assertIn("client disconnected before the response was sent", stderr.getvalue())

    def test_server_host_matches_independence_loopback_policy(self) -> None:
        self.assertEqual(_loopback_host("127.0.0.1"), "127.0.0.1")
        self.assertEqual(_loopback_host("::1"), "::1")
        with self.assertRaises(Exception):
            _loopback_host("0.0.0.0")
        with self.assertRaises(Exception):
            _loopback_host("127.0.0.2")

    def test_http_server_serves_the_ollama_show_contract_on_loopback(self) -> None:
        application = self.app()
        config = BridgeConfig(
            host="127.0.0.1",
            port=0,
            model="qwen3-vl:2b-instruct",
            hef_path=Path("/not-used-by-fake.hef"),
            max_generated_tokens=200,
        )
        try:
            server = BridgeHttpServer(config, application)
        except PermissionError:
            self.skipTest("this sandbox does not permit a loopback test listener")
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            request = Request(
                f"http://127.0.0.1:{server.server_port}/api/show",
                data=json.dumps({"model": "qwen3-vl:2b-instruct"}).encode(),
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urlopen(request, timeout=2) as response:
                payload = json.loads(response.read().decode())
                self.assertEqual(response.status, 200)
        finally:
            server.shutdown()
            worker.join(timeout=2)
            server.server_close()

        self.assertEqual(payload["capabilities"], ["vision"])

    def test_chat_preserves_one_image_in_the_qwen3_prompt(self) -> None:
        application = self.app()
        images = [f"in-memory image {index}".encode("ascii") for index in range(MAX_IMAGES)]
        request = {
            "model": "qwen3-vl:2b-instruct",
            "stream": False,
            "messages": [
                {"role": "system", "content": "local system instruction"},
                {"role": "assistant", "content": "prior answer"},
                {
                    "role": "user",
                    "content": "describe this picture",
                    "images": [base64.b64encode(image).decode("ascii") for image in images],
                },
            ],
        }

        response = application.handle("POST", "/api/chat", json.dumps(request).encode())

        self.assertEqual(response.status, 200)
        self.assertEqual(response.body["message"], {"role": "assistant", "content": "a local answer"})
        self.assertTrue(response.body["done"])
        self.assertEqual(len(self.backend.calls), 1)
        prompt, supplied_images = self.backend.calls[0]
        self.assertEqual(supplied_images, images)
        self.assertEqual(prompt[-1]["content"][0], {"type": "image"})
        self.assertEqual(prompt[-1]["content"][1], {"type": "text", "text": "describe this picture"})

    def test_more_than_the_bounded_image_limit_is_rejected_without_inference(self) -> None:
        application = self.app()
        request = {
            "model": "qwen3-vl:2b-instruct",
            "stream": False,
            "messages": [
                {
                    "role": "user",
                    "content": "private question",
                    "images": ["aW1hZ2U="] * (MAX_IMAGES + 1),
                }
            ],
        }

        response = application.handle("POST", "/api/chat", json.dumps(request).encode())

        self.assertEqual(response.status, 413)
        self.assertEqual(
            response.body["error"],
            f"this local Hailo VLM bridge accepts at most {MAX_IMAGES} images per request",
        )
        self.assertEqual(self.backend.calls, [])

    def test_hailo_backend_prepares_and_passes_each_frame_in_order(self) -> None:
        class FakeGeneration:
            def __enter__(self) -> list[str]:
                return ["ordered", " frames", "<|im_end|>"]

            def __exit__(self, *_: object) -> None:
                return None

        class FakeVlm:
            def __init__(self) -> None:
                self.frames: list[object] | None = None
                self.cleared = False

            def generate_all(self, **kwargs: object) -> str:
                self.frames = kwargs["frames"]  # type: ignore[assignment]
                return "ordered frames<|im_end|>"

            def clear_context(self) -> None:
                self.cleared = True

        backend = object.__new__(HailoVlmBackend)
        fake_vlm = FakeVlm()
        backend._generation_lock = threading.Lock()
        backend._vlm = fake_vlm
        backend._max_generated_tokens = 200
        backend._temperature = 0.1
        backend._seed = 42
        backend._prepare_image = lambda image: f"frame:{image.decode('ascii')}"

        answer = backend.generate([], [b"first"])

        self.assertEqual(answer, "ordered frames")
        self.assertEqual(fake_vlm.frames, ["frame:first"])
        self.assertTrue(fake_vlm.cleared)

    def test_optional_telemetry_omits_prompt_image_and_answer_contents(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            telemetry_path = Path(temporary_directory) / "hailo-vlm.jsonl"
            application = self.app(telemetry_path)
            secret_question = "private spoken question should never be logged"
            images = [b"private first image bytes"]
            encoded_images = [base64.b64encode(image).decode("ascii") for image in images]
            request = {
                "model": "qwen3-vl:2b-instruct",
                "stream": False,
                "messages": [
                    {"role": "user", "content": secret_question, "images": encoded_images}
                ],
            }

            response = application.handle("POST", "/api/chat", json.dumps(request).encode())
            logged = telemetry_path.read_text(encoding="utf-8")
            event = json.loads(logged)

        self.assertEqual(response.status, 200)
        self.assertNotIn(secret_question, logged)
        for encoded_image in encoded_images:
            self.assertNotIn(encoded_image, logged)
        self.assertNotIn("a local answer", logged)
        self.assertEqual(event["event"], "hailo_vlm.api_call")
        self.assertEqual(event["image_count"], 1)
        self.assertEqual(event["image_bytes"], sum(len(image) for image in images))
        self.assertEqual(event["message_count"], 1)


if __name__ == "__main__":
    unittest.main()
