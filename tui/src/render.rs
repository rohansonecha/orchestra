// render.rs — drawing: the Agent View list, Tree View, overlays and the
// footer, plus view_model(), which turns App state into what gets drawn.

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::agent_view;
use crate::app::*;
use crate::commands;
use crate::import::{self, ExternalSession};
use crate::teleport;
use crate::switch;
use crate::tree_store;
use crate::tree_view;
use crate::session::{Backend, Session, SessionState};

pub(crate) fn ui(f: &mut ratatui::Frame, app: &mut App) {
    // Some terminals reserve the last row for the cursor.
    let area = f.area();
    let area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));

    match app.mode {
        ViewMode::Tree => ui_tree(f, app, area),
        ViewMode::Agent => ui_agent(f, app, area),
    }
}

/// "3m" / "5h" / "2d" since a unix timestamp.
pub(crate) fn ago(ts: u64) -> String {
    let d = tree_store::unix_now().saturating_sub(ts);
    match d {
        0..=59 => format!("{d}s"),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

pub(crate) fn ui_tree(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let footer_h = 3u16.min(area.height);
    let footer_area = Rect::new(area.x, area.bottom().saturating_sub(footer_h), area.width, footer_h);
    let graph_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(footer_h));
    let selected = app.selected_node_id.as_deref();
    let buf = f.buffer_mut();
    tree_view::render_tree(buf, graph_area, &app.tree, selected, app.show_detail);
    render_footer(f, app, footer_area);
}

/// Build what the main list shows this frame.
pub(crate) fn view_model(app: &App) -> agent_view::ViewModel {
    use agent_view::{Group, Row, Status};
    let working = |s: &Session| app.activity.get(&s.name).is_some_and(|a| a.working);
    let session_row = |s: &Session| -> Row {
            let act = app.activity.get(&s.name);
            let switching = app.switching.get(&s.name);
            let status = if s.state == SessionState::Completed || s.state == SessionState::Failed {
                Status::Stopped
            } else if switching.is_some() || s.state == SessionState::Initializing {
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
                    None if status == Status::Stopped => "enter resumes the conversation".to_string(),
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
    };
    let root = app.repo.as_ref().map(|r| r.root.clone());
    let ext_row = |e: &ExternalSession, pinned: bool| -> Row {
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
                    // In the pinned group, say which agent it belongs to.
                    meta: if pinned { format!("{} · {dir}", backend_name(e.backend)) } else { dir },
                    age: ago(e.modified),
                }
    };
    // Place each row in its group, in the order rows() gives (selection
    // indexes follow it).
    let (mut pinned_rows, mut session_rows) = (Vec::new(), Vec::new());
    let mut ext_rows: HashMap<Backend, Vec<Row>> = HashMap::new();
    for r in app.rows() {
        match r {
            RowRef::Session(i) => {
                let s = &app.sessions[i];
                if app.session_pinned(s) { pinned_rows.push(session_row(s)) } else { session_rows.push(session_row(s)) }
            }
            RowRef::External(i) => {
                let e = &app.import.items[i];
                if app.external_pinned(e) {
                    pinned_rows.push(ext_row(e, true));
                } else {
                    ext_rows.entry(e.backend).or_default().push(ext_row(e, false));
                }
            }
            RowRef::Group(_) => {}
        }
    }
    let mut external_group = |backend: Backend, name: &str| {
        let total = app
            .import
            .items
            .iter()
            .filter(|e| e.backend == backend && app.managing(e).is_none() && !app.external_pinned(e))
            .count();
        let collapsed = app.is_collapsed(GroupKey::Outside(backend));
        Group {
            title: if collapsed {
                format!("{name} · not in orchestra")
            } else {
                if backend == Backend::Pi {
                    format!("{name} · not in orchestra — enter adopts")
                } else {
                    format!("{name} · not in orchestra — enter adopts, ctrl+f forks into pi")
                }
            },
            rows: if collapsed { Vec::new() } else { ext_rows.remove(&backend).unwrap_or_default() },
            collapsible: total > 0,
            collapsed,
            hidden: if collapsed { total } else { 0 },
        }
    };
    // Counted here because rows() leaves out a folded group's rows.
    let n_pinned = app.sessions.iter().filter(|s| app.session_pinned(s)).count()
        + app.import.items.iter().filter(|e| app.managing(e).is_none() && app.external_pinned(e)).count();
    let n_unpinned = app.sessions.len() - app.sessions.iter().filter(|s| app.session_pinned(s)).count();
    let own_group = |key: GroupKey, title: &str, rows: Vec<Row>, total: usize| {
        let collapsed = app.is_collapsed(key);
        Group {
            title: title.to_string(),
            rows: if collapsed { Vec::new() } else { rows },
            collapsible: total > 0,
            collapsed,
            hidden: if collapsed { total } else { 0 },
        }
    };
    let groups = vec![
        own_group(GroupKey::Pinned, "Pinned", pinned_rows, n_pinned),
        own_group(GroupKey::Sessions, "Sessions", session_rows, n_unpinned),
        external_group(Backend::Claude, "Claude Code"),
        external_group(Backend::Codex, "Codex"),
        external_group(Backend::Pi, "pi"),
    ];
    let n_working = app.sessions.iter().filter(|s| working(s)).count();
    let n_stopped = app.sessions.iter().filter(|s| matches!(s.state, SessionState::Completed | SessionState::Failed)).count();
    let n_ready = app.sessions.len() - n_working - n_stopped;
    let stopped = if n_stopped > 0 { format!(" · {n_stopped} stopped") } else { String::new() };
    let default = switch::Target { backend: app.config.default_backend, model: app.config.model_for(app.config.default_backend) };
    let (title, place) = match &app.repo {
        Some(r) => (tilde(&r.root), format!("new sessions get a worktree off {}", r.base_ref)),
        None => (tilde(&app.launch_dir), "not a git repo — new sessions run here without a worktree".into()),
    };
    let overlay = match &app.overlay {
        None => None,
        Some(Overlay::Help) => Some(agent_view::Overlay::Help(
            [
                ("enter", "open / adopt"), ("ctrl+s", "switch agent or model"),
                ("ctrl+f", "fork into pi"), ("ctrl+x ×2", "delete session"),
                ("/import all", "all directories"), ("ctrl+r", "rename session"),
                ("ctrl+p", "pin / unpin"),
                ("← → on a group", "collapse / expand"),
                ("/model", "default for new sessions"),
                ("← (in session)", "back to this list"), ("tab", "complete / tree view"),
                ("esc", "clear the prompt"), ("ctrl+c", "quit"),
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
        subtitle: format!("{n_working} working · {n_ready} ready{stopped} · {place}"),
        hint: "enter opens · ← inside a session comes back here · ctrl+s switches agent/model · ? for shortcuts".into(),
        groups,
        selected: (!app.rows().is_empty()).then_some(app.sel.min(app.rows().len().saturating_sub(1))),
        input: app.input.replace('\n', "↵"),
        cursor: app.cursor_pos,
        placeholder: if renaming { "new name".into() } else { "describe a task for a new session".into() },
        footer,
        footer_is_status,
        overlay,
        empty_text: "No sessions yet — describe a task below to start one".into(),
        suggestions,
    }
}

pub(crate) fn ui_agent(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
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

pub(crate) fn render_footer(f: &mut ratatui::Frame, app: &App, area: Rect) {
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

fn backend_name(b: Backend) -> &'static str {
    match b {
        Backend::Claude => "Claude Code",
        Backend::Codex => "Codex",
        Backend::Pi => "pi",
    }
}
