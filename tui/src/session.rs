// Session management — each session is a pi process running inside a tmux
// session in a worktree. tmux gives us attach/detach for free.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const SESSIONS_DIR: &str = "/home/sky/.orchestra/sessions";
const ENV_FILE: &str = "/home/sky/.orchestra/env";
const SKILLS_DIR: &str = "/home/sky/.orchestra/skills";

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

/// Concatenate all skill files from ~/.orchestra/skills/*.md into a temp
/// file and return its path. Returns None if no skills exist.
fn load_skills_prompt() -> Option<String> {
    let mut content = String::new();
    let mut entries: Vec<_> = std::fs::read_dir(SKILLS_DIR).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let name = path.file_name()?.to_string_lossy();
        if name == "README.md" {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            content.push_str(&text);
            content.push_str("\n\n---\n\n");
        }
    }
    if content.is_empty() {
        return None;
    }
    // Write to a temp file — pi reads it via --append-system-prompt.
    let path = format!("/tmp/orchestra-skills-{unix}.md", unix = unix_now());
    std::fs::write(&path, &content).ok()?;
    Some(path)
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

    // Concatenate all skill files into a temp file and pass via
    // --append-system-prompt so sessions load skills automatically.
    let skills_prompt = load_skills_prompt();
    let skills_flag = match &skills_prompt {
        Some(path) => format!("--append-system-prompt {path}"),
        None => String::new(),
    };

    let cmd_str = format!(
        "set -a; source {ENV_FILE}; set +a; pi -p '{escaped}' --name {name} --provider glm --model zai-org/GLM-5.2-FP8 {skills_flag}; while true; do pi --name {name} --provider glm --model zai-org/GLM-5.2-FP8 {skills_flag}; sleep 1; done"
    );

    // Set global tmux options BEFORE creating the session so the session
    // inherits them. default-terminal must be screen-256color for pi's
    // color escape sequences to render correctly (the default "screen"
    // only supports 8 colors, causing weird highlighting on normal text).
    Command::new("tmux")
        .args(["set", "-g", "default-terminal", "screen-256color"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    Command::new("tmux")
        .args(["set", "-ga", "terminal-overrides", ",*256col*:Tc"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    Command::new("tmux")
        .args(["set", "-g", "extended-keys", "on"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    Command::new("tmux")
        .args(["set", "-g", "remain-on-exit", "on"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    // Left arrow and Ctrl+C detach back to the orchestra TUI (root table
    // = no prefix needed). Ctrl+C would otherwise exit pi and kill the
    // session.
    for key in ["Left", "C-c"] {
        Command::new("tmux")
            .args(["bind-key", "-n", key, "detach-client"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
    }

    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-c", worktree_path])
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
