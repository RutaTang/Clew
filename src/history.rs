//! Tree-structured navigation history over (file, line) locations.
//!
//! Unlike a linear back/forward stack, backtracking and then navigating
//! elsewhere *branches* rather than discarding the old forward path — so an
//! exploration you backed out of is never lost. `forward` follows the branch you
//! most recently took; the others stay reachable from the history tree view.
//! The tree is persisted per-project (`<root>/.clew/history.json`, relative
//! paths) so a reading session survives a restart.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Safety cap on the persisted tree; a session that somehow exceeds it starts
/// fresh rather than growing without bound.
const MAX_NODES: usize = 800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loc {
    pub path: PathBuf,
    pub line: Option<usize>,
}

#[derive(Debug, Clone)]
struct Node {
    loc: Loc,
    parent: Option<usize>,
    children: Vec<usize>,
    /// The child `forward` returns to — the most recently taken branch.
    preferred: Option<usize>,
}

#[derive(Debug, Default)]
pub struct History {
    nodes: Vec<Node>,
    current: Option<usize>,
}

/// One row of the flattened history tree, for display.
pub struct Visit {
    pub id: usize,
    pub loc: Loc,
    pub depth: usize,
    pub is_current: bool,
    /// True at a fork — this node has more than one child branch.
    pub forks: bool,
}

impl History {
    /// Record a navigation to `loc` as a child of the current node, branching if
    /// the current node already had children. A jump to the current spot is a
    /// no-op; re-taking a branch already present reuses it instead of duplicating.
    pub fn push(&mut self, loc: Loc) {
        if self.nodes.len() >= MAX_NODES {
            self.clear();
        }
        let Some(cur) = self.current else {
            self.nodes.push(Node { loc, parent: None, children: Vec::new(), preferred: None });
            self.current = Some(0);
            return;
        };
        if self.nodes[cur].loc == loc {
            return; // already here
        }
        let existing =
            self.nodes[cur].children.iter().copied().find(|&c| self.nodes[c].loc == loc);
        let child = existing.unwrap_or_else(|| {
            let id = self.nodes.len();
            self.nodes.push(Node { loc, parent: Some(cur), children: Vec::new(), preferred: None });
            self.nodes[cur].children.push(id);
            id
        });
        self.nodes[cur].preferred = Some(child);
        self.current = Some(child);
    }

    pub fn back(&mut self) -> Option<Loc> {
        let cur = self.current?;
        let parent = self.nodes[cur].parent?;
        self.nodes[parent].preferred = Some(cur); // forward returns to where we were
        self.current = Some(parent);
        Some(self.nodes[parent].loc.clone())
    }

    pub fn forward(&mut self) -> Option<Loc> {
        let cur = self.current?;
        let child = self.nodes[cur]
            .preferred
            .or_else(|| self.nodes[cur].children.last().copied())?;
        self.current = Some(child);
        Some(self.nodes[child].loc.clone())
    }

    pub fn can_back(&self) -> bool {
        self.current.and_then(|c| self.nodes[c].parent).is_some()
    }

    pub fn can_forward(&self) -> bool {
        self.current.map(|c| !self.nodes[c].children.is_empty()).unwrap_or(false)
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.current = None;
    }

    /// Jump to an arbitrary node (from the tree view). Makes the path from that
    /// node up to the root `preferred`, so `back`/`forward` stay consistent with
    /// where you landed.
    pub fn goto(&mut self, id: usize) -> Option<Loc> {
        let loc = self.nodes.get(id)?.loc.clone();
        let mut child = id;
        while let Some(parent) = self.nodes[child].parent {
            self.nodes[parent].preferred = Some(child);
            child = parent;
        }
        self.current = Some(id);
        Some(loc)
    }

    /// Depth-first pre-order flattening for display: roots first, each node's
    /// children in first-visited order, with indentation depth.
    pub fn flatten(&self) -> Vec<Visit> {
        let mut out = Vec::new();
        for r in (0..self.nodes.len()).filter(|&i| self.nodes[i].parent.is_none()) {
            self.dfs(r, 0, &mut out);
        }
        out
    }

    fn dfs(&self, id: usize, depth: usize, out: &mut Vec<Visit>) {
        let n = &self.nodes[id];
        out.push(Visit {
            id,
            loc: n.loc.clone(),
            depth,
            is_current: self.current == Some(id),
            forks: n.children.len() > 1,
        });
        for &c in &n.children {
            self.dfs(c, depth + 1, out);
        }
    }

    /// Drop the whole tree if any stored index is out of range (corrupt file).
    fn validate(&mut self) {
        let n = self.nodes.len();
        let in_range = |o: Option<usize>| o.map(|i| i < n).unwrap_or(true);
        let ok = in_range(self.current)
            && self.nodes.iter().all(|nd| {
                in_range(nd.parent)
                    && in_range(nd.preferred)
                    && nd.children.iter().all(|&c| c < n)
            });
        if !ok {
            self.clear();
        }
    }
}

// ------------------------------------------------------------- persistence

#[derive(Serialize, Deserialize)]
struct StoredNode {
    rel: String,
    line: Option<usize>,
    parent: Option<usize>,
    children: Vec<usize>,
    preferred: Option<usize>,
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    nodes: Vec<StoredNode>,
    current: Option<usize>,
}

fn store_path(root: &Path) -> PathBuf {
    root.join(".clew").join("history.json")
}

/// Load the project's navigation tree, converting stored relative paths back to
/// absolute. Returns an empty history on any error / missing file.
pub fn load(root: &Path) -> History {
    let Some(stored) = std::fs::read_to_string(store_path(root))
        .ok()
        .and_then(|s| serde_json::from_str::<Stored>(&s).ok())
    else {
        return History::default();
    };
    let nodes = stored
        .nodes
        .into_iter()
        .map(|n| Node {
            loc: Loc { path: root.join(&n.rel), line: n.line },
            parent: n.parent,
            children: n.children,
            preferred: n.preferred,
        })
        .collect();
    let mut h = History { nodes, current: stored.current };
    h.validate();
    h
}

/// Persist the navigation tree (relative paths, atomic temp+rename). An empty
/// tree removes the store file; `.clew/` itself stays (it records consent).
pub fn save(root: &Path, h: &History) -> std::io::Result<()> {
    let path = store_path(root);
    if h.nodes.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let nodes = h
        .nodes
        .iter()
        .map(|n| StoredNode {
            rel: n.loc.path.strip_prefix(root).unwrap_or(&n.loc.path).to_string_lossy().to_string(),
            line: n.loc.line,
            parent: n.parent,
            children: n.children.clone(),
            preferred: n.preferred,
        })
        .collect();
    let stored = Stored { nodes, current: h.current };
    let json = serde_json::to_string(&stored).map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(name: &str, line: Option<usize>) -> Loc {
        Loc { path: PathBuf::from(name), line }
    }

    #[test]
    fn back_and_forward_walk_the_spine() {
        let mut h = History::default();
        assert!(!h.can_back() && !h.can_forward());
        h.push(loc("a", None));
        h.push(loc("b", Some(10)));
        h.push(loc("c", None));
        assert!(h.can_back() && !h.can_forward());
        assert_eq!(h.back(), Some(loc("b", Some(10))));
        assert_eq!(h.back(), Some(loc("a", None)));
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), Some(loc("b", Some(10))));
        assert!(h.can_forward());
    }

    #[test]
    fn backtracking_then_navigating_branches_instead_of_truncating() {
        let mut h = History::default();
        h.push(loc("a", None));
        h.push(loc("b", None));
        h.back(); // at a
        h.push(loc("c", None)); // new branch a→c, a→b preserved
        // The old branch is still in the tree.
        let locs: Vec<Loc> = h.flatten().into_iter().map(|v| v.loc).collect();
        assert!(locs.contains(&loc("b", None)), "old branch kept: {locs:?}");
        assert!(locs.contains(&loc("c", None)));
        // Back returns to the fork, forward follows the branch just taken (c).
        assert_eq!(h.back(), Some(loc("a", None)));
        assert_eq!(h.forward(), Some(loc("c", None)));
    }

    #[test]
    fn retaking_an_existing_branch_reuses_it() {
        let mut h = History::default();
        h.push(loc("a", None));
        h.push(loc("b", None));
        h.back();
        h.push(loc("b", None)); // same as existing child → reused, no duplicate
        assert_eq!(h.flatten().len(), 2);
    }

    #[test]
    fn goto_jumps_and_sets_the_forward_path() {
        let mut h = History::default();
        h.push(loc("a", None));
        h.push(loc("b", None));
        h.push(loc("c", None));
        let a_id = h.flatten().iter().find(|v| v.loc == loc("a", None)).unwrap().id;
        assert_eq!(h.goto(a_id), Some(loc("a", None)));
        // From a, forward walks back down the preferred path toward c.
        assert_eq!(h.forward(), Some(loc("b", None)));
        assert_eq!(h.forward(), Some(loc("c", None)));
    }

    #[test]
    fn push_dedupes_current_location() {
        let mut h = History::default();
        h.push(loc("a", Some(1)));
        h.push(loc("a", Some(1)));
        assert!(!h.can_back());
        assert_eq!(h.flatten().len(), 1);
    }

    #[test]
    fn save_load_roundtrips_relative_to_root() {
        let root = std::env::temp_dir().join("clew-history-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".clew")).unwrap();

        let mut h = History::default();
        h.push(Loc { path: root.join("src/a.rs"), line: Some(3) });
        h.push(Loc { path: root.join("src/b.rs"), line: None });
        save(&root, &h).unwrap();

        let loaded = load(&root);
        let locs: Vec<Loc> = loaded.flatten().into_iter().map(|v| v.loc).collect();
        assert_eq!(locs[0], Loc { path: root.join("src/a.rs"), line: Some(3) });
        assert_eq!(locs[1], Loc { path: root.join("src/b.rs"), line: None });
        assert_eq!(loaded.current, h.current);
    }
}
