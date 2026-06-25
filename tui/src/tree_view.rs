// tree_view.rs — Tree View renderer for the TUI.
//
// Draws the spatial graph described in Design_Document.md §9:
//   - Dots: `●` for agents, `○` for sessions.
//   - Color encodes state (yellow=working, dark gray=idle, ...).
//   - Right-angle connectors between parent↔child centers.
//   - Legend in the top-right corner (always visible).
//   - Selected node ringed; its path to root drawn brighter.
//   - No labels in the graph — identity lives in the detail pane.
//
// All rendering writes into a `ratatui::buffer::Buffer`, which is
// inspectable in tests without a real terminal. The renderer owns no
// state — it's a pure function of (tree, layout, selection).

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::tree_layout::{connectors, layout, to_screen, Connector};
use crate::tree_store::{NodeKind, NodeState, NodeView, Tree};

/// The dot glyph for a node kind. `●` for agents, `○` for sessions
/// (Design §9 — reuses the session-state dot glyphs).
pub fn kind_glyph(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Agent => "●",
        NodeKind::Session => "○",
    }
}

/// State → color. Agents and sessions share the palette (Design §9).
pub fn state_color(state: NodeState) -> Color {
    match state {
        NodeState::Working => Color::Yellow,
        NodeState::Idle => Color::DarkGray,
        NodeState::NeedsInput => Color::Cyan,
        NodeState::Completed => Color::Green,
        NodeState::Failed => Color::Red,
        NodeState::Stale => Color::Gray,
        NodeState::Initializing => Color::Blue,
        NodeState::Unknown => Color::Magenta,
    }
}

/// Render the full Tree View into `area` of `buf`. Draws connectors first
/// (so dots paint over them), then dots, then the legend overlay in the
/// top-right. `selected_id` controls the highlight ring + bright path.
pub fn render_tree(
    buf: &mut Buffer,
    area: Rect,
    tree: &Tree,
    selected_id: Option<&str>,
    show_detail: bool,
) {
    if !tree.has_root() {
        render_empty_tree(buf, area);
        render_legend(buf, area);
        return;
    }

    let lay = layout(tree);
    // Reserve right columns for the legend. The legend is ~16 wide; give it
    // a 1-cell margin from the graph.
    let legend_cols = 20u16;
    let screen = to_screen(&lay, area.width, area.height, legend_cols);

    // Compute the path-to-root set for the selected node (bright connectors).
    let bright = selected_id
        .map(|id| path_to_root(tree, id))
        .unwrap_or_default();

    // Detail pane takes the bottom 8 rows when toggled.
    let (graph_area, detail_area) = if show_detail {
        let dh = 8u16.min(area.height.saturating_sub(4));
        (
            Rect::new(area.x, area.y, area.width, area.height.saturating_sub(dh)),
            Rect::new(area.x, area.y + area.height.saturating_sub(dh), area.width, dh),
        )
    } else {
        (area, Rect::new(0, 0, 0, 0))
    };

    // Draw connectors into the graph area.
    let conns = connectors(tree, &screen);
    for c in &conns {
        let on_path = bright.contains(&c.parent_id.as_str())
            && bright.contains(&c.child_id.as_str());
        draw_connector(buf, graph_area, c, on_path);
    }

    // Draw dots (on top of connectors).
    for (id, &(col, row)) in &screen.cells {
        let node = match tree.get(id) {
            Some(n) => n,
            None => continue,
        };
        let is_selected = selected_id == Some(id.as_str());
        let color = state_color(node.effective);
        let glyph = if is_selected {
            // Selected: ring the dot with brackets and bold it.
            "[●]"
        } else {
            kind_glyph(node.raw.kind)
        };
        let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        let (abs_col, abs_row) = absolute_cell(graph_area, col, row);
        set_glyph(buf, abs_col, abs_row, glyph, style);
    }

    // Legend always on top, top-right.
    render_legend(buf, area);

    // Detail pane for the selected node.
    if show_detail && detail_area.height > 0 {
        if let Some(id) = selected_id {
            if let Some(node) = tree.get(id) {
                render_detail_pane(buf, detail_area, node);
            }
        }
    }
}

/// Empty-tree state: a single dim dot at the left center with a hint.
fn render_empty_tree(buf: &mut Buffer, area: Rect) {
    let row = area.y + area.height / 2;
    let col = area.x + 2;
    set_glyph(
        buf,
        col,
        row,
        "●",
        Style::default().fg(Color::Blue).add_modifier(Modifier::DIM),
    );
    let hint = "Main Agent — no sessions yet. Press Enter to drop into Agent View.";
    let hint_col = col + 2;
    set_glyph(buf, hint_col, row, hint, Style::default().fg(Color::DarkGray));
}

/// Draw a right-angle connector between a parent and child. The path goes:
///   parent → right to mid-column → vertical to child row → right to child.
/// `on_path` brightens the connector (selected lineage).
fn draw_connector(buf: &mut Buffer, area: Rect, c: &Connector, on_path: bool) {
    let (pc, pr) = absolute_cell(area, c.parent_col, c.parent_row);
    let (cc, cr) = absolute_cell(area, c.child_col, c.child_row);
    if cc <= pc {
        return; // child not to the right — skip (shouldn't happen in a tree)
    }
    let mid = pc + (cc - pc) / 2;
    let style = if on_path {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // Horizontal from parent rightward to mid.
    for x in (pc + 1)..mid {
        set_glyph(buf, x, pr, "─", style);
    }
    // Vertical from parent row to child row at mid column.
    if pr != cr {
        let (lo, hi) = if pr < cr { (pr, cr) } else { (cr, pr) };
        for y in (lo + 1)..hi {
            set_glyph(buf, mid, y, "│", style);
        }
        // Corners at parent and child rows.
        let top_corner = if pr < cr { "┐" } else { "┘" };
        let bot_corner = if pr < cr { "└" } else { "┌" };
        set_glyph(buf, mid, pr, top_corner, style);
        set_glyph(buf, mid, cr, bot_corner, style);
    } else {
        // Same row: tee at mid.
        set_glyph(buf, mid, pr, "┬", style);
    }
    // Horizontal from mid to child.
    for x in (mid + 1)..cc {
        set_glyph(buf, x, cr, "─", style);
    }
}

/// The set of node ids from `id` up to the root (inclusive). Used to
/// brighten the selected lineage.
fn path_to_root<'a>(tree: &'a Tree, id: &'a str) -> Vec<&'a str> {
    let mut path = Vec::new();
    let mut cur: Option<&'a str> = Some(id);
    while let Some(cid) = cur {
        path.push(cid);
        let parent = tree.get(cid).and_then(|n| n.raw.parent_id.as_deref());
        cur = parent;
        // Guard against cycles (shouldn't happen, but cheap insurance).
        if path.len() > tree.nodes.len() {
            break;
        }
    }
    path
}

/// Compact legend in the top-right corner (Design §9). Always visible.
fn render_legend(buf: &mut Buffer, area: Rect) {
    let lines = [
        ("● agent  ○ session", Color::White),
        ("● working", Color::Yellow),
        ("● idle", Color::DarkGray),
        ("● needs_input", Color::Cyan),
        ("● completed", Color::Green),
        ("● failed", Color::Red),
        ("● stale", Color::Gray),
        ("● initializing", Color::Blue),
    ];
    let width = 18u16;
    let height = lines.len() as u16;
    let x = area.right().saturating_sub(width + 1);
    let y = area.y + 1;
    // Clear the legend region with spaces (so it doesn't overlap graph dots).
    for row in 0..height {
        for col in 0..width {
            if x + col < area.right() && y + row < area.bottom() {
                let cell = buf.cell_mut((x + col, y + row)).expect("in-bounds");
                cell.set_char(' ');
                cell.set_style(Style::default());
            }
        }
    }
    for (i, (text, color)) in lines.iter().enumerate() {
        for (col, ch) in text.chars().enumerate() {
            let xi = x + col as u16;
            let yi = y + i as u16;
            if xi < area.right() && yi < area.bottom() {
                let cell = buf.cell_mut((xi, yi)).expect("in-bounds");
                cell.set_char(ch);
                cell.set_style(Style::default().fg(*color));
            }
        }
    }
}

/// Detail pane for the selected node: id, kind, state, config, sessions.
fn render_detail_pane(buf: &mut Buffer, area: Rect, node: &NodeView) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", node.raw.label()))
        .border_style(Style::default().fg(state_color(node.effective)));
    let inner = block.inner(area);
    block.render(area, buf);

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::raw("id: "),
        Span::styled(node.raw.id.clone(), Style::default().fg(Color::White)),
        Span::raw("   kind: "),
        Span::styled(
            format!("{:?}", node.raw.kind).to_lowercase(),
            Style::default().fg(Color::White),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw("state: "),
        Span::styled(
            format!("{:?}", node.effective).to_lowercase(),
            Style::default().fg(state_color(node.effective)),
        ),
    ]));
    if let Some(parent) = &node.raw.parent_id {
        lines.push(Line::from(vec![
            Span::raw("parent: "),
            Span::styled(parent.clone(), Style::default().fg(Color::White)),
        ]));
    }
    if let Some(cluster) = &node.raw.sky_cluster {
        lines.push(Line::from(vec![
            Span::raw("cluster: "),
            Span::styled(cluster.clone(), Style::default().fg(Color::White)),
        ]));
    }
    if let Some(tmux) = &node.raw.tmux_session {
        lines.push(Line::from(vec![
            Span::raw("tmux: "),
            Span::styled(tmux.clone(), Style::default().fg(Color::White)),
        ]));
    }
    if !node.raw.children.is_empty() {
        lines.push(Line::from(format!("children: {}", node.raw.children.join(", "))));
    }
    if let Some(model) = &node.raw.config.model {
        lines.push(Line::from(format!("model: {model}")));
    }
    if let Some(host) = &node.raw.box_host {
        lines.push(Line::from(format!("host: {host}")));
    }
    lines.push(Line::from(format!(
        "last_pulled: {}s ago",
        crate::tree_store::unix_now().saturating_sub(node.raw.last_pulled)
    )));

    Paragraph::new(lines).render(inner, buf);
}

/// Convert a (col, row) relative to `area`'s top-left into absolute buffer
/// coordinates, clamped to the area.
fn absolute_cell(area: Rect, col: u16, row: u16) -> (u16, u16) {
    let x = area.x.saturating_add(col).min(area.right().saturating_sub(1));
    let y = area.y.saturating_add(row).min(area.bottom().saturating_sub(1));
    (x, y)
}

/// Write a glyph (possibly multi-char) at a cell, with style. Multi-char
/// glyphs advance rightward. Out-of-bounds writes are silently dropped.
fn set_glyph(buf: &mut Buffer, x: u16, y: u16, glyph: &str, style: Style) {
    for (i, ch) in glyph.chars().enumerate() {
        let xi = x.checked_add(i as u16).unwrap_or(u16::MAX);
        if let Some(cell) = buf.cell_mut((xi, y)) {
            cell.set_char(ch);
            cell.set_style(style);
        }
    }
}

/// Re-export for the App: which node should be auto-selected after a store
/// reload. Defaults to the root, or the previously selected id if still
/// present.
pub fn default_selection(tree: &Tree, prev: Option<&str>) -> Option<String> {
    if let Some(prev) = prev {
        if tree.get(prev).is_some() {
            return Some(prev.to_string());
        }
    }
    tree.root_id.clone()
}

/// Navigation helper: the parent of `id` in the tree.
pub fn parent_of(tree: &Tree, id: &str) -> Option<String> {
    tree.get(id).and_then(|n| n.raw.parent_id.clone())
}

/// Navigation helper: the first child of `id`, if any.
pub fn first_child(tree: &Tree, id: &str) -> Option<String> {
    tree.children(id).first().map(|v| v.raw.id.clone())
}

/// Navigation helper: nodes at the same depth as `id` (including itself),
/// in DFS order. Used for ↑/↓ within-level movement.
pub fn nodes_at_depth(tree: &Tree, id: &str) -> Vec<String> {
    let target_depth = tree.get(id).map(|n| {
        // Depth = number of ancestors.
        let mut d = 0usize;
        let mut cur = n.raw.parent_id.as_deref();
        while let Some(pid) = cur {
            d += 1;
            cur = tree.get(pid).and_then(|p| p.raw.parent_id.as_deref());
            if d > tree.nodes.len() {
                break;
            }
        }
        d
    }).unwrap_or(0);
    let order = tree.dfs();
    order
        .into_iter()
        .filter(|nid| {
            tree.get(nid).map(|n| {
                let mut d = 0usize;
                let mut cur = n.raw.parent_id.as_deref();
                while let Some(pid) = cur {
                    d += 1;
                    cur = tree.get(pid).and_then(|p| p.raw.parent_id.as_deref());
                    if d > tree.nodes.len() {
                        break;
                    }
                }
                d == target_depth
            }).unwrap_or(false)
        })
        .collect()
}

/// Pick the nearest node to `id` by y-coordinate from a candidate list.
/// Used by ↑/↓ to move to the nearest sibling/cousin at the same depth.
pub fn nearest_by_y(tree: &Tree, candidates: &[String], id: &str) -> Option<String> {
    let lay = layout(tree);
    let target = lay.get(id)?;
    let mut best: Option<(f64, String)> = None;
    for c in candidates {
        if c == id {
            continue;
        }
        let Some(p) = lay.get(c) else { continue };
        let dy = (p.y - target.y).abs();
        match &best {
            None => best = Some((dy, c.clone())),
            Some((bdy, _)) if dy < *bdy => best = Some((dy, c.clone())),
            _ => {}
        }
    }
    best.map(|(_, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_store::{Node, NodeKind, NodeState, NodeView};

    fn buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    fn tree_from(specs: &[(&str, Option<&str>, &[&str], NodeKind, NodeState)]) -> Tree {
        let now = crate::tree_store::unix_now();
        let mut nodes = HashMap::new();
        for &(id, parent, children, kind, state) in specs {
            let n = Node {
                id: id.to_string(),
                kind,
                parent_id: parent.map(|s| s.to_string()),
                name: id.to_string(),
                display_name: None,
                sky_cluster: if kind == NodeKind::Agent { Some(id.to_string()) } else { None },
                tmux_session: if kind == NodeKind::Session { Some(id.to_string()) } else { None },
                box_host: None,
                bridge_url: None,
                gateway_url: None,
                state,
                config: crate::tree_store::Config::default(),
                openclaw_sessions: Vec::new(),
                pi_sessions: Vec::new(),
                created_at: now,
                last_pulled: now,
                children: children.iter().map(|s| s.to_string()).collect(),
            };
            nodes.insert(id.to_string(), NodeView { raw: n, effective: state });
        }
        let root_id = specs.iter().find(|(_, p, _, _, _)| p.is_none()).map(|(id, _, _, _, _)| id.to_string());
        Tree { root_id, nodes, updated_at: now }
    }

    #[test]
    fn kind_glyph_agent_vs_session() {
        assert_eq!(kind_glyph(NodeKind::Agent), "●");
        assert_eq!(kind_glyph(NodeKind::Session), "○");
    }

    #[test]
    fn state_color_covers_all_variants() {
        for s in [
            NodeState::Working,
            NodeState::Idle,
            NodeState::NeedsInput,
            NodeState::Completed,
            NodeState::Failed,
            NodeState::Stale,
            NodeState::Initializing,
            NodeState::Unknown,
        ] {
            let _ = state_color(s); // just assert it doesn't panic
        }
    }

    #[test]
    fn empty_tree_renders_hint() {
        let tree = Tree::default();
        let mut b = buf(60, 20);
        render_tree(&mut b, Rect::new(0, 0, 60, 20), &tree, None, false);
        let s = buffer_string(&b, 0, 0, 60, 20);
        assert!(s.contains("no sessions yet"), "got: {s}");
    }

    #[test]
    fn renders_root_and_child_dots() {
        let tree = tree_from(&[
            ("root", None, &["a"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &[], NodeKind::Session, NodeState::Working),
        ]);
        let mut b = buf(70, 20);
        render_tree(&mut b, Rect::new(0, 0, 70, 20), &tree, None, false);
        let s = buffer_string(&b, 0, 0, 70, 20);
        // Both dots should appear.
        assert!(s.contains('●'), "agent dot missing: {s}");
        assert!(s.contains('○'), "session dot missing: {s}");
    }

    #[test]
    fn selected_node_is_ringed() {
        let tree = tree_from(&[
            ("root", None, &["a"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &[], NodeKind::Session, NodeState::Working),
        ]);
        let mut b = buf(70, 20);
        render_tree(&mut b, Rect::new(0, 0, 70, 20), &tree, Some("root"), false);
        let s = buffer_string(&b, 0, 0, 70, 20);
        assert!(s.contains("[●]"), "selected ring missing: {s}");
    }

    #[test]
    fn legend_is_present() {
        let tree = tree_from(&[
            ("root", None, &["a"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &[], NodeKind::Session, NodeState::Working),
        ]);
        let mut b = buf(70, 24);
        render_tree(&mut b, Rect::new(0, 0, 70, 24), &tree, None, false);
        let s = buffer_string(&b, 0, 0, 70, 24);
        assert!(s.contains("agent"), "legend missing agent: {s}");
        assert!(s.contains("working"), "legend missing working: {s}");
        assert!(s.contains("stale"), "legend missing stale: {s}");
    }

    #[test]
    fn connector_drawn_between_parent_and_child() {
        let tree = tree_from(&[
            ("root", None, &["a"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &[], NodeKind::Session, NodeState::Working),
        ]);
        let mut b = buf(70, 20);
        render_tree(&mut b, Rect::new(0, 0, 70, 20), &tree, None, false);
        let s = buffer_string(&b, 0, 0, 70, 20);
        // Some box-drawing char should connect them.
        assert!(
            s.contains('─') || s.contains('│') || s.contains('┐') || s.contains('└'),
            "no connector drawn: {s}"
        );
    }

    #[test]
    fn detail_pane_shows_node_label_when_toggled() {
        let tree = tree_from(&[
            ("root", None, &["a"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &[], NodeKind::Session, NodeState::Working),
        ]);
        let mut b = buf(70, 24);
        render_tree(&mut b, Rect::new(0, 0, 70, 24), &tree, Some("a"), true);
        let s = buffer_string(&b, 0, 0, 70, 24);
        assert!(s.contains("id: a"), "detail pane missing id: {s}");
        assert!(s.contains("kind:"), "detail pane missing kind: {s}");
    }

    #[test]
    fn path_to_root_walks_ancestors() {
        let tree = tree_from(&[
            ("root", None, &["a"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &["a1"], NodeKind::Agent, NodeState::Idle),
            ("a1", Some("a"), &[], NodeKind::Session, NodeState::Working),
        ]);
        let path = path_to_root(&tree, "a1");
        assert_eq!(path, vec!["a1", "a", "root"]);
    }

    #[test]
    fn default_selection_keeps_prev_or_root() {
        let tree = tree_from(&[
            ("root", None, &["a"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &[], NodeKind::Session, NodeState::Working),
        ]);
        assert_eq!(default_selection(&tree, None), Some("root".to_string()));
        assert_eq!(default_selection(&tree, Some("a")), Some("a".to_string()));
        // Stale prev falls back to root.
        assert_eq!(default_selection(&tree, Some("ghost")), Some("root".to_string()));
    }

    #[test]
    fn parent_and_first_child() {
        let tree = tree_from(&[
            ("root", None, &["a", "b"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &[], NodeKind::Session, NodeState::Working),
            ("b", Some("root"), &[], NodeKind::Session, NodeState::Idle),
        ]);
        assert_eq!(parent_of(&tree, "a"), Some("root".to_string()));
        assert_eq!(parent_of(&tree, "root"), None);
        assert_eq!(first_child(&tree, "root"), Some("a".to_string()));
        assert_eq!(first_child(&tree, "a"), None);
    }

    #[test]
    fn nodes_at_depth_returns_siblings() {
        let tree = tree_from(&[
            ("root", None, &["a", "b"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &[], NodeKind::Session, NodeState::Working),
            ("b", Some("root"), &[], NodeKind::Session, NodeState::Idle),
        ]);
        // depth 1 = a, b (both children of root).
        let depth1 = nodes_at_depth(&tree, "a");
        assert!(depth1.contains(&"a".to_string()));
        assert!(depth1.contains(&"b".to_string()));
        assert_eq!(depth1.len(), 2);
        // depth 0 = root only.
        let depth0 = nodes_at_depth(&tree, "root");
        assert_eq!(depth0, vec!["root".to_string()]);
    }

    #[test]
    fn nearest_by_y_picks_closest() {
        // root → a, b, c at depth 1. b is between a and c.
        let tree = tree_from(&[
            ("root", None, &["a", "b", "c"], NodeKind::Agent, NodeState::Idle),
            ("a", Some("root"), &[], NodeKind::Session, NodeState::Idle),
            ("b", Some("root"), &[], NodeKind::Session, NodeState::Idle),
            ("c", Some("root"), &[], NodeKind::Session, NodeState::Idle),
        ]);
        let cands = nodes_at_depth(&tree, "b");
        let near = nearest_by_y(&tree, &cands, "b");
        // a and c are equidistant from b (both 1 unit away); nearest is
        // whichever the scan hits first with strictly-smaller dy. With a
        // tie, the first one wins — either is acceptable.
        assert!(near == Some("a".to_string()) || near == Some("c".to_string()));
    }

    /// Flatten a buffer region to a string for assertion.
    fn buffer_string(buf: &Buffer, x: u16, y: u16, w: u16, h: u16) -> String {
        let mut out = String::new();
        for row in y..(y + h) {
            for col in x..(x + w) {
                if let Some(cell) = buf.cell((col, row)) {
                    out.push(cell.symbol().chars().next().unwrap_or(' '));
                } else {
                    out.push(' ');
                }
            }
            out.push('\n');
        }
        out
    }
}
