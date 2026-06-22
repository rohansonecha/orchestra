# private/ — Local-only config (gitignored)

This directory holds real endpoint URLs, tokens, and proprietary references.
It is gitignored and **never committed**.

## Required files

### `private/env`
Environment variables for launching. Copy from `private.example/env.example`:
```bash
cp ../private.example/env.example env
# Edit with real values
```

### `private/models.json`
Pi provider config with real vLLM endpoint. Copy from `pi/models.json.example`:
```bash
cp ../pi/models.json.example models.json
# Replace YOUR_VLLM_HOST with the real host
```

### `private/agents/work.md`
Work agent recipe with real repos, conventions, tooling. Copy from `pi/agents/work.md`:
```bash
cp ../pi/agents/work.md agents/work.md
# Fill in real repo names, conventions, etc.
```

## Security

- Never commit files in `private/` — `.gitignore` excludes the entire directory.
- Rotate tokens if they are ever exposed (e.g. shared in chat).
- The `GLM_API_KEY` is referenced by `!printf %s $GLM_API_KEY` in models.json
  and resolved at runtime from the environment — the key itself is not in the file.
