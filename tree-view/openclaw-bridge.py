#!/usr/bin/env python3
"""openclaw-bridge — A2A HTTP front-end for an OpenClaw agent box.

Design_Document.md §6. Thin HTTP server (stdlib only) that presents the
standard A2A protocol and forwards to the local OpenClaw gateway's
WebSocket. One per agent box, on :8080.

This v1 implements:
  - GET /.well-known/agent.json  → Agent Card
  - GET /state                   → aggregated state (tmux + openclaw sessions)
                                   This is the collector's pull target.
  - GET /health                  → liveness

A2A JSON-RPC methods (POST /) are stubbed — they return a clear
"not yet implemented" response. Full translation to the OpenClaw gateway
WS (tasks/sendSubscribe → SSE, etc.) is Phase 5 of the design. The /state
endpoint is enough for the collector to populate the tree store.

Usage:
    openclaw-bridge.py             # daemon on :8080
    openclaw-bridge.py --port 8081
    openclaw-bridge.py --once      # print /state JSON and exit (testing)

No third-party deps — stdlib only.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

DEFAULT_PORT = 8080
GATEWAY_URL = "ws://localhost:18789"


def run(cmd: list[str], timeout: float = 5.0) -> tuple[int, str, str]:
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout, r.stderr
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as e:
        return 1, "", str(e)


def tmux_sessions() -> list[str]:
    rc, out, _ = run(["tmux", "list-sessions", "-F", "#{session_name}"])
    if rc != 0:
        return []
    return [line.strip() for line in out.splitlines() if line.strip()]


def openclaw_sessions() -> list[str]:
    rc, out, _ = run(["openclaw", "sessions", "list", "--json"])
    if rc != 0:
        return []
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        return []
    if isinstance(data, list):
        return [s.get("id", s.get("name", "")) for s in data if isinstance(s, dict)]
    return []


def agent_state() -> str:
    """Best-effort: idle if no tmux sessions, else working."""
    return "idle" if not tmux_sessions() else "working"


def agent_card(host: str, port: int) -> dict[str, Any]:
    name = os.environ.get("ORCHESTRA_AGENT_NAME", "main-box")
    return {
        "name": name,
        "version": "0.1.0",
        "description": "orchestra agent (openclaw-backed)",
        "capabilities": {"streaming": True, "pushNotifications": False},
        "defaultInputModes": ["text"],
        "defaultOutputModes": ["text", "json"],
        "authentication": {"schemes": ["bearer"]},
        "url": f"http://{host}:{port}/",
    }


def state_payload() -> dict[str, Any]:
    """Aggregated state for the collector's GET /state pull."""
    return {
        "id": os.environ.get("ORCHESTRA_AGENT_ID", "agent-main-box"),
        "state": agent_state(),
        "config": {},
        "openclaw_sessions": openclaw_sessions(),
        "pi_sessions": tmux_sessions(),
        "active_task_ids": [],
    }


def make_handler(host: str, port: int):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):
            pass

        def _send(self, code: int, body: Any, ctype: str = "application/json") -> None:
            data = json.dumps(body).encode() if not isinstance(body, bytes) else body
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.path == "/.well-known/agent.json":
                self._send(200, agent_card(host, port))
            elif self.path == "/state":
                self._send(200, state_payload())
            elif self.path == "/health":
                self._send(200, {"ok": True})
            else:
                self._send(404, {"error": "not found"})

        def do_POST(self):
            # A2A JSON-RPC 2.0 over POST /.
            length = int(self.headers.get("Content-Length", 0))
            raw = self.rfile.read(length) if length else b"{}"
            try:
                req = json.loads(raw)
            except json.JSONDecodeError:
                self._send(400, {"jsonrpc": "2.0", "error": {"code": -32700, "message": "parse error"}})
                return
            method = req.get("method", "")
            rid = req.get("id")
            # v1 stub: all tasks/* and message/* methods are not yet wired
            # to the OpenClaw gateway WS. Return a clear error so callers
            # know to wait for Phase 5.
            if method in ("message/send", "tasks/send", "tasks/get", "tasks/cancel", "tasks/sendSubscribe"):
                self._send(
                    200,
                    {
                        "jsonrpc": "2.0",
                        "id": rid,
                        "error": {
                            "code": -32601,
                            "message": f"{method} not implemented in v1 bridge (Phase 5)",
                        },
                    },
                )
            else:
                self._send(
                    200,
                    {"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": f"unknown method: {method}"}},
                )

    return Handler


def main() -> int:
    ap = argparse.ArgumentParser(description="openclaw-bridge: A2A HTTP front-end")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--host", default="0.0.0.0")
    ap.add_argument("--once", action="store_true", help="print /state JSON and exit")
    args = ap.parse_args()

    if args.once:
        print(json.dumps(state_payload(), indent=2))
        return 0

    server = ThreadingHTTPServer((args.host, args.port), make_handler(args.host, args.port))
    print(f"openclaw-bridge on :{args.port} (gateway: {GATEWAY_URL})")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
