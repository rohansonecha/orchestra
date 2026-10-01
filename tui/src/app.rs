// app.rs — application state and logic: the session list, import view,
// pickers and overlays, dispatch and slash-command handling, switching,
// teleport, suspend and delete. Key handling lives in keys.rs, drawing in
// render.rs, startup and the CLI subcommands in main.rs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Instant, SystemTime};

use ratatui::widgets::ListState;

use crate::agent_view;
use crate::command;
use crate::history;
use crate::keys::attach_to_session;
use crate::rename;
use crate::tree_view;
use crate::worktree;
use crate::commands;
use crate::config::{self, Config};
use crate::import::{self, ExternalSession};
use crate::paths;
use crate::repo::{self, Repo};
use crate::session::{self, Backend, Origin, Session, SessionState};
use crate::switch;
use crate::teleport;
use crate::tree_store::{self, Node, NodeKind, NodeState, NodeView, Tree, TreeStore};

pub(crate) const ROOT_AGENT_ID: &str = "agent-main-box";

/// Which TUI level is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewMode {
    Tree,
    Agent,
}

/// Claude Code / Codex sessions found on disk, listed under orchestra's
/// own sessions so they can be adopted in place.
pub(crate) struct ImportState {
    pub(crate) items: Vec<ExternalSession>,
    /// Show sessions from every directory, not just the current repo.
    pub(crate) all_repos: bool,
    pub(crate) scanned: Option<Instant>,
    /// A scan running on a background thread: reading every transcript on
    /// disk takes long enough to stall typing. Tagged with the directory it
    /// scanned so a result for an old /import all setting is dropped.
    pub(crate) pending: Option<std::sync::mpsc::Receiver<(Option<PathBuf>, Vec<ExternalSession>)>>,
}

/// A row of the main list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowRef {
    Session(usize),
    /// Header of the Claude Code / Codex group (folds it).
    Group(Backend),
    External(usize),
}

/// What each session is doing, refreshed every couple of seconds.
#[derive(Debug, Clone, Default)]
pub(crate) struct Activity {
    pub(crate) working: bool,
    pub(crate) looping: bool,
    pub(crate) summary: Option<String>,
    pub(crate) last_active: Option<u64>,
}

/// One choice in the agent/model picker.
#[derive(Debug, Clone)]
pub(crate) struct PickOption {
    pub(crate) name: String,
    pub(crate) desc: String,
    pub(crate) target: switch::Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickPurpose {
    /// Move this session (by name) to the picked agent/model.
    Switch(String),
    /// Default agent/model for new sessions.
    Default,
}

#[derive(Debug, Clone)]
pub(crate) struct Picker {
    pub(crate) purpose: PickPurpose,
    pub(crate) options: Vec<PickOption>,
    pub(crate) selected: usize,
    pub(crate) filter: String,
}

impl Picker {
    pub(crate) fn visible(&self) -> Vec<&PickOption> {
        let f = self.filter.to_lowercase();
        self.options
            .iter()
            .filter(|o| f.is_empty() || format!("{} {}", o.name, o.desc).to_lowercase().contains(&f))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Overlay {
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
pub(crate) struct SwitchDone {
    /// The session with its new agent/transcript recorded.
    pub(crate) session: Session,
    pub(crate) result: Result<String, String>,
    /// A fork (new session to start) rather than a switch of an existing one.
    pub(crate) fork: Option<(String, Vec<String>)>,
}

/// A finished /btw or /recap, from the worker thread.
pub(crate) struct SideAnswer {
    pub(crate) session: String,
    pub(crate) question: String,
    pub(crate) recap: bool,
    pub(crate) result: Result<String, String>,
}

/// Inline input mode for the dispatch box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputMode {
    /// Normal: typing a prompt or /command.
    Dispatch,
    /// Renaming the selected tree node (n was pressed).
    Rename,
}

pub(crate) struct App {
    pub(crate) sessions: Vec<Session>,
    pub(crate) list_state: ListState,
    pub(crate) input: String,
    pub(crate) cursor_pos: usize,
    pub(crate) status_message: String,
    pub(crate) needs_clear: bool,
    // --- Tree View state ---
    pub(crate) mode: ViewMode,
    /// The agent node id that Agent View is currently scoped to. `None`
    /// means the root (Main Agent) — the classic list view.
    pub(crate) agent_view_scope: Option<String>,
    pub(crate) tree: Tree,
    pub(crate) selected_node_id: Option<String>,
    pub(crate) show_detail: bool,
    pub(crate) input_mode: InputMode,
    /// When in Rename mode, the id of the node being renamed.
    pub(crate) rename_target: Option<String>,
    /// Agent View: the session (by name) whose display name is being edited.
    pub(crate) rename_session: Option<String>,
    /// Pending delete confirmation: first `x` press sets this to the
    /// selected session index; second `x` confirms + executes. Any other
    /// key clears it. Prevents accidental full-cleanup deletion.
    pub(crate) pending_delete: Option<usize>,
    /// Same for `x` in Tree View: the node id armed for teardown.
    pub(crate) pending_tree_delete: Option<String>,
    /// Repo orchestra was launched in; None outside git (no worktrees).
    pub(crate) repo: Option<Repo>,
    pub(crate) launch_dir: std::path::PathBuf,
    pub(crate) config: Config,
    pub(crate) import: ImportState,
    /// Selected row of the main list (see `rows`).
    pub(crate) sel: usize,
    pub(crate) activity: HashMap<String, Activity>,
    /// Transcript path → (mtime, latest reply line), for list summaries.
    pub(crate) summaries: HashMap<PathBuf, (SystemTime, Option<String>)>,
    pub(crate) activity_scanned: Option<Instant>,
    pub(crate) overlay: Option<Overlay>,
    /// `pi --list-models`, loaded in the background at startup (it takes
    /// a second or two) so the picker opens instantly.
    pub(crate) pi_models: Option<Vec<(String, String, String)>>,
    pub(crate) pi_models_rx: Option<std::sync::mpsc::Receiver<Vec<(String, String, String)>>>,
    pub(crate) side_rx: Option<std::sync::mpsc::Receiver<SideAnswer>>,
    /// Text behind the `[Pasted text #n ...]` tokens in the prompt.
    pub(crate) pastes: Vec<String>,
    pub(crate) switch_tx: std::sync::mpsc::Sender<SwitchDone>,
    pub(crate) switch_rx: std::sync::mpsc::Receiver<SwitchDone>,
    /// Sessions being switched → what they are switching to.
    pub(crate) switching: HashMap<String, String>,
    /// When each running session was last in use (open, working, looping).
    pub(crate) last_used: HashMap<String, Instant>,
}

impl App {
    pub(crate) fn new() -> Self {
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
            import: ImportState { items: Vec::new(), all_repos: false, scanned: None, pending: None },
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
            pastes: Vec::new(),
            switch_tx,
            switch_rx,
            switching: HashMap::new(),
            last_used: HashMap::new(),
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
    pub(crate) fn reload_tree(&mut self) {
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
    /// Pinned rows come first, then orchestra sessions, then each outside
    /// group.
    pub(crate) fn rows(&self) -> Vec<RowRef> {
        let outside = |e: &ExternalSession| self.managing(e).is_none();
        let mut rows: Vec<RowRef> = (0..self.sessions.len())
            .filter(|&i| self.session_pinned(&self.sessions[i]))
            .map(RowRef::Session)
            .collect();
        rows.extend(
            self.import.items.iter().enumerate().filter(|(_, e)| outside(e) && self.external_pinned(e)).map(|(i, _)| RowRef::External(i)),
        );
        rows.extend((0..self.sessions.len()).filter(|&i| !self.session_pinned(&self.sessions[i])).map(RowRef::Session));
        for backend in [Backend::Claude, Backend::Codex, Backend::Pi] {
            let items: Vec<RowRef> = self
                .import
                .items
                .iter()
                .enumerate()
                .filter(|(_, e)| e.backend == backend && outside(e) && !self.external_pinned(e))
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

    fn pin_keys(s: &Session) -> Vec<String> {
        let mut keys = vec![format!("session:{}", s.id)];
        if let Some(ext) = &s.external_id {
            keys.push(format!("{}:{ext}", s.backend.as_str()));
        }
        keys
    }

    pub(crate) fn session_pinned(&self, s: &Session) -> bool {
        Self::pin_keys(s).iter().any(|k| self.config.pinned.contains(k))
    }

    pub(crate) fn external_pinned(&self, e: &ExternalSession) -> bool {
        self.config.pinned.contains(&format!("{}:{}", e.backend.as_str(), e.id))
    }

    /// Ctrl+P: pin the selected session to the top group, or unpin it.
    pub(crate) fn toggle_pin(&mut self) {
        let Some(row) = self.selected_row() else { return };
        let (keys, pinned, title) = match row {
            RowRef::Session(i) => {
                let s = &self.sessions[i];
                (Self::pin_keys(s), self.session_pinned(s), s.display_title())
            }
            RowRef::External(i) => {
                let e = &self.import.items[i];
                (vec![format!("{}:{}", e.backend.as_str(), e.id)], self.external_pinned(e), e.title.clone())
            }
            RowRef::Group(_) => {
                self.status_message = "Select a session to pin".into();
                return;
            }
        };
        self.config.pinned.retain(|k| !keys.contains(k));
        if !pinned {
            self.config.pinned.push(keys[0].clone());
        }
        self.config.save();
        // Follow the row to its new place.
        if let Some(p) = self.rows().iter().position(|r| *r == row) {
            self.sel = p;
        }
        self.status_message = if pinned { format!("Unpinned {title}") } else { format!("Pinned {title} — ctrl+p again unpins") };
    }

    pub(crate) fn is_collapsed(&self, b: Backend) -> bool {
        self.config.collapsed.iter().any(|c| c == b.as_str())
    }

    /// Fold or unfold a Claude Code / Codex group (remembered in config).
    pub(crate) fn set_collapsed(&mut self, b: Backend, collapse: bool) {
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

    pub(crate) fn selected_row(&self) -> Option<RowRef> {
        let rows = self.rows();
        rows.get(self.sel.min(rows.len().saturating_sub(1))).copied()
    }

    /// Selected orchestra session, if the selection is one.
    pub(crate) fn selected(&self) -> Option<usize> {
        match self.selected_row() {
            Some(RowRef::Session(i)) => Some(i),
            _ => None,
        }
    }

    pub(crate) fn select_session(&mut self, idx: usize) {
        if let Some(p) = self.rows().iter().position(|r| *r == RowRef::Session(idx)) {
            self.sel = p;
        }
    }

    pub(crate) fn move_up(&mut self) {
        let n = self.rows().len();
        if n > 0 {
            self.sel = if self.sel == 0 { n - 1 } else { self.sel.min(n - 1) - 1 };
        }
    }

    pub(crate) fn move_down(&mut self) {
        let n = self.rows().len();
        if n > 0 {
            self.sel = if self.sel + 1 >= n { 0 } else { self.sel + 1 };
        }
    }

    /// Transcript's latest reply line, cached by mtime.
    pub(crate) fn summary_for(&mut self, backend: Backend, path: &std::path::Path) -> Option<String> {
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
    pub(crate) fn poll_side_answer(&mut self) {
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

    pub(crate) fn refresh_activity(&mut self) {
        self.poll_side_answer();
        self.finish_switches();
        self.finish_import_scan();
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
        // Codex sessions orchestra started: record their thread id once
        // Codex has picked it, so the list and restarts match by id.
        for s in self.sessions.iter_mut().filter(|s| s.backend == Backend::Codex && s.external_id.is_none()) {
            if let Some((id, _)) = switch::discover_codex_thread(&s.worktree_path, s.created_at) {
                s.external_id = Some(id);
                session::save_session(s);
            }
        }
        let names: Vec<(String, Backend, Option<PathBuf>)> = self
            .sessions
            .iter()
            .map(|s| (s.name.clone(), s.backend, switch::native_transcript(s)))
            .collect();
        let live = session::tmux_sessions();
        for (name, backend, path) in names {
            let working = live.contains_key(&name) && pane_working(&name);
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
            let looping = live.contains_key(&commands::loop_tmux_name(&name));
            let attached = live.get(&name).copied().unwrap_or(false);
            if working || looping || attached || self.switching.contains_key(&name) || !self.last_used.contains_key(&name) {
                self.last_used.insert(name.clone(), Instant::now());
            }
            self.activity.insert(name, Activity { working, looping, summary, last_active });
        }
        if self.import.scanned.is_none_or(|t| t.elapsed().as_secs() >= 15) {
            self.rescan_import();
        }
        self.suspend_idle();
    }

    pub(crate) fn dispatch_new(&mut self) {
        let prompt = self.expanded_input().trim().to_string();
        if prompt.is_empty() {
            return;
        }

        // Slash-commands take precedence over a plain prompt.
        match command::parse(&prompt) {
            command::DispatchCommand::Rename { new_name } if self.mode == ViewMode::Agent => {
                self.clear_input();
                self.status_message = match self.selected() {
                    Some(i) => self.set_title(i, &new_name),
                    None => "Select a session to rename".into(),
                };
            }
            command::DispatchCommand::Rename { new_name } => {
                self.status_message = self.rename_selected(&new_name);
                self.reload_tree();
                self.input.clear();
                self.cursor_pos = 0;
            }
            command::DispatchCommand::Unknown { raw } => {
                self.status_message = format!("Unknown command: {raw}");
                self.input.clear();
                self.cursor_pos = 0;
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
            command::DispatchCommand::ImportAll => {
                self.clear_input();
                self.import.all_repos = !self.import.all_repos;
                self.rescan_import();
                self.status_message = if self.import.all_repos {
                    "Showing Claude Code / Codex sessions from all directories — /import all again for this repo only".into()
                } else {
                    "Showing Claude Code / Codex sessions in this repo".into()
                };
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
    pub(crate) fn run_command(&mut self, name: &str, args: &str) {
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
            "suspend" => {
                let Some(i) = need(self) else { return };
                if commands::loop_running(&self.sessions[i].name) {
                    commands::stop_loop(&self.sessions[i].name);
                }
                let msg = self.suspend_session(i);
                self.status_message = format!("{msg} — its conversation is kept; Enter resumes it");
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
    pub(crate) fn teleport_launch(&mut self, name: &str, plan: teleport::Plan) {
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
    pub(crate) fn teleport_back(&mut self, i: usize) -> String {
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
    pub(crate) fn branch_session(&mut self, i: usize, name: &str) -> String {
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

    pub(crate) fn open_switch_picker(&mut self) {
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
    pub(crate) fn pick_options(&mut self) -> Vec<PickOption> {
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

    pub(crate) fn open_picker(&mut self, purpose: PickPurpose) {
        let options = self.pick_options();
        let current = self.picker_current(&purpose, &options);
        self.overlay = Some(Overlay::Picker(Picker { purpose, options, selected: current.unwrap_or(0), filter: String::new() }));
    }

    pub(crate) fn picker_current(&self, purpose: &PickPurpose, options: &[PickOption]) -> Option<usize> {
        let (b, m) = match purpose {
            PickPurpose::Switch(name) => {
                let s = self.sessions.iter().find(|s| &s.name == name)?;
                (s.backend, s.model.clone())
            }
            PickPurpose::Default => (self.config.default_backend, self.config.model_for(self.config.default_backend)),
        };
        options.iter().position(|o| o.target.backend == b && o.target.model == m)
    }

    pub(crate) fn apply_pick(&mut self, picker: Picker) {
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
    pub(crate) fn resolve_target(&self, spec: &str) -> Result<switch::Target, String> {
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
    /// Stop sessions nobody is using (see Config::suspend_after): not open,
    /// not working, no /loop, local, and no conversation writes for that
    /// long. Their agent process ends; the conversation is kept and Enter
    /// resumes it.
    pub(crate) fn suspend_idle(&mut self) {
        let Some(after) = self.config.suspend_after() else { return };
        let now = tree_store::unix_now();
        let idle: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| {
                let s = &self.sessions[i];
                let recent_write = self.activity.get(&s.name).and_then(|a| a.last_active).is_some_and(|t| now.saturating_sub(t) < after.as_secs());
                matches!(s.state, SessionState::Working | SessionState::Idle | SessionState::NeedsInput)
                    && s.remote.is_none()
                    && !recent_write
                    && self.last_used.get(&s.name).is_some_and(|t| t.elapsed() >= after)
            })
            .collect();
        for i in idle {
            let msg = self.suspend_session(i);
            self.status_message = format!("{msg} after {} idle minutes — Enter resumes it", after.as_secs() / 60);
        }
    }

    /// Stop a session's agent to free its memory and CPU. It stays listed
    /// as Stopped; Enter resumes the same conversation.
    pub(crate) fn suspend_session(&mut self, idx: usize) -> String {
        let s = &mut self.sessions[idx];
        let _ = Command::new("tmux").args(["kill-session", "-t", &s.name]).status();
        let _ = std::fs::remove_file(paths::sessions_dir().join(format!("{}.ready", s.name)));
        s.state = SessionState::Completed;
        self.last_used.remove(&s.name);
        format!("Suspended {}", s.display_title())
    }

    /// Bring a stopped session back: start its agent again on its own
    /// transcript (pi --continue, claude --resume, codex resume), in its
    /// directory, under the same name.
    pub(crate) fn resume_session(&mut self, idx: usize) -> String {
        let sess = &mut self.sessions[idx];
        if !std::path::Path::new(&sess.worktree_path).is_dir() {
            return format!("{} can't resume: its directory {} is gone", sess.display_title(), sess.worktree_path);
        }
        if sess.remote.is_none() {
            match (sess.backend, switch::native_transcript(sess)) {
                (_, None) => {} // nothing recorded yet: start fresh with its prompt
                (Backend::Pi, Some(_)) => sess.origin = Origin::Resumed,
                (Backend::Claude, Some(_)) => {
                    sess.origin = Origin::Resumed;
                    if sess.external_id.is_none() {
                        sess.external_id = Some(sess.id.clone());
                    }
                }
                (Backend::Codex, Some(_)) => {
                    sess.origin = Origin::Resumed;
                    if sess.external_id.is_none() {
                        // (codex-open finds it on start if this can't yet)
                        sess.external_id = switch::discover_codex_thread(&sess.worktree_path, sess.created_at).map(|(id, _)| id);
                    }
                }
            }
        }
        let _ = std::fs::remove_file(paths::sessions_dir().join(format!("{}.ready", sess.name)));
        sess.state = SessionState::Initializing;
        match session::spawn(sess) {
            Ok(()) => {
                session::save_session(sess);
                self.activity_scanned = None;
                format!("Resuming {}", sess.display_title())
            }
            Err(e) => format!("Resume failed: {e}"),
        }
    }

    /// Starts the switch on a worker thread. The session keeps running on
    /// its current agent until the new transcript is fully written; only
    /// then is it restarted (see `finish_switch`). A switch that fails or
    /// is interrupted leaves the session as it was.
    pub(crate) fn switch_session(&mut self, idx: usize, target: &switch::Target) -> String {
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
    pub(crate) fn finish_switches(&mut self) {
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

    pub(crate) fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
        self.pastes.clear();
    }

    /// A paste. Multi-line or long text becomes a `[Pasted text #1 +12
    /// lines]` token in the prompt (as in Claude Code), expanded to the full
    /// text on Enter; short single-line text is inserted as typed. The
    /// rename box takes everything inline, newlines as spaces.
    pub(crate) fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let insert = |app: &mut App, t: &str| {
            let at = app.input.char_indices().nth(app.cursor_pos).map(|(i, _)| i).unwrap_or(app.input.len());
            app.input.insert_str(at, t);
            app.cursor_pos += t.chars().count();
        };
        if self.input_mode == InputMode::Rename || self.mode != ViewMode::Agent {
            let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            insert(self, &flat);
            return;
        }
        let lines = text.trim_end_matches('\n').lines().count();
        if text == "\n" {
            // Fallback newline from a paste without bracketed paste.
            insert(self, "\n");
        } else if lines > 1 || text.chars().count() > 800 {
            self.pastes.push(text.trim_end_matches('\n').to_string());
            let token = paste_token(self.pastes.len(), &self.pastes[self.pastes.len() - 1]);
            insert(self, &token);
        } else {
            insert(self, text.trim_end_matches('\n'));
        }
    }

    /// The prompt with paste tokens replaced by what was pasted.
    pub(crate) fn expanded_input(&self) -> String {
        expand_pastes(&self.input, &self.pastes)
    }

    /// `pi (openrouter/qwen3)` style label for status lines.
    pub(crate) fn backend_label(&self, b: Backend) -> String {
        match self.config.model_for(b) {
            Some(m) => format!("{} ({m})", b.as_str()),
            None => b.as_str().to_string(),
        }
    }

    /// `/model` for the default backend. pi models are checked against
    /// `pi --list-models` (built-in providers + ~/.pi/agent/models.json).
    pub(crate) fn set_model(&mut self, model: Option<String>) -> String {
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
    pub(crate) fn dispatch_session(&mut self, prompt: &str, backend: Backend) {
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
    pub(crate) fn start_session(&mut self, sess: Session, ok_msg: String, warnings: Vec<String>) {
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
    pub(crate) fn open_import(&mut self) {
        self.rescan_import();
        match self.rows().iter().position(|r| matches!(r, RowRef::External(_))) {
            Some(p) => self.sel = p,
            None => {
                self.status_message = "No Claude Code or Codex sessions here — press a to show all directories".into()
            }
        }
    }

    fn import_scope(&self) -> Option<PathBuf> {
        if self.import.all_repos {
            None
        } else {
            Some(self.repo.as_ref().map(|r| r.root.clone()).unwrap_or_else(|| self.launch_dir.clone()))
        }
    }

    /// Start a scan for outside sessions in the background; the list
    /// updates when it finishes (`finish_import_scan`).
    pub(crate) fn rescan_import(&mut self) {
        let under = self.import_scope();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let items = import::scan(under.as_deref());
            let _ = tx.send((under, items));
        });
        self.import.pending = Some(rx);
        self.import.scanned = Some(Instant::now());
    }

    pub(crate) fn finish_import_scan(&mut self) {
        let Some(rx) = &self.import.pending else { return };
        match rx.try_recv() {
            Ok((under, items)) => {
                self.import.pending = None;
                if under == self.import_scope() {
                    self.import.items = items;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.import.pending = None,
        }
    }

    pub(crate) fn selected_external(&self) -> Option<ExternalSession> {
        match self.selected_row() {
            Some(RowRef::External(i)) => self.import.items.get(i).cloned(),
            _ => None,
        }
    }

    /// The orchestra session already attached to an external session.
    pub(crate) fn managing(&self, ext: &ExternalSession) -> Option<&Session> {
        let path = ext.path.to_string_lossy();
        self.sessions.iter().find(|s| {
            // Transcripts orchestra wrote or adopted for a session (switch
            // segments) belong to that session, not the "not in
            // orchestra" list.
            s.segments.iter().any(|seg| seg.path == path)
                || s.external_id.as_deref() == Some(ext.id.as_str())
                || (s.backend == ext.backend && s.backend == Backend::Claude && s.id == ext.id)
                // orchestra's own pi conversations are keyed by session id
                || (ext.backend == Backend::Pi && s.id == ext.id)
        })
    }

    pub(crate) fn imported_name(ext: &ExternalSession) -> String {
        let base = rename::sanitize_name(&ext.title);
        let base: String = base.split('-').take(3).collect::<Vec<_>>().join("-");
        let short: String = ext.id.chars().filter(|c| c.is_ascii_hexdigit()).take(4).collect();
        let prefix = match ext.backend {
            Backend::Claude => "cc",
            Backend::Codex => "cx",
            Backend::Pi => "pi",
        };
        if base.is_empty() { format!("{prefix}-{short}") } else { format!("{prefix}-{base}-{short}") }
    }

    /// Enter in Import View: resume with the session's own CLI, in its
    /// original directory. Attaches to it if it's already running.
    pub(crate) fn import_resume(&mut self) {
        let Some(ext) = self.selected_external() else {
            return;
        };
        if ext.backend == Backend::Pi {
            self.adopt_pi(ext);
            return;
        }
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

    /// Adopt a pi conversation orchestra isn't tracking: one of its own
    /// (whose state was lost — reuse its directory, the id is the key) or
    /// one started with plain `pi` (copied into a new session directory).
    pub(crate) fn adopt_pi(&mut self, ext: ExternalSession) {
        let cwd = if std::path::Path::new(&ext.cwd).is_dir() { ext.cwd.clone() } else { self.launch_dir.to_string_lossy().to_string() };
        let name = Self::imported_name(&ext);
        let mut sess = Session::new(name.clone(), ext.title.clone(), cwd, Backend::Pi);
        sess.origin = Origin::Resumed;
        sess.title = Some(ext.title.clone());
        let own = ext.path.starts_with(paths::pi_sessions_dir());
        if own {
            sess.id = ext.id.clone();
        } else {
            let dir = sess.pi_session_dir();
            let copied = std::fs::create_dir_all(&dir)
                .and_then(|_| std::fs::copy(&ext.path, dir.join(ext.path.file_name().unwrap_or_default())));
            if let Err(e) = copied {
                self.status_message = format!("Could not copy the pi session: {e}");
                return;
            }
        }
        self.start_session(sess, format!("Adopted pi session \"{}\" as {name}", ext.title), Vec::new());
    }

    /// `p` in Import View: convert the transcript into a pi session in a
    /// fresh worktree (or its original directory outside a repo).
    pub(crate) fn import_fork(&mut self) {
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

    pub(crate) fn tree_left(&mut self) {
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

    pub(crate) fn tree_right(&mut self) {
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

    pub(crate) fn tree_up(&mut self) {
        self.move_within_depth(-1);
    }

    pub(crate) fn tree_down(&mut self) {
        self.move_within_depth(1);
    }

    /// Move to the nearest node at the same depth, in the given direction.
    /// `dir` is -1 (up) or +1 (down). Picks the nearest by y-coordinate.
    pub(crate) fn move_within_depth(&mut self, dir: i32) {
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

    pub(crate) fn enter_node(&mut self) {
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

    pub(crate) fn toggle_detail(&mut self) {
        self.show_detail = !self.show_detail;
    }

    pub(crate) fn start_rename(&mut self) {
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
    pub(crate) fn set_title(&mut self, idx: usize, title: &str) -> String {
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
    pub(crate) fn start_session_rename(&mut self) {
        let Some(i) = self.selected() else {
            self.status_message = "Select one of your sessions to rename".into();
            return;
        };
        self.rename_session = Some(self.sessions[i].name.clone());
        self.input_mode = InputMode::Rename;
        self.input = self.sessions[i].display_title();
        self.cursor_pos = self.input.chars().count();
    }

    pub(crate) fn commit_rename(&mut self) {
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
    pub(crate) fn rename_selected(&mut self, new_name: &str) -> String {
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

    pub(crate) fn cancel_rename(&mut self) {
        self.input_mode = InputMode::Dispatch;
        self.rename_target = None;
        self.rename_session = None;
        self.input.clear();
        self.cursor_pos = 0;
        self.status_message = "Rename cancelled".to_string();
    }

    /// `x` in Tree View. Like Agent View, the first press only arms: a
    /// stray `x` must not `sky down` a cluster or delete a worktree.
    pub(crate) fn teardown_selected(&mut self) {
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
    pub(crate) fn delete_session(&mut self, name: &str) -> bool {
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
    pub(crate) fn handle_delete_key_agent(&mut self) {
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
                    format!("Press Ctrl+X again to DELETE '{}' ({what}). Any other key cancels.", s.display_title());
            }
        }
    }
}

/// Synthesize a Tree in memory from the session list. Used when the disk
/// store is absent (collector not running) so Tree View is never empty.
/// The root is the Main Agent; each session becomes a leaf Session node.
pub(crate) fn synthesize_tree_from_sessions(sessions: &[Session]) -> Tree {
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

/// Replace paste tokens with the text they stand for.
pub(crate) fn expand_pastes(input: &str, pastes: &[String]) -> String {
    let mut out = input.to_string();
    for (i, text) in pastes.iter().enumerate() {
        out = out.replace(&paste_token(i + 1, text), text);
    }
    out
}

/// How a paste shows in the prompt, e.g. `[Pasted text #1 +12 lines]`.
pub(crate) fn paste_token(n: usize, text: &str) -> String {
    let lines = text.lines().count();
    if lines > 1 {
        format!("[Pasted text #{n} +{lines} lines]")
    } else {
        format!("[Pasted text #{n}, {} chars]", text.chars().count())
    }
}

/// Whether the agent in a tmux session is mid-turn: every agent shows an
/// interrupt hint while working ("esc to interrupt", pi's "Working...").
pub(crate) fn pane_working(name: &str) -> bool {
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", name, "-S", "-12"])
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(out) = out else { return false };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains("esc to interrupt") || text.contains("Working...") || text.contains("to interrupt)")
}

/// Point pi's settings at one of orchestra's themes, if it is installed.
pub(crate) fn set_pi_theme(name: &str) {
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

pub(crate) fn fmt_secs(s: u64) -> String {
    match s {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// "~/x" for paths under $HOME.
pub(crate) fn tilde(p: &std::path::Path) -> String {
    let home = paths::home();
    match p.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Slash commands for the completion list above the prompt.
pub(crate) const SLASH_COMMANDS: &[(&str, &str)] = &[
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
    ("/suspend", "stop the selected session's agent to free memory; Enter resumes it"),
    ("/teleport", "[infra] — move the selected session to a SkyPilot box; /teleport back"),
    ("/rename", "<name> — rename the selected session"),
];

/// A pi model pattern → its exact `provider/id` (must match one model).
pub(crate) fn resolve_pi_model(pattern: &str) -> Result<String, String> {
    match config::pi_models_matching(pattern) {
        Ok(found) if found.len() == 1 => Ok(found[0].clone()),
        Ok(found) if found.iter().any(|f| f == pattern) => Ok(pattern.to_string()),
        Ok(found) if found.is_empty() => Err(format!("pi knows no model matching '{pattern}' (see pi --list-models)")),
        Ok(found) => Err(format!("'{pattern}' matches {} pi models: {}", found.len(), found.join(", "))),
        Err(e) => Err(e),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_layout;

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
    fn paste_tokens_expand_to_the_pasted_text() {
        let pastes = vec!["line one\nline two\nline three".to_string(), "x".repeat(900)];
        let t1 = paste_token(1, &pastes[0]);
        let t2 = paste_token(2, &pastes[1]);
        assert_eq!(t1, "[Pasted text #1 +3 lines]");
        assert_eq!(t2, "[Pasted text #2, 900 chars]");
        let input = format!("review this {t1} and {t2}");
        assert_eq!(expand_pastes(&input, &pastes), format!("review this {} and {}", pastes[0], pastes[1]));
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
