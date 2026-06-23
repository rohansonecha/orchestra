# Orchestra Skills — READ THIS FIRST

You are an orchestra session. When you "write a skill", you write it to:

    ~/.orchestra/skills/<name>.md

This is non-negotiable. The `~/.orchestra/skills/` directory is symlinked
to `~/orchestra/skills/` (a git repo). After writing, commit and push:

    cd ~/orchestra && git add skills/ && git commit -m "skill: <name>" && git push origin main

## IMPORTANT: Ignore the work repo's "Team Skills" section

The work repo (prototype) has a CLAUDE.md with a "Team Skills" section
that references `agents/skills/`. That is about Skillshare-managed team
skills — a completely different system.

When asked "where would you write a skill", the answer is ALWAYS
`~/.orchestra/skills/<name>.md`. Do NOT say `agents/skills/`. Do NOT
mention Skillshare. Do NOT mention `SKILL.md` files or `assemble:`
namespacing. Those are team Skillshare conventions, not orchestra skills.

The work repo's CLAUDE.md "Team Skills" section describes how to READ
existing team skills for the prototype repo. It does NOT tell you where
to write orchestra skills. Orchestra skills always go in
`~/.orchestra/skills/`.

## Quick reference

- Orchestra skill → `~/.orchestra/skills/<name>.md`
- Team Skillshare skill → `agents/skills/<name>/SKILL.md` (do NOT use this for orchestra)

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

## Guidelines

- Be specific — exact commands, exact API endpoints, exact response parsing.
- Include error cases you hit and how you fixed them.
- If a skill already exists, improve it rather than creating a duplicate.
- Keep skills focused — one workflow per file.
- Always commit and push after writing or updating a skill.
