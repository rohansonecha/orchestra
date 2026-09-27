// Git worktree management — each dispatched session gets its own worktree.
//
// Layout mirrors Claude Code's `.claude/worktrees/`:
//
//   <repo>/.orchestra/worktrees/<name>/   on branch worktree-<name>
//
// branched from origin/<default-branch> after a fetch (Claude Code's
// `fresh` baseRef). The main checkout is never touched — no pull, no
// checkout — so a dirty or feature-branch main checkout is fine.
//
// `.orchestra/` is added to the repo's .git/info/exclude (not .gitignore,
// which is tracked) so worktrees never show up as untracked files.
//
// Gitignored files listed in `.worktreeinclude` (e.g. `.env`) are copied
// into each new worktree, same file format and semantics as Claude Code.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::repo::{git, Repo};

pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    /// Non-fatal problems (fetch failed, include file skipped, ...).
    pub warnings: Vec<String>,
}

pub fn branch_name(name: &str) -> String {
    format!("worktree-{name}")
}

fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Create `<repo>/.orchestra/worktrees/<name>` on branch `worktree-<name>`,
/// branched from the repo's default base (origin/<default>, fetched first).
pub fn create_worktree(repo: &Repo, name: &str) -> Result<Worktree, String> {
    create_worktree_from(repo, name, None)
}

/// Same, from an explicit commit/ref (no fetch). Used when forking a
/// session so the copy starts from the code the conversation was about.
pub fn create_worktree_from(repo: &Repo, name: &str, base: Option<&str>) -> Result<Worktree, String> {
    let mut warnings = Vec::new();
    let branch = branch_name(name);
    let dot = repo.root.join(".orchestra");
    let parent = repo.worktrees_dir();

    // A committed symlink at .orchestra or .orchestra/worktrees could point
    // worktree creation outside the repo. Refuse, like Claude Code does.
    for p in [&dot, &parent] {
        if p.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("{} is a symlink; remove it and retry", p.display()));
        }
    }
    std::fs::create_dir_all(&parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    ensure_excluded(&repo.root);

    let base_ref = base.unwrap_or(&repo.base_ref);
    if let (None, Some(b)) = (base, repo.remote_branch()) {
        if let Err(e) = run(&repo.root, &["fetch", "--quiet", "origin", b]) {
            // Offline or auth trouble: branch from the last-fetched ref.
            warnings.push(format!("fetch failed, using cached {}: {e}", repo.base_ref));
        }
    }

    let path = parent.join(name);
    let path_s = path.to_string_lossy().to_string();
    run(
        &repo.root,
        &["worktree", "add", "--quiet", "--no-track", "-b", &branch, &path_s, base_ref],
    )?;

    warnings.extend(copy_worktree_includes(&repo.root, &path));
    Ok(Worktree { path, branch, warnings })
}

/// Add `/.orchestra/` to the repo's info/exclude once.
fn ensure_excluded(root: &Path) {
    let Some(common) = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]) else {
        return;
    };
    let info = PathBuf::from(common).join("info");
    let exclude = info.join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == "/.orchestra/" || l.trim() == ".orchestra/") {
        return;
    }
    let _ = std::fs::create_dir_all(&info);
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str("# orchestra session worktrees\n/.orchestra/\n");
    let _ = std::fs::write(&exclude, next);
}

/// Copy gitignored files matching `.worktreeinclude` (gitignore syntax) from
/// the main checkout into a new worktree. Tracked files are already there;
/// symlinks and anything resolving outside the worktree are skipped.
fn copy_worktree_includes(root: &Path, worktree: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let include = root.join(".worktreeinclude");
    if !include.is_file() {
        return warnings;
    }
    let include_s = include.to_string_lossy().to_string();
    let listed = match run(
        root,
        &["ls-files", "--others", "--ignored", "--exclude-from", &include_s, "-z"],
    ) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(format!(".worktreeinclude skipped: {e}"));
            return warnings;
        }
    };
    for rel in listed.split('\0').filter(|s| !s.is_empty()) {
        // Never copy our own worktrees into each other.
        if rel.starts_with(".orchestra/") || rel.starts_with(".claude/worktrees/") {
            continue;
        }
        let src = root.join(rel);
        if src.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(true) {
            warnings.push(format!("skipped symlink in .worktreeinclude: {rel}"));
            continue;
        }
        let dst = worktree.join(rel);
        if !dst.starts_with(worktree) || rel.split('/').any(|c| c == "..") {
            continue;
        }
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::copy(&src, &dst) {
            warnings.push(format!("copy {rel}: {e}"));
        }
    }
    warnings
}

/// Uncommitted changes, or commits not on `base_ref`. Used to warn before
/// deleting a session. Returns a short description, or None when clean.
pub fn pending_changes(worktree: &Path, base_ref: &str) -> Option<String> {
    if !worktree.exists() {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(s) = git(worktree, &["status", "--porcelain"]) {
        let n = s.lines().count();
        if n > 0 {
            parts.push(format!("{n} uncommitted file(s)"));
        }
    }
    if let Some(s) = git(worktree, &["rev-list", "--count", &format!("{base_ref}..HEAD")]) {
        if let Ok(n) = s.parse::<u32>() {
            if n > 0 {
                parts.push(format!("{n} commit(s) not on {base_ref}"));
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Remove a worktree and delete its branch.
pub fn remove_worktree(repo_root: &Path, worktree_path: &Path, branch: &str) -> Result<(), String> {
    let wt = worktree_path.to_string_lossy().to_string();
    let removed = run(repo_root, &["worktree", "remove", "--force", &wt]);
    // Branch delete is best-effort: it may already be gone or renamed.
    let _ = run(repo_root, &["branch", "-D", branch]);
    removed.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{detect, tests::clone_fixture};

    #[test]
    fn creates_nested_worktree_off_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = clone_fixture(tmp.path());
        // Main checkout on some other branch with a dirty file: must be
        // left alone.
        crate::repo::tests::sh(&clone, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(clone.join("dirty"), "x").unwrap();

        let repo = detect(&clone).unwrap();
        let wt = create_worktree(&repo, "fix-bug").unwrap();
        assert!(wt.path.ends_with(".orchestra/worktrees/fix-bug"));
        assert_eq!(wt.branch, "worktree-fix-bug");
        assert_eq!(
            git(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            "worktree-fix-bug"
        );
        assert_eq!(
            git(&wt.path, &["rev-parse", "HEAD"]),
            git(&clone, &["rev-parse", "origin/trunk"])
        );
        // Main checkout untouched and .orchestra/ hidden from status.
        assert_eq!(git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(), "feature");
        let status = git(&clone, &["status", "--porcelain"]).unwrap();
        assert_eq!(status, "?? dirty");
        // Exclude line is written once.
        create_worktree(&repo, "second").unwrap();
        let excl = std::fs::read_to_string(clone.join(".git/info/exclude")).unwrap();
        assert_eq!(excl.matches("/.orchestra/").count(), 1);
    }

    #[test]
    fn copies_worktreeinclude_files() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = clone_fixture(tmp.path());
        std::fs::write(clone.join(".gitignore"), ".env\nsecrets/\nbuild/\n").unwrap();
        std::fs::write(clone.join(".worktreeinclude"), ".env\nsecrets/\n").unwrap();
        std::fs::write(clone.join(".env"), "A=1").unwrap();
        std::fs::create_dir_all(clone.join("secrets")).unwrap();
        std::fs::write(clone.join("secrets/key"), "k").unwrap();
        std::fs::create_dir_all(clone.join("build")).unwrap();
        std::fs::write(clone.join("build/out"), "big").unwrap();

        let repo = detect(&clone).unwrap();
        let wt = create_worktree(&repo, "inc").unwrap();
        assert_eq!(std::fs::read_to_string(wt.path.join(".env")).unwrap(), "A=1");
        assert!(wt.path.join("secrets/key").exists());
        assert!(!wt.path.join("build/out").exists(), "not listed in .worktreeinclude");
    }

    #[test]
    fn refuses_symlinked_dot_orchestra() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = clone_fixture(tmp.path());
        std::os::unix::fs::symlink(tmp.path(), clone.join(".orchestra")).unwrap();
        let repo = detect(&clone).unwrap();
        let err = create_worktree(&repo, "x").err().unwrap();
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn pending_changes_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = clone_fixture(tmp.path());
        let repo = detect(&clone).unwrap();
        let wt = create_worktree(&repo, "w").unwrap();
        assert_eq!(pending_changes(&wt.path, &repo.base_ref), None);
        std::fs::write(wt.path.join("new"), "x").unwrap();
        assert_eq!(
            pending_changes(&wt.path, &repo.base_ref).as_deref(),
            Some("1 uncommitted file(s)")
        );
        remove_worktree(&repo.root, &wt.path, &wt.branch).unwrap();
        assert!(!wt.path.exists());
        assert!(git(&clone, &["rev-parse", "--verify", "--quiet", "refs/heads/worktree-w"]).is_none());
    }
}
