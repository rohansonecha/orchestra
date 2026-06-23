# Linear — Read Issues by Project / Assignee / Priority

## What it does

Query the Linear GraphQL API to list issues filtered by **project**, **assignee
email**, and **priority**, and print them as a clean Open vs. Done report with
links. Reusable for any project / person / priority level.

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

## Pagination — IMPORTANT

- Default page size is **50**. You will silently miss issues if you don't page.
- `after` (cursor) **cannot be used without `first`** — the API returns
  `INVALID_INPUT: "after cannot be used without at least one of first."`
- Use `first: 250` (the max) with `after` from `pageInfo.endCursor`, looping
  until `pageInfo.hasNextPage` is false.
- There is **no `totalCount`** field on `IssueConnection` — count `nodes`
  yourself.

## Step 1 — Resolve project ID and user ID by name/email

Linear has no name-based filter on the `projects`/`users` connections, so fetch
all and match client-side:

```bash
source ~/.orchestra/env
curl -s https://api.linear.app/graphql \
  -H "Authorization: $LINEAR_TOKEN" -H "Content-Type: application/json" \
  -X POST -d '{ "query": "{ projects { nodes { id name } } users { nodes { id name email } } }" }' \
  | python3 -c "
import sys, json
d = json.load(sys.stdin)['data']
for p in d['projects']['nodes']:
    if 'GPU'.lower() in p['name'].lower(): print('PROJECT', p)
for u in d['users']['nodes']:
    if 'rohan' in (u.get('email') or '').lower(): print('USER', u)
"
```

Project and team are different things in Linear. "GPU Manager" here is a
**project** (team is `SKY` / SkyPilot). Check both `projects` and `teams` if a
name doesn't show up where you expect.

## Step 2 — Query issues (reusable script)

Save as `linear-issues.py` and run:
`python3 linear-issues.py --project "GPU manager" --email rohan@assemblesys.com --priority high`

```python
#!/usr/bin/env python3
import argparse, json, os, sys, urllib.request
from collections import Counter

PRIORITY = {0: "No priority", 1: "Urgent", 2: "High", 3: "Medium", 4: "Low"}
# Map a --priority word to the set of numeric levels it covers.
# "high" => Urgent + High. Using explicit eq's (not lte) avoids the
# priority-0 trap where {lte: 2} also matches "No priority".
PRIORITY_LEVELS = {
    "urgent": (1,),
    "high":   (1, 2),
    "medium": (3,),
    "low":    (4,),
}
DONE_WORDS = ("done", "canceled", "cancelled")

def gql(token, query, variables):
    body = json.dumps({"query": query, "variables": variables}).encode()
    req = urllib.request.Request(
        "https://api.linear.app/graphql", data=body,
        headers={"Authorization": token, "Content-Type": "application/json"})
    d = json.loads(urllib.request.urlopen(req).read())
    if "errors" in d:
        sys.exit("GraphQL errors:\n" + json.dumps(d["errors"], indent=2))
    return d["data"]

def resolve(token, project_name, email):
    d = gql(token, "{ projects { nodes { id name } } users { nodes { id name email } } }", {})
    proj = next((p for p in d["projects"]["nodes"]
                 if p["name"].lower() == project_name.lower()), None)
    if not proj:
        proj = next((p for p in d["projects"]["nodes"]
                     if project_name.lower() in p["name"].lower()), None)
    if not proj:
        sys.exit(f"No project matching {project_name!r}")
    user = next((u for u in d["users"]["nodes"]
                 if (u.get("email") or "").lower() == email.lower()), None)
    if not user:
        sys.exit(f"No user matching {email!r}")
    return proj["id"], proj["name"], user["id"]

def fetch_issues(token, project_id, user_id, priority_levels):
    q = """
    query($f: IssueFilter!, $after: String, $first: Int) {
      issues(filter: $f, after: $after, first: $first) {
        pageInfo { hasNextPage endCursor }
        nodes { identifier title priority url state { name } }
      }
    }"""
    # OR of exact priority matches => never accidentally includes priority 0.
    f = {"project": {"id": {"eq": project_id}},
         "assignee": {"id": {"eq": user_id}},
         "or": [{"priority": {"eq": p}} for p in priority_levels]}
    out, after = [], None
    while True:
        d = gql(token, q, {"f": f, "after": after, "first": 250})
        conn = d["issues"]
        out.extend(conn["nodes"])
        if not conn["pageInfo"]["hasNextPage"]:
            break
        after = conn["pageInfo"]["endCursor"]
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--project", required=True)
    ap.add_argument("--email", required=True)
    ap.add_argument("--priority", default="high",
                    choices=list(PRIORITY_LEVELS))
    ap.add_argument("--all", action="store_true",
                    help="also print Done issues, not just open")
    args = ap.parse_args()

    token = os.environ.get("LINEAR_TOKEN")
    if not token:
        sys.exit("LINEAR_TOKEN not set (run: source ~/.orchestra/env)")

    pid, pname, uid = resolve(token, args.project, args.email)
    levels = list(PRIORITY_LEVELS[args.priority])
    issues = fetch_issues(token, pid, uid, levels)
    issues.sort(key=lambda i: (i["priority"], i["identifier"]))
    open_i = [i for i in issues if i["state"]["name"].lower() not in DONE_WORDS]
    done_i = [i for i in issues if i["state"]["name"].lower() in DONE_WORDS]

    print(f"Project: {pname}")
    print(f"Assignee: {args.email}  Priority: {args.priority}")
    print(f"TOTAL: {len(issues)}  (open: {len(open_i)}, done: {len(done_i)})")
    c = Counter(i["priority"] for i in issues)
    print("by priority:", {PRIORITY.get(k, k): v for k, v in c.items()})
    print("\n===== OPEN =====")
    for i in open_i:
        print(f"[{PRIORITY.get(i['priority'], i['priority'])}] "
              f"{i['identifier']} ({i['state']['name']}) - {i['title']}\n   {i['url']}")
    if args.all:
        print("\n===== DONE =====")
        for i in done_i:
            print(f"[{PRIORITY.get(i['priority'], i['priority'])}] "
                  f"{i['identifier']} - {i['title']}\n   {i['url']}")

if __name__ == "__main__":
    main()
```

> Note: the key correctness point is that `fetch_issues` builds
> `or: [{priority:{eq:p}} for p in levels]` — exact `eq` matches instead of a
> range — to avoid the priority-0 trap where `{lte: 2}` matches "No priority".

## Step 3 — Expected output format

```
Project: GPU manager
Assignee: rohan@assemblesys.com  Priority: high
TOTAL: 62  (open: 16, done: 46)
by priority: {'Urgent': 7, 'High': 55}

===== OPEN =====
[High] SKY-5856 (Todo) - [GPU Manager] Integrate GPU metrics checks ...
   https://linear.app/skypilot/issue/SKY-5856/...
...
```

Terminal states to treat as "done": `Done`, `Canceled`. Everything else
(`Todo`, `In Progress`, `In Review`, `Backlog`, `Triage`, `Paused`) is open.

## Common errors and fixes

| Error | Cause | Fix |
|-------|-------|-----|
| `after cannot be used without at least one of first` | Paged with `after` but no `first` | Always pass `first: 250` with `after` |
| No-priority issues appear in "high priority" results | `priority: {lte: 2}` matches `0` | Use `or: [{eq:1},{eq:2}]` instead of a range |
| Only 50 results returned | Default page size, no paging loop | Loop on `pageInfo.hasNextPage`/`endCursor` |
| `Cannot query field "totalCount"` | Field doesn't exist on `IssueConnection` | Count `nodes` in code |
| Project name not found under `projects` | It may be a **team**, not a project | Also check `teams { nodes { id name key } }` |
| `401 Unauthorized` | Token missing/invalid | `source ~/.orchestra/env`; rotate key in Linear → Settings → API |

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
