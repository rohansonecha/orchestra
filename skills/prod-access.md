# Prod Access — Read-only Tenant Access with a CP Access Token

## What it does

Read-only access to SkyPilot production (or dev) tenants through a control-plane
access token (`sky_cp_…`) instead of kubectl, a gcloud user login, or the
super-admin session from `pull_debug_dump.py --login` (deprecated by yelei the
week of 2026-10-12). Covers five paths: the tenant API server via the `sky` CLI,
Cloud Logging, the central Grafana, debug dumps, and a shell command on the
tenant's API server that a super admin approves.

The canonical, maintained version of this skill is the team skill
`assemble:prod-access` in assemble-org/prototype (merged in #2301, SKY-6133).
Before a non-trivial investigation, read the current copy from origin, not a
local checkout:

    git -C ~/prototype-sky7321-deploy fetch -q origin master
    git -C ~/prototype-sky7321-deploy show origin/master:agents/skills/prod-access/SKILL.md

Announcement and background: Notion "Agent Read-only Access to Prod"
(https://app.notion.com/p/Agent-Read-only-Access-to-Prod-3f3d3d4bb77481259068d1074dc6db57).

## Prerequisites

- **Scripts** (stdlib Python, feature-plugin `main`). A detached worktree that
  tracks `origin/main` lives at `~/fp-main-tools`; refresh it before use:

      git -C ~/fp-main-tools fetch -q origin main && git -C ~/fp-main-tools checkout -q --detach origin/main
      AA=~/fp-main-tools/scripts/pull-debug-dump/agent_access.py
      CLI=~/fp-main-tools/scripts/pull-debug-dump/pull_debug_dump.py

- **Token.** Check that one is saved without reading it (`_DEV` for dev):

      grep -qs '^CP_ACCESS_TOKEN_PROD=' "${SKYPILOT_CP_DIAGNOSE_CONFIG:-$HOME/.config/skypilot-cp-diagnose.conf}" && echo ok || echo missing

  If missing, stop and walk the user through it. Never ask them to paste the
  token into the conversation.
  1. A super admin creates a token at https://platform.skypilot.co/settings/access-tokens
     (dev: https://platform.dev.skypilot.co/settings/access-tokens) with
     **All organizations**, scopes `diagnose:read`, `diagnose:dump`,
     `shell:propose`, `tenant:read`, `logs:read`, `metrics:read`, and an expiry.
  2. The user runs in their own terminal (e.g. `! ` prefix):
     `python3 ~/fp-main-tools/scripts/pull-debug-dump/agent_access.py --save-token --cp-env prod`
     and pastes the token at the hidden prompt.

  Bots and CI set `SKYPILOT_CP_ACCESS_TOKEN` instead. Never fall back to
  `pull_debug_dump.py --login`.

## Conventions

- Pass `--cp-env prod` (or `dev` for `*.dev.skypilot.co`) on every command.
- `--org <name>` resolves names when the token has `diagnose:read`; else pass
  the org id. `python3 "$CLI" --cp-env prod --list-orgs --json` lists orgs.
- Credentials dirs: `D=~/.cache/cp-creds/<env>/<tenant>` per tenant,
  `F=~/.cache/cp-creds/<env>/fleet` for logging/grafana. Minted credentials
  expire after one hour; re-run the same mint command on `NEEDS_AUTH`.

| You need | Path | Scope |
|---|---|---|
| Live clusters / managed jobs, job or cluster logs | §1 sky CLI | `tenant:read` |
| Why a request failed; errors, crashes, restarts | §2 Cloud Logging | `logs:read` (all orgs) |
| Memory, latency, alerts, trends | §3 Grafana | `metrics:read` (all orgs) |
| Full snapshot: task specs, events, k8s objects, request logs | §4 Debug dump | `diagnose:read` + `diagnose:dump` |
| Anything only the API-server host can see | §5 Proposed command | `shell:propose` |

Start with §1 for job/cluster problems, §2 for failed requests or server
problems; correlate by timestamp.

## §1 Tenant API server (as skypilot-system-viewer)

    python3 "$AA" tenant --cp-env prod --org <tenant> -o "$D"
    source "$D/tenant.env"
    sky api info            # User: skypilot-system-viewer
    sky status -u
    sky jobs queue -u
    sky jobs logs <job-id> --no-follow
    sky logs <cluster> <job-id> --no-follow

- Always `-u` on listings and `--no-follow` on logs. Writes fail with
  `PermissionDeniedError`. REST: `curl -sb "$SKYPILOT_API_COOKIE_FILE" "$SKYPILOT_API_SERVER_ENDPOINT/api/health"`.
- `sky api status/logs` only show the viewer's own requests; use §2 + §4 for
  another user's request. Private/operator tenants that time out: use §4/§5.

## §2 Cloud Logging

    python3 "$AA" gcp-logging --cp-env prod -o "$F"
    source "$F/gcp-logging.env"
    API='resource.type="k8s_container" resource.labels.namespace_name="sky-tenant-<tenant>" resource.labels.container_name="skypilot-api"'
    gcloud logging read "$API textPayload:\"failed due to\" -textPayload:\"No live services\"" \
      --freshness=6h --limit=50 --format='value(timestamp, resource.labels.pod_name, textPayload)'

- Every API-server line is severity INFO: filter on text, never `severity`.
  Drop `-textPayload:"Metered billing"` noise. Keep `pod_name` in the format.
- `--order=asc` ignores `--freshness`; bound it with `timestamp>="$SINCE"`.
- Hosted tenants only (prod `sky-prod-465`, dev `sky-dev-465`). More recipes:
  `~/.sky/wiki/infrastructure/pulling-tenant-logs-cloud-logging.md`.

## §3 Grafana

    python3 "$AA" grafana --cp-env prod -o "$F"
    source "$F/grafana.env"
    P="$GRAFANA_URL/api/datasources/proxy/uid/prometheus/api/v1"
    curl -sH @"$GRAFANA_AUTH_HEADER_FILE" -G "$P/query" --data-urlencode 'query=ALERTS{alertstate="firing", namespace="sky-tenant-<tenant>"}'

- cAdvisor `container_*` metrics need `container="skypilot-api"`; the tenant
  namespace also holds compute-cluster containers. API-server metrics are
  `sky_apiserver_*`. Viewer only: writes return 403. The canonical skill has
  the full query set (memory vs limit, restarts, event-loop lag/stalls).

## §4 Debug dump

    python3 "$CLI" --cp-env prod --org <tenant> --request-ids <id> --no-server-logs -o ~/debug_dumps
    python3 "$CLI" --cp-env prod --org <tenant> --job-ids <id> --clusters <name> --no-server-logs -o ~/debug_dumps

- Cause is usually in `requests/<id>/request_debug.log`
  (`grep -n -E 'HTTP response body|Traceback|Error'`); it also holds the merged
  server config, so grep it, never print it whole.
- `request_info.json` / `job_info.json` can carry tokens in task envs: read
  named fields only. On timeout, `--download-id <id>`; don't create another.

## §5 Proposed command (super admin approves)

    python3 "$CLI" --cp-env prod --org <tenant> --json \
      --propose-command 'kubectl get pods -A -o wide' --description '<why, what you expect>'
    python3 "$CLI" --cp-env prod --org <tenant> --command-status <session_id> --wait 600 --json

- Nothing runs until approved: hand the printed `approval_url` to the
  requester and keep working other sources. Never re-propose the same command;
  `--cancel-command <session_id>` to withdraw. Read-only unless asked.
- Runs as the API server's user with its kubeconfig; for the compute cluster
  pass `--context <ctx> -n <ns>` (from a dump's `kubernetes_contexts/`).

## Rules

1. Never print a secret: token, tenant JWT, cookies, Grafana/GCP creds stay in
   files under the creds dir, referenced by path. Never `cat` them or `set -x`,
   and never put them in Slack, Linear, PRs, or the wiki.
2. Failures are answers: a 403 names the missing scope/org; `503 ... is not
   configured` means that CP lacks Logging/Grafana. Report and switch paths,
   don't retry.
3. Quote only the tenant lines that support a conclusion.
4. Don't load a degraded server: no `--follow`, no `sky api logs` streams, no
   repeated full listings.
