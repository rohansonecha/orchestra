// orchestra-tui — Terminal UI for managing parallel pi coding agent sessions.
//
// Two-level TUI (Design_Document.md §9):
//   - Tree View (default): spatial graph of the agent/session hierarchy.
//   - Agent View: the classic session list + dispatch input, scoped to one
//     agent (the Main Agent for now; sub-agents are read-only).
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
// Keybindings — Agent View (unchanged from the classic list TUI):
//   Up/Down     — navigate session list
//   Enter       — dispatch new session (if input non-empty) or attach
//   Right       — move cursor right / attach to session (empty input)
//   Left        — move cursor left (detach inside tmux only at column 0)
//   Alt+Left    — jump to previous word
//   Alt+Right   — jump to next word
//   Alt+Delete  — delete previous word
//   Tab/Esc     — return to Tree View
//   q/Ctrl+C    — quit
//
// The TUI is a pure reader of the tree store at ~/.orchestra/tree/. If the
// store is absent (collector not running), it synthesizes a tree in memory
// from the session list so Tree View is never empty on first run.

use std::collections::HashMap;
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

mod command;
mod session;
mod tree_layout;
mod tree_store;
mod tree_view;
mod worktree;

use session::{Session, SessionState};
use tree_store::{Node, NodeKind, NodeState, NodeView, Tree, TreeStore};

const WORK_REPO_PATH: &str = "/home/sky/work-repos/prototype";
const SESSIONS_DIR: &str = "/home/sky/.orchestra/sessions";
const WORKTREES_DIR: &str = "/home/sky/orchestra/worktrees";
const ORCHESTRA_DIR: &str = "/home/sky/orchestra";
const ROOT_AGENT_ID: &str = "agent-main-box";

/// Which TUI level is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Tree,
    Agent,
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
}

impl App {
    fn new() -> Self {
        let sessions = session::load_sessions();
        let mut app = Self {
            sessions,
            list_state: ListState::default(),
            input: String::new(),
            cursor_pos: 0,
            status_message: String::new(),
            needs_clear: false,
            mode: ViewMode::Tree,
            agent_view_scope: None,
            tree: Tree::default(),
            selected_node_id: None,
            show_detail: false,
            input_mode: InputMode::Dispatch,
            rename_target: None,
        };
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

    fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    fn move_up(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(0) | None => self.sessions.len() - 1,
            Some(i) => i - 1,
        };
        self.list_state.select(Some(i));
    }

    fn move_down(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) if i >= self.sessions.len() - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        };
        self.list_state.select(Some(i));
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
            command::DispatchCommand::Rename { new_name } => {
                if let Some(id) = &self.selected_node_id {
                    rename_node(id, &new_name);
                    self.status_message = format!("Renamed to '{new_name}'");
                    self.reload_tree();
                } else {
                    self.status_message = "No node selected to rename".to_string();
                }
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
            command::DispatchCommand::PlainPrompt { text } => {
                self.dispatch_session(&text);
            }
        }
    }

    fn dispatch_session(&mut self, prompt: &str) {
        let name = session::generate_name(prompt);
        let worktree_path = format!("{WORKTREES_DIR}/{name}");

        match worktree::create_worktree(WORK_REPO_PATH, &worktree_path, &name) {
            Ok(_) => {}
            Err(e) => {
                self.status_message = format!("Worktree failed: {e}");
                return;
            }
        }

        match session::spawn_pi(&name, &worktree_path, prompt) {
            Ok(()) => {
                let sess = Session::new(name.clone(), prompt.to_string(), worktree_path);
                session::save_session(&sess);
                self.sessions.push(sess);
                self.input.clear();
                self.cursor_pos = 0;
                self.status_message = format!("Dispatched: {name}");
                self.list_state.select(Some(self.sessions.len() - 1));
                // Refresh the tree so the new session appears.
                self.reload_tree();
            }
            Err(e) => {
                self.status_message = format!("pi spawn failed: {e}");
            }
        }
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

    fn commit_rename(&mut self) {
        let new_name = self.input.trim().to_string();
        if new_name.is_empty() {
            self.status_message = "Rename cancelled (empty name)".to_string();
        } else if let Some(id) = self.rename_target.take() {
            rename_node(&id, &new_name);
            self.status_message = format!("Renamed to '{new_name}'");
            self.reload_tree();
        }
        self.input_mode = InputMode::Dispatch;
        self.input.clear();
        self.cursor_pos = 0;
    }

    fn cancel_rename(&mut self) {
        self.input_mode = InputMode::Dispatch;
        self.rename_target = None;
        self.input.clear();
        self.cursor_pos = 0;
        self.status_message = "Rename cancelled".to_string();
    }

    fn teardown_selected(&mut self) {
        let Some(id) = self.selected_node_id.clone() else {
            return;
        };
        let Some(node) = self.tree.get(&id) else {
            return;
        };
        match node.raw.kind {
            NodeKind::Agent => {
                let cluster = node.raw.sky_cluster.clone().unwrap_or_else(|| id.clone());
                self.status_message = format!("Tearing down agent '{cluster}' (sky down)...");
                let status = Command::new("sky")
                    .args(["down", "-y", &cluster])
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
                let tmux_name = node
                    .raw
                    .tmux_session
                    .clone()
                    .unwrap_or_else(|| id.clone());
                let status = Command::new("tmux")
                    .args(["kill-session", "-t", &tmux_name])
                    .status();
                match status {
                    Ok(s) if s.success() => {
                        self.status_message = format!("Session '{tmux_name}' killed");
                    }
                    _ => {
                        self.status_message = format!("tmux kill-session failed for '{tmux_name}'");
                    }
                }
                self.reload_tree();
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
            display_name: None,
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

/// Write a `display_name` to a node's JSON file directly. This is a TUI-side
/// rename that the collector must preserve on its next pull (it reads the
/// existing node and keeps `display_name` while updating state/sessions).
/// If the store is absent (in-memory tree), the rename is a no-op — it
/// only persists once the collector has written the store.
fn rename_node(id: &str, new_name: &str) {
    let store = TreeStore::default_dir();
    let path = store.dir().join("nodes").join(format!("{id}.json"));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return; // store absent — nothing to persist
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return;
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("display_name".to_string(), serde_json::json!(new_name));
    }
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap_or_default());
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
            _ => {}
        }
    }

    std::fs::create_dir_all(SESSIONS_DIR).ok();
    std::fs::create_dir_all(WORKTREES_DIR).ok();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    loop {
        if app.needs_clear {
            terminal.clear()?;
            app.needs_clear = false;
        }
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if handle_key(&mut app, key) {
                    break;
                }
            }
        }

        for sess in &mut app.sessions {
            sess.refresh_state();
        }
        // Refresh the tree every poll cycle. Cheap: a few small JSON files,
        // or the in-memory synthesis. Keeps the tree live as the collector
        // writes new state.
        app.reload_tree();
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
        KeyCode::Char(c) => {
            let byte_idx = char_to_byte(&app.input, app.cursor_pos);
            app.input.insert(byte_idx, c);
            app.cursor_pos += 1;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.cancel_rename();
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

fn handle_agent_key(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Tab | KeyCode::Esc => {
            // Return to Tree View.
            app.mode = ViewMode::Tree;
            app.input.clear();
            app.cursor_pos = 0;
            app.input_mode = InputMode::Dispatch;
        }
        KeyCode::Up => app.move_up(),
        KeyCode::Down => app.move_down(),
        KeyCode::Enter => {
            if app.input.trim().is_empty() {
                if let Some(idx) = app.selected() {
                    if app.sessions[idx].state == SessionState::Initializing {
                        app.status_message =
                            format!("{} is still initializing...", app.sessions[idx].name);
                    } else {
                        let (name, path) = (
                            app.sessions[idx].name.clone(),
                            app.sessions[idx].worktree_path.clone(),
                        );
                        attach_to_session(&name, &mut app.status_message);
                        app.needs_clear = true;
                    }
                }
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
                if let Some(idx) = app.selected() {
                    if app.sessions[idx].state == SessionState::Initializing {
                        app.status_message =
                            format!("{} is still initializing...", app.sessions[idx].name);
                    } else {
                        let (name, path) = (
                            app.sessions[idx].name.clone(),
                            app.sessions[idx].worktree_path.clone(),
                        );
                        attach_to_session(&name, &mut app.status_message);
                        app.needs_clear = true;
                    }
                }
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
    // Don't leave the alternate screen — tmux handles its own screen
    // management. Leaving/entering the alternate screen causes a flash
    // of the normal terminal buffer between transitions.
    disable_raw_mode().ok();

    // The main-box has TERM=dumb (set by SkyPilot/SSH), but the actual
    // terminal emulator supports 256 colors. Without overriding TERM,
    // tmux renders pi's 256-color output incorrectly — dark green/blue
    // backgrounds appear as grey highlighting on normal text.
    let status = Command::new("tmux")
        .arg("attach")
        .arg("-t")
        .arg(name)
        .env("TERM", "xterm-256color")
        .status();

    enable_raw_mode().ok();
    // Force a full redraw — tmux corrupted our screen buffer.
    execute!(io::stdout(), Clear(ClearType::All)).ok();

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

/// Pull latest code and rebuild the TUI binary. tmux sessions are
/// independent processes — they survive the upgrade. The user just
/// needs to restart the TUI after upgrading.
fn upgrade() -> anyhow::Result<()> {
    println!("Upgrading orchestra...");

    let status = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "cd {ORCHESTRA_DIR} && git pull origin main && cd tui && source ~/.cargo/env && cargo build --release"
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

fn ui_tree(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let footer_h = 3u16.min(area.height);
    let footer_area = Rect::new(area.x, area.bottom().saturating_sub(footer_h), area.width, footer_h);
    let graph_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(footer_h));
    let selected = app.selected_node_id.as_deref();
    let buf = f.buffer_mut();
    tree_view::render_tree(buf, graph_area, &app.tree, selected, app.show_detail);
    render_footer(f, app, footer_area);
}

fn ui_agent(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
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
            " Agent View: {scope} — {} active — ↑↓ navigate, → attach, Enter dispatch, Tab=Tree ",
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
                format!("{:<24}", s.name),
                Style::default().add_modifier(Modifier::BOLD),
            );
            let prompt = Span::raw(s.prompt.chars().take(60).collect::<String>());
            ListItem::new(Line::from(vec![icon, name, Span::raw(" "), prompt]))
        }).collect();
        let list_widget = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list_widget, list_area, &mut app.list_state);
    }

    render_footer(f, app, input_area);
}

fn render_footer(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let (input_title, placeholder) = match (app.mode, app.input_mode) {
        (ViewMode::Tree, InputMode::Rename) => (
            " Rename — Enter=commit, Esc=cancel ",
            "Type new display name...",
        ),
        (ViewMode::Tree, InputMode::Dispatch) => (
            " Tree View — ←→↑↓ nav, Enter=enter, d=detail, n=rename, r=reload, x=teardown, Tab=Agent ",
            "Tree View",
        ),
        (ViewMode::Agent, _) => {
            if app.input.is_empty() {
                (
                    " Type a prompt + Enter to dispatch, /agent <name> to spawn, Tab=Tree ",
                    "Type a prompt or /agent <name>",
                )
            } else {
                (" Press Enter to dispatch ", "")
            }
        }
    };

    let status = if !app.status_message.is_empty() {
        app.status_message.as_str()
    } else if app.mode == ViewMode::Tree && app.tree.nodes.is_empty() {
        "No tree state yet"
    } else {
        placeholder
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
        Session {
            name: name.to_string(),
            prompt: format!("prompt for {name}"),
            worktree_path: format!("/tmp/{name}"),
            created_at: 1000,
            last_activity: 1000,
            state,
        }
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
}
