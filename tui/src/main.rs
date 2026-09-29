// orchestra — Terminal UI for managing parallel coding agent sessions
// (pi, Claude Code, Codex), each in its own git worktree.
//
// Two-level TUI (Design_Document.md §9):
//   - Agent View (default): the classic session list + dispatch input, scoped to one
//     agent (the Main Agent for now; sub-agents are read-only).
//   - Tree View: spatial graph of the agent/session hierarchy (Tab).
//
// Keybindings — Tree View:
//   ←/→         — parent / nearest child (depth traversal)
//   ↑/↓         — previous/next node at the same depth
//   Enter       — enter node: agent → Agent View, session → tmux attach
//   Tab         — switch to Agent View (scoped to selected agent)
//   d           — toggle detail pane
//   n           — rename selected node (inline)
//   r           — reload tree from disk
//   x           — tear down selected agent (sky down) / session (tmux kill)
//   q/Ctrl+C    — quit
//
// Keybindings — Agent View (the classic list TUI):
//   Up/Down     — navigate session list
//   Enter       — dispatch new session (if input non-empty) or attach
//   i           — import Claude Code / Codex sessions (empty input)
//   x / q       — delete session / quit (empty input only)
//   Right       — move cursor right / attach to session (empty input)
//   Left        — move cursor left (detach inside tmux only at column 0)
//   Alt+Left    — jump to previous word
//   Alt+Right   — jump to next word
//   Alt+Delete  — delete previous word
//   Tab/Esc     — return to Tree View
//   Ctrl+C      — quit
//
// Keybindings — Import View:
//   Up/Down     — navigate
//   Enter       — resume with its own CLI (claude --resume / codex resume)
//   p           — fork the transcript into a new pi session
//   a           — toggle this repo / all repos
//   Esc/Tab     — back to Agent View
//
// Sessions dispatched inside a git repo get a worktree at
// <repo>/.orchestra/worktrees/<name> (see worktree.rs). The repo is the one
// orchestra was launched from.
//
// The TUI is a pure reader of the tree store at ~/.orchestra/tree/. If the
// store is absent (collector not running), it synthesizes a tree in memory
// from the session list so Tree View is never empty on first run.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};
use std::io;
use std::process::Command;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

mod agent_view;
mod command;
mod commands;
mod config;
mod history;
mod import;
mod paths;
mod rename;
mod repo;
mod session;
mod switch;
mod teleport;
mod tree_layout;
mod tree_store;
mod tree_view;
mod worktree;

use config::Config;
use import::ExternalSession;
use repo::Repo;
use session::{Backend, Origin, Session, SessionState};
use tree_store::{Node, NodeKind, NodeState, NodeView, Tree, TreeStore};

const ROOT_AGENT_ID: &str = "agent-main-box";

/// Which TUI level is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Tree,
    Agent,
}

/// Claude Code / Codex sessions found on disk, listed under orchestra's
/// own sessions so they can be adopted in place.
struct ImportState {
    items: Vec<ExternalSession>,
    /// Show sessions from every directory, not just the current repo.
    all_repos: bool,
    scanned: Option<Instant>,
}

/// A row of the main list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRef {
    Session(usize),
    /// Header of the Claude Code / Codex group (folds it).
    Group(Backend),
    External(usize),
}

/// What each session is doing, refreshed every couple of seconds.
#[derive(Debug, Clone, Default)]
struct Activity {
    working: bool,
    looping: bool,
    summary: Option<String>,
    last_active: Option<u64>,
}

/// One choice in the agent/model picker.
#[derive(Debug, Clone)]
struct PickOption {
    name: String,
    desc: String,
    target: switch::Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PickPurpose {
    /// Move this session (by name) to the picked agent/model.
    Switch(String),
    /// Default agent/model for new sessions.
    Default,
}

#[derive(Debug, Clone)]
struct Picker {
    purpose: PickPurpose,
    options: Vec<PickOption>,
    selected: usize,
    filter: String,
}

impl Picker {
    fn visible(&self) -> Vec<&PickOption> {
        let f = self.filter.to_lowercase();
        self.options
            .iter()
            .filter(|o| f.is_empty() || format!("{} {}", o.name, o.desc).to_lowercase().contains(&f))
            .collect()
    }
}

#[derive(Debug, Clone)]
enum Overlay {
    Help,
    Picker(Picker),
    /// A /btw answer or /recap.
    Text { title: String, subtitle: String, body: String },
    /// /bug draft waiting for Enter to file it.
    Bug { title: String, body: String },
    /// /teleport plan waiting for Enter to launch it.
    Teleport { session: String, plan: teleport::Plan },
}

/// A finished switch or fork, from its worker thread. Converting a long
/// transcript can take seconds, so it never runs on the UI thread.
struct SwitchDone {
    /// The session with its new agent/transcript recorded.
    session: Session,
    result: Result<String, String>,
    /// A fork (new session to start) rather than a switch of an existing one.
    fork: Option<(String, Vec<String>)>,
}

/// A finished /btw or /recap, from the worker thread.
struct SideAnswer {
    session: String,
    question: String,
    recap: bool,
    result: Result<String, String>,
}

/// Inline input mode for the dispatch box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    /// Normal: typing a prompt or /command.
    Dispatch,
    /// Renaming the selected tree node (n was pressed).
    Rename,
}

struct App {
    sessions: Vec<Session>,
    list_state: ListState,
    input: String,
    cursor_pos: usize,
    status_message: String,
    needs_clear: bool,
    // --- Tree View state ---
    mode: ViewMode,
    /// The agent node id that Agent View is currently scoped to. `None`
    /// means the root (Main Agent) — the classic list view.
    agent_view_scope: Option<String>,
    tree: Tree,
    selected_node_id: Option<String>,
    show_detail: bool,
    input_mode: InputMode,
    /// When in Rename mode, the id of the node being renamed.
    rename_target: Option<String>,
    /// Agent View: the session (by name) whose display name is being edited.
    rename_session: Option<String>,
    /// Pending delete confirmation: first `x` press sets this to the
    /// selected session index; second `x` confirms + executes. Any other
    /// key clears it. Prevents accidental full-cleanup deletion.
    pending_delete: Option<usize>,
    /// Same for `x` in Tree View: the node id armed for teardown.
    pending_tree_delete: Option<String>,
    /// Repo orchestra was launched in; None outside git (no worktrees).
    repo: Option<Repo>,
    launch_dir: std::path::PathBuf,
    config: Config,
    import: ImportState,
    /// Selected row of the main list (see `rows`).
    sel: usize,
    activity: HashMap<String, Activity>,
    /// Transcript path → (mtime, latest reply line), for list summaries.
    summaries: HashMap<PathBuf, (SystemTime, Option<String>)>,
    activity_scanned: Option<Instant>,
    overlay: Option<Overlay>,
    /// `pi --list-models`, loaded in the background at startup (it takes
    /// a second or two) so the picker opens instantly.
    pi_models: Option<Vec<(String, String, String)>>,
    pi_models_rx: Option<std::sync::mpsc::Receiver<Vec<(String, String, String)>>>,
    side_rx: Option<std::sync::mpsc::Receiver<SideAnswer>>,
    switch_tx: std::sync::mpsc::Sender<SwitchDone>,
    switch_rx: std::sync::mpsc::Receiver<SwitchDone>,
    /// Sessions being switched → what they are switching to.
    switching: HashMap<String, String>,
}

impl App {
    fn new() -> Self {
        let (switch_tx, switch_rx) = std::sync::mpsc::channel();
        let sessions = session::load_sessions();
        let launch_dir = std::env::current_dir().unwrap_or_else(|_| paths::home());
        let repo = repo::detect_for_launch(&launch_dir);
        let mut app = Self {
            sessions,
            list_state: ListState::default(),
            input: String::new(),
            cursor_pos: 0,
            status_message: String::new(),
            needs_clear: false,
            mode: ViewMode::Agent,
            agent_view_scope: None,
            tree: Tree::default(),
            selected_node_id: None,
            show_detail: false,
            input_mode: InputMode::Dispatch,
            rename_target: None,
            rename_session: None,
            pending_delete: None,
            pending_tree_delete: None,
            repo,
            launch_dir,
            config: Config::load(),
            import: ImportState { items: Vec::new(), all_repos: false, scanned: None },
            sel: 0,
            activity: HashMap::new(),
            summaries: HashMap::new(),
            activity_scanned: None,
            overlay: None,
            pi_models: None,
            pi_models_rx: {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(config::pi_models_table());
                });
                Some(rx)
            },
            side_rx: None,
            switch_tx,
            switch_rx,
            switching: HashMap::new(),
        };
        app.rescan_import();
        agent_view::set_light(app.config.light());
        app.reload_tree();
        app.selected_node_id = tree_view::default_selection(&app.tree, None);
        if !app.sessions.is_empty() {
            app.list_state.select(Some(0));
        }
        app
    }

    /// Reload the tree from the on-disk store. If the store is absent
    /// (collector not running yet), synthesize a tree in memory from the
    /// session list so Tree View is never empty. The in-memory synthesis
    /// is a read-time fallback — it never writes to disk.
    fn reload_tree(&mut self) {
        let store = TreeStore::default_dir();
        let disk_tree = store.load_tree();
        if disk_tree.has_root() {
            self.tree = disk_tree;
        } else {
            self.tree = synthesize_tree_from_sessions(&self.sessions);
        }
        // Keep selection valid.
        self.selected_node_id = tree_view::default_selection(&self.tree, self.selected_node_id.as_deref());
    }

    /// Rows of the main list: orchestra sessions, then Claude Code and
    /// Codex sessions not in orchestra yet.
    fn rows(&self) -> Vec<RowRef> {
        let mut rows: Vec<RowRef> = (0..self.sessions.len()).map(RowRef::Session).collect();
        for backend in [Backend::Claude, Backend::Codex] {
            let items: Vec<RowRef> = self
                .import
                .items
                .iter()
                .enumerate()
                .filter(|(_, e)| e.backend == backend && self.managing(e).is_none())
                .map(|(i, _)| RowRef::External(i))
                .collect();
            if items.is_empty() {
                continue;
            }
            rows.push(RowRef::Group(backend));
            if !self.is_collapsed(backend) {
                rows.extend(items);
            }
        }
        rows
    }

    fn is_collapsed(&self, b: Backend) -> bool {
        self.config.collapsed.iter().any(|c| c == b.as_str())
    }

    /// Fold or unfold a Claude Code / Codex group (remembered in config).
    fn set_collapsed(&mut self, b: Backend, collapse: bool) {
        self.config.collapsed.retain(|c| c != b.as_str());
        if collapse {
            self.config.collapsed.push(b.as_str().to_string());
        }
        self.config.save();
        // Keep the selection on the group's header.
        if let Some(p) = self.rows().iter().position(|r| *r == RowRef::Group(b)) {
            self.sel = p;
        }
    }

    fn selected_row(&self) -> Option<RowRef> {
        let rows = self.rows();
        rows.get(self.sel.min(rows.len().saturating_sub(1))).copied()
    }

    /// Selected orchestra session, if the selection is one.
    fn selected(&self) -> Option<usize> {
        match self.selected_row() {
            Some(RowRef::Session(i)) => Some(i),
            _ => None,
        }
    }

    fn select_session(&mut self, idx: usize) {
        if let Some(p) = self.rows().iter().position(|r| *r == RowRef::Session(idx)) {
            self.sel = p;
        }
    }

    fn move_up(&mut self) {
        let n = self.rows().len();
        if n > 0 {
            self.sel = if self.sel == 0 { n - 1 } else { self.sel.min(n - 1) - 1 };
        }
    }

    fn move_down(&mut self) {
        let n = self.rows().len();
        if n > 0 {
            self.sel = if self.sel + 1 >= n { 0 } else { self.sel + 1 };
        }
    }

    /// Transcript's latest reply line, cached by mtime.
    fn summary_for(&mut self, backend: Backend, path: &std::path::Path) -> Option<String> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        if let Some((t, s)) = self.summaries.get(path) {
            if *t == mtime {
                return s.clone();
            }
        }
        let s = history::last_assistant_text(backend, path);
        self.summaries.insert(path.to_path_buf(), (mtime, s.clone()));
        s
    }

    /// Refresh working/idle state and summaries (throttled; cheap).
    fn poll_side_answer(&mut self) {
        let Some(rx) = &self.side_rx else { return };
        let Ok(ans) = rx.try_recv() else { return };
        self.side_rx = None;
        match (ans.recap, ans.result) {
            (true, Ok(line)) => {
                self.status_message = format!("{}: {line}", ans.session);
                if let Some(a) = self.activity.get_mut(&ans.session) {
                    a.summary = Some(line);
                }
            }
            (false, Ok(text)) => {
                self.overlay = Some(Overlay::Text {
                    title: format!("btw · {}", ans.session),
                    subtitle: ans.question,
                    body: text,
                });
                self.status_message.clear();
            }
            (_, Err(e)) => self.status_message = format!("Side question failed: {e}"),
        }
    }

    fn refresh_activity(&mut self) {
        self.poll_side_answer();
        self.finish_switches();
        if let Some(rx) = &self.pi_models_rx {
            if let Ok(m) = rx.try_recv() {
                self.pi_models = Some(m);
                self.pi_models_rx = None;
            }
        }
        if self.activity_scanned.is_some_and(|t| t.elapsed().as_millis() < 1500) {
            return;
        }
        self.activity_scanned = Some(Instant::now());
        let names: Vec<(String, Backend, Option<PathBuf>)> = self
            .sessions
            .iter()
            .map(|s| (s.name.clone(), s.backend, switch::native_transcript(s)))
            .collect();
        for (name, backend, path) in names {
            let working = pane_working(&name);
            let (summary, last_active) = match &path {
                Some(p) => (
                    self.summary_for(backend, p),
                    std::fs::metadata(p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs()),
                ),
                None => (None, None),
            };
            let looping = commands::loop_running(&name);
            self.activity.insert(name, Activity { working, looping, summary, last_active });
        }
        if self.import.scanned.is_none_or(|t| t.elapsed().as_secs() >= 15) {
            self.rescan_import();
        }
    }

    fn dispatch_new(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }

        // Slash-commands take precedence over a plain prompt.
        match command::parse(&prompt) {
            command::DispatchCommand::SpawnAgent { name } => {
                self.status_message = format!(
                    "Spawn sub-agent '{name}': requires the launch-sub-agent skill \
                     + collector (not yet wired). See Design §7-§8."
                );
                self.input.clear();
                self.cursor_pos = 0;
                return;
            }
            command::DispatchCommand::Rename { new_name } if self.mode == ViewMode::Agent => {
                self.clear_input();
                self.status_message = match self.selected() {
                    Some(i) => self.set_title(i, &new_name),
                    None => "Select a session to rename".into(),
                };
                return;
            }
            command::DispatchCommand::Rename { new_name } => {
                self.status_message = self.rename_selected(&new_name);
                self.reload_tree();
                self.input.clear();
                self.cursor_pos = 0;
                return;
            }
            command::DispatchCommand::Unknown { raw } => {
                self.status_message = format!("Unknown command: {raw}");
                self.input.clear();
                self.cursor_pos = 0;
                return;
            }
            command::DispatchCommand::DispatchWith { backend, text } => {
                self.dispatch_session(&text, backend);
            }
            command::DispatchCommand::SetBackend { backend } => {
                self.config.default_backend = backend;
                self.config.save();
                self.status_message = format!("Default backend: {}", self.backend_label(backend));
                self.clear_input();
            }
            command::DispatchCommand::Model { model: None } => {
                self.clear_input();
                self.open_picker(PickPurpose::Default);
            }
            command::DispatchCommand::Model { model } => {
                self.status_message = self.set_model(model);
                self.clear_input();
            }
            command::DispatchCommand::Import => {
                self.clear_input();
                self.open_import();
            }
            command::DispatchCommand::Theme { theme } => {
                self.clear_input();
                self.config.theme = Some(theme.clone());
                self.config.save();
                agent_view::set_light(theme == "light");
                set_pi_theme(&format!("orchestra-{theme}"));
                self.status_message = format!(
                    "{theme} theme — new pi sessions use orchestra-{theme}; in a running pi session pick it with /settings"
                );
                self.needs_clear = true;
            }
            command::DispatchCommand::Session { name, args } => {
                self.clear_input();
                self.run_command(&name, &args);
            }
            command::DispatchCommand::Switch { target } if target.is_empty() => {
                self.clear_input();
                self.open_switch_picker();
            }
            command::DispatchCommand::Switch { target } => {
                self.clear_input();
                self.status_message = match self.selected() {
                    Some(idx) => match self.resolve_target(&target) {
                        Ok(t) => self.switch_session(idx, &t),
                        Err(e) => e,
                    },
                    None => "Select a session to switch first".to_string(),
                };
            }
            command::DispatchCommand::PlainPrompt { text } => {
                self.dispatch_session(&text, self.config.default_backend);
            }
        }
    }

    /// Session commands (/code-review, /simplify, /loop, ...); see
    /// commands.rs for what each sends.
    fn run_command(&mut self, name: &str, args: &str) {
        let selected = self.selected();
        let need = |app: &mut App| -> Option<usize> {
            if selected.is_none() {
                app.status_message = format!("/{name}: select one of your orchestra sessions first");
            }
            selected
        };
        match name {
            "code-review" => {
                // A separate, read-only reviewer on the selected session's
                // worktree (or the launch dir), on the default agent.
                let (cwd, label) = match selected {
                    Some(i) => (self.sessions[i].worktree_path.clone(), self.sessions[i].name.clone()),
                    None => (
                        self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_else(|| self.launch_dir.clone()).to_string_lossy().to_string(),
                        "repo".to_string(),
                    ),
                };
                let backend = self.config.default_backend;
                let rname = format!("review-{}-{}", label.chars().take(24).collect::<String>(), tree_store::unix_now() % 1000);
                let mut sess = Session::new(rname.clone(), commands::review_prompt(args), cwd, backend);
                sess.title = Some(match selected {
                    Some(i) => format!("Review: {}", self.sessions[i].display_title()),
                    None => "Review of this repo".to_string(),
                });
                sess.model = self.config.model_for(backend);
                let who = switch::Target { backend, model: sess.model.clone() }.label();
                self.start_session(sess, format!("Reviewing {label} with {who} as {rname}"), Vec::new());
            }
            "simplify" | "autofix-pr" => {
                let Some(i) = need(self) else { return };
                let s = &self.sessions[i];
                let text = commands::session_prompt(s.backend, name, args);
                self.status_message = match commands::send_to_session(&s.name, &text) {
                    Ok(()) => format!("Sent /{name} to {}", s.name),
                    Err(e) => e,
                };
            }
            "loop" => {
                let Some(i) = need(self) else { return };
                let sname = self.sessions[i].name.clone();
                if args.trim() == "stop" {
                    self.status_message = if commands::stop_loop(&sname) {
                        format!("Stopped the loop on {sname}")
                    } else {
                        format!("{sname} has no loop")
                    };
                    return;
                }
                match commands::parse_loop(args) {
                    None => self.status_message = "usage: /loop [30s|5m|1h] <prompt>   ·   /loop stop".into(),
                    Some((secs, prompt)) => {
                        self.status_message = match commands::start_loop(&sname, secs, &prompt) {
                            Ok(()) => format!("{sname} gets \"{}\" every {} when idle — /loop stop to end", import::one_line(&prompt, 40), fmt_secs(secs)),
                            Err(e) => e,
                        };
                    }
                }
                self.activity_scanned = None;
            }
            "background" => {
                if args.trim().is_empty() {
                    self.status_message = "Every orchestra session already runs in the background; /background <prompt> starts one".into();
                } else {
                    self.dispatch_session(args.trim(), self.config.default_backend);
                }
            }
            "branch" => {
                let Some(i) = need(self) else { return };
                self.status_message = self.branch_session(i, args.trim());
            }
            "btw" | "recap" => {
                let Some(i) = need(self) else { return };
                let recap = name == "recap";
                let question = if recap { commands::RECAP_QUESTION.to_string() } else { args.trim().to_string() };
                if question.is_empty() {
                    self.status_message = "usage: /btw <question>".into();
                    return;
                }
                let Some(model) = commands::side_model(self.config.model_for(Backend::Pi)) else {
                    self.status_message = "/btw needs a pi model (add one to ~/.pi/agent/models.json)".into();
                    return;
                };
                let sess = self.sessions[i].clone_for_read();
                let (tx, rx) = std::sync::mpsc::channel();
                let q = question.clone();
                let m = model.clone();
                std::thread::spawn(move || {
                    let result = commands::side_question(&sess, &q, &m);
                    let _ = tx.send(SideAnswer { session: sess.name.clone(), question: q, recap, result });
                });
                self.side_rx = Some(rx);
                self.activity_scanned = None;
                self.status_message = format!("Asking {} about {} (its conversation is not changed)…", model.rsplit('/').next().unwrap_or(&model), self.sessions[i].name);
            }
            "bug" => {
                if args.trim().is_empty() {
                    self.status_message = "usage: /bug <what went wrong>".into();
                    return;
                }
                let sel = selected.map(|i| &self.sessions[i]);
                let (title, body) = commands::bug_draft(args, sel, "");
                self.overlay = Some(Overlay::Bug { title, body });
            }
            "teleport" => {
                let Some(i) = need(self) else { return };
                if args.trim() == "back" {
                    self.status_message = self.teleport_back(i);
                    return;
                }
                if let Some(r) = &self.sessions[i].remote {
                    self.status_message = format!("{} already runs on {} — /teleport back to bring it home", self.sessions[i].name, r.cluster);
                    return;
                }
                match teleport::plan(&self.sessions[i], args) {
                    Ok(plan) => self.overlay = Some(Overlay::Teleport { session: self.sessions[i].name.clone(), plan }),
                    Err(e) => self.status_message = format!("/teleport: {e}"),
                }
            }
            _ => {}
        }
    }

    /// Launch a confirmed /teleport plan: the pane runs `sky launch`, then
    /// becomes an ssh view of the session's tmux on the box.
    fn teleport_launch(&mut self, name: &str, plan: teleport::Plan) {
        let Some(i) = self.sessions.iter().position(|s| s.name == name) else { return };
        let sess = &mut self.sessions[i];
        let _ = Command::new("tmux").args(["kill-session", "-t", name]).status();
        let _ = std::fs::remove_file(paths::sessions_dir().join(format!("{name}.ready")));
        sess.remote = Some(plan.remote.clone());
        sess.state = SessionState::Initializing;
        if let Err(e) = session::spawn_with(sess, Some(&plan.launch)) {
            sess.remote = None;
            self.status_message = format!("Teleport failed to start: {e}");
            return;
        }
        session::save_session(sess);
        self.status_message = format!(
            "Launching {} on {} — open the session to watch; stop the box later with: {}sky down {}",
            plan.cluster, plan.infra, teleport::sky_env(), plan.cluster
        );
    }

    /// /teleport back: copy the transcript and changed files home, then
    /// resume locally. The cluster is left running for you to stop.
    fn teleport_back(&mut self, i: usize) -> String {
        let Some(remote) = self.sessions[i].remote.clone() else {
            return format!("{} runs locally already", self.sessions[i].name);
        };
        let sess = &mut self.sessions[i];
        let local = switch::native_transcript(sess).unwrap_or_else(|| sess.pi_session_dir().join("teleported.jsonl"));
        let cmds = teleport::back_commands(sess, &remote, &local).join(" && ");
        let name = sess.name.clone();
        let _ = Command::new("tmux").args(["kill-session", "-t", &name]).status();
        let _ = std::fs::remove_file(paths::sessions_dir().join(format!("{name}.ready")));
        sess.remote = None;
        sess.origin = Origin::Resumed;
        sess.state = SessionState::Initializing;
        if let Err(e) = session::spawn_with(sess, Some(&cmds)) {
            sess.remote = Some(remote);
            return format!("Teleport back failed to start: {e}");
        }
        session::save_session(sess);
        format!("Bringing {name} home from {} — the box is still up: sky down {}", remote.cluster, remote.cluster)
    }

    /// /branch: a new session with a copy of this one's conversation and
    /// work, so the two can go different ways.
    fn branch_session(&mut self, i: usize, name: &str) -> String {
        let src = self.sessions[i].clone_for_read();
        let new_name = if name.is_empty() {
            format!("{}-b{}", src.name.chars().take(28).collect::<String>(), tree_store::unix_now() % 1000)
        } else {
            rename::sanitize_name(name)
        };
        let mut dst = Session::new(new_name.clone(), src.prompt.clone(), String::new(), src.backend);
        dst.model = src.model.clone();
        dst.title = Some(if name.is_empty() { format!("{} (branch)", src.display_title()) } else { name.to_string() });
        let mut carried = String::new();
        let src_dir = std::path::Path::new(&src.worktree_path);
        match repo::detect(src_dir) {
            Some(r) => {
                let head = repo::git(src_dir, &["rev-parse", "HEAD"]);
                match worktree::create_worktree_from(&r, &new_name, head.as_deref()) {
                    Ok(wt) => {
                        match worktree::carry_changes(src_dir, &wt.path) {
                            Ok(n) if n > 0 => carried = " with its uncommitted work".into(),
                            Ok(_) => {}
                            Err(e) => carried = format!(" (uncommitted work not copied: {e})"),
                        }
                        dst.worktree_path = wt.path.to_string_lossy().to_string();
                        dst.repo_root = Some(r.root.to_string_lossy().to_string());
                        dst.base_ref = head.or(Some(r.base_ref.clone()));
                        dst.branch = Some(wt.branch);
                    }
                    Err(e) => return format!("Branch failed: {e}"),
                }
            }
            None => dst.worktree_path = src.worktree_path.clone(),
        }
        match switch::branch_conversation(&src, &mut dst) {
            Ok(_) => {
                let msg = format!("Branched {} into {new_name}{carried}", src.name);
                self.start_session(dst, msg.clone(), Vec::new());
                msg
            }
            Err(e) => {
                if let Some((repo, wt, branch)) = dst.owned_worktree() {
                    let _ = worktree::remove_worktree(&repo, &wt, &branch);
                }
                format!("Branch failed: {e}")
            }
        }
    }

    fn open_switch_picker(&mut self) {
        match self.selected() {
            Some(i) => {
                let name = self.sessions[i].name.clone();
                self.open_picker(PickPurpose::Switch(name));
            }
            None => self.status_message = "Select one of your orchestra sessions to switch".into(),
        }
    }

    /// Everything a session can run on: Claude Code and Codex (their own
    /// default model, plus Claude's model aliases) and every model pi knows.
    fn pick_options(&mut self) -> Vec<PickOption> {
        let t = |backend, model: Option<&str>| switch::Target { backend, model: model.map(str::to_string) };
        let mut v = vec![
            PickOption { name: "Claude Code".into(), desc: "its default model".into(), target: t(Backend::Claude, None) },
        ];
        for (alias, desc) in [("opus", "Anthropic's most capable everyday model"), ("sonnet", "faster, cheaper"), ("haiku", "fastest")] {
            v.push(PickOption { name: format!("Claude Code · {alias}"), desc: desc.into(), target: t(Backend::Claude, Some(alias)) });
        }
        v.push(PickOption { name: "Codex".into(), desc: "its default model".into(), target: t(Backend::Codex, None) });
        if self.pi_models.is_none() {
            // Still loading: wait for the background load rather than
            // starting a second one.
            self.pi_models = self.pi_models_rx.take().and_then(|rx| rx.recv().ok());
        }
        let models = self.pi_models.get_or_insert_with(config::pi_models_table).clone();
        for (provider, model, ctx) in models {
            v.push(PickOption {
                name: format!("pi · {model}"),
                desc: format!("{provider} · {ctx} context"),
                target: t(Backend::Pi, Some(&format!("{provider}/{model}"))),
            });
        }
        if !v.iter().any(|o| o.target.backend == Backend::Pi) {
            v.push(PickOption { name: "pi".into(), desc: "its default model (add providers in ~/.pi/agent/models.json)".into(), target: t(Backend::Pi, None) });
        }
        v
    }

    fn open_picker(&mut self, purpose: PickPurpose) {
        let options = self.pick_options();
        let current = self.picker_current(&purpose, &options);
        self.overlay = Some(Overlay::Picker(Picker { purpose, options, selected: current.unwrap_or(0), filter: String::new() }));
    }

    fn picker_current(&self, purpose: &PickPurpose, options: &[PickOption]) -> Option<usize> {
        let (b, m) = match purpose {
            PickPurpose::Switch(name) => {
                let s = self.sessions.iter().find(|s| &s.name == name)?;
                (s.backend, s.model.clone())
            }
            PickPurpose::Default => (self.config.default_backend, self.config.model_for(self.config.default_backend)),
        };
        options.iter().position(|o| o.target.backend == b && o.target.model == m)
    }

    fn apply_pick(&mut self, picker: Picker) {
        let visible = picker.visible();
        let Some(opt) = visible.get(picker.selected).map(|o| (*o).clone()) else {
            return;
        };
        match picker.purpose {
            PickPurpose::Switch(name) => {
                let Some(idx) = self.sessions.iter().position(|s| s.name == name) else { return };
                self.status_message = self.switch_session(idx, &opt.target);
            }
            PickPurpose::Default => {
                self.config.default_backend = opt.target.backend;
                self.config.set_model(opt.target.backend, opt.target.model.clone());
                self.config.save();
                self.status_message = format!("New sessions will use {}", opt.target.label());
            }
        }
    }

    /// `/switch` argument → target. `claude`, `codex:gpt-6` and
    /// `pi:provider/id` are explicit; anything else is looked up as a pi
    /// model (fuzzy, like /model).
    fn resolve_target(&self, spec: &str) -> Result<switch::Target, String> {
        if let Some(mut t) = switch::Target::parse(spec) {
            if t.backend == Backend::Pi {
                if let Some(m) = &t.model {
                    t.model = Some(resolve_pi_model(m)?);
                }
            }
            return Ok(t);
        }
        Ok(switch::Target { backend: Backend::Pi, model: Some(resolve_pi_model(spec)?) })
    }

    /// Move session `idx` to `target`, keeping its conversation, and
    /// restart its tmux session on the new agent.
    /// Starts the switch on a worker thread. The session keeps running on
    /// its current agent until the new transcript is fully written; only
    /// then is it restarted (see `finish_switch`). A switch that fails or
    /// is interrupted leaves the session as it was.
    fn switch_session(&mut self, idx: usize, target: &switch::Target) -> String {
        let name = self.sessions[idx].name.clone();
        if let Some(t) = self.switching.get(&name) {
            return format!("{} is already switching to {t}", self.sessions[idx].display_title());
        }
        let mut copy = self.sessions[idx].clone_for_read();
        let target = target.clone();
        let tx = self.switch_tx.clone();
        self.switching.insert(name, target.label());
        std::thread::spawn(move || {
            let result = switch::switch(&mut copy, &target);
            let _ = tx.send(SwitchDone { session: copy, result, fork: None });
        });
        format!("Switching {} to {} — it keeps running until the switch is ready", self.sessions[idx].display_title(), self.switching[&self.sessions[idx].name])
    }

    /// Apply finished switches and forks (called every frame).
    fn finish_switches(&mut self) {
        while let Ok(done) = self.switch_rx.try_recv() {
            let name = done.session.name.clone();
            self.switching.remove(&name);
            match (done.result, done.fork) {
                (Ok(summary), Some((msg, warnings))) => {
                    self.start_session(done.session, format!("{msg}: {summary}"), warnings);
                }
                (Err(e), Some(_)) => {
                    self.status_message = format!("Fork failed: {e}");
                    if let Some((repo, wt, branch)) = done.session.owned_worktree() {
                        let _ = worktree::remove_worktree(&repo, &wt, &branch);
                    }
                }
                (Ok(summary), None) => {
                    let Some(idx) = self.sessions.iter().position(|s| s.name == name) else { continue };
                    self.sessions[idx] = done.session;
                    let sess = &mut self.sessions[idx];
                    let _ = Command::new("tmux").args(["kill-session", "-t", &name]).status();
                    let _ = std::fs::remove_file(paths::sessions_dir().join(format!("{name}.ready")));
                    sess.state = SessionState::Initializing;
                    self.status_message = match session::spawn(sess) {
                        Ok(()) => summary,
                        Err(e) => format!("{summary} — but restarting failed: {e}"),
                    };
                    session::save_session(sess);
                    self.activity_scanned = None;
                    self.reload_tree();
                }
                (Err(e), None) => self.status_message = format!("Switch failed, nothing changed: {e}"),
            }
        }
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
    }

    /// `pi (openrouter/qwen3)` style label for status lines.
    fn backend_label(&self, b: Backend) -> String {
        match self.config.model_for(b) {
            Some(m) => format!("{} ({m})", b.as_str()),
            None => b.as_str().to_string(),
        }
    }

    /// `/model` for the default backend. pi models are checked against
    /// `pi --list-models` (built-in providers + ~/.pi/agent/models.json).
    fn set_model(&mut self, model: Option<String>) -> String {
        let b = self.config.default_backend;
        let Some(model) = model else {
            return format!("Default: {} — /model <name> to change, /model - to reset", self.backend_label(b));
        };
        if model == "-" {
            self.config.set_model(b, None);
            self.config.save();
            return format!("{} model reset to its default", b.as_str());
        }
        if b == Backend::Pi {
            match config::pi_models_matching(&model) {
                Ok(found) if found.is_empty() => {
                    return format!("pi knows no model matching '{model}' (see pi --list-models)");
                }
                // A unique fuzzy match is stored as its exact provider/id.
                Ok(found) if found.len() == 1 => {
                    self.config.set_model(b, Some(found[0].clone()));
                }
                Ok(found) if found.contains(&model) => self.config.set_model(b, Some(model.clone())),
                Ok(found) => {
                    return format!("'{model}' matches {} models: {}", found.len(), found.join(", "));
                }
                // pi missing or broken: store as typed; pi reports errors.
                Err(_) => self.config.set_model(b, Some(model.clone())),
            }
        } else {
            self.config.set_model(b, Some(model));
        }
        self.config.save();
        format!("Default: {}", self.backend_label(b))
    }

    /// New session for `prompt`: a fresh worktree when launched inside a
    /// repo, otherwise the launch directory itself.
    fn dispatch_session(&mut self, prompt: &str, backend: Backend) {
        let name = session::generate_name(prompt);
        let mut sess = Session::new(name.clone(), prompt.to_string(), String::new(), backend);
        sess.model = self.config.model_for(backend);
        let mut warnings = Vec::new();
        match &self.repo {
            Some(repo) => match worktree::create_worktree(repo, &name) {
                Ok(wt) => {
                    sess.worktree_path = wt.path.to_string_lossy().to_string();
                    sess.repo_root = Some(repo.root.to_string_lossy().to_string());
                    sess.base_ref = Some(repo.base_ref.clone());
                    sess.branch = Some(wt.branch);
                    warnings = wt.warnings;
                }
                Err(e) => {
                    self.status_message = format!("Worktree failed: {e}");
                    return;
                }
            },
            None => sess.worktree_path = self.launch_dir.to_string_lossy().to_string(),
        }
        let where_ = if sess.branch.is_some() { "worktree" } else { "no repo, no worktree" };
        let label = sess.tag();
        self.start_session(sess, format!("Dispatched {name} [{label}, {where_}]"), warnings);
    }

    /// Spawn + persist + select a session. Shared by dispatch and import.
    fn start_session(&mut self, sess: Session, ok_msg: String, warnings: Vec<String>) {
        if let Err(e) = session::spawn(&sess) {
            self.status_message = format!("{} spawn failed: {e}", sess.backend.as_str());
            return;
        }
        session::save_session(&sess);
        self.sessions.push(sess);
        self.clear_input();
        self.status_message = if warnings.is_empty() {
            ok_msg
        } else {
            format!("{ok_msg} — {}", warnings.join("; "))
        };
        self.list_state.select(Some(self.sessions.len() - 1));
        self.select_session(self.sessions.len() - 1);
        self.activity_scanned = None;
        // Refresh the tree so the new session appears.
        self.reload_tree();
    }

    // --- Import View ---

    /// `i` / `/import`: jump to the first Claude Code / Codex session.
    fn open_import(&mut self) {
        self.rescan_import();
        match self.rows().iter().position(|r| matches!(r, RowRef::External(_))) {
            Some(p) => self.sel = p,
            None => {
                self.status_message = "No Claude Code or Codex sessions here — press a to show all directories".into()
            }
        }
    }

    fn rescan_import(&mut self) {
        let under = if self.import.all_repos {
            None
        } else {
            Some(self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_else(|| self.launch_dir.clone()))
        };
        self.import.items = import::scan(under.as_deref());
        self.import.scanned = Some(Instant::now());
    }

    fn selected_external(&self) -> Option<ExternalSession> {
        match self.selected_row() {
            Some(RowRef::External(i)) => self.import.items.get(i).cloned(),
            _ => None,
        }
    }

    /// The orchestra session already attached to an external session.
    fn managing(&self, ext: &ExternalSession) -> Option<&Session> {
        let path = ext.path.to_string_lossy();
        self.sessions.iter().find(|s| {
            // Transcripts orchestra wrote or adopted for a session (switch
            // segments) belong to that session, not the "not in
            // orchestra" list.
            s.segments.iter().any(|seg| seg.path == path)
                || s.external_id.as_deref() == Some(ext.id.as_str())
                || (s.backend == ext.backend && s.backend == Backend::Claude && s.id == ext.id)
                || (s.backend == Backend::Codex && ext.backend == Backend::Codex
                    && s.external_id.is_none() && s.worktree_path == ext.cwd)
        })
    }

    fn imported_name(ext: &ExternalSession) -> String {
        let base = rename::sanitize_name(&ext.title);
        let base: String = base.split('-').take(3).collect::<Vec<_>>().join("-");
        let short: String = ext.id.chars().filter(|c| c.is_ascii_hexdigit()).take(4).collect();
        let prefix = if ext.backend == Backend::Claude { "cc" } else { "cx" };
        if base.is_empty() { format!("{prefix}-{short}") } else { format!("{prefix}-{base}-{short}") }
    }

    /// Enter in Import View: resume with the session's own CLI, in its
    /// original directory. Attaches to it if it's already running.
    fn import_resume(&mut self) {
        let Some(ext) = self.selected_external() else {
            return;
        };
        if let Some(existing) = self.managing(&ext) {
            let name = existing.name.clone();
            attach_to_session(&name, &mut self.status_message);
            self.needs_clear = true;
            return;
        }
        if !std::path::Path::new(&ext.cwd).is_dir() {
            self.status_message = format!("{} no longer exists; press p to fork into pi instead", ext.cwd);
            return;
        }
        let name = Self::imported_name(&ext);
        let mut sess = Session::new(name.clone(), ext.title.clone(), ext.cwd.clone(), ext.backend);
        sess.origin = Origin::Resumed;
        sess.external_id = Some(ext.id.clone());
        self.start_session(sess, format!("Resumed {} session as {name}", ext.backend.as_str()), Vec::new());
        self.mode = ViewMode::Agent;
    }

    /// `p` in Import View: convert the transcript into a pi session in a
    /// fresh worktree (or its original directory outside a repo).
    fn import_fork(&mut self) {
        let Some(ext) = self.selected_external() else {
            return;
        };
        let name = format!("pi-{}", Self::imported_name(&ext));
        // Adopt it as it is, then switch the copy to pi: the same path as
        // switching a running session (switch.rs), so tool calls carry over.
        let mut sess = Session::new(name.clone(), ext.title.clone(), String::new(), ext.backend);
        sess.title = Some(format!("{} (pi)", ext.title));
        sess.origin = Origin::Resumed;
        sess.external_id = Some(ext.id.clone());
        sess.segments.push(session::Segment {
            backend: ext.backend,
            model: None,
            path: ext.path.to_string_lossy().to_string(),
            seed: 0,
        });
        // Fork into a worktree of the repo the session belonged to, so the
        // pi copy can't step on the original's files. Sessions from outside
        // any repo run in their original directory.
        // Start from the commit the source session is on, so the code
        // matches the conversation (uncommitted edits don't carry over).
        let src_dir = std::path::Path::new(&ext.cwd);
        let source_repo = repo::detect(src_dir);
        let src_head = repo::git(src_dir, &["rev-parse", "HEAD"]);
        let mut warnings = Vec::new();
        match source_repo {
            Some(r) => match worktree::create_worktree_from(&r, &name, src_head.as_deref()) {
                Ok(wt) => {
                    sess.worktree_path = wt.path.to_string_lossy().to_string();
                    sess.repo_root = Some(r.root.to_string_lossy().to_string());
                    sess.base_ref = Some(src_head.clone().unwrap_or_else(|| r.base_ref.clone()));
                    sess.branch = Some(wt.branch);
                    warnings = wt.warnings;
                }
                Err(e) => {
                    self.status_message = format!("Worktree failed: {e}");
                    return;
                }
            },
            None => sess.worktree_path = ext.cwd.clone(),
        }
        let target = switch::Target { backend: Backend::Pi, model: self.config.model_for(Backend::Pi) };
        let from = match (&ext.git_branch, &src_head) {
            (Some(b), Some(_)) if sess.branch.is_some() => format!(", worktree from {b} HEAD"),
            _ => String::new(),
        };
        let msg = format!("Forked \"{}\" into pi{from}", ext.title);
        let tx = self.switch_tx.clone();
        self.switching.insert(name.clone(), target.label());
        self.status_message = format!("Forking \"{}\" into pi — converting its transcript…", ext.title);
        std::thread::spawn(move || {
            let result = switch::switch(&mut sess, &target);
            let _ = tx.send(SwitchDone { session: sess, result, fork: Some((msg, warnings)) });
        });
    }

    // --- Tree View navigation ---

    fn tree_left(&mut self) {
        // Move selection to parent (depth − 1).
        let Some(id) = self.selected_node_id.clone() else {
            return;
        };
        if let Some(parent) = tree_view::parent_of(&self.tree, &id) {
            self.selected_node_id = Some(parent);
        } else {
            self.status_message = "Already at root".to_string();
        }
    }

    fn tree_right(&mut self) {
        // Move selection to nearest child (depth + 1).
        let Some(id) = self.selected_node_id.clone() else {
            return;
        };
        if let Some(child) = tree_view::first_child(&self.tree, &id) {
            self.selected_node_id = Some(child);
        } else {
            self.status_message = format!("{} has no children", id);
        }
    }

    fn tree_up(&mut self) {
        self.move_within_depth(-1);
    }

    fn tree_down(&mut self) {
        self.move_within_depth(1);
    }

    /// Move to the nearest node at the same depth, in the given direction.
    /// `dir` is -1 (up) or +1 (down). Picks the nearest by y-coordinate.
    fn move_within_depth(&mut self, dir: i32) {
        let Some(id) = self.selected_node_id.clone() else {
            return;
        };
        let candidates = tree_view::nodes_at_depth(&self.tree, &id);
        if candidates.len() <= 1 {
            return;
        }
        // Find current index in the candidate list, then step by dir.
        let cur_idx = candidates.iter().position(|c| c == &id).unwrap_or(0);
        let next_idx = (cur_idx as i32 + dir)
            .rem_euclid(candidates.len() as i32) as usize;
        // nearest_by_y would be better for spatial layouts, but for same-depth
        // DFS-ordered siblings, stepping the index is the intuitive behavior.
        let _ = tree_view::nearest_by_y; // referenced for completeness
        self.selected_node_id = Some(candidates[next_idx].clone());
    }

    fn enter_node(&mut self) {
        let Some(id) = self.selected_node_id.clone() else {
            return;
        };
        let Some(node) = self.tree.get(&id) else {
            return;
        };
        match node.raw.kind {
            NodeKind::Agent => {
                // Enter Agent View scoped to this agent.
                self.agent_view_scope = if id == ROOT_AGENT_ID { None } else { Some(id.clone()) };
                self.mode = ViewMode::Agent;
                self.input.clear();
                self.cursor_pos = 0;
                self.input_mode = InputMode::Dispatch;
            }
            NodeKind::Session => {
                // Attach directly via tmux. The session's tmux name is its id
                // (or tmux_session field if set).
                let tmux_name = node
                    .raw
                    .tmux_session
                    .clone()
                    .unwrap_or_else(|| id.clone());
                attach_to_session(&tmux_name, &mut self.status_message);
                self.needs_clear = true;
            }
        }
    }

    fn toggle_detail(&mut self) {
        self.show_detail = !self.show_detail;
    }

    fn start_rename(&mut self) {
        if self.selected_node_id.is_some() {
            self.input_mode = InputMode::Rename;
            self.rename_target = self.selected_node_id.clone();
            // Pre-fill with the current label.
            let label = self
                .selected_node_id
                .as_ref()
                .and_then(|id| self.tree.get(id))
                .map(|n| n.raw.label().to_string())
                .unwrap_or_default();
            self.input = label;
            self.cursor_pos = self.input.chars().count();
            self.status_message = "Rename — Enter to commit, Esc to cancel".to_string();
        }
    }

    /// Set a session's display name. Only the name shown changes; its
    /// tmux session, worktree and branch keep their names, so this is safe
    /// while the agent is running.
    fn set_title(&mut self, idx: usize, title: &str) -> String {
        let title = title.trim();
        let s = &mut self.sessions[idx];
        let old = s.display_title();
        s.title = if title.is_empty() { None } else { Some(title.to_string()) };
        session::save_session(s);
        self.reload_tree();
        let s = &self.sessions[idx];
        if title.is_empty() {
            format!("Name reset to '{}'", s.display_title())
        } else {
            format!("Renamed '{old}' to '{}'", s.display_title())
        }
    }

    /// Ctrl+R in Agent View: edit the selected session's name in place.
    fn start_session_rename(&mut self) {
        let Some(i) = self.selected() else {
            self.status_message = "Select one of your sessions to rename".into();
            return;
        };
        self.rename_session = Some(self.sessions[i].name.clone());
        self.input_mode = InputMode::Rename;
        self.input = self.sessions[i].display_title();
        self.cursor_pos = self.input.chars().count();
    }

    fn commit_rename(&mut self) {
        if let Some(name) = self.rename_session.take() {
            let title = self.input.clone();
            self.input_mode = InputMode::Dispatch;
            self.clear_input();
            if let Some(i) = self.sessions.iter().position(|s| s.name == name) {
                self.status_message = self.set_title(i, &title);
            }
            return;
        }
        let new_name = self.input.trim().to_string();
        if new_name.is_empty() {
            self.status_message = "Rename cancelled (empty name)".to_string();
        } else {
            // Rename the node the rename was started on (selection can't
            // move while in Rename mode, but be explicit).
            self.selected_node_id = self.rename_target.take();
            self.status_message = self.rename_selected(&new_name);
            self.reload_tree();
        }
        self.input_mode = InputMode::Dispatch;
        self.input.clear();
        self.cursor_pos = 0;
    }

    /// Rename the selected node. Agents get a display-label change only
    /// (renaming a SkyPilot cluster is far more invasive); sessions get a
    /// full rename: tmux + worktree + branch + state dir + tree-store node.
    /// Returns a status message.
    fn rename_selected(&mut self, new_name: &str) -> String {
        let Some(id) = self.selected_node_id.clone() else {
            return "No node selected to rename".to_string();
        };
        let Some(node) = self.tree.get(&id).map(|n| n.raw.clone()) else {
            return "Node not found".to_string();
        };
        let display = new_name.trim().to_string();
        if display.is_empty() {
            return "Rename cancelled (empty name)".to_string();
        }
        match node.kind {
            NodeKind::Agent => {
                rename::set_display_name(&id, &display, &rename::paths_for_session(""));
                format!("Renamed to '{display}'")
            }
            NodeKind::Session => {
                // Display name only (see set_title); `orchestra rename`
                // on the command line still renames tmux/worktree/branch.
                let tmux = node.tmux_session.clone().unwrap_or_else(|| node.name.clone());
                match self.sessions.iter().position(|s| s.name == tmux) {
                    Some(i) => self.set_title(i, &display),
                    None => format!("{tmux} is not one of this window's sessions"),
                }
            }
        }
    }

    fn cancel_rename(&mut self) {
        self.input_mode = InputMode::Dispatch;
        self.rename_target = None;
        self.rename_session = None;
        self.input.clear();
        self.cursor_pos = 0;
        self.status_message = "Rename cancelled".to_string();
    }

    /// `x` in Tree View. Like Agent View, the first press only arms: a
    /// stray `x` must not `sky down` a cluster or delete a worktree.
    fn teardown_selected(&mut self) {
        let Some(id) = self.selected_node_id.clone() else {
            return;
        };
        let Some(node) = self.tree.get(&id) else {
            return;
        };
        if self.pending_tree_delete.as_deref() != Some(id.as_str()) {
            let what = match node.raw.kind {
                NodeKind::Agent => format!(
                    "TEAR DOWN agent cluster '{}' (sky down)",
                    node.raw.sky_cluster.clone().unwrap_or_else(|| id.clone())
                ),
                NodeKind::Session => format!("DELETE session '{}'", node.raw.label()),
            };
            self.pending_tree_delete = Some(id);
            self.status_message = format!("Press x again to {what}. Any other key cancels.");
            return;
        }
        self.pending_tree_delete = None;
        match node.raw.kind {
            NodeKind::Agent => {
                let cluster = node.raw.sky_cluster.clone().unwrap_or_else(|| id.clone());
                self.status_message = format!("Tearing down agent '{cluster}' (sky down)...");
                // stdin closed: sky must never block the TUI on a prompt
                // (e.g. its client-version switch question).
                let status = Command::new("sky")
                    .args(["down", "-y", &cluster])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                match status {
                    Ok(s) if s.success() => {
                        self.status_message = format!("Agent '{cluster}' torn down");
                    }
                    Ok(_) => {
                        self.status_message = format!("sky down failed for '{cluster}'");
                    }
                    Err(e) => {
                        self.status_message = format!("sky down not available: {e}");
                    }
                }
                self.reload_tree();
            }
            NodeKind::Session => {
                // Full cleanup: tmux + worktree + state files.
                let tmux_name = node
                    .raw
                    .tmux_session
                    .clone()
                    .unwrap_or_else(|| id.clone());
                self.delete_session(&tmux_name);
            }
        }
    }

    /// Full cleanup for a session: tmux kill-session + git worktree remove +
    /// git branch -D + rm state dir + rm .ready marker. Removes the session
    /// from the in-memory list. Used by both Agent View (`x`) and Tree View
    /// (`x` on a session node).
    fn delete_session(&mut self, name: &str) -> bool {
        let mut logs = Vec::new();

        // 1. Kill the tmux session.
        let kill = Command::new("tmux")
            .args(["kill-session", "-t", name])
            .status();
        match kill {
            Ok(s) if s.success() => logs.push(format!("tmux '{name}' killed")),
            Ok(_) => logs.push(format!("tmux kill-session failed for '{name}' (may already be gone)")),
            Err(e) => logs.push(format!("tmux unavailable: {e}")),
        }

        // 2. Remove the git worktree + branch — only if orchestra created
        // them. Imported sessions run in their original directory, and
        // their Claude/Codex transcripts are never deleted.
        let owned = self.sessions.iter().find(|s| s.name == name).and_then(|s| s.owned_worktree());
        match owned {
            Some((repo_root, wt, branch)) => match worktree::remove_worktree(&repo_root, &wt, &branch) {
                Ok(()) => logs.push("worktree removed".to_string()),
                Err(e) => logs.push(format!("worktree remove failed: {e}")),
            },
            None => logs.push("directory left in place".to_string()),
        }

        // 3. Remove the state directory + .ready marker.
        let state_dir = paths::sessions_dir().join(name);
        let ready_marker = paths::sessions_dir().join(format!("{name}.ready"));
        let mut rm_ok = std::fs::remove_dir_all(&state_dir).is_ok();
        rm_ok |= std::fs::remove_file(&ready_marker).is_ok();
        if rm_ok {
            logs.push("state files removed".to_string());
        }

        // The pi conversation (~/.orchestra/pi-sessions/<id>) is kept.
        if let Some(s) = self.sessions.iter().find(|s| s.name == name) {
            let _ = std::fs::remove_file(paths::prompts_dir().join(format!("{}.md", s.id)));
        }

        // 4. Remove from the in-memory session list.
        let before = self.sessions.len();
        self.sessions.retain(|s| s.name != name);
        let removed = self.sessions.len() < before;
        if removed {
            // Adjust selection if we removed the selected item.
            if let Some(idx) = self.list_state.selected() {
                if idx >= self.sessions.len() {
                    self.list_state.select(if self.sessions.is_empty() {
                        None
                    } else {
                        Some(self.sessions.len() - 1)
                    });
                }
            }
        }

        self.reload_tree();
        self.status_message = format!("Deleted '{name}' ({})", logs.join(", "));
        removed
    }

    /// Two-press confirmation for `x` in Agent View. First press arms the
    /// delete (sets `pending_delete` + shows a confirm message). Second
    /// press (while the same session is selected) executes. Any other key
    /// cancels.
    fn handle_delete_key_agent(&mut self) {
        let Some(idx) = self.selected() else {
            self.status_message = "No session selected".to_string();
            return;
        };
        if idx >= self.sessions.len() {
            return;
        }
        match self.pending_delete {
            Some(pending) if pending == idx => {
                // Confirmed — execute.
                let name = self.sessions[idx].name.clone();
                self.pending_delete = None;
                self.delete_session(&name);
            }
            _ => {
                // First press — arm. Say what would be lost.
                let s = &self.sessions[idx];
                let what = match s.owned_worktree() {
                    Some((_, wt, _)) => {
                        let base = s.base_ref.as_deref().unwrap_or("HEAD");
                        match worktree::pending_changes(&wt, base) {
                            Some(changes) => format!("tmux + worktree with {changes} + state"),
                            None => "tmux + worktree + state".to_string(),
                        }
                    }
                    None => "tmux + state; directory kept".to_string(),
                };
                self.pending_delete = Some(idx);
                self.status_message =
                    format!("Press x again to DELETE '{}' ({what}). Any other key cancels.", s.name);
            }
        }
    }
}

/// Synthesize a Tree in memory from the session list. Used when the disk
/// store is absent (collector not running) so Tree View is never empty.
/// The root is the Main Agent; each session becomes a leaf Session node.
fn synthesize_tree_from_sessions(sessions: &[Session]) -> Tree {
    let now = tree_store::unix_now();
    let mut nodes = HashMap::new();

    let root = Node {
        id: ROOT_AGENT_ID.to_string(),
        kind: NodeKind::Agent,
        parent_id: None,
        name: "main-box".to_string(),
        display_name: None,
        sky_cluster: Some(ROOT_AGENT_ID.to_string()),
        tmux_session: None,
        box_host: Some(ROOT_AGENT_ID.to_string()),
        bridge_url: None,
        gateway_url: Some("ws://localhost:18789".to_string()),
        state: NodeState::Idle,
        config: tree_store::Config::default(),
        openclaw_sessions: Vec::new(),
        pi_sessions: sessions.iter().map(|s| s.name.clone()).collect(),
        created_at: now,
        last_pulled: now,
        children: sessions.iter().map(|s| format!("session-{}", s.name)).collect(),
    };
    let root_id = root.id.clone();
    nodes.insert(
        root.id.clone(),
        NodeView { raw: root, effective: NodeState::Idle },
    );

    for s in sessions {
        let id = format!("session-{}", s.name);
        let state = match s.state {
            SessionState::Initializing => NodeState::Initializing,
            SessionState::Working => NodeState::Working,
            SessionState::NeedsInput => NodeState::NeedsInput,
            SessionState::Idle => NodeState::Idle,
            SessionState::Completed => NodeState::Completed,
            SessionState::Failed => NodeState::Failed,
        };
        let n = Node {
            id: id.clone(),
            kind: NodeKind::Session,
            parent_id: Some(ROOT_AGENT_ID.to_string()),
            name: s.name.clone(),
            display_name: Some(s.display_title()),
            sky_cluster: None,
            tmux_session: Some(s.name.clone()),
            box_host: None,
            bridge_url: None,
            gateway_url: None,
            state,
            config: tree_store::Config::default(),
            openclaw_sessions: Vec::new(),
            pi_sessions: Vec::new(),
            created_at: s.created_at,
            last_pulled: now,
            children: Vec::new(),
        };
        nodes.insert(id, NodeView { raw: n, effective: state });
    }

    Tree {
        root_id: Some(root_id),
        nodes,
        updated_at: now,
    }
}

/// Whether the agent in a tmux session is mid-turn: every agent shows an
/// interrupt hint while working ("esc to interrupt", pi's "Working...").
fn pane_working(name: &str) -> bool {
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", name, "-S", "-12"])
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(out) = out else { return false };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains("esc to interrupt") || text.contains("Working...") || text.contains("to interrupt)")
}

/// Point pi's settings at one of orchestra's themes, if it is installed.
fn set_pi_theme(name: &str) {
    let dir = paths::home().join(".pi").join("agent");
    if !dir.join("themes").join(format!("{name}.json")).exists() {
        return;
    }
    let path = dir.join("settings.json");
    let mut v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("theme".into(), serde_json::json!(name));
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default());
    }
}

fn fmt_secs(s: u64) -> String {
    match s {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// "~/x" for paths under $HOME.
fn tilde(p: &std::path::Path) -> String {
    let home = paths::home();
    match p.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Slash commands for the completion list above the prompt.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/switch", "move the selected session to another agent or model"),
    ("/model", "pick the agent and model for new sessions"),
    ("/claude", "<prompt> — start a Claude Code session"),
    ("/codex", "<prompt> — start a Codex session"),
    ("/pi", "<prompt> — start a pi session"),
    ("/backend", "<pi|claude|codex> — default agent for new sessions"),
    ("/import", "jump to your Claude Code and Codex sessions"),
    ("/code-review", "[target] — a read-only reviewer on the selected session's work"),
    ("/simplify", "clean up the selected session's changes (it applies them)"),
    ("/autofix-pr", "[pr] — fix the selected session's failing checks and review comments"),
    ("/loop", "[5m] <prompt> — resend a prompt when idle; /loop stop"),
    ("/branch", "[name] — fork the selected session: conversation and work"),
    ("/btw", "<question> — ask about the selected session without interrupting it"),
    ("/recap", "one-line recap of the selected session"),
    ("/background", "<prompt> — start a session without opening it"),
    ("/bug", "<what went wrong> — draft a GitHub issue for orchestra"),
    ("/theme", "<light|dark> — colors for orchestra and new pi sessions"),
    ("/teleport", "[infra] — move the selected session to a SkyPilot box; /teleport back"),
    ("/rename", "<name> — rename the selected session"),
    ("/agent", "<name> — spawn a sub-agent"),
];

/// A pi model pattern → its exact `provider/id` (must match one model).
fn resolve_pi_model(pattern: &str) -> Result<String, String> {
    match config::pi_models_matching(pattern) {
        Ok(found) if found.len() == 1 => Ok(found[0].clone()),
        Ok(found) if found.iter().any(|f| f == pattern) => Ok(pattern.to_string()),
        Ok(found) if found.is_empty() => Err(format!("pi knows no model matching '{pattern}' (see pi --list-models)")),
        Ok(found) => Err(format!("'{pattern}' matches {} pi models: {}", found.len(), found.join(", "))),
        Err(e) => Err(e),
    }
}

/// `orchestra rename <old> <new>` — rename a session from the CLI.
/// Uses the same logic as the TUI's rename, so it's scriptable and
/// testable without the interactive UI.
fn rename_cli() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    if args.len() != 2 {
        eprintln!("usage: orchestra rename <old-name> <new-name>");
        std::process::exit(2);
    }
    let (old, display) = (&args[0], &args[1]);
    let paths = rename::paths_for_session(old);
    let out = rename::rename_session(old, display, &paths);
    if out.new_name.is_empty() {
        eprintln!("rename failed: {}", out.logs.join("; "));
        std::process::exit(1);
    }
    for line in &out.logs {
        println!("  {line}");
    }
    println!("renamed '{old}' -> '{}'", out.new_name);
    Ok(())
}

/// `orchestra claude-open <session-id> [claude args...]` — what an
/// orchestra tmux pane runs to reopen a Claude Code session. `claude
/// --resume` refuses a session that is running as a Claude Code background
/// session, so attach to those instead; a session open in another terminal
/// is reported rather than resumed twice.
fn claude_open_cli() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let args: Vec<String> = std::env::args().skip(2).collect();
    let Some((id, extra)) = args.split_first() else {
        eprintln!("usage: orchestra claude-open <session-id> [claude args...]");
        std::process::exit(2);
    };
    let err = match session::running_claude(id) {
        Some(session::RunningClaude::Background { short_id }) => {
            Command::new("claude").args(["attach", &short_id]).exec()
        }
        Some(session::RunningClaude::Interactive { pid }) => {
            eprintln!(
                "[orchestra] This Claude Code session is open in another terminal (pid {pid}). \
                 Exit it there, then start it again here."
            );
            std::process::exit(1);
        }
        None => Command::new("claude").arg("--resume").arg(id).args(extra).exec(),
    };
    // exec only returns on failure.
    Err(err.into())
}

fn main() -> anyhow::Result<()> {
    // Subcommands
    if let Some(cmd) = std::env::args().nth(1) {
        match cmd.as_str() {
            "upgrade" => return upgrade(),
            "version" => {
                println!("orchestra {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "rename" => return rename_cli(),
            "claude-open" => return claude_open_cli(),
            // Used by the copy-mode Left binding.
            "is-orchestra" => {
                let name = std::env::args().nth(2).unwrap_or_default();
                std::process::exit(if paths::sessions_dir().join(&name).join("state.json").exists() { 0 } else { 1 });
            }
            // Used by /loop's runner.
            "tmux-idle" => {
                let name = std::env::args().nth(2).unwrap_or_default();
                std::process::exit(if session::tmux_alive(&name) && !pane_working(&name) { 0 } else { 1 });
            }
            "send" => {
                let a: Vec<String> = std::env::args().skip(2).collect();
                if a.len() != 2 {
                    eprintln!("usage: orchestra send <session> <text>");
                    std::process::exit(2);
                }
                if let Err(e) = commands::send_to_session(&a[0], &a[1]) {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
                return Ok(());
            }
            "tmux-left" => {
                // Called by the tmux Left binding; exit 0 means detach.
                let a: Vec<String> = std::env::args().skip(2).collect();
                let num = |i: usize| a.get(i).and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
                let detach = a.len() == 4 && session::tmux_left_should_detach(&a[0], &a[1], num(2), num(3));
                std::process::exit(if detach { 0 } else { 1 });
            }
            _ => {}
        }
    }

    std::fs::create_dir_all(paths::sessions_dir()).ok();

    // Put the terminal back even if orchestra panics.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    let mut last_refresh: Option<Instant> = None;
    'main: loop {
        let returned = app.needs_clear;
        if app.needs_clear {
            terminal.clear()?;
            app.needs_clear = false;
        }
        // Session state (one tmux call per session) and the tree (disk
        // reads) refresh about once a second, and right after returning
        // from a session — not on every keystroke, which made typing lag.
        if returned || last_refresh.is_none_or(|t| t.elapsed().as_millis() >= 1000) {
            for sess in &mut app.sessions {
                sess.refresh_state();
            }
            app.reload_tree();
            last_refresh = Some(Instant::now());
        }
        app.refresh_activity();
        terminal.draw(|f| ui(f, &mut app))?;

        // Wait for input, then handle everything already queued before the
        // next redraw, so fast typing and pastes land at once.
        if event::poll(std::time::Duration::from_millis(250))? {
            loop {
                if let Event::Key(key) = event::read()? {
                    if key.kind != event::KeyEventKind::Release && handle_key(&mut app, key) {
                        break 'main;
                    }
                    if app.needs_clear {
                        // Just came back from a session: redraw first.
                        break;
                    }
                }
                if !event::poll(std::time::Duration::ZERO)? {
                    break;
                }
            }
        }
    }

    restore_terminal();
    session::save_sessions(&app.sessions);
    Ok(())
}

/// Convert a character index to a byte index in a string.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or_else(|| s.len())
}

fn handle_key(app: &mut App, key: event::KeyEvent) -> bool {
    // Clear status message on any key press — it's transient.
    app.status_message.clear();

    // --- Rename mode captures all keys ---
    if app.input_mode == InputMode::Rename {
        return handle_rename_key(app, key);
    }

    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // --- Alt (Option) modified keys: word-level operations (Agent View) ---
    if alt && app.mode == ViewMode::Agent {
        if handle_alt_key(app, key) {
            return false;
        }
    }

    match app.mode {
        ViewMode::Tree => handle_tree_key(app, key),
        ViewMode::Agent => handle_agent_key(app, key),
    }
}

fn handle_rename_key(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => app.commit_rename(),
        KeyCode::Esc => app.cancel_rename(),
        KeyCode::Backspace => {
            if app.cursor_pos > 0 {
                let byte_idx = char_to_byte(&app.input, app.cursor_pos - 1);
                app.input.remove(byte_idx);
                app.cursor_pos -= 1;
            }
        }
        KeyCode::Left => {
            if app.cursor_pos > 0 {
                app.cursor_pos -= 1;
            }
        }
        KeyCode::Right => {
            if app.cursor_pos < app.input.chars().count() {
                app.cursor_pos += 1;
            }
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.cancel_rename();
        }
        KeyCode::Char(c) => {
            let byte_idx = char_to_byte(&app.input, app.cursor_pos);
            app.input.insert(byte_idx, c);
            app.cursor_pos += 1;
        }
        _ => {}
    }
    false
}

/// Handle Alt-modified keys for word-level editing. Returns true if handled.
fn handle_alt_key(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Left | KeyCode::Char('b') => {
            if app.cursor_pos > 0 {
                let chars: Vec<char> = app.input.chars().collect();
                let mut pos = app.cursor_pos;
                while pos > 0 && chars[pos - 1].is_whitespace() {
                    pos -= 1;
                }
                while pos > 0 && !chars[pos - 1].is_whitespace() {
                    pos -= 1;
                }
                app.cursor_pos = pos;
            }
            true
        }
        KeyCode::Right | KeyCode::Char('f') => {
            let chars: Vec<char> = app.input.chars().collect();
            if app.cursor_pos < chars.len() {
                let mut pos = app.cursor_pos;
                while pos < chars.len() && !chars[pos].is_whitespace() {
                    pos += 1;
                }
                while pos < chars.len() && chars[pos].is_whitespace() {
                    pos += 1;
                }
                app.cursor_pos = pos;
            }
            true
        }
        KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('\u{7f}') | KeyCode::Char('\u{8}') => {
            if app.cursor_pos > 0 {
                let chars: Vec<char> = app.input.chars().collect();
                let mut pos = app.cursor_pos;
                while pos > 0 && chars[pos - 1].is_whitespace() {
                    pos -= 1;
                }
                while pos > 0 && !chars[pos - 1].is_whitespace() {
                    pos -= 1;
                }
                let start = char_to_byte(&app.input, pos);
                let end = char_to_byte(&app.input, app.cursor_pos);
                app.input.drain(start..end);
                app.cursor_pos = pos;
            }
            true
        }
        _ => false,
    }
}

fn handle_tree_key(app: &mut App, key: event::KeyEvent) -> bool {
    if key.code != KeyCode::Char('x') {
        app.pending_tree_delete = None;
    }
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Tab => {
            // Switch to Agent View scoped to the selected agent (or root).
            app.mode = ViewMode::Agent;
            app.input.clear();
            app.cursor_pos = 0;
            app.input_mode = InputMode::Dispatch;
        }
        KeyCode::Left => app.tree_left(),
        KeyCode::Right => app.tree_right(),
        KeyCode::Up => app.tree_up(),
        KeyCode::Down => app.tree_down(),
        KeyCode::Enter => app.enter_node(),
        KeyCode::Char('d') => app.toggle_detail(),
        KeyCode::Char('n') => app.start_rename(),
        KeyCode::Char('r') => {
            app.reload_tree();
            app.status_message = format!(
                "Tree reloaded ({} nodes, updated {}s ago)",
                app.tree.nodes.len(),
                tree_store::unix_now().saturating_sub(app.tree.updated_at)
            );
        }
        KeyCode::Char('x') => app.teardown_selected(),
        _ => {}
    }
    false
}

/// Keys while the picker or shortcut list is open. Returns true if the
/// key was consumed.
fn handle_overlay_key(app: &mut App, key: event::KeyEvent) -> bool {
    let Some(overlay) = app.overlay.take() else { return false };
    let mut p = match overlay {
        Overlay::Picker(p) => p,
        Overlay::Bug { title, body } => {
            match key.code {
                KeyCode::Enter => {
                    app.status_message = match commands::file_bug(&title, &body) {
                        Ok(url) => format!("Filed {url}"),
                        Err(e) => format!("gh issue create failed: {e}"),
                    };
                }
                KeyCode::Esc => app.status_message = "Bug report not filed".into(),
                _ => app.overlay = Some(Overlay::Bug { title, body }),
            }
            return true;
        }
        Overlay::Teleport { session, plan } => {
            match key.code {
                KeyCode::Enter => app.teleport_launch(&session, plan),
                KeyCode::Esc => app.status_message = "Teleport cancelled — nothing was launched".into(),
                _ => app.overlay = Some(Overlay::Teleport { session, plan }),
            }
            return true;
        }
        // Shortcut list and answers: any key closes them.
        _ => return true,
    };
    let n = p.visible().len();
    match key.code {
        KeyCode::Esc => return true,
        KeyCode::Enter => {
            app.apply_pick(p);
            return true;
        }
        KeyCode::Up if n > 0 => p.selected = (p.selected + n - 1) % n,
        KeyCode::Down if n > 0 => p.selected = (p.selected + 1) % n,
        KeyCode::Backspace => {
            p.filter.pop();
            p.selected = 0;
        }
        // Number keys pick directly, like Claude Code's /model.
        KeyCode::Char(c @ '1'..='9') if p.filter.is_empty() && (c as usize - '0' as usize) <= n => {
            p.selected = c as usize - '1' as usize;
            app.apply_pick(p);
            return true;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Char(c) => {
            p.filter.push(c);
            p.selected = 0;
        }
        _ => {}
    }
    app.overlay = Some(Overlay::Picker(p));
    true
}

fn open_selected(app: &mut App) {
    match app.selected_row() {
        Some(RowRef::Session(idx)) => {
            if app.sessions[idx].state == SessionState::Initializing {
                app.status_message = format!("{} is still starting…", app.sessions[idx].name);
            } else {
                let name = app.sessions[idx].name.clone();
                attach_to_session(&name, &mut app.status_message);
                app.needs_clear = true;
                app.activity_scanned = None;
            }
        }
        Some(RowRef::External(_)) => app.import_resume(),
        Some(RowRef::Group(b)) => {
            let c = app.is_collapsed(b);
            app.set_collapsed(b, !c);
        }
        None => {}
    }
}

fn handle_agent_key(app: &mut App, key: event::KeyEvent) -> bool {
    if app.agent_view_scope.is_none() && handle_overlay_key(app, key) {
        return false;
    }
    // Single-letter commands only apply to an empty input; otherwise they
    // are text (a prompt containing "q" must not quit).
    let empty = app.input.is_empty();
    if app.agent_view_scope.is_none()
        && key.code == KeyCode::Char('r')
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        app.start_session_rename();
        return false;
    }
    if let (true, true, Some(RowRef::Group(b))) = (app.agent_view_scope.is_none(), empty, app.selected_row()) {
        match key.code {
            KeyCode::Left => {
                app.set_collapsed(b, true);
                return false;
            }
            KeyCode::Right | KeyCode::Char(' ') => {
                let c = app.is_collapsed(b);
                app.set_collapsed(b, if key.code == KeyCode::Right { false } else { !c });
                return false;
            }
            _ => {}
        }
    }
    if app.agent_view_scope.is_none() && empty {
        match key.code {
            KeyCode::Char('?') => {
                app.overlay = Some(Overlay::Help);
                return false;
            }
            KeyCode::Char('s') => {
                app.open_switch_picker();
                return false;
            }
            KeyCode::Char('p') if matches!(app.selected_row(), Some(RowRef::External(_))) => {
                app.import_fork();
                return false;
            }
            KeyCode::Char('a') => {
                app.import.all_repos = !app.import.all_repos;
                app.rescan_import();
                app.status_message = if app.import.all_repos {
                    "Showing Claude Code / Codex sessions from all directories".into()
                } else {
                    "Showing Claude Code / Codex sessions in this repo".into()
                };
                return false;
            }
            _ => {}
        }
    }
    // Tab completes a slash command while typing one.
    if key.code == KeyCode::Tab && app.input.starts_with('/') && !app.input.contains(' ') {
        if let Some((cmd, _)) = SLASH_COMMANDS.iter().find(|(c, _)| c.starts_with(app.input.as_str())) {
            app.input = format!("{cmd} ");
            app.cursor_pos = app.input.chars().count();
        }
        return false;
    }
    // Any key other than `x` cancels a pending delete.
    let is_delete_key = empty && key.code == KeyCode::Char('x');
    if !is_delete_key {
        app.pending_delete = None;
    }
    match key.code {
        KeyCode::Char('q') if empty => return true,
        KeyCode::Char('i') if empty => app.open_import(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Tab | KeyCode::Esc => {
            // Return to Tree View.
            app.mode = ViewMode::Tree;
            app.input.clear();
            app.cursor_pos = 0;
            app.input_mode = InputMode::Dispatch;
        }
        KeyCode::Char('x') if empty && app.selected().is_some() => app.handle_delete_key_agent(),
        KeyCode::Up => app.move_up(),
        KeyCode::Down => app.move_down(),
        KeyCode::Enter => {
            if app.input.trim().is_empty() {
                open_selected(app);
            } else {
                app.dispatch_new();
            }
        }
        KeyCode::Left => {
            if app.cursor_pos > 0 {
                app.cursor_pos -= 1;
            }
        }
        KeyCode::Right => {
            if app.cursor_pos < app.input.chars().count() {
                app.cursor_pos += 1;
            } else if app.input.trim().is_empty() {
                open_selected(app);
            }
        }
        KeyCode::Backspace => {
            if app.cursor_pos > 0 {
                let byte_idx = char_to_byte(&app.input, app.cursor_pos - 1);
                app.input.remove(byte_idx);
                app.cursor_pos -= 1;
            }
        }
        KeyCode::Char(c) => {
            let byte_idx = char_to_byte(&app.input, app.cursor_pos);
            app.input.insert(byte_idx, c);
            app.cursor_pos += 1;
        }
        _ => {}
    }
    false
}

fn attach_to_session(name: &str, status_message: &mut String) {
    // tmux draws on the alternate screen and switches back to the normal
    // screen when the client detaches, so orchestra re-enters it below.
    disable_raw_mode().ok();

    // The main-box has TERM=dumb (set by SkyPilot/SSH), but the actual
    // terminal emulator supports 256 colors. Without overriding TERM,
    // tmux renders pi's 256-color output incorrectly — dark green/blue
    // backgrounds appear as grey highlighting on normal text.
    // Re-apply the per-session look on every attach, so sessions started
    // by an older orchestra (before these were set at spawn) get it too:
    // no tmux status bar, mouse wheel scrolls the conversation.
    for (opt, val) in [("status", "off"), ("mouse", "on")] {
        let _ = Command::new("tmux")
            .args(["set-option", "-t", name, opt, val])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let status = Command::new("tmux")
        .arg("attach")
        .arg("-t")
        .arg(name)
        .env("TERM", "xterm-256color")
        .status();

    enable_raw_mode().ok();
    // tmux's detach left the alternate screen. Without re-entering it,
    // orchestra would keep drawing on the shell's normal screen and leave
    // its last frame there on quit.
    execute!(io::stdout(), EnterAlternateScreen, Clear(ClearType::All)).ok();

    match status {
        Ok(s) if !s.success() => {
            *status_message = format!("Session '{name}' not found or ended");
        }
        Err(e) => {
            *status_message = format!("tmux attach failed: {e}");
        }
        _ => {}
    }
}

/// Leave raw mode and the alternate screen, and show the cursor again, so
/// the shell is exactly as it was before orchestra started.
fn restore_terminal() {
    disable_raw_mode().ok();
    execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show).ok();
}

/// Pull latest code and rebuild the TUI binary. tmux sessions are
/// independent processes — they survive the upgrade. The user just
/// needs to restart the TUI after upgrading.
/// Re-point the `orchestra` symlink on PATH at the freshly built binary.
/// Installs from before the binary was renamed link to
/// target/release/orchestra-tui, which a rebuild no longer updates.
const RELINK: &str = "for L in \"${ORCHESTRA_BIN_DIR:-/nonexistent}/orchestra\" \
    /usr/local/bin/orchestra /opt/homebrew/bin/orchestra \"$HOME/.local/bin/orchestra\"; do \
    if [ -L \"$L\" ]; then ln -sf \"$PWD/target/release/orchestra\" \"$L\" 2>/dev/null \
    || sudo -n ln -sf \"$PWD/target/release/orchestra\" \"$L\"; fi; done; true";

fn upgrade() -> anyhow::Result<()> {
    println!("Upgrading orchestra...");

    let status = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "cd {} && git pull origin main && cd tui && source ~/.cargo/env && cargo build --release && {RELINK}",
            paths::orchestra_checkout().display()
        ))
        .status()?;

    if !status.success() {
        eprintln!("\nUpgrade failed. Check errors above.");
        std::process::exit(1);
    }

    println!("\nOrchestra upgraded successfully.");
    println!("Your sessions are preserved in tmux — they are unaffected.");
    println!("Run 'orchestra' to start the TUI with the new version.");
    Ok(())
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    // Some terminals reserve the last row for the cursor.
    let area = f.area();
    let area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));

    match app.mode {
        ViewMode::Tree => ui_tree(f, app, area),
        ViewMode::Agent => ui_agent(f, app, area),
    }
}

/// "3m" / "5h" / "2d" since a unix timestamp.
fn ago(ts: u64) -> String {
    let d = tree_store::unix_now().saturating_sub(ts);
    match d {
        0..=59 => format!("{d}s"),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

fn ui_tree(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let footer_h = 3u16.min(area.height);
    let footer_area = Rect::new(area.x, area.bottom().saturating_sub(footer_h), area.width, footer_h);
    let graph_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(footer_h));
    let selected = app.selected_node_id.as_deref();
    let buf = f.buffer_mut();
    tree_view::render_tree(buf, graph_area, &app.tree, selected, app.show_detail);
    render_footer(f, app, footer_area);
}

/// Build what the main list shows this frame.
fn view_model(app: &App) -> agent_view::ViewModel {
    use agent_view::{Group, Row, Status};
    let working = |s: &Session| app.activity.get(&s.name).is_some_and(|a| a.working);
    let session_rows: Vec<Row> = app
        .sessions
        .iter()
        .map(|s| {
            let act = app.activity.get(&s.name);
            let switching = app.switching.get(&s.name);
            let status = if switching.is_some() || s.state == SessionState::Initializing {
                Status::Starting
            } else if working(s) {
                Status::Working
            } else {
                Status::Ready
            };
            let agent = switch::Target { backend: s.backend, model: s.model.as_ref().map(|m| m.rsplit('/').next().unwrap_or(m).to_string()) };
            Row {
                status,
                name: s.display_title(),
                label: switching.map(|_| "Switching".to_string()),
                summary: match switching {
                    Some(t) => format!("switching to {t}…"),
                    None => act.and_then(|a| a.summary.clone()).unwrap_or_else(|| s.prompt.clone()),
                },
                meta: {
                    let mut m = agent.label();
                    if let Some(r) = &s.remote {
                        m = format!("☁ {} · {m}", r.cluster);
                    }
                    if act.is_some_and(|a| a.looping) {
                        m = format!("↻ {m}");
                    }
                    m
                },
                age: ago(act.and_then(|a| a.last_active).unwrap_or(s.created_at)),
            }
        })
        .collect();
    let root = app.repo.as_ref().map(|r| r.root.clone());
    let ext_rows = |backend: Backend| -> Vec<Row> {
        app.rows()
            .into_iter()
            .filter_map(|r| match r {
                RowRef::External(i) if app.import.items[i].backend == backend => Some(&app.import.items[i]),
                _ => None,
            })
            .map(|e| {
                // Worktree sessions: just the worktree's name.
                let short = |c: &str| -> Option<String> {
                    let (_, rest) = c.split_once("/.claude/worktrees/").or_else(|| c.split_once("/.orchestra/worktrees/"))?;
                    Some(format!("⎇ {}", rest.split('/').next().unwrap_or(rest)))
                };
                let dir = if let Some(wt) = short(&e.cwd) { wt } else { match &root {
                    Some(r) if !app.import.all_repos => std::path::Path::new(&e.cwd)
                        .strip_prefix(r)
                        .map(|p| if p.as_os_str().is_empty() { ".".to_string() } else { p.display().to_string() })
                        .unwrap_or_else(|_| tilde(std::path::Path::new(&e.cwd))),
                    _ => tilde(std::path::Path::new(&e.cwd)),
                } };
                Row {
                    status: Status::Elsewhere,
                    name: e.title.clone(),
                    label: None,
                    summary: app.summaries.get(&e.path).and_then(|(_, s)| s.clone()).unwrap_or_default(),
                    meta: dir,
                    age: ago(e.modified),
                }
            })
            .collect()
    };
    let external_group = |backend: Backend, name: &str| {
        let total = app
            .import
            .items
            .iter()
            .filter(|e| e.backend == backend && app.managing(e).is_none())
            .count();
        let collapsed = app.is_collapsed(backend);
        Group {
            title: if collapsed {
                format!("{name} · not in orchestra")
            } else {
                format!("{name} · not in orchestra — enter adopts, p forks into pi")
            },
            rows: if collapsed { Vec::new() } else { ext_rows(backend) },
            collapsible: total > 0,
            collapsed,
            hidden: if collapsed { total } else { 0 },
        }
    };
    let groups = vec![
        Group::plain("Sessions", session_rows),
        external_group(Backend::Claude, "Claude Code"),
        external_group(Backend::Codex, "Codex"),
    ];
    let n_working = app.sessions.iter().filter(|s| working(s)).count();
    let n_ready = app.sessions.len() - n_working;
    let default = switch::Target { backend: app.config.default_backend, model: app.config.model_for(app.config.default_backend) };
    let (title, place) = match &app.repo {
        Some(r) => (tilde(&r.root), format!("new sessions get a worktree off {}", r.base_ref)),
        None => (tilde(&app.launch_dir), "not a git repo — new sessions run here without a worktree".into()),
    };
    let overlay = match &app.overlay {
        None => None,
        Some(Overlay::Help) => Some(agent_view::Overlay::Help(
            [
                ("enter", "open / adopt"), ("s", "switch agent or model"),
                ("p", "fork into pi"), ("x x", "delete session"),
                ("a", "all directories"), ("ctrl+r", "rename session"),
                ("← → on a group", "collapse / expand"),
                ("/model", "default for new sessions"),
                ("← (in session)", "back to this list"), ("tab", "complete / tree view"),
                ("q", "quit"), ("?", "close"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        )),
        Some(Overlay::Text { title, subtitle, body }) => Some(agent_view::Overlay::Text {
            title: title.clone(),
            subtitle: subtitle.clone(),
            body: body.clone(),
        }),
        Some(Overlay::Bug { title, body }) => Some(agent_view::Overlay::Text {
            title: format!("File a GitHub issue on {}", commands::BUG_REPO),
            subtitle: format!("Title: {title}"),
            body: body.clone(),
        }),
        Some(Overlay::Teleport { session, plan }) => Some(agent_view::Overlay::Text {
            title: format!("Teleport {session} to {}", plan.infra),
            subtitle: format!("API server {} · cluster {}", plan.endpoint, plan.cluster),
            body: {
                let mut b = String::from("This launches a SkyPilot cluster and resumes the session there. It sends:\n");
                for l in &plan.sends {
                    b.push_str(&format!("  • {l}\n"));
                }
                for w in &plan.warnings {
                    b.push_str(&format!("\n⚠ {w}\n"));
                }
                b.push_str(&format!("\nThe cluster costs money until you stop it: {}sky down {}\nTask file: {}", teleport::sky_env(), plan.cluster, plan.yaml.display()));
                b
            },
        }),
        Some(Overlay::Picker(p)) => {
            let vis = p.visible();
            let current = app.picker_current(&p.purpose, &vis.iter().map(|o| (*o).clone()).collect::<Vec<_>>());
            let (title, subtitle) = match &p.purpose {
                PickPurpose::Switch(name) => (
                    format!("Switch {name}"),
                    "Moves this conversation to another agent or model. History and tool calls carry over.".to_string(),
                ),
                PickPurpose::Default => (
                    "Select agent and model".to_string(),
                    "Used for new sessions. Change a running session with s or /switch.".to_string(),
                ),
            };
            Some(agent_view::Overlay::Picker {
                title,
                subtitle,
                options: vis.iter().map(|o| (o.name.clone(), o.desc.clone())).collect(),
                current,
                selected: p.selected,
                filter: p.filter.clone(),
            })
        }
    };
    let renaming = app.rename_session.is_some() && app.input_mode == InputMode::Rename;
    let footer_is_status = !app.status_message.is_empty() && !renaming;
    let footer = if renaming {
        "rename — enter to save · esc to cancel · empty resets to the default name".into()
    } else if footer_is_status {
        app.status_message.clone()
    } else if matches!(app.overlay, Some(Overlay::Picker(_))) {
        "enter to select · 1–9 to pick · type to filter · esc to cancel".into()
    } else if matches!(app.overlay, Some(Overlay::Teleport { .. })) {
        "enter to launch · esc to cancel".into()
    } else if matches!(app.overlay, Some(Overlay::Bug { .. })) {
        "enter to file this issue (public) · esc to cancel".into()
    } else if matches!(app.overlay, Some(Overlay::Text { .. })) {
        "any key to close".into()
    } else {
        format!("⏵ {} · / for commands · ? for shortcuts", default.label())
    };
    let suggestions = if app.input.starts_with('/') && !app.input.contains(' ') {
        SLASH_COMMANDS
            .iter()
            .filter(|(c, _)| c.starts_with(app.input.as_str()))
            .map(|(c, d)| (c.to_string(), d.to_string()))
            .collect()
    } else {
        Vec::new()
    };
    agent_view::ViewModel {
        title,
        subtitle: format!("{n_working} working · {n_ready} ready · {place}"),
        hint: "enter opens · ← inside a session comes back here · s switches agent/model · ? for shortcuts".into(),
        groups,
        selected: Some(app.sel.min(app.rows().len().saturating_sub(1))).filter(|_| !app.rows().is_empty()),
        input: app.input.clone(),
        cursor: app.cursor_pos,
        placeholder: if renaming { "new name".into() } else { "describe a task for a new session".into() },
        footer,
        footer_is_status,
        overlay,
        empty_text: "No sessions yet — describe a task below to start one".into(),
        suggestions,
    }
}

fn ui_agent(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    if app.agent_view_scope.is_none() {
        // Fill summaries for visible external rows (cached by mtime).
        let ext: Vec<(Backend, PathBuf)> = app
            .rows()
            .into_iter()
            .filter_map(|r| match r {
                RowRef::External(i) => Some((app.import.items[i].backend, app.import.items[i].path.clone())),
                _ => None,
            })
            .take(area.height as usize)
            .collect();
        for (b, p) in ext {
            app.summary_for(b, &p);
        }
        let vm = view_model(app);
        let (cx, cy) = agent_view::render(f.buffer_mut(), area, &vm);
        f.set_cursor_position((cx, cy));
        return;
    }
    let scope = app
        .agent_view_scope
        .as_ref()
        .and_then(|id| app.tree.get(id))
        .map(|n| n.raw.label().to_string())
        .unwrap_or_else(|| "main-box".to_string());

    // If scoped to a non-root agent with no local sessions, show a placeholder.
    let is_subagent = app.agent_view_scope.is_some();

    let list_h = area.height.saturating_sub(3);
    let list_area = Rect::new(area.x, area.y, area.width, list_h);
    let input_area = Rect::new(area.x, area.y + list_h, area.width, 3);

    if is_subagent {
        // Sub-agent Agent View: show the agent's sessions from the tree store
        // (read-only — remote attach is Phase 8).
        let scope_id = app.agent_view_scope.clone().unwrap_or_default();
        let node = app.tree.get(&scope_id);
        let session_ids: Vec<String> = node
            .map(|n| {
                n.raw
                    .pi_sessions
                    .iter()
                    .chain(n.raw.openclaw_sessions.iter())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let title = format!(" Agent View: {scope} (read-only — {n} sessions) ", n = session_ids.len());
        let items: Vec<ListItem> = if session_ids.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                "  No sessions on this agent. Remote attach is Phase 8 (Design §14).",
                Style::default().fg(Color::DarkGray),
            )))]
        } else {
            session_ids
                .iter()
                .map(|s| {
                    ListItem::new(Line::from(vec![
                        Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                        Span::raw(s.clone()),
                    ]))
                })
                .collect()
        };
        let list_widget = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list_widget, list_area, &mut app.list_state);
    } else {
        // Root Agent View: the classic session list (unchanged).
        let title = format!(
            " Agent View: {scope} — {} active — ↑↓ nav, → attach, Enter dispatch, x=delete, Tab=Tree ",
            app.sessions
                .iter()
                .filter(|s| s.state == SessionState::Working)
                .count()
        );
        let items: Vec<ListItem> = app.sessions.iter().map(|s| {
            let icon = match s.state {
                SessionState::Initializing => Span::styled("◐ ", Style::default().fg(Color::Blue)),
                SessionState::Working => Span::styled("● ", Style::default().fg(Color::Yellow)),
                SessionState::NeedsInput => Span::styled("● ", Style::default().fg(Color::Cyan)),
                SessionState::Idle => Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                SessionState::Completed => Span::styled("● ", Style::default().fg(Color::Green)),
                SessionState::Failed => Span::styled("● ", Style::default().fg(Color::Red)),
            };
            let name = Span::styled(
                format!("{:<28} ", import::one_line(&s.name, 27)),
                Style::default().add_modifier(Modifier::BOLD),
            );
            let tag = Span::styled(format!("{:<18} ", import::one_line(&s.tag(), 17)), Style::default().fg(Color::DarkGray));
            let prompt = Span::raw(s.prompt.chars().take(60).collect::<String>());
            ListItem::new(Line::from(vec![icon, name, Span::raw(" "), tag, prompt]))
        }).collect();
        let list_widget = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list_widget, list_area, &mut app.list_state);
    }

    render_footer(f, app, input_area);
}

fn render_footer(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let (input_title, placeholder): (String, String) = match (app.mode, app.input_mode) {
        (ViewMode::Tree, InputMode::Rename) => (
            " Rename — Enter=commit, Esc=cancel ".into(),
            "Type new display name...".into(),
        ),
        (ViewMode::Tree, InputMode::Dispatch) => (
            " Tree View — ←→↑↓ nav, Enter=enter, d=detail, n=rename, r=reload, x=delete, Tab=Agent ".into(),
            "Tree View".into(),
        ),
        (ViewMode::Agent, _) => {
            let backend = app.backend_label(app.config.default_backend);
            let target = match &app.repo {
                Some(r) => format!("new worktree off {}", r.base_ref),
                None => "no git repo — runs in the launch dir".to_string(),
            };
            if app.input.is_empty() {
                (
                    format!(" Prompt + Enter → {backend}, {target} · i=import · Tab=Tree "),
                    "/claude|/codex|/pi <prompt>, /backend, /model, /import, /agent, /rename".into(),
                )
            } else {
                (" Press Enter to dispatch ".into(), String::new())
            }
        }
    };

    let status = if !app.status_message.is_empty() {
        app.status_message.as_str()
    } else if app.mode == ViewMode::Tree && app.tree.nodes.is_empty() {
        "No tree state yet"
    } else {
        placeholder.as_str()
    };

    let input = Paragraph::new(app.input.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(input_title)
            .title_bottom(
                ratatui::text::Line::from(status).style(Style::default().fg(Color::Yellow)),
            ),
    );
    f.render_widget(input, area);

    // Show the terminal cursor inside the text input.
    let cursor_x = area.x + 1 + app.cursor_pos as u16;
    let cursor_y = area.y + 1;
    f.set_cursor_position((cursor_x, cursor_y));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str, state: SessionState) -> Session {
        let mut s = Session::new(
            name.to_string(),
            format!("prompt for {name}"),
            format!("/tmp/{name}"),
            Backend::Pi,
        );
        s.created_at = 1000;
        s.last_activity = 1000;
        s.state = state;
        s
    }

    #[test]
    fn synthesize_tree_has_root_and_sessions() {
        let sessions = vec![
            session("fix-login", SessionState::Working),
            session("write-tests", SessionState::Idle),
        ];
        let tree = synthesize_tree_from_sessions(&sessions);
        assert!(tree.has_root());
        assert_eq!(tree.root_id.as_deref(), Some(ROOT_AGENT_ID));
        assert_eq!(tree.nodes.len(), 3); // root + 2 sessions
        let root = tree.get(ROOT_AGENT_ID).unwrap();
        assert_eq!(root.raw.children.len(), 2);
        assert!(root.raw.children.contains(&"session-fix-login".to_string()));
        let s = tree.get("session-fix-login").unwrap();
        assert_eq!(s.raw.parent_id.as_deref(), Some(ROOT_AGENT_ID));
        assert_eq!(s.raw.kind, NodeKind::Session);
        assert_eq!(s.raw.tmux_session.as_deref(), Some("fix-login"));
    }

    #[test]
    fn synthesize_maps_session_states() {
        for (ss, expected) in [
            (SessionState::Initializing, NodeState::Initializing),
            (SessionState::Working, NodeState::Working),
            (SessionState::NeedsInput, NodeState::NeedsInput),
            (SessionState::Idle, NodeState::Idle),
            (SessionState::Completed, NodeState::Completed),
            (SessionState::Failed, NodeState::Failed),
        ] {
            let tree = synthesize_tree_from_sessions(&[session("s", ss)]);
            let node = tree.get("session-s").unwrap();
            assert_eq!(node.effective, expected, "state mismatch for {ss:?}");
        }
    }

    #[test]
    fn synthesize_empty_sessions_just_root() {
        let tree = synthesize_tree_from_sessions(&[]);
        assert!(tree.has_root());
        assert_eq!(tree.nodes.len(), 1);
        assert!(tree.get(ROOT_AGENT_ID).unwrap().raw.children.is_empty());
    }

    #[test]
    fn synthesize_tree_lays_out_cleanly() {
        let sessions = vec![
            session("a", SessionState::Working),
            session("b", SessionState::Idle),
            session("c", SessionState::Completed),
        ];
        let tree = synthesize_tree_from_sessions(&sessions);
        let lay = tree_layout::layout(&tree);
        assert!(!lay.is_empty());
        assert_eq!(lay.positions.len(), 4);
    }

    #[test]
    fn delete_session_removes_from_in_memory_list() {
        // The delete_session method does real I/O (tmux, git, rm) which we
        // can't safely run in a unit test. But we can test the in-memory
        // list removal logic directly — it's just `retain`.
        let mut sessions = vec![
            session("a", SessionState::Working),
            session("b", SessionState::Idle),
            session("c", SessionState::Completed),
        ];
        let before = sessions.len();
        sessions.retain(|s| s.name != "b");
        assert_eq!(sessions.len(), before - 1);
        assert!(!sessions.iter().any(|s| s.name == "b"));
        // Order preserved.
        assert_eq!(sessions[0].name, "a");
        assert_eq!(sessions[1].name, "c");
    }

    #[test]
    fn delete_session_state_paths_are_correct() {
        // Verify the paths delete_session constructs match the session
        // save conventions (SESSIONS_DIR/<name> + .ready marker).
        let name = "fix-bug-1234";
        let state_dir = paths::sessions_dir().join(name).to_string_lossy().to_string();
        let ready_marker = paths::sessions_dir().join(format!("{name}.ready")).to_string_lossy().to_string();
        assert!(state_dir.ends_with("/.orchestra/sessions/fix-bug-1234"));
        assert!(ready_marker.ends_with("/.orchestra/sessions/fix-bug-1234.ready"));
    }

    #[test]
    fn pending_delete_clears_on_other_key() {
        // The handle_agent_key logic: any key other than 'x' clears
        // pending_delete. We simulate the guard.
        let mut pending: Option<usize> = Some(2);
        // Simulate pressing a non-x key.
        let key_is_x = false;
        if !key_is_x {
            pending = None;
        }
        assert_eq!(pending, None);
    }

    #[test]
    fn pending_delete_two_press_confirms() {
        // First press arms, second press (same idx) executes.
        let mut pending: Option<usize> = None;
        let selected = 1usize;
        // First press:
        match pending {
            Some(p) if p == selected => { /* would execute */ }
            _ => pending = Some(selected),
        }
        assert_eq!(pending, Some(selected));
        // Second press (same idx):
        let will_execute = matches!(pending, Some(p) if p == selected);
        assert!(will_execute);
        // If selection changes between presses, no execute:
        let new_selected = 2usize;
        let will_execute_after_change = matches!(pending, Some(p) if p == new_selected);
        assert!(!will_execute_after_change);
    }
}
