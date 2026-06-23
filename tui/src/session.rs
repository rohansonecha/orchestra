// Session management — each session is a pi process running inside a tmux
// session in a worktree. tmux gives us attach/detach for free.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const SESSIONS_DIR: &str = "/home/sky/.orchestra/sessions";
const ENV_FILE: &str = "/home/sky/.orchestra/env";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Working,
    NeedsInput,
    Idle,
    Completed,
    Failed,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub prompt: String,
    pub worktree_path: String,
    pub created_at: u64,
    pub last_activity: u64,
    pub state: SessionState,
}

impl Session {
    pub fn new(name: String, prompt: String, worktree_path: String) -> Self {
        let now = unix_now();
        Self {
            name,
            prompt,
            worktree_path,
            created_at: now,
            last_activity: now,
            state: SessionState::Working,
        }
    }

    /// Refresh state by checking if the tmux session is still alive.
    pub fn refresh_state(&mut self) {
        let alive = Command::new("tmux")
            .arg("has-session")
            .arg("-t")
            .arg(&self.name)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if alive {
            self.state = SessionState::Working;
        } else if self.state == SessionState::Working {
            // tmux session ended — the process finished
            self.state = SessionState::Completed;
        }
    }
}

/// Load env vars from ~/.orchestra/env (written during main-box setup).
/// Secrets are only available during setup scripts, not SSH sessions.
/// The TUI loads them here so pi can resolve $GLM_API_KEY etc.
pub fn load_env() -> HashMap<String, String> {
    let mut vars = HashMap::new();
    if let Ok(content) = std::fs::read_to_string(ENV_FILE) {
        for line in content.lines() {
            if let Some((key, value)) = line.split_once('=') {
                vars.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }
    vars
}

/// Spawn pi inside a detached tmux session. The initial prompt is run via
/// `pi -p` (print mode) so it starts processing immediately on dispatch.
/// After print mode finishes, interactive pi starts in a restart loop —
/// if pi exits (crash, Ctrl+C, etc.), it restarts so the session stays
/// attachable. pi persists conversation history by --name, so restarts
/// resume the existing conversation.
///
/// Left arrow and Ctrl+C are bound to detach-client so the user can
/// return to the orchestra TUI without killing pi. Use Escape to
/// interrupt pi operations.
pub fn spawn_pi(name: &str, worktree_path: &str, initial_prompt: &str) -> std::io::Result<()> {
    // Escape single quotes for bash — the only char that needs escaping
    // inside single-quoted strings.
    let escaped = initial_prompt.replace('\'', "'\\''");
    let cmd_str = format!(
        "set -a; source {ENV_FILE}; set +a; pi -p '{escaped}' --name {name} --provider glm --model zai-org/GLM-5.2-FP8; while true; do pi --name {name} --provider glm --model zai-org/GLM-5.2-FP8; sleep 1; done"
    );

    let status = Command::new("tmux")
        .arg("new-session")
        .arg("-d")
        .arg("-s")
        .arg(name)
        .arg("-c")
        .arg(worktree_path)
        .arg("bash")
        .arg("-c")
        .arg(&cmd_str)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "tmux new-session failed",
        ));
    }

    // Global tmux settings (idempotent — safe to run for every session).
    // Left arrow and Ctrl+C detach back to the orchestra TUI (root table
    // = no prefix needed). Ctrl+C would otherwise exit pi and kill the
    // session.
    for key in ["Left", "C-c"] {
        Command::new("tmux")
            .arg("bind-key")
            .arg("-n")
            .arg(key)
            .arg("detach-client")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
    }

    // Keep pane alive even if all processes exit, so the user can see
    // what happened instead of getting a dead session.
    Command::new("tmux")
        .arg("set")
        .arg("-g")
        .arg("remain-on-exit")
        .arg("on")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    // Enable extended-keys for proper Enter/modified key handling.
    Command::new("tmux")
        .arg("set")
        .arg("-g")
        .arg("extended-keys")
        .arg("on")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    Ok(())
}

pub fn load_sessions() -> Vec<Session> {
    let mut sessions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(SESSIONS_DIR) {
        for entry in entries.flatten() {
            let path = entry.path().join("state.json");
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(sess) = serde_json::from_str::<Session>(&content) {
                    sessions.push(sess);
                }
            }
        }
    }
    // Sort by created_at descending (newest first).
    sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    sessions
}

pub fn save_sessions(sessions: &[Session]) {
    for sess in sessions {
        let dir = PathBuf::from(SESSIONS_DIR).join(&sess.name);
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("state.json");
        if let Ok(json) = serde_json::to_string_pretty(sess) {
            std::fs::write(path, json).ok();
        }
    }
}

pub fn generate_name(prompt: &str) -> String {
    // Take first 3 words, lowercase, replace non-alphanumeric with hyphen.
    let words: Vec<&str> = prompt.split_whitespace().take(3).collect();
    let base: String = words
        .join("-")
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Ensure uniqueness with a short suffix.
    let suffix = unix_now() % 10000;
    format!("{base}-{suffix}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs()
}
