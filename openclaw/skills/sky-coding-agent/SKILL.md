---
name: sky-coding-agent
description: >
  Launch and drive ephemeral coding-agent sandboxes via SkyPilot. Use when the
  user asks a coding question, wants to work on a project, debug an issue, or
  perform any task that benefits from a full development environment (file
  access, git, build tools). For simple factual questions, answer directly.
allowed-tools: bash
---

# Sky Coding Agent

This skill launches ephemeral coding sandboxes via SkyPilot and drives the pi
coding agent inside them. Each sandbox is a disposable VM/pod with full
development tools, git, and pi configured to use GLM.

## When to use

- User asks a coding question that requires reading/editing files or running code
- User wants to start or continue a project
- User wants to debug an issue in a repo

Do NOT use for:
- Simple factual questions (answer directly via GLM)
- Questions about the orchestrator itself

## One-off question (quick)

For a single coding question, launch a sandbox, ask, and tear down:

    ./scripts/launch-sandbox.sh "<task-id>"
    ./scripts/send-prompt.sh "<task-id>" "What files are in /root/workspace?"
    # Read the response, relay to user
    ./scripts/teardown-sandbox.sh "<task-id>"

## Extended project (persistent session)

For multi-turn project work, keep the sandbox alive across messages. The pi
session persists to disk inside the sandbox, so context is preserved between
prompts:

    # First message — launch sandbox and send initial prompt
    ./scripts/launch-sandbox.sh "<project-id>"
    ./scripts/send-prompt.sh "<project-id>" "Clone my-repo and add a health check endpoint"

    # Subsequent messages — same sandbox, continued session
    ./scripts/send-prompt.sh "<project-id>" "Now add tests for the health check"

    # When the user is done
    ./scripts/teardown-sandbox.sh "<project-id>"

## Tracking sandboxes

The scripts store state in `/tmp/orchestra-sandboxes/`:
- `<id>.cluster` — the SkyPilot cluster name
- `<id>.session` — the pi session name (for multi-turn)

Always check if a sandbox already exists before launching:
    `[ -f /tmp/orchestra-sandboxes/<id>.cluster ]`

## Relaying responses

After `send-prompt.sh`, read the response from stdout. Relay the agent's text
back to the user in the Slack thread. If the response is long, summarize and
offer to share full output.

## Teardown

Always tear down sandboxes when the user is done, or after 30 minutes of
inactivity. SkyPilot clusters cost money while running.
