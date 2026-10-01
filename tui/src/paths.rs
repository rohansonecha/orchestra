// paths.rs — Locations orchestra reads and writes, resolved from $HOME so the
// TUI works for any user (not just the main box's `sky` user).

use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/sky"))
}

/// ~/.orchestra — per-user state (sessions, tree store, skills, config).
pub fn state_dir() -> PathBuf {
    home().join(".orchestra")
}

pub fn sessions_dir() -> PathBuf {
    state_dir().join("sessions")
}

/// pi conversation files, keyed by the session's stable id (not its name)
/// so a rename never moves a file a running pi is appending to.
pub fn pi_sessions_dir() -> PathBuf {
    state_dir().join("pi-sessions")
}

/// Per-session system prompt files (orchestra note + skills), by id.
pub fn prompts_dir() -> PathBuf {
    state_dir().join("prompts")
}

pub fn env_file() -> PathBuf {
    state_dir().join("env")
}

pub fn skills_dir() -> PathBuf {
    state_dir().join("skills")
}

pub fn config_file() -> PathBuf {
    state_dir().join("config.json")
}

pub fn tree_store_dir() -> PathBuf {
    state_dir().join("tree")
}

/// ~/orchestra — the orchestra checkout itself (used by `orchestra upgrade`).
pub fn orchestra_checkout() -> PathBuf {
    home().join("orchestra")
}

/// Where sessions created before per-repo worktrees lived. Only used to
/// clean up / rename those old sessions.
pub fn legacy_worktrees_dir() -> PathBuf {
    home().join("orchestra").join("worktrees")
}

pub fn legacy_repo() -> PathBuf {
    home().join("work-repos").join("prototype")
}

pub fn claude_projects_dir() -> PathBuf {
    home().join(".claude").join("projects")
}

pub fn codex_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"))
}
