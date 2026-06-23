# Linear — Read Issues by Project / Assignee / Priority

## What it does

Query the Linear GraphQL API to list issues filtered by **project or team**,
**assignee email**, and **priority**, sorted by priority / due date / last-updated,
and print them as a clean Open vs. Done report with links. Reusable for any
project / team / person / priority level.

## Prerequisites

- `LINEAR_TOKEN` env var (Linear personal API key). In orchestra sessions this
  is already loaded from `~/.orchestra/env`; otherwise `source ~/.orchestra/env`
  or `export LINEAR_TOKEN=lin_api_...`.
- `python3` (stdlib only — uses `urllib`).
- `curl` (optional, for one-shot exploration).

## Linear API basics

- Endpoint: `https://api.linear.app/graphql`
- Auth header: `Authorization: $LINEAR_TOKEN`
- All queries are POST with `{"query": "...", "variables": {...}}` JSON body.

## Priority mapping — IMPORTANT

Linear stores priority as an **int where lower = more urgent**:

| Value | Meaning |
|------:|---------|
| 0 | No priority |
| 1 | Urgent |
| 2 | High |
| 3 | Medium |
| 4 | Low |

### GOTCHA: `priority: { lte: 2 }` also matches priority 0

Because `0 <= 2`, filtering `priority: { lte: 2 }` returns No-priority issues
too — NOT what you want when someone asks for "high priority". Always filter
"high priority" (Urgent + High) as an **OR of exact matches**:

```json
{ "or": [ { "priority": { "eq": 1 } }, { "priority": { "eq": 2 } } ] }
```

Likewise `gte: 3` is correct for "Medium and below" (3,4) since 0 won't match,
but `lte`-style ranges are dangerous because of the 0 trap.

### `--priority all` is the exception

`--priority all` drops the priority filter entirely, so priority-0 ("No
priority") issues ARE included — which is what you want when listing
everything. Every other level uses exact `eq` matches (see the script's
`PRIORITY_LEVELS`), so the 0-trap never bites except by explicit request.

## Pagination — IMPORTANT

- Default page size is **50**. You will silently miss issues if you don't page.
- `after` (cursor) **cannot be used without `first`** — the API returns
  `INVALID_INPUT: "after cannot be used without at least one of first."`
- Use `first: 250` (the max) with `after` from `pageInfo.endCursor`, looping
  until `pageInfo.hasNextPage` is false.
- There is **no `totalCount`** field on `IssueConnection` — count `nodes`
  yourself.

## Step 1 — Resolve project/team ID and user ID by name/email

Linear has no name-based filter on the `projects`/`teams`/`users` connections,
so fetch all and match client-side. **Fetch `teams` too** — a name may be a
team (key `SKY` / SkyPilot), not a project ("GPU manager" is a project):

```bash
source ~/.orchestra/env
curl -s https://api.linear.app/graphql \
  -H "Authorization: $LINEAR_TOKEN" -H "Content-Type: application/json" \
  -X POST -d '{ "query": "{ projects { nodes { id name } } teams { nodes { id name key } } users { nodes { id name email } } }" }' \
  | python3 -c "
import sys, json
d = json.load(sys.stdin)['data']
for p in d['projects']['nodes']:
    if 'gpu' in p['name'].lower(): print('PROJECT', p)
for t in d['teams']['nodes']:
    print('TEAM', t)
for u in d['users']['nodes']:
    if 'rohan' in (u.get('email') or '').lower(): print('USER', u)
"
```

Project and team are different things in Linear. "GPU Manager" here is a
**project** (team is `SKY` / SkyPilot). The script resolves both automatically
(see Step 2): it tries **exact** matches across `projects` and `teams` first
(by name, then by team key), and only falls back to substring matching if
nothing exact hits. This matters — without exact-first, passing `SKY` would
substring-match the project "SkyServe v2" (it contains "sky") instead of the
team whose key is `SKY`.

## Step 2 — Query issues (reusable script)

Save as `linear-issues.py` and run:

```
python3 linear-issues.py --project "GPU manager" --email rohan@assemblesys.com --priority high
```

`--project` accepts a project name **or** a team name/key (e.g. `SKY`).
`--priority` choices: `urgent, high, medium, low, all`. `--sort` choices:
`priority` (default), `due` (due-date ascending, undated last), `updated`
(most recently updated first). Add `--all` to also print Done issues.

```python
#!/usr/bin/env python3
import argparse, json, os, sys, urllib.error, urllib.request
from collections import Counter

PRIORITY = {0: "No priority", 1: "Urgent", 2: "High", 3: "Medium", 4: "Low"}
# Map a --priority word to the set of numeric levels it covers.
# "high" => Urgent + High. Using explicit eq's (not lte) avoids the
# priority-0 trap where {lte: 2} also matches "No priority".
# "all" => no priority filter at all (None), so priority-0 issues ARE included.
PRIORITY_LEVELS = {
    "urgent": (1,),
    "high":   (1, 2),
    "medium": (3,),
    "low":    (4,),
    "all":    None,
}
DONE_WORDS = ("done", "canceled", "cancelled")
TIMEOUT = 30

def gql(token, query, variables):
    body = json.dumps({"query": query, "variables": variables}).encode()
    req = urllib.request.Request(
        "https://api.linear.app/graphql", data=body,
        headers={"Authorization": token, "Content-Type": "application/json"})
    try:
        raw = urllib.request.urlopen(req, timeout=TIMEOUT).read()
    except urllib.error.HTTPError as e:
        sys.exit(f"Linear API HTTP {e.code}: {e.reason} "
                 f"(check LINEAR_TOKEN / network)")
    except urllib.error.URLError as e:
        sys.exit(f"Network error talking to Linear API: {e.reason}")
    d = json.loads(raw)
    if "errors" in d:
        sys.exit("GraphQL errors:\n" + json.dumps(d["errors"], indent=2))
    return d["data"]

def resolve(token, scope_name, email):
    """Resolve a project-or-team by name/key and an assignee by email.

    Linear splits "GPU manager" (a project) from its team (SKY). The old
    version only looked at `projects`; now we also check `teams` so a team
    name or key works. IMPORTANT: we try EXACT matches across both projects
    and teams first, and only fall back to substring matching if nothing
    exact hits — otherwise passing "SKY" would substring-match the project
    "SkyServe v2" (it contains "sky") instead of the team whose key is SKY.
    Returns (kind, id, display, uid) where kind is 'project' or 'team' and
    drives the IssueFilter key.
    """
    d = gql(token,
            "{ projects { nodes { id name } } "
            "teams { nodes { id name key } } "
            "users { nodes { id name email } } }", {})
    name_l = scope_name.lower()
    projs = d["projects"]["nodes"]
    teams = d["teams"]["nodes"]

    # Pass 1: exact matches. Prefer project, then team-by-name, then team-by-key.
    proj = next((p for p in projs if p["name"].lower() == name_l), None)
    team = next((t for t in teams if t["name"].lower() == name_l), None)
    if not team:
        team = next((t for t in teams
                     if (t.get("key") or "").lower() == name_l), None)

    # Pass 2: substring fallback ONLY if no exact match at all. Project first
    # (preserves original behavior), then team.
    if not proj and not team:
        proj = next((p for p in projs if name_l in p["name"].lower()), None)
        if not proj:
            team = next((t for t in teams
                         if name_l in t["name"].lower()), None)

    if not proj and not team:
        sys.exit(f"No project or team matching {scope_name!r} "
                 f"(checked both projects and teams)")
    user = next((u for u in d["users"]["nodes"]
                 if (u.get("email") or "").lower() == email.lower()), None)
    if not user:
        sys.exit(f"No user matching {email!r}")
    if proj:
        return "project", proj["id"], proj["name"], user["id"]
    return "team", team["id"], team["name"], user["id"]

def fetch_issues(token, kind, scope_id, user_id, priority_levels):
    q = """
    query($f: IssueFilter!, $after: String, $first: Int) {
      issues(filter: $f, after: $after, first: $first) {
        pageInfo { hasNextPage endCursor }
        nodes { identifier title priority url dueDate updatedAt state { name } }
      }
    }"""
    # `kind` is 'project' or 'team' -> the matching IssueFilter key, so the
    # same code scopes to either. OR of exact priority matches (or none for
    # "all") => never accidentally includes priority 0 unless asked.
    f = {"assignee": {"id": {"eq": user_id}},
         kind: {"id": {"eq": scope_id}}}
    if priority_levels:
        f["or"] = [{"priority": {"eq": p}} for p in priority_levels]
    out, after = [], None
    while True:
        d = gql(token, q, {"f": f, "after": after, "first": 250})
        conn = d["issues"]
        out.extend(conn["nodes"])
        if not conn["pageInfo"]["hasNextPage"]:
            break
        after = conn["pageInfo"]["endCursor"]
    return out

def sort_issues(issues, mode):
    if mode == "due":
        issues.sort(key=lambda i: (i["dueDate"] is None, i["dueDate"] or "",
                                   i["identifier"]))
    elif mode == "updated":
        issues.sort(key=lambda i: i["updatedAt"] or "", reverse=True)
    else:  # priority
        issues.sort(key=lambda i: (i["priority"], i["identifier"]))

def fmt_due(i):
    return f" [due {i['dueDate'][:10]}]" if i.get("dueDate") else ""

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--project", required=True,
                    help="project OR team name/key (e.g. 'GPU manager' or 'SKY')")
    ap.add_argument("--email", required=True)
    ap.add_argument("--priority", default="high", choices=list(PRIORITY_LEVELS))
    ap.add_argument("--sort", default="priority",
                    choices=["priority", "due", "updated"])
    ap.add_argument("--all", action="store_true",
                    help="also print Done issues, not just open")
    args = ap.parse_args()

    token = os.environ.get("LINEAR_TOKEN")
    if not token:
        sys.exit("LINEAR_TOKEN not set (run: source ~/.orchestra/env)")

    kind, sid, sname, uid = resolve(token, args.project, args.email)
    levels = PRIORITY_LEVELS[args.priority]
    issues = fetch_issues(token, kind, sid, uid, levels)
    sort_issues(issues, args.sort)
    open_i = [i for i in issues if i["state"]["name"].lower() not in DONE_WORDS]
    done_i = [i for i in issues if i["state"]["name"].lower() in DONE_WORDS]

    print(f"{kind.capitalize()}: {sname}")
    print(f"Assignee: {args.email}  Priority: {args.priority}  Sort: {args.sort}")
    print(f"TOTAL: {len(issues)}  (open: {len(open_i)}, done: {len(done_i)})")
    c = Counter(i["priority"] for i in issues)
    print("by priority:", {PRIORITY.get(k, k): v for k, v in c.items()})
    print("\n===== OPEN =====")
    for i in open_i:
        print(f"[{PRIORITY.get(i['priority'], i['priority'])}] "
              f"{i['identifier']} ({i['state']['name']}){fmt_due(i)} - {i['title']}\n"
              f"   {i['url']}")
    if args.all:
        print("\n===== DONE =====")
        for i in done_i:
            print(f"[{PRIORITY.get(i['priority'], i['priority'])}] "
                  f"{i['identifier']}{fmt_due(i)} - {i['title']}\n   {i['url']}")

if __name__ == "__main__":
    main()
```

> Key correctness points:
> - `fetch_issues` builds `or: [{priority:{eq:p}} for p in levels]` — exact
>   `eq` matches instead of a range — to avoid the priority-0 trap where
>   `{lte: 2}` matches "No priority". `--priority all` simply omits the `or`
>   so priority 0 is included by design.
> - `resolve()` matches **exactly** (project name, then team name, then team
>   key) before any substring fallback, so `SKY` resolves to the team (key
>   `SKY`), not the project "SkyServe v2".
> - `gql()` has a 30s timeout and turns HTTP/network errors into clean
>   one-line `sys.exit` messages instead of a raw traceback.

## Step 3 — Expected output format

```
Project: GPU manager
Assignee: rohan@assemblesys.com  Priority: high  Sort: priority
TOTAL: 62  (open: 16, done: 46)
by priority: {'Urgent': 7, 'High': 55}

===== OPEN =====
[High] SKY-5856 (Todo) - [GPU Manager] Integrate GPU metrics checks ...
   https://linear.app/skypilot/issue/SKY-5856/...
...
```

If an issue has a due date, `[due YYYY-MM-DD]` appears before the `-`, e.g.
`[High] SKY-5829 (In Progress) [due 2026-06-30] - ...`. The header line shows
which `Scope` (Project/Team), `Priority`, and `Sort` were used.

Terminal states to treat as "done": `Done`, `Canceled`. Everything else
(`Todo`, `In Progress`, `In Review`, `Backlog`, `Triage`, `Paused`) is open.

## Common errors and fixes

| Error | Cause | Fix |
|-------|-------|-----|
| `after cannot be used without at least one of first` | Paged with `after` but no `first` | Always pass `first: 250` with `after` |
| No-priority issues appear in "high priority" results | `priority: {lte: 2}` matches `0` | Use `or: [{eq:1},{eq:2}]` instead of a range (script already does) |
| Only 50 results returned | Default page size, no paging loop | Loop on `pageInfo.hasNextPage`/`endCursor` (script already does) |
| `Cannot query field "totalCount"` | Field doesn't exist on `IssueConnection` | Count `nodes` in code |
| Project name not found under `projects` | It may be a **team**, not a project | The script checks `teams` too (by name or key); pass the team name/key, e.g. `--project SKY`. Exact matches are tried before substring, so `SKY` resolves to the team (key `SKY`), not the project "SkyServe v2" |
| `Linear API HTTP 401: Unauthorized` | Token missing/invalid | `source ~/.orchestra/env`; rotate key in Linear → Settings → API |
| `Network error talking to Linear API: ...` | Timeout / connectivity (30s) | Retry; check VPN/proxy; the script aborts cleanly instead of hanging |

## Example (the query this skill was built from)

List all high-priority (Urgent + High) Linear tickets assigned to
`rohan@assemblesys.com` in the **GPU manager** project:

```bash
source ~/.orchestra/env
python3 linear-issues.py --project "GPU manager" \
  --email rohan@assemblesys.com --priority high --all
```

Result: **62 total** (7 Urgent, 55 High) — 16 open, 46 done. The 16 open ones
are all `High` priority, e.g. SKY-5856 (integrate GPU metrics checks with
alerting), SKY-5829 (In Progress — historical utilization view shows no data),
SKY-5226 (GPU reaper: kill idle GPU jobs), SKY-5049 (SXID error detection in
dmesg), SKY-4682 (show workloads per node). Run the script for the live list.

### Variations

Scope to a **team** by name or key (here the SkyPilot team, key `SKY`):

```bash
python3 linear-issues.py --project "SKY" --email rohan@assemblesys.com --priority urgent
# -> Team: SkyPilot  (24 urgent, all done)
```

List **every** priority for an assignee in a project, sorted by due date:

```bash
python3 linear-issues.py --project "GPU manager" \
  --email rohan@assemblesys.com --priority all --sort due
# -> TOTAL: 92  (open: 35, done: 57)  by priority: {'Urgent': 7, 'High': 55, 'Medium': 23, 'Low': 3, 'No priority': 4}
```
