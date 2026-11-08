#!/usr/bin/env python3
"""Serve a local Hailo VLM through the small Ollama contract Independence uses.

This program intentionally has no web framework dependency.  Hailo Apps already
provides the hardware/runtime dependencies; the standard-library HTTP server is
enough for one loopback-only local client.  It accepts the existing
``/api/show`` and non-streaming ``/api/chat`` shapes, so the Rust application
keeps owning capability checks, conversation history, camera consent, and turn
authorization.

The bridge never writes prompts, image bytes, base64 data, or model replies to
its own logs or optional JSONL telemetry.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import ipaddress
import json
import os
import socket
import sys
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Protocol
from urllib.parse import urlsplit


BRIDGE_VERSION = "0.2.0"
DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 11435
DEFAULT_MODEL = "qwen2-vl:2b"
DEFAULT_MAX_GENERATED_TOKENS = 200
MAX_REQUEST_BYTES = 14 * 1024 * 1024
MAX_IMAGE_BYTES = 10 * 1024 * 1024
MAX_IMAGES = 4
MAX_IMAGE_PIXELS = 10_000_000
MAX_MESSAGES = 16
MAX_TEXT_BYTES = 64 * 1024
VLM_IMAGE_SIZE = 336


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
    """A concise, safe diagnosis for one failed Hailo generation."""


class InvalidImageError(Exception):
    """A client input error discovered while decoding image bytes locally."""


@dataclass(frozen=True)
class BridgeConfig:
    host: str
    port: int
    model: str
    hef_path: Path
    max_generated_tokens: int
    telemetry_jsonl: str | None = None

    @property
    def endpoint(self) -> str:
        if ":" in self.host:
            return f"http://[{self.host}]:{self.port}"
        return f"http://{self.host}:{self.port}"


@dataclass(frozen=True)
class BridgeResponse:
    status: int
    body: dict[str, Any]


@dataclass
class RequestMetadata:
    message_count: int = 0
    image_count: int = 0
    image_bytes: int = 0
    generation_ms: int | None = None


class VlmBackend(Protocol):
    """The small bridge-owned contract that makes API tests hardware-free."""

    def generate(self, prompt: list[dict[str, Any]], images: list[bytes]) -> str: ...

    def close(self) -> None: ...


class HailoVlmBackend:
    """A one-model, serialized Hailo Qwen2-VL runtime.

    Hailo's VLM context is cleared after every request.  Independence supplies
    accepted text history again in the next request, rather than leaving private
    state resident in the accelerator runtime.
    """

    def __init__(self, hef_path: Path, max_generated_tokens: int) -> None:
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
        try:
            params = VDevice.create_params()
            # Hailo's shared VDevice group lets this local VLM coexist with
            # ordinary vision pipelines. It does not make two GenAI models
            # concurrent; this bridge owns exactly one VLM.
            params.group_id = SHARED_VDEVICE_GROUP_ID
            self._vdevice = VDevice(params)
            self._vlm = VLM(self._vdevice, str(hef_path))
            self._max_generated_tokens = max_generated_tokens
        except Exception as error:  # Hailo exceptions are not safe HTTP output.
            self.close()
            raise BackendStartupError(
                "The Hailo VLM could not initialize. Check the HAT, HailoRT version, "
                "and downloaded VLM model."
            ) from error

    def generate(self, prompt: list[dict[str, Any]], images: list[bytes]) -> str:
        # Hailo validates that each image placeholder in ``prompt`` has a
        # matching frame. Preserve Independence's capture order in both lists;
        # do not make a collage or discard a later photo.
        frames = [self._prepare_image(image_bytes) for image_bytes in images]

        with self._generation_lock:
            try:
                # Hailo's VLM generator is streamed internally, then collected
                # before the response. The HTTP contract remains deliberately
                # non-streaming, matching Independence's existing client.
                response_parts: list[str] = []
                with self._vlm.generate(
                    prompt=prompt,
                    frames=frames,
                    temperature=0.1,
                    seed=42,
                    max_generated_tokens=self._max_generated_tokens,
                ) as generation:
                    for chunk in generation:
                        if chunk != "<|im_end|>":
                            response_parts.append(str(chunk))
            except Exception as error:
                raise BackendInferenceError(
                    "The Hailo VLM could not complete this local request."
                ) from error
            finally:
                try:
                    self._vlm.clear_context()
                except Exception:
                    # A failed cleanup must not replace the safe inference
                    # error or expose a device/runtime diagnostic to a caller.
                    pass

        text = "".join(response_parts).replace("<|im_end|>", "").strip()
        if not text:
            raise BackendInferenceError("The Hailo VLM returned no usable text.")
        return text

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

        # Hailo's Qwen2-VL example uses a central crop at 336x336 RGB. Keep
        # that model-specific adaptation here, outside Independence's generic
        # camera and request path.
        scale = max(VLM_IMAGE_SIZE / width, VLM_IMAGE_SIZE / height)
        resized_width = max(VLM_IMAGE_SIZE, round(width * scale))
        resized_height = max(VLM_IMAGE_SIZE, round(height * scale))
        resized = self._cv2.resize(
            image,
            (resized_width, resized_height),
            interpolation=self._cv2.INTER_LINEAR,
        )
        left = (resized_width - VLM_IMAGE_SIZE) // 2
        top = (resized_height - VLM_IMAGE_SIZE) // 2
        cropped = resized[top : top + VLM_IMAGE_SIZE, left : left + VLM_IMAGE_SIZE]
        return self._cv2.cvtColor(cropped, self._cv2.COLOR_BGR2RGB).astype(self._np.uint8)

    def close(self) -> None:
        with getattr(self, "_generation_lock", threading.Lock()):
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
    ) -> None:
        self._config = config
        self._backend = backend
        self._telemetry = telemetry

    def handle(self, method: str, raw_path: str, body: bytes = b"") -> BridgeResponse:
        route = urlsplit(raw_path).path
        started = time.monotonic()
        metadata = RequestMetadata()
        outcome = "ok"
        try:
            if method == "GET" and route == "/healthz":
                response = BridgeResponse(
                    200,
                    {
                        "status": "ready",
                        "endpoint": self._config.endpoint,
                        "model": self._config.model,
                        "capabilities": ["vision"],
                    },
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
                response = BridgeResponse(200, {"capabilities": ["vision"]})
            elif method == "POST" and route == "/api/chat":
                response, metadata = self._chat(body)
            else:
                raise BridgeError(404, "the requested local API route is not available", "route_not_found")
        except BridgeError as error:
            outcome = error.outcome
            response = BridgeResponse(error.status, {"error": error.message})
        except BackendInferenceError:
            outcome = "hailo_inference_failed"
            response = BridgeResponse(
                503,
                {"error": "the local Hailo VLM could not complete the request"},
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

    def _chat(self, body: bytes) -> tuple[BridgeResponse, RequestMetadata]:
        request = self._json_request(body)
        self._require_served_model(request)
        if request.get("stream", False) is not False:
            raise BridgeError(
                400,
                "this local bridge supports only stream=false requests",
                "streaming_not_supported",
            )

        messages = request.get("messages")
        if not isinstance(messages, list) or not messages:
            raise BridgeError(400, "the local request must contain messages", "missing_messages")
        if len(messages) > MAX_MESSAGES:
            raise BridgeError(413, "the local request has too many messages", "too_many_messages")

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
        )
        generation_started = time.monotonic()
        answer = self._backend.generate(prompt, image_bytes)
        metadata.generation_ms = int((time.monotonic() - generation_started) * 1000)
        return (
            BridgeResponse(
                200,
                {
                    "model": self._config.model,
                    "message": {"role": "assistant", "content": answer},
                    "done": True,
                    "done_reason": "stop",
                },
            ),
            metadata,
        )

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
        self._respond(self.server.application.handle("POST", self.path, body))

    def _respond(self, response: BridgeResponse) -> None:
        body = json.dumps(response.body, separators=(",", ":")).encode("utf-8")
        self.send_response(response.status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

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


def _environment_port() -> int:
    value = os.environ.get("HAILO_VLM_PORT", str(DEFAULT_PORT))
    try:
        return _port(value)
    except argparse.ArgumentTypeError as error:
        raise SystemExit(f"HAILO_VLM_PORT: {error}") from error


def parse_arguments(argv: list[str] | None = None) -> BridgeConfig:
    parser = argparse.ArgumentParser(
        description="Serve one local Hailo Qwen2-VL model through Independence's Ollama-compatible API."
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
        help="local API model alias (default: qwen2-vl:2b)",
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
        telemetry_jsonl=arguments.telemetry_jsonl,
    )


def main(argv: list[str] | None = None) -> int:
    config = parse_arguments(argv)
    try:
        telemetry = SafeTelemetry(config.telemetry_jsonl)
    except ValueError as error:
        print(f"Hailo VLM bridge did not start: {error}.", file=sys.stderr)
        return 2

    try:
        backend = HailoVlmBackend(config.hef_path, config.max_generated_tokens)
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
    print(f"Health: {config.endpoint}/healthz | model: {config.model} | capabilities: vision")
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
