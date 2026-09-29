# orchestra

Personal agent orchestration platform. Manages parallel coding agent sessions
with a terminal UI, git worktree isolation, and OpenClaw as the gateway.

## Current functionality

- **TUI** (`orchestra`) — terminal UI for dispatching and navigating
  parallel coding agent sessions (pi, Claude Code, or Codex), each in its own
  git worktree. It can also import your existing Claude Code and Codex
  sessions.
- **Main box** — long-lived VM running OpenClaw + pi + the TUI, launched via
  SkyPilot.
- **Work agent** — OpenClaw agent persona for work tasks, with its own workspace
  and identity.

## User stories

### Dispatch a coding task
```
$ cd ~/code/my-repo
$ orchestra
```
Type a prompt in the dispatch input, press Enter. A new session starts in its
own git worktree of the repo you launched from, branched off the latest
`origin/<default-branch>`. The agent receives the prompt and begins working.

By default the agent is pi. To pick another one for a single session, prefix
the prompt:

- `/claude <prompt>` runs Claude Code
- `/codex <prompt>` runs Codex
- `/pi <prompt>` runs pi

`/backend claude` (or `codex`, `pi`) changes the default for later prompts.
`/model <name>` sets the model new sessions of the default backend start
with, `/model` shows it, and `/model -` goes back to the backend's own
default. For pi, the name is checked against `pi --list-models`, so it can be
any model from pi's built-in providers or from a custom provider in
`~/.pi/agent/models.json` (see [Model config](#model-config)). Both settings
are saved in `~/.orchestra/config.json`.

### Navigate between sessions
orchestra opens on its session list, styled after Claude Code's agents view:
your orchestra sessions, with what each is doing and its latest reply, then
your Claude Code and Codex sessions for this repo that are not in orchestra
yet. `Tab` switches to Tree View and back.

- `↑` / `↓` — move through the list
- `Enter` — open the selected session, or adopt a Claude Code / Codex one
- `←` inside a session (on an empty prompt) — come back to the list
- `s` — switch the selected session to another agent or model (a picker, like
  Claude Code's `/model`; number keys pick directly, typing filters)
- `p` — fork a Claude Code / Codex session into pi
- `a` — show Claude Code / Codex sessions from all directories
- `x` twice — delete the selected session
- `/` — commands, with completions above the prompt (`Tab` completes)
- `?` — shortcuts
- `q` (on an empty prompt) or `Ctrl+C` — quit

### Work in parallel
Worktrees follow the same layout Claude Code uses for `.claude/worktrees/`:

```
<repo>/.orchestra/worktrees/<name>/    branch worktree-<name>
```

- The repo is the git repo containing the directory you run `orchestra` from.
  Running it from inside a worktree still uses the main checkout. If the
  directory is not in a git repo, orchestra uses `ORCHESTRA_WORK_REPO` from the
  environment or `~/.orchestra/env`. The main box sets this to its work repo
  because you land in `~` after `sky ssh`. With neither, sessions run in the
  launch directory without a worktree.
- New worktrees branch from `origin/<default-branch>` (the remote's HEAD, so
  `main` or `master` is detected, not assumed) after a `git fetch`. Your main
  checkout is never pulled or switched, so it can be dirty or on a feature
  branch.
- `.orchestra/` is added to the repo's `.git/info/exclude`, so worktrees never
  show up in `git status` and no tracked file is changed.
- Gitignored files listed in a `.worktreeinclude` file at the repo root (for
  example `.env`) are copied into each new worktree. This uses the same file
  format as Claude Code.
- Deleting a session removes its worktree and branch. The confirmation message
  says first if the worktree has uncommitted files or commits that are not on
  the base branch.

### Import Claude Code and Codex sessions
The list shows your Claude Code sessions
(`~/.claude/projects/`) and Codex sessions (`~/.codex/sessions/`) whose
directory is inside the current repo, including their worktrees. Press `a` to
show sessions from every directory.

- `Enter` resumes the session with its own CLI (`claude --resume <id>` or
  `codex resume <id>`) in its original directory, inside an orchestra tmux
  session. A `●` marks sessions that are already open in orchestra; `Enter` on
  one of those attaches to it.
- `p` forks the conversation into a new pi session, so you can continue it
  with any model pi supports. The fork gets its own worktree, starting from the
  commit the original session is on (uncommitted edits do not carry over). Tool
  calls and their output are kept as text, capped in length, and history from
  before the last compaction is left out, which matches what the original agent
  itself would see.

Deleting an imported session only stops its tmux session. Its directory and
its Claude Code or Codex transcript are left alone.

### Switch a session to another agent or model
Select a session in Agent View and type `/switch <target>`. The session keeps
its name, worktree and conversation; only the agent running it changes.

- `/switch claude` or `/switch codex` moves it to Claude Code or Codex
  (`/switch claude:opus` picks a model).
- `/switch pi:<provider>/<model>`, or just a model name such as
  `/switch GLM-5.3`, moves it to pi on that model. Any model pi knows works,
  including custom providers in `~/.pi/agent/models.json`.
- Switching models within the same agent restarts it on its own transcript
  with the new model, so nothing is converted.

When the agent changes, orchestra rewrites the conversation as the new
agent's own session file and resumes it there:

- Shell, file read, file write and file edit calls (nearly all tool use) are
  kept as real tool calls, with names and arguments mapped to the new agent's
  tools. Other tools, such as MCP tools or Codex's JavaScript cells, are kept
  as text describing the call. Codex receives everything as text.
- If the session ran in the new agent before, that agent gets its own
  original transcript back, with only the turns since then added. So switching
  Claude Code → pi → Claude Code loses nothing from the Claude Code part.
- Long tool output is shortened, and a short note tells the model the
  conversation was moved and that it should re-read files before editing.

### Resume after disconnect
Session state is persisted to `~/.orchestra/sessions/<name>/state.json`. On TUI
restart, previous sessions appear in the list (marked as Idle). Attach to resume
the conversation where you left off. If the agent process exits, it is
restarted in the same conversation: pi with `--continue` on the session's own
conversation directory (`~/.orchestra/pi-sessions/<id>/`), Claude Code with
`--resume`, and Codex with `resume`.

### Rename a session
Renaming moves the session's *whole* identity, not just its label:

- **Tree View:** select the session and press `n`, type a name, Enter.
- **Dispatch input:** `/rename My Session` (renames the node currently
  selected in Tree View).
- **CLI:** `orchestra rename <old-name> <new-name>`

A rename updates the tmux session, the git worktree dir + branch, the state
dir (`~/.orchestra/sessions/`), and the tree-store node id — so the
collector keeps matching the node instead of creating a duplicate. The label
keeps your raw text (`My Session`) while the underlying name is sanitized
(`my-session`). The pi conversation is stored under the session's id, which
never changes, so it is unaffected. Imported sessions keep their directory;
only orchestra's own names change.

## Architecture

```
You ──SSH──▶  MAIN BOX (sky launch main-box.yaml)
                orchestra (ratatui.rs terminal app)
                  · dispatch input → new session per prompt
                  · session list with state icons
                  · attach → full-screen agent (pi / claude / codex)
                  · per-session git worktree off origin/<default>
                  · import Claude Code / Codex sessions
                      │
                      ▼
                WORK REPO (git clone, $ORCHESTRA_WORK_REPO)
                  ~/work-repos/prototype/
                  └── .orchestra/worktrees/<name>/  (branch: worktree-<name>)

                coding agent (per session, in tmux)
                  · pi: built-in providers + models.json custom providers
                  · Claude Code / Codex: their own CLIs and logins

                OpenClaw gateway (daemon, persistent)
                  · model = configured in openclaw.json
                  · agents: main (default), work
```

## Components

| Component | Role |
|-----------|------|
| [pi](https://pi.dev) | Default coding agent harness; runs per-session in a worktree |
| [Claude Code](https://claude.com/claude-code), [Codex](https://github.com/openai/codex) | Optional session backends (`/claude`, `/codex`), used through their own CLIs |
| [OpenClaw](https://openclaw.ai) | Gateway + agent personas + skills |
| [SkyPilot](https://skypilot.co) | Compute orchestration for the main box |
| [ratatui.rs](https://ratatui.rs) | Terminal UI framework |
| Your LLM provider | Any OpenAI-compatible API (vLLM, OpenRouter, OpenAI, token proxies, …) |

## Install

One command — installs deps (Node, Rust, tmux, pi, OpenClaw, SkyPilot CLI),
clones the repo to `~/orchestra`, builds the TUI, and puts `orchestra` on your
PATH:

```bash
curl -fsSL https://raw.githubusercontent.com/rohansonecha/orchestra/main/install.sh | bash
```

Re-running the same command upgrades everything (git pull + rebuild). Existing
config in `private/` and `~/.orchestra` is never overwritten.

## Setup

### 1. Configure private values

The installer creates `~/orchestra/private/` from the templates. Fill in real
values:

```bash
cd ~/orchestra
$EDITOR private/env                                        # endpoints + tokens
cp pi/models.json.example private/models.json              # fill in real endpoint
cp private.example/openclaw/openclaw.json private/openclaw/openclaw.json  # fill in real values
```

### 2. Required secrets

Store these as secrets on your SkyPilot API server (resolved at launch time via
the `secrets:` field in `main-box.yaml`):

| Secret | Purpose |
|--------|---------|
| `ORCHESTRA_API_KEY` | API key for your model provider |
| `SKY_API_ENDPOINT` | SkyPilot API server URL |
| `ORCHESTRA_SKY_API_TOKEN` | SkyPilot API server token (orchestra service account) |
| `GIT_EMAIL` | Git commit email |
| `GIT_NAME` | Git commit name |
| `GITHUB_TOKEN` | GitHub PAT (for cloning private repos) |
| `LINEAR_TOKEN` | Linear API token |

### 3. Required local env (in `private/env`)

| Variable | Purpose |
|----------|---------|
| `SKY_INFRA` | SkyPilot infra to launch on (e.g. `k8s/your-context`) |
| `SKY_API_ENDPOINT` | SkyPilot API server URL (also a secret, but needed locally for `sky api login`) |
| `ORCHESTRA_SKY_API_TOKEN` | SkyPilot API server token (also a secret, but needed locally for `sky api login`) |

### 4. Launch the main box

```bash
source private/env
sky launch --infra "$SKY_INFRA" -c main-box skypilot/main-box.yaml
```

### 5. SSH in and run the TUI

```bash
sky ssh main-box
orchestra
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
│       ├── session.rs              # Session state, agent launch commands, persistence
│       ├── repo.rs                 # Find the launch repo and its default branch
│       ├── worktree.rs             # Git worktree create/remove, .worktreeinclude
│       ├── import.rs               # Find Claude Code / Codex sessions on disk
│       ├── convert.rs              # Convert a transcript into a pi session
│       └── config.rs               # /backend and /model defaults
├── pi/
│   └── models.json.example         # Generic OpenAI-compatible provider template
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

Orchestra is model-agnostic: pi talks to any **OpenAI-compatible API**
(`openai-completions`) — vLLM, OpenRouter, OpenAI, a token proxy, etc.
The OpenAI-compatible API is used rather than the Anthropic API because most
providers validate `Authorization: Bearer` and ignore `x-api-key`; the OpenAI
client sends Bearer natively. See `pi/models.json.example`.

**Multiple models:** list as many models as you want under the provider in
`models.json`. pi also has built-in providers (Anthropic, OpenAI, OpenRouter,
Gemini, and others) that work with their usual API key env vars or pi's
`/login`. A new pi session starts on, in order: the model set with orchestra's
`/model`, else `ORCHESTRA_PROVIDER` / `ORCHESTRA_MODEL` from
`~/.orchestra/env`, else pi's own default. You can switch anytime with pi's
`/model` command inside the session, and the switch survives pi restarts.
`models.json` reloads every time you open `/model`, so edits on the box take
effect immediately.

The API key is read from the `ORCHESTRA_API_KEY` env var using the `!printf`
command syntax: `"apiKey": "!printf %s $ORCHESTRA_API_KEY"`. This avoids
hardcoding the key in config files and lets secrets-manager inject it at
launch time. (`GLM_API_KEY` is accepted as a legacy fallback.)

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
- **Todo skill** — maintain work and personal todo lists
- **Context routing** — automatically route work vs personal queries to the
  right agent

## License

MIT
