// Session management — each session is a pi process running in a worktree.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command};
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
    /// Serialized child process — we track if it's alive via try_wait.
    #[serde(skip)]
    pub child: Option<Child>,
}

impl Session {
    pub fn new(name: String, prompt: String, worktree_path: String, child: Child) -> Self {
        let now = unix_now();
        Self {
            name,
            prompt,
            worktree_path,
            created_at: now,
            last_activity: now,
            state: SessionState::Working,
            child: Some(child),
        }
    }

    /// Refresh state by checking if the pi process is still alive.
    pub fn refresh_state(&mut self) {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.state = if status.success() {
                        SessionState::Completed
                    } else {
                        SessionState::Failed
                    };
                }
                Ok(None) => {
                    // Still running — check if the process is waiting for input
                    // by looking at recent activity in the worktree. For now,
                    // mark as Working.
                    self.state = SessionState::Working;
                }
                Err(_) => {
                    self.state = SessionState::Failed;
                }
            }
        } else {
            // No child process — session was resumed from disk, not currently running.
            self.state = SessionState::Idle;
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

pub fn spawn_pi(
    name: &str,
    worktree_path: &str,
    initial_prompt: &str,
) -> std::io::Result<Child> {
    // Spawn pi in print mode with the initial prompt, in the worktree.
    // The session is saved by name, so subsequent `pi -p --name <name>` calls
    // continue the conversation.
    //
    // ALL stdio redirected to null so pi's output never leaks into the TUI.
    // The session runs silently in the background; attach with Right arrow
    // to see it interactively.
    //
    // Env vars are loaded from ~/.orchestra/env (written during setup)
    // because SSH sessions don't have the secret env vars.
    let env = load_env();
    let mut cmd = Command::new("pi");
    cmd.arg("-p")
        .arg(initial_prompt)
        .arg("--name")
        .arg(name)
        .arg("--provider")
        .arg("glm")
        .arg("--model")
        .arg("zai-org/GLM-5.2-FP8")
        .current_dir(worktree_path)
        .envs(&env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
}

pub fn load_sessions() -> Vec<Session> {
    let mut sessions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(SESSIONS_DIR) {
        for entry in entries.flatten() {
            let path = entry.path().join("state.json");
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(mut sess) = serde_json::from_str::<Session>(&content) {
                    // No live child process after restore — mark as idle.
                    sess.child = None;
                    sess.state = SessionState::Idle;
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
        // Clone without the child handle (child is skipped in serde).
        let to_save = Session {
            child: None,
            name: sess.name.clone(),
            prompt: sess.prompt.clone(),
            worktree_path: sess.worktree_path.clone(),
            created_at: sess.created_at,
            last_activity: sess.last_activity,
            state: sess.state,
        };
        if let Ok(json) = serde_json::to_string_pretty(&to_save) {
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
