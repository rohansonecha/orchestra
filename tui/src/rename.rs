// rename.rs — Full session rename across the four places a session's
// identity lives:
//
//   1. tmux session name
//   2. git worktree dir (<worktrees>/<name>) + branch (worktree-<name>)
//   3. state dir + ready marker (<sessions>/<name>{,.ready})
//   4. tree-store node (id `session-<name>`, plus `name` / `tmux_session`)
//
// Why the tree-store node must move together with the tmux session: the
// collector identifies session nodes by `tmux_session` and derives *new*
// node ids as `session-<tmux_name>`. If tmux is renamed without moving the
// node id (and the root's `children` entry), the next 30s pull sees an
// unknown tmux session and creates a duplicate node next to the renamed one.
//
// The display label is separate from the underlying name: `display_name`
// keeps whatever the user typed ("My Researcher"), while the tmux/worktree
// name is sanitized ("my-researcher").

use std::path::Path;
use std::process::Command;

/// Filesystem + store locations a rename touches. Injected so tests can
/// point at a temp dir. Defaults must match the constants in main.rs /
/// session.rs.
pub struct Paths {
    pub repo: String,
    pub worktrees: String,
    pub sessions: String,
    pub store: String,
    pub root_id: String,
}

pub fn default_paths() -> Paths {
    Paths {
        repo: "/home/sky/work-repos/prototype".to_string(),
        worktrees: "/home/sky/orchestra/worktrees".to_string(),
        sessions: "/home/sky/.orchestra/sessions".to_string(),
        store: "/home/sky/.orchestra/tree".to_string(),
        root_id: "agent-main-box".to_string(),
    }
}

/// Result of a rename attempt.
pub struct Outcome {
    /// The sanitized underlying name, or empty on refusal.
    pub new_name: String,
    /// Human-readable steps (for the status line / CLI output).
    pub logs: Vec<String>,
}

/// Sanitize a candidate name into a valid tmux/git identifier: lowercase,
/// alphanumeric + hyphens, collapsed runs, no leading/trailing hyphens.
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_hyphen = false;
    for c in raw.trim().chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_hyphen = false;
        } else if !prev_hyphen && !out.is_empty() {
            out.push('-');
            prev_hyphen = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

fn tmux_alive(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Rename a session end-to-end. `display` is the raw user input; the
/// underlying name is sanitized from it. Returns the new name (empty on
/// refusal) plus logs.
pub fn rename_session(old: &str, display: &str, p: &Paths) -> Outcome {
    let mut logs = Vec::new();
    let new = sanitize_name(display);

    if new.is_empty() {
        return Outcome {
            new_name: String::new(),
            logs: vec!["invalid name (no usable characters)".to_string()],
        };
    }
    if new == old {
        // Nothing physical to rename; caller updates the label only.
        return Outcome { new_name: old.to_string(), logs };
    }
    if tmux_alive(&new) {
        return Outcome {
            new_name: String::new(),
            logs: vec![format!("a session named '{new}' already exists")],
        };
    }

    // 1. tmux session.
    if tmux_alive(old) {
        let ok = Command::new("tmux")
            .args(["rename-session", "-t", old, &new])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        logs.push(if ok {
            format!("tmux: {old} -> {new}")
        } else {
            format!("tmux rename failed ({old})")
        });
    } else {
        logs.push(format!("tmux: {old} not running (skipped)"));
    }

    // 2. git worktree dir + branch.
    let old_wt = format!("{}/{old}", p.worktrees);
    let new_wt = format!("{}/{new}", p.worktrees);
    if Path::new(&old_wt).exists() {
        let moved = Command::new("git")
            .args(["worktree", "move", &old_wt, &new_wt])
            .current_dir(&p.repo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if moved {
            logs.push(format!("worktree: {old} -> {new}"));
            // Rename the branch from inside the worktree (renaming a branch
            // checked out in a linked worktree must be done there).
            let branch_ok = Command::new("git")
                .args(["branch", "-m", &format!("worktree-{new}")])
                .current_dir(&new_wt)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            logs.push(if branch_ok {
                format!("branch: worktree-{old} -> worktree-{new}")
            } else {
                "branch rename failed".to_string()
            });
        } else {
            logs.push(format!("worktree move failed ({old_wt})"));
        }
    } else {
        logs.push(format!("worktree: {old_wt} not found (skipped)"));
    }

    // 3. state dir + ready marker.
    let old_dir = Path::new(&p.sessions).join(old);
    let new_dir = Path::new(&p.sessions).join(&new);
    if old_dir.exists() {
        std::fs::create_dir_all(&new_dir).ok();
        let old_state = old_dir.join("state.json");
        if let Ok(content) = std::fs::read_to_string(&old_state) {
            if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("name".to_string(), serde_json::json!(new));
                }
                let _ = std::fs::write(
                    new_dir.join("state.json"),
                    serde_json::to_string_pretty(&v).unwrap_or_default(),
                );
            }
        }
        let _ = std::fs::remove_dir_all(&old_dir);
        logs.push(format!("state dir: {old} -> {new}"));
    }
    let old_ready = Path::new(&p.sessions).join(format!("{old}.ready"));
    if old_ready.exists() {
        let _ = std::fs::rename(&old_ready, Path::new(&p.sessions).join(format!("{new}.ready")));
    }

    // 4. tree-store node + root children.
    if rename_node_file(old, &new, display, p) {
        logs.push(format!("tree node: session-{old} -> session-{new}"));
    }

    Outcome { new_name: new, logs }
}

/// Rewrite `nodes/session-<old>.json` as `nodes/session-<new>.json` and
/// repoint the root's `children` entry. Returns false if no node file
/// exists (session predates the tree store, or collector never ran).
fn rename_node_file(old: &str, new: &str, display: &str, p: &Paths) -> bool {
    let nodes = Path::new(&p.store).join("nodes");
    let old_path = nodes.join(format!("session-{old}.json"));
    let new_path = nodes.join(format!("session-{new}.json"));

    let Ok(content) = std::fs::read_to_string(&old_path) else {
        return false;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert("id".to_string(), serde_json::json!(format!("session-{new}")));
        obj.insert("name".to_string(), serde_json::json!(new));
        obj.insert("tmux_session".to_string(), serde_json::json!(new));
        obj.insert("display_name".to_string(), serde_json::json!(display));
    }
    if std::fs::write(&new_path, serde_json::to_string_pretty(&v).unwrap_or_default()).is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&old_path);

    // Repoint the root's children list so the collector keeps matching the
    // node to the (renamed) tmux session instead of creating a duplicate.
    let root_path = nodes.join(format!("{}.json", p.root_id));
    if let Ok(rc) = std::fs::read_to_string(&root_path) {
        if let Ok(mut rv) = serde_json::from_str::<serde_json::Value>(&rc) {
            let old_id = format!("session-{old}");
            let new_id = format!("session-{new}");
            if let Some(children) = rv.get_mut("children").and_then(|c| c.as_array_mut()) {
                for c in children.iter_mut() {
                    if c.as_str() == Some(old_id.as_str()) {
                        *c = serde_json::json!(new_id);
                    }
                }
            }
            let _ = std::fs::write(&root_path, serde_json::to_string_pretty(&rv).unwrap_or_default());
        }
    }
    true
}

/// Set only the display label of a node (used for agents, and for sessions
/// whose sanitized name is unchanged).
pub fn set_display_name(id: &str, display: &str, p: &Paths) -> bool {
    let path = Path::new(&p.store).join("nodes").join(format!("{id}.json"));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert("display_name".to_string(), serde_json::json!(display));
    }
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> Paths {
        Paths {
            repo: root.join("repo").to_string_lossy().to_string(),
            worktrees: root.join("worktrees").to_string_lossy().to_string(),
            sessions: root.join("sessions").to_string_lossy().to_string(),
            store: root.join("tree").to_string_lossy().to_string(),
            root_id: "agent-main-box".to_string(),
        }
    }

    fn write_node(p: &Paths, id: &str, kind: &str, tmux: &str, display: Option<&str>) {
        let nodes = Path::new(&p.store).join("nodes");
        std::fs::create_dir_all(&nodes).unwrap();
        let node = serde_json::json!({
            "id": id, "kind": kind, "parent_id": "agent-main-box",
            "name": tmux, "tmux_session": tmux, "display_name": display,
            "state": "working", "children": []
        });
        std::fs::write(
            nodes.join(format!("{id}.json")),
            serde_json::to_string_pretty(&node).unwrap(),
        )
        .unwrap();
    }

    fn write_root(p: &Paths, children: &[&str]) {
        let nodes = Path::new(&p.store).join("nodes");
        std::fs::create_dir_all(&nodes).unwrap();
        let root = serde_json::json!({
            "id": p.root_id, "kind": "agent", "parent_id": null,
            "name": "main-box", "state": "working", "children": children
        });
        std::fs::write(
            nodes.join(format!("{}.json", p.root_id)),
            serde_json::to_string_pretty(&root).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn sanitize_cases() {
        assert_eq!(sanitize_name("My Researcher"), "my-researcher");
        assert_eq!(sanitize_name("  spaced  out  "), "spaced-out");
        assert_eq!(sanitize_name("a--b"), "a-b");
        assert_eq!(sanitize_name("--lead/trail--"), "lead-trail");
        assert_eq!(sanitize_name("!!!"), "");
        assert_eq!(sanitize_name("Fix bug #42"), "fix-bug-42");
        assert_eq!(sanitize_name("Already-ok"), "already-ok");
    }

    #[test]
    fn empty_name_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let out = rename_session("old", "!!!", &p);
        assert!(out.new_name.is_empty());
        assert!(out.logs[0].contains("invalid"));
    }

    #[test]
    fn unchanged_name_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let out = rename_session("my-session", "My Session", &p);
        assert_eq!(out.new_name, "my-session");
        assert!(out.logs.is_empty());
    }

    #[test]
    fn moves_state_dir_and_ready_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let old_dir = Path::new(&p.sessions).join("old-name");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(
            old_dir.join("state.json"),
            r#"{"name":"old-name","prompt":"hi","state":"working"}"#,
        )
        .unwrap();
        std::fs::write(Path::new(&p.sessions).join("old-name.ready"), "").unwrap();

        let out = rename_session("old-name", "New Name", &p);
        assert_eq!(out.new_name, "new-name");

        assert!(!old_dir.exists());
        let new_state = Path::new(&p.sessions).join("new-name/state.json");
        assert!(new_state.exists());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&new_state).unwrap()).unwrap();
        assert_eq!(v["name"], "new-name");
        assert_eq!(v["prompt"], "hi", "other fields preserved");
        assert!(Path::new(&p.sessions).join("new-name.ready").exists());
        assert!(!Path::new(&p.sessions).join("old-name.ready").exists());
    }

    #[test]
    fn moves_node_id_and_repoints_root_children() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        write_node(&p, "session-old-name", "session", "old-name", None);
        write_node(&p, "session-other", "session", "other", None);
        write_root(&p, &["session-old-name", "session-other"]);

        let out = rename_session("old-name", "New Name", &p);
        assert_eq!(out.new_name, "new-name");

        let nodes = Path::new(&p.store).join("nodes");
        assert!(!nodes.join("session-old-name.json").exists());
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(nodes.join("session-new-name.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["id"], "session-new-name");
        assert_eq!(v["name"], "new-name");
        assert_eq!(v["tmux_session"], "new-name");
        assert_eq!(v["display_name"], "New Name");

        // Root children repointed, sibling untouched.
        let root: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(nodes.join("agent-main-box.json")).unwrap(),
        )
        .unwrap();
        let children: Vec<&str> = root["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(children, vec!["session-new-name", "session-other"]);
    }

    #[test]
    fn set_display_name_updates_label_only() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        write_node(&p, "session-x", "session", "x", None);
        assert!(set_display_name("session-x", "Pretty Label", &p));
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(Path::new(&p.store).join("nodes/session-x.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["display_name"], "Pretty Label");
        assert_eq!(v["tmux_session"], "x", "underlying name untouched");
    }

    #[test]
    fn missing_node_file_is_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        // No tree store at all — physical rename still reports success.
        let out = rename_session("ghost", "Renamed", &p);
        assert_eq!(out.new_name, "renamed");
        assert!(out.logs.iter().any(|l| l.contains("not found") || l.contains("not running")));
    }
}
