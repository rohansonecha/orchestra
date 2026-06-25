// tree_layout.rs — Spatial layout engine for Tree View.
//
// Pure function: Tree in, (x, y) positions out. No terminal, no rendering.
// This separation (Design §9) lets the layout be unit-tested without a TTY
// and reused later for Graph View (which swaps this for a force-directed
// layout but keeps the same renderer).
//
// Algorithm: left-to-right tidy tree (a simplified Reingold-Tilford).
//   - x = depth in the tree (root at x=0, leaves at x=max_depth)
//   - y = vertical position; leaves are assigned successive unit slots,
//     internal nodes sit at the midpoint of their first and last child.
//   - Siblings never overlap because y is monotonic across the DFS.
//
// Coordinates are in abstract "layout units" (floats). Leaves are 1.0 apart
// in y. The renderer scales x by column width and y by row height to fit
// the terminal — it does not recompute the tree structure.
//
// Limitation: the simple algorithm does not implement Reingold-Tilford's
// subtree-separation contour check, so two subtrees under different parents
// could be placed at y-positions that look uneven. For a personal platform
// (tens of nodes) this is fine; revisit if the tree grows dense.

use std::collections::HashMap;

use crate::tree_store::{NodeKind, Tree};

/// A node's position in layout units. `x` is depth (0 = root), `y` is the
/// vertical coordinate (0 = top, increasing downward).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

/// The complete layout: every node id mapped to its position, plus the
/// bounding box (max depth + total vertical span) so the renderer can
/// scale to fit.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub positions: HashMap<String, Pos>,
    pub max_depth: usize,
    pub max_y: f64,
}

impl Layout {
    pub fn get(&self, id: &str) -> Option<Pos> {
        self.positions.get(id).copied()
    }

    /// True if the layout has at least one positioned node.
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }
}

/// Compute the layout for a tree. Returns an empty `Layout` if the tree
/// has no root. The root is placed at (0, max_y / 2.0) — vertically
/// centered — which matches the Design §9 spec ("root is a single dot on
/// the left, vertically centered").
///
/// Actually: the root's y falls out of the recursion as the midpoint of
/// its children, which naturally centers it over its subtree. We then
/// shift everything so the root (and thus the whole tree) is centered at
/// y=0 if desired — but the renderer handles vertical centering against
/// the terminal height, so we leave y starting at 0 (top) here.
pub fn layout(tree: &Tree) -> Layout {
    let Some(root_id) = tree.root_id.as_ref() else {
        return Layout::default();
    };
    let mut state = WalkState::default();
    let root_y = walk(tree, root_id, 0, &mut state);
    let _ = root_y; // root_y is already recorded in state.positions
    Layout {
        positions: state.positions,
        max_depth: state.max_depth,
        max_y: state.next_y,
    }
}

#[derive(Debug, Default)]
struct WalkState {
    positions: HashMap<String, Pos>,
    /// Next available y slot for a leaf. Monotonic across the whole DFS
    /// so no two nodes share a y.
    next_y: f64,
    max_depth: usize,
}

/// Post-order walk. Returns the y of `id`. Leaves consume a slot; internal
/// nodes center over their first and last child.
fn walk(tree: &Tree, id: &str, depth: usize, state: &mut WalkState) -> f64 {
    state.max_depth = state.max_depth.max(depth);
    let children = tree.children(id);
    let y = if children.is_empty() {
        let y = state.next_y;
        state.next_y += 1.0;
        y
    } else {
        // Recurse into all children first (they assign their own y's).
        let first = walk(tree, &children[0].raw.id, depth + 1, state);
        for child in &children[1..] {
            walk(tree, &child.raw.id, depth + 1, state);
        }
        let last_id = children.last().unwrap().raw.id.clone();
        let last = state.positions[&last_id].y;
        // Center between first and last child. If there's one child, this
        // collapses to that child's y (parent aligns with single child).
        (first + last) / 2.0
    };
    state.positions.insert(id.to_string(), Pos {
        x: depth as f64,
        y,
    });
    y
}

/// Map a layout (abstract units) to terminal cells. x → column, y → row.
/// Both are scaled to fit the available area. Returns a map of id → (col, row)
/// plus the depth→column mapping for drawing connectors.
///
/// `area_width` / `area_height` are the renderable cell counts. The layout
/// reserves `legend_cols` on the right for the legend (Design §9).
#[derive(Debug, Clone)]
pub struct ScreenLayout {
    pub cells: HashMap<String, (u16, u16)>,
    pub max_depth: usize,
}

pub fn to_screen(layout: &Layout, area_width: u16, area_height: u16, legend_cols: u16) -> ScreenLayout {
    let mut cells = HashMap::new();
    if layout.is_empty() {
        return ScreenLayout { cells, max_depth: 0 };
    }
    let usable_width = area_width.saturating_sub(legend_cols + 2).max(1);
    let usable_height = area_height.saturating_sub(2).max(1);

    // x: depth 0..=max_depth → columns. Reserve left margin of 2.
    let col_count = (layout.max_depth + 1) as u16;
    let col_width = (usable_width / col_count.max(1)).max(1);

    // y: 0..=max_y → rows. Vertically center the block.
    let y_span = layout.max_y.max(1.0);
    let row_scale = (usable_height as f64 / y_span).max(1.0);
    // If the tree is short, center it vertically.
    let block_height = y_span * row_scale;
    let y_offset = ((usable_height as f64 - block_height) / 2.0).max(0.0);

    for (id, pos) in &layout.positions {
        let col = 2 + (pos.x as u16) * col_width;
        let row = (y_offset + pos.y * row_scale) as u16;
        cells.insert(id.clone(), (col, row));
    }
    ScreenLayout { cells, max_depth: layout.max_depth }
}

/// A connector segment between a parent and one of its children. Used by
/// the renderer to draw lines. Each segment is a run of cells from the
/// parent's column to the child's column at the child's row, plus a
/// vertical riser from the parent's row.
#[derive(Debug, Clone)]
pub struct Connector {
    pub parent_id: String,
    pub child_id: String,
    pub parent_col: u16,
    pub parent_row: u16,
    pub child_col: u16,
    pub child_row: u16,
}

/// Compute connectors for a laid-out tree. One per parent→child edge.
pub fn connectors(tree: &Tree, screen: &ScreenLayout) -> Vec<Connector> {
    let mut out = Vec::new();
    for (id, pos) in &screen.cells {
        let node = match tree.get(id) {
            Some(n) => n,
            None => continue,
        };
        for child in node.raw.children.iter() {
            if let Some(child_pos) = screen.cells.get(child) {
                out.push(Connector {
                    parent_id: id.clone(),
                    child_id: child.clone(),
                    parent_col: pos.0,
                    parent_row: pos.1,
                    child_col: child_pos.0,
                    child_row: child_pos.1,
                });
            }
        }
    }
    out
}

/// Helper: is this node an agent (vs session)? Looks up the tree.
pub fn is_agent(tree: &Tree, id: &str) -> bool {
    tree.get(id).map(|v| v.raw.kind == NodeKind::Agent).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_store::{Node, NodeKind, NodeState, NodeView, TreeStore};
    use std::collections::HashMap;

    /// Build a Tree directly from node specs (no disk). `specs` is a list of
    /// (id, parent, children, kind). All nodes are fresh (not stale).
    fn tree_from(specs: &[(&str, Option<&str>, &[&str], NodeKind)]) -> Tree {
        let now = crate::tree_store::unix_now();
        let mut nodes = HashMap::new();
        for &(id, parent, children, kind) in specs {
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
                state: NodeState::Idle,
                config: crate::tree_store::Config::default(),
                openclaw_sessions: Vec::new(),
                pi_sessions: Vec::new(),
                created_at: now,
                last_pulled: now,
                children: children.iter().map(|s| s.to_string()).collect(),
            };
            nodes.insert(id.to_string(), NodeView { raw: n, effective: NodeState::Idle });
        }
        let root_id = specs.iter().find(|(_, p, _, _)| p.is_none()).map(|(id, _, _, _)| id.to_string());
        Tree { root_id, nodes, updated_at: now }
    }

    #[test]
    fn empty_tree_lays_out_nothing() {
        let tree = Tree::default();
        let l = layout(&tree);
        assert!(l.is_empty());
    }

    #[test]
    fn single_node_is_at_origin() {
        let tree = tree_from(&[("root", None, &[], NodeKind::Agent)]);
        let l = layout(&tree);
        let p = l.get("root").unwrap();
        assert_eq!(p, Pos { x: 0.0, y: 0.0 });
        assert_eq!(l.max_depth, 0);
        assert_eq!(l.max_y, 1.0);
    }

    #[test]
    fn depth_maps_to_x() {
        let tree = tree_from(&[
            ("root", None, &["a"], NodeKind::Agent),
            ("a", Some("root"), &["a1"], NodeKind::Agent),
            ("a1", Some("a"), &[], NodeKind::Session),
        ]);
        let l = layout(&tree);
        assert_eq!(l.get("root").unwrap().x, 0.0);
        assert_eq!(l.get("a").unwrap().x, 1.0);
        assert_eq!(l.get("a1").unwrap().x, 2.0);
        assert_eq!(l.max_depth, 2);
    }

    #[test]
    fn leaves_get_distinct_y() {
        // root → a, b, c (three leaves at depth 1)
        let tree = tree_from(&[
            ("root", None, &["a", "b", "c"], NodeKind::Agent),
            ("a", Some("root"), &[], NodeKind::Session),
            ("b", Some("root"), &[], NodeKind::Session),
            ("c", Some("root"), &[], NodeKind::Session),
        ]);
        let l = layout(&tree);
        let ya = l.get("a").unwrap().y;
        let yb = l.get("b").unwrap().y;
        let yc = l.get("c").unwrap().y;
        // All distinct, monotonically increasing.
        assert!(ya < yb);
        assert!(yb < yc);
        // root centers over first (a) and last (c).
        let yr = l.get("root").unwrap().y;
        assert!((yr - (ya + yc) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn parent_centers_over_first_and_last_child() {
        // root → a (→ a1, a2, a3), b
        let tree = tree_from(&[
            ("root", None, &["a", "b"], NodeKind::Agent),
            ("a", Some("root"), &["a1", "a2", "a3"], NodeKind::Agent),
            ("a1", Some("a"), &[], NodeKind::Session),
            ("a2", Some("a"), &[], NodeKind::Session),
            ("a3", Some("a"), &[], NodeKind::Session),
            ("b", Some("root"), &[], NodeKind::Session),
        ]);
        let l = layout(&tree);
        let ya1 = l.get("a1").unwrap().y;
        let ya3 = l.get("a3").unwrap().y;
        let ya = l.get("a").unwrap().y;
        // a centers over a1 and a3 (its first and last child).
        assert!((ya - (ya1 + ya3) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn single_child_parent_aligns_with_child() {
        // root → a → a1 (single chain)
        let tree = tree_from(&[
            ("root", None, &["a"], NodeKind::Agent),
            ("a", Some("root"), &["a1"], NodeKind::Agent),
            ("a1", Some("a"), &[], NodeKind::Session),
        ]);
        let l = layout(&tree);
        // With one child, parent y == child y (midpoint of [c, c] = c).
        assert_eq!(l.get("root").unwrap().y, l.get("a").unwrap().y);
        assert_eq!(l.get("a").unwrap().y, l.get("a1").unwrap().y);
    }

    #[test]
    fn no_two_nodes_share_a_position() {
        // A bushy tree.
        let tree = tree_from(&[
            ("root", None, &["a", "b"], NodeKind::Agent),
            ("a", Some("root"), &["a1", "a2"], NodeKind::Agent),
            ("b", Some("root"), &["b1", "b2"], NodeKind::Agent),
            ("a1", Some("a"), &[], NodeKind::Session),
            ("a2", Some("a"), &[], NodeKind::Session),
            ("b1", Some("b"), &[], NodeKind::Session),
            ("b2", Some("b"), &[], NodeKind::Session),
        ]);
        let l = layout(&tree);
        let mut seen = std::collections::HashSet::new();
        for (id, pos) in &l.positions {
            let key = (pos.x.to_bits(), pos.y.to_bits());
            assert!(seen.insert(key), "node {id} duplicates position {pos:?}");
        }
    }

    #[test]
    fn screen_layout_scales_to_area() {
        let tree = tree_from(&[
            ("root", None, &["a", "b"], NodeKind::Agent),
            ("a", Some("root"), &[], NodeKind::Session),
            ("b", Some("root"), &[], NodeKind::Session),
        ]);
        let l = layout(&tree);
        let screen = to_screen(&l, 80, 24, 20);
        let (rc, _rr) = screen.cells["root"];
        let (ac, ar) = screen.cells["a"];
        let (bc, br) = screen.cells["b"];
        // Root is at depth 0, children at depth 1 → children are to the right.
        assert!(ac > rc);
        assert!(bc > rc);
        // Leaves a, b are at distinct rows.
        assert_ne!(ar, br);
        // All within bounds.
        for (_, &(_, row)) in &screen.cells {
            assert!(row < 24);
        }
    }

    #[test]
    fn connectors_one_per_edge() {
        let tree = tree_from(&[
            ("root", None, &["a", "b"], NodeKind::Agent),
            ("a", Some("root"), &[], NodeKind::Session),
            ("b", Some("root"), &[], NodeKind::Session),
        ]);
        let l = layout(&tree);
        let screen = to_screen(&l, 80, 24, 0);
        let conns = connectors(&tree, &screen);
        assert_eq!(conns.len(), 2); // root→a, root→b
        // Each connector references parent and child.
        for c in &conns {
            assert_eq!(c.parent_id, "root");
            assert!(c.child_id == "a" || c.child_id == "b");
        }
    }

    #[test]
    fn layout_matches_disk_loaded_tree() {
        // The layout should work identically on a Tree loaded from disk
        // via TreeStore (the real path) as on one built in-memory.
        let dir = tempfile::tempdir().unwrap();
        let nodes_dir = dir.path().join("nodes");
        std::fs::create_dir_all(&nodes_dir).unwrap();
        let now = crate::tree_store::unix_now();
        let mk = |id: &str, parent: Option<&str>, children: &[&str], kind: NodeKind| {
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
                state: NodeState::Idle,
                config: crate::tree_store::Config::default(),
                openclaw_sessions: Vec::new(),
                pi_sessions: Vec::new(),
                created_at: now,
                last_pulled: now,
                children: children.iter().map(|s| s.to_string()).collect(),
            };
            std::fs::write(
                nodes_dir.join(format!("{id}.json")),
                serde_json::to_string_pretty(&n).unwrap(),
            )
            .unwrap();
        };
        mk("root", None, &["a", "b"], NodeKind::Agent);
        mk("a", Some("root"), &[], NodeKind::Session);
        mk("b", Some("root"), &[], NodeKind::Session);
        std::fs::write(
            dir.path().join("index.json"),
            serde_json::to_string_pretty(&crate::tree_store::Index {
                root_id: "root".to_string(),
                version: crate::tree_store::SCHEMA_VERSION,
                updated_at: now,
            })
            .unwrap(),
        )
        .unwrap();
        let store = TreeStore::open(dir.path());
        let tree = store.load_tree();
        let l = layout(&tree);
        // Same invariant: root centers over a and b.
        let ya = l.get("a").unwrap().y;
        let yb = l.get("b").unwrap().y;
        let yr = l.get("root").unwrap().y;
        assert!((yr - (ya + yb) / 2.0).abs() < 1e-9);
    }
}
