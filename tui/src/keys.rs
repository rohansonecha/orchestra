// keys.rs — key handling for every view (Agent View, Tree View, Import)
// and input mode (dispatch, rename), plus tmux attach.

use std::io;
use std::process::Command;

use crossterm::event::{self, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, disable_raw_mode, enable_raw_mode};

use crate::app::*;
use crate::line_edit;
use crate::commands;
use crate::session::{self, Backend, SessionState};
use crate::tree_store;

pub(crate) fn handle_key(app: &mut App, key: event::KeyEvent) -> bool {
    // Clear status message on any key press — it's transient.
    app.status_message.clear();

    // --- Rename mode captures all keys ---
    if app.input_mode == InputMode::Rename {
        return handle_rename_key(app, key);
    }


    match app.mode {
        ViewMode::Tree => handle_tree_key(app, key),
        ViewMode::Agent => handle_agent_key(app, key),
    }
}

pub(crate) fn handle_rename_key(app: &mut App, key: event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => app.commit_rename(),
        KeyCode::Esc => app.cancel_rename(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_rename(),
        _ => {
            if line_edit::apply(&mut app.input, &mut app.cursor_pos, key) == line_edit::Edit::NotHandled {
                match key.code {
                    KeyCode::Left => app.cursor_pos = app.cursor_pos.saturating_sub(1),
                    KeyCode::Right => app.cursor_pos = (app.cursor_pos + 1).min(app.input.chars().count()),
                    _ => {}
                }
            }
        }
    }
    false
}

pub(crate) fn handle_tree_key(app: &mut App, key: event::KeyEvent) -> bool {
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
pub(crate) fn handle_overlay_key(app: &mut App, key: event::KeyEvent) -> bool {
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
        KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            p.filter.push(c);
            p.selected = 0;
        }
        _ => {}
    }
    app.overlay = Some(Overlay::Picker(p));
    true
}

pub(crate) fn open_selected(app: &mut App) {
    match app.selected_row() {
        Some(RowRef::Session(idx)) if matches!(app.sessions[idx].state, SessionState::Completed | SessionState::Failed) => {
            app.status_message = app.resume_session(idx);
        }
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

pub(crate) fn handle_agent_key(app: &mut App, key: event::KeyEvent) -> bool {
    if app.agent_view_scope.is_none() && handle_overlay_key(app, key) {
        return false;
    }
    // Letters always go into the prompt (a prompt may start with any
    // letter); shortcuts use Ctrl, like Claude Code's agents view. The one
    // exception is `?` on an empty prompt.
    let empty = app.input.is_empty();
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if app.agent_view_scope.is_none() && ctrl {
        match key.code {
            KeyCode::Char('r') => {
                app.start_session_rename();
                return false;
            }
            KeyCode::Char('s') => {
                app.open_switch_picker();
                return false;
            }
            KeyCode::Char('p') => {
                app.toggle_pin();
                return false;
            }
            KeyCode::Char('f') => {
                if app.selected_external().is_some_and(|e| e.backend == Backend::Pi) {
                    app.status_message = "That is already a pi session — Enter adopts it".into();
                } else if matches!(app.selected_row(), Some(RowRef::External(_))) {
                    app.import_fork();
                } else {
                    app.status_message = "Ctrl+F forks a Claude Code or Codex session into pi — select one below".into();
                }
                return false;
            }
            KeyCode::Char('x') => {
                if app.selected().is_some() {
                    app.handle_delete_key_agent();
                } else {
                    app.status_message = "Select one of your sessions to delete".into();
                }
                return false;
            }
            _ => {}
        }
    }
    if let (true, true, Some(RowRef::Group(b))) = (app.agent_view_scope.is_none(), empty, app.selected_row()) {
        match key.code {
            KeyCode::Left => {
                app.set_collapsed(b, true);
                return false;
            }
            KeyCode::Right => {
                app.set_collapsed(b, false);
                return false;
            }
            _ => {}
        }
    }
    if app.agent_view_scope.is_none() && empty {
        if let KeyCode::Char('?') = key.code {
            app.overlay = Some(Overlay::Help);
            return false;
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
    // Any key other than Ctrl+X cancels a pending delete.
    app.pending_delete = None;
    if ctrl && key.code == KeyCode::Char('c') {
        return true;
    }
    // Backspace right after a paste token removes the whole token.
    if key.code == KeyCode::Backspace && key.modifiers.is_empty() {
        let before: String = app.input.chars().take(app.cursor_pos).collect();
        if before.ends_with(']') {
            if let Some(start) = before.rfind("[Pasted text #") {
                let token = &before[start..];
                if app.pastes.iter().enumerate().any(|(i, t)| paste_token(i + 1, t) == token) {
                    let n = token.chars().count();
                    let from = app.cursor_pos - n;
                    let a = app.input.char_indices().nth(from).map(|(i, _)| i).unwrap_or(app.input.len());
                    let b = app.input.char_indices().nth(app.cursor_pos).map(|(i, _)| i).unwrap_or(app.input.len());
                    app.input.replace_range(a..b, "");
                    app.cursor_pos = from;
                    return false;
                }
            }
        }
    }
    // Typing and line editing (Ctrl+W, Option+Delete, Ctrl+U, Ctrl+A/E, ...).
    if line_edit::apply(&mut app.input, &mut app.cursor_pos, key) != line_edit::Edit::NotHandled {
        return false;
    }
    match key.code {
        // Esc clears what you typed (it used to jump to Tree View).
        KeyCode::Esc => app.clear_input(),
        KeyCode::Tab => {
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
        _ => {}
    }
    false
}

pub(crate) fn attach_to_session(name: &str, status_message: &mut String) {
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
    session::session_look(name);
    crate::health::phase(crate::health::ATTACHED);
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
    execute!(io::stdout(), EnterAlternateScreen, event::EnableBracketedPaste, Clear(ClearType::All)).ok();

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
