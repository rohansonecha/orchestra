#!/usr/bin/env python3
"""sky-bridge — SkyPilot SDK / CLI wrapper for cluster reconciliation.

Design_Document.md §5, §8, §10. Wraps `sky status` so the collector can
reconcile the tree store against SkyPilot's view of "what boxes exist".

`sky status` is the authoritative source for box existence — independent
of the tree store, so the TUI can always rebuild the agent roster even if
the store is corrupted. Sessions (tmux, not SkyPilot clusters) are NOT in
`sky status`; they live only in the tree store + `tmux ls` per box.

Usage:
    sky-bridge.py             # print sky status as parsed JSON
    sky-bridge.py --raw       # raw sky status output (debug)

Standalone script + importable `sky_status()` function. The collector
inlines its own sky scraping (no import dependency) but this script is
useful for manual inspection + future SDK migration.

No third-party deps — stdlib only. Calls the `sky` CLI.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from typing import Any

AGENT_PREFIX = "agent-"


def sky_status(timeout: float = 30.0) -> list[dict[str, Any]]:
    """Parse `sky status --json` into a list of cluster dicts.

    Returns only agent clusters (name starts with `agent-`), since the
    collector only tracks agents (not other SkyPilot clusters the user
    may have launched separately). Returns [] on any failure.

    Each dict: {name, status, host, resources}.
    """
    try:
        r = subprocess.run(
            ["sky", "status", "--json"], capture_output=True, text=True, timeout=timeout
        )
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
        return []
    if r.returncode != 0:
        return []
    try:
        data = json.loads(r.stdout)
    except json.JSONDecodeError:
        return []
    if isinstance(data, dict):
        clusters = data.get("clusters", [])
    elif isinstance(data, list):
        clusters = data
    else:
        return []
    out = []
    for c in clusters:
        if not isinstance(c, dict):
            continue
        name = c.get("name", c.get("cluster", ""))
        if not name.startswith(AGENT_PREFIX):
            continue
        out.append(
            {
                "name": name,
                "status": c.get("status", ""),
                "host": c.get("host", c.get("hostname", "")),
                "resources": c.get("resources", ""),
            }
        )
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description="sky status reconciliation wrapper")
    ap.add_argument("--raw", action="store_true", help="raw sky status output")
    args = ap.parse_args()

    if args.raw:
        try:
            r = subprocess.run(["sky", "status"], timeout=60)
            return r.returncode
        except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as e:
            print(f"sky status failed: {e}", file=sys.stderr)
            return 1

    clusters = sky_status()
    print(json.dumps(clusters, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
