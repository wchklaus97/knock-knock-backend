#!/usr/bin/env python3
"""Deterministic loopback provider boundary for the lifecycle smoke.

The mock never contacts a vendor, persists no credentials, and logs only the
HTTP method and path. It is deliberately bound to loopback unless explicitly
overridden for a separately controlled test environment.
"""

from __future__ import annotations

import argparse
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

MAX_BODY_BYTES = 64 * 1024


class ProviderState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.next_id = 1
        self.resources: dict[str, dict[str, Any]] = {}
        self.cancel_attempts: dict[str, int] = {}

    def resource(self, kind: str, key: str, command_id: str) -> dict[str, Any]:
        with self.lock:
            existing = self.resources.get(key)
            if existing is not None:
                return dict(existing)
            prefix = "rem" if kind == "reminder" else "msg"
            value = {
                "provider_id": f"mock-{prefix}-{self.next_id}",
                "kind": kind,
                "command_id": command_id,
            }
            self.next_id += 1
            self.resources[key] = value
            return dict(value)

    def resource_for(self, key: str) -> dict[str, Any] | None:
        with self.lock:
            resource = self.resources.get(key)
            return dict(resource) if resource is not None else None

    def fail_delivery_once(self, key: str) -> bool:
        with self.lock:
            resource = self.resources.get(key)
            if resource is None or resource.get("failed_once") is True:
                return False
            resource["failed_once"] = True
            return True

    def cancel_attempt(self, idempotency_key: str) -> int:
        with self.lock:
            attempt = self.cancel_attempts.get(idempotency_key, 0) + 1
            self.cancel_attempts[idempotency_key] = attempt
            return attempt


class Handler(BaseHTTPRequestHandler):
    server_version = "KnockKnockProviderMock/1"

    @property
    def state(self) -> ProviderState:
        return self.server.provider_state  # type: ignore[attr-defined]

    @property
    def expected_token(self) -> str:
        return self.server.expected_token  # type: ignore[attr-defined]

    def log_message(self, format: str, *args: object) -> None:
        del format, args
        sys.stderr.write(f"[provider-mock] {self.command} {self.path}\n")

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            self.send_json(200, {"ok": True, "provider": "mock"})
            return
        self.send_json(404, {"error": "not_found"})

    def do_POST(self) -> None:  # noqa: N802
        if not self.authorized():
            self.send_json(401, {"error": "unauthorized"})
            return
        body = self.read_json()
        if body is None:
            return
        key = self.headers.get("x-idempotency-key", "").strip()
        intent = self.headers.get("x-knock-knock-intent", "").strip()
        command_id = str(body.get("command_id", "")).strip()
        if not key or not command_id:
            self.send_json(400, {"error": "missing_idempotency_or_command"})
            return

        if self.path == "/reminders/deliver" and intent == "create_reminder":
            self.reminder_delivery(key, command_id)
        elif self.path == "/reminders/status" and intent == "create_reminder":
            self.reminder_status(key)
        elif self.path == "/reminders/cancel" and intent == "create_reminder":
            self.reminder_cancel(body, key)
        elif self.path == "/messages/deliver" and intent == "send_message":
            self.message_delivery(key, command_id)
        elif self.path == "/messages/status" and intent == "send_message":
            self.message_status(key)
        else:
            self.send_json(404, {"error": "unknown_provider_operation"})

    def authorized(self) -> bool:
        return self.headers.get("authorization", "") == f"Bearer {self.expected_token}"

    def read_json(self) -> dict[str, Any] | None:
        try:
            length = int(self.headers.get("content-length", "0"))
            if length < 0 or length > MAX_BODY_BYTES:
                self.send_json(413, {"error": "request_too_large"})
                return None
            raw = self.rfile.read(length)
            value = json.loads(raw.decode("utf-8"))
            if not isinstance(value, dict):
                raise ValueError("object required")
            return value
        except (ValueError, TypeError, json.JSONDecodeError):
            self.send_json(400, {"error": "invalid_json"})
            return None

    def reminder_delivery(self, key: str, command_id: str) -> None:
        resource = self.state.resource("reminder", key, command_id)
        if command_id.startswith("cmd-status-reconcile-") and self.state.fail_delivery_once(key):
            self.send_json(503, {"error": "simulated_delivery_timeout"})
            return
        self.send_json(200, {"provider_id": resource["provider_id"], "state": "scheduled"})

    def reminder_status(self, key: str) -> None:
        resource = self.state.resource_for(key)
        if resource is None:
            self.send_json(404, {"error": "unknown_resource"})
            return
        self.send_json(200, {"provider_id": resource["provider_id"], "state": "scheduled"})

    def reminder_cancel(self, body: dict[str, Any], key: str) -> None:
        command_id = str(body.get("command_id", "")).strip()
        provider_id = str(body.get("provider_id", "")).strip()
        attempt = self.state.cancel_attempt(key)
        if command_id.startswith("cmd-cancel-missing-id-"):
            self.send_json(200, {"state": "cancelled"})
            return
        if command_id.startswith("cmd-cancel-mismatch-"):
            self.send_json(200, {"provider_id": "mock-rem-not-the-requested-resource", "state": "cancelled"})
            return
        if command_id.startswith("cmd-cancel-reconcile-") and attempt == 1:
            self.send_json(200, {"provider_id": provider_id, "state": "pending"})
            return
        self.send_json(200, {"provider_id": provider_id, "state": "cancelled"})

    def message_delivery(self, key: str, command_id: str) -> None:
        resource = self.state.resource("message", key, command_id)
        if command_id.startswith("cmd-message-missing-id-"):
            self.send_json(200, {"state": "delivered"})
            return
        self.send_json(200, {"provider_id": resource["provider_id"], "state": "accepted"})

    def message_status(self, key: str) -> None:
        resource = self.state.resource_for(key)
        if resource is None:
            self.send_json(404, {"error": "unknown_resource"})
            return
        self.send_json(200, {"provider_id": resource["provider_id"], "delivery_state": "delivered"})

    def send_json(self, status: int, value: dict[str, Any]) -> None:
        encoded = json.dumps(value, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address: tuple[str, int], token: str) -> None:
        super().__init__(address, Handler)
        self.provider_state = ProviderState()
        self.expected_token = token


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8889)
    parser.add_argument("--token", required=True)
    parser.add_argument("--allow-non-loopback", action="store_true")
    args = parser.parse_args()
    if args.host not in {"127.0.0.1", "localhost", "::1"} and not args.allow_non_loopback:
        parser.error("refusing non-loopback bind without --allow-non-loopback")
    server = Server((args.host, args.port), args.token)
    print(f"provider mock listening on http://{args.host}:{args.port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
