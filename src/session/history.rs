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
            self.nodes.push(Node {
                loc,
                label,
                parent: None,
                children: Vec::new(),
                preferred: None,
            });
            self.current = Some(0);
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
/// absolute. Returns an empty history on any error / missing file — including
/// any stored path that would escape the project: the file ships with the
/// repository, and `root.join(rel)` with an absolute or `..` rel would make a
/// later click read a file outside it.
pub fn load(root: &Path) -> History {
    let Some(stored) = clew_core::statefile::read(&store_path(root))
        .and_then(|s| serde_json::from_str::<Stored>(&s).ok())
    else {
        return History::default();
    };
    // The cap `push` enforces must hold on load too: the file ships with the
    // repository, and a crafted deep chain far past it would stall (or
    // overflow) every traversal before the first push ever ran.
    if stored.nodes.len() > MAX_NODES {
        return History::default();
    }
    if !stored
        .nodes
        .iter()
        .all(|n| clew_core::statefile::safe_rel(&n.rel))
    {
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

/// Persist the navigation tree (relative paths, atomic temp+rename). An empty
/// tree removes the store file; `.clew/` itself stays (it records consent).
pub fn save(root: &Path, h: &History) -> std::io::Result<()> {
    let path = store_path(root);
    if h.nodes.is_empty() {
        return clew_core::statefile::remove(&path);
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
    let stored = Stored {
        nodes,
        current: h.current,
    };
    let json = serde_json::to_string(&stored).map_err(|e| std::io::Error::other(e.to_string()))?;
    clew_core::statefile::write_atomic(&path, json.as_bytes())
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

    /// A hostile repository ships `.clew/history.json`. Escaping paths and
    /// non-forest graphs must reset the history, never be walked or opened.
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
        assert!(load(&root).flatten().is_empty(), "absolute rel must reset");

        // Traversal: joins outside the project.
        std::fs::write(
            &store,
            r#"{"nodes":[{"rel":"../../outside.rs","line":null,"parent":null,"children":[],"preferred":null}],"current":0}"#,
        )
        .unwrap();
        assert!(load(&root).flatten().is_empty(), "`..` rel must reset");

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
