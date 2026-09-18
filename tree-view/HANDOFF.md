# Tree View — Handoff Prompt

> **Context for the next agent.** You're picking up an in-progress
> implementation of the Tree View TUI for the orchestra project — a
> personal agent orchestration platform (OpenClaw + pi + SkyPilot + any
> OpenAI-compatible LLM).
> The foundation is built and tested; the sub-agent spawn + A2A
> communication layers are stubbed and need to be wired. This document
> tells you exactly what exists, what's stubbed, and what to do next.

## 1. What this project is

**orchestra** is a personal agent orchestration platform. The Main Agent
runs on a long-lived SkyPilot box (`main-box`) as an OpenClaw gateway
(LLM brain, WS :18789) + a Rust TUI. The TUI dispatches pi coding agent
sessions (tmux + git worktree per session). The Tree View extends this
to a hierarchy: the Main Agent can spawn sub-agents (separate SkyPilot
boxes), sub-agents can spawn their own sessions + sub-agents, and
agents communicate via the A2A protocol.

**The authoritative spec** is `tree-view/Design_Document.md` — read it
before doing anything. It covers the hierarchy model, A2A protocol,
component architecture, tree state store, SkyPilot integration, the TUI
Tree View renderer, and a 9-phase plan. This handoff tells you what's
done vs. stubbed against that plan.

**Repo:** `rohansonecha/orchestra` (personal GitHub). Branch `main`.
Working dir on main-box: `~/orchestra`. Identity: Rohan Sonecha
<rohan@sonecha.com>.

## 2. Environment (read this — it matters)

- **main-box:** Long-lived SkyPilot box. Hosts the OpenClaw gateway, the
  TUI, the (new) state collector, and the (new) A2A bridge. This is where
  the user runs `orchestra`. Secrets live in `~/.orchestra/env` (loaded
  during setup, not SSH sessions).
- **devspace:** Browser dev env (code-server on a head pod). Used for
  writing code. Rust + Python available here.
- **test-box:** Ephemeral SkyPilot box launched by
  `tests/run-on-staging.sh`. **The test suite kills all tmux sessions —
  NEVER run `tests/run-all.sh` on main-box.** Always use
  `tests/run-on-staging.sh` (it launches a test-box, runs tests, tears
  it down).
- The user has a memory rule: "Never test on main-box." Respect it.

## 3. What's been built (commits `dd471ff`..`1e6bdfe`)

9 commits implementing Design Doc phases 1-3 + parts of 7-8. All pushed
to `origin/main`. All tests pass (55 Rust + 29 Python = 84 tests).

### Rust TUI (`tui/src/`)

| File | What it does | Tests |
|------|-------------|-------|
| `tree_store.rs` | Read-only client for `~/.orchestra/tree/`. Parses the versioned node schema (agents + sessions, extensible config with unknown-field passthrough via `serde_json::Map` extras). Stale detection: 90s = 3 missed 30s pulls. Corrupt JSON skipped. | 15 |
| `tree_layout.rs` | Pure `(x, y)` layout engine (Reingold-Tilford-style). Depth=x, leaves get successive y slots, internal nodes center over first/last child. No two nodes share a position. Separated from renderer so Graph View (Phase 9) can swap in force-directed later. | 10 |
| `tree_view.rs` | Renderer into `ratatui::Buffer` (no TTY needed for tests). Dots: `●` agent / `○` session, colored by effective state. Right-angle connectors (`─ │ ┐ └ ┌`). Legend top-right (always visible). Selected node ringed `[●]`, path-to-root brightened. Detail pane (toggled `d`). Nav helpers: `parent_of`, `first_child`, `nodes_at_depth`, `nearest_by_y`. | 13 |
| `command.rs` | Parses `/agent <name>` and `/rename <name>` from the dispatch input. Sanitizes names to valid cluster suffixes. | 8 |
| `main.rs` | Two-level TUI: `ViewMode::Tree` (default, spatial graph) ↔ `ViewMode::Agent` (classic session list, scoped to selected agent). `Tab` switches. Tree nav: `←→` parent/child, `↑↓` within-depth, `Enter` (agent→Agent View, session→tmux attach), `d` detail, `n` rename, `r` reload, `x` delete. `x` does full cleanup (tmux kill + worktree remove + branch delete + state file rm) with two-press confirmation in Agent View. In-memory tree synthesis from session list when the disk store is absent (so Tree View is never empty on first run). Ghost-session fix: dead tmux sessions are cleaned from state on load/exit. | 9 |
| `session.rs` | Existing session management. Modified: `load_sessions()` now skips dead tmux sessions + removes their state files; `save_sessions()` no longer re-saves Completed/Failed sessions. | (existing) |
| `worktree.rs` | Existing git worktree management. `remove_worktree()` is now called by `delete_session` (was unused before). | (existing) |

**Build:** `cd tui && cargo build --release` — zero warnings. Binary
symlinked at `/usr/local/bin/orchestra` on main-box.

### Python daemons (`tree-view/`)

All stdlib only — no `pip install` needed on main-box's miniconda3.

| File | What it does | Tests |
|------|-------------|-------|
| `orchestra-collector.py` | State Collector daemon (HTTP :7777). Scrapes main-box's own tmux + OpenClaw sessions every 30s, writes `~/.orchestra/tree/`. Pulls `GET /state` from sub-agent bridges. `sky status` reconcile every 60s (orphan `agent-*` clusters discovered→added as `unknown`; dead clusters→`failed`). Preserves `display_name` across pulls. Removed tmux sessions→`completed`. HTTP API: `POST /tree/edge`, `/tree/node`, `/tree/rename`, `/tree/refresh`; `GET /state`, `/health`. `--once` mode for testing. | 16 |
| `openclaw-bridge.py` | A2A HTTP front-end (HTTP :8080). `GET /.well-known/agent.json` (Agent Card). `GET /state` (aggregated tmux + openclaw sessions — the collector's pull target). A2A JSON-RPC (`POST /`) **stubbed** — returns `-32601 not implemented` for all `message/*` + `tasks/*` methods. `--once` mode prints `/state` JSON. | — |
| `sky-bridge.py` | `sky status --json` wrapper. Filters to `agent-*` clusters. Importable `sky_status()` + standalone CLI. | — |
| `gen-agent-yaml.py` | Renders `agent-template.yaml` from a config JSON. `{{VAR}}` substitution + `{{#COND}}` conditional blocks. Config-over-defaults merge. Per-agent token generation (`oc-` + 16 hex bytes). | 13 |

### Skills + template

| File | What it does |
|------|-------------|
| `skills/spawn-sub-agent.md` | Orchestra skill (pi-loaded via `--append-system-prompt`). Tells a pi session how to format a spawn request and shell out to `openclaw agent --message`. |
| `openclaw-skills/launch-sub-agent/SKILL.md` | OpenClaw skill (gateway-loaded). Tells OpenClaw how to execute a spawn: `gen-agent-yaml.py` → `sky launch` → `POST /tree/edge` → reply with `bridge_url`. |
| `tree-view/agent-template.yaml` | SkyPilot template for sub-agents. Based on `main-box.yaml` minus OpenClaw workspace mounts, plus bridge start, agent-token, collector registration in `run`, env vars. |

### Wiring

- `skypilot/main-box.yaml`: starts the collector (:7777) + bridge (:8080)
  as `nohup` daemons during setup, after the OpenClaw gateway starts.
- `tests/run-all.sh`: runs the Rust + Python test suites on test-box
  alongside the existing `basic.sh` + `upgrade.sh`.

### Test fixtures (`tree-view/tests/fixtures/`)

`state-main-box.json`, `state-sub-agent.json`, `sky-status.json` —
canned `/state` + `sky status` responses for future tui-use tests.

## 4. What's stubbed (needs implementation + tests)

Map to Design Doc §14 phases. Phases 1-3 done. Here's what's left:

### Phase 4 — `/agent <name>` spawn (HIGH PRIORITY — coolest demo)

**Current state:** `command.rs` parses `/agent research-box` correctly.
`main.rs` `dispatch_new()` catches it but returns a status message:
`"Spawn sub-agent '...' requires the launch-sub-agent skill + collector
(not yet wired)."`.

**What to build:**
1. When the TUI sees `/agent <name>`, send a spawn request to the
   current agent's OpenClaw gateway via the bridge (`POST /` with A2A
   `message/send` containing the spawn intent JSON).
2. The gateway (with `launch-sub-agent` skill loaded) runs
   `gen-agent-yaml.py` + `sky launch -c agent-<name>` + `POST /tree/edge`
   to the collector.
3. TUI switches back to Tree View, highlights the new agent node
   (auto-selected, `state: initializing` → `idle` once bridge reports
   ready).

**The skill docs already exist** (`skills/spawn-sub-agent.md` +
`openclaw-skills/launch-sub-agent/SKILL.md`) — they describe the exact
steps. The template + generator are tested. What's missing is the TUI
→ bridge → gateway → sky launch wiring.

**Blocker to confirm first (Design §15 open question):** how does
OpenClaw load skills? The SkyPilot setup script needs to either
`openclaw skills install` from a path or drop files into
`~/.openclaw/workspace/skills/<name>/SKILL.md`. Spike this before
building the full spawn flow — if the gateway can't load the skill,
the spawn request has nothing to execute it.

**Tests:** the A2A bridge's `POST /` handler needs to actually translate
`message/send` to the OpenClaw gateway's WebSocket (currently returns
`-32601`). Write a test that mocks the gateway WS and asserts the bridge
forwards correctly.

### Phase 5 — A2A tasks + SSE (the "thinking together" mode)

**Current state:** `openclaw-bridge.py` `do_POST()` returns
`-32601 not implemented` for all `message/*` + `tasks/*` methods.

**What to build:**
1. Bridge opens a persistent WS connection to `ws://localhost:18789`
   (the local OpenClaw gateway), authenticated with the token from
   `~/.openclaw/openclaw.json` (`gateway.auth.token`).
2. `message/send` → translate to OpenClaw `sessions.send` (or
   `chat.send`).
3. `tasks/send` → `sessions.create` + `sessions.send`.
4. `tasks/get` → `sessions.describe` + `chat.history`.
5. `tasks/cancel` → `sessions.abort`.
6. `tasks/sendSubscribe` → `sessions.create` + `sessions.send` +
   `sessions.messages.subscribe` → pump `session.message` /
   `session.tool` WS events into the HTTP SSE response as
   `TaskStatusUpdate` events.

**Blocker to confirm first (Design §15):** confirm the bridge can read
the gateway token from `~/.openclaw/openclaw.json` at startup and that
the token doesn't rotate automatically. Spike the WS connection before
building all the method translations.

**Tests:** mock the OpenClaw gateway WS with a stdlib socket server,
assert the bridge translates A2A → WS correctly for each method. Test
the SSE streaming by asserting event sequence on a canned gateway
response.

### Phase 6 — Session-spawns-agent

**Current state:** the orchestra skill (`spawn-sub-agent.md`) tells pi
how to shell out to `openclaw agent --message`. The OpenClaw skill
(`launch-sub-agent/SKILL.md`) documents the execution steps.

**What to build:** end-to-end — a pi session autonomously calls
`openclaw agent --message '{"intent":"spawn-agent",...}'`, the parent
gateway runs the launch, the sub-agent appears in the tree. This is
mostly wiring Phase 4 + confirming the gateway actually invokes the
loaded skill on receipt of the spawn intent.

**Tests:** integration test (manual / nightly — slow, launches a real
SkyPilot box). Not in the fast loop.

### Phase 7 — Reconciliation (PARTIALLY DONE)

**Current state:** `reconcile_sky()` in `orchestra-collector.py` is
implemented + tested — orphan discovery, dead-cluster→failed. The
collector runs `sky status` every 60s.

**What's left:**
- Verify `sky status --json` actually parses on a real main-box (the
  JSON shape may differ from the fixture — `sky-bridge.py` guesses at
  field names `name`/`cluster`, `host`/`hostname`). Test against real
  `sky status` output on main-box.
- Stale handling: the TUI already marks nodes stale (90s = 3 missed
  pulls), but verify the collector's `last_pulled` semantics match
  when bridges are unreachable.

### Phase 8 — Remote session attach

**Current state:** Tree View `Enter` on a session node calls
`attach_to_session()` which runs `tmux attach -t <name>`. This works
for sessions on main-box. For sessions on sub-agent boxes, it does
nothing useful (the tmux session is on a different box).

**What to build:** for a session node whose `box_host` is not
main-box, run `sky ssh <box_host> -- tmux attach -t <tmux_session>`.
Detect this in `enter_node()` — if `node.raw.box_host` is set and != main-box,
use `sky ssh` instead of `tmux attach`.

**Tests:** the decision logic (local vs remote) is unit-testable. The
actual `sky ssh` attach is a manual integration test.

### Phase 9 — Graph View (LATER)

**Current state:** data model is already graph-ready. `Node` has
`parent_id` (degenerate graph, ≤1 inbound edge). The Design Doc §11
describes adding `edges.jsonl` for interaction edges (A2A `tasks/send`
from A to B = edge `A → B kind=delegated`).

**What to build:** a second renderer mode that lays out nodes by
force-directed / layered algorithm instead of indent. A query bar.
Edge coloring by kind. The bridge is the natural instrumentation point
— log every A2A call as an edge. Don't start this until Phases 4-5
are done (no A2A traffic = no interaction edges to show).

## 5. How to run things

### Build + run the TUI (on main-box)

```bash
cd ~/orchestra && git pull origin main
cd tui && source ~/.cargo/env && cargo build --release
orchestra                    # launches into Tree View (new default)
```

### Run the collector (on main-box, for live state)

```bash
python3 ~/orchestra/tree-view/orchestra-collector.py &
# or with --once for a single pull cycle (testing)
python3 ~/orchestra/tree-view/orchestra-collector.py --once
```

### Run the bridge (on main-box, for A2A)

```bash
python3 ~/orchestra/tree-view/openclaw-bridge.py &
python3 ~/orchestra/tree-view/openclaw-bridge.py --once   # prints /state JSON
```

### Run tests

```bash
# Rust unit tests (safe anywhere, no tmux impact)
cd ~/orchestra/tui && cargo test

# Python tests (safe anywhere)
cd ~/orchestra/tree-view && python3 tests/test_collector.py
cd ~/orchestra/tree-view && python3 tests/test_gen_yaml.py

# Full suite (MUST run on test-box — kills tmux)
bash ~/orchestra/tests/run-on-staging.sh
```

### TUI keybindings (Tree View)

`←→` parent/child · `↑↓` within-depth · `Enter` enter node · `Tab` Agent View ·
`d` detail pane · `n` rename · `r` reload · `x` delete · `q` quit

### TUI keybindings (Agent View)

`↑↓` navigate · `Enter` dispatch or attach · `→` attach · `x` delete (two-press) ·
`Tab`/`Esc` Tree View · `q` quit

## 6. Conventions + gotchas

- **Git identity:** `Rohan Sonecha <rohan@sonecha.com>` (personal repo).
  The user has a memory rule about per-repo identity — check
  `~/.claude/projects/-/memory/user_github_identity.md` if in doubt.
- **Public repo hygiene:** this is a personal repo but the user keeps
  work-specific config/secrets out of it (`private/` is gitignored).
  Never commit tokens, kubeconfigs, or customer URLs.
- **Progressive commits:** the user wants one commit per logical step,
  not giant batches. Each commit should leave the tree in a working
  state. Modular code, tests alongside.
- **The TUI never writes state except user-initiated commands** (spawn,
  teardown, rename — all go through the collector). The collector is the
  only writer of `~/.orchestra/tree/`. Exception: `rename_node()` in
  `main.rs` writes `display_name` directly to the node JSON — the
  collector preserves it across pulls.
- **Schema versioning:** `SCHEMA_VERSION = 1` in `tree_store.rs`. Additive
  changes (new optional fields) don't require a bump — serde ignores
  unknown fields, and `Config.extras` (a `serde_json::Map`) passes
  unknown config fields through so an agent can read its own config at
  runtime.
- **Naming convention:** agents are `agent-<name>` (SkyPilot cluster
  prefix — the collector discovers them via `sky status` by this prefix).
  Sessions are `session-<tmux-name>`.
- **Ghost sessions bug (fixed in `1e6bdfe`):** the TUI used to re-save
  dead sessions on exit, recreating stale state files. `load_sessions()`
  now checks `tmux has-session` and cleans up dead sessions. If you see
  ghost sessions again, check whether `save_sessions()` is being called
  on a path that includes Completed/Failed sessions.

## 7. Suggested next steps (priority order)

1. **Spike OpenClaw skill loading** (Design §15 open question). Confirm
   `openclaw skills install` vs. file-drop into
   `~/.openclaw/workspace/skills/<name>/SKILL.md`. Blocks Phase 4.
2. **Wire Phase 4 (`/agent` spawn).** TUI → bridge → gateway →
   `gen-agent-yaml.py` → `sky launch` → `POST /tree/edge`. This is the
   "cool demo" — give the Main Agent a multi-part task and watch the
   tree grow.
3. **Wire Phase 5 (A2A tasks + SSE).** Bridge WS connection to the
   gateway, translate `tasks/sendSubscribe` → SSE. Enables inter-agent
   delegation streaming.
4. **Verify `sky status --json` parsing** against real main-box output.
   The field names in `sky-bridge.py` are guesses.
5. **Phase 8 (remote session attach)** — `sky ssh` for sessions on
   sub-agent boxes. Quick win once the tree has real sub-agents.

Start by reading `tree-view/Design_Document.md` end-to-end, then
`tree-view/HANDOFF.md` (this file), then the source in dependency order:
`tree_store.rs` → `tree_layout.rs` → `tree_view.rs` → `main.rs` →
`orchestra-collector.py` → `openclaw-bridge.py`.
