//! Tree-structured navigation history over (file, line) locations.
//!
//! Unlike a linear back/forward stack, backtracking and then navigating
//! elsewhere *branches* rather than discarding the old forward path — so an
//! exploration you backed out of is never lost. `forward` follows the branch you
//! most recently took; the others stay reachable from the history tree view.
//! The tree is persisted per-project (`<root>/.clew/history.json`, relative
//! paths) so a reading session survives a restart.

use std::collections::HashSet;
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
    /// The function/method defined at this location when it was visited, if any.
    /// Kept so the entry can be re-anchored to the symbol's new line after the
    /// file is edited, and so its label stays stable even as lines shift.
    label: Option<String>,
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
    /// The symbol name recorded at this location, if any (stable across edits).
    pub label: Option<String>,
    pub depth: usize,
    pub is_current: bool,
    /// True at a fork — this node has more than one child branch.
    pub forks: bool,
    /// True if this node has any children (so the trail can offer collapse).
    pub has_children: bool,
    /// True if this node's subtree is currently collapsed in the trail view.
    pub collapsed: bool,
}

impl History {
    /// Record a navigation to `loc` (with the symbol name there, if any) as a
    /// child of the current node, branching if the current node already had
    /// children. A jump to the current spot is a no-op; re-taking a branch
    /// already present reuses it instead of duplicating.
    pub fn push(&mut self, loc: Loc, label: Option<String>) {
        if self.nodes.len() >= MAX_NODES {
            self.clear();
        }
        let Some(cur) = self.current else {
            // No current node, but `nodes` is not necessarily empty: a load
            // whose recorded position was pruned away leaves survivors behind
            // (`prune_unloadable` has no ancestor to remap to when the dropped
            // node is a ROOT). Index the new node where it actually lands and
            // make IT current — hardcoding 0 here pointed the reader at an
            // unrelated surviving node, so `back` walked to a file they had
            // never visited and every later visit was recorded under it.
            let id = self.nodes.len();
            self.nodes.push(Node {
                loc,
                label,
                parent: None,
                children: Vec::new(),
                preferred: None,
            });
            self.current = Some(id);
            return;
        };
        if self.nodes[cur].loc == loc {
            return; // already here
        }
        let existing = self.nodes[cur]
            .children
            .iter()
            .copied()
            .find(|&c| self.nodes[c].loc == loc);
        let child = existing.unwrap_or_else(|| {
            let id = self.nodes.len();
            self.nodes.push(Node {
                loc,
                label,
                parent: Some(cur),
                children: Vec::new(),
                preferred: None,
            });
            self.nodes[cur].children.push(id);
            id
        });
        self.nodes[cur].preferred = Some(child);
        self.current = Some(child);
    }

    /// Re-anchor this file's entries after it changed: an entry whose stored
    /// symbol name still exists moves to that symbol's current line (following
    /// edits above it), choosing the same-named symbol nearest its old line when
    /// there are several. Entries without a label, or whose symbol vanished, keep
    /// their line. Returns whether anything moved (so the caller can re-persist).
    pub fn reanchor(&mut self, file: &Path, symbols: &[(String, usize)]) -> bool {
        let mut changed = false;
        for n in &mut self.nodes {
            if n.loc.path != file {
                continue;
            }
            let (Some(label), Some(old)) = (&n.label, n.loc.line) else {
                continue;
            };
            let best = symbols
                .iter()
                .filter(|(name, _)| name == label)
                .min_by_key(|(_, line)| line.abs_diff(old));
            if let Some(&(_, line)) = best
                && n.loc.line != Some(line)
            {
                n.loc.line = Some(line);
                changed = true;
            }
        }
        changed
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
        self.current
            .map(|c| !self.nodes[c].children.is_empty())
            .unwrap_or(false)
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
        self.flatten_with(&HashSet::new())
    }

    /// Like [`flatten`], but skips the children of nodes in `collapsed` so the
    /// trail view can fold branches. Indentation follows the real tree depth
    /// (each child one level deeper), preserving the parent→child structure.
    pub fn flatten_with(&self, collapsed: &HashSet<usize>) -> Vec<Visit> {
        let mut out = Vec::new();
        for r in (0..self.nodes.len()).filter(|&i| self.nodes[i].parent.is_none()) {
            self.dfs(r, 0, collapsed, &mut out);
        }
        out
    }

    // Iterative, not recursive: the tree is user-shaped (and its persisted
    // form repo-shaped), so its depth must never translate into stack depth.
    fn dfs(&self, id: usize, depth: usize, collapsed: &HashSet<usize>, out: &mut Vec<Visit>) {
        let mut stack = vec![(id, depth)];
        while let Some((id, depth)) = stack.pop() {
            let n = &self.nodes[id];
            let is_collapsed = collapsed.contains(&id);
            out.push(Visit {
                id,
                loc: n.loc.clone(),
                label: n.label.clone(),
                depth,
                is_current: self.current == Some(id),
                forks: n.children.len() > 1,
                has_children: !n.children.is_empty(),
                collapsed: is_collapsed,
            });
            if is_collapsed {
                continue;
            }
            // Reversed so the LIFO stack emits children in their real order.
            for &c in n.children.iter().rev() {
                stack.push((c, depth + 1));
            }
        }
    }

    /// Drop the whole tree if any stored index is out of range (corrupt file).
    fn validate(&mut self) {
        let n = self.nodes.len();
        let in_range = |o: Option<usize>| o.map(|i| i < n).unwrap_or(true);
        let ok = in_range(self.current)
            && self.nodes.iter().all(|nd| {
                in_range(nd.parent) && in_range(nd.preferred) && nd.children.iter().all(|&c| c < n)
            });
        if !ok {
            self.clear();
            return;
        }
        // The persisted file ships with the repository, so index-range checks
        // are not enough: the graph must actually be a forest. A crafted file
        // with a cycle (or a node claimed by two parents) would otherwise
        // overflow the display's DFS or hang `goto`'s parent walk.
        let mut visited = vec![false; n];
        let mut stack: Vec<usize> = (0..n).filter(|&i| self.nodes[i].parent.is_none()).collect();
        let mut seen = 0usize;
        while let Some(i) = stack.pop() {
            if visited[i] {
                self.clear(); // reached twice: shared child or child-cycle
                return;
            }
            visited[i] = true;
            seen += 1;
            stack.extend(self.nodes[i].children.iter().copied());
        }
        if seen != n {
            // Unreachable nodes — including any parent-link cycle, whose
            // members have parents and so are never roots.
            self.clear();
            return;
        }
        // Child links must agree with parent links (goto walks parents).
        for (i, nd) in self.nodes.iter().enumerate() {
            for &c in &nd.children {
                if self.nodes[c].parent != Some(i) {
                    self.clear();
                    return;
                }
            }
        }
        // `preferred` is "the branch forward follows": it must be one of the
        // node's own children. A crafted value pointing elsewhere — the node
        // itself, an ancestor, a sibling branch — would make `forward` loop
        // in place or jump across the tree.
        for nd in &self.nodes {
            if let Some(p) = nd.preferred
                && !nd.children.contains(&p)
            {
                self.clear();
                return;
            }
        }
    }
}

// ------------------------------------------------------------- persistence

#[derive(Serialize, Deserialize)]
struct StoredNode {
    rel: String,
    line: Option<usize>,
    #[serde(default)]
    label: Option<String>,
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
/// absolute. Returns an empty history on any error / missing file. A stored
/// path that would escape the project does not empty the store: that entry is
/// spliced out and the rest is kept (see `prune_unloadable`), because the file
/// ships with the repository and `root.join(rel)` with an absolute or `..` rel
/// would make a later click read a file outside it.
pub fn load(root: &Path) -> History {
    clew_core::statefile::read(&store_path(root))
        .map(|s| from_text(root, &s))
        .unwrap_or_default()
}

/// Decode a store file's text, converting stored relative paths back to
/// absolute against `root` (identities only for a remote root). Returns an
/// empty history on unparseable text or a chain past `MAX_NODES`. A stored path
/// that would escape the project is spliced out rather than emptying the store:
/// the file ships with the repository (or arrives from the server), and
/// `root.join(rel)` with an absolute or `..` rel would make a later click read
/// a file outside it — but one such entry must not cost the reader the rest of
/// the trail.
pub fn from_text(root: &Path, text: &str) -> History {
    let Ok(stored) = serde_json::from_str::<Stored>(text) else {
        return History::default();
    };
    // The cap `push` enforces must hold on load too: the file ships with the
    // repository, and a crafted deep chain far past it would stall (or
    // overflow) every traversal before the first push ever ran.
    if stored.nodes.len() > MAX_NODES {
        return History::default();
    }
    let stored = prune_unloadable(stored);
    if stored.nodes.is_empty() {
        return History::default();
    }
    let nodes = stored
        .nodes
        .into_iter()
        .map(|n| Node {
            loc: Loc {
                path: root.join(&n.rel),
                line: n.line,
            },
            label: n.label,
            parent: n.parent,
            children: n.children,
            preferred: n.preferred,
        })
        .collect();
    let mut h = History {
        nodes,
        current: stored.current,
    };
    h.validate();
    h
}

/// Splice out nodes whose stored `rel` is not loadable, keeping the rest of
/// the forest.
///
/// Two callers, one reason. `to_text` records `strip_prefix(root)` and falls
/// back to the ABSOLUTE path for a node outside the project — which an ordinary
/// go-to-definition into a dependency or stdlib source produces, since
/// `open_file` pushes the visit before it decides the target is external (see
/// `external_local` in `session.rs`). `from_text` refuses an absolute `rel`, so
/// clew wrote a history file it could not read back: one such visit used to
/// discard the reader's ENTIRE trail on the next launch. Pruning on the way out
/// keeps clew's own files loadable; pruning on the way in stops a
/// repository-shipped file from costing the reader everything else it holds,
/// while still keeping `root.join(rel)` inside the project.
///
/// A dropped node's children are re-parented to its nearest surviving ancestor
/// rather than dropped with it — losing one stop must not lose everything
/// reached through it. Child links are rebuilt from the parent links, so the
/// result cannot contradict itself and `validate` accepts it.
fn prune_unloadable(stored: Stored) -> Stored {
    let n = stored.nodes.len();
    let keep: Vec<bool> = stored
        .nodes
        .iter()
        .map(|nd| clew_core::statefile::safe_rel(&nd.rel))
        .collect();
    if keep.iter().all(|&k| k) {
        return stored;
    }
    // Nearest surviving ancestor. `validate` has not run yet and the input may
    // be crafted, so the walk is bounded by the node count: a parent cycle has
    // to terminate here rather than spin.
    let surviving_ancestor = |start: usize| -> Option<usize> {
        let mut i = start;
        for _ in 0..n {
            let parent = stored.nodes.get(i)?.parent?;
            if parent >= n {
                return None;
            }
            if keep[parent] {
                return Some(parent);
            }
            i = parent;
        }
        None
    };
    let mut new_index = vec![usize::MAX; n];
    let mut next = 0usize;
    for (i, &k) in keep.iter().enumerate() {
        if k {
            new_index[i] = next;
            next += 1;
        }
    }
    let mut nodes: Vec<StoredNode> = Vec::with_capacity(next);
    for (i, nd) in stored.nodes.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        nodes.push(StoredNode {
            rel: nd.rel.clone(),
            line: nd.line,
            label: nd.label.clone(),
            parent: surviving_ancestor(i).map(|p| new_index[p]),
            children: Vec::new(),
            preferred: None,
        });
    }
    let parents: Vec<Option<usize>> = nodes.iter().map(|nd| nd.parent).collect();
    for (i, parent) in parents.iter().enumerate() {
        if let Some(p) = *parent {
            nodes[p].children.push(i);
        }
    }
    // `preferred` names the branch `forward` follows, so it must still be one
    // of this node's own children after the splice; anything else is dropped
    // rather than guessed at.
    for (i, nd) in stored.nodes.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        let Some(pref) = nd.preferred else { continue };
        if pref >= n || !keep[pref] {
            continue;
        }
        let (here, target) = (new_index[i], new_index[pref]);
        if nodes[here].children.contains(&target) {
            nodes[here].preferred = Some(target);
        }
    }
    // A dropped ROOT has no surviving ancestor, so `current` legitimately
    // becomes `None` beside nodes that survived. That is left as-is rather
    // than aimed at some other node: the reader's position is genuinely gone,
    // and naming a survivor would be a guess they would then navigate from.
    // `push` treats it as "start a new root here" (see `History::push`).
    let current = stored.current.filter(|&c| c < n).and_then(|c| {
        if keep[c] {
            Some(new_index[c])
        } else {
            surviving_ancestor(c).map(|a| new_index[a])
        }
    });
    Stored { nodes, current }
}

/// Encode for persistence (relative paths); `None` means "delete the store
/// file" (empty tree — `.clew/` itself stays, it records consent).
pub fn to_text(root: &Path, h: &History) -> Option<String> {
    if h.nodes.is_empty() {
        return None;
    }
    let nodes = h
        .nodes
        .iter()
        .map(|n| StoredNode {
            rel: n
                .loc
                .path
                .strip_prefix(root)
                .unwrap_or(&n.loc.path)
                .to_string_lossy()
                .to_string(),
            line: n.loc.line,
            label: n.label.clone(),
            parent: n.parent,
            children: n.children.clone(),
            preferred: n.preferred,
        })
        .collect();
    // `strip_prefix` above falls back to the absolute path for a visit outside
    // the project, which `from_text` cannot accept. Prune those here so what
    // clew writes is always something clew can read back.
    let stored = prune_unloadable(Stored {
        nodes,
        current: h.current,
    });
    if stored.nodes.is_empty() {
        return None;
    }
    serde_json::to_string(&stored).ok()
}

/// Persist the navigation tree (relative paths, atomic temp+rename). An empty
/// tree removes the store file; `.clew/` itself stays (it records consent).
///
/// Deliberately last-writer-wins, unlike the keyed stores (`bookmarks::edit`,
/// `notes::edit`): a trail is ONE reader's path through the code, and two
/// windows' trees have no common identity to merge on — grafting them together
/// would persist a branching session neither reader took, and `current` can
/// only point into one of them. So the window that navigated last owns the
/// stored trail; nothing the reader authored is lost, only the other window's
/// crumbs.
pub fn save(root: &Path, h: &History) -> std::io::Result<()> {
    let path = store_path(root);
    match to_text(root, h) {
        None => clew_core::statefile::remove(&path),
        Some(json) => clew_core::statefile::write_atomic(&path, json.as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(name: &str, line: Option<usize>) -> Loc {
        Loc {
            path: PathBuf::from(name),
            line,
        }
    }

    /// Push without a symbol label (most tests don't care about re-anchoring).
    fn push(h: &mut History, name: &str, line: Option<usize>) {
        h.push(loc(name, line), None);
    }

    #[test]
    fn back_and_forward_walk_the_spine() {
        let mut h = History::default();
        assert!(!h.can_back() && !h.can_forward());
        push(&mut h, "a", None);
        push(&mut h, "b", Some(10));
        push(&mut h, "c", None);
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
        push(&mut h, "a", None);
        push(&mut h, "b", None);
        h.back(); // at a
        push(&mut h, "c", None); // new branch a→c, a→b preserved
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
        push(&mut h, "a", None);
        push(&mut h, "b", None);
        h.back();
        push(&mut h, "b", None); // same as existing child → reused, no duplicate
        assert_eq!(h.flatten().len(), 2);
    }

    #[test]
    fn goto_jumps_and_sets_the_forward_path() {
        let mut h = History::default();
        push(&mut h, "a", None);
        push(&mut h, "b", None);
        push(&mut h, "c", None);
        let a_id = h
            .flatten()
            .iter()
            .find(|v| v.loc == loc("a", None))
            .unwrap()
            .id;
        assert_eq!(h.goto(a_id), Some(loc("a", None)));
        // From a, forward walks back down the preferred path toward c.
        assert_eq!(h.forward(), Some(loc("b", None)));
        assert_eq!(h.forward(), Some(loc("c", None)));
    }

    #[test]
    fn push_dedupes_current_location() {
        let mut h = History::default();
        push(&mut h, "a", Some(1));
        push(&mut h, "a", Some(1));
        assert!(!h.can_back());
        assert_eq!(h.flatten().len(), 1);
    }

    #[test]
    fn reanchor_follows_a_symbol_to_its_new_line() {
        let mut h = History::default();
        // Two labelled entries in a.rs; one unlabelled entry stays put.
        h.push(
            Loc {
                path: PathBuf::from("a.rs"),
                line: Some(10),
            },
            Some("foo".into()),
        );
        h.push(
            Loc {
                path: PathBuf::from("a.rs"),
                line: Some(30),
            },
            Some("bar".into()),
        );
        h.push(
            Loc {
                path: PathBuf::from("a.rs"),
                line: Some(50),
            },
            None,
        );

        // After an edit, foo moved 10→14, bar 30→34; a b.rs symbol is irrelevant.
        let symbols = vec![("foo".to_string(), 14), ("bar".to_string(), 34)];
        assert!(h.reanchor(std::path::Path::new("a.rs"), &symbols));

        let lines: Vec<Option<usize>> = h.flatten().into_iter().map(|v| v.loc.line).collect();
        assert_eq!(lines, vec![Some(14), Some(34), Some(50)]); // labelled moved, plain kept
        // Idempotent: a second pass with the same symbols moves nothing.
        assert!(!h.reanchor(std::path::Path::new("a.rs"), &symbols));
    }

    #[test]
    fn save_load_roundtrips_relative_to_root() {
        let root = std::env::temp_dir().join("clew-history-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".clew")).unwrap();

        let mut h = History::default();
        h.push(
            Loc {
                path: root.join("src/a.rs"),
                line: Some(3),
            },
            Some("f".into()),
        );
        h.push(
            Loc {
                path: root.join("src/b.rs"),
                line: None,
            },
            None,
        );
        save(&root, &h).unwrap();

        let loaded = load(&root);
        let visits = loaded.flatten();
        assert_eq!(
            visits[0].loc,
            Loc {
                path: root.join("src/a.rs"),
                line: Some(3)
            }
        );
        assert_eq!(visits[0].label.as_deref(), Some("f")); // label survives the round-trip
        assert_eq!(
            visits[1].loc,
            Loc {
                path: root.join("src/b.rs"),
                line: None
            }
        );
        assert_eq!(loaded.current, h.current);
    }

    /// Reading a dependency's source is a normal move (`external_local` in
    /// `session.rs` exists for it), and `open_file` pushes the visit before it
    /// decides the target is external. `to_text` then has no `root` to strip
    /// and records the ABSOLUTE path, which `from_text` refuses — so clew wrote
    /// a file it could not read back, and one such visit discarded the whole
    /// trail on the next launch. The out-of-project stop is still dropped; what
    /// must not happen is losing everything around it.
    #[test]
    fn a_visit_outside_the_project_does_not_cost_the_whole_trail() {
        let root = std::env::temp_dir().join("clew-history-external");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".clew")).unwrap();

        let mut h = History::default();
        h.push(
            Loc {
                path: root.join("src/a.rs"),
                line: Some(3),
            },
            Some("f".into()),
        );
        // Go-to-definition into a dependency: outside the project root.
        h.push(
            Loc {
                path: PathBuf::from("/opt/rustup/lib/core/src/option.rs"),
                line: Some(571),
            },
            Some("Option::map".into()),
        );
        // ...and back to project code, reached THROUGH the external stop.
        h.push(
            Loc {
                path: root.join("src/b.rs"),
                line: Some(9),
            },
            None,
        );
        save(&root, &h).unwrap();

        let visits = load(&root).flatten();
        let paths: Vec<_> = visits.iter().map(|v| v.loc.path.clone()).collect();
        assert_eq!(
            paths,
            vec![root.join("src/a.rs"), root.join("src/b.rs")],
            "the in-project stops must survive an external one, in order"
        );
        assert_eq!(visits[0].label.as_deref(), Some("f"));
        // b.rs hung off the dropped node; it must re-parent to a.rs rather than
        // vanish with it or become a second root.
        assert_eq!(
            visits[1].depth, 1,
            "the child re-parents to its grandparent"
        );
    }

    /// Pruning the node the reader was ON leaves `current` unset while other
    /// stops survive (the dropped node was the root, so there is no ancestor to
    /// fall back to). The next visit must become its own root and the reader's
    /// position — pushing it while pointing `current` at node 0 adopted an
    /// unrelated survivor, after which `back` walked to a file the reader had
    /// never opened and every later visit hung off it.
    #[test]
    fn a_pruned_position_does_not_make_an_unrelated_node_current() {
        let root = std::env::temp_dir().join("clew-history-pruned-current");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".clew")).unwrap();

        // What clew itself writes after Clear trail → go-to-definition into a
        // dependency (root, outside the project) → click a project file.
        std::fs::write(
            root.join(".clew").join("history.json"),
            r#"{"nodes":[
                {"rel":"/opt/rustup/lib/core/src/option.rs","line":571,"parent":null,"children":[1],"preferred":1},
                {"rel":"src/main.rs","line":9,"parent":0,"children":[],"preferred":null}
            ],"current":0}"#,
        )
        .unwrap();

        let mut h = load(&root);
        assert_eq!(h.flatten().len(), 1, "the in-project stop survives");
        assert_eq!(h.current, None, "the position itself was pruned away");

        h.push(
            Loc {
                path: root.join("src/other.rs"),
                line: Some(1),
            },
            None,
        );
        let visits = h.flatten();
        let current: Vec<_> = visits
            .iter()
            .filter(|v| v.is_current)
            .map(|v| v.loc.path.clone())
            .collect();
        assert_eq!(
            current,
            vec![root.join("src/other.rs")],
            "the file just opened is where the reader is"
        );
        assert!(!h.can_back(), "a fresh root has nothing behind it");
        // And the survivor is untouched: not adopted as this visit's parent,
        // not re-pointed, still reachable from the trail list.
        assert_eq!(visits.len(), 2);
        assert!(
            visits.iter().all(|v| v.depth == 0),
            "two independent roots, not a fabricated parent link"
        );
    }

    /// A hostile repository ships `.clew/history.json`. Escaping paths are
    /// dropped and non-forest graphs reset the history — neither is ever walked
    /// or opened.
    #[test]
    fn hostile_history_files_reset_instead_of_escaping_or_looping() {
        let root = std::env::temp_dir().join("clew-history-hostile");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        let store = root.join(".clew").join("history.json");

        // Absolute path: `root.join("/etc/hosts")` would REPLACE the root.
        std::fs::write(
            &store,
            r#"{"nodes":[{"rel":"/etc/hosts","line":null,"parent":null,"children":[],"preferred":null}],"current":0}"#,
        )
        .unwrap();
        assert!(
            load(&root).flatten().is_empty(),
            "an absolute rel is dropped; it was the only node, so nothing is left"
        );

        // Traversal: joins outside the project.
        std::fs::write(
            &store,
            r#"{"nodes":[{"rel":"../../outside.rs","line":null,"parent":null,"children":[],"preferred":null}],"current":0}"#,
        )
        .unwrap();
        assert!(
            load(&root).flatten().is_empty(),
            "a `..` rel is dropped; it was the only node, so nothing is left"
        );

        // Self-referential child: the display DFS would recurse forever.
        std::fs::write(
            &store,
            r#"{"nodes":[{"rel":"a.rs","line":null,"parent":null,"children":[0],"preferred":null}],"current":0}"#,
        )
        .unwrap();
        assert!(load(&root).flatten().is_empty(), "child cycle must reset");

        // Parent cycle (no roots): goto's parent walk would never end.
        std::fs::write(
            &store,
            r#"{"nodes":[
                {"rel":"a.rs","line":null,"parent":1,"children":[1],"preferred":null},
                {"rel":"b.rs","line":null,"parent":0,"children":[0],"preferred":null}
            ],"current":0}"#,
        )
        .unwrap();
        assert!(load(&root).flatten().is_empty(), "parent cycle must reset");

        // Two parents claiming one child (a DAG, not a forest).
        std::fs::write(
            &store,
            r#"{"nodes":[
                {"rel":"a.rs","line":null,"parent":null,"children":[2],"preferred":null},
                {"rel":"b.rs","line":null,"parent":null,"children":[2],"preferred":null},
                {"rel":"c.rs","line":null,"parent":0,"children":[],"preferred":null}
            ],"current":null}"#,
        )
        .unwrap();
        assert!(load(&root).flatten().is_empty(), "shared child must reset");

        // `preferred` outside the node's own children: `forward` would loop
        // in place (self) or jump across branches.
        std::fs::write(
            &store,
            r#"{"nodes":[
                {"rel":"a.rs","line":null,"parent":null,"children":[1],"preferred":0},
                {"rel":"b.rs","line":null,"parent":0,"children":[],"preferred":null}
            ],"current":0}"#,
        )
        .unwrap();
        assert!(
            load(&root).flatten().is_empty(),
            "preferred must be a child"
        );

        // A valid but oversized tree (a chain far past MAX_NODES): the cap
        // must hold on load, not only on push.
        let n = MAX_NODES + 1;
        let nodes: Vec<String> = (0..n)
            .map(|i| {
                let parent = if i == 0 {
                    "null".into()
                } else {
                    (i - 1).to_string()
                };
                let children = if i + 1 < n {
                    format!("[{}]", i + 1)
                } else {
                    "[]".into()
                };
                format!(
                    r#"{{"rel":"f{i}.rs","line":null,"parent":{parent},"children":{children},"preferred":null}}"#
                )
            })
            .collect();
        std::fs::write(
            &store,
            format!(r#"{{"nodes":[{}],"current":0}}"#, nodes.join(",")),
        )
        .unwrap();
        assert!(
            load(&root).flatten().is_empty(),
            "an over-cap tree must reset"
        );

        // A symlinked store file is refused outright.
        #[cfg(unix)]
        {
            let outside = root.join("outside.json");
            std::fs::write(
                &outside,
                r#"{"nodes":[{"rel":"a.rs","line":null,"parent":null,"children":[],"preferred":null}],"current":0}"#,
            )
            .unwrap();
            std::fs::remove_file(&store).unwrap();
            std::os::unix::fs::symlink(&outside, &store).unwrap();
            assert!(load(&root).flatten().is_empty(), "symlink store must reset");
        }
    }
}
