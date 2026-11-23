"""Contract tests for the Hailo bridge that do not need Hailo hardware."""

from __future__ import annotations

import base64
import io
import json
import socket
import sys
import tempfile
import threading
import time
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
    GenerationResult,
    GenerationStats,
    HailoVlmBackend,
    MAX_IMAGES,
    SafeTelemetry,
    _loopback_host,
    _stop_on_terminate,
    install_termination_handler,
)


class FakeBackend:
    """Streams a fixed answer piece by piece and honours `on_token` stops."""

    def __init__(self, pieces: list[str] | None = None, delay: float = 0.0) -> None:
        self.calls: list[tuple[list[dict[str, object]], list[bytes]]] = []
        self.pieces = pieces if pieces is not None else ["a local", " answer"]
        self.delay = delay
        self.delivered = 0
        self.primed: list[str] = []

    def generate(self, prompt, images, on_token=None) -> GenerationResult:  # type: ignore[no-untyped-def]
        self.calls.append((prompt, images))
        stats = GenerationStats(prompt_tokens=42, time_to_first_token_ms=10, generation_ms=50)
        parts = []
        for piece in self.pieces:
            if self.delay:
                time.sleep(self.delay)
            stats.output_tokens += 1
            parts.append(piece)
            self.delivered += 1
            if on_token is not None and not on_token(piece):
                stats.outcome = "aborted"
                break
        return GenerationResult("".join(parts), stats)

    def prime_system_prompt(self, text: str) -> bool:
        self.primed.append(text)
        return True

    def close(self) -> None:
        return


class RecordingSink:
    def __init__(self, fail_after: int | None = None, gone: bool = False) -> None:
        self.began = False
        self.chunks: list[dict[str, object]] = []
        self.fail_after = fail_after
        self.gone = gone

    def begin_stream(self) -> bool:
        self.began = True
        return True

    def send_chunk(self, chunk: dict[str, object]) -> bool:
        if self.fail_after is not None and len(self.chunks) >= self.fail_after:
            return False
        self.chunks.append(chunk)
        return True

    def client_gone(self) -> bool:
        return self.gone


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
            def generate(self, prompt, images, on_token=None):  # type: ignore[no-untyped-def]
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
        fake_vlm = FakeVlm(["ordered", " frames", "<|im_end|>"])
        backend = hailo_backend(fake_vlm, cache=False)

        result = backend.generate([], [b"first"])

        self.assertEqual(result.text, "ordered frames")
        self.assertEqual(fake_vlm.generate_calls[0]["frames"], ["frame:first"])
        self.assertGreaterEqual(fake_vlm.cleared, 1)

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


class FakeGeneration:
    def __init__(self, vlm: "FakeVlm", tokens: list[str]) -> None:
        self.vlm = vlm
        self.tokens = tokens

    def __enter__(self):  # type: ignore[no-untyped-def]
        def iterate():  # type: ignore[no-untyped-def]
            for token in self.tokens:
                self.vlm.yielded += 1
                self.vlm.usage += 1
                yield token

        return iterate()

    def __exit__(self, *_: object) -> None:
        self.vlm.exits += 1


class FakeVlm:
    """Mimics hailo_platform.genai.VLM: context accumulates until cleared."""

    def __init__(self, tokens: list[str], prompt_cost: int = 7) -> None:
        self.tokens = tokens
        self.prompt_cost = prompt_cost
        self.generate_calls: list[dict[str, object]] = []
        self.usage = 0
        self.cleared = 0
        self.saved = 0
        self.loaded = 0
        self.yielded = 0
        self.exits = 0

    def generate(self, **kwargs: object) -> FakeGeneration:
        self.generate_calls.append(kwargs)
        # Reading the prompt fills the context before the first token.
        self.usage += self.prompt_cost * len(kwargs["prompt"])  # type: ignore[arg-type]
        limit = int(kwargs["max_generated_tokens"])  # type: ignore[arg-type]
        return FakeGeneration(self, self.tokens[:limit])

    def get_context_usage_size(self) -> int:
        return self.usage

    def save_context(self) -> bytes:
        self.saved += 1
        return f"snapshot:{self.usage}".encode()

    def load_context(self, data: bytes) -> None:
        self.loaded += 1
        self.usage = int(data.decode().split(":")[1])

    def clear_context(self) -> None:
        self.cleared += 1
        self.usage = 0

    def get_generation_recovery_sequence(self) -> str:
        return "<|recovery|>"


def hailo_backend(fake_vlm: FakeVlm, cache: bool = True, cap: int = 200) -> HailoVlmBackend:
    backend = object.__new__(HailoVlmBackend)
    backend._generation_lock = threading.Lock()
    backend._vlm = fake_vlm
    backend._max_generated_tokens = cap
    backend._temperature = 0.3
    backend._seed = 42
    backend._cache_enabled = cache
    backend._system_text = None
    backend._system_snapshot = None
    backend._recovery_sequence = fake_vlm.get_generation_recovery_sequence()
    backend._prepare_image = lambda image: f"frame:{image.decode('ascii')}"
    return backend


def system_prompt(text: str = "be brief") -> list[dict[str, object]]:
    return [
        {"role": "system", "content": [{"type": "text", "text": text}]},
        {"role": "user", "content": [{"type": "text", "text": "question"}]},
    ]


class HailoBackendGenerationTests(unittest.TestCase):
    def test_metrics_count_tokens_and_time_without_text(self) -> None:
        fake_vlm = FakeVlm(["Hello", " there", "<|recovery|>", "<|im_end|>"])
        backend = hailo_backend(fake_vlm, cache=False)

        result = backend.generate(system_prompt(), [])

        self.assertEqual(result.text, "Hello there")
        stats = result.stats
        self.assertEqual(stats.outcome, "completed")
        self.assertEqual(stats.output_tokens, 4)
        self.assertEqual(stats.context_tokens_before, 0)
        self.assertEqual(stats.prompt_tokens, 14)  # two messages at 7 each
        self.assertIsNotNone(stats.time_to_first_token_ms)
        self.assertFalse(stats.hit_token_cap)
        self.assertNotIn("Hello", stats.log_line())
        self.assertIn("prompt_tokens=14", stats.log_line())

    def test_token_cap_is_reported(self) -> None:
        backend = hailo_backend(FakeVlm(["a", "b", "c", "d"]), cache=False, cap=3)
        result = backend.generate(system_prompt(), [])
        self.assertTrue(result.stats.hit_token_cap)
        self.assertEqual(result.stats.output_tokens, 3)

    def test_stopping_mid_generation_leaves_the_generator_and_clears_context(self) -> None:
        fake_vlm = FakeVlm([f"t{index}" for index in range(50)])
        backend = hailo_backend(fake_vlm, cache=False)
        received: list[str] = []

        def stop_after_three(piece: str) -> bool:
            received.append(piece)
            return len(received) < 3

        result = backend.generate(system_prompt(), [], stop_after_three)

        self.assertEqual(result.stats.outcome, "aborted")
        self.assertEqual(len(received), 3)
        self.assertEqual(fake_vlm.yielded, 3, "no further tokens are pulled from the Hailo")
        self.assertEqual(fake_vlm.exits, 1)
        self.assertEqual(fake_vlm.usage, 0, "context is cleared after an abort")

    def test_system_prompt_snapshot_is_built_once_then_restored(self) -> None:
        fake_vlm = FakeVlm(["ok"])
        backend = hailo_backend(fake_vlm)

        first = backend.generate(system_prompt(), [])
        second = backend.generate(system_prompt(), [])

        self.assertEqual(first.stats.system_prompt_cache, "miss")
        self.assertEqual(second.stats.system_prompt_cache, "hit")
        self.assertEqual(fake_vlm.saved, 1)
        self.assertEqual(fake_vlm.loaded, 1)
        # Priming generated one token from the system message alone; later
        # requests send only the remaining messages.
        self.assertEqual(fake_vlm.generate_calls[0]["max_generated_tokens"], 1)
        self.assertEqual(len(fake_vlm.generate_calls[0]["prompt"]), 1)
        self.assertEqual(fake_vlm.generate_calls[0]["prompt"][0]["role"], "system")
        self.assertEqual([m["role"] for m in fake_vlm.generate_calls[2]["prompt"]], ["user"])
        self.assertGreater(second.stats.context_tokens_before, 0)
        self.assertEqual(second.stats.prompt_tokens, 7, "only the user message is read")

    def test_a_different_system_prompt_rebuilds_the_snapshot(self) -> None:
        fake_vlm = FakeVlm(["ok"])
        backend = hailo_backend(fake_vlm)
        backend.generate(system_prompt("voice"), [])
        result = backend.generate(system_prompt("typed"), [])
        self.assertEqual(result.stats.system_prompt_cache, "miss")
        self.assertEqual(fake_vlm.saved, 2)

    def test_prime_builds_the_snapshot_before_any_question(self) -> None:
        fake_vlm = FakeVlm(["ok"])
        backend = hailo_backend(fake_vlm)

        self.assertTrue(backend.prime_system_prompt("be brief"))
        self.assertEqual(fake_vlm.usage, 0, "priming leaves no context behind")
        result = backend.generate(system_prompt("be brief"), [])
        self.assertEqual(result.stats.system_prompt_cache, "hit")
        self.assertEqual(fake_vlm.saved, 1)

    def test_cache_off_sends_the_whole_prompt(self) -> None:
        fake_vlm = FakeVlm(["ok"])
        backend = hailo_backend(fake_vlm, cache=False)
        self.assertFalse(backend.prime_system_prompt("be brief"))
        result = backend.generate(system_prompt(), [])
        self.assertEqual(result.stats.system_prompt_cache, "off")
        self.assertEqual(len(fake_vlm.generate_calls[-1]["prompt"]), 2)
        self.assertEqual(fake_vlm.saved, 0)

    def test_snapshot_failure_falls_back_to_reading_the_prompt(self) -> None:
        class NoSaveVlm(FakeVlm):
            def save_context(self) -> bytes:
                raise RuntimeError("save not supported")

        fake_vlm = NoSaveVlm(["ok"])
        backend = hailo_backend(fake_vlm)
        stderr = io.StringIO()
        with redirect_stderr(stderr):
            result = backend.generate(system_prompt(), [])
        self.assertEqual(result.text, "ok")
        self.assertEqual(result.stats.system_prompt_cache, "unavailable")
        self.assertEqual(len(fake_vlm.generate_calls[-1]["prompt"]), 2)
        self.assertIn("cache unavailable", stderr.getvalue())


class BridgeStreamingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.config = BridgeConfig(
            host="127.0.0.1",
            port=11435,
            model="qwen3-vl:2b-instruct",
            hef_path=Path("/not-used-by-fake.hef"),
            max_generated_tokens=200,
        )

    def request(self, stream: bool) -> bytes:
        return json.dumps(
            {
                "model": "qwen3-vl:2b-instruct",
                "stream": stream,
                "messages": [
                    {"role": "system", "content": "be brief"},
                    {"role": "user", "content": "private question"},
                ],
            }
        ).encode()

    def test_non_streaming_answer_carries_ollama_metrics(self) -> None:
        application = BridgeApplication(self.config, FakeBackend(), SafeTelemetry(None))
        with redirect_stderr(io.StringIO()):
            response = application.handle("POST", "/api/chat", self.request(False))
        self.assertEqual(response.body["message"]["content"], "a local answer")
        self.assertEqual(response.body["done_reason"], "stop")
        self.assertEqual(response.body["eval_count"], 2)
        self.assertEqual(response.body["prompt_eval_count"], 42)

    def test_streaming_sends_pieces_then_a_final_done_chunk(self) -> None:
        application = BridgeApplication(self.config, FakeBackend(), SafeTelemetry(None))
        sink = RecordingSink()
        with redirect_stderr(io.StringIO()):
            response = application.handle("POST", "/api/chat", self.request(True), sink)

        self.assertTrue(response.streamed)
        self.assertTrue(sink.began)
        contents = [chunk["message"]["content"] for chunk in sink.chunks[:-1]]  # type: ignore[index]
        self.assertEqual(contents, ["a local", " answer"])
        self.assertTrue(all(chunk["done"] is False for chunk in sink.chunks[:-1]))
        final = sink.chunks[-1]
        self.assertTrue(final["done"])
        self.assertEqual(final["done_reason"], "stop")
        self.assertEqual(final["eval_count"], 2)

    def test_streaming_stops_generation_when_the_client_goes_away(self) -> None:
        backend = FakeBackend(pieces=[f"p{index}" for index in range(20)])
        application = BridgeApplication(self.config, backend, SafeTelemetry(None))
        sink = RecordingSink(fail_after=3)
        with redirect_stderr(io.StringIO()):
            application.handle("POST", "/api/chat", self.request(True), sink)
        self.assertEqual(backend.delivered, 4, "the first failed write stops generation")

    def test_non_streaming_generation_stops_when_the_client_hangs_up(self) -> None:
        backend = FakeBackend(pieces=[f"p{index}" for index in range(20)])
        application = BridgeApplication(self.config, backend, SafeTelemetry(None))
        with redirect_stderr(io.StringIO()):
            application.handle("POST", "/api/chat", self.request(False), RecordingSink(gone=True))
        self.assertEqual(backend.delivered, 1)

    def test_streaming_failure_is_sent_as_a_final_error_chunk(self) -> None:
        class FailingBackend(FakeBackend):
            def generate(self, prompt, images, on_token=None):  # type: ignore[no-untyped-def]
                raise BackendInferenceError("Hailo VLM inference failed", "hailo_inference_failed", "device stopped")

        application = BridgeApplication(self.config, FailingBackend(), SafeTelemetry(None))
        sink = RecordingSink()
        with redirect_stderr(io.StringIO()):
            response = application.handle("POST", "/api/chat", self.request(True), sink)
        self.assertTrue(response.streamed)
        self.assertEqual(sink.chunks[-1]["code"], "hailo_inference_failed")
        self.assertTrue(sink.chunks[-1]["done"])
        self.assertEqual(application.handle("GET", "/healthz").body["status"], "degraded")

    def test_system_only_request_primes_the_cache_without_answering(self) -> None:
        backend = FakeBackend()
        application = BridgeApplication(self.config, backend, SafeTelemetry(None))
        body = json.dumps(
            {
                "model": "qwen3-vl:2b-instruct",
                "stream": False,
                "messages": [{"role": "system", "content": "be brief"}],
            }
        ).encode()
        with redirect_stderr(io.StringIO()):
            response = application.handle("POST", "/api/chat", body)
        self.assertEqual(response.status, 200)
        self.assertEqual(response.body["done_reason"], "load")
        self.assertEqual(response.body["message"]["content"], "")
        self.assertEqual(backend.primed, ["be brief"])
        self.assertEqual(backend.calls, [])

    def test_telemetry_records_generation_metrics_but_no_text(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bridge.jsonl"
            application = BridgeApplication(self.config, FakeBackend(), SafeTelemetry(str(path)))
            with redirect_stderr(io.StringIO()):
                application.handle("POST", "/api/chat", self.request(True), RecordingSink())
            logged = path.read_text(encoding="utf-8")
        event = json.loads(logged)
        self.assertEqual(event["generation_output_tokens"], 2)
        self.assertEqual(event["generation_prompt_tokens"], 42)
        self.assertTrue(event["stream"])
        self.assertNotIn("private question", logged)
        self.assertNotIn("a local", logged)
        self.assertNotIn("be brief", logged)


class BridgeHttpStreamingTests(unittest.TestCase):
    """Real loopback sockets: NDJSON over HTTP and hang-up detection."""

    def serve(self, backend: FakeBackend):  # type: ignore[no-untyped-def]
        config = BridgeConfig(
            host="127.0.0.1",
            port=0,
            model="qwen3-vl:2b-instruct",
            hef_path=Path("/not-used-by-fake.hef"),
            max_generated_tokens=200,
        )
        application = BridgeApplication(config, backend, SafeTelemetry(None))
        try:
            server = BridgeHttpServer(config, application)
        except PermissionError:
            self.skipTest("this sandbox does not permit a loopback test listener")
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        self.addCleanup(server.server_close)
        self.addCleanup(worker.join, 2)
        self.addCleanup(server.shutdown)
        return server

    def chat(self, stream: bool) -> bytes:
        return json.dumps(
            {
                "model": "qwen3-vl:2b-instruct",
                "stream": stream,
                "messages": [{"role": "user", "content": "question"}],
            }
        ).encode()

    def test_streamed_response_is_newline_delimited_json(self) -> None:
        server = self.serve(FakeBackend(pieces=["one", " two", " three"]))
        request = Request(
            f"http://127.0.0.1:{server.server_port}/api/chat",
            data=self.chat(True),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with redirect_stderr(io.StringIO()), urlopen(request, timeout=5) as response:
            self.assertEqual(response.headers["Content-Type"], "application/x-ndjson")
            lines = [json.loads(line) for line in response.read().decode().splitlines()]
        self.assertEqual("".join(line["message"]["content"] for line in lines), "one two three")
        self.assertTrue(lines[-1]["done"])

    def test_hanging_up_stops_a_non_streaming_generation(self) -> None:
        backend = FakeBackend(pieces=[f"p{index}" for index in range(100)], delay=0.02)
        server = self.serve(backend)
        body = self.chat(False)
        with redirect_stderr(io.StringIO()):
            connection = socket.create_connection(("127.0.0.1", server.server_port), timeout=5)
            connection.sendall(
                b"POST /api/chat HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n"
                + f"Content-Length: {len(body)}\r\n\r\n".encode()
                + body
            )
            time.sleep(0.2)
            connection.close()
            deadline = time.monotonic() + 3
            while backend.delivered and time.monotonic() < deadline:
                before = backend.delivered
                time.sleep(0.1)
                if backend.delivered == before:
                    break
        self.assertLess(backend.delivered, 50, "generation stops soon after the hang-up")


if __name__ == "__main__":
    unittest.main()
