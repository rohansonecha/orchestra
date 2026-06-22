# orchestra

Personal agent orchestration platform. Manages parallel coding agent sessions
with a terminal UI, git worktree isolation, and OpenClaw as the gateway.

## Current functionality

- **TUI** (`orchestra-tui`) — terminal UI for dispatching and navigating
  parallel pi coding agent sessions, with automatic git worktree isolation per
  session.
- **Main box** — long-lived VM running OpenClaw + pi + the TUI, launched via
  SkyPilot.
- **Work agent** — OpenClaw agent persona for work tasks, with its own workspace
  and identity.

## User stories

### Dispatch a coding task
```
$ orchestra-tui
```
Type a prompt in the dispatch input, press Enter. A new session starts in its
own git worktree (branched off latest master of your work repo). Pi receives the
prompt and begins working.

### Navigate between sessions
- `↑` / `↓` — move between sessions in the list
- `→` or `Enter` (on empty input) — attach to the selected session (full-screen
  pi interactive mode)
- `←` (on empty input) — detach back to the session list
- `q` or `Ctrl+C` — quit

### Work in parallel
Each session runs in its own git worktree at `~/orchestra/worktrees/<name>/` on
branch `worktree-<name>`. Multiple sessions can work simultaneously without
conflicting with each other or with your main checkout.

### Resume after disconnect
Session state is persisted to `~/.orchestra/sessions/<name>/state.json`. On TUI
restart, previous sessions appear in the list (marked as Idle). Attach to resume
the pi conversation where you left off.

## Architecture

```
You ──SSH──▶  MAIN BOX (sky launch main-box.yaml)
                orchestra-tui (ratatui.rs terminal app)
                  · dispatch input → new session per prompt
                  · session list with state icons
                  · attach → full-screen pi interactive
                  · per-session git worktree off origin/master
                      │
                      ▼
                WORK REPO (git clone)
                  ~/work-repos/prototype/
                  └── worktrees/<session-name>/  (branch: worktree-<name>)

                pi coding agent (per session)
                  · models.json → GLM endpoint (openai-completions)
                  · session saved by name for multi-turn continuity

                OpenClaw gateway (daemon, persistent)
                  · model = GLM via openclaw.json
                  · agents: main (default), work
```

## Components

| Component | Role |
|-----------|------|
| [pi](https://pi.dev) | Coding agent harness; runs per-session in a worktree |
| [OpenClaw](https://openclaw.ai) | Gateway + agent personas + skills |
| [SkyPilot](https://skypilot.co) | Compute orchestration for the main box |
| [ratatui.rs](https://ratatui.rs) | Terminal UI framework |
| GLM 5.2 (vLLM) | Self-hosted LLM; serves OpenAI-compatible API |

## Setup

### 1. Clone and configure private values

```bash
git clone https://github.com/rohansonecha/orchestra.git
cd orchestra
mkdir private
cp pi/models.json.example private/models.json            # fill in real endpoint
cp private.example/openclaw/openclaw.json private/openclaw/openclaw.json  # fill in real values
# Edit private/ files with your real endpoint URLs, tokens, and work repo path
```

### 2. Required secrets

Store these as secrets on your SkyPilot API server (resolved at launch time via
the `secrets:` field in `main-box.yaml`):

| Secret | Purpose |
|--------|---------|
| `GLM_API_KEY` | vLLM bearer token for GLM |
| `SKY_API_ENDPOINT` | SkyPilot API server URL |
| `SKY_API_TOKEN` | SkyPilot API server token |
| `GIT_EMAIL` | Git commit email |
| `GIT_NAME` | Git commit name |
| `GITHUB_TOKEN` | GitHub PAT (for cloning private repos) |
| `LINEAR_TOKEN` | Linear API token |

### 3. Required local env (in `private/env`)

| Variable | Purpose |
|----------|---------|
| `SKY_INFRA` | SkyPilot infra to launch on (e.g. `k8s/your-context`) |
| `SKY_API_ENDPOINT` | SkyPilot API server URL (also a secret, but needed locally for `sky api login`) |
| `SKY_API_TOKEN` | SkyPilot API server token (also a secret, but needed locally for `sky api login`) |

### 4. Launch the main box

```bash
source private/env
sky launch --infra "$SKY_INFRA" -c main-box skypilot/main-box.yaml
```

### 5. SSH in and run the TUI

```bash
sky ssh main-box
orchestra-tui
```

## Repository layout

```
orchestra/
├── skypilot/
│   └── main-box.yaml               # Long-lived orchestrator box
├── tui/
│   ├── Cargo.toml                  # Rust dependencies
│   └── src/
│       ├── main.rs                 # TUI entrypoint, key handling, rendering
│       ├── session.rs              # Session state, pi spawn, persistence
│       └── worktree.rs             # Git worktree create/remove
├── pi/
│   └── models.json.example         # GLM/vLLM provider config template
├── openclaw/
│   └── skills/                    # OpenClaw custom skills (add your own)
├── private.example/                # Parameterized templates (committed)
│   └── openclaw/
│       ├── openclaw.json           # OpenClaw config template
│       └── workspace/              # Personality files (main agent)
│       └── workspace-work/         # Personality files (work agent)
└── private/                        # gitignored — real values
```

## Model config

pi drives GLM via the **OpenAI-compatible API** (`openai-completions`), not the
Anthropic API. This is because vLLM validates `Authorization: Bearer` and ignores
`x-api-key`; the OpenAI client sends Bearer natively, the Anthropic client sends
`x-api-key`. See `pi/models.json.example`.

The API key is read from the `GLM_API_KEY` env var using the `!printf` command
syntax: `"apiKey": "!printf %s $GLM_API_KEY"`. This avoids hardcoding the key
in config files and lets secrets-manager inject it at launch time.

## OpenClaw agents

Two agents are defined in `openclaw.json`:

- **`main`** (default) — general-purpose, personal tasks
- **`work`** — work-specific, with the `sky-coding-agent` skill and a separate
  workspace at `~/.openclaw/workspace-work/`

Each agent has its own identity (name, emoji), workspace directory, and skill
list. Routing rules can map channels (Slack, Telegram, etc.) to specific agents
via the `bindings` field — see [OpenClaw agent config docs](https://docs.openclaw.ai/gateway/config-agents).

## Future directions

- **Entrypoints** — Slack/Telegram integration so you can dispatch from mobile
  without SSHing into the main box
- **Session summaries** — one-line activity summary per session, refreshed
  periodically (like `claude agents`)
- **Session peek** — preview a session's recent output without attaching
- **Worktree cleanup** — auto-remove worktrees when sessions complete
- **Todo skill** — maintain work and personal todo lists
- **Context routing** — automatically route work vs personal queries to the
  right agent

## License

MIT
