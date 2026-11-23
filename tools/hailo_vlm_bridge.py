#!/usr/bin/env python3
"""Serve a local Hailo VLM through the small Ollama contract Independence uses.

This program intentionally has no web framework dependency.  Hailo Apps already
provides the hardware/runtime dependencies; the standard-library HTTP server is
enough for one loopback-only local client.  It accepts the existing
``/api/show`` and ``/api/chat`` shapes (``stream: false`` returns one JSON
object; ``stream: true`` returns Ollama-style newline-delimited JSON chunks), so
the Rust application keeps owning capability checks, conversation history,
camera consent, and turn authorization.

Generation always runs through Hailo's streaming ``generate()`` so the bridge
can measure it (time to first token, token counts, tokens/s) and stop it as
soon as the client goes away. A request whose first message is the system
prompt can reuse a saved context snapshot of that prompt instead of re-reading
it every turn. The snapshot holds only the system prompt, never user content.

The bridge never writes prompts, image bytes, base64 data, or model replies to
its own logs or optional JSONL telemetry; it records counts and timings only.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import ipaddress
import json
import os
import signal
import socket
import sys
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable, Protocol
from urllib.parse import urlsplit


BRIDGE_VERSION = "0.4.0"
DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 11435
DEFAULT_MODEL = "qwen3-vl:2b-instruct"
DEFAULT_MAX_GENERATED_TOKENS = 200
DEFAULT_TEMPERATURE = 0.1
DEFAULT_SEED = 42
MAX_REQUEST_BYTES = 14 * 1024 * 1024
MAX_IMAGE_BYTES = 10 * 1024 * 1024
MAX_IMAGES = 1
MAX_IMAGE_PIXELS = 10_000_000
MAX_MESSAGES = 16
MAX_TEXT_BYTES = 64 * 1024
VLM_IMAGE_WIDTH = 512
VLM_IMAGE_HEIGHT = 288


class BridgeError(Exception):
    """A client-safe HTTP error that deliberately contains no request data."""

    def __init__(self, status: int, message: str, outcome: str) -> None:
        super().__init__(message)
        self.status = status
        self.message = message
        self.outcome = outcome


class BackendStartupError(Exception):
    """A concise, safe diagnosis for an unavailable local Hailo backend."""


class BackendInferenceError(Exception):
    """A bounded, safe diagnosis for one failed Hailo generation."""

    def __init__(self, message: str, code: str, detail: str | None = None) -> None:
        super().__init__(message)
        self.message = message
        self.code = code
        self.detail = detail

    def public_message(self) -> str:
        if self.detail:
            return f"{self.message}: {self.detail}"
        return self.message


class InvalidImageError(Exception):
    """A client input error discovered while decoding image bytes locally."""


def _single_text(message: dict[str, Any]) -> str | None:
    """The text of a prompt message made of exactly one text item."""

    content = message.get("content")
    if (
        isinstance(content, list)
        and len(content) == 1
        and isinstance(content[0], dict)
        and content[0].get("type") == "text"
        and isinstance(content[0].get("text"), str)
    ):
        return content[0]["text"]
    return None


def _safe_exception_detail(error: BaseException, limit: int = 240) -> str:
    """Keep runtime diagnostics bounded and free of request payload formatting."""

    detail = " ".join(str(error).split())
    if not detail:
        detail = type(error).__name__
    else:
        detail = f"{type(error).__name__}: {detail}"
    if len(detail) > limit:
        detail = f"{detail[:limit]}…"
    return detail


@dataclass(frozen=True)
class BackendHealthSnapshot:
    status: str
    error_code: str | None = None
    error: str | None = None


class BackendHealth:
    """Tracks runtime readiness without recording prompts, images, or answers."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._snapshot = BackendHealthSnapshot("ready")

    def snapshot(self) -> BackendHealthSnapshot:
        with self._lock:
            return self._snapshot

    def mark_failure(self, error: BackendInferenceError) -> None:
        snapshot = BackendHealthSnapshot("degraded", error.code, error.public_message())
        with self._lock:
            changed = snapshot != self._snapshot
            self._snapshot = snapshot
        if changed:
            print(
                f"Hailo VLM health degraded: {error.code}: {error.public_message()}",
                file=sys.stderr,
                flush=True,
            )

    def mark_ready(self) -> None:
        with self._lock:
            recovered = self._snapshot.status != "ready"
            self._snapshot = BackendHealthSnapshot("ready")
        if recovered:
            print("Hailo VLM health recovered: ready", file=sys.stderr, flush=True)


@dataclass(frozen=True)
class BridgeConfig:
    host: str
    port: int
    model: str
    hef_path: Path
    max_generated_tokens: int
    temperature: float = DEFAULT_TEMPERATURE
    seed: int = DEFAULT_SEED
    telemetry_jsonl: str | None = None
    system_prompt_cache: bool = True

    @property
    def endpoint(self) -> str:
        if ":" in self.host:
            return f"http://[{self.host}]:{self.port}"
        return f"http://{self.host}:{self.port}"


@dataclass(frozen=True)
class BridgeResponse:
    status: int
    body: dict[str, Any]
    # True when the response was already written chunk by chunk.
    streamed: bool = False


@dataclass
class GenerationStats:
    """Counts and timings for one generation. Never contains text."""

    outcome: str = "completed"  # completed | aborted
    lock_wait_ms: int = 0
    system_prompt_cache: str = "off"  # off | hit | miss | unavailable
    context_tokens_before: int | None = None
    prompt_tokens: int | None = None
    output_tokens: int = 0
    time_to_first_token_ms: int | None = None
    generation_ms: int = 0
    tokens_per_second: float | None = None
    hit_token_cap: bool = False

    def fields(self) -> dict[str, Any]:
        return {
            "outcome": self.outcome,
            "lock_wait_ms": self.lock_wait_ms,
            "system_prompt_cache": self.system_prompt_cache,
            "context_tokens_before": self.context_tokens_before,
            "prompt_tokens": self.prompt_tokens,
            "output_tokens": self.output_tokens,
            "time_to_first_token_ms": self.time_to_first_token_ms,
            "generation_ms": self.generation_ms,
            "tokens_per_second": self.tokens_per_second,
            "hit_token_cap": self.hit_token_cap,
        }

    def log_line(self) -> str:
        parts = []
        for key, value in self.fields().items():
            if value is None:
                value = "n/a"
            elif isinstance(value, bool):
                value = "yes" if value else "no"
            parts.append(f"{key}={value}")
        return "Hailo VLM request: " + " ".join(parts)

    def ollama_metrics(self) -> dict[str, Any]:
        """Ollama-compatible counters, so existing clients can read them."""

        metrics: dict[str, Any] = {
            "eval_count": self.output_tokens,
            "total_duration": self.lock_wait_ms * 1_000_000 + self.generation_ms * 1_000_000,
        }
        if self.prompt_tokens is not None:
            metrics["prompt_eval_count"] = self.prompt_tokens
        if self.time_to_first_token_ms is not None:
            metrics["prompt_eval_duration"] = self.time_to_first_token_ms * 1_000_000
            metrics["eval_duration"] = max(
                0, (self.generation_ms - self.time_to_first_token_ms) * 1_000_000
            )
        return metrics


@dataclass
class GenerationResult:
    text: str
    stats: GenerationStats


# Receives each generated text piece; returns False to stop generation.
TokenSink = Callable[[str], bool]


@dataclass
class RequestMetadata:
    message_count: int = 0
    image_count: int = 0
    image_bytes: int = 0
    generation_ms: int | None = None
    stream: bool = False
    generation: GenerationStats | None = None


class VlmBackend(Protocol):
    """The small bridge-owned contract that makes API tests hardware-free."""

    def generate(
        self,
        prompt: list[dict[str, Any]],
        images: list[bytes],
        on_token: TokenSink | None = None,
    ) -> GenerationResult: ...

    def prime_system_prompt(self, text: str) -> bool: ...

    def close(self) -> None: ...


class ResponseSink(Protocol):
    """The live HTTP connection, as seen by a request that streams or that
    should stop when the client goes away."""

    def begin_stream(self) -> bool: ...

    def send_chunk(self, chunk: dict[str, Any]) -> bool: ...

    def client_gone(self) -> bool: ...


class HailoVlmBackend:
    """A one-model, serialized Hailo Qwen3-VL runtime.

    Hailo's VLM context is cleared after every request.  Independence supplies
    accepted text history again in the next request, rather than leaving private
    state resident in the accelerator runtime. The only state kept between
    requests is an optional snapshot of the fixed, non-private system prompt.
    """

    def __init__(
        self,
        hef_path: Path,
        max_generated_tokens: int,
        temperature: float = DEFAULT_TEMPERATURE,
        seed: int = DEFAULT_SEED,
        system_prompt_cache: bool = True,
    ) -> None:
        if not hef_path.is_file():
            raise BackendStartupError(
                "HAILO_VLM_HEF must point to a downloaded, readable Hailo VLM .hef file."
            )

        try:
            import cv2  # type: ignore[import-not-found]
            import numpy as np  # type: ignore[import-not-found]
            from hailo_platform import VDevice  # type: ignore[import-not-found]
            from hailo_platform.genai import VLM  # type: ignore[import-not-found]
            from hailo_apps.python.core.common.defines import (  # type: ignore[import-not-found]
                SHARED_VDEVICE_GROUP_ID,
            )
        except Exception as error:
            raise BackendStartupError(
                "Hailo Apps Python dependencies are unavailable. Activate hailo-apps/setup_env.sh "
                "and install its gen-ai extra first."
            ) from error

        self._cv2 = cv2
        self._np = np
        self._vdevice: Any | None = None
        self._vlm: Any | None = None
        self._generation_lock = threading.Lock()
        self._cache_enabled = system_prompt_cache
        self._system_text: str | None = None
        self._system_snapshot: Any | None = None
        self._recovery_sequence: str | None = None
        try:
            params = VDevice.create_params()
            # Hailo's shared VDevice group lets this local VLM coexist with
            # ordinary vision pipelines. It does not make two GenAI models
            # concurrent; this bridge owns exactly one VLM.
            params.group_id = SHARED_VDEVICE_GROUP_ID
            self._vdevice = VDevice(params)
            self._vlm = VLM(self._vdevice, str(hef_path))
            self._max_generated_tokens = max_generated_tokens
            self._temperature = temperature
            self._seed = seed
        except Exception as error:  # Expose only bounded, request-free detail.
            self.close()
            raise BackendStartupError(
                "The Hailo VLM could not initialize. "
                f"{_safe_exception_detail(error)} Check the HAT, HailoRT version, "
                "and downloaded VLM model."
            ) from error
        try:
            # Hailo may append a fixed recovery sequence to a generation; it
            # is a runtime artifact, not part of the answer.
            self._recovery_sequence = self._vlm.get_generation_recovery_sequence()
        except Exception:
            self._recovery_sequence = None

    def _context_usage(self) -> int | None:
        try:
            return int(self._vlm.get_context_usage_size())
        except Exception:
            return None

    def _clear(self) -> None:
        try:
            self._vlm.clear_context()
        except Exception:
            # A failed cleanup must not replace the safe inference error or
            # expose a device/runtime diagnostic to a caller.
            pass

    def _build_system_snapshot(self, text: str) -> bool:
        """Read only the system prompt and save that context (lock held).

        Hailo has no "prefill only" call; like Hailo Apps' own context cache,
        this generates one token so the prompt is read into the context, then
        saves the context. On success the context still holds the system
        prompt, so the caller can continue with the remaining messages.
        """

        self._clear()
        try:
            system = [{"role": "system", "content": [{"type": "text", "text": text}]}]
            with self._vlm.generate(
                prompt=system,
                frames=[],
                temperature=self._temperature,
                seed=self._seed,
                max_generated_tokens=1,
            ) as generation:
                for _token in generation:
                    pass
            snapshot = self._vlm.save_context()
        except Exception as error:
            print(
                "Hailo VLM system prompt cache unavailable: "
                f"{_safe_exception_detail(error)}; reading the system prompt every request",
                file=sys.stderr,
                flush=True,
            )
            self._cache_enabled = False
            self._system_text = None
            self._system_snapshot = None
            self._clear()
            return False
        if not snapshot:
            self._cache_enabled = False
            self._clear()
            return False
        self._system_text = text
        self._system_snapshot = snapshot
        return True

    def prime_system_prompt(self, text: str) -> bool:
        """Prepare the system prompt snapshot before the first question."""

        with self._generation_lock:
            if not self._cache_enabled:
                return False
            if self._system_text == text and self._system_snapshot is not None:
                return True
            try:
                return self._build_system_snapshot(text)
            finally:
                self._clear()

    def _start_from_system_prompt(
        self, prompt: list[dict[str, Any]], stats: GenerationStats
    ) -> list[dict[str, Any]]:
        """Return the messages still to send, using the snapshot if possible."""

        if not self._cache_enabled or not prompt or prompt[0].get("role") != "system":
            stats.system_prompt_cache = "off"
            return prompt
        text = _single_text(prompt[0])
        if text is None:
            stats.system_prompt_cache = "off"
            return prompt
        if self._system_text == text and self._system_snapshot is not None:
            try:
                self._vlm.load_context(self._system_snapshot)
            except Exception as error:
                print(
                    "Hailo VLM could not restore the system prompt snapshot: "
                    f"{_safe_exception_detail(error)}; reading it in full",
                    file=sys.stderr,
                    flush=True,
                )
                self._cache_enabled = False
                self._system_snapshot = None
                self._system_text = None
                self._clear()
                stats.system_prompt_cache = "unavailable"
                return prompt
            stats.system_prompt_cache = "hit"
            return prompt[1:]
        if self._build_system_snapshot(text):
            stats.system_prompt_cache = "miss"
            return prompt[1:]
        stats.system_prompt_cache = "unavailable"
        return prompt

    def generate(
        self,
        prompt: list[dict[str, Any]],
        images: list[bytes],
        on_token: TokenSink | None = None,
    ) -> GenerationResult:
        # Hailo validates that each image placeholder in ``prompt`` has a
        # matching frame. The bridge accepts one image, so these lists contain
        # either one matching pair or no image at all.
        frames = [self._prepare_image(image_bytes) for image_bytes in images]
        stats = GenerationStats()
        waiting_since = time.monotonic()

        with self._generation_lock:
            stats.lock_wait_ms = int((time.monotonic() - waiting_since) * 1000)
            parts: list[str] = []
            try:
                messages = self._start_from_system_prompt(prompt, stats)
                stats.context_tokens_before = self._context_usage()
                started = time.monotonic()
                first_token_at: float | None = None
                # Streaming generation, so tokens can be measured, forwarded,
                # and abandoned the moment the client no longer wants them.
                with self._vlm.generate(
                    prompt=messages,
                    frames=frames,
                    temperature=self._temperature,
                    seed=self._seed,
                    max_generated_tokens=self._max_generated_tokens,
                ) as generation:
                    for token in generation:
                        if first_token_at is None:
                            first_token_at = time.monotonic()
                        stats.output_tokens += 1
                        if self._recovery_sequence and token == self._recovery_sequence:
                            continue
                        piece = str(token).replace("<|im_end|>", "")
                        if not piece:
                            continue
                        parts.append(piece)
                        if on_token is not None and not on_token(piece):
                            stats.outcome = "aborted"
                            break
                finished = time.monotonic()
                context_after = self._context_usage()
            except Exception as error:
                raise BackendInferenceError(
                    "Hailo VLM inference failed",
                    "hailo_inference_failed",
                    _safe_exception_detail(error),
                ) from error
            finally:
                self._clear()

        stats.generation_ms = int((finished - started) * 1000)
        if first_token_at is not None:
            stats.time_to_first_token_ms = int((first_token_at - started) * 1000)
            if stats.output_tokens > 1 and finished > first_token_at:
                stats.tokens_per_second = round(
                    (stats.output_tokens - 1) / (finished - first_token_at), 2
                )
        stats.hit_token_cap = stats.output_tokens >= self._max_generated_tokens
        if stats.context_tokens_before is not None and context_after is not None:
            prompt_tokens = context_after - stats.context_tokens_before - stats.output_tokens
            stats.prompt_tokens = prompt_tokens if prompt_tokens >= 0 else None

        text = "".join(parts).strip()
        if not text and stats.outcome == "completed":
            raise BackendInferenceError(
                "Hailo VLM returned no usable text",
                "empty_generation",
            )
        return GenerationResult(text, stats)

    def _prepare_image(self, image_bytes: bytes) -> Any:
        if len(image_bytes) > MAX_IMAGE_BYTES:
            raise InvalidImageError("The supplied image exceeds the local size limit.")

        encoded = self._np.frombuffer(image_bytes, dtype=self._np.uint8)
        image = self._cv2.imdecode(encoded, self._cv2.IMREAD_COLOR)
        if image is None or image.size == 0:
            raise InvalidImageError("The supplied image could not be decoded locally.")

        height, width = image.shape[:2]
        if height * width > MAX_IMAGE_PIXELS:
            raise InvalidImageError("The supplied image exceeds the local pixel limit.")

        # Exact Qwen3 smoke-test preprocessing: convert BGR to RGB, then resize
        # directly to the model's required 512x288 uint8 frame.
        rgb = self._cv2.cvtColor(image, self._cv2.COLOR_BGR2RGB)
        return self._cv2.resize(
            rgb,
            (VLM_IMAGE_WIDTH, VLM_IMAGE_HEIGHT),
            interpolation=self._cv2.INTER_LINEAR,
        ).astype(self._np.uint8)

    def close(self) -> None:
        with getattr(self, "_generation_lock", threading.Lock()):
            self._system_snapshot = None
            self._system_text = None
            if getattr(self, "_vlm", None) is not None:
                try:
                    self._vlm.clear_context()
                    self._vlm.release()
                except Exception:
                    pass
                self._vlm = None
            if getattr(self, "_vdevice", None) is not None:
                try:
                    self._vdevice.release()
                except Exception:
                    pass
                self._vdevice = None


class SafeTelemetry:
    """Optional local JSONL metadata with an explicit no-payload boundary."""

    def __init__(self, path: str | None) -> None:
        self._path = Path(path).expanduser() if path else None
        self._lock = threading.Lock()
        if self._path is not None and not self._path.parent.is_dir():
            raise ValueError("the telemetry directory does not exist")

    def record(self, fields: dict[str, Any]) -> None:
        if self._path is None:
            return
        line = json.dumps(fields, separators=(",", ":"), sort_keys=True)
        try:
            with self._lock, self._path.open("a", encoding="utf-8") as output:
                output.write(line)
                output.write("\n")
        except OSError:
            # Keep inference available if an opt-in observability file becomes
            # unwritable. The warning names only the failure category.
            print("warning: hailo_vlm_telemetry_write_failed", file=sys.stderr)


class BridgeApplication:
    """Maps a narrow, validated HTTP contract to a single local VLM backend."""

    def __init__(
        self,
        config: BridgeConfig,
        backend: VlmBackend,
        telemetry: SafeTelemetry,
        health: BackendHealth | None = None,
    ) -> None:
        self._config = config
        self._backend = backend
        self._telemetry = telemetry
        self._health = health or BackendHealth()

    def handle(
        self,
        method: str,
        raw_path: str,
        body: bytes = b"",
        sink: ResponseSink | None = None,
    ) -> BridgeResponse:
        route = urlsplit(raw_path).path
        started = time.monotonic()
        metadata = RequestMetadata()
        outcome = "ok"
        try:
            if method == "GET" and route == "/healthz":
                health = self._health.snapshot()
                health_body: dict[str, Any] = {
                    "status": health.status,
                    "endpoint": self._config.endpoint,
                    "model": self._config.model,
                    "capabilities": ["vision"],
                }
                if health.error_code is not None:
                    health_body["error_code"] = health.error_code
                if health.error is not None:
                    health_body["error"] = health.error
                response = BridgeResponse(
                    200 if health.status == "ready" else 503,
                    health_body,
                )
            elif method == "GET" and route == "/api/version":
                response = BridgeResponse(200, {"version": f"independence-hailo-vlm-bridge/{BRIDGE_VERSION}"})
            elif method == "GET" and route == "/api/tags":
                response = BridgeResponse(
                    200,
                    {"models": [{"name": self._config.model, "model": self._config.model}]},
                )
            elif method == "POST" and route == "/api/show":
                request = self._json_request(body)
                self._require_served_model(request)
                health = self._health.snapshot()
                if health.status != "ready":
                    response = BridgeResponse(
                        503,
                        {
                            "error": "the local Hailo VLM bridge is not ready",
                            "code": health.error_code or "backend_degraded",
                            "health": health.status,
                        },
                    )
                else:
                    response = BridgeResponse(
                        200,
                        {
                            "capabilities": ["vision"],
                            "health": health.status,
                        },
                    )
            elif method == "POST" and route == "/api/chat":
                response, metadata = self._chat(body, sink)
            else:
                raise BridgeError(404, "the requested local API route is not available", "route_not_found")
        except BridgeError as error:
            outcome = error.outcome
            response = BridgeResponse(error.status, {"error": error.message})
        except _StreamFailed as failure:
            # Headers were already sent; the error went out as a final chunk.
            outcome = "hailo_inference_failed"
            self._health.mark_failure(failure.error)
            metadata = failure.metadata
            response = BridgeResponse(200, {}, streamed=True)
        except BackendInferenceError as error:
            outcome = "hailo_inference_failed"
            self._health.mark_failure(error)
            response = BridgeResponse(
                503,
                {
                    "error": error.public_message(),
                    "code": error.code,
                    "health": "degraded",
                },
            )
        except InvalidImageError:
            outcome = "invalid_image"
            response = BridgeResponse(400, {"error": "the local image could not be decoded"})
        except Exception:
            # Do not turn an unexpected model/runtime diagnostic into a prompt,
            # image, filesystem, or device disclosure over HTTP.
            outcome = "internal_error"
            response = BridgeResponse(500, {"error": "the local VLM bridge encountered an internal error"})

        self._record_call(route, method, response.status, outcome, metadata, started)
        return response

    def reject_before_reading(
        self,
        method: str,
        raw_path: str,
        status: int,
        message: str,
        outcome: str,
    ) -> BridgeResponse:
        route = urlsplit(raw_path).path
        self._record_call(route, method, status, outcome, RequestMetadata(), time.monotonic())
        return BridgeResponse(status, {"error": message})

    def _record_call(
        self,
        route: str,
        method: str,
        status: int,
        outcome: str,
        metadata: RequestMetadata,
        started: float,
    ) -> None:
        event: dict[str, Any] = {
            "event": "hailo_vlm.api_call",
            "timestamp_unix_ms": int(time.time() * 1000),
            "route": route,
            "method": method,
            "status": status,
            "outcome": outcome,
            "duration_ms": int((time.monotonic() - started) * 1000),
            "model": self._config.model,
            "message_count": metadata.message_count,
            "image_count": metadata.image_count,
            "image_bytes": metadata.image_bytes,
        }
        if metadata.generation_ms is not None:
            event["hailo_generation_ms"] = metadata.generation_ms
        if metadata.stream:
            event["stream"] = True
        if metadata.generation is not None:
            for key, value in metadata.generation.fields().items():
                event[f"generation_{key}"] = value
        self._telemetry.record(event)

    def _json_request(self, body: bytes) -> dict[str, Any]:
        if len(body) > MAX_REQUEST_BYTES:
            raise BridgeError(413, "the local request exceeds the configured size limit", "request_too_large")
        try:
            request = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise BridgeError(400, "the local request must be valid JSON", "invalid_json") from error
        if not isinstance(request, dict):
            raise BridgeError(400, "the local request must be a JSON object", "invalid_json_shape")
        return request

    def _require_served_model(self, request: dict[str, Any]) -> None:
        model = request.get("model")
        if not isinstance(model, str) or not model.strip():
            raise BridgeError(400, "the local request must name a model", "missing_model")
        if model != self._config.model:
            raise BridgeError(404, "the requested model is not served by this local bridge", "model_not_served")

    def _chat(
        self, body: bytes, sink: ResponseSink | None = None
    ) -> tuple[BridgeResponse, RequestMetadata]:
        request = self._json_request(body)
        self._require_served_model(request)
        stream = request.get("stream", False)
        if not isinstance(stream, bool):
            raise BridgeError(400, "stream must be true or false", "invalid_stream")
        if stream and sink is None:
            raise BridgeError(400, "streaming needs a live connection", "streaming_unavailable")

        messages = request.get("messages")
        if not isinstance(messages, list) or not messages:
            raise BridgeError(400, "the local request must contain messages", "missing_messages")
        if len(messages) > MAX_MESSAGES:
            raise BridgeError(413, "the local request has too many messages", "too_many_messages")

        if (
            len(messages) == 1
            and isinstance(messages[0], dict)
            and messages[0].get("role") == "system"
        ):
            return self._prime(messages[0])

        prompt: list[dict[str, Any]] = []
        image_bytes: list[bytes] = []
        text_bytes = 0
        for index, message in enumerate(messages):
            if not isinstance(message, dict):
                raise BridgeError(400, "each local message must be an object", "invalid_message")
            role = message.get("role")
            content = message.get("content")
            if role not in {"system", "user", "assistant"} or not isinstance(content, str):
                raise BridgeError(
                    400,
                    "local messages must use system, user, or assistant text roles",
                    "invalid_message",
                )
            try:
                text_bytes += len(content.encode("utf-8"))
            except UnicodeEncodeError as error:
                raise BridgeError(
                    400,
                    "the local message text must be valid UTF-8",
                    "invalid_message_text",
                ) from error
            if text_bytes > MAX_TEXT_BYTES:
                raise BridgeError(413, "the local message text exceeds the configured size limit", "text_too_large")

            images = message.get("images", [])
            if not isinstance(images, list):
                raise BridgeError(400, "local message images must be a list", "invalid_images")
            if index != len(messages) - 1 and images:
                raise BridgeError(
                    400,
                    "this local bridge accepts an image only on the current user message",
                    "history_images_not_supported",
                )
            if index == len(messages) - 1:
                if role != "user":
                    raise BridgeError(
                        400,
                        "the final local message must be a user message",
                        "invalid_final_message",
                    )
                image_bytes = self._decode_images(images)
                content_items: list[dict[str, str]] = []
                content_items.extend({"type": "image"} for _ in image_bytes)
                content_items.append({"type": "text", "text": content})
            else:
                content_items = [{"type": "text", "text": content}]
            prompt.append({"role": role, "content": content_items})

        metadata = RequestMetadata(
            message_count=len(messages),
            image_count=len(image_bytes),
            image_bytes=sum(len(image) for image in image_bytes),
            stream=stream,
        )
        if stream:
            return self._chat_streaming(prompt, image_bytes, metadata, sink)  # type: ignore[arg-type]

        def keep_going(_piece: str) -> bool:
            # Stop as soon as the client has hung up (for example, a new
            # question or Escape in Independence), so the Hailo is free.
            return sink is None or not sink.client_gone()

        generation_started = time.monotonic()
        result = self._backend.generate(prompt, image_bytes, keep_going)
        metadata.generation_ms = int((time.monotonic() - generation_started) * 1000)
        metadata.generation = result.stats
        _log_generation(result.stats)
        self._health.mark_ready()
        return (
            BridgeResponse(
                200,
                {
                    "model": self._config.model,
                    "message": {"role": "assistant", "content": result.text},
                    "done": True,
                    "done_reason": "length" if result.stats.hit_token_cap else "stop",
                    **result.stats.ollama_metrics(),
                },
            ),
            metadata,
        )

    def _prime(self, message: dict[str, Any]) -> tuple[BridgeResponse, RequestMetadata]:
        """A request holding only a system message prepares the system-prompt
        snapshot without answering anything (Ollama answers an empty chat
        with ``done_reason: load`` in the same spirit)."""

        content = message.get("content")
        if not isinstance(content, str) or not content.strip():
            raise BridgeError(400, "the system message must be non-empty text", "invalid_message")
        if len(content.encode("utf-8")) > MAX_TEXT_BYTES:
            raise BridgeError(413, "the local message text exceeds the configured size limit", "text_too_large")
        cached = self._backend.prime_system_prompt(content)
        print(
            "Hailo VLM system prompt "
            + ("loaded and kept for every request" if cached else "cache is off; it is read every request"),
            file=sys.stderr,
            flush=True,
        )
        return (
            BridgeResponse(
                200,
                {
                    "model": self._config.model,
                    "message": {"role": "assistant", "content": ""},
                    "done": True,
                    "done_reason": "load",
                    "system_prompt_cached": cached,
                },
            ),
            RequestMetadata(message_count=1),
        )

    def _chat_streaming(
        self,
        prompt: list[dict[str, Any]],
        image_bytes: list[bytes],
        metadata: RequestMetadata,
        sink: ResponseSink,
    ) -> tuple[BridgeResponse, RequestMetadata]:
        """Write Ollama-style NDJSON chunks as tokens arrive.

        A failed write means the client went away; generation stops at once.
        """

        if not sink.begin_stream():
            metadata.generation = GenerationStats(outcome="aborted")
            return BridgeResponse(200, {}, streamed=True), metadata

        def forward(piece: str) -> bool:
            return sink.send_chunk(
                {
                    "model": self._config.model,
                    "message": {"role": "assistant", "content": piece},
                    "done": False,
                }
            )

        generation_started = time.monotonic()
        try:
            result = self._backend.generate(prompt, image_bytes, forward)
        except BackendInferenceError as error:
            sink.send_chunk(
                {
                    "error": error.public_message(),
                    "code": error.code,
                    "health": "degraded",
                    "done": True,
                }
            )
            metadata.generation_ms = int((time.monotonic() - generation_started) * 1000)
            raise _StreamFailed(error, metadata) from error
        metadata.generation_ms = int((time.monotonic() - generation_started) * 1000)
        metadata.generation = result.stats
        _log_generation(result.stats)
        if result.stats.outcome == "completed":
            self._health.mark_ready()
            sink.send_chunk(
                {
                    "model": self._config.model,
                    "message": {"role": "assistant", "content": ""},
                    "done": True,
                    "done_reason": "length" if result.stats.hit_token_cap else "stop",
                    **result.stats.ollama_metrics(),
                }
            )
        return BridgeResponse(200, {}, streamed=True), metadata

    def _decode_images(self, images: list[Any]) -> list[bytes]:
        if len(images) > MAX_IMAGES:
            raise BridgeError(
                413,
                f"this local Hailo VLM bridge accepts at most {MAX_IMAGES} images per request",
                "too_many_images",
            )

        decoded_images: list[bytes] = []
        for encoded in images:
            if not isinstance(encoded, str):
                raise BridgeError(400, "the local image must be base64 text", "invalid_image")
            try:
                image = base64.b64decode(encoded.encode("ascii"), validate=True)
            except (UnicodeEncodeError, binascii.Error) as error:
                raise BridgeError(400, "the local image must be valid base64", "invalid_image") from error
            if not image:
                raise BridgeError(400, "the local image must not be empty", "invalid_image")
            if len(image) > MAX_IMAGE_BYTES:
                raise BridgeError(413, "the local image exceeds the configured size limit", "image_too_large")
            decoded_images.append(image)
        return decoded_images


class _StreamFailed(Exception):
    """A backend failure after streaming began (headers already sent)."""

    def __init__(self, error: BackendInferenceError, metadata: RequestMetadata) -> None:
        super().__init__(error.message)
        self.error = error
        self.metadata = metadata


def _log_generation(stats: GenerationStats) -> None:
    # Counts and timings only: this line goes to the launcher's bridge log.
    print(stats.log_line(), file=sys.stderr, flush=True)


class _ConnectionSink:
    """A `ResponseSink` over one HTTP connection handled by the bridge."""

    def __init__(self, handler: "BridgeRequestHandler") -> None:
        self._handler = handler
        self._open = True

    def begin_stream(self) -> bool:
        try:
            self._handler.send_response(200)
            self._handler.send_header("Content-Type", "application/x-ndjson")
            self._handler.send_header("Cache-Control", "no-store")
            # The body ends when the connection closes (HTTP/1.0 style).
            self._handler.send_header("Connection", "close")
            self._handler.end_headers()
            self._handler.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            self._open = False
        return self._open

    def send_chunk(self, chunk: dict[str, Any]) -> bool:
        if not self._open:
            return False
        try:
            line = json.dumps(chunk, separators=(",", ":")).encode("utf-8") + b"\n"
            self._handler.wfile.write(line)
            self._handler.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            self._open = False
            print(
                "Hailo VLM client disconnected; generation stopped",
                file=sys.stderr,
                flush=True,
            )
        return self._open

    def client_gone(self) -> bool:
        """True once the client has closed its side of the connection.

        The request body has already been read, so any readable end-of-file
        means the client hung up.
        """

        if not self._open:
            return True
        try:
            data = self._handler.connection.recv(1, socket.MSG_PEEK | socket.MSG_DONTWAIT)
        except (BlockingIOError, InterruptedError):
            return False
        except OSError:
            self._open = False
            return True
        if data == b"":
            self._open = False
            print(
                "Hailo VLM client disconnected; generation stopped",
                file=sys.stderr,
                flush=True,
            )
            return True
        return False


class BridgeHttpServer(ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, config: BridgeConfig, application: BridgeApplication) -> None:
        if ":" in config.host:
            self.address_family = socket.AF_INET6
        super().__init__((config.host, config.port), BridgeRequestHandler)
        self.application = application


class BridgeRequestHandler(BaseHTTPRequestHandler):
    server_version = "IndependenceHailoVlmBridge"
    sys_version = ""

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        self._respond(self.server.application.handle("GET", self.path))

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        length_text = self.headers.get("Content-Length")
        if length_text is None:
            self._respond(
                self.server.application.reject_before_reading(
                    "POST",
                    self.path,
                    411,
                    "the local request must include Content-Length",
                    "content_length_required",
                )
            )
            return
        try:
            length = int(length_text)
        except ValueError:
            self._respond(
                self.server.application.reject_before_reading(
                    "POST",
                    self.path,
                    400,
                    "the local request has an invalid Content-Length",
                    "invalid_content_length",
                )
            )
            return
        if length < 0 or length > MAX_REQUEST_BYTES:
            self._respond(
                self.server.application.reject_before_reading(
                    "POST",
                    self.path,
                    413,
                    "the local request exceeds the configured size limit",
                    "request_too_large",
                )
            )
            return
        body = self.rfile.read(length)
        if len(body) != length:
            self._respond(
                self.server.application.reject_before_reading(
                    "POST",
                    self.path,
                    400,
                    "the local request body was incomplete",
                    "incomplete_body",
                )
            )
            return
        self._respond(
            self.server.application.handle("POST", self.path, body, _ConnectionSink(self))
        )

    def _respond(self, response: BridgeResponse) -> None:
        if response.streamed:
            return
        try:
            body = json.dumps(response.body, separators=(",", ":")).encode("utf-8")
            self.send_response(response.status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            # Cancellation or a client timeout can close the loopback socket
            # while Hailo is still generating. This is not a backend failure.
            print(
                "Hailo VLM client disconnected before the response was sent",
                file=sys.stderr,
                flush=True,
            )

    def log_message(self, _format: str, *_args: object) -> None:
        # BaseHTTPRequestHandler's default access log can contain request paths
        # and error strings. The bridge emits only SafeTelemetry metadata.
        return


def _loopback_host(value: str) -> str:
    try:
        address = ipaddress.ip_address(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("host must be a numeric loopback address") from error
    if str(address) not in {"127.0.0.1", "::1"}:
        raise argparse.ArgumentTypeError(
            "the bridge may bind only to 127.0.0.1 or ::1 so Independence can reach it"
        )
    return str(address)


def _port(value: str) -> int:
    try:
        port = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("port must be an integer") from error
    if not 1 <= port <= 65535:
        raise argparse.ArgumentTypeError("port must be between 1 and 65535")
    return port


def _positive_integer(value: str) -> int:
    try:
        number = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("value must be an integer") from error
    if number < 1:
        raise argparse.ArgumentTypeError("value must be positive")
    return number


def _temperature(value: str) -> float:
    try:
        number = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("temperature must be a number") from error
    if not 0.0 < number <= 2.0:
        raise argparse.ArgumentTypeError("temperature must be greater than 0 and at most 2")
    return number


def _environment_port() -> int:
    value = os.environ.get("HAILO_VLM_PORT", str(DEFAULT_PORT))
    try:
        return _port(value)
    except argparse.ArgumentTypeError as error:
        raise SystemExit(f"HAILO_VLM_PORT: {error}") from error


def parse_arguments(argv: list[str] | None = None) -> BridgeConfig:
    parser = argparse.ArgumentParser(
        description="Serve one local Hailo Qwen3-VL model through Independence's Ollama-compatible API."
    )
    parser.add_argument(
        "--host",
        type=_loopback_host,
        default=os.environ.get("HAILO_VLM_HOST", DEFAULT_HOST),
        help="numeric loopback address only (default: 127.0.0.1)",
    )
    parser.add_argument(
        "--port",
        type=_port,
        default=_environment_port(),
        help="loopback HTTP port (default: HAILO_VLM_PORT or 11435)",
    )
    parser.add_argument(
        "--model",
        default=os.environ.get("HAILO_VLM_MODEL", DEFAULT_MODEL),
        help="local API model alias (default: qwen3-vl:2b-instruct)",
    )
    parser.add_argument(
        "--hef-path",
        default=os.environ.get("HAILO_VLM_HEF"),
        help="downloaded Hailo VLM .hef path (required; no automatic download)",
    )
    parser.add_argument(
        "--telemetry-jsonl",
        default=os.environ.get("HAILO_VLM_TELEMETRY_JSONL"),
        help="optional existing-directory JSONL path for safe API metadata",
    )
    parser.add_argument(
        "--max-generated-tokens",
        type=_positive_integer,
        default=DEFAULT_MAX_GENERATED_TOKENS,
        help="maximum Hailo generation tokens (default: 200)",
    )
    parser.add_argument(
        "--temperature",
        type=_temperature,
        default=DEFAULT_TEMPERATURE,
        help="sampling temperature from 0 to 2 (default: 0.1)",
    )
    parser.add_argument(
        "--seed",
        type=int,
        default=DEFAULT_SEED,
        help="sampling seed (default: 42)",
    )
    parser.add_argument(
        "--no-system-prompt-cache",
        dest="system_prompt_cache",
        action="store_false",
        help="read the system prompt in full on every request instead of reusing a snapshot",
    )
    arguments = parser.parse_args(argv)
    if not arguments.model.strip():
        parser.error("--model must not be empty")
    if not arguments.hef_path:
        parser.error("--hef-path or HAILO_VLM_HEF is required; use the Pi Hailo runbook to download it explicitly")
    return BridgeConfig(
        host=arguments.host,
        port=arguments.port,
        model=arguments.model,
        hef_path=Path(arguments.hef_path).expanduser(),
        max_generated_tokens=arguments.max_generated_tokens,
        temperature=arguments.temperature,
        seed=arguments.seed,
        telemetry_jsonl=arguments.telemetry_jsonl,
        system_prompt_cache=arguments.system_prompt_cache,
    )


def _stop_on_terminate(_signum: int, _frame: object) -> None:
    """Turn SIGTERM into the same orderly shutdown as Ctrl+C.

    A launcher that starts the bridge in the background cannot use Ctrl+C:
    background processes started by a script ignore SIGINT. It sends SIGTERM
    instead, and this makes the `finally` block below release the Hailo
    device rather than leaving that to process teardown.
    """
    raise KeyboardInterrupt


def install_termination_handler() -> None:
    signal.signal(signal.SIGTERM, _stop_on_terminate)


def main(argv: list[str] | None = None) -> int:
    config = parse_arguments(argv)
    try:
        telemetry = SafeTelemetry(config.telemetry_jsonl)
    except ValueError as error:
        print(f"Hailo VLM bridge did not start: {error}.", file=sys.stderr)
        return 2

    try:
        backend = HailoVlmBackend(
            config.hef_path,
            config.max_generated_tokens,
            config.temperature,
            config.seed,
            config.system_prompt_cache,
        )
    except BackendStartupError as error:
        print(f"Hailo VLM bridge did not start: {error}", file=sys.stderr)
        return 2

    application = BridgeApplication(config, backend, telemetry)
    try:
        server = BridgeHttpServer(config, application)
    except OSError:
        backend.close()
        print(
            f"Hailo VLM bridge did not start: could not bind {config.endpoint}.",
            file=sys.stderr,
        )
        return 2

    telemetry.record(
        {
            "event": "hailo_vlm.bridge_started",
            "timestamp_unix_ms": int(time.time() * 1000),
            "endpoint": config.endpoint,
            "model": config.model,
            "capabilities": ["vision"],
        }
    )
    print(f"Hailo VLM bridge listening on {config.endpoint}")
    print(
        f"Health: {config.endpoint}/healthz | status: ready | model: {config.model} | capabilities: vision"
    )
    install_termination_handler()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("Hailo VLM bridge stopping")
    finally:
        server.server_close()
        backend.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
