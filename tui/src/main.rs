// orchestra-tui — Terminal UI for managing parallel pi coding agent sessions.
//
// Keybindings:
//   Up/Down     — navigate session list
//   Enter       — dispatch new session (if input non-empty) or attach to selected
//   Right       — attach to selected session
//   Left        — detach from session (inside tmux)
//   q/Ctrl+C    — quit

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

mod session;
mod worktree;

use session::{Session, SessionState};

const WORK_REPO_PATH: &str = "/home/sky/work-repos/prototype";
const SESSIONS_DIR: &str = "/home/sky/.orchestra/sessions";
const WORKTREES_DIR: &str = "/home/sky/orchestra/worktrees";

struct App {
    sessions: Vec<Session>,
    list_state: ListState,
    input: String,
    status_message: String,
    needs_clear: bool,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            sessions: session::load_sessions(),
            list_state: ListState::default(),
            input: String::new(),
            status_message: String::new(),
            needs_clear: false,
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

        let name = session::generate_name(&prompt);
        let worktree_path = format!("{WORKTREES_DIR}/{name}");

        match worktree::create_worktree(WORK_REPO_PATH, &worktree_path, &name) {
            Ok(_) => {}
            Err(e) => {
                self.status_message = format!("Worktree failed: {e}");
                return;
            }
        }

        match session::spawn_pi(&name, &worktree_path, &prompt) {
            Ok(()) => {
                let sess = Session::new(name.clone(), prompt.clone(), worktree_path);
                self.sessions.push(sess);
                self.input.clear();
                self.status_message = format!("Dispatched: {name}");
                self.list_state.select(Some(self.sessions.len() - 1));
            }
            Err(e) => {
                self.status_message = format!("pi spawn failed: {e}");
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
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
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    session::save_sessions(&app.sessions);
    Ok(())
}

fn handle_key(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Up => app.move_up(),
        KeyCode::Down => app.move_down(),
        KeyCode::Enter => {
            if app.input.trim().is_empty() {
                if let Some(idx) = app.selected() {
                    if app.sessions[idx].state == SessionState::Initializing {
                        app.status_message = format!("{} is still initializing...", app.sessions[idx].name);
                    } else {
                        let (name, path) = (
                            app.sessions[idx].name.clone(),
                            app.sessions[idx].worktree_path.clone(),
                        );
                        attach_to_session(&name, &path, &mut app.status_message);
                        app.needs_clear = true;
                    }
                }
            } else {
                app.dispatch_new();
            }
        }
        KeyCode::Right => {
            if let Some(idx) = app.selected() {
                if app.sessions[idx].state == SessionState::Initializing {
                    app.status_message = format!("{} is still initializing...", app.sessions[idx].name);
                } else {
                    let (name, path) = (
                        app.sessions[idx].name.clone(),
                        app.sessions[idx].worktree_path.clone(),
                    );
                    attach_to_session(&name, &path, &mut app.status_message);
                    app.needs_clear = true;
                }
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

fn attach_to_session(name: &str, _worktree_path: &str, status_message: &mut String) {
    disable_raw_mode().ok();
    execute!(io::stdout(), LeaveAlternateScreen).ok();

    let status = Command::new("tmux").arg("attach").arg("-t").arg(name).status();

    enable_raw_mode().ok();
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

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    // Some terminals reserve the last row for the cursor, so the effective
    // renderable height is area.height - 1.
    let area = f.area();
    let area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    let list_h = area.height.saturating_sub(3);
    let list_area = Rect::new(area.x, area.y, area.width, list_h);
    let input_area = Rect::new(area.x, area.y + list_h, area.width, 3);

    // Session list
    let title = format!(
        " Sessions ({} active) — ↑↓ navigate, → attach, Enter dispatch ",
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

    // Dispatch input with status in the footer
    let input_title = if app.input.is_empty() {
        " Type a prompt + Enter to dispatch new session "
    } else {
        " Press Enter to dispatch "
    };
    let status = if !app.status_message.is_empty() {
        app.status_message.as_str()
    } else if app.sessions.is_empty() {
        "No sessions yet — type a prompt above"
    } else {
        ""
    };
    let input = Paragraph::new(app.input.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(input_title)
                .title_bottom(ratatui::text::Line::from(status).style(Style::default().fg(Color::Yellow))),
        );
    f.render_widget(input, input_area);
}
