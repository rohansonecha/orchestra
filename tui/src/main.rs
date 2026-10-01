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

use std::io;
use std::process::Command;
use std::time::Instant;

use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

mod agent_view;
mod app;
mod command;
mod commands;
mod config;
mod coverage;
mod history;
mod import;
mod keys;
mod line_edit;
mod paths;
mod render;
mod rename;
mod repo;
mod session;
mod switch;
mod teleport;
mod tree_layout;
mod tree_store;
mod tree_view;
mod worktree;

use app::{pane_working, App, InputMode, ViewMode};
use session::Session;
use keys::handle_key;
use render::ui;


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

/// `orchestra codex-open <session> [codex args...]` — what the pane runs to
/// reopen a Codex session orchestra started: resume its own thread by id
/// (recording it the first time), never "the latest in this directory".
fn codex_open_cli() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let args: Vec<String> = std::env::args().skip(2).collect();
    let Some((name, extra)) = args.split_first() else {
        eprintln!("usage: orchestra codex-open <session> [codex args...]");
        std::process::exit(2);
    };
    let state = paths::sessions_dir().join(name).join("state.json");
    let mut sess: Option<Session> = std::fs::read_to_string(&state).ok().and_then(|c| serde_json::from_str(&c).ok());
    let id = sess.as_ref().and_then(|s| s.external_id.clone()).or_else(|| {
        let s = sess.as_mut()?;
        let (id, _) = switch::discover_codex_thread(&s.worktree_path, s.created_at)?;
        s.external_id = Some(id.clone());
        session::save_session(s);
        Some(id)
    });
    let err = match id {
        Some(id) => Command::new("codex").arg("resume").arg(&id).args(extra).exec(),
        None => {
            eprintln!("[orchestra] Codex has not recorded this session yet; starting it again.");
            Command::new("codex").args(extra).exec()
        }
    };
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
            "codex-open" => return codex_open_cli(),
            "roi" => {
                coverage::run();
                return Ok(());
            }
            "tmux-setup" => {
                session::tmux_setup_now();
                println!("orchestra tmux settings applied");
                return Ok(());
            }
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
    // Bracketed paste: a paste arrives as one event instead of keystrokes,
    // so its newlines don't each submit the prompt.
    execute!(stdout, EnterAlternateScreen, event::EnableBracketedPaste)?;
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
                let ev = event::read()?;
                if let Event::Paste(text) = &ev {
                    app.paste(text);
                }
                if let Event::Key(key) = ev {
                    // Without bracketed paste, a pasted newline arrives as
                    // Enter in the same burst as more text: keep it as a
                    // newline instead of submitting each line.
                    let more = key.code == KeyCode::Enter
                        && key.modifiers.is_empty()
                        && app.mode == ViewMode::Agent
                        && app.input_mode == InputMode::Dispatch
                        && !app.input.is_empty()
                        && event::poll(std::time::Duration::ZERO)?;
                    if more {
                        app.paste("\n");
                    } else if key.kind != event::KeyEventKind::Release && handle_key(&mut app, key) {
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



/// Leave raw mode and the alternate screen, and show the cursor again, so
/// the shell is exactly as it was before orchestra started.
fn restore_terminal() {
    disable_raw_mode().ok();
    execute!(io::stdout(), event::DisableBracketedPaste, LeaveAlternateScreen, crossterm::cursor::Show).ok();
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

