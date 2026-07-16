//! Call-hierarchy tree state: a lazily-expanded arena of callers (or callees)
//! around a symbol, backed by the LSP call-hierarchy API. Each node fetches its
//! children only when expanded; a node whose callable already appears among its
//! ancestors is marked cyclic and never expanded further, so recursion can't
//! loop forever.

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
}

#[derive(Debug, Clone)]
pub struct CallTree {
    pub direction: Direction,
    /// Language of the server used for every fetch in this tree.
    pub lang: &'static str,
    pub root_name: String,
    /// A file this tree references changed on disk — the tree may be out of date.
    pub stale: bool,
    nodes: Vec<Node>,
    roots: Vec<usize>,
}

impl CallTree {
    pub fn new(direction: Direction, lang: &'static str, roots: Vec<CallItem>) -> Self {
        let root_name = roots.first().map(|i| i.name.clone()).unwrap_or_default();
        let mut tree = CallTree {
            direction,
            lang,
            root_name,
            stale: false,
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

    pub fn node(&self, id: usize) -> &Node {
        &self.nodes[id]
    }

    fn push(&mut self, item: CallItem, depth: usize, parent: Option<usize>) -> usize {
        let cyclic = self.is_ancestor(parent, &item);
        let id = self.nodes.len();
        self.nodes.push(Node {
            item,
            depth,
            parent,
            children: None,
            expanded: false,
            loading: false,
            cyclic,
        });
        id
    }

    fn is_ancestor(&self, mut parent: Option<usize>, item: &CallItem) -> bool {
        while let Some(p) = parent {
            let n = &self.nodes[p];
            if n.item.path == item.path && n.item.line == item.line && n.item.name == item.name {
                return true;
            }
            parent = n.parent;
        }
        false
    }

    /// Attach freshly fetched children to a node and expand it. A no-op (beyond
    /// re-expanding) if the children were already loaded, so a duplicate fetch
    /// can't orphan or double the arena.
    pub fn set_children(&mut self, id: usize, items: Vec<CallItem>) {
        self.nodes[id].loading = false;
        if self.nodes[id].children.is_some() {
            self.nodes[id].expanded = true;
            return;
        }
        let depth = self.nodes[id].depth + 1;
        let child_ids: Vec<usize> = items
            .into_iter()
            .map(|item| self.push(item, depth, Some(id)))
            .collect();
        self.nodes[id].children = Some(child_ids);
        self.nodes[id].expanded = true;
    }

    /// The raw LSP item to pass to incoming/outgoing when expanding `id`.
    pub fn raw_of(&self, id: usize) -> serde_json::Value {
        self.nodes[id].item.raw.clone()
    }

    /// Whether `id` still needs its children fetched (not a cyclic leaf).
    pub fn needs_fetch(&self, id: usize) -> bool {
        let n = &self.nodes[id];
        n.children.is_none() && !n.cyclic
    }

    /// Mark a node as having a fetch in flight (for the loading indicator).
    pub fn set_loading(&mut self, id: usize) {
        self.nodes[id].loading = true;
    }

    /// Whether any node in the tree references `path` — i.e. a change to `path`
    /// could make this call hierarchy out of date.
    pub fn depends_on(&self, path: &std::path::Path) -> bool {
        self.nodes.iter().any(|n| n.item.path == path)
    }

    /// Collapse/expand a node whose children are already loaded.
    pub fn toggle(&mut self, id: usize) {
        if self.nodes[id].children.is_some() {
            self.nodes[id].expanded = !self.nodes[id].expanded;
        }
    }

    /// Node ids in display order (depth-first; children only under expanded nodes).
    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for &r in &self.roots {
            self.walk(r, &mut out);
        }
        out
    }

    fn walk(&self, id: usize, out: &mut Vec<usize>) {
        out.push(id);
        let n = &self.nodes[id];
        if n.expanded && let Some(children) = &n.children {
            for &c in children {
                self.walk(c, out);
            }
        }
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
        let mut t = CallTree::new(Direction::Incoming, "rust", vec![item("root", 1)]);
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
    fn recursion_is_marked_cyclic_not_infinite() {
        let mut t = CallTree::new(Direction::Incoming, "rust", vec![item("f", 1)]);
        let root = t.roots()[0];
        // f calls f (same path/line/name) → the child is cyclic and won't fetch.
        t.set_children(root, vec![item("f", 1)]);
        let child = t.visible()[1];
        assert!(t.node(child).cyclic);
        assert!(!t.needs_fetch(child));
    }
}
