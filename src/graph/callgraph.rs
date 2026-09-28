//! Call-hierarchy tree state: a lazily-expanded arena of callers (or callees)
//! around a symbol, backed by the LSP call-hierarchy API. Each node fetches its
//! children only when expanded; a node whose callable already appears among its
//! ancestors is marked cyclic and never expanded further, so recursion can't
//! loop forever.

use crate::graph::tree::{self, TreeNode};
use crate::lsp::client::CallItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Who calls this symbol.
    Incoming,
    /// What this symbol calls.
    Outgoing,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::Incoming => "Callers",
            Direction::Outgoing => "Callees",
        }
    }

    pub fn toggled(self) -> Direction {
        match self {
            Direction::Incoming => Direction::Outgoing,
            Direction::Outgoing => Direction::Incoming,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub item: CallItem,
    pub depth: usize,
    pub parent: Option<usize>,
    /// `None` until this node's children have been fetched.
    pub children: Option<Vec<usize>>,
    pub expanded: bool,
    /// A fetch of this node's children is in flight.
    pub loading: bool,
    /// The callable already appears on the ancestor path — treated as a leaf.
    pub cyclic: bool,
    /// Callers/callees the server reported that are NOT shown because the
    /// tree reached [`MAX_NODES`]. Non-zero means "more exist" — without it a
    /// node fetched once the arena was full read as "no callers".
    pub hidden: usize,
}

impl TreeNode for Node {
    fn parent(&self) -> Option<usize> {
        self.parent
    }
    fn children(&self) -> Option<&[usize]> {
        self.children.as_deref()
    }
    fn expanded(&self) -> bool {
        self.expanded
    }
}

#[derive(Debug, Clone)]
pub struct CallTree {
    /// Identity of this tree (minted from `App::call_token`). Child fetches
    /// carry it back; node ids are bare indices, so a result may only attach
    /// to the exact tree it was requested from.
    pub token: u64,
    pub direction: Direction,
    /// Language of the server used for every fetch in this tree.
    pub lang: &'static str,
    pub root_name: String,
    /// A file this tree references changed on disk — the tree may be out of date.
    pub stale: bool,
    /// "Expand all": newly fetched children are auto-expanded recursively (up to
    /// the project boundary / a node cap).
    pub full: bool,
    nodes: Vec<Node>,
    roots: Vec<usize>,
}

/// Cap on tree size so "Expand all" on a hub symbol can't fan out unboundedly.
pub const MAX_NODES: usize = 800;

impl CallTree {
    pub fn new(token: u64, direction: Direction, lang: &'static str, roots: Vec<CallItem>) -> Self {
        let root_name = roots.first().map(|i| i.name.clone()).unwrap_or_default();
        let mut tree = CallTree {
            token,
            direction,
            lang,
            root_name,
            stale: false,
            full: false,
            nodes: Vec::new(),
            roots: Vec::new(),
        };
        for item in roots {
            let id = tree.push(item, 0, None);
            tree.roots.push(id);
        }
        tree
    }

    pub fn roots(&self) -> &[usize] {
        &self.roots
    }

    /// Node `id`. Panics on an id this tree never issued; message handlers
    /// that carry ids across a possible tree swap use [`CallTree::get`].
    pub fn node(&self, id: usize) -> &Node {
        &self.nodes[id]
    }

    /// Node `id`, or `None` for a stale id (from a tree since replaced).
    pub fn get(&self, id: usize) -> Option<&Node> {
        self.nodes.get(id)
    }

    fn push(&mut self, item: CallItem, depth: usize, parent: Option<usize>) -> usize {
        let cyclic = tree::any_on_path(&self.nodes, parent, |n| {
            n.item.path == item.path && n.item.line == item.line && n.item.name == item.name
        });
        let id = self.nodes.len();
        self.nodes.push(Node {
            item,
            depth,
            parent,
            children: None,
            expanded: false,
            loading: false,
            cyclic,
            hidden: 0,
        });
        id
    }

    /// Attach freshly fetched children to a node and expand it. A no-op (beyond
    /// re-expanding) if the children were already loaded, so a duplicate fetch
    /// can't orphan or double the arena — and for a stale `id`.
    /// Returns the ids of the newly created children (empty if already loaded),
    /// so an "expand all" walk can recurse into them.
    ///
    /// Children past [`MAX_NODES`] are counted in [`Node::hidden`]. When the
    /// arena is already full the node stays UNFETCHED (not "no callers"): the
    /// UI shows how many exist, and nothing claims the symbol has none.
    pub fn set_children(&mut self, id: usize, items: Vec<CallItem>) -> Vec<usize> {
        let Some(node) = self.nodes.get_mut(id) else {
            return Vec::new();
        };
        node.loading = false;
        if node.children.is_some() {
            node.expanded = true;
            return Vec::new();
        }
        // Enforce the cap per batch, not just between batches: one response
        // for a hub symbol can carry thousands of callers, and the arena must
        // never exceed MAX_NODES no matter how it grows.
        let room = MAX_NODES.saturating_sub(self.nodes.len());
        let hidden = items.len().saturating_sub(room);
        if room == 0 && hidden > 0 {
            self.nodes[id].hidden = hidden;
            return Vec::new();
        }
        let depth = self.nodes[id].depth + 1;
        let child_ids: Vec<usize> = items
            .into_iter()
            .take(room)
            .map(|item| self.push(item, depth, Some(id)))
            .collect();
        let node = &mut self.nodes[id];
        node.children = Some(child_ids.clone());
        node.expanded = true;
        node.hidden = hidden;
        child_ids
    }

    /// "12 more callers not shown" for a node the size cap cut short.
    pub fn hidden_note(&self, id: usize) -> Option<String> {
        let hidden = self.get(id)?.hidden;
        (hidden > 0).then(|| {
            let what = match self.direction {
                Direction::Incoming => "callers",
                Direction::Outgoing => "callees",
            };
            format!("{hidden} more {what} not shown (tree limit {MAX_NODES})")
        })
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// All currently-visible nodes that still need their children fetched — the
    /// frontier "Expand all" kicks off from.
    pub fn unfetched_frontier(&self) -> Vec<usize> {
        self.visible()
            .into_iter()
            .filter(|&id| self.needs_fetch(id))
            .collect()
    }

    /// The raw LSP item to pass to incoming/outgoing when expanding `id`
    /// (`Null` for a stale id).
    pub fn raw_of(&self, id: usize) -> serde_json::Value {
        self.get(id)
            .map(|n| n.item.raw.clone())
            .unwrap_or(serde_json::Value::Null)
    }

    /// Whether `id` still needs its children fetched (not a cyclic leaf;
    /// false for a stale id).
    pub fn needs_fetch(&self, id: usize) -> bool {
        self.get(id)
            .is_some_and(|n| n.children.is_none() && !n.cyclic)
    }

    /// Mark a node as having a fetch in flight (for the loading indicator).
    pub fn set_loading(&mut self, id: usize) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.loading = true;
        }
    }

    /// Whether any node in the tree references `path` — i.e. a change to `path`
    /// could make this call hierarchy out of date.
    pub fn depends_on(&self, path: &std::path::Path) -> bool {
        self.nodes.iter().any(|n| n.item.path == path)
    }

    /// Collapse/expand a node whose children are already loaded.
    pub fn toggle(&mut self, id: usize) {
        if let Some(n) = self.nodes.get_mut(id)
            && n.children.is_some()
        {
            n.expanded = !n.expanded;
        }
    }

    /// Node ids in display order (depth-first; children only under expanded nodes).
    pub fn visible(&self) -> Vec<usize> {
        tree::visible(&self.nodes, &self.roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(name: &str, line: usize) -> CallItem {
        CallItem {
            name: name.into(),
            detail: String::new(),
            kind: 12,
            path: std::path::PathBuf::from("/p/x.rs"),
            line,
            character: 0,
            raw: json!({}),
        }
    }

    #[test]
    fn expands_and_flattens_in_order() {
        let mut t = CallTree::new(1, Direction::Incoming, "rust", vec![item("root", 1)]);
        assert_eq!(t.visible(), t.roots().to_vec());
        let root = t.roots()[0];
        assert!(t.needs_fetch(root));

        t.set_children(root, vec![item("a", 2), item("b", 3)]);
        // root + its two children, in order.
        assert_eq!(t.visible().len(), 3);
        assert_eq!(t.node(t.visible()[1]).item.name, "a");

        // Collapsing hides the children.
        t.toggle(root);
        assert_eq!(t.visible(), t.roots().to_vec());
    }

    #[test]
    fn frontier_tracks_unfetched_visible_nodes() {
        let mut t = CallTree::new(1, Direction::Incoming, "rust", vec![item("root", 1)]);
        let root = t.roots()[0];
        assert_eq!(t.unfetched_frontier(), vec![root]);

        let kids = t.set_children(root, vec![item("a", 2), item("b", 3)]);
        assert_eq!(kids.len(), 2);
        // Both children are now the unfetched frontier.
        assert_eq!(t.unfetched_frontier(), kids);

        // Fetching `a` (it has no project-internal callers) drops it from the
        // frontier; only `b` remains to expand.
        assert!(t.set_children(kids[0], vec![]).is_empty());
        assert_eq!(t.unfetched_frontier(), vec![kids[1]]);
    }

    #[test]
    fn one_batch_cannot_exceed_max_nodes() {
        // A hub symbol's single response can carry thousands of callers; the
        // arena caps per batch, not just between batches.
        let mut t = CallTree::new(1, Direction::Incoming, "rust", vec![item("root", 1)]);
        let root = t.roots()[0];
        let many: Vec<CallItem> = (0..MAX_NODES * 2)
            .map(|i| item(&format!("f{i}"), i + 2))
            .collect();
        let kids = t.set_children(root, many);
        assert!(t.node_count() <= MAX_NODES, "count {}", t.node_count());
        assert_eq!(kids.len(), MAX_NODES - 1); // root already occupied one slot
    }

    /// Whatever the cap cuts is counted on the node, and a node fetched once
    /// the arena is full stays unfetched instead of claiming "no callers".
    #[test]
    fn the_cap_is_visible_and_never_reads_as_no_callers() {
        let mut t = CallTree::new(1, Direction::Incoming, "rust", vec![item("root", 1)]);
        let root = t.roots()[0];
        let many: Vec<CallItem> = (0..MAX_NODES + 10)
            .map(|i| item(&format!("f{i}"), i + 2))
            .collect();
        let kids = t.set_children(root, many);
        assert_eq!(
            t.node(root).hidden,
            11,
            "10 over the cap plus the root's slot"
        );
        assert!(
            t.hidden_note(root)
                .unwrap()
                .starts_with("11 more callers not shown")
        );
        // The arena is full: a child fetched now keeps needing a fetch and
        // says how many callers exist.
        let child = kids[0];
        assert!(
            t.set_children(child, vec![item("g", 9000), item("h", 9001)])
                .is_empty()
        );
        assert!(
            t.node(child).children.is_none(),
            "not recorded as 'no callers'"
        );
        assert!(t.needs_fetch(child));
        assert_eq!(t.node(child).hidden, 2);
        // An ordinary node shows no note.
        assert_eq!(t.hidden_note(kids[1]), None);
    }

    /// Ids carried in messages can outlive the tree they came from (a new
    /// hierarchy replaces it); every id-taking method tolerates that.
    #[test]
    fn stale_ids_are_ignored_not_panicked_on() {
        let mut t = CallTree::new(1, Direction::Incoming, "rust", vec![item("root", 1)]);
        let stale = 99;
        assert!(t.get(stale).is_none());
        assert!(!t.needs_fetch(stale));
        assert_eq!(t.raw_of(stale), serde_json::Value::Null);
        t.set_loading(stale);
        t.toggle(stale);
        assert!(t.set_children(stale, vec![item("x", 2)]).is_empty());
        assert_eq!(t.node_count(), 1);
        assert_eq!(t.hidden_note(stale), None);
    }

    #[test]
    fn recursion_is_marked_cyclic_not_infinite() {
        let mut t = CallTree::new(1, Direction::Incoming, "rust", vec![item("f", 1)]);
        let root = t.roots()[0];
        // f calls f (same path/line/name) → the child is cyclic and won't fetch.
        t.set_children(root, vec![item("f", 1)]);
        let child = t.visible()[1];
        assert!(t.node(child).cyclic);
        assert!(!t.needs_fetch(child));
    }
}
