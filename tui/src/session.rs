// Session management — each session is a coding agent (pi, Claude Code, or
// Codex) running inside a tmux session. tmux gives us attach/detach for free.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Initializing,
    Working,
    NeedsInput,
    Idle,
    Completed,
    Failed,
}

/// Which agent CLI runs in the session's tmux pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Pi,
    Claude,
    Codex,
}

impl Backend {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pi" => Some(Self::Pi),
            "claude" | "cc" | "claude-code" => Some(Self::Claude),
            "codex" | "cx" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// How a session's conversation starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum Origin {
    /// Dispatched from orchestra with a fresh prompt.
    #[default]
    New,
    /// An existing Claude Code / Codex session, resumed with its own CLI.
    Resumed,
    /// A Claude Code / Codex transcript converted into a pi session.
    Forked { from: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub prompt: String,
    /// The session's working directory. For dispatched sessions in a repo
    /// this is the orchestra-owned worktree.
    pub worktree_path: String,
    pub created_at: u64,
    pub last_activity: u64,
    pub state: SessionState,
    /// Stable id; never changes on rename. Keys the pi conversation dir and
    /// is the Claude Code session id for claude sessions orchestra starts.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub backend: Backend,
    #[serde(default)]
    pub origin: Origin,
    /// Model to start with (`provider/id` for pi, `--model` for the others).
    #[serde(default)]
    pub model: Option<String>,
    /// Main checkout of the repo the worktree belongs to.
    #[serde(default)]
    pub repo_root: Option<String>,
    /// Ref the worktree branched from (for the "unmerged commits" check).
    #[serde(default)]
    pub base_ref: Option<String>,
    /// Branch orchestra created for this session, if it owns the worktree.
    /// Imported sessions run in someone else's directory and leave it alone.
    #[serde(default)]
    pub branch: Option<String>,
    /// Claude Code / Codex session id for resumed sessions.
    #[serde(default)]
    pub external_id: Option<String>,
    /// Each agent this conversation has run in, oldest first (filled in on
    /// the first switch). See switch.rs.
    #[serde(default)]
    pub segments: Vec<Segment>,
    /// Set while the session runs on a SkyPilot box (/teleport).
    #[serde(default)]
    pub remote: Option<crate::teleport::Remote>,
    /// Display name shown in the list (Ctrl+R / /rename). Independent of
    /// `name`, which is the tmux session, worktree and branch identity.
    #[serde(default)]
    pub title: Option<String>,
}

/// One stretch of a conversation in one agent's own transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub backend: Backend,
    #[serde(default)]
    pub model: Option<String>,
    /// The agent's transcript file.
    pub path: String,
    /// Where this segment's own turns start in that file: the history
    /// copied in from earlier segments comes first. Lines for Claude Code
    /// and Codex, messages on the active branch for pi.
    pub seed: usize,
}

impl Session {
    pub fn new(name: String, prompt: String, cwd: String, backend: Backend) -> Self {
        let now = unix_now();
        Self {
            name,
            prompt,
            worktree_path: cwd,
            created_at: now,
            last_activity: now,
            state: SessionState::Initializing,
            id: uuid::Uuid::new_v4().to_string(),
            backend,
            origin: Origin::New,
            model: None,
            repo_root: None,
            base_ref: None,
            branch: None,
            external_id: None,
            segments: Vec::new(),
            remote: None,
            title: None,
        }
    }

    /// A copy for read-only work on another thread (/btw, /branch).
    pub fn clone_for_read(&self) -> Session {
        serde_json::from_value(serde_json::to_value(self).expect("session serializes")).expect("session deserializes")
    }

    /// What the list shows: the title if one was set, else the first line
    /// of the prompt (for adopted sessions, the title Claude Code / Codex
    /// gave them), tidied up.
    pub fn display_title(&self) -> String {
        if let Some(t) = self.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            return t.to_string();
        }
        let from_prompt = humanize(&self.prompt);
        if from_prompt.is_empty() { self.name.clone() } else { from_prompt }
    }

    /// Short tag for the session list.
    pub fn tag(&self) -> String {
        let b = match self.backend {
            Backend::Pi => "pi",
            Backend::Claude => "cc",
            Backend::Codex => "cx",
        };
        match &self.model {
            Some(m) => format!("{b}:{}", m.rsplit('/').next().unwrap_or(m)),
            None => b.to_string(),
        }
    }

    /// Whether deleting this session should remove its worktree + branch.
    /// Sessions from before per-repo worktrees don't record a branch but
    /// always owned `~/orchestra/worktrees/<name>`.
    pub fn owned_worktree(&self) -> Option<(PathBuf, PathBuf, String)> {
        let wt = PathBuf::from(&self.worktree_path);
        if let (Some(repo), Some(branch)) = (&self.repo_root, &self.branch) {
            return Some((PathBuf::from(repo), wt, branch.clone()));
        }
        if self.origin == Origin::New && wt.starts_with(paths::legacy_worktrees_dir()) {
            return Some((paths::legacy_repo(), wt, format!("worktree-{}", self.name)));
        }
        None
    }

    pub fn pi_session_dir(&self) -> PathBuf {
        paths::pi_sessions_dir().join(&self.id)
    }

    fn ready_path(&self) -> PathBuf {
        paths::sessions_dir().join(format!("{}.ready", self.name))
    }

    /// Check if the session is ready to attach (the agent UI has rendered).
    pub fn is_ready(&self) -> bool {
        self.ready_path().exists()
    }

    /// Refresh state by checking if the tmux session is still alive and
    /// whether the readiness marker exists.
    pub fn refresh_state(&mut self) {
        if !tmux_alive(&self.name) {
            // tmux session ended — the process finished
            if self.state != SessionState::Failed {
                self.state = SessionState::Completed;
            }
            return;
        }
        self.state = if self.is_ready() {
            SessionState::Working
        } else {
            SessionState::Initializing
        };
    }
}

/// A prompt's first line as a readable name: whitespace collapsed, first
/// letter capitalized, trailing punctuation dropped, at most 60 chars.
pub fn humanize(prompt: &str) -> String {
    let line = prompt.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = line.trim_end_matches(['.', ':', ';', ',', '!', '?']);
    let mut out: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        out = out.trim_end().to_string();
        out.push('…');
    }
    let mut c = out.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Single-quote a string for bash.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Tells agents where orchestra skills live. Replaces the AGENTS.md that
/// used to be written into every worktree (which showed up as an
/// untracked file and clobbered any AGENTS.md the repo already had).
const ORCHESTRA_NOTE: &str = "\
# Orchestra Session

You are running as an orchestra session. When asked to \"write a skill\",
write it to `~/.orchestra/skills/<name>.md` — the only location orchestra
loads skills from — then commit and push it:

    cd ~/orchestra && git add skills/ && git commit -m \"skill: <name>\" && git push origin main

Do not confuse these with any skills directory the current repo defines.
";

/// Write the orchestra note plus all skills from ~/.orchestra/skills/*.md
/// into one file for --append-system-prompt. Returns its path.
fn write_system_prompt(id: &str) -> Option<PathBuf> {
    let mut content = String::from(ORCHESTRA_NOTE);
    if let Ok(rd) = std::fs::read_dir(paths::skills_dir()) {
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if path.extension().is_none_or(|ext| ext != "md")
                || path.file_name().is_some_and(|n| n == "README.md")
            {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                content.push_str("\n---\n\n");
                content.push_str(&text);
            }
        }
    }
    let dir = paths::prompts_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{id}.md"));
    std::fs::write(&path, content).ok()?;
    Some(path)
}

/// The shell commands tmux runs: `first` once, then `restart` in a loop
/// whenever the agent exits, so a crash or /exit resumes the same
/// conversation instead of starting over.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    pub first: String,
    pub restart: String,
}

pub fn launch_commands(sess: &Session, system_prompt: Option<&Path>) -> Launch {
    if let Some(remote) = &sess.remote {
        // The agent runs on the box; this pane is an ssh view of it.
        let attach = crate::teleport::pane_command(sess, remote, None);
        return Launch { first: attach.clone(), restart: attach };
    }
    let prompt_arg = if sess.prompt.trim().is_empty() || sess.origin != Origin::New {
        String::new()
    } else {
        format!(" {}", sq(&sess.prompt))
    };
    match sess.backend {
        Backend::Pi => {
            let dir = sq(&sess.pi_session_dir().to_string_lossy());
            let sp = system_prompt
                .map(|p| format!(" --append-system-prompt {}", sq(&p.to_string_lossy())))
                .unwrap_or_default();
            // An explicit model wins; otherwise ORCHESTRA_PROVIDER /
            // ORCHESTRA_MODEL from ~/.orchestra/env pin the initial model,
            // and with neither pi uses its own default. Restarts pass no
            // model so a /model switch inside pi survives them.
            let model = match &sess.model {
                Some(m) => format!(" --model {}", sq(m)),
                None => " ${ORCHESTRA_PROVIDER:+--provider \"$ORCHESTRA_PROVIDER\"} \
                         ${ORCHESTRA_MODEL:+--model \"$ORCHESTRA_MODEL\"}"
                    .to_string(),
            };
            let cont = if sess.origin == Origin::New { "" } else { " --continue" };
            Launch {
                first: format!("pi --session-dir {dir}{cont}{model}{sp}{prompt_arg}"),
                restart: format!("pi --session-dir {dir} --continue{sp}"),
            }
        }
        Backend::Claude => {
            let model = sess.model.as_ref().map(|m| format!(" --model {}", sq(m))).unwrap_or_default();
            let sp = system_prompt
                .map(|p| format!(" --append-system-prompt \"$(cat {})\"", sq(&p.to_string_lossy())))
                .unwrap_or_default();
            // Reopening goes through `orchestra claude-open`, which attaches
            // instead when the session is running as a Claude Code
            // background session (`claude --resume` refuses those).
            let open = |id: &str| format!("{} claude-open {}{model}", sq(&orchestra_bin()), sq(id));
            match &sess.external_id {
                Some(ext) => Launch { first: open(ext), restart: open(ext) },
                None => Launch {
                    first: format!("claude --session-id {}{model}{sp}{prompt_arg}", sq(&sess.id)),
                    restart: open(&sess.id),
                },
            }
        }
        Backend::Codex => {
            let model = sess.model.as_ref().map(|m| format!(" -m {}", sq(m))).unwrap_or_default();
            match &sess.external_id {
                Some(ext) => {
                    let resume = format!("codex resume {}{model}", sq(ext));
                    Launch { first: resume.clone(), restart: resume }
                }
                // Codex picks its own session id; `orchestra codex-open`
                // finds and records it, then resumes that thread by id.
                None => Launch {
                    first: format!("codex{model}{prompt_arg}"),
                    restart: format!("{} codex-open {}{model}", sq(&orchestra_bin()), sq(&sess.name)),
                },
            }
        }
    }
}

/// Scrolling puts the pane in tmux copy mode, where letters are copy-mode
/// commands (`f` waits for a character to "jump forward" to, and looks
/// frozen). Make every printable key, Space, Enter and Backspace leave copy
/// mode and go to the agent instead, so typing after scrolling just types.
/// Arrow and page keys still move in copy mode. Done once per tmux server,
/// in a single tmux call.
pub fn bind_typing_in_copy_mode() {
    const MARK: &str = "@orchestra_typing_keys";
    const VERSION: &str = "1";
    let current = Command::new("tmux").args(["show-options", "-gqv", MARK]).output();
    if current.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == VERSION) {
        return;
    }
    let _ = Command::new("tmux").args(typing_bind_args(VERSION)).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Arguments for one tmux call that binds the typing keys in both copy-mode
/// tables and records the marker. Commands are separated by ";" arguments;
/// a literal semicolon is written "\\;".
fn typing_bind_args(version: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut push = |cmd: Vec<String>| {
        if !args.is_empty() {
            args.push(";".into());
        }
        args.extend(cmd);
    };
    let s = |x: &str| x.to_string();
    for table in ["copy-mode", "copy-mode-vi"] {
        for c in (0x21u8..=0x7e).map(char::from) {
            let lit = if c == ';' { s("\\;") } else { c.to_string() };
            push(vec![s("bind-key"), s("-T"), s(table), lit.clone(), s("send-keys"), s("-X"), s("cancel"), s("\\;"), s("send-keys"), s("-l"), lit]);
        }
        for k in ["Space", "Enter", "BSpace"] {
            push(vec![s("bind-key"), s("-T"), s(table), s(k), s("send-keys"), s("-X"), s("cancel"), s("\\;"), s("send-keys"), s(k)]);
        }
    }
    push(vec![s("set-option"), s("-g"), s("@orchestra_typing_keys"), s(version)]);
    args
}

/// A stable copy of this binary for commands tmux runs later (the Left
/// key, `claude-open`, /loop): ~/.orchestra/bin/orchestra, replaced by an
/// atomic rename, so rebuilding or upgrading orchestra never leaves tmux
/// pointing at a missing or half-written file.
pub fn orchestra_bin() -> String {
    let exe = std::env::current_exe().ok();
    let dst = paths::state_dir().join("bin").join("orchestra");
    if let Some(exe) = &exe {
        let same = std::fs::metadata(exe)
            .ok()
            .zip(std::fs::metadata(&dst).ok())
            .is_some_and(|(a, b)| a.len() == b.len() && a.modified().ok() <= b.modified().ok());
        if !same {
            let tmp = dst.with_extension(format!("tmp{}", std::process::id()));
            let ok = std::fs::create_dir_all(dst.parent().unwrap_or(&dst))
                .and_then(|_| std::fs::copy(exe, &tmp))
                .and_then(|_| std::fs::rename(&tmp, &dst));
            if ok.is_err() {
                let _ = std::fs::remove_file(&tmp);
                return exe.to_string_lossy().to_string();
            }
        }
        return dst.to_string_lossy().to_string();
    }
    "orchestra".to_string()
}

/// A Claude Code session that is currently running, per
/// `claude agents --json` (which lists interactive and background ones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunningClaude {
    /// A background session; `claude attach <short_id>` opens it.
    Background { short_id: String },
    /// Open in a terminal somewhere else (listed with its pid, no short id).
    Interactive { pid: String },
}

pub fn running_claude(session_id: &str) -> Option<RunningClaude> {
    let out = Command::new("claude")
        .args(["agents", "--json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    find_running(&String::from_utf8_lossy(&out.stdout), session_id)
}

fn find_running(json: &str, session_id: &str) -> Option<RunningClaude> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.as_array()?.iter().find_map(|a| {
        if a.get("sessionId")?.as_str()? != session_id {
            return None;
        }
        let field = |k: &str| a.get(k).map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
        match a.get("kind").and_then(|k| k.as_str()) {
            Some("background") => Some(RunningClaude::Background { short_id: field("id")? }),
            _ => Some(RunningClaude::Interactive { pid: field("pid").unwrap_or_else(|| "?".to_string()) }),
        }
    })
}

/// Shell command tmux runs on Left (see `orchestra tmux-left`). If the
/// orchestra binary is gone the command fails and Left goes to the agent.
fn left_key_check() -> String {
    format!(
        "{} tmux-left '#{{session_name}}' '#{{pane_id}}' '#{{cursor_x}}' '#{{cursor_y}}'",
        sq(&orchestra_bin())
    )
}

/// Whether Left should detach: nothing but blanks or a prompt symbol to the
/// left of the cursor. pi's input starts at column 0; Claude Code's and
/// Codex's start after a `❯` / `›` prompt, where Left on an empty input
/// would otherwise go to the agent (Claude Code opens its agents view).
pub fn at_input_start(line: &str, cursor_x: usize) -> bool {
    line.chars()
        .take(cursor_x)
        .all(|c| c.is_whitespace() || matches!(c, '❯' | '>' | '›' | '│' | '┃' | '|'))
}

/// `orchestra tmux-left <session> <pane> <x> <y>`: exit status for the Left
/// binding. Only orchestra's own sessions detach; the binding is global, so
/// any other tmux session gets its Left key unchanged.
pub fn tmux_left_should_detach(session: &str, pane: &str, x: usize, y: usize) -> bool {
    if !paths::sessions_dir().join(session).join("state.json").exists() {
        return false;
    }
    let y = y.to_string();
    let line = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", pane, "-S", &y, "-E", &y])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    at_input_start(&line, x)
}

/// Spawn the session's agent inside a detached tmux session.
///
/// A background subshell polls the pane until the agent UI has rendered,
/// then touches the ready marker so the TUI shows the session as
/// attachable. pi's footer shows context usage ("%/<ctx>" or "?/<ctx>");
/// Claude Code and Codex are considered up once the pane has content.
///
/// Left arrow (at column 0) and Ctrl+C are bound to detach-client so the
/// user can return to the orchestra TUI without killing the agent. Use
/// Escape to interrupt agent operations.
pub fn spawn(sess: &Session) -> std::io::Result<()> {
    spawn_with(sess, None)
}

/// Like `spawn`, running `pre` in the pane first (a `sky launch`, or the
/// copy back from a box); the agent starts only if it succeeds.
pub fn spawn_with(sess: &Session, pre: Option<&str>) -> std::io::Result<()> {
    let system_prompt = match sess.backend {
        Backend::Pi | Backend::Claude => write_system_prompt(&sess.id),
        Backend::Codex => None,
    };
    let mut launch = launch_commands(sess, system_prompt.as_deref());
    if let Some(pre) = pre {
        launch.first = format!("{{ {pre}; }} && {}", launch.first);
    }
    let name = &sess.name;
    let ready_marker = sq(&sess.ready_path().to_string_lossy());
    let env_file = sq(&paths::env_file().to_string_lossy());
    let ready_check = match sess.backend {
        Backend::Pi => format!("tmux capture-pane -t {name} -p 2>/dev/null | grep -qE '[%?]/'"),
        _ => format!("[ \"$(tmux capture-pane -t {name} -p 2>/dev/null | grep -c .)\" -ge 3 ]"),
    };
    let cmd_str = format!(
        "[ -f {env_file} ] && {{ set -a; . {env_file}; set +a; }}
        export COLORTERM=truecolor PI_SKIP_VERSION_CHECK=1
        (
            for i in $(seq 1 30); do
                if {ready_check}; then break; fi
                sleep 0.5
            done
            touch {ready_marker}
        ) &
        started=$(date +%s)
        {first}
        while true; do
            # An agent that exits right after starting would otherwise be
            # relaunched every second, flooding the pane with its error.
            if [ $(( $(date +%s) - started )) -lt 5 ]; then
                printf '\n[orchestra] The agent exited right after starting. Press Enter to start it again.\n'
                read -r _
            else
                sleep 1
            fi
            started=$(date +%s)
            {restart}
        done",
        first = launch.first,
        restart = launch.restart,
    );

    // Write ~/.tmux.conf if it doesn't exist. The tmux server reads this
    // file on startup (before creating any sessions), so settings like
    // default-terminal are applied before the session's PTY is created.
    // The main-box has TERM=dumb, so without this, tmux defaults to
    // "screen" (8 colors) causing weird highlighting in agent output.
    let left_check = left_key_check();
    let tmux_conf = paths::home().join(".tmux.conf");
    if !tmux_conf.exists() {
        std::fs::write(&tmux_conf, format!("\
set -g default-terminal \"screen-256color\"
set -ga terminal-overrides \",*256col*:Tc\"
set -g extended-keys on
set -g remain-on-exit on
# In orchestra sessions, Left detaches back to orchestra when the cursor is
# at the start of the agent's input; otherwise it goes to the agent.
bind-key -n Left if-shell \"{left_check}\" detach-client 'send-keys Left'
bind-key -n C-c detach-client
"))?;
    }

    // Long scrollback, so the whole conversation stays scrollable. It
    // applies to panes created after it is set, hence before new-session.
    let _ = Command::new("tmux")
        .args(["set-option", "-g", "history-limit", "50000"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-c", &sess.worktree_path])
        .arg("bash")
        .arg("-c")
        .arg(&cmd_str)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("tmux new-session failed"));
    }

    // Like the agent alone: no tmux status bar (as in Claude Code's own
    // agents view), mouse wheel scrolls the conversation (without mouse
    // mode tmux turns the wheel into Up/Down, which agents read as prompt
    // history), and tagged so orchestra's click rules apply.
    session_look(name);

    apply_tmux_setup(&left_check);
    Ok(())
}

/// Server-wide tmux settings orchestra relies on: Left returns to the
/// list, Ctrl+C detaches, mouse copy goes to the clipboard, typing after
/// scrolling types, and Left leaves copy mode. Run at every spawn and by
/// `orchestra tmux-setup` (to update a running tmux server in place).
pub fn apply_tmux_setup(left_check: &str) {
    // Set keybindings after session creation as a fallback, in case
    // the tmux server was already running without ~/.tmux.conf.
    let _ = Command::new("tmux")
        .args(["bind-key", "-n", "Left", "if-shell", left_check, "detach-client", "send-keys Left"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("tmux")
        .args(["bind-key", "-n", "C-c", "detach-client"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // Mouse selection copies to your clipboard (OSC 52; allowed by most
    // terminals, iTerm2 needs "Applications in terminal may access
    // clipboard"; browsers usually refuse it). On release: copy, clear the
    // highlight (a kept one lingers while scrolling) and stay at the
    // scrolled position. No "Copied" message: tmux cannot know whether the
    // terminal accepted it.
    let _ = Command::new("tmux").args(["set-option", "-s", "set-clipboard", "on"]).status();
    for table in ["copy-mode", "copy-mode-vi"] {
        let _ = Command::new("tmux")
            .args(["bind-key", "-T", table, "MouseDragEnd1Pane", "send-keys", "-X", "copy-pipe"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    bind_typing_in_copy_mode();
    // Quiet copy mode: tmux draws the selection and its position counter
    // (top right) in mode-style, bright yellow by default. Use the theme's
    // selection color instead (tmux 3.4 cannot hide the counter), and a
    // muted style for tmux messages.
    let light = crate::config::Config::load().light();
    let (mode, msg) = if light {
        ("bg=#b4d5ff,fg=default", "bg=default,fg=#666666")
    } else {
        ("bg=#264f78,fg=default", "bg=default,fg=#999999")
    };
    for (opt, val) in [("mode-style", mode), ("message-style", msg)] {
        let _ = Command::new("tmux").args(["set-option", "-g", opt, val]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    // Mouse clicks in orchestra sessions (tagged @orchestra): a click puts
    // you back at the agent's prompt — it leaves copy mode and is not
    // passed to the agent, so agents that track the mouse (Claude Code's
    // attach view, Codex) don't move their cursor onto output text. A drag
    // always makes a tmux selection, for copying. The wheel is unchanged.
    for (table, key, cmd) in [
        ("root", "MouseDown1Pane", ["if-shell", "-F", "#{@orchestra}", "select-pane -t =", "select-pane -t = ; send-keys -M"]),
        ("root", "MouseUp1Pane", ["if-shell", "-F", "#{@orchestra}", "select-pane -t =", "send-keys -M"]),
        ("root", "MouseDrag1Pane", ["if-shell", "-F", "#{@orchestra}", "copy-mode -M",
            "if-shell -F \"#{||:#{pane_in_mode},#{mouse_any_flag}}\" \"send-keys -M\" \"copy-mode -M\""]),
        ("copy-mode", "MouseUp1Pane", ["if-shell", "-F", "#{@orchestra}", "send-keys -X cancel", "send-keys -X clear-selection"]),
        ("copy-mode-vi", "MouseUp1Pane", ["if-shell", "-F", "#{@orchestra}", "send-keys -X cancel", "send-keys -X clear-selection"]),
    ] {
        let _ = Command::new("tmux")
            .args(["bind-key", "-T", table, key])
            .args(cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    // After scrolling (tmux copy mode), Left should still mean "back to
    // orchestra", not "move the copy-mode cursor".
    let is_orch = format!("{} is-orchestra '#{{session_name}}'", sq(&orchestra_bin()));
    for table in ["copy-mode", "copy-mode-vi"] {
        let _ = Command::new("tmux")
            .args(["bind-key", "-T", table, "Left", "if-shell", &is_orch,
                   "send-keys -X cancel ; detach-client", "send-keys -X cursor-left"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// `orchestra tmux-setup`: apply the settings above to the running tmux
/// server now (typing keys are re-bound even if already marked), and tag
/// and style every running orchestra session.
pub fn tmux_setup_now() {
    let _ = Command::new("tmux").args(["set-option", "-gu", "@orchestra_typing_keys"]).status();
    apply_tmux_setup(&left_key_check());
    let out = Command::new("tmux").args(["list-sessions", "-F", "#{session_name}"]).output();
    for name in out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default().lines() {
        if paths::sessions_dir().join(name).join("state.json").exists() {
            session_look(name);
        }
    }
}

/// Per-session tmux options for an orchestra session: tagged @orchestra
/// (mouse rules above apply only to these), no status bar, mouse on.
pub fn session_look(name: &str) {
    for (opt, val) in [("@orchestra", "1"), ("status", "off"), ("mouse", "on")] {
        let _ = Command::new("tmux")
            .args(["set-option", "-t", name, opt, val])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub fn load_sessions() -> Vec<Session> {
    let mut sessions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths::sessions_dir()) {
        for entry in entries.flatten() {
            let path = entry.path().join("state.json");
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(mut sess) = serde_json::from_str::<Session>(&content) {
                    // Hide sessions with no tmux session, so the list never
                    // shows one that can't be attached to. Their state is
                    // kept rather than deleted: "no tmux session" can also
                    // mean this TUI is talking to a different tmux server
                    // (e.g. run inside another tmux via -L), and deleting
                    // would lose the worktree/branch/conversation record of
                    // a live session.
                    if !tmux_alive(&sess.name) {
                        continue;
                    }
                    if sess.id.is_empty() {
                        sess.id = uuid::Uuid::new_v4().to_string();
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

/// On quit: forget sessions this TUI saw end. Live sessions are not
/// rewritten — every change is saved when it happens, and rewriting here
/// would let a second orchestra window overwrite newer state (a /switch
/// made in the other window) with its stale copy.
pub fn save_sessions(sessions: &[Session]) {
    for sess in sessions {
        if (sess.state == SessionState::Completed || sess.state == SessionState::Failed) && !tmux_alive(&sess.name) {
            remove_session_state(&sess.name);
        }
    }
}

/// Check if a tmux session is alive.
pub fn tmux_alive(name: &str) -> bool {
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
pub fn remove_session_state(name: &str) {
    let dir = paths::sessions_dir();
    let _ = std::fs::remove_dir_all(dir.join(name));
    let _ = std::fs::remove_file(dir.join(format!("{name}.ready")));
}

/// Save a single session's state to disk. Called immediately after
/// dispatch so that the session survives TUI crashes/upgrades — the
/// TUI process is the control plane and can be killed at any time,
/// but the session state must persist so a restarted TUI can find it.
pub fn save_session(sess: &Session) {
    let dir = paths::sessions_dir().join(&sess.name);
    std::fs::create_dir_all(&dir).ok();
    if let Ok(json) = serde_json::to_string_pretty(sess) {
        std::fs::write(dir.join("state.json"), json).ok();
    }
}

pub fn generate_name(prompt: &str) -> String {
    // Take first 3 words, lowercase, replace non-alphanumeric with hyphen.
    let words: Vec<&str> = prompt.split_whitespace().take(3).collect();
    let base: String = words
        .join("-")
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(backend: Backend, prompt: &str) -> Session {
        let mut s = Session::new("n".into(), prompt.into(), "/w".into(), backend);
        s.id = "ID".into();
        s
    }

    #[test]
    fn legacy_state_json_still_loads() {
        let old = r#"{"name":"a","prompt":"p","worktree_path":"/home/sky/orchestra/worktrees/a",
            "created_at":1,"last_activity":1,"state":"Working"}"#;
        let s: Session = serde_json::from_str(old).unwrap();
        assert_eq!(s.backend, Backend::Pi);
        assert_eq!(s.origin, Origin::New);
        assert!(s.branch.is_none());
    }

    #[test]
    fn pi_uses_session_dir_not_name() {
        let l = launch_commands(&sess(Backend::Pi, "fix it's bug"), Some(Path::new("/sp.md")));
        assert!(l.first.starts_with("pi --session-dir '"), "{}", l.first);
        assert!(l.first.contains("/pi-sessions/ID'"));
        assert!(l.first.ends_with(" 'fix it'\\''s bug'"), "{}", l.first);
        assert!(l.first.contains("--append-system-prompt '/sp.md'"));
        assert!(l.first.contains("ORCHESTRA_MODEL"));
        assert!(!l.first.contains("--continue"));
        assert!(l.restart.contains("--continue") && !l.restart.contains("fix"));
        assert!(!l.first.contains("--name"));
    }

    #[test]
    fn pi_explicit_model_and_fork() {
        let mut s = sess(Backend::Pi, "hello");
        s.model = Some("openrouter/qwen/qwen3".into());
        s.origin = Origin::Forked { from: "claude:abc".into() };
        let l = launch_commands(&s, None);
        assert!(l.first.contains("--continue --model 'openrouter/qwen/qwen3'"), "{}", l.first);
        assert!(!l.first.contains("hello"), "forked sessions don't re-send the prompt");
        assert!(!l.restart.contains("--model"));
    }

    #[test]
    fn claude_new_and_resumed() {
        let l = launch_commands(&sess(Backend::Claude, "do x"), Some(Path::new("/sp.md")));
        assert_eq!(
            l.first,
            "claude --session-id 'ID' --append-system-prompt \"$(cat '/sp.md')\" 'do x'"
        );
        assert!(l.restart.ends_with(" claude-open 'ID'"), "{}", l.restart);

        let mut r = sess(Backend::Claude, "");
        r.origin = Origin::Resumed;
        r.external_id = Some("abc".into());
        r.model = Some("opus".into());
        let l = launch_commands(&r, None);
        assert!(l.first.ends_with(" claude-open 'abc' --model 'opus'"), "{}", l.first);
        assert_eq!(l.first, l.restart);
    }

    #[test]
    fn left_detaches_only_at_input_start() {
        // pi: input at column 0.
        assert!(at_input_start("hello", 0));
        // Claude Code empty prompt: cursor after "❯\u{a0}", placeholder after it.
        assert!(at_input_start("❯\u{a0}Try \"fix typecheck errors\"", 2));
        assert!(at_input_start("› ", 2));
        // Text before the cursor: Left moves the cursor instead.
        assert!(!at_input_start("❯ abc", 5));
        assert!(!at_input_start("❯ abc", 3));
        assert!(!at_input_start("hello", 2));
    }

    #[test]
    fn typing_keys_cover_printables_in_both_tables() {
        let a = typing_bind_args("1");
        let binds = a.iter().filter(|x| *x == "bind-key").count();
        assert_eq!(binds, 2 * (94 + 3));
        assert!(a.windows(4).any(|w| w == ["-T", "copy-mode", "f", "send-keys"]));
        assert!(a.windows(4).any(|w| w == ["-T", "copy-mode-vi", "\\;", "send-keys"]));
        assert_eq!(a.last().map(String::as_str), Some("1"));
    }

    #[test]
    fn finds_running_claude_session() {
        let json = r#"[
            {"id":"a4a20d11","sessionId":"a4a20d11-e247","kind":"background","state":"done"},
            {"pid":"859259","sessionId":"77aa0000-1111","kind":"interactive","status":"busy"}
        ]"#;
        assert_eq!(
            find_running(json, "a4a20d11-e247"),
            Some(RunningClaude::Background { short_id: "a4a20d11".into() })
        );
        assert_eq!(
            find_running(json, "77aa0000-1111"),
            Some(RunningClaude::Interactive { pid: "859259".into() })
        );
        assert_eq!(find_running(json, "nope"), None);
        assert_eq!(find_running("not json", "x"), None);
    }

    #[test]
    fn codex_new_and_resumed() {
        let mut s = sess(Backend::Codex, "do y");
        s.model = Some("gpt-6".into());
        let l = launch_commands(&s, None);
        assert_eq!(l.first, "codex -m 'gpt-6' 'do y'");
        assert!(l.restart.ends_with(" codex-open 'n' -m 'gpt-6'"), "{}", l.restart);

        let mut r = sess(Backend::Codex, "");
        r.origin = Origin::Resumed;
        r.external_id = Some("019a".into());
        assert_eq!(launch_commands(&r, None).first, "codex resume '019a'");
    }

    #[test]
    fn owned_worktree_rules() {
        let mut s = sess(Backend::Pi, "p");
        assert!(s.owned_worktree().is_none(), "no repo, no branch");
        s.repo_root = Some("/r".into());
        s.branch = Some("worktree-n".into());
        assert_eq!(s.owned_worktree().unwrap().2, "worktree-n");

        let mut legacy = sess(Backend::Pi, "p");
        legacy.worktree_path = paths::legacy_worktrees_dir().join("n").to_string_lossy().into();
        assert!(legacy.owned_worktree().is_some());
        legacy.origin = Origin::Resumed;
        assert!(legacy.owned_worktree().is_none());
    }

    #[test]
    fn display_titles() {
        let mut s = sess(Backend::Pi, "fix the add function in calc.py.\nmore detail");
        assert_eq!(s.display_title(), "Fix the add function in calc.py");
        s.title = Some("Calc fix".into());
        assert_eq!(s.display_title(), "Calc fix");
        s.title = Some("  ".into());
        assert_eq!(s.display_title(), "Fix the add function in calc.py");
        let long = humanize(&"word ".repeat(30));
        assert!(long.ends_with('…') && long.chars().count() <= 61);
        let empty = sess(Backend::Pi, "");
        assert_eq!(empty.display_title(), "n");
    }

    #[test]
    fn tag_shows_backend_and_model() {
        let mut s = sess(Backend::Claude, "p");
        assert_eq!(s.tag(), "cc");
        s.model = Some("anthropic/claude-opus-5-5".into());
        assert_eq!(s.tag(), "cc:claude-opus-5-5");
    }
}
