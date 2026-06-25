# Tree View — Design Document

> Status: Draft · Owner: orchestra · Module: `tree-view/`
> Scope: TUI Tree View mode + agent lifecycle + A2A communication.
> Out of scope: Graph View rendering (but the data model is designed for it).

## 1. Goals

- Render the agent/session hierarchy as an indented tree in the TUI.
- Let any agent (LLM-backed OpenClaw) autonomously spawn sessions and
  sub-agents, and let sessions trigger new sub-agents.
- Make every agent queryable two ways: (a) the local tree-state store on
  main-box, (b) the SkyPilot SDK (`sky status`) as the source of truth for
  "what boxes exist".
- Keep the TUI a pure control plane: it reads state, renders, and sends
  commands — it owns no long-lived work.
- Ship a design whose data model extends to a Graph View without rewrite.
- **Reuse OpenClaw as the brain, same-box transport, and session manager.**
  Do not rebuild what the gateway already provides.

## 2. Hierarchy Model

Two node types, strict parent → child edges for now:

```
Main Agent (main-box, OpenClaw + TUI)
├── Session: fix-login-bug        (tmux + pi + worktree, leaf)
├── Sub-Agent: research-box       (SkyPilot box, OpenClaw)
│   ├── Session: lit-review       (leaf)
│   └── Sub-Agent: deep-reader    (SkyPilot box)
└── Sub-Agent: deploy-box
    └── Session: canary-deploy
```

**Edge rules (confirmed):**

| Node type   | Can spawn sessions | Can spawn agents | Leaf? |
|-------------|:--:|:--:|:--:|
| Main Agent  | yes | yes | no  |
| Sub-Agent   | yes | yes | no  |
| Session     | no  | yes | yes (for sessions) |

A Session is a tmux session running pi in a git worktree, on the **same box**
as its parent agent. Sessions cannot host further sessions, but a pi session
may request a new sub-agent (the parent agent's OpenClaw gateway performs the
actual SkyPilot launch — the session only sends the request).

**Node identity.** Each node has a stable `id` (e.g. `agent-research-box`,
`session-lit-review-8824`). Agents are identified 1:1 with a SkyPilot cluster
name; sessions are identified 1:1 with a tmux session name on their parent's
box.

## 3. What OpenClaw Provides (and what we still build)

OpenClaw is already running on main-box as a daemon (port 18789, loopback,
token auth). It speaks **JSON-RPC over WebSocket** (protocol v4) and gives us,
for free:

| Capability | OpenClaw method | Replaces in v1 design |
|------------|-----------------|----------------------|
| Agent personas (main, work, ...) | `agents.list/create/update` | Custom agent config loader |
| Create/send sessions | `sessions.create/send` | Custom session spawn |
| List sessions + state | `sessions.list` | `tmux ls` scraping |
| Stream session output | `sessions.messages.subscribe` → `session.message`/`session.operation`/`session.tool` events | Custom SSE |
| Task objects (A2A-shaped) | `tasks.list/get/cancel` | Custom task store |
| Chat history | `chat.history`, `chat.message.get` | Custom transcript reader |
| CLI injection | `openclaw agent --message "..." --thinking high` | Custom HTTP client |
| Skills (gateway-loaded) | `skills.search/detail/install`, `~/.openclaw/workspace/skills/<name>/SKILL.md` | — (different from orchestra skills, see §6) |
| Webhooks + cron | `cron.add`, webhook endpoints | Custom scheduler |

**What OpenClaw does NOT provide (gaps we still fill):**

1. **Cross-box transport.** Gateway binds to loopback. For agent-on-A to talk
   to agent-on-B, we expose a thin HTTP bridge per box (§5) that forwards to
   the local gateway's WebSocket and presents an A2A-compliant face.
2. **Hierarchy/tree state across boxes.** `sessions.list` is per-box. Nothing
   tracks parent→child agent relationships or aggregates state centrally.
   The `~/.orchestra/tree/` store + collector is still needed (§4).
3. **SkyPilot launch mechanics.** OpenClaw can spawn *sessions* (same-box) but
   not *boxes*. The `launch-sub-agent` skill + `gen-agent-yaml.py` + `sky
   launch` is still our job (§7).
4. **The TUI tree renderer.** Still ours (§8).
5. **Pi session management.** Pi coding sessions (tmux + worktree) are separate
   from OpenClaw sessions. OpenClaw manages the *agent persona*; pi manages
   the *coding work*. The TUI dispatches pi sessions as today. OpenClaw does
   not know about pi sessions unless we tell it (the collector scrapes
   `tmux ls` per box and merges into the tree store alongside OpenClaw's
   session list).

## 4. Component Architecture

```
                                MAIN-BOX
  ┌─────────────────────────────────────────────────────────────┐
  │  TUI (rust)        State Collector          OpenClaw gateway │
  │  - Tree View       (HTTP :7777, python)      (WS :18789)     │
  │  - reads disk ◀──── writes ~/.orchestra/   (Main Agent brain)│
  │                                       ▲                      │
  │  tree-state store: ~/.orchestra/     │ sky-bridge.py         │
  │  tree/<id>.json                       │ (sky status reconcile)│
  └─────────────────────────────────────────────────────────────┘
            ▲ collector pulls state (30s)   ▲ A2A HTTP/JSON-RPC
            │ via openclaw-bridge :8080     │ (delegation, spawn requests)
            │                               │
  ┌─────────┴──────────────────┐    ┌───────┴──────────────────────┐
  │  SUB-AGENT box             │    │  SUB-AGENT box               │
  │  OpenClaw gateway (WS :18789)  │  OpenClaw gateway (WS :18789) │
  │  openclaw-bridge (HTTP :8080)  │  openclaw-bridge (HTTP :8080) │
  │  tmux + pi sessions         │    │  tmux + pi sessions          │
  └────────────────────────────┘    └──────────────────────────────┘
```

**Long-lived processes per agent box:**

1. **OpenClaw gateway** (WS :18789, loopback) — the agent's LLM brain,
   session manager, and same-box transport. Already running on main-box;
   sub-agent boxes start it via SkyPilot setup.
2. **openclaw-bridge** (HTTP :8080) — thin A2A-compliant front-end. Exposes
   Agent Card + JSON-RPC 2.0 over HTTP, translates to the local gateway's
   WebSocket. Also exposes `GET /state` for the collector. ~150 lines of
   Python (stdlib + websockets). One per agent box.

**On main-box only:**

3. **State Collector** (HTTP :7777, python) — pulls state from every known
   agent's bridge every 30s, runs `sky status` reconciliation every 60s,
   writes the tree-state store to disk. Separate from the TUI so TUI
   crashes don't drop state. ~200 lines.
4. **TUI** (rust) — pure reader of the tree-state store + renderer. Sends
   user-initiated commands (spawn, teardown) through the collector so the
   store stays authoritative.

> **Why not host the collector in the TUI?** The TUI is the control plane
> and is killed/upgraded freely. The collector must survive TUI restarts.
> A 200-line standalone daemon is cheap insurance.

> **Why pull instead of push heartbeats?** OpenClaw already exposes
> `sessions.list` + `agents.list` + `tasks.list` per box. Pulling via the
> bridge reuses this — no custom heartbeat daemon per box. Push heartbeats
> would duplicate state OpenClaw already tracks. Trade-off: 30s latency on
> state updates (acceptable for a personal platform; the TUI renders from
> disk so it's smooth between pulls).

## 5. Tree State Store (centralized, main-box)

Single source of truth, on disk at `~/.orchestra/tree/`:

```
~/.orchestra/tree/
  index.json                  # { root_id, version, updated_at }
  nodes/
    agent-main-box.json
    agent-research-box.json
    session-fix-login-8824.json
    ...
```

**Node record:**

```json
{
  "id": "agent-research-box",
  "kind": "agent",                 // "agent" | "session"
  "parent_id": "agent-main-box",
  "name": "research-box",
  "display_name": null,            // user-set display name; null → use name
  "sky_cluster": "agent-research-box",   // SkyPilot cluster name (agents only)
  "tmux_session": "lit-review-8824",     // tmux name (sessions only)
  "box_host": "agent-research-box",      // DNS/local name for bridge + SSH
  "bridge_url": "http://agent-research-box:8080/",
  "gateway_url": "ws://localhost:18789", // same-box only; not cross-box reachable
  "state": "working",              // initializing|working|idle|needs_input|completed|failed|stale
  "config": { /* §6 */ },
  "openclaw_sessions": ["lit-review-8824"],  // OpenClaw-managed sessions on this box
  "pi_sessions": ["fix-login-8824"],         // pi tmux sessions on this box
  "created_at": 1719300000,
  "last_pulled": 1719300123,        // last successful collector pull
  "children": ["session-lit-review-8824", "agent-deep-reader"]
}
```

**Write path.** Only the State Collector writes. It pulls from each agent's
bridge (`GET /state`) and `sky status`, merges into the store. Edges
(parent→child) are written when an agent spawns a child — the spawning
agent POSTs the new edge to the collector (`POST /tree/edge`) as part of
the launch-sub-agent skill (§7).

**Read path.** The TUI reads `index.json` + all `nodes/*.json` on each
100ms poll (cheap: a few dozen small files). This matches the current
session-list polling pattern.

**Edge authority.** Parent → child edges are owned by the **parent agent's
OpenClaw gateway**, not the child. When an agent spawns a child, the
launch-sub-agent skill (running inside OpenClaw) POSTs the edge to the
collector. Children cannot rewrite their own lineage.

**Reconciliation via SkyPilot SDK.** The collector's `sky-bridge.py` calls
`sky.status()` (Python SDK) every 60s and reconciles: any `agent-*` cluster
in SkyPilot with no node record is an orphan (added with `state: "unknown"`);
any node whose cluster is gone is marked `failed`. This makes the store
self-healing — even if bridges go down, `sky status` is the ground truth for
box existence. Sessions (tmux, not SkyPilot clusters) are NOT in `sky
status`; they live in the tree store + `tmux ls` per box (scraped by the
collector via the bridge's `/state` endpoint).

## 6. A2A Communication (OpenClaw-backed)

Each agent box runs `openclaw-bridge` on `:8080` — a thin HTTP server that
presents the standard A2A protocol and forwards to the local OpenClaw
gateway's WebSocket.

**Agent Card** — `GET /.well-known/agent.json`:

```json
{
  "name": "research-box",
  "version": "0.1.0",
  "description": "Research sub-agent (orchestra)",
  "capabilities": { "streaming": true, "pushNotifications": false },
  "defaultInputModes": ["text"],
  "defaultOutputModes": ["text", "json"],
  "authentication": { "schemes": ["bearer"] },
  "url": "http://research-box:8080/"
}
```

**A2A JSON-RPC 2.0 methods** (POST `/`) — translated by the bridge to
OpenClaw gateway calls:

| A2A method            | Bridge translates to (OpenClaw WS)              | Semantics |
|-----------------------|-------------------------------------------------|-----------|
| `message/send`        | `sessions.send` (or `chat.send`)                | One-shot message to the agent. |
| `tasks/send`          | `sessions.create` + `sessions.send`             | Create a stateful task session. |
| `tasks/get`           | `sessions.describe` + `chat.history`            | Poll task status + transcript. |
| `tasks/cancel`        | `sessions.abort`                                | Cancel active work. |
| `tasks/sendSubscribe` | `sessions.create` + `sessions.send` + `sessions.messages.subscribe` → SSE | Create task + stream `session.message`/`session.tool` events as SSE `TaskStatusUpdate`. |
| `tasks/pushNotification/set` | (deferred)                              | Optional webhook registration. |

**Bridge internals (~150 lines Python):**
- HTTP server (stdlib `http.server` or `aiohttp`).
- One persistent WebSocket connection to `ws://localhost:18789` (the local
  OpenClaw gateway), authenticated with the gateway token from
  `~/.openclaw/openclaw.json`.
- Request: parse A2A JSON-RPC → translate to OpenClaw method+params → send
  WS frame `{type:"req", id, method, params}` → await `{type:"res", id,
  ok, payload}` → translate response back to A2A shape.
- For `tasks/sendSubscribe`: after `sessions.send`, call
  `sessions.messages.subscribe`, then pump `session.message`/`session.tool`
  WS events into the HTTP SSE response as `TaskStatusUpdate` events until
  the task completes or the client disconnects.
- `GET /state`: calls `sessions.list` + `agents.list` + `tasks.list` on the
  gateway, merges with `tmux ls` output, returns aggregated JSON for the
  collector. This is the collector's pull target.

**Auth.** Bearer token = a per-agent secret generated at launch, written to
`~/.orchestra/agent-token` on the agent box and registered with the
collector (which holds the token registry). The TUI reads the registry to
make authenticated calls when the user manually pokes an agent. Same-box
traffic (session → parent agent via CLI) doesn't need the token —
`openclaw agent --message` uses the local gateway directly.

**How orchestra uses it:**

- When the Main Agent wants `research-box` to investigate something, its
  OpenClaw gateway calls `tasks/sendSubscribe` on `research-box`'s bridge
  URL and streams progress. The SSE events feed back into the agent's
  reasoning.
- A pi **session** that needs a sub-agent shells out to
  `openclaw agent --message '{"intent":"spawn-agent","config":{...}}'`
  (the parent gateway's CLI). The parent gateway has the
  `launch-sub-agent` skill loaded (§7), performs the SkyPilot launch, and
  replies with the new agent's `bridge_url`. The session cannot launch
  boxes itself — only the agent can.
- `message/send` is used for quick synchronous queries ("are you busy?",
  "what's your model?"). `tasks/*` is used for long-running delegated work.

**Two skill systems — IMPORTANT distinction:**

| Skill system | Location | Loaded by | Purpose |
|--------------|----------|-----------|---------|
| Orchestra skills | `~/.orchestra/skills/<name>.md` | pi (via `--append-system-prompt`) | Tell pi sessions how to do things (linear-read, spawn-sub-agent request formatting) |
| OpenClaw skills | `~/.openclaw/workspace/skills/<name>/SKILL.md` | OpenClaw gateway | Tell the agent persona how to do things (launch-sub-agent execution, cross-box delegation) |

The `spawn-sub-agent` workflow spans both:
- **Orchestra skill** (`~/.orchestra/skills/spawn-sub-agent.md`, pi-loaded):
  tells pi how to format the spawn request and shell out to
  `openclaw agent --message`.
- **OpenClaw skill** (`~/.openclaw/workspace/skills/launch-sub-agent/SKILL.md`,
  gateway-loaded): tells OpenClaw how to actually run `gen-agent-yaml.py` +
  `sky launch` + register the edge with the collector when it receives a
  spawn request.

## 7. Agent Config (extensible)

Every agent carries a config. Defaults mirror main-box so an unspecified
agent is "just another main-box".

```json
{
  "model": "zai-org/GLM-5.2-FP8",
  "provider": "glm",
  "effort": "medium",          // off|minimal|low|medium|high|xhigh
  "autonomy": "guided",        // guided|semi|full  (what the agent may do without asking)
  "urgency": "normal",         // low|normal|high|urgent  (affects priority queuing + SSE push rate)
  "skills": ["linear-read"],   // orchestra skills to load into pi sessions
  "openclaw_skills": ["launch-sub-agent"],  // OpenClaw skills to load into the gateway
  "workspace_files": [],       // openclaw workspace files to mount (identity, soul, ...)
  "resources": {               // SkyPilot resources
    "cloud": "kubernetes",
    "cpus": 4,
    "memory": 16,
    "gpu": 0
  },
  "bridge_port": 8080,
  "pull_interval_s": 30,       // how often the collector pulls this box
  "idle_timeout_s": 0          // 0 = never auto-teardown (persistent)
}
```

**Extensibility.** The schema is versioned (`"schema_version": 1`) and all
fields are optional with serde-style defaults. Adding a new knob (e.g.
`"max_subagents": 3`) is additive — old configs still parse. The launcher
skill (§8) documents the current field set; unknown fields are passed
through to the agent's OpenClaw persona as context, so an agent can read
its own config at runtime.

**Spawn intent (session → agent).** A pi session requests a new agent by
shelling out to the parent OpenClaw gateway's CLI:

```bash
openclaw agent --message "$(cat <<'EOF'
{"intent":"spawn-agent","config":{"model":"...","effort":"high","urgency":"urgent"},"reason":"need a fresh box with GPU for model eval"}
EOF
)" --thinking high
```

The parent gateway (with the `launch-sub-agent` OpenClaw skill loaded)
validates autonomy rules, launches via SkyPilot, registers the edge with
the collector, and replies with the new agent's `bridge_url`. This is the
only sanctioned way a session creates an agent — it never calls `sky
launch` directly.

## 8. SkyPilot Integration

**Main Agent spec** = current `skypilot/main-box.yaml`, plus three additions
to setup:
1. Install + start the `openclaw-bridge` daemon (systemd unit or
   `openclaw daemon start`-style).
2. Install + start the State Collector daemon.
3. Install `sky-bridge.py` + `gen-agent-yaml.py` + `openclaw-bridge.py` +
   the `launch-sub-agent` OpenClaw skill.

**Sub-agent spec** = generated from a template at
`tree-view/agent-template.yaml`. The template is `main-box.yaml` minus the
OpenClaw workspace file mounts (sub-agents get workspace files via their
config, not hardcoded) plus:

1. A `setup` block that installs + starts `openclaw-bridge`, writes
   `~/.orchestra/agent-token`, and loads the `launch-sub-agent` OpenClaw
   skill.
2. A templated `run` that registers with the main-box collector
   (`POST /tree/node` with its bridge_url + token) and then
   `sleep infinity`.
3. Env vars `ORCHESTRA_PARENT_URL` (parent's bridge URL) and
   `ORCHESTRA_COLLECTOR_URL` (main-box:7777).

**Generator.** `tree-view/gen-agent-yaml.py` takes a config JSON, renders
the template, and emits a yaml. Invoked by the launch-sub-agent skill.

**Launcher skill (OpenClaw, gateway-loaded)** at
`~/.openclaw/workspace/skills/launch-sub-agent/SKILL.md`:

```
1. Read desired config (model, effort, urgency, ...) from the spawn request.
2. python3 ~/orchestra/tree-view/gen-agent-yaml.py --config <json> --out /tmp/agent-<name>.yaml
3. sky launch --infra "$SKY_INFRA" -c agent-<name> /tmp/agent-<name>.yaml
4. POST the new edge to main-box collector: POST http://main-box:7777/tree/edge
   {parent_id: <self>, child_id: "agent-<name>", bridge_url: "http://agent-<name>:8080/"}
5. Return the agent's bridge_url to the caller.
```

**Launcher skill (orchestra, pi-loaded)** at
`~/.orchestra/skills/spawn-sub-agent.md` — tells pi how to format the
spawn request and shell out to `openclaw agent --message`. This skill is
concatenated into pi's `--append-system-prompt` by the TUI's
`load_skills_prompt()` (existing mechanism).

Naming convention `agent-<name>` lets `sky status` reconciliation discover
agent clusters by prefix.

**Queryable via SkyPilot SDK.** The collector's `sky-bridge.py` calls
`sky.status()` which returns cluster name, status, resources, hostname, and
uptime for every cluster. This is the authoritative "what agents exist"
query — independent of the tree store, so the TUI can always rebuild the
agent roster even if the store is corrupted. Sessions (tmux, not SkyPilot
clusters) are NOT in `sky status`; they live only in the tree store +
`tmux ls` on each box (scraped by the collector via bridge `/state`).

## 9. TUI Tree View

**Mode switch.** The current list view becomes one mode; Tree View is a
second mode. `Tab` cycles modes (or a keymap entry). Both modes share the
session-dispatch input.

**Rendering — spatial left-to-right graph (neural-net style).**

The root (Main Agent) is a single dot on the left, vertically centered.
Children fan out to the right, connected by diagonal lines. Depth maps to
horizontal column; siblings spread vertically. No text labels in the graph
itself — just dots and lines. State is encoded by color; identity is
revealed in a detail pane on selection.

```
                                  ○ session-lit-review
                          ┌───────●
                          │       ○ session-deep-reader
                  ● agent-research-box
                ┌─┴───────● agent-deploy-box
        ● ──────┤         └───────○ session-canary
   root │       ● agent-experimental
        └───────○ session-fix-login-bug

  legend: ● agent   ○ session   (color encodes state — see below)
```

- `●` = agent node, `○` = session node (reuses current session-state icons
  for the dot glyphs).
- **Horizontal position** = depth in the tree (root at left, leaves at
  right). Columns auto-sized to terminal width / max depth.
- **Vertical position** = sibling spread. Children of a node are centered
  around the parent's Y, fanned out with even spacing. Subtree heights
  bubble up so parents center over their children (standard
  left-to-right tree layout, Reingold-Tilford-style).
- **Lines** are drawn with box-drawing characters (`╲`, `╱`, `─`, `┌`, `└`,
  `├`) connecting parent centers to child centers. Diagonal segments use
  the sloped chars where the terminal supports them; fall back to
  right-angle stepping on narrow terminals.
- **No labels in the graph.** Node identity, config, last pull, and recent
  OpenClaw task log are shown in a **detail pane** on the right (or bottom)
  when a node is selected. This keeps the graph itself clean and lets it
  scale to many nodes without horizontal scrolling.
- **Color encodes state** (v1 — implemented from the start):

  | State           | Color      |
  |-----------------|------------|
  | working         | yellow     |
  | idle            | dark gray  |
  | needs_input     | cyan       |
  | completed       | green      |
  | failed          | red        |
  | stale           | dim gray   |
  | initializing    | blue       |

  Agents and sessions use the same palette; the dot glyph (`●` vs `○`)
  distinguishes agent vs session.
- **Legend (top-right corner).** A compact legend is always visible in the
  top-right corner of the TUI, showing the color → state mapping and the
  dot glyphs. Rendered as a small floating block that doesn't overlap the
  graph (the layout engine reserves the rightmost columns for it). Example:

  ```
  ● agent  ○ session
  ● working   ● idle    ● stale
  ● failed    ● needs_input
  ```

  (Colors are applied via ratatui `Style::fg`; the legend uses the same
  colors so it's self-documenting.)
- **Selected node** is drawn with a highlighted ring (e.g. reversed-video
  or a bright outline `[●]`) and its inbound path back to the root is
  drawn in a brighter line color so the user can see lineage at a glance.

**Navigation & actions.**

| Key           | Action                                                    |
|---------------|-----------------------------------------------------------|
| `←`           | Move selection to parent (go up the tree, depth − 1).    |
| `→`           | Move selection to nearest child (go down the tree, depth + 1). |
| `↑`/`↓`       | Move selection to previous/next node at the same depth (within-level traversal). |
| `Enter`       | Enter the selected node (see "Enter semantics" below).   |
| `n`           | Rename the selected node (prompts for new display name). |
| `r`           | Refresh: force `sky status` reconcile now.               |
| `x`           | Tear down selected agent (`sky down`) with confirm.      |
| `d`           | Toggle detail pane for selected node.                    |

Navigation is axis-aligned to the spatial layout: `←`/`→` traverse depth
(parent ↔ child), `↑`/`↓` move within the current depth level (between
siblings or cousins at the same column). Within-level movement picks the
nearest node by Y coordinate; wrapping is off by default.

**Enter semantics — the two-level TUI.** The TUI has two levels:

1. **Tree View** (top level) — the spatial graph described above. Shows
   the whole agent/session hierarchy.
2. **Agent View** (entered from Tree View) — the **classic orchestra TUI**
   (the current list view: session list + dispatch input), scoped to one
   agent's sessions.

`Enter` on a node drops into that node's context:

- **Agent node (including Main Agent)** — enters **Agent View**: the
  classic orchestra list view scoped to that agent. The user sees that
  agent's sessions (the current session list widget) + the dispatch input.
  From here they can:
  - Type a prompt + `Enter` → spawn a new **session** under this agent
    (existing behavior, unchanged).
  - Type `/agent <name>` → spawn a new **sub-agent** under this agent,
    then **return to Tree View** with the new agent node highlighted.
    (This is the primary way to grow the tree.)
  - `Tab` or `Esc` → return to Tree View.
  - Attach to a session with `→`/`Enter` (existing behavior).
- **Session node** — attaches directly via tmux (bypasses Agent View).
  Same as `→` on a session in the list view. Detach returns to Tree View.
- **Root (Main Agent) with no children** — `Enter` drops into Agent View,
  which shows an empty session list + the dispatch input. The user's only
  meaningful action is to type a prompt (spawn a session) or
  `/agent <name>` (spawn a sub-agent). This is the bootstrap flow.

**`/agent <name>` — spawning a sub-agent from Agent View.**

When the user types `/agent <name>` in the Agent View dispatch input and
presses `Enter`:

1. The TUI sends a spawn request to the current agent's OpenClaw gateway
   (via the bridge, §6) with the requested name + default config.
2. The gateway runs the `launch-sub-agent` skill: `gen-agent-yaml.py` +
   `sky launch` + register edge with collector (§8).
3. The TUI switches back to **Tree View** and highlights the newly created
   agent node (auto-selected, path to root drawn bright).
4. The new agent appears as a child of the current agent. Its initial
   state is `initializing` (SkyPilot is launching); it transitions to
   `idle` once the bridge reports ready.

The `/agent` command is parsed by the TUI's dispatch input handler — if
the input starts with `/agent `, it's treated as a spawn-agent request
instead of a session prompt. Other `/`-commands can be added later
(`/session`, `/model`, etc.).

**Renaming nodes (`n`).**

- Pressing `n` on a selected node opens an inline rename prompt
  (reuses the dispatch input area).
- Renaming sets the node's `display_name` field in the tree store. The
  internal `id` (SkyPilot cluster name for agents, tmux session name for
  sessions) is **never changed** — it's the stable identity. Only the
  display name changes.
- The detail pane shows `display_name` if set, else `id`. The Tree View
  graph has no labels (so renaming only affects the detail pane + Agent
  View list). Future Graph View with labels will pick up `display_name`
  automatically.
- Renaming is cheap, instant, and reversible. No processes are restarted,
  no boxes are relaunched.
- The collector persists `display_name` to the node record
  (`~/.orchestra/tree/nodes/<id>.json`), so names survive TUI restarts.

**Detach / return path.** Every enter/attach path terminates back at the
Tree View TUI on main-box:

- From **Agent View**: `Tab` or `Esc` returns to Tree View.
- From **tmux attach** (session): `Left` (at column 0) or `Ctrl+C`
  detaches tmux. For sessions on sub-agent boxes, detaching tmux exits the
  `sky ssh` connection, which returns to the TUI.
- The TUI process stays alive on main-box throughout (it suspends
  rendering and waits for the child process to exit / the user to press
  `Tab`), so the tree state keeps refreshing in the background. After
  returning, the TUI forces a full redraw (clears tmux leftover output).

**Layout engine.** `tui/src/tree_layout.rs` computes (x, y) coordinates
for each node from the tree store, using a bounded Reingold-Tilford
algorithm. The engine is pure (tree in, positions out) and unit-tested
without a terminal. The renderer in `tree_view.rs` just draws dots at
positions and lines between parent↔child positions. This separation makes
the layout reusable for Graph View (which swaps the tree layout for a
force-directed one but keeps the same renderer).

**Consistent state.** The TUI reads the tree store every 100ms and
re-renders. Stale agents (no successful pull for >90s = 3 missed pulls)
are marked `stale` and dimmed. The TUI never writes state except
user-initiated spawn/teardown commands, which go through the collector
(so the store stays the authority even for user actions).

## 10. State Tracking & Consistency

- **Pull interval:** 30s. Collector fetches `GET /state` from each known
  agent's bridge. Payload: `{id, state, config, openclaw_sessions[],
  pi_sessions[], active_task_ids}`.
- **Stale threshold:** 90s (3 missed pulls). Stale agents are dimmed; the
  TUI offers `r` to reconcile via `sky status`.
- **Reconciliation cadence:** collector runs `sky status` every 60s and on
  demand. Discrepancies (orphan clusters, dead clusters) are logged and
  reconciled into the store.
- **Session state** within a box: the bridge's `/state` endpoint aggregates
  OpenClaw's `sessions.list` (for OpenClaw-managed sessions) + `tmux ls`
  + readiness markers (current `.ready` file scheme) for pi sessions, so
  the collector sees all session states in one pull.
- **Crash recovery.** If the collector dies, the tree store stays on disk
  (last-written state, marked stale by the TUI after 90s). On collector
  restart, it re-pulls all bridges + `sky status` and refreshes. If a
  sub-agent's bridge dies, the collector marks it stale; if the SkyPilot
  cluster is down, `sky status` marks the node `failed`. If the TUI dies,
  nothing is lost — the store is on disk and the collector keeps writing.

## 11. Extensibility to Graph View

The Tree View is a special case of a graph. The data model is already
graph-shaped; only the renderer is tree-specific.

**What's already graph-ready:**

- `nodes/*.json` with `parent_id` is a degenerate graph (each node ≤ 1
  inbound edge). Adding a separate `edges.jsonl` (one edge per line:
  `{src, dst, kind, last_seen}`) generalizes it without changing the node
  schema.
- The A2A `tasks/*` call log is a natural source of **interaction edges**:
  every `tasks/send` from A to B is an edge `A → B kind=delegated`. The
  collector already sees these (agents report `active_task_ids` naming the
  peer; the bridge can log `tasks/send` callers).
- The query layer (`TreeStore`) exposes `children(id)`/`parent(id)` for
  Tree View; Graph View calls `neighbors(id)`/`edges()` over the same
  store. Same backing data, different projection.

**What Graph View adds later:**

- A second renderer mode that lays out nodes by a force-directed / layered
  algorithm instead of indent. The tree renderer and graph renderer share
  the same `Node`/`Edge` view structs.
- A query bar: "show me all agents that have talked to research-box in the
  last hour" → filter `edges.jsonl` by `dst == research-box &&
  last_seen > now-1h`.
- A2A already gives us the edge semantics (`delegated`, `queried`,
  `reported-to`). Graph View colors edges by kind; Tree View ignores them.

**A2A is the Graph View's backbone.** Because A2A (via the bridge) is the
only sanctioned inter-agent channel, every cross-agent interaction is
observable at the protocol layer. This means the graph is not inferred from
side channels — it is the protocol's own task graph. That is why committing
to A2A now (rather than file mailboxes) is what unlocks Graph View later
without re-instrumenting communication. The bridge is the natural
instrumentation point: it already translates every A2A call, so logging
edges there is a one-line addition.

## 12. Testing Strategy (tui-use)

- **Unit/state tests:** `tree-view/tests/` — Rust tests for the store
  read/write, stale detection, reconciliation merge logic. These run on
  test-box via `tests/run-on-staging.sh`.
- **tui-use TUI tests:** drive the rendered TUI and assert on output.
  Scenarios:
  1. Empty tree shows only Main Agent.
  2. Dispatch a session → appears indented under Main Agent.
  3. Mock a sub-agent bridge `/state` response → appears as child, state
     updates live.
  4. Stop the mock bridge → node goes `stale` within 90s.
  5. `sky status` reconciliation discovers a mock orphan cluster.
  The harness seeds the store with a fixture tree and starts a fake
  collector + fake bridges that replay canned `/state` responses, so tests
  are deterministic and don't pay SkyPilot launch latency.
- **Integration test (manual / nightly):** launch one real sub-agent via
  the launch-sub-agent skill on test-box, confirm it heartbeats and appears
  in the TUI, then tear it down. Not in the fast loop — SkyPilot latency
  makes it slow.

## 13. File / Module Layout

```
tree-view/
  Design_Document.md            # this file
  agent-template.yaml           # SkyPilot template for sub-agents (§8)
  gen-agent-yaml.py             # config JSON → yaml renderer
  sky-bridge.py                 # SkyPilot SDK wrapper for the collector
  openclaw-bridge.py            # A2A HTTP front-end → OpenClaw WS (§6)
  orchestra-collector.py        # main-box state collector daemon (§4, §5)
  tests/
    store_test.rs               # state store unit tests
    tui_tree_view.tui-use       # tui-use scenario scripts
    fixtures/                   # canned /state + sky status fixtures

tui/src/
  tree_view.rs                  # Tree View renderer + keymap (new module)
  tree_layout.rs                # spatial layout engine (tree → (x,y) positions)
  tree_store.rs                 # read-only client for ~/.orchestra/tree/
  command.rs                    # parses /agent, /session, etc. from dispatch input

skills/
  spawn-sub-agent.md            # orchestra skill (pi-loaded): how to request a spawn

openclaw-skills/                # OpenClaw skills (gateway-loaded)
  launch-sub-agent/
    SKILL.md                    # how to execute a sky launch + register edge
```

> **Note on skill locations.** Orchestra skills stay at
> `~/.orchestra/skills/<name>.md` (pi-loaded, unchanged). OpenClaw skills
> live at `~/.openclaw/workspace/skills/<name>/SKILL.md` (gateway-loaded).
> The repo mirrors the latter at `openclaw-skills/` and the SkyPilot setup
> script symlinks/copies them into place, same as today's
> `~/.orchestra/skills` symlink.

## 14. Phased Plan

1. **Store + collector + bridge.** Define node schema (with
   `display_name`), write `orchestra-collector.py` + `openclaw-bridge.py`.
   Wire into main-box.yaml so main-box hosts the collector + bridge, and
   the bridge queries the existing OpenClaw gateway. Collector scrapes
   main-box's own state (OpenClaw sessions + `tmux ls` for pi sessions)
   and writes the tree store. TUI still in list mode. No sub-agents yet.
2. **Tree View renderer + two-level TUI.** `tui/src/tree_view.rs` +
   `tree_layout.rs` read the store and render the spatial graph with the
   color legend (top-right). `Tab` switches between Tree View and the
   classic list view (now "Agent View" scoped to main-box). `Enter` on
   the Main Agent drops into Agent View; `Tab`/`Esc` returns. tui-use
   tests for rendering + nav + legend. No spawn yet — read-only tree.
3. **Renaming.** `n` key opens inline rename prompt, writes
   `display_name` to the tree store. Detail pane + Agent View list show
   `display_name`. tui-use test for rename flow.
4. **`/agent <name>` spawn.** TUI parses `/agent ` prefix in the dispatch
   input, sends spawn request to the OpenClaw gateway (via bridge), which
   runs `launch-sub-agent` skill (`gen-agent-yaml.py` + `sky launch` +
   register edge). TUI returns to Tree View with new node highlighted.
   `agent-template.yaml` + `launch-sub-agent` OpenClaw skill +
   `spawn-sub-agent` orchestra skill. A2A `message/send` only (no tasks).
5. **A2A tasks + SSE.** Implement `tasks/sendSubscribe` in the bridge
   (translate to `sessions.create` + `sessions.send` +
   `sessions.messages.subscribe` → SSE). Main Agent can now delegate real
   work to a sub-agent and stream progress.
6. **Session-spawns-agent.** Pi session shells out to
   `openclaw agent --message '{"intent":"spawn-agent",...}'`. End-to-end:
   a session autonomously brings up a sub-agent.
7. **Reconciliation.** `sky-bridge.py` + `sky status` reconcile loop,
   orphan/stale handling.
8. **Remote session attach.** TUI calls `sky ssh agent-<name> -- tmux
   attach -t <session>` for sessions on sub-agent boxes. `Enter` on
   remote session nodes in Tree View.
9. **Graph View (later).** Add `edges.jsonl`, second renderer, query bar.
   Data model already supports it.

## 15. Open Questions

- **Bridge language.** Python reference is fast to build (stdlib +
  `websockets` lib) but adds a Python process per agent box. Rust
  single-binary would be cleaner long term but slower to ship. Proposal:
  Python now, port to Rust if it sticks. The bridge is ~150 lines — porting
  is cheap if needed.
- **Collector HA.** Single main-box collector is an SPOF. Acceptable for a
  personal platform; flag for later if reliability matters.
- **OpenClaw skill loading.** How exactly are OpenClaw skills installed into
  the gateway? The protocol has `skills.install` — need to confirm whether
  the SkyPilot setup script should `openclaw skills install` from a path,
  or just drop files into `~/.openclaw/workspace/skills/<name>/SKILL.md`
  and let the gateway discover them. Needs a spike before phase 3.
- **OpenClaw gateway token.** The bridge needs to authenticate to the local
  gateway's WebSocket. The token is in `~/.openclaw/openclaw.json`
  (`gateway.auth.token`). Confirm the bridge can read this at startup and
  that the token doesn't rotate automatically.
- **Pi vs OpenClaw session overlap.** Today the TUI dispatches pi sessions
  (tmux + worktree). OpenClaw also has sessions (`sessions.create`). Should
  the Tree View treat pi sessions and OpenClaw sessions uniformly, or are
  they different node types? Proposal: uniform (both are "session" nodes),
  with a `kind: "pi"|"openclaw"` discriminator. The collector merges both
  into the session list per box.
