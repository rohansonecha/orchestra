#!/usr/bin/env python3
"""gen-agent-yaml — Render a sub-agent SkyPilot YAML from a config JSON.

Design_Document.md §8. Invoked by the launch-sub-agent OpenClaw skill:

    python3 ~/orchestra/tree-view/gen-agent-yaml.py \\
        --name research-box \\
        --config '{"model":"...","effort":"high"}' \\
        --out /tmp/agent-research-box.yaml

The template (agent-template.yaml) uses {{DOUBLE_BRACE}} placeholders that
this script substitutes. Shell ${VARS} in the template are left untouched
(resolved by SkyPilot at launch). Conditional blocks {{#GPU}}...{{/GPU}}
are included only when the condition is truthy.

Stdlib only — no PyYAML dependency. The output is a SkyPilot YAML that
`sky launch` consumes.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import secrets
import sys
from pathlib import Path
from typing import Any

TEMPLATE_PATH = Path(__file__).resolve().parent / "agent-template.yaml"

# Defaults mirror main-box so an unspecified agent is "just another main-box"
# (Design §7).
DEFAULTS: dict[str, Any] = {
    "model": "zai-org/GLM-5.2-FP8",
    "provider": "glm",
    "effort": "medium",
    "autonomy": "guided",
    "urgency": "normal",
    "resources": {"cloud": "kubernetes", "cpus": 4, "memory": 16, "gpu": 0},
    "bridge_port": 8080,
    "pull_interval_s": 30,
    "idle_timeout_s": 0,
}

# Regex for {{VAR}} placeholders.
_VAR_RE = re.compile(r"\{\{(\w+)\}\}")
# Regex for conditional blocks {{#NAME}}...{{/NAME}} — included only if
# NAME resolves to a truthy value.
_COND_RE = re.compile(r"\{\{#(\w+)\}\}(.*?)\{\{/\1\}\}", re.DOTALL)


def render(template: str, vars_: dict[str, Any]) -> str:
    """Substitute {{VAR}} placeholders and process {{#COND}} blocks."""
    # First, handle conditional blocks.
    def cond_repl(m: re.Match) -> str:
        name = m.group(1)
        body = m.group(2)
        val = vars_.get(name)
        if _truthy(val):
            return body
        return ""

    out = _COND_RE.sub(cond_repl, template)
    # Then, substitute variables.
    def var_repl(m: re.Match) -> str:
        return str(vars_.get(m.group(1), ""))

    out = _VAR_RE.sub(var_repl, out)
    return out


def _truthy(val: Any) -> bool:
    if val is None:
        return False
    if isinstance(val, bool):
        return val
    if isinstance(val, (int, float)):
        return val != 0
    if isinstance(val, str):
        return val.lower() not in ("", "0", "false", "none")
    return bool(val)


def generate_token() -> str:
    """A per-agent secret for A2A bearer auth (Design §6)."""
    return "oc-" + secrets.token_hex(16)


def build_vars(
    name: str,
    config: dict[str, Any],
    parent_url: str,
    collector_url: str,
    token: str | None = None,
) -> dict[str, Any]:
    """Merge config over defaults and produce the template variable map."""
    merged = {**DEFAULTS, **config}
    # Deep-merge resources (config can override individual fields).
    resources = {**DEFAULTS["resources"], **config.get("resources", {})}
    merged["resources"] = resources

    agent_id = f"agent-{name}"
    tok = token or generate_token()

    return {
        "AGENT_NAME": name,
        "AGENT_ID": agent_id,
        "MODEL": merged["model"],
        "PROVIDER": merged["provider"],
        "CLOUD": resources["cloud"],
        "CPUS": resources["cpus"],
        "MEMORY": resources["memory"],
        "GPU": resources["gpu"],
        "BRIDGE_PORT": merged["bridge_port"],
        "PARENT_URL": parent_url,
        "COLLECTOR_URL": collector_url,
        "AGENT_TOKEN": tok,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="render a sub-agent SkyPilot YAML")
    ap.add_argument("--name", required=True, help="agent name (e.g. research-box)")
    ap.add_argument("--config", default="{}", help="config JSON string or path")
    ap.add_argument("--parent-url", default=os.environ.get("ORCHESTRA_PARENT_URL", ""))
    ap.add_argument(
        "--collector-url",
        default=os.environ.get("ORCHESTRA_COLLECTOR_URL", "http://agent-main-box:7777"),
    )
    ap.add_argument("--token", default=None, help="reuse an existing agent token")
    ap.add_argument("--template", default=str(TEMPLATE_PATH))
    ap.add_argument("--out", required=True, help="output YAML path")
    args = ap.parse_args()

    # Config can be a JSON string or a path to a JSON file.
    config: dict[str, Any] = {}
    if args.config.strip().startswith("{"):
        config = json.loads(args.config)
    elif args.config != "{}":
        config = json.loads(Path(args.config).read_text())

    template = Path(args.template).read_text()
    vars_ = build_vars(args.name, config, args.parent_url, args.collector_url, args.token)
    rendered = render(template, vars_)

    Path(args.out).write_text(rendered)
    print(f"rendered {args.out} (agent-{args.name}, token={vars_['AGENT_TOKEN'][:12]}...)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
