# Skills

Skills are reusable workflows that pi coding agent sessions learn and
document for future sessions. Each skill is a markdown file with exact
steps — commands, API calls, file paths — that a fresh session can
follow without rediscovering them.

## How it works

1. A session encounters a task that requires figuring something out
   (e.g., how to read a Linear ticket, how to deploy a service).
2. The session does the work, figures out the exact steps.
3. The session writes a skill file here: `skills/<name>.md`.
4. All future sessions automatically load these skills via
   `--append-system-prompt` at dispatch time.

## Skill format

```markdown
# <Skill Name>

## What it does

One-line description.

## Prerequisites

- Env vars needed (e.g., LINEAR_TOKEN)
- Tools that must be installed

## Steps

Exact commands with explanations. Include:
- The exact curl/API calls
- Expected output format
- How to parse/extract what you need
- Common errors and fixes

## Example

A worked example showing the skill in action.
```

## Writing a skill

When you figure out a reusable workflow, write it to
`~/.orchestra/skills/<name>.md`. The skills directory is symlinked to
`~/orchestra/skills/` which is a git repo — commit your changes so
they persist across launches.

Guidelines:
- Be specific — exact commands, exact API endpoints, exact response parsing.
- Include error cases you hit and how you fixed them.
- If a skill already exists, improve it rather than creating a duplicate.
- Keep skills focused — one workflow per file.
