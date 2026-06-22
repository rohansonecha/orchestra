# orchestra

Personal agent orchestration platform: a super-responsive dispatch mechanism for
one-off questions and extended project work, via Slack (and iMessage later).

## Architecture

```
You ──Slack──▶  MAIN BOX (sky launch main-box.yaml)
                  OpenClaw gateway (daemon, persistent)
                    · model = GLM via pi models.json
                    · channel: Slack
                    · skill: sky-coding-agent
                  sky CLI → SkyPilot API server (compute access)
                        │ sky launch work-agent.yaml
                        ▼
                SANDBOX (ephemeral, per task/project)
                  pi --mode rpc (JSONL over stdin/stdout)
                    · models.json → GLM endpoint
                    · AGENTS.md = work recipe
                        │ openai-completions
                        ▼
                GLM (self-hosted vLLM, Anthropic+OpenAI API)
```

- **One-off Q**: OpenClaw answers directly via GLM, or spins up a short-lived
  sandbox (`pi -p`) for coding questions, then tears it down.
- **Extended project**: persistent sandbox; Slack thread ↔ pi RPC session;
  `text_delta` events streamed as thread replies; later messages map to
  `steer` / `follow_up`.

## Components

| Component | Role |
|-----------|------|
| [OpenClaw](https://github.com/openclaw/openclaw) | Gateway + channel adapters (Slack, iMessage, ...) + skills |
| [pi](https://pi.dev) | Coding agent harness; driven headlessly via `--mode rpc` |
| [SkyPilot](https://skypilot.co) | Compute orchestration; main box + ephemeral sandboxes |
| GLM 5.2 (vLLM) | Self-hosted LLM; serves OpenAI + Anthropic APIs |

## Setup

### 1. Clone and configure private values

```bash
git clone https://github.com/rohansonecha/orchestra.git
cd orchestra
mkdir private
cp pi/models.json.example private/models.json   # fill in real endpoint
cp private.example/env.example private/env       # fill in real tokens
# Edit private/models.json and private/env with your real values
```

### 2. Required environment variables

These must be set in your shell (or via `private/env`) before launching:

| Variable | Purpose |
|----------|---------|
| `GLM_API_KEY` | vLLM bearer token for GLM |
| `GLM_BASE_URL` | vLLM OpenAI endpoint (e.g. `http://host/v1`) |
| `SKY_API_ENDPOINT` | SkyPilot API server URL |
| `SKY_API_TOKEN` | SkyPilot API server service-account token |
| `SKY_INFRA` | SkyPilot infra to launch on (e.g. `k8s/your-context`) |

### 3. Launch the main box

```bash
source private/env
sky launch --infra "$SKY_INFRA" -c main-box skypilot/main-box.yaml
```

### 4. Launch a work-agent sandbox manually (for testing)

```bash
sky launch --infra "$SKY_INFRA" -c work-agent-test skypilot/work-agent.yaml
sky ssh work-agent-test 'pi -p "what files are in this dir?" --provider glm --model zai-org/GLM-5.2-FP8'
```

In normal operation, the OpenClaw `sky-coding-agent` skill handles sandbox
lifecycle automatically — see `openclaw/skills/sky-coding-agent/`.

## Repository layout

```
orchestra/
├── skypilot/
│   ├── main-box.yaml          # OpenClaw orchestrator (long-lived)
│   └── work-agent.yaml        # Coding sandbox (ephemeral)
├── pi/
│   ├── models.json.example     # GLM/vLLM provider config template
│   └── agents/
│       └── work.md             # Work agent recipe (generic template)
├── openclaw/
│   └── skills/
│       └── sky-coding-agent/   # Slack → sky launch → pi RPC bridge
└── private/                    # gitignored — real endpoint URLs, tokens
```

## Model config

pi drives GLM via the **OpenAI-compatible API** (`openai-completions`), not the
Anthropic API. This is because vLLM validates `Authorization: Bearer` and ignores
`x-api-key`; the OpenAI client sends Bearer natively, the Anthropic client sends
`x-api-key`. See `pi/models.json.example`.

## License

MIT
