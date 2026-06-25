# Spawn Sub-Agent (Orchestra Skill — pi-loaded)

## What this skill does

Tells a pi **session** how to request a new sub-agent from its parent
OpenClaw gateway. The session does NOT launch SkyPilot boxes itself —
only the parent agent's gateway can (it runs the `launch-sub-agent`
OpenClaw skill). The session just formats the request and shells out to
the gateway's CLI.

## When to use

- You need a fresh box with different resources (e.g. a GPU for model
  eval, more CPUs for a parallel sweep).
- You want to delegate a long-running task to a separate agent that runs
  in parallel, so your own session stays responsive.
- You need a fresh git worktree / clean state on a different box.

## How to spawn — the only sanctioned way

Shell out to the parent gateway's CLI with a JSON message:

```bash
openclaw agent --message "$(cat <<'EOF'
{"intent":"spawn-agent","config":{"model":"zai-org/GLM-5.2-FP8","effort":"high","urgency":"normal","resources":{"cpus":4,"memory":16,"gpu":0}},"reason":"need a fresh box for parallel model evaluation"}
EOF
)" --thinking high
```

The gateway (with the `launch-sub-agent` OpenClaw skill loaded) will:
1. Validate the config against autonomy rules.
2. Render the SkyPilot YAML via `gen-agent-yaml.py`.
3. `sky launch` a new `agent-<name>` cluster.
4. Register the parent→child edge with the main-box collector.
5. Reply with the new agent's `bridge_url`.

## Config fields (all optional — defaults mirror main-box)

| Field       | Values                                  | Default            |
|-------------|------------------------------------------|--------------------|
| model       | any model id                             | zai-org/GLM-5.2-FP8 |
| provider    | glm, ...                                 | glm                |
| effort      | off, minimal, low, medium, high, xhigh   | medium             |
| autonomy    | guided, semi, full                        | guided             |
| urgency     | low, normal, high, urgent                 | normal             |
| resources   | {cloud, cpus, memory, gpu}               | 4 CPU, 16 GB, 0 GPU |
| skills      | orchestra skills to load into pi sessions | []                 |
| openclaw_skills | OpenClaw skills to load into gateway | ["launch-sub-agent"] |

Unknown fields pass through to the agent's persona as context (Design §7).

## Naming

The agent name is derived from your `reason` or you can pass it
explicitly as `"name":"research-box"` in the config. The SkyPilot cluster
will be `agent-<name>` — this prefix is how the collector discovers
agents via `sky status`.

## After spawning

The gateway replies with the new agent's `bridge_url`. To talk to it
(A2A), send a `tasks/send` to that URL (Phase 5). For now, you can
`sky ssh agent-<name>` to interact with it directly.

## Important — sessions cannot launch boxes

You (a pi session) **never** call `sky launch` directly. You always go
through `openclaw agent --message`. This keeps the tree store
authoritative — the gateway registers every spawn with the collector so
the TUI's tree stays correct. Bypassing the gateway creates orphan
clusters the TUI won't know about.
