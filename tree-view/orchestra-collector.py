#!/usr/bin/env python3
"""orchestra-collector — State Collector daemon for the Tree View.

Design_Document.md §4-§5, §10. Long-lived process on main-box that:

  1. Scrapes main-box's own state (tmux sessions + OpenClaw sessions) and
     writes it to the tree store at ~/.orchestra/tree/.
  2. Pulls GET /state from every known sub-agent's bridge every 30s.
  3. Runs `sky status` reconciliation every 60s (orphan/dead cluster handling).
  4. Serves an HTTP API on :7777 for TUI commands (rename, edge registration,
     node registration, force-refresh).

The TUI is a pure reader of the store. The collector is the only writer.

Usage:
    orchestra-collector.py                 # daemon (server + pull loop)
    orchestra-collector.py --once          # one pull cycle, then exit
    orchestra-collector.py --store-dir DIR # override store location
    orchestra-collector.py --port 7777     # override HTTP port

No third-party deps — stdlib only (http.server, subprocess, json). This
runs on main-box's miniconda3 Python without `pip install`.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

# --- Constants (mirror tui/src/tree_store.rs) ---

DEFAULT_STORE_DIR = os.path.expanduser("~/.orchestra/tree")
DEFAULT_PORT = 7777
ROOT_AGENT_ID = "agent-main-box"
SCHEMA_VERSION = 1
PULL_INTERVAL_S = 30
SKY_RECONCILE_INTERVAL_S = 60
STALE_THRESHOLD_S = 90

AGENT_PREFIX = "agent-"  # SkyPilot cluster naming convention (Design §8)


# --- Tree store (disk) ---


def unix_now() -> int:
    return int(time.time())


def store_path(store_dir: str, *parts: str) -> Path:
    return Path(store_dir).joinpath(*parts)


def load_index(store_dir: str) -> dict[str, Any] | None:
    p = store_path(store_dir, "index.json")
    if not p.exists():
        return None
    try:
        return json.loads(p.read_text())
    except (json.JSONDecodeError, OSError):
        return None


def load_node(store_dir: str, node_id: str) -> dict[str, Any] | None:
    p = store_path(store_dir, "nodes", f"{node_id}.json")
    if not p.exists():
        return None
    try:
        return json.loads(p.read_text())
    except (json.JSONDecodeError, OSError):
        return None


def write_node(store_dir: str, node: dict[str, Any]) -> None:
    nodes_dir = store_path(store_dir, "nodes")
    nodes_dir.mkdir(parents=True, exist_ok=True)
    p = nodes_dir / f"{node['id']}.json"
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(node, indent=2, sort_keys=False))
    tmp.replace(p)  # atomic


def write_index(store_dir: str, root_id: str, updated_at: int) -> None:
    Path(store_dir).mkdir(parents=True, exist_ok=True)
    idx = {"root_id": root_id, "version": SCHEMA_VERSION, "updated_at": updated_at}
    p = store_path(store_dir, "index.json")
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(idx, indent=2))
    tmp.replace(p)


def load_all_node_ids(store_dir: str) -> list[str]:
    nodes_dir = store_path(store_dir, "nodes")
    if not nodes_dir.exists():
        return []
    return sorted(p.stem for p in nodes_dir.glob("*.json"))


# --- Node construction ---


def root_node() -> dict[str, Any]:
    """The Main Agent node (main-box)."""
    now = unix_now()
    return {
        "id": ROOT_AGENT_ID,
        "kind": "agent",
        "parent_id": None,
        "name": "main-box",
        "display_name": None,
        "sky_cluster": ROOT_AGENT_ID,
        "tmux_session": None,
        "box_host": ROOT_AGENT_ID,
        "bridge_url": "http://localhost:8080/",
        "gateway_url": "ws://localhost:18789",
        "state": "idle",
        "config": {},
        "openclaw_sessions": [],
        "pi_sessions": [],
        "created_at": now,
        "last_pulled": now,
        "children": [],
    }


def session_node(
    name: str, parent_id: str, state: str = "working", tmux_kind: str = "pi"
) -> dict[str, Any]:
    """A session node (tmux session on a parent agent's box)."""
    now = unix_now()
    return {
        "id": f"session-{name}",
        "kind": "session",
        "parent_id": parent_id,
        "name": name,
        "display_name": None,
        "sky_cluster": None,
        "tmux_session": name,
        "box_host": None,
        "bridge_url": None,
        "gateway_url": None,
        "state": state,
        "config": {"tmux_kind": tmux_kind},  # pi | openclaw — extras passthrough
        "openclaw_sessions": [],
        "pi_sessions": [],
        "created_at": now,
        "last_pulled": now,
        "children": [],
    }


# --- Reconciliation logic (pure, testable) ---


@dataclasses.dataclass
class ScrapeResult:
    """Scraped state for one agent box. The collector merges this into
    the store."""

    agent_id: str
    tmux_sessions: list[str] = dataclasses.field(default_factory=list)
    openclaw_sessions: list[str] = dataclasses.field(default_factory=list)
    agent_state: str = "idle"
    bridge_reachable: bool = True


def reconcile_local(store_dir: str, scrape: ScrapeResult) -> None:
    """Merge a local scrape (main-box's own tmux + OpenClaw sessions) into
    the store. Seeds the root if absent. Preserves display_name on existing
    nodes. Removes session nodes whose tmux session has disappeared.

    Pure I/O: reads/writes the store dir, takes no subprocess input. The
    scrape is passed in, so this is unit-testable with fixture data.
    """
    now = unix_now()
    Path(store_dir).joinpath("nodes").mkdir(parents=True, exist_ok=True)

    # --- Root (Main Agent) ---
    root = load_node(store_dir, ROOT_AGENT_ID) or root_node()
    # Derive state from active sessions: working if any tmux/openclaw
    # session is alive, else idle. (scrape.agent_state is a hint; the
    # session-derived state is more accurate for the root.)
    has_sessions = bool(scrape.tmux_sessions) or bool(scrape.openclaw_sessions)
    root["state"] = "working" if has_sessions else "idle"
    root["pi_sessions"] = list(scrape.tmux_sessions)
    root["openclaw_sessions"] = list(scrape.openclaw_sessions)
    root["last_pulled"] = now

    # --- Session nodes under root ---
    # Index existing session children so we can preserve display_name and
    # detect removed sessions.
    existing_children: dict[str, dict[str, Any]] = {}
    for child_id in list(root.get("children", [])):
        node = load_node(store_dir, child_id)
        if node and node.get("kind") == "session":
            existing_children[node["tmux_session"]] = node

    desired_tmux = set(scrape.tmux_sessions) | set(scrape.openclaw_sessions)
    new_children: list[str] = []
    # Preserve declared order: existing first (in their order), then new ones.
    seen: set[str] = set()
    for child_id in root.get("children", []):
        node = load_node(store_dir, child_id)
        if not node or node.get("kind") != "session":
            # Non-session child (sub-agent) — keep it.
            new_children.append(child_id)
            seen.add(child_id)
            continue
        tmux_name = node.get("tmux_session")
        if tmux_name in desired_tmux:
            # Still alive — refresh state, preserve display_name.
            node["state"] = _tmux_state(tmux_name, scrape)
            node["last_pulled"] = now
            write_node(store_dir, node)
            new_children.append(child_id)
            seen.add(child_id)
        else:
            # tmux session disappeared — mark completed (don't delete the
            # node; keep history). Drop from children list so it doesn't
            # render as active.
            node["state"] = "completed"
            node["last_pulled"] = now
            write_node(store_dir, node)

    # Add new sessions not yet in the tree.
    for tmux_name in scrape.tmux_sessions:
        sid = f"session-{tmux_name}"
        if sid in seen:
            continue
        node = session_node(tmux_name, ROOT_AGENT_ID, state="working", tmux_kind="pi")
        write_node(store_dir, node)
        new_children.append(sid)
        seen.add(sid)
    for tmux_name in scrape.openclaw_sessions:
        sid = f"session-{tmux_name}"
        if sid in seen:
            continue
        node = session_node(tmux_name, ROOT_AGENT_ID, state="working", tmux_kind="openclaw")
        write_node(store_dir, node)
        new_children.append(sid)
        seen.add(sid)

    root["children"] = new_children
    write_node(store_dir, root)
    write_index(store_dir, ROOT_AGENT_ID, now)


def _tmux_state(tmux_name: str, scrape: ScrapeResult) -> str:
    """Best-effort state for a tmux session. The collector can't easily tell
    if pi is working vs idle vs needs_input without scraping the pane — for
    v1, alive = working. The TUI's own refresh_state() does finer-grained
    detection via the .ready marker."""
    return "working"


def reconcile_sky(store_dir: str, sky_clusters: list[dict[str, Any]]) -> list[str]:
    """Merge `sky status` output into the store. Returns a list of log lines.

    - Any `agent-*` cluster with no node record is an orphan → added with
      state=unknown.
    - Any agent node whose cluster is gone → state=failed.

    Pure I/O: takes the parsed sky clusters, no subprocess.
    """
    logs: list[str] = []
    now = unix_now()
    agent_clusters = {
        c["name"]: c for c in sky_clusters if c.get("name", "").startswith(AGENT_PREFIX)
    }

    # Mark dead agents.
    for node_id in load_all_node_ids(store_dir):
        node = load_node(store_dir, node_id)
        if not node or node.get("kind") != "agent" or node_id == ROOT_AGENT_ID:
            continue
        cluster = node.get("sky_cluster") or node_id
        if cluster not in agent_clusters:
            if node.get("state") != "failed":
                node["state"] = "failed"
                node["last_pulled"] = now
                write_node(store_dir, node)
                logs.append(f"agent '{cluster}' cluster gone → failed")

    # Add orphan clusters.
    for cname, cdata in agent_clusters.items():
        existing = load_node(store_dir, cname)
        if existing:
            continue
        # New orphan — add under root.
        now = unix_now()
        node = {
            "id": cname,
            "kind": "agent",
            "parent_id": ROOT_AGENT_ID,
            "name": cname.removeprefix(AGENT_PREFIX),
            "display_name": None,
            "sky_cluster": cname,
            "tmux_session": None,
            "box_host": cdata.get("host") or cname,
            "bridge_url": f"http://{cdata.get('host', cname)}:8080/",
            "gateway_url": "ws://localhost:18789",
            "state": "unknown",
            "config": {},
            "openclaw_sessions": [],
            "pi_sessions": [],
            "created_at": now,
            "last_pulled": 0,
            "children": [],
        }
        write_node(store_dir, node)
        # Attach to root's children.
        root = load_node(store_dir, ROOT_AGENT_ID) or root_node()
        if cname not in root.get("children", []):
            root.setdefault("children", []).append(cname)
            root["last_pulled"] = now
            write_node(store_dir, root)
        logs.append(f"orphan cluster '{cname}' discovered → added (unknown)")

    return logs


# --- Scrapers (subprocess wrappers) ---


def run(cmd: list[str], timeout: float = 10.0) -> tuple[int, str, str]:
    """Run a command, return (returncode, stdout, stderr). Never raises."""
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout, r.stderr
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as e:
        return 1, "", str(e)


def scrape_tmux_sessions() -> list[str]:
    """List tmux session names. Returns [] if tmux isn't running."""
    rc, out, _ = run(["tmux", "list-sessions", "-F", "#{session_name}"])
    if rc != 0:
        return []
    return [line.strip() for line in out.splitlines() if line.strip()]


def scrape_openclaw_sessions() -> list[str]:
    """List OpenClaw session ids via the CLI. Returns [] if unavailable."""
    # The exact CLI is TBD (Open Question §15). Try `openclaw sessions list`
    # and fall back to empty. This is non-fatal — pi sessions are the
    # primary ones for now.
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


def scrape_sky_status() -> list[dict[str, Any]]:
    """Parse `sky status` into a list of cluster dicts. Returns [] on failure."""
    rc, out, _ = run(["sky", "status", "--json"], timeout=30.0)
    if rc != 0:
        return []
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        return []
    # sky status --json returns {"clusters": [...]} or a list directly.
    if isinstance(data, dict):
        clusters = data.get("clusters", [])
    elif isinstance(data, list):
        clusters = data
    else:
        return []
    out_list = []
    for c in clusters:
        if not isinstance(c, dict):
            continue
        out_list.append(
            {
                "name": c.get("name", c.get("cluster", "")),
                "status": c.get("status", ""),
                "host": c.get("host", c.get("hostname", "")),
                "resources": c.get("resources", ""),
            }
        )
    return out_list


def pull_bridge_state(bridge_url: str, timeout: float = 5.0) -> dict[str, Any] | None:
    """GET /state from an agent's bridge. Returns None if unreachable."""
    import urllib.request

    url = bridge_url.rstrip("/") + "/state"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return json.loads(resp.read().decode())
    except Exception:
        return None


# --- Collector (daemon) ---


class Collector:
    def __init__(self, store_dir: str, port: int = DEFAULT_PORT):
        self.store_dir = store_dir
        self.port = port
        self.last_sky_reconcile = 0.0
        self.stop = threading.Event()

    def pull_cycle(self) -> list[str]:
        """One pull cycle: scrape local + pull bridges + maybe reconcile sky."""
        logs: list[str] = []
        # 1. Local scrape (main-box).
        tmux = scrape_tmux_sessions()
        openclaw = scrape_openclaw_sessions()
        scrape = ScrapeResult(
            agent_id=ROOT_AGENT_ID,
            tmux_sessions=tmux,
            openclaw_sessions=openclaw,
            agent_state="idle" if not tmux else "working",
        )
        reconcile_local(self.store_dir, scrape)
        logs.append(f"local: {len(tmux)} tmux, {len(openclaw)} openclaw sessions")

        # 2. Pull bridges for sub-agents.
        for node_id in load_all_node_ids(self.store_dir):
            if node_id == ROOT_AGENT_ID:
                continue
            node = load_node(self.store_dir, node_id)
            if not node or node.get("kind") != "agent":
                continue
            bridge_url = node.get("bridge_url")
            if not bridge_url:
                continue
            state = pull_bridge_state(bridge_url)
            now = unix_now()
            if state is None:
                # Bridge unreachable — leave state, last_pulled goes stale.
                logs.append(f"bridge {node_id}: unreachable")
                continue
            # Merge bridge state into the node.
            node["state"] = state.get("state", node.get("state", "idle"))
            node["pi_sessions"] = state.get("pi_sessions", [])
            node["openclaw_sessions"] = state.get("openclaw_sessions", [])
            node["last_pulled"] = now
            write_node(self.store_dir, node)
            logs.append(f"bridge {node_id}: state={node['state']}")

        # 3. Sky reconcile (every 60s).
        if time.time() - self.last_sky_reconcile > SKY_RECONCILE_INTERVAL_S:
            clusters = scrape_sky_status()
            sky_logs = reconcile_sky(self.store_dir, clusters)
            logs.extend(sky_logs)
            self.last_sky_reconcile = time.time()
        return logs

    def run_once(self) -> list[str]:
        """Single pull cycle (for --once mode + tests)."""
        return self.pull_cycle()

    def run_daemon(self) -> None:
        """Server + pull loop. Blocks until self.stop is set."""
        server = ThreadingHTTPServer(("127.0.0.1", self.port), self._make_handler())
        t = threading.Thread(target=server.serve_forever, daemon=True)
        t.start()
        print(f"orchestra-collector on :{self.port} (store: {self.store_dir})")
        while not self.stop.is_set():
            try:
                logs = self.pull_cycle()
                for line in logs:
                    print(f"[{unix_now()}] {line}")
            except Exception as e:
                print(f"[{unix_now()}] pull cycle error: {e}", file=sys.stderr)
            self.stop.wait(PULL_INTERVAL_S)
        server.shutdown()

    def _make_handler(self):
        store_dir = self.store_dir
        collector = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, fmt, *args):
                pass  # quiet

            def _send(self, code: int, body: Any) -> None:
                data = json.dumps(body).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                if self.path == "/state":
                    # Aggregate the whole store for the caller.
                    nodes = {}
                    for nid in load_all_node_ids(store_dir):
                        n = load_node(store_dir, nid)
                        if n:
                            nodes[nid] = n
                    idx = load_index(store_dir) or {}
                    self._send(200, {"index": idx, "nodes": nodes})
                elif self.path == "/health":
                    self._send(200, {"ok": True})
                else:
                    self._send(404, {"error": "not found"})

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                raw = self.rfile.read(length) if length else b"{}"
                try:
                    body = json.loads(raw)
                except json.JSONDecodeError:
                    self._send(400, {"error": "bad json"})
                    return

                if self.path == "/tree/edge":
                    # Register parent→child edge.
                    parent = body.get("parent_id")
                    child = body.get("child_id")
                    if not parent or not child:
                        self._send(400, {"error": "parent_id + child_id required"})
                        return
                    pnode = load_node(store_dir, parent)
                    if pnode:
                        children = pnode.setdefault("children", [])
                        if child not in children:
                            children.append(child)
                        pnode["last_pulled"] = unix_now()
                        write_node(store_dir, pnode)
                    self._send(200, {"ok": True})
                elif self.path == "/tree/node":
                    # Register/update a node (from a sub-agent bootstrapping).
                    node = body
                    if "id" not in node:
                        self._send(400, {"error": "id required"})
                        return
                    # Preserve display_name if the caller didn't set it.
                    existing = load_node(store_dir, node["id"])
                    if existing and "display_name" not in node:
                        node["display_name"] = existing.get("display_name")
                    write_node(store_dir, node)
                    self._send(200, {"ok": True})
                elif self.path == "/tree/rename":
                    nid = body.get("id")
                    name = body.get("display_name")
                    if not nid:
                        self._send(400, {"error": "id required"})
                        return
                    node = load_node(store_dir, nid)
                    if not node:
                        self._send(404, {"error": "node not found"})
                        return
                    node["display_name"] = name
                    write_node(store_dir, node)
                    self._send(200, {"ok": True})
                elif self.path == "/tree/refresh":
                    # Force a pull cycle now.
                    logs = collector.pull_cycle()
                    self._send(200, {"ok": True, "logs": logs})
                else:
                    self._send(404, {"error": "not found"})

        return Handler


def main() -> int:
    ap = argparse.ArgumentParser(description="orchestra state collector")
    ap.add_argument("--store-dir", default=DEFAULT_STORE_DIR)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--once", action="store_true", help="one pull cycle then exit")
    args = ap.parse_args()

    collector = Collector(store_dir=args.store_dir, port=args.port)
    if args.once:
        logs = collector.run_once()
        for line in logs:
            print(f"[{unix_now()}] {line}")
        return 0
    try:
        collector.run_daemon()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
