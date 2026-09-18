// tree_store.rs — Read-only client for the centralized tree state store.
//
// The store lives on disk at ~/.orchestra/tree/::
//
//     index.json              { root_id, version, updated_at }
//     nodes/<id>.json         one Node record per file
//
// Only the State Collector (orchestra-collector.py) writes. The TUI is a
// pure reader — it never mutates state directly. User-initiated actions
// (spawn, teardown, rename) go through the collector's HTTP endpoint so the
// store stays the single source of truth.
//
// Layout per Design_Document.md §5. The schema is versioned
// (`schema_version: 1`); unknown fields pass through so additive config
// changes don't break old TUI binaries.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Default store location. Overridable via `TreeStore::open` for tests.
pub const DEFAULT_STORE_DIR: &str = "/home/sky/.orchestra/tree";

/// A node is stale if no successful collector pull has landed in this long.
/// 90s = 3 missed 30s pulls (Design §10).
pub const STALE_THRESHOLD_SECS: u64 = 90;

/// Schema version of the on-disk node record. Bump when the wire format
/// changes in a backwards-incompatible way. Additive changes (new optional
/// fields) do NOT require a bump — serde ignores unknown fields.
#[allow(dead_code)]
pub const SCHEMA_VERSION: u32 = 1;

/// `index.json` — points at the root node and records the store version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub root_id: String,
    pub version: u32,
    pub updated_at: u64,
}

/// A single node in the agent/session hierarchy.
///
/// `kind` discriminates agents (SkyPilot clusters) from sessions (tmux
/// sessions on a parent agent's box). Edge rules are enforced by the
/// collector; the store just records what it was told.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub parent_id: Option<String>,
    pub name: String,
    /// User-set display name; `None` means "use `name`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// SkyPilot cluster name (agents only). 1:1 with `id` by convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sky_cluster: Option<String>,
    /// tmux session name (sessions only). 1:1 with `id` by convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session: Option<String>,
    /// DNS/local name for bridge + SSH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub box_host: Option<String>,
    /// A2A bridge URL (`http://<host>:8080/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_url: Option<String>,
    /// OpenClaw gateway WS URL — same-box only, not cross-box reachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_url: Option<String>,
    pub state: NodeState,
    #[serde(default)]
    pub config: Config,
    /// OpenClaw-managed session ids on this agent's box.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub openclaw_sessions: Vec<String>,
    /// pi tmux session ids on this agent's box.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pi_sessions: Vec<String>,
    pub created_at: u64,
    /// Last successful collector pull (unix seconds). Drives stale detection.
    #[serde(default)]
    pub last_pulled: u64,
    /// Child node ids. Edges are owned by the parent (Design §5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Agent,
    Session,
}

/// Agent/session lifecycle state. Drives the dot color in Tree View.
///
/// `Stale` is computed by the reader (not persisted as-is by the collector)
/// — if `last_pulled` is older than `STALE_THRESHOLD_SECS`, `effective_state`
/// reports `Stale` regardless of the on-disk `state`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Initializing,
    Working,
    Idle,
    NeedsInput,
    Completed,
    Failed,
    Stale,
    /// `sky status` saw the cluster but no node record exists yet. Only the
    /// collector writes this; the TUI renders it like `Initializing`.
    Unknown,
}

/// Agent config (Design §7). All fields optional with defaults — an
/// unspecified agent is "just another main-box". Unknown fields pass
/// through in `extras` so additive schema changes don't break old TUIs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// off|minimal|low|medium|high|xhigh
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// guided|semi|full
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomy: Option<String>,
    /// low|normal|high|urgent
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub openclaw_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_interval_s: Option<u32>,
    /// 0 = never auto-teardown (persistent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_s: Option<u32>,
    /// Unknown config fields pass through here so an agent can read its own
    /// config at runtime (Design §7 extensibility).
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<u32>,
}

impl Node {
    /// The name to show in the UI: `display_name` if set, else `name`.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// State accounting for staleness. If `last_pulled` is older than the
    /// threshold, the node is `Stale` regardless of its on-disk `state`.
    /// `Unknown` is sticky (the collector hasn't classified it yet).
    pub fn effective_state(&self, now: u64) -> NodeState {
        match self.state {
            NodeState::Unknown => NodeState::Unknown,
            other => {
                if self.is_stale(now) {
                    NodeState::Stale
                } else {
                    other
                }
            }
        }
    }

    /// True if no successful collector pull has landed within the stale
    /// threshold. Nodes with `last_pulled == 0` (never pulled) are NOT
    /// stale — they're `Initializing` until the first pull or `Failed`
    /// if `sky status` says the cluster is gone.
    pub fn is_stale(&self, now: u64) -> bool {
        self.last_pulled != 0 && now.saturating_sub(self.last_pulled) > STALE_THRESHOLD_SECS
    }
}

/// Read-only view over the on-disk tree store. Cheap to construct; reads
/// files on demand. The TUI rebuilds a `Tree` from disk every 100ms poll
/// (Design §9) — a few dozen small JSON files, so this is fast enough.
#[derive(Debug, Clone)]
pub struct TreeStore {
    dir: PathBuf,
}

impl TreeStore {
    /// Open the default store at `~/.orchestra/tree/`.
    pub fn default_dir() -> Self {
        Self {
            dir: PathBuf::from(DEFAULT_STORE_DIR),
        }
    }

    /// Open a store at an explicit path (used by tests + the `--store` flag).
    #[allow(dead_code)]
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    /// Load `index.json`. Returns `None` if the store doesn't exist yet
    /// (fresh main-box with no collector run).
    pub fn load_index(&self) -> Option<Index> {
        let path = self.dir.join("index.json");
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Load a single node by id. Returns `None` if missing or unparseable.
    #[allow(dead_code)]
    pub fn load_node(&self, id: &str) -> Option<Node> {
        let path = self.dir.join("nodes").join(format!("{id}.json"));
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Load every node in the store. Nodes with corrupt JSON are skipped
    /// (the collector re-writes them on the next pull). Returns an empty
    /// vec if the store doesn't exist yet.
    pub fn load_all_nodes(&self) -> Vec<Node> {
        let mut nodes = Vec::new();
        let dir = self.dir.join("nodes");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return nodes;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(node) = serde_json::from_str::<Node>(&content) {
                nodes.push(node);
            }
        }
        nodes
    }

    /// Build the full tree: index + all nodes, with `effective_state`
    /// applied. This is what the TUI renderer wants.
    pub fn load_tree(&self) -> Tree {
        let now = unix_now();
        let index = self.load_index();
        let root_id = index.as_ref().map(|i| i.root_id.clone());
        let nodes = self
            .load_all_nodes()
            .into_iter()
            .map(|n| {
                let eff = n.effective_state(now);
                (n.id.clone(), NodeView { raw: n, effective: eff })
            })
            .collect();
        Tree {
            root_id,
            nodes,
            updated_at: index.map(|i| i.updated_at).unwrap_or(0),
        }
    }
}

/// A node with its staleness-adjusted state precomputed at load time.
/// The renderer reads `effective` rather than `raw.state` so dimmed/stale
/// rendering is consistent across the whole tree.
#[derive(Debug, Clone)]
pub struct NodeView {
    pub raw: Node,
    pub effective: NodeState,
}

/// The full tree, ready to render. Built by `TreeStore::load_tree`.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    /// `None` if the store has no index yet (fresh main-box).
    pub root_id: Option<String>,
    /// Node id → view. Not ordered; the layout engine walks from `root_id`.
    pub nodes: std::collections::HashMap<String, NodeView>,
    pub updated_at: u64,
}

impl Tree {
    /// True if the store exists and has at least a root node.
    pub fn has_root(&self) -> bool {
        self.root_id
            .as_ref()
            .map(|id| self.nodes.contains_key(id))
            .unwrap_or(false)
    }

    /// Get a node view by id.
    pub fn get(&self, id: &str) -> Option<&NodeView> {
        self.nodes.get(id)
    }

    /// Children of a node, in declared order. Missing child ids are
    /// skipped (the collector may not have written them yet).
    pub fn children(&self, id: &str) -> Vec<&NodeView> {
        let Some(parent) = self.nodes.get(id) else {
            return Vec::new();
        };
        parent
            .raw
            .children
            .iter()
            .filter_map(|cid| self.nodes.get(cid))
            .collect()
    }

    /// Walk the tree depth-first from `root_id` (or the store's root).
    /// Used by the layout engine + renderer. Returns ids in DFS order.
    pub fn dfs(&self) -> Vec<String> {
        let Some(root) = self.root_id.clone() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            out.push(id.clone());
            // Push children in reverse so the leftmost child is visited
            // first (stack is LIFO).
            if let Some(node) = self.nodes.get(&id) {
                for child in node.raw.children.iter().rev() {
                    stack.push(child.clone());
                }
            }
        }
        out
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write a node + index into a temp store dir, returning the store.
    fn store_with(nodes: &[Node], root_id: &str) -> (TreeStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let nodes_dir = dir.path().join("nodes");
        fs::create_dir_all(&nodes_dir).unwrap();
        for n in nodes {
            let path = nodes_dir.join(format!("{}.json", n.id));
            fs::write(&path, serde_json::to_string_pretty(n).unwrap()).unwrap();
        }
        let index = Index {
            root_id: root_id.to_string(),
            version: SCHEMA_VERSION,
            updated_at: unix_now(),
        };
        fs::write(
            dir.path().join("index.json"),
            serde_json::to_string_pretty(&index).unwrap(),
        )
        .unwrap();
        (TreeStore::open(dir.path()), dir)
    }

    fn agent(id: &str, parent: Option<&str>, children: &[&str]) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Agent,
            parent_id: parent.map(|s| s.to_string()),
            name: id.trim_start_matches("agent-").to_string(),
            display_name: None,
            sky_cluster: Some(id.to_string()),
            tmux_session: None,
            box_host: Some(id.to_string()),
            bridge_url: Some(format!("http://{id}:8080/")),
            gateway_url: Some("ws://localhost:18789".to_string()),
            state: NodeState::Idle,
            config: Config::default(),
            openclaw_sessions: Vec::new(),
            pi_sessions: Vec::new(),
            created_at: 1719300000,
            last_pulled: unix_now(),
            children: children.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn session(id: &str, parent: &str) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Session,
            parent_id: Some(parent.to_string()),
            name: id.trim_start_matches("session-").to_string(),
            display_name: None,
            sky_cluster: None,
            tmux_session: Some(id.to_string()),
            box_host: None,
            bridge_url: None,
            gateway_url: None,
            state: NodeState::Working,
            config: Config::default(),
            openclaw_sessions: Vec::new(),
            pi_sessions: Vec::new(),
            created_at: 1719300000,
            last_pulled: unix_now(),
            children: Vec::new(),
        }
    }

    #[test]
    fn loads_index_and_nodes() {
        let (store, _dir) = store_with(
            &[
                agent("agent-main-box", None, &["agent-research-box", "session-fix-8824"]),
                agent("agent-research-box", Some("agent-main-box"), &[]),
                session("session-fix-8824", "agent-main-box"),
            ],
            "agent-main-box",
        );

        let idx = store.load_index().unwrap();
        assert_eq!(idx.root_id, "agent-main-box");
        assert_eq!(idx.version, SCHEMA_VERSION);

        let tree = store.load_tree();
        assert!(tree.has_root());
        assert_eq!(tree.nodes.len(), 3);
        assert!(tree.get("agent-research-box").is_some());
    }

    #[test]
    fn empty_store_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = TreeStore::open(dir.path());
        assert!(store.load_index().is_none());
        assert!(store.load_all_nodes().is_empty());
        let tree = store.load_tree();
        assert!(!tree.has_root());
        assert!(tree.dfs().is_empty());
    }

    #[test]
    fn corrupt_node_json_is_skipped() {
        let (store, dir) = store_with(&[agent("agent-main-box", None, &[])], "agent-main-box");
        // Write a corrupt node file alongside the good ones.
        fs::write(
            dir.path().join("nodes").join("garbage.json"),
            "not valid json {{{",
        )
        .unwrap();
        let nodes = store.load_all_nodes();
        assert_eq!(nodes.len(), 1); // only the good one
        assert_eq!(nodes[0].id, "agent-main-box");
    }

    #[test]
    fn dfs_visits_parents_before_children() {
        let (store, _dir) = store_with(
            &[
                agent("root", None, &["a", "b"]),
                agent("a", Some("root"), &["a1"]),
                agent("b", Some("root"), &[]),
                session("a1", "a"),
            ],
            "root",
        );
        let tree = store.load_tree();
        let order = tree.dfs();
        // Root first, then each subtree fully before the next sibling.
        let root_pos = order.iter().position(|x| x == "root").unwrap();
        let a_pos = order.iter().position(|x| x == "a").unwrap();
        let a1_pos = order.iter().position(|x| x == "a1").unwrap();
        let b_pos = order.iter().position(|x| x == "b").unwrap();
        assert!(root_pos < a_pos);
        assert!(a_pos < a1_pos); // a1 is inside a's subtree
        // b comes after a's entire subtree (DFS).
        assert!(a1_pos < b_pos);
    }

    #[test]
    fn children_preserves_declared_order() {
        let (store, _dir) = store_with(
            &[
                agent("root", None, &["c3", "c1", "c2"]),
                agent("c1", Some("root"), &[]),
                agent("c2", Some("root"), &[]),
                agent("c3", Some("root"), &[]),
            ],
            "root",
        );
        let tree = store.load_tree();
        let kids: Vec<&str> = tree.children("root").iter().map(|v| v.raw.id.as_str()).collect();
        assert_eq!(kids, vec!["c3", "c1", "c2"]);
    }

    #[test]
    fn missing_children_are_skipped() {
        // root declares a child that has no node file yet.
        let (store, _dir) = store_with(&[agent("root", None, &["ghost", "real"]), agent("real", Some("root"), &[])], "root");
        let tree = store.load_tree();
        let kids: Vec<&str> = tree.children("root").iter().map(|v| v.raw.id.as_str()).collect();
        assert_eq!(kids, vec!["real"]);
    }

    #[test]
    fn stale_node_reports_stale_effective_state() {
        let now = unix_now();
        let mut n = agent("agent-x", None, &[]);
        // Last pull was 2 minutes ago — beyond the 90s threshold.
        n.last_pulled = now.saturating_sub(120);
        n.state = NodeState::Idle;
        assert_eq!(n.effective_state(now), NodeState::Stale);
    }

    #[test]
    fn fresh_node_keeps_its_state() {
        let now = unix_now();
        let mut n = agent("agent-x", None, &[]);
        n.last_pulled = now.saturating_sub(10);
        n.state = NodeState::Working;
        assert_eq!(n.effective_state(now), NodeState::Working);
    }

    #[test]
    fn never_pulled_node_is_not_stale() {
        // last_pulled == 0 means "never successfully pulled" — the node is
        // Initializing, not Stale. Stale is for "was alive, now silent".
        let now = unix_now();
        let mut n = agent("agent-x", None, &[]);
        n.last_pulled = 0;
        n.state = NodeState::Initializing;
        assert_eq!(n.effective_state(now), NodeState::Initializing);
    }

    #[test]
    fn unknown_state_is_sticky() {
        let now = unix_now();
        let mut n = agent("agent-x", None, &[]);
        n.last_pulled = now.saturating_sub(10); // fresh
        n.state = NodeState::Unknown;
        assert_eq!(n.effective_state(now), NodeState::Unknown);
    }

    #[test]
    fn label_falls_back_to_name_when_no_display_name() {
        let n = agent("agent-research-box", None, &[]);
        assert_eq!(n.label(), "research-box");
    }

    #[test]
    fn label_uses_display_name_when_set() {
        let mut n = agent("agent-research-box", None, &[]);
        n.display_name = Some("My Researcher".to_string());
        assert_eq!(n.label(), "My Researcher");
    }

    #[test]
    fn unknown_config_fields_pass_through() {
        let json = r#"{
            "id": "agent-x",
            "kind": "agent",
            "name": "x",
            "state": "idle",
            "created_at": 0,
            "last_pulled": 0,
            "config": { "model": "my-model", "max_subagents": 5, "custom_flag": true }
        }"#;
        let n: Node = serde_json::from_str(json).unwrap();
        assert_eq!(n.config.model.as_deref(), Some("my-model"));
        // Unknown fields land in extras.
        assert_eq!(n.config.extras.len(), 2);
        assert_eq!(n.config.extras["max_subagents"], serde_json::json!(5));
        assert_eq!(n.config.extras["custom_flag"], serde_json::json!(true));
    }

    #[test]
    fn node_round_trips_through_json() {
        let n = agent("agent-round", Some("agent-main-box"), &["session-foo"]);
        let s = serde_json::to_string(&n).unwrap();
        let back: Node = serde_json::from_str(&s).unwrap();
        assert_eq!(n, back);
    }

    #[test]
    fn missing_optional_fields_default() {
        // Minimal valid node — only required fields.
        let json = r#"{
            "id": "agent-min",
            "kind": "agent",
            "name": "min",
            "state": "initializing",
            "created_at": 1
        }"#;
        let n: Node = serde_json::from_str(json).unwrap();
        assert_eq!(n.parent_id, None);
        assert_eq!(n.children, Vec::<String>::new());
        assert_eq!(n.last_pulled, 0);
        assert!(n.sky_cluster.is_none());
    }

    #[test]
    fn state_enum_serializes_snake_case() {
        for s in [
            NodeState::Initializing,
            NodeState::NeedsInput,
            NodeState::Completed,
            NodeState::Unknown,
        ] {
            let v = serde_json::to_value(s).unwrap();
            let back: NodeState = serde_json::from_value(v).unwrap();
            assert_eq!(s, back);
        }
        // Spot-check the exact wire string.
        assert_eq!(
            serde_json::to_string(&NodeState::NeedsInput).unwrap(),
            "\"needs_input\""
        );
    }
}
