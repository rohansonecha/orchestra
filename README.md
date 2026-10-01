# orchestra

orchestra is an agent orchestration platform for coding agents. It runs Claude Code, Codex and pi sessions side by side from one terminal UI, gives each session its own git worktree, and moves work between them. You can adopt a session you started in Claude Code, switch a running session to Codex or to any model pi supports, fork it, or get a second opinion from a different model, without losing the conversation or the tool calls in it.

## What it does

- **One list for every agent.** Your orchestra sessions, plus the Claude Code, Codex and pi sessions you started elsewhere, in a list styled after Claude Code's agents view: what each one is doing, its latest reply, its agent and model, and how long ago it was active.
- **Cross-harness interoperability.** Move a session between Claude Code, Codex and pi, or between models within one of them. The conversation is rewritten into the target agent's own session format, with shell, read, write and edit tool calls carried over as real tool calls, and the target agent resumes it natively.
- **Any model through pi.** Built-in providers (Anthropic, OpenAI, OpenRouter, Gemini and more) and any OpenAI-compatible endpoint you add to `~/.pi/agent/models.json`.
- **Isolation.** Each session works in its own git worktree and branch, so sessions running in parallel never touch each other's files or your main checkout.
- **Session commands that work with any agent:** a read-only review by a different model, side questions that don't interrupt the session, recaps, recurring prompts, branching a session, and more.
- **Light on the machine.** Sessions nobody is using are stopped after 30 minutes and resume on Enter with their conversation intact.

## Quick start

```bash
curl -fsSL https://raw.githubusercontent.com/rohansonecha/orchestra/main/install.sh | bash
cd ~/code/my-repo
orchestra
```

Type what you want done and press Enter. A pi session starts in a fresh worktree of `my-repo` (start the prompt with `/claude` or `/codex` to use those agents instead). Press Enter on the session to watch it work, and press ← on an empty prompt to come back to the list.

## A walkthrough

### 1. Start orchestra in a repo

Run `orchestra` inside any git repo. It opens on the session list:

```
 orchestra  ~/code/shop-api
 1 working · 2 ready · new sessions get a worktree off origin/main

 enter opens · ← inside a session comes back here · ctrl+s switches agent/model · ? for shortcuts

 Sessions
 ● Fix flaky checkout test        Working · Running the checkout suite again with -count=5    pi · GLM-5.3      2m
 ✻ Add rate limiting to /login    Ready · Added a token bucket per IP and tests; ready for…   Claude Code      14m
 ✻ Review: Add rate limiting      Ready · 2 findings: the bucket map is never pruned, and…    Codex             9m

 ▾ Claude Code · not in orchestra — enter adopts, ctrl+f forks into pi
 ∙ Migrate payments to v2 API     Done with the client; the webhook handler still uses v1…    ⎇ payments-v2     1d

────────────────────────────────────────────────────────────────────────────────────────────────
❯ describe a task for a new session
────────────────────────────────────────────────────────────────────────────────────────────────
  ⏵ pi (skypilot-tokens/zai-org/GLM-5.3) · / for commands · ? for shortcuts
```

The first group is your orchestra sessions. Below it are the Claude Code, Codex and pi sessions you started outside orchestra in this repo.

### 2. Start a task

Type a prompt and press Enter. orchestra creates a worktree at `.orchestra/worktrees/<name>` on a new branch off the latest `origin/main`, starts the agent there in its own tmux session, and gives it your prompt. The new row shows **Starting**, then **Working** while the agent runs, and **Ready** when it is waiting for you.

- `/claude <prompt>` or `/codex <prompt>` starts that agent for one task; `/backend codex` makes it the default.
- `/model` opens a picker for the default agent and model of new sessions.
- A multi-line paste stays in the prompt as a `[Pasted text #1 +12 lines]` token and is sent in full.

### 3. Watch and steer a session

Press Enter on a session to open it full screen. It looks exactly like the agent on its own, and you talk to it as usual. Press ← on an empty prompt to come back to the list; the session keeps running. Scroll with the mouse wheel, and start typing to jump back to the prompt.

### 4. Pick up work from another agent

Select a session under "Claude Code · not in orchestra" (or Codex, or pi) and press Enter. It resumes in its own agent and its own directory, and is now one of your sessions. Press Ctrl+F instead to fork a copy into pi, in a new worktree started from the commit the original is on. You continue the same conversation on another model while the original stays untouched.

### 5. Move a session to another agent or model

Select a session and press Ctrl+S:

```
  Switch Add rate limiting to /login
  Moves this conversation to another agent or model. History and tool calls carry over.
  ❯ 1. Claude Code ✔                          its default model
    2. Claude Code · opus                     Anthropic's most capable everyday model
    3. Claude Code · sonnet                   faster, cheaper
    4. Codex                                  its default model
    5. pi · deepseek-ai/DeepSeek-V4.1-Flash    skypilot-tokens · 1.0M context
    6. pi · zai-org/GLM-5.3                    skypilot-tokens · 1.0M context
```

Pick one (number keys work, and typing filters the list). The row shows **Switching** for a moment, then the session continues in the new agent with its whole conversation: what you asked, what the agent did, which commands it ran and what they printed. If you later switch back, the first agent gets its own original transcript back, plus only the turns that happened since.

### 6. Ask without interrupting, or get a second opinion

- `/btw what did the last test run print?` answers from a copy of the session's conversation and leaves the session alone.
- `/recap` gives a one-line summary of where a session stands.
- `/code-review` starts a separate, read-only reviewer on the session's worktree using your default agent. Codex or a pi model can review work Claude Code did, or the other way round.

### 7. Branch, repeat, clean up

- `/branch` forks a session into a new one: a copy of its conversation, and a new worktree with its uncommitted work, so you can try another approach.
- `/loop 10m check CI and fix failures` resends a prompt whenever the session is idle; `/loop stop` ends it.
- Ctrl+P pins a session, yours or one from outside orchestra, to a Pinned group at the top of the list; Ctrl+P again unpins it. An outside session stays pinned after you adopt it.
- Ctrl+R renames a session. Ctrl+X twice deletes it along with its worktree and branch; you are told first if it has uncommitted work.
- Sessions you haven't used for 30 minutes are stopped to free memory and CPU. They stay in the list as **Stopped**, and Enter resumes them where they left off.

## How cross-harness interoperability works

Each session runs one agent through that agent's own CLI (`pi`, `claude` or `codex`), with its own login and provider. orchestra never sits between an agent and its model. Each agent keeps its own transcript, and that file is the source of truth.

- **Changing the model within one agent** restarts the agent on its own transcript with the new model (`pi --continue --model …`, `claude --resume <id> --model …`, `codex resume <id> -m …`). Nothing is converted, because each agent knows how to replay its own history to another model.
- **Moving to another agent** converts the conversation. orchestra reads the transcript into one neutral form and maps the four tools every agent has (run a shell command, read, write and edit a file) to the target's own tool names and arguments. Every other tool call is kept as text describing the call. It then makes sure every call has its result and that turns alternate correctly, fits the history to the target model's context window, writes it in the target's own session format, and resumes it there. Codex receives text only, because its tool calls are small JavaScript programs.
- **Going back** to an agent the session used before restores that agent's own transcript and appends only what happened since, so a session that moves back and forth doesn't degrade.

| Carries over | Doesn't carry over |
|---|---|
| Every message, in order | The model's hidden reasoning (signed or encrypted by each provider) |
| Shell, read, write and edit calls, as real tool calls | Codex's compaction summaries (encrypted) |
| Every other tool call, as text | The middle of very long tool output (the start and end are kept) |
| Tool output | Images (replaced by a placeholder) |
| Claude Code's compaction summaries | |

[docs/switching-agents.md](docs/switching-agents.md) explains each of these, why it is hard, and how it is handled. `orchestra roi` measures it on your own transcripts: the share of tool calls a switch keeps as real tool calls, by week and by agent, what each of your switches carried, and the most common calls that fall back to text.

## Reference

### Keys

| Key | Action |
|---|---|
| ↑ / ↓ | Move through the list |
| Enter | Open the selected session, adopt an outside one, or resume a stopped one |
| ← (inside a session, on an empty prompt) | Back to the list |
| Ctrl+S | Switch the selected session to another agent or model |
| Ctrl+F | Fork a Claude Code or Codex session into pi |
| Ctrl+P | Pin the selected session to the Pinned group at the top, or unpin it |
| Ctrl+R | Rename the selected session |
| Ctrl+X twice | Delete the selected session |
| ← / → on a group heading | Collapse or expand the group (remembered) |
| / | Commands, with completions (Tab completes) |
| ? on an empty prompt | Shortcuts |
| Esc | Clear the prompt |
| Tab | Tree view |
| Ctrl+C | Quit |

Letters always go into the prompt; shortcuts use Ctrl. The prompt has the usual editing keys: Option+Delete, Ctrl+W or Ctrl+Backspace delete a word; Cmd+Delete or Ctrl+U delete to the start of the line; Ctrl+K deletes to the end; Ctrl+A / Ctrl+E or Home / End jump; Option or Ctrl with ← / → move by word.

### Commands

| Command | What it does |
|---|---|
| `/claude`, `/codex`, `/pi <prompt>` | Start a session on that agent |
| `/background <prompt>` | Start a session without opening it |
| `/backend <pi\|claude\|codex>` | Default agent for new sessions |
| `/model [name]` | Default model for new sessions (no name opens the picker; `-` resets) |
| `/switch [target]` | Move the selected session to another agent or model (no target opens the picker). Targets look like `claude`, `claude:opus`, `codex`, `pi:<provider>/<model>` or a pi model name |
| `/code-review [target]` | A separate, read-only reviewer on the selected session's worktree, using your default agent |
| `/simplify` | Ask the session to clean up its own changes and apply the fixes |
| `/autofix-pr [pr]` | Ask the session to fix its PR's failing checks and review comments, then push |
| `/loop [5m] <prompt>` | Resend a prompt whenever the session is idle (`↻` in the list); `/loop stop` ends it |
| `/branch [name]` | Fork the session: its conversation, and a new worktree with its uncommitted work |
| `/btw <question>` | A side question answered from a copy of the session's conversation |
| `/recap` | A one-line recap of the session |
| `/rename <name>` | Rename the selected session |
| `/suspend` | Stop the selected session now to free resources; Enter resumes it |
| `/import`, `/import all` | Jump to outside sessions; show them from every directory |
| `/theme <light\|dark>` | Colors for orchestra and new pi sessions |
| `/bug <what went wrong>` | Draft a GitHub issue for orchestra; Enter files it (publicly), Esc cancels |
| `/teleport [infra]`, `/teleport back` | Move the selected session to a SkyPilot cluster and back (experimental, see below) |

On the command line: `orchestra roi` (the interoperability report), `orchestra rename <old> <new>` (also renames the tmux session, worktree and branch), `orchestra tmux-setup` (reapplies orchestra's tmux settings to a running tmux server) and `orchestra upgrade`.

### Worktrees

Worktrees follow the layout Claude Code uses for `.claude/worktrees/`:

```
<repo>/.orchestra/worktrees/<name>/    branch worktree-<name>
```

- The repo is the git repo containing the directory you run `orchestra` from; from inside a worktree, it is the main checkout. Outside a git repo, orchestra uses `ORCHESTRA_WORK_REPO` from the environment or `~/.orchestra/env`. With neither, sessions run in the launch directory without a worktree.
- New worktrees branch from `origin/<default-branch>` (detected from the remote, not assumed) after a `git fetch`. Your main checkout is never pulled or switched, so it can be dirty or on a feature branch.
- `.orchestra/` is added to the repo's `.git/info/exclude`, so worktrees never show up in `git status`.
- Gitignored files listed in a `.worktreeinclude` file at the repo root (for example `.env`) are copied into each new worktree. The format is the same as Claude Code's.
- Deleting a session removes its worktree and branch, and the confirmation tells you first if it has uncommitted files or commits not on the base branch. Deleting an adopted session only stops it; its directory and its Claude Code or Codex transcript are left alone.

### Sessions from outside orchestra

The list shows Claude Code sessions (`~/.claude/projects/`), Codex sessions (`~/.codex/sessions/`) and pi sessions (`~/.pi/agent/sessions/`, plus orchestra's own pi conversations that no session tracks) whose directory is inside the current repo, including its worktrees. Titles come from the agent (Claude Code's `/rename` or generated title, Codex's thread name), falling back to the first prompt. Subagent and reviewer threads are skipped. Enter adopts a session in its own agent, and a `●` marks the ones already open in orchestra.

### Stopped and idle sessions

A session whose tmux session is gone (tmux restarted, or the machine rebooted) stays in the list as **Stopped**, and so does one stopped for being idle. Enter resumes it: its agent starts again on its own conversation (`pi --continue`, `claude --resume`, `codex resume`) in the same directory. Only the running process and its screen are lost; the conversation, the worktree and the session's settings are kept.

A session is stopped after 30 minutes when it is not open, not working, has no `/loop`, and has written nothing to its conversation. `/suspend` stops one right away. Set `"suspend_after_minutes"` in `~/.orchestra/config.json` to change the limit (`0` turns it off). Claude Code's own background sessions are managed by Claude Code, not orchestra.

### Look and feel

- Sessions look like the agent on its own: no tmux status bar, the mouse wheel scrolls the conversation, a click returns to the prompt, and typing after scrolling jumps back to the prompt. With tmux 3.6 or newer, copy mode's position counter and cursor are hidden too.
- Links open with Cmd+click (Ctrl+click on Linux and Windows), including links the agent shows as text such as "PR #5", which tmux passes on as terminal hyperlinks.
- pi is set up to look like Claude Code. `pi/extensions/claude-look.ts` draws tool calls as `● Bash(command)` with a status-colored dot and gray, collapsed output under `⎿`. The `orchestra-light` and `orchestra-dark` themes use Claude Code's palettes, reasoning is hidden, and the startup notices are off. `install.sh` installs all of this, `/theme` switches orchestra and pi together, and a running pi session picks changes up with `/reload`.
- **Copying text:** drag over it with the mouse. On release it is sent to your clipboard through the terminal's clipboard support (OSC 52). iTerm2 needs Settings → General → Selection → "Applications in terminal may access clipboard". macOS Terminal.app and browser terminals such as code-server usually block it. There, use the terminal's own selection instead: Option+drag on a Mac (in VS Code, with `terminal.integrated.macOptionClickForcesSelection` on) or Shift+drag elsewhere, and turn on `terminal.integrated.copyOnSelection` to copy on release.

### Renaming

Sessions are listed by a readable name: the title Claude Code or Codex gave an adopted session, or else the first line of the prompt. Ctrl+R or `/rename <name>` changes only the name shown, which is safe while the agent is working; an empty name goes back to the default. `orchestra rename <old> <new>` on the command line also renames the tmux session, the worktree directory and branch, and the state directory.

## Models and providers

Claude Code and Codex use their own logins and models (for example `/switch claude:opus`). pi works with any provider:

- **Built-in providers** (Anthropic, OpenAI, OpenRouter, Gemini, Groq, Mistral and more), through their usual API key environment variables or pi's `/login`.
- **Custom providers** in `~/.pi/agent/models.json`: any OpenAI-compatible endpoint, such as vLLM, OpenRouter, or a token proxy or gateway. See `pi/models.json.example`. Keep keys out of the file with pi's command syntax, for example `"apiKey": "!printf %s \"$MY_API_KEY\""`.

A new pi session starts on the model set with `/model`, or else `ORCHESTRA_PROVIDER` / `ORCHESTRA_MODEL` from `~/.orchestra/env`, or else pi's own default. You can also switch inside a pi session with pi's `/model`, and the change survives restarts.

Global instructions can be shared across agents: pi reads `~/.pi/agent/AGENTS.md` and Codex reads `~/.codex/AGENTS.md`. Make both symlinks to your `~/.claude/CLAUDE.md` and every agent follows the same rules.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/rohansonecha/orchestra/main/install.sh | bash
```

This installs the dependencies (Node, Rust, tmux, pi, and the OpenClaw and SkyPilot CLIs for the optional main box), clones the repo to `~/orchestra`, builds the `orchestra` binary, puts it on your PATH, and installs the pi look (extension, themes and settings). Install Claude Code and Codex separately if you want to use them. tmux 3.6 or newer is recommended. Re-running the command upgrades everything, and `orchestra upgrade` does the same from an existing install. Existing config in `~/.orchestra` and `~/orchestra/private` is never overwritten.

## Optional: an always-on box with SkyPilot

orchestra runs anywhere you have a terminal. To keep sessions running on a remote machine you SSH into, launch the included SkyPilot "main box". It also runs an [OpenClaw](https://openclaw.ai) gateway with `main` and `work` agent personas, which chat channels can route to.

1. Fill in `~/orchestra/private/env` (endpoints and tokens), `private/models.json` (from `pi/models.json.example`) and `private/openclaw/openclaw.json` (from `private.example/`).
2. Store these secrets on your SkyPilot API server: `ORCHESTRA_API_KEY`, `SKY_API_ENDPOINT`, `ORCHESTRA_SKY_API_TOKEN`, `GIT_EMAIL`, `GIT_NAME`, `GITHUB_TOKEN` and `LINEAR_TOKEN`.
3. Set `SKY_INFRA`, `SKY_API_ENDPOINT` and `ORCHESTRA_SKY_API_TOKEN` in `private/env`.
4. Launch it: `source private/env && sky launch --infra "$SKY_INFRA" -c main-box skypilot/main-box.yaml`
5. Connect and run it: `ssh main-box`, then `orchestra`. The box sets `ORCHESTRA_WORK_REPO`, so sessions get worktrees in its work repo.

`/teleport` moves a single session to its own SkyPilot cluster and back. Before launching, it always shows the API server, the cluster and everything it will send. It passes secrets by name only, and it can use its own API server (`SKY_API_ENDPOINT` with `ORCHESTRA_SKY_API_TOKEN` or `ORCHESTRA_SKY_TOKEN_FILE` in `~/.orchestra/env`) without touching `~/.sky/config.yaml`. It is experimental: the plan and the task file are tested, but a real launch is not yet.

## Architecture

```
orchestra (terminal UI, Rust + ratatui)
  │  session list · picker · commands · interoperability (history.rs, switch.rs)
  │
  ├── one tmux session per agent session (no status bar, mouse scrolling)
  │     ├── claude   → ~/.claude/projects/<dir>/<id>.jsonl
  │     ├── codex    → ~/.codex/sessions/…/rollout-…-<id>.jsonl
  │     └── pi       → ~/.orchestra/pi-sessions/<id>/   (any provider via models.json)
  │
  ├── one git worktree per session: <repo>/.orchestra/worktrees/<name>  (branch worktree-<name>)
  │
  └── state: ~/.orchestra/  (sessions/, pi-sessions/, prompts/, config.json, env)
```

## Repository layout

```
orchestra/
├── tui/src/
│   ├── main.rs          # startup, terminal setup, CLI subcommands
│   ├── app.rs           # application state: sessions, pickers, dispatch, commands
│   ├── keys.rs          # key handling for every view and input mode
│   ├── render.rs        # drawing: list, tree, overlays, footer
│   ├── agent_view.rs    # the session list, picker and panels
│   ├── session.rs       # sessions, agent launch commands, tmux setup
│   ├── history.rs       # reads and writes each agent's transcript format; tool mapping
│   ├── switch.rs        # switching sessions between agents and models
│   ├── import.rs        # finds Claude Code, Codex and pi sessions on disk
│   ├── commands.rs      # session commands (/btw, /loop, /code-review, /bug, …)
│   ├── coverage.rs      # orchestra roi
│   ├── teleport.rs      # /teleport (experimental)
│   ├── worktree.rs      # worktrees, .worktreeinclude
│   ├── repo.rs          # finds the repo and its default branch
│   ├── line_edit.rs     # prompt editing keys
│   ├── config.rs        # ~/.orchestra/config.json
│   └── …                # tree view, rename, paths
├── pi/
│   ├── extensions/claude-look.ts   # Claude Code-style tool rendering for pi
│   ├── agents/                     # pi agent definitions
│   └── models.json.example         # OpenAI-compatible provider template
├── docs/switching-agents.md        # what carries over in a switch, and why
├── skypilot/            # main box and test box YAMLs, pi themes
├── tree-view/           # agent tree collector and SkyPilot sub-agent tooling
├── skills/              # orchestra skills, loaded into sessions
├── openclaw-skills/     # skills for the OpenClaw gateway
├── private.example/     # templates for private config (OpenClaw, env)
├── tests/
└── install.sh
```

## Future directions

- **Artifacts:** publish pages that agents make, viewable locally or on the local network ([#3](https://github.com/rohansonecha/orchestra/issues/3)).
- **Better Codex coverage:** translate Codex's multi-step `exec` programs into individual tool calls instead of text. `orchestra roi` lists which calls fall back to text most often.
- **Handoff summaries:** have an agent summarize its decisions before a switch, to carry over what hidden reasoning and encrypted compaction can't.
- **Chat entrypoints:** dispatch and check on sessions from Slack or Telegram.
- **Teleport:** validate real launches, and make moving sessions between machines routine.

## License

MIT
