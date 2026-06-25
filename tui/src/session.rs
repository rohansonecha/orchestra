// Session management — each session is a pi process running inside a tmux
// session in a worktree. tmux gives us attach/detach for free.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const SESSIONS_DIR: &str = "/home/sky/.orchestra/sessions";
const ENV_FILE: &str = "/home/sky/.orchestra/env";
const SKILLS_DIR: &str = "/home/sky/.orchestra/skills";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Initializing,
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
            state: SessionState::Initializing,
        }
    }

    /// Path to the readiness marker file. The tmux session touches this
    /// file when print mode finishes and interactive pi starts.
    fn ready_path(&self) -> String {
        format!("{SESSIONS_DIR}/{}.ready", self.name)
    }

    /// Check if the session is ready to attach (interactive pi is running).
    pub fn is_ready(&self) -> bool {
        std::path::Path::new(&self.ready_path()).exists()
    }

    /// Refresh state by checking if the tmux session is still alive and
    /// whether the readiness marker exists.
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

        if !alive {
            // tmux session ended — the process finished
            if self.state != SessionState::Failed {
                self.state = SessionState::Completed;
            }
            return;
        }

        // tmux is alive — check if interactive pi has started yet.
        if self.is_ready() {
            self.state = SessionState::Working;
        } else {
            self.state = SessionState::Initializing;
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

/// Spawn pi inside a detached tmux session. Interactive pi starts
/// immediately and the initial prompt is sent as keystrokes after a
/// short delay (to let pi's UI render). This way the session becomes
/// attachable in ~3 seconds and the user can watch pi think in real
/// time, rather than waiting for print mode to finish.
///
/// If pi exits (crash, Ctrl+C, etc.), a restart loop relaunches it.
/// pi persists conversation history by --name, so restarts resume
/// the existing conversation.
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

    let ready_marker = format!("{SESSIONS_DIR}/{name}.ready");
    // Start interactive pi in the foreground. A background subshell
    // polls the tmux pane until pi's UI is rendered (detected by the
    // model quality string "medium" in the status bar), then touches
    // the ready marker and sends the prompt as keystrokes.
    let cmd_str = format!(
        "set -a; source {ENV_FILE}; set +a; (
            for i in $(seq 1 30); do
                if tmux capture-pane -t {name} -p 2>/dev/null | grep -q 'medium'; then
                    break
                fi
                sleep 0.5
            done
            touch {ready_marker}
            tmux send-keys -t {name} -l '{escaped}'
            tmux send-keys -t {name} Enter
        ) & pi --name {name} --provider glm --model zai-org/GLM-5.2-FP8 {skills_flag}; while true; do pi --name {name} --provider glm --model zai-org/GLM-5.2-FP8 {skills_flag}; sleep 1; done"
    );

    // Write ~/.tmux.conf if it doesn't exist. The tmux server reads this
    // file on startup (before creating any sessions), so settings like
    // default-terminal are applied before the session's PTY is created.
    // This is necessary because `tmux start-server` exits immediately
    // when there are no sessions — all set/bind-key commands fail silently.
    // The main-box has TERM=dumb, so without this, tmux defaults to
    // "screen" (8 colors) causing weird highlighting in pi's output.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/sky".to_string());
    let tmux_conf = format!("{home}/.tmux.conf");
    if !std::path::Path::new(&tmux_conf).exists() {
        std::fs::write(&tmux_conf, "\
set -g default-terminal \"screen-256color\"
set -ga terminal-overrides \",*256col*:Tc\"
set -g extended-keys on
set -g remain-on-exit on
# Left detaches only when cursor is at column 0, otherwise passes through
# to pi so the user can move the cursor left within the text input.
bind-key -n Left if-shell -F '#{==:#{cursor_x},0}' detach-client 'send-keys Left'
bind-key -n C-c detach-client
")?;
    }

    // Create the session — this starts the tmux server if it's not
    // already running. The server reads ~/.tmux.conf on startup, so
    // terminal settings and keybindings are applied before the session.
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

    // Set keybindings after session creation as a fallback, in case
    // the tmux server was already running without ~/.tmux.conf.
    // Left detaches only at column 0, otherwise passes through to pi.
    Command::new("tmux")
        .args(["bind-key", "-n", "Left", "if-shell", "-F", "#{==:#{cursor_x},0}", "detach-client", "send-keys", "Left"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Command::new("tmux")
        .args(["bind-key", "-n", "C-c", "detach-client"])
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
                    // Skip sessions whose tmux is dead — their state files
                    // are stale. Clean up the state dir + .ready marker so
                    // they don't accumulate. This prevents the TUI from
                    // showing sessions that can't be attached to.
                    if !tmux_alive(&sess.name) {
                        remove_session_state(&sess.name);
                        continue;
                    }
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
        // Don't re-save sessions whose tmux is dead — that recreates stale
        // state files and causes the "ghost sessions" problem where the
        // TUI shows sessions that can't be attached to.
        if sess.state == SessionState::Completed || sess.state == SessionState::Failed {
            remove_session_state(&sess.name);
            continue;
        }
        save_session(sess);
    }
}

/// Check if a tmux session is alive.
fn tmux_alive(name: &str) -> bool {
    Command::new("tmux")
        .arg("has-session")
        .arg("-t")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Remove a session's state directory + .ready marker from disk.
fn remove_session_state(name: &str) {
    let dir = PathBuf::from(SESSIONS_DIR).join(name);
    let ready = PathBuf::from(SESSIONS_DIR).join(format!("{name}.ready"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&ready);
}

/// Save a single session's state to disk. Called immediately after
/// dispatch so that the session survives TUI crashes/upgrades — the
/// TUI process is the control plane and can be killed at any time,
/// but the session state must persist so a restarted TUI can find it.
pub fn save_session(sess: &Session) {
    let dir = PathBuf::from(SESSIONS_DIR).join(&sess.name);
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("state.json");
    if let Ok(json) = serde_json::to_string_pretty(sess) {
        std::fs::write(path, json).ok();
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
