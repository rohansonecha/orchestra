// agent_view.rs — The main list, styled after Claude Code's agents view:
// no boxes, a muted palette where only status words carry color, sessions
// grouped under a header, one row per session (glyph · name · status and a
// one-line summary · agent/model · age), and a prompt between two rules
// with a single line of hints under it.
//
// Rendering works from a plain `ViewModel` the app builds each frame, so
// layout can be tested without a terminal.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// Light or dark palette, set from config at startup and by /theme.
static LIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_light(light: bool) {
    LIGHT.store(light, std::sync::atomic::Ordering::Relaxed);
}

fn pick(dark: Color, light: Color) -> Color {
    if LIGHT.load(std::sync::atomic::Ordering::Relaxed) { light } else { dark }
}

// Claude Code's palettes: muted text, color only for status. Primary text
// is the terminal's own foreground so it reads on any background.
fn grey() -> Color { pick(Color::Indexed(246), Color::Rgb(102, 102, 102)) }
fn dim() -> Color { pick(Color::Indexed(240), Color::Rgb(153, 153, 153)) }
fn text() -> Color { Color::Reset }
fn bright() -> Color { Color::Reset }
fn accent() -> Color { pick(Color::Indexed(110), Color::Rgb(215, 119, 87)) }
fn green() -> Color { pick(Color::Indexed(114), Color::Rgb(44, 122, 57)) }
fn yellow() -> Color { pick(Color::Indexed(220), Color::Rgb(150, 108, 30)) }
/// Dialog titles (Claude Code's pickers use light blue on dark, its
/// permission blue on light).
fn title_color() -> Color { pick(Color::Indexed(153), Color::Rgb(87, 105, 247)) }
fn selected_bg() -> Color { pick(Color::Indexed(236), Color::Rgb(240, 240, 240)) }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Working,
    Ready,
    Starting,
    /// A Claude Code / Codex session not (yet) in orchestra.
    Elsewhere,
}

impl Status {
    fn glyph(self) -> (&'static str, Color) {
        match self {
            Status::Working => ("●", yellow()),
            Status::Ready => ("✻", green()),
            Status::Starting => ("◌", accent()),
            Status::Elsewhere => ("∙", dim()),
        }
    }
    fn word(self) -> (&'static str, Color) {
        match self {
            Status::Working => ("Working", yellow()),
            Status::Ready => ("Ready", green()),
            Status::Starting => ("Starting", accent()),
            Status::Elsewhere => ("", grey()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub status: Status,
    pub name: String,
    /// Replaces the status word (e.g. "Claude Code" for external rows).
    pub label: Option<String>,
    pub summary: String,
    /// Right-aligned: agent/model tag.
    pub meta: String,
    pub age: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub title: String,
    pub rows: Vec<Row>,
    /// Header is a selectable row that folds the group (▾ open, ▸ closed).
    pub collapsible: bool,
    pub collapsed: bool,
    /// Rows hidden while collapsed (shown in the header).
    pub hidden: usize,
}

impl Group {
    pub fn plain(title: impl Into<String>, rows: Vec<Row>) -> Self {
        Self { title: title.into(), rows, collapsible: false, collapsed: false, hidden: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// Two-column shortcut list.
    Help(Vec<(String, String)>),
    /// A titled block of text (a /btw answer, a /bug draft).
    Text { title: String, subtitle: String, body: String },
    /// Pick one option (name, description); `current` gets a ✔.
    Picker {
        title: String,
        subtitle: String,
        options: Vec<(String, String)>,
        current: Option<usize>,
        selected: usize,
        filter: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewModel {
    pub title: String,
    pub subtitle: String,
    pub hint: String,
    pub groups: Vec<Group>,
    /// Index into the flattened rows of all groups.
    pub selected: Option<usize>,
    pub input: String,
    pub cursor: usize,
    pub placeholder: String,
    /// Status message if any, else the key hints.
    pub footer: String,
    pub footer_is_status: bool,
    pub overlay: Option<Overlay>,
    pub empty_text: String,
    /// Slash-command completions shown above the prompt while typing `/`.
    pub suggestions: Vec<(String, String)>,
}

fn width(s: &str) -> usize {
    s.chars().count()
}

/// Truncate to `max` columns with an ellipsis.
pub fn fit(s: &str, max: usize) -> String {
    let s = s.replace(['\n', '\t'], " ");
    if width(&s) <= max {
        return s;
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn pad(s: &str, w: usize) -> String {
    let s = fit(s, w);
    let n = width(&s);
    format!("{s}{}", " ".repeat(w.saturating_sub(n)))
}

/// One list row as a Line, `w` columns wide.
#[cfg(test)]
pub fn row_line(r: &Row, w: usize, selected: bool) -> Line<'static> {
    row_line_with(r, w, selected, width(&r.meta))
}

/// Same, with the meta column padded to `meta_w` so rows line up.
pub fn row_line_with(r: &Row, w: usize, selected: bool, meta_w: usize) -> Line<'static> {
    let (glyph, gcolor) = r.status.glyph();
    let (word, wcolor) = match &r.label {
        Some(l) => (l.as_str(), accent()),
        None => r.status.word(),
    };
    let name_w = (w * 3 / 10).clamp(16, 44);
    let right = format!("{:>meta_w$}  {:>4}", fit(&r.meta, meta_w), r.age);
    let right_w = width(&right);
    let summary_w = w.saturating_sub(3 + name_w + 2 + width(word) + 3 + right_w + 2);
    let summary = fit(&r.summary, summary_w);
    let gap = w.saturating_sub(3 + name_w + 2 + width(word) + if summary.is_empty() { 0 } else { 3 + width(&summary) } + right_w);
    let name_style = if selected {
        Style::default().fg(bright()).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(text())
    };
    let mut spans = vec![
        Span::styled(format!(" {glyph} "), Style::default().fg(gcolor)),
        Span::styled(pad(&r.name, name_w), name_style),
        Span::raw("  "),
        Span::styled(word.to_string(), Style::default().fg(wcolor)),
    ];
    if !summary.is_empty() {
        spans.push(Span::styled(if word.is_empty() { "".into() } else { " · ".to_string() }, Style::default().fg(grey())));
        spans.push(Span::styled(summary, Style::default().fg(grey())));
    }
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(right, Style::default().fg(dim())));
    let mut line = Line::from(spans);
    if selected {
        line = line.style(Style::default().bg(selected_bg()));
    }
    line
}

/// Word-wrap to `w` columns, keeping existing line breaks.
pub fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(10);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split(' ') {
            if width(&line) + width(word) + 1 > w && !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

fn rule(w: u16) -> Line<'static> {
    Line::from(Span::styled("─".repeat(w as usize), Style::default().fg(dim())))
}

/// Draw the whole view. Returns the cursor position for the prompt.
pub fn render(buf: &mut Buffer, area: Rect, vm: &ViewModel) -> (u16, u16) {
    let w = area.width as usize;
    let mut y = area.y;
    let line_at = |buf: &mut Buffer, y: u16, line: Line<'static>| {
        if y < area.bottom() {
            Paragraph::new(line).render(Rect::new(area.x, y, area.width, 1), buf);
        }
    };

    // Header: title, subtitle, blank, hint, blank.
    line_at(buf, y, Line::from(vec![
        Span::styled(" orchestra", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", vm.title), Style::default().fg(grey())),
    ]));
    y += 1;
    line_at(buf, y, Line::from(Span::styled(format!(" {}", vm.subtitle), Style::default().fg(grey()))));
    y += 2;
    line_at(buf, y, Line::from(Span::styled(format!(" {}", fit(&vm.hint, w.saturating_sub(1))), Style::default().fg(grey()))));
    y += 2;

    // Bottom block: rule, prompt, rule, footer.
    let bottom_h = 4u16;
    let list_top = y;
    let list_bottom = area.bottom().saturating_sub(bottom_h);
    let list_h = list_bottom.saturating_sub(list_top) as usize;

    let sugg_h = vm.suggestions.len().min(list_h.saturating_sub(2));
    let list_h = list_h - sugg_h;
    for (i, (cmd, desc)) in vm.suggestions.iter().take(sugg_h).enumerate() {
        line_at(buf, list_top + (list_h + i) as u16, Line::from(vec![
            Span::styled(format!("  {}", pad(cmd, 18)), Style::default().fg(if i == 0 { bright() } else { text() })),
            Span::styled(fit(desc, w.saturating_sub(20)), Style::default().fg(grey())),
        ]));
    }
    match &vm.overlay {
        Some(o) => render_overlay(buf, Rect::new(area.x, list_top, area.width, list_h as u16), o),
        None => {
            // Flatten groups into lines, remembering where the selection is.
            let meta_w = vm.groups.iter().flat_map(|g| &g.rows).map(|r| width(&r.meta)).max().unwrap_or(0).min(w / 4);
            let mut lines: Vec<Line<'static>> = Vec::new();
            let mut sel_line = None;
            let mut idx = 0usize;
            for (gi, g) in vm.groups.iter().enumerate() {
                if g.rows.is_empty() && !(g.collapsible && g.hidden > 0) {
                    continue;
                }
                if gi > 0 && !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                if g.collapsible {
                    // The header is a row of its own: it can be selected
                    // and folds the group.
                    let selected = vm.selected == Some(idx);
                    if selected {
                        sel_line = Some(lines.len());
                    }
                    let arrow = if g.collapsed { "▸" } else { "▾" };
                    let count = if g.collapsed { format!("  {} hidden", g.hidden) } else { String::new() };
                    let style = if selected {
                        Style::default().fg(bright()).bg(selected_bg()).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(grey())
                    };
                    let mut line = Line::from(vec![
                        Span::styled(format!(" {arrow} {}", g.title), style),
                        Span::styled(count, Style::default().fg(dim())),
                    ]);
                    if selected {
                        line = line.style(Style::default().bg(selected_bg()));
                    }
                    lines.push(line);
                    idx += 1;
                } else {
                    lines.push(Line::from(Span::styled(format!(" {}", g.title), Style::default().fg(grey()))));
                }
                for r in &g.rows {
                    let selected = vm.selected == Some(idx);
                    if selected {
                        sel_line = Some(lines.len());
                    }
                    lines.push(row_line_with(r, w, selected, meta_w));
                    idx += 1;
                }
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled(format!(" {}", vm.empty_text), Style::default().fg(dim()))));
            }
            // Scroll so the selection stays visible.
            let offset = match sel_line {
                Some(s) if s >= list_h => s + 1 - list_h,
                _ => 0,
            };
            for (i, l) in lines.into_iter().skip(offset).take(list_h).enumerate() {
                line_at(buf, list_top + i as u16, l);
            }
        }
    }

    // Prompt.
    let py = list_bottom;
    line_at(buf, py, rule(area.width));
    let prompt = if vm.input.is_empty() {
        Line::from(vec![
            Span::styled("❯ ", Style::default().fg(text())),
            Span::styled(vm.placeholder.clone(), Style::default().fg(dim())),
        ])
    } else {
        Line::from(vec![
            Span::styled("❯ ", Style::default().fg(text())),
            Span::styled(vm.input.clone(), Style::default().fg(bright())),
        ])
    };
    line_at(buf, py + 1, prompt);
    line_at(buf, py + 2, rule(area.width));
    let footer_color = if vm.footer_is_status { yellow() } else { grey() };
    line_at(buf, py + 3, Line::from(Span::styled(
        format!("  {}", fit(&vm.footer, w.saturating_sub(2))),
        Style::default().fg(footer_color),
    )));
    (area.x + 2 + vm.cursor as u16, py + 1)
}

fn render_overlay(buf: &mut Buffer, area: Rect, o: &Overlay) {
    let w = area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    match o {
        Overlay::Help(pairs) => {
            lines.push(Line::from(Span::styled(" Shortcuts", Style::default().fg(grey()))));
            let col = w / 2;
            for chunk in pairs.chunks(2) {
                let cell = |(k, v): &(String, String)| format!("   {:<14}{}", k, v);
                let left = pad(&cell(&chunk[0]), col);
                let right = chunk.get(1).map(cell).unwrap_or_default();
                lines.push(Line::from(Span::styled(format!("{left}{right}"), Style::default().fg(text()))));
            }
        }
        Overlay::Text { title, subtitle, body } => {
            lines.push(Line::from(Span::styled(format!("  {title}"), Style::default().fg(title_color()).add_modifier(Modifier::BOLD))));
            lines.push(Line::from(Span::styled(format!("  {}", fit(subtitle, w.saturating_sub(2))), Style::default().fg(grey()))));
            lines.push(Line::raw(""));
            for l in wrap(body, w.saturating_sub(4)) {
                lines.push(Line::from(Span::styled(format!("  {l}"), Style::default().fg(text()))));
            }
        }
        Overlay::Picker { title, subtitle, options, current, selected, filter } => {
            lines.push(Line::from(vec![
                Span::styled(format!("  {title}"), Style::default().fg(title_color()).add_modifier(Modifier::BOLD)),
                Span::styled(if filter.is_empty() { String::new() } else { format!("   filter: {filter}") }, Style::default().fg(accent())),
            ]));
            lines.push(Line::from(Span::styled(format!("  {}", fit(subtitle, w.saturating_sub(2))), Style::default().fg(grey()))));
            let visible = (area.height as usize).saturating_sub(3).max(1);
            let offset = selected.saturating_sub(visible.saturating_sub(1));
            let name_w = options.iter().map(|(n, _)| width(n) + 2).max().unwrap_or(10).min(w / 2);
            let num_w = options.len().to_string().len() + 1;
            for (i, (name, desc)) in options.iter().enumerate().skip(offset).take(visible) {
                let sel = i == *selected;
                let arrow = if sel { "❯" } else if i == offset && offset > 0 { "↑" } else if i + 1 == offset + visible && i + 1 < options.len() { "↓" } else { " " };
                let check = if Some(i) == *current { " ✔" } else { "" };
                let num = format!("{:<num_w$}", format!("{}.", i + 1));
                let name_style = if sel { Style::default().fg(bright()).add_modifier(Modifier::BOLD) } else { Style::default().fg(text()) };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {arrow} {num} "), Style::default().fg(if sel { bright() } else { grey() })),
                    Span::styled(pad(&format!("{name}{check}"), name_w), name_style),
                    Span::styled(fit(desc, w.saturating_sub(name_w + num_w + 6)), Style::default().fg(grey())),
                ]));
            }
            if options.len() > offset + visible {
                lines.push(Line::from(Span::styled(format!("     … +{} more", options.len() - offset - visible), Style::default().fg(dim()))));
            }
            if options.is_empty() {
                lines.push(Line::from(Span::styled("    no match", Style::default().fg(dim()))));
            }
        }
    }
    for (i, l) in lines.into_iter().take(area.height as usize).enumerate() {
        Paragraph::new(l).render(Rect::new(area.x, area.y + i as u16, area.width, 1), buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, status: Status) -> Row {
        Row { status, name: name.into(), label: None, summary: "did a thing".into(), meta: "pi · glm".into(), age: "3m".into() }
    }

    fn text_of(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()
    }

    fn vm(groups: Vec<Group>) -> ViewModel {
        ViewModel {
            title: "~/repo".into(),
            subtitle: "1 working".into(),
            hint: "enter opens".into(),
            groups,
            selected: Some(0),
            input: String::new(),
            cursor: 0,
            placeholder: "describe a task for a new session".into(),
            footer: "? for shortcuts".into(),
            footer_is_status: false,
            overlay: None,
            empty_text: "No sessions".into(),
            suggestions: Vec::new(),
        }
    }

    #[test]
    fn row_fits_width_and_right_aligns_meta() {
        for w in [60usize, 100, 160] {
            let l = row_line(&row("fix-the-bug", Status::Working), w, false);
            let s: String = l.spans.iter().map(|s| s.content.to_string()).collect();
            assert_eq!(width(&s), w, "row fills exactly {w} columns: {s:?}");
            assert!(s.trim_end().ends_with("pi · glm    3m"), "{s:?}");
            assert!(s.contains("Working · did a thing"));
        }
    }

    #[test]
    fn long_summary_is_truncated_not_wrapped() {
        let mut r = row("n", Status::Ready);
        r.summary = "x".repeat(500);
        let s: String = row_line(&r, 80, false).spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(width(&s), 80);
        assert!(s.contains('…'));
    }

    #[test]
    fn renders_groups_prompt_and_footer() {
        let v = vm(vec![
            Group::plain("Sessions", vec![row("a", Status::Working), row("b", Status::Ready)]),
            Group::plain("Claude Code · not in orchestra", vec![row("c", Status::Elsewhere)]),
        ]);
        let area = Rect::new(0, 0, 100, 20);
        let mut buf = Buffer::empty(area);
        let (cx, cy) = render(&mut buf, area, &v);
        let screen: Vec<String> = (0..20).map(|y| text_of(&buf, y)).collect();
        assert!(screen[0].starts_with(" orchestra  ~/repo"));
        assert_eq!(screen[5], " Sessions");
        assert!(screen[6].contains("● a"));
        assert!(screen[9].contains("Claude Code · not in orchestra"));
        assert!(screen[16].starts_with("────"));
        assert!(screen[17].starts_with("❯ describe a task"));
        assert!(screen[19].contains("? for shortcuts"));
        assert_eq!((cx, cy), (2, 17));
        // No box-drawing corners anywhere.
        assert!(!screen.iter().any(|l| l.contains('┌') || l.contains('└')));
    }

    #[test]
    fn scrolls_to_keep_selection_visible() {
        let rows: Vec<Row> = (0..30).map(|i| row(&format!("s{i}"), Status::Ready)).collect();
        let mut v = vm(vec![Group::plain("Sessions", rows)]);
        v.selected = Some(29);
        let area = Rect::new(0, 0, 80, 16);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &v);
        let screen: Vec<String> = (0..16).map(|y| text_of(&buf, y)).collect();
        assert!(screen.iter().any(|l| l.contains("s29")), "{screen:#?}");
    }

    #[test]
    fn collapsed_group_header_is_selectable_and_counts() {
        let mut v = vm(vec![
            Group::plain("Sessions", vec![row("a", Status::Ready)]),
            Group { title: "Claude Code".into(), rows: vec![], collapsible: true, collapsed: true, hidden: 7 },
        ]);
        v.selected = Some(1);
        let area = Rect::new(0, 0, 80, 14);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &v);
        let screen: Vec<String> = (0..14).map(|y| text_of(&buf, y)).collect();
        assert!(screen.iter().any(|l| l.contains("▸ Claude Code  7 hidden")), "{screen:#?}");
    }

    #[test]
    fn picker_marks_current_and_selection() {
        let mut v = vm(vec![]);
        v.overlay = Some(Overlay::Picker {
            title: "Switch to".into(),
            subtitle: "Moves this conversation.".into(),
            options: vec![("Claude Code".into(), "default model".into()), ("pi · GLM".into(), "1M context".into())],
            current: Some(0),
            selected: 1,
            filter: String::new(),
        });
        let area = Rect::new(0, 0, 80, 14);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &v);
        let screen: Vec<String> = (0..14).map(|y| text_of(&buf, y)).collect();
        assert!(screen.iter().any(|l| l.contains("1. Claude Code ✔")), "{screen:#?}");
        assert!(screen.iter().any(|l| l.contains("❯ 2. pi · GLM")), "{screen:#?}");
    }
}
