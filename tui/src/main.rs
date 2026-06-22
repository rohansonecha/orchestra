// orchestra-tui — Terminal UI for managing parallel pi coding agent sessions.
//
// Modes:
//   - List mode: shows all sessions, dispatch input at bottom
//   - Attach mode: full-screen pi session (spawned in a worktree)
//
// Keybindings:
//   Up/Down     — navigate session list
//   Enter       — dispatch new session with current input (in list mode)
//   Right/Enter — attach to selected session
//   Left        — detach back to list (when in attach mode, or on empty input)
//   q/Ctrl+C    — quit

use std::io::{self, Stdout};
use std::process::{Child, Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;

mod session;
mod worktree;

use session::{Session, SessionState};

const WORK_REPO_PATH: &str = "/home/sky/work-repos/prototype";
const SESSIONS_DIR: &str = "/home/sky/.orchestra/sessions";
const WORKTREES_DIR: &str = "/home/sky/orchestra/worktrees";

enum Mode {
    List,
    Attach,
}

struct App {
    sessions: Vec<Session>,
    list_state: ListState,
    input: String,
    mode: Mode,
    attached_index: Option<usize>,
    status_message: String,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            sessions: session::load_sessions(),
            list_state: ListState::default(),
            input: String::new(),
            mode: Mode::List,
            attached_index: None,
            status_message: String::new(),
        };
        if !app.sessions.is_empty() {
            app.list_state.select(Some(0));
        }
        app
    }

    fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    fn move_up(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.sessions.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn move_down(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i >= self.sessions.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn dispatch_new(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }

        // Generate session name from prompt (first few words) or timestamp.
        let name = session::generate_name(&prompt);
        let worktree_path = format!("{WORKTREES_DIR}/{name}");

        // Create worktree off latest master.
        match worktree::create_worktree(WORK_REPO_PATH, &worktree_path, &name) {
            Ok(_) => {}
            Err(e) => {
                self.status_message = format!("Worktree creation failed: {e}");
                return;
            }
        }

        // Spawn pi in the worktree.
        match session::spawn_pi(&name, &worktree_path, &prompt) {
            Ok(child) => {
                let sess = Session::new(name.clone(), prompt.clone(), worktree_path, child);
                self.sessions.push(sess);
                self.input.clear();
                self.status_message = format!("Dispatched session: {name}");
                // Select the new session.
                self.list_state.select(Some(self.sessions.len() - 1));
            }
            Err(e) => {
                self.status_message = format!("Failed to spawn pi: {e}");
            }
        }
    }

    fn attach_selected(&mut self) -> Option<(String, String)> {
        let idx = self.selected()?;
        let sess = &self.sessions[idx];
        // Suspend TUI, let the caller spawn an interactive pi attach.
        self.mode = Mode::Attach;
        self.attached_index = Some(idx);
        Some((sess.name.clone(), sess.worktree_path.clone()))
    }

    fn detach(&mut self) {
        self.mode = Mode::List;
        self.attached_index = None;
    }
}

fn main() -> anyhow::Result<()> {
    // Ensure dirs exist.
    std::fs::create_dir_all(SESSIONS_DIR).ok();
    std::fs::create_dir_all(WORKTREES_DIR).ok();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    loop {
        terminal.draw(|f| ui(f, &app))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match app.mode {
                    Mode::List => {
                        if handle_list_key(&mut app, key) {
                            break;
                        }
                    }
                    Mode::Attach => {
                        // In attach mode, we've already suspended the TUI.
                        // This shouldn't be reached — we exit the TUI before
                        // spawning pi, then re-enter on return.
                        if key.code == KeyCode::Left {
                            app.detach();
                        }
                    }
                }
            }
        }

        // Check for state updates from sessions (non-blocking).
        for sess in &mut app.sessions {
            sess.refresh_state();
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    // Save sessions on exit.
    session::save_sessions(&app.sessions);
    Ok(())
}

fn handle_list_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Up => app.move_up(),
        KeyCode::Down => app.move_down(),
        KeyCode::Enter => {
            if app.input.trim().is_empty() {
                // No input — attach to selected session.
                if let Some((name, worktree_path)) = app.attach_selected() {
                    attach_to_session(&name, &worktree_path, &mut app.status_message);
                    app.detach();
                }
            } else {
                app.dispatch_new();
            }
        }
        KeyCode::Right => {
            if let Some((name, worktree_path)) = app.attach_selected() {
                attach_to_session(&name, &worktree_path, &mut app.status_message);
                app.detach();
            }
        }
        KeyCode::Left => {
            if app.input.is_empty() {
                app.detach();
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(c) => {
            app.input.push(c);
        }
        _ => {}
    }
    false
}

/// Suspend the TUI, run pi interactively in the session's worktree, then resume.
fn attach_to_session(name: &str, worktree_path: &str, status_message: &mut String) {
    drop(suspend_tui());
    let env = session::load_env();
    let status = Command::new("pi")
        .arg("--name")
        .arg(name)
        .arg("--provider")
        .arg("glm")
        .arg("--model")
        .arg("zai-org/GLM-5.2-FP8")
        .current_dir(worktree_path)
        .envs(&env)
        .status();
    resume_tui();
    if let Err(e) = status {
        *status_message = format!("pi attach failed: {e}");
    }
}

fn suspend_tui() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

fn resume_tui() {
    enable_raw_mode().ok();
    execute!(io::stdout(), EnterAlternateScreen).ok();
}

fn ui(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3), Constraint::Length(1)])
        .split(f.size());

    // Session list
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            let state_icon = match s.state {
                SessionState::Working => Span::styled("✽ ", Style::default().fg(Color::Yellow)),
                SessionState::NeedsInput => Span::styled("✻ ", Style::default().fg(Color::Cyan)),
                SessionState::Idle => Span::styled("· ", Style::default().fg(Color::DarkGray)),
                SessionState::Completed => Span::styled("✓ ", Style::default().fg(Color::Green)),
                SessionState::Failed => Span::styled("✗ ", Style::default().fg(Color::Red)),
            };
            let name = Span::styled(
                format!("{:<20} ", s.name),
                Style::default().add_modifier(Modifier::BOLD),
            );
            let prompt = Span::raw(s.prompt.chars().take(60).collect::<String>());
            ListItem::new(Line::from(vec![state_icon, name, prompt]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Sessions (↑↓ navigate, → attach, Enter dispatch)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    f.render_stateful_widget(list, chunks[0], &mut app.list_state.clone());

    // Dispatch input
    let input_title = if app.input.is_empty() {
        "Dispatch new session (type prompt + Enter) — or press Enter to attach to selected"
    } else {
        "Dispatch new session (Enter to create)"
    };
    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(input_title))
        .wrap(Wrap { trim: false });
    f.render_widget(input, chunks[1]);

    // Status bar
    let status = if !app.status_message.is_empty() {
        app.status_message.as_str()
    } else if app.sessions.is_empty() {
        "No sessions. Type a prompt above and press Enter to start."
    } else {
        ""
    };
    let status_bar = Paragraph::new(status).style(Style::default().fg(Color::Yellow));
    f.render_widget(status_bar, chunks[2]);
}
