// repo.rs — Find the git repo orchestra was launched from.
//
// Mirrors Claude Code: the repo is the git toplevel of the launch directory.
// Launching from inside a linked worktree resolves to the *main* checkout
// (via --git-common-dir), so new worktrees always nest under one
// <repo>/.orchestra/worktrees/ instead of inside each other.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// Main checkout root.
    pub root: PathBuf,
    /// Ref new worktrees branch from, e.g. `origin/main`. Falls back to
    /// `HEAD` for repos without an `origin` remote.
    pub base_ref: String,
}

impl Repo {
    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join(".orchestra").join("worktrees")
    }

    /// The branch part of `base_ref` (`origin/main` → `main`), if it is a
    /// remote-tracking ref. Used to `git fetch origin <branch>`.
    pub fn remote_branch(&self) -> Option<&str> {
        self.base_ref.strip_prefix("origin/")
    }
}

pub fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Detect the repo containing `dir`. Returns None outside a git repo.
pub fn detect(dir: &Path) -> Option<Repo> {
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = PathBuf::from(common);
    // A normal checkout's common dir is <root>/.git. Anything else (bare
    // repo, GIT_DIR tricks) falls back to the plain toplevel.
    let root = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent()?.to_path_buf()
    } else {
        PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?)
    };
    let base_ref = default_base_ref(&root);
    Some(Repo { root, base_ref })
}

/// Resolve `origin/<default-branch>` without touching the network when
/// possible: origin/HEAD first, then common names, then `HEAD`.
fn default_base_ref(root: &Path) -> String {
    if let Some(r) = git(root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if !r.is_empty() {
            return r;
        }
    }
    if git(root, &["remote", "get-url", "origin"]).is_some() {
        // origin/HEAD unset (common on clones made with --single-branch or
        // by older tooling). Ask the remote once.
        if let Some(out) = git(root, &["ls-remote", "--symref", "origin", "HEAD"]) {
            if let Some(branch) = parse_ls_remote_head(&out) {
                return format!("origin/{branch}");
            }
        }
        for b in ["main", "master"] {
            if git(root, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{b}")]).is_some() {
                return format!("origin/{b}");
            }
        }
    }
    "HEAD".to_string()
}

/// The repo for new worktrees: the one containing `launch_dir`, or else
/// `ORCHESTRA_WORK_REPO` (from the environment or ~/.orchestra/env). The
/// fallback keeps worktree isolation on the main box, where orchestra is
/// started from $HOME after `sky ssh`.
pub fn detect_for_launch(launch_dir: &Path) -> Option<Repo> {
    detect(launch_dir).or_else(|| {
        let configured = std::env::var("ORCHESTRA_WORK_REPO").ok().or_else(|| {
            let env = std::fs::read_to_string(crate::paths::env_file()).ok()?;
            env_file_value(&env, "ORCHESTRA_WORK_REPO")
        })?;
        detect(Path::new(&configured))
    })
}

/// `KEY=value` lookup in a shell env file (optional `export`, quotes).
fn env_file_value(content: &str, key: &str) -> Option<String> {
    content.lines().rev().find_map(|l| {
        let l = l.trim();
        let l = l.strip_prefix("export ").unwrap_or(l);
        let v = l.strip_prefix(key)?.strip_prefix('=')?;
        let v = v.trim().trim_matches('"').trim_matches('\'');
        let v = match v.strip_prefix("$HOME") {
            Some(rest) => format!("{}{rest}", crate::paths::home().display()),
            None => v.to_string(),
        };
        (!v.is_empty()).then_some(v)
    })
}

/// Parse `ref: refs/heads/main\tHEAD` from `git ls-remote --symref`.
fn parse_ls_remote_head(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.strip_prefix("ref: refs/heads/"))
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_string)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// origin (bare, default branch `trunk`) + a clone of it.
    pub fn clone_fixture(tmp: &Path) -> PathBuf {
        let origin = tmp.join("origin.git");
        let seed = tmp.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        sh(&seed, &["init", "-q", "-b", "trunk"]);
        std::fs::write(seed.join("README"), "hi").unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-q", "-m", "init"]);
        sh(tmp, &["clone", "-q", "--bare", seed.to_str().unwrap(), origin.to_str().unwrap()]);
        let clone = tmp.join("clone");
        sh(tmp, &["clone", "-q", origin.to_str().unwrap(), clone.to_str().unwrap()]);
        clone
    }

    #[test]
    fn parses_ls_remote() {
        assert_eq!(
            parse_ls_remote_head("ref: refs/heads/main\tHEAD\nabc123\tHEAD\n"),
            Some("main".to_string())
        );
        assert_eq!(parse_ls_remote_head("abc\tHEAD"), None);
    }

    #[test]
    fn env_file_lookup() {
        let env = "A=1\nexport ORCHESTRA_WORK_REPO=\"/w/p\"\n# ORCHESTRA_WORK_REPO=/no\n";
        assert_eq!(env_file_value(env, "ORCHESTRA_WORK_REPO").as_deref(), Some("/w/p"));
        assert_eq!(env_file_value("ORCHESTRA_WORK_REPO=", "ORCHESTRA_WORK_REPO"), None);
        assert_eq!(env_file_value("ORCHESTRA_WORK_REPOX=1", "ORCHESTRA_WORK_REPO"), None);
        let home = crate::paths::home().display().to_string();
        assert_eq!(
            env_file_value("ORCHESTRA_WORK_REPO=$HOME/r", "ORCHESTRA_WORK_REPO"),
            Some(format!("{home}/r"))
        );
    }

    #[test]
    fn not_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect(tmp.path()).is_none());
    }

    #[test]
    fn detects_root_and_default_branch_from_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = clone_fixture(tmp.path());
        let sub = clone.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let repo = detect(&sub).unwrap();
        assert_eq!(repo.root.canonicalize().unwrap(), clone.canonicalize().unwrap());
        assert_eq!(repo.base_ref, "origin/trunk");
        assert_eq!(repo.remote_branch(), Some("trunk"));
    }

    #[test]
    fn linked_worktree_resolves_to_main_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = clone_fixture(tmp.path());
        let wt = tmp.path().join("wt");
        sh(&clone, &["worktree", "add", "-q", "-b", "x", wt.to_str().unwrap()]);
        let repo = detect(&wt).unwrap();
        assert_eq!(repo.root.canonicalize().unwrap(), clone.canonicalize().unwrap());
    }

    #[test]
    fn no_origin_uses_head() {
        let tmp = tempfile::tempdir().unwrap();
        sh(tmp.path(), &["init", "-q"]);
        let repo = detect(tmp.path()).unwrap();
        assert_eq!(repo.base_ref, "HEAD");
        assert_eq!(repo.remote_branch(), None);
    }
}
