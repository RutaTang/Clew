//! Tree-structured navigation history over (file, line) locations.
//!
//! Unlike a linear back/forward stack, backtracking and then navigating
//! elsewhere *branches* rather than discarding the old forward path — so an
//! exploration you backed out of is never lost. `forward` follows the branch you
//! most recently took; the others stay reachable from the history tree view.
//! The tree is persisted per-project (`<root>/.clew/history.json`, relative
//! paths) so a reading session survives a restart. The file is one reader's
//! private trail, so the `.gitignore` the state layer writes into `.clew/`
//! keeps it out of the repository.
//!
//! The stored object carries a `schema_version` ([`HISTORY_SCHEMA`]; absent
//! in files written before it existed, which read as 1). A history this build
//! cannot understand — unparseable, refused, or from a newer schema — is
//! never overwritten (see [`save_text`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use clew_core::statefile::StoreError;
use serde::{Deserialize, Serialize};

/// Bound on the tree, in memory and on disk. Reaching it drops the OLDEST
/// visits (see [`History::push`]) — the trail keeps its recent past instead
/// of starting over.
const MAX_NODES: usize = 800;

/// How many of the oldest visits one eviction drops, so a trail at the cap
/// is not re-spliced on every single navigation.
const EVICT_BATCH: usize = MAX_NODES / 8;

/// The layout of `history.json` this build writes and understands.
pub const HISTORY_SCHEMA: u64 = 1;

/// The most visits a stored history may hold to be processed at all: room
/// for a newer clew with a larger [`MAX_NODES`] (the excess is trimmed on
/// load), while bounding the work a crafted file can cause.
const MAX_STORED_NODES: usize = 4 * MAX_NODES;

/// Longest symbol label a visit keeps, in characters. A label is a function
/// or method name recorded for re-anchoring and display; past this it is
/// noise (or a crafted file's padding), and it is cut on the way in — at
/// [`History::push`] and on load — so no label can grow the file.
const MAX_LABEL_CHARS: usize = 200;

/// Longest stored path, in bytes: `PATH_MAX` on Linux, above macOS's. A visit
/// whose path is longer is dropped like any path clew cannot load.
const MAX_REL_BYTES: usize = 4096;

/// Byte cap for reading `history.json` — its own, far below the 64 MiB of a
/// generic state file, because the trail is re-checked before saves that
/// happen on every navigation. A trail of [`MAX_NODES`] visits with capped
/// paths and labels is tens of KB in practice and ~4 MiB with maximal
/// ordinary names; only names made of characters JSON must escape could push
/// it further, and [`to_text`] drops the oldest visits rather than write past
/// the cap. A file past it is not one clew wrote, and is refused (and so
/// never overwritten) like any other.
const MAX_HISTORY_BYTES: u64 = 8 * 1024 * 1024;

/// `label`, cut to [`MAX_LABEL_CHARS`] characters.
fn cap_label(label: String) -> String {
    match label.char_indices().nth(MAX_LABEL_CHARS) {
        Some((cut, _)) => label[..cut].to_string(),
        None => label,
    }
}

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
    /// Bumped whenever node ids change meaning (an eviction renumbers the
    /// survivors, a clear empties the tree), so a view keeping ids — the
    /// trail's collapsed branches — can tell its ids went stale.
    renumbered: u64,
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
    ///
    /// At [`MAX_NODES`] the oldest visits are dropped ([`EVICT_BATCH`] at a
    /// time) — never the whole trail, which reaching the cap used to wipe (and
    /// then persist wiped).
    pub fn push(&mut self, loc: Loc, label: Option<String>) {
        let label = label.map(cap_label);
        if self.nodes.len() >= MAX_NODES {
            self.evict_oldest(EVICT_BATCH.max(self.nodes.len() + 1 - MAX_NODES));
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
        self.renumbered += 1;
    }

    /// Where node `id` points, if there is such a node — how a click on a
    /// trail row checks that the id it carries still names the visit it drew
    /// (ids are renumbered by an eviction, and a trail can be replaced
    /// wholesale).
    pub fn loc(&self, id: usize) -> Option<&Loc> {
        self.nodes.get(id).map(|n| &n.loc)
    }

    /// Changes whenever node ids stop meaning what they meant (see the
    /// field): a view that keeps ids — the trail's collapsed branches — must
    /// drop them when this moves.
    pub fn renumbering(&self) -> u64 {
        self.renumbered
    }

    /// Drop the `count` oldest visits (lowest ids — ids are assigned in visit
    /// order), never the current one. Their children re-attach to the nearest
    /// surviving ancestor (or become roots), exactly as a pruned stored node's
    /// do, so dropping old stops never strands what was reached through them.
    fn evict_oldest(&mut self, count: usize) {
        let mut keep = vec![true; self.nodes.len()];
        let mut dropped = 0;
        for (i, k) in keep.iter_mut().enumerate() {
            if dropped == count {
                break;
            }
            if Some(i) != self.current {
                *k = false;
                dropped += 1;
            }
        }
        if dropped == 0 {
            return;
        }
        let parents: Vec<Option<usize>> = self.nodes.iter().map(|n| n.parent).collect();
        let preferred: Vec<Option<usize>> = self.nodes.iter().map(|n| n.preferred).collect();
        let spliced = splice(&parents, &preferred, self.current, &keep);
        let mut old: Vec<Option<Node>> = std::mem::take(&mut self.nodes)
            .into_iter()
            .map(Some)
            .collect();
        self.nodes = spliced
            .order
            .iter()
            .zip(spliced.links)
            .map(|(&i, links)| {
                let n = old[i].take().expect("each survivor is taken once");
                Node {
                    loc: n.loc,
                    label: n.label,
                    parent: links.parent,
                    children: links.children,
                    preferred: links.preferred,
                }
            })
            .collect();
        self.current = spliced.current;
        self.renumbered += 1;
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

    /// Like [`Self::flatten`], but skips the children of nodes in `collapsed`
    /// so the trail view can fold branches. Indentation follows the real tree
    /// depth (each child one level deeper), preserving the parent→child
    /// structure.
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
    /// [`HISTORY_SCHEMA`]; files from before the field existed are schema 1.
    #[serde(default = "schema_one")]
    schema_version: u64,
    nodes: Vec<StoredNode>,
    current: Option<usize>,
}

fn schema_one() -> u64 {
    1
}

fn store_path(root: &Path) -> PathBuf {
    root.join(".clew").join("history.json")
}

/// Load the project's navigation tree (see [`try_from_text`]): empty when
/// there is none yet, an error when the file cannot be read or understood —
/// which the caller should show, and which [`save_text`] then refuses to
/// replace.
pub fn load_checked(root: &Path) -> Result<History, StoreError> {
    match clew_core::statefile::read_capped_checked(&store_path(root), MAX_HISTORY_BYTES) {
        Ok(None) => Ok(History::default()),
        Ok(Some(text)) => try_from_text(root, &text),
        Err(e) => Err(StoreError::Refused(e)),
    }
}

/// Parse a stored history and check that this build may process it: it is a
/// history, at a schema this build knows, and at a size it would process.
/// The one gate both loading ([`try_from_text`]) and replacing
/// ([`save_text`]) go through.
fn parse_stored(text: &str) -> Result<Stored, StoreError> {
    let stored =
        serde_json::from_str::<Stored>(text).map_err(|e| StoreError::Unparseable(e.to_string()))?;
    if stored.schema_version > HISTORY_SCHEMA {
        return Err(StoreError::NewerSchema {
            found: stored.schema_version,
            supported: HISTORY_SCHEMA,
        });
    }
    // Far past anything clew writes: not processed at all (the splice walks
    // ancestor chains, which a crafted tree of this size makes quadratic),
    // and not overwritten either — it is not a file this version understands.
    if stored.nodes.len() > MAX_STORED_NODES {
        return Err(StoreError::Unparseable(format!(
            "{} visits, more than clew keeps ({MAX_NODES})",
            stored.nodes.len()
        )));
    }
    Ok(stored)
}

/// [`load_checked`] for DISPLAY: an unreadable history shows as empty.
#[cfg(test)]
pub fn load(root: &Path) -> History {
    load_checked(root).unwrap_or_default()
}

/// Decode a store file's text, converting stored relative paths back to
/// absolute against `root` (identities only for a remote root).
///
/// An error for text that is not a history at all, or one from a newer
/// schema. Content that parses but is not a sane tree is sanitized instead,
/// because the file ships with the repository (or arrives from the server):
///
/// - a stored path that would escape the project is spliced out rather than
///   emptying the store — `root.join(rel)` with an absolute or `..` rel would
///   make a later click read a file outside it, but one such entry must not
///   cost the reader the rest of the trail;
/// - a graph that is not a forest (cycles, shared children, dangling ids)
///   resets to empty, since nothing in it can be trusted to walk;
/// - a tree past [`MAX_NODES`] keeps its newest visits, like [`History::push`]
///   does, so the bound holds on load too without discarding the trail.
pub fn try_from_text(root: &Path, text: &str) -> Result<History, StoreError> {
    let stored = prune_unloadable(parse_stored(text)?);
    if stored.nodes.is_empty() {
        return Ok(History::default());
    }
    let nodes = stored
        .nodes
        .into_iter()
        .map(|n| Node {
            loc: Loc {
                path: root.join(&n.rel),
                line: n.line,
            },
            label: n.label.map(cap_label),
            parent: n.parent,
            children: n.children,
            preferred: n.preferred,
        })
        .collect();
    let mut h = History {
        nodes,
        current: stored.current,
        renumbered: 0,
    };
    h.validate();
    if h.nodes.len() > MAX_NODES {
        h.evict_oldest(h.nodes.len() - MAX_NODES);
    }
    Ok(h)
}

/// [`try_from_text`] for DISPLAY: text that is not a history shows as empty.
pub fn from_text(root: &Path, text: &str) -> History {
    try_from_text(root, text).unwrap_or_default()
}

/// Whether a stored `rel` can be loaded: it stays inside the project, and it
/// is not longer than any real path ([`MAX_REL_BYTES`]).
fn loadable_rel(rel: &str) -> bool {
    rel.len() <= MAX_REL_BYTES && clew_core::statefile::safe_rel(rel)
}

/// Splice out nodes whose stored `rel` is not loadable, keeping the rest of
/// the forest.
///
/// Two callers, one reason. `to_text` records `strip_prefix(root)` and falls
/// back to the ABSOLUTE path for a node outside the project — which an ordinary
/// go-to-definition into a dependency or stdlib source produces, since
/// `open_file` pushes the visit before it decides the target is external (see
/// `external_local` in `App::open_file`). `from_text` refuses an absolute `rel`, so
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
    let keep: Vec<bool> = stored
        .nodes
        .iter()
        .map(|nd| loadable_rel(&nd.rel))
        .collect();
    if keep.iter().all(|&k| k) {
        return stored;
    }
    splice_stored(stored, &keep)
}

/// Drop the `count` oldest stored visits (ids are visit order), never the
/// current one unless nothing else is left to drop — the same eviction
/// [`History::push`] makes at the node cap, on the stored form.
fn drop_oldest(stored: Stored, count: usize) -> Stored {
    let mut dropped = 0;
    let mut keep: Vec<bool> = (0..stored.nodes.len())
        .map(|i| {
            if dropped < count && Some(i) != stored.current {
                dropped += 1;
                false
            } else {
                true
            }
        })
        .collect();
    if dropped == 0 {
        keep.fill(false);
    }
    splice_stored(stored, &keep)
}

/// `stored` without the nodes whose `keep` is false (see [`splice`]).
fn splice_stored(stored: Stored, keep: &[bool]) -> Stored {
    let parents: Vec<Option<usize>> = stored.nodes.iter().map(|nd| nd.parent).collect();
    let preferred: Vec<Option<usize>> = stored.nodes.iter().map(|nd| nd.preferred).collect();
    let spliced = splice(&parents, &preferred, stored.current, keep);
    let nodes = spliced
        .order
        .iter()
        .zip(spliced.links)
        .map(|(&i, links)| {
            let nd = &stored.nodes[i];
            StoredNode {
                rel: nd.rel.clone(),
                line: nd.line,
                label: nd.label.clone(),
                parent: links.parent,
                children: links.children,
                preferred: links.preferred,
            }
        })
        .collect();
    Stored {
        schema_version: stored.schema_version,
        nodes,
        current: spliced.current,
    }
}

/// The tree links of one surviving node after a [`splice`].
struct Links {
    parent: Option<usize>,
    children: Vec<usize>,
    preferred: Option<usize>,
}

/// The survivors of a [`splice`]: their old ids in order (the new id of each
/// is its position here), their new links, and where the reader now is.
struct Spliced {
    order: Vec<usize>,
    links: Vec<Links>,
    current: Option<usize>,
}

/// Remove the nodes whose `keep` is false from a tree given as parent and
/// preferred-child links, without stranding anything: a removed node's
/// children are re-parented to its nearest surviving ancestor (or become
/// roots), child lists are rebuilt from the parent links (so the result
/// cannot contradict itself and `validate` accepts it), and `preferred` —
/// the branch `forward` follows — survives only if it is still one of the
/// node's own children, never guessed at. The one splice both the load-time
/// pruning and the cap's eviction use.
///
/// Input may be crafted (the load path runs this BEFORE `validate`), so every
/// ancestor walk is bounded by the node count: a parent cycle terminates here
/// rather than spinning, and out-of-range ids are treated as absent.
fn splice(
    parents: &[Option<usize>],
    preferred: &[Option<usize>],
    current: Option<usize>,
    keep: &[bool],
) -> Spliced {
    let n = parents.len();
    let surviving_ancestor = |start: usize| -> Option<usize> {
        let mut i = start;
        for _ in 0..n {
            let parent = (*parents.get(i)?)?;
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
    let mut order = Vec::new();
    for (i, &k) in keep.iter().enumerate() {
        if k {
            new_index[i] = order.len();
            order.push(i);
        }
    }
    let mut links: Vec<Links> = order
        .iter()
        .map(|&i| Links {
            parent: surviving_ancestor(i).map(|p| new_index[p]),
            children: Vec::new(),
            preferred: None,
        })
        .collect();
    let new_parents: Vec<Option<usize>> = links.iter().map(|l| l.parent).collect();
    for (i, parent) in new_parents.into_iter().enumerate() {
        if let Some(p) = parent {
            links[p].children.push(i);
        }
    }
    for (new, &old) in order.iter().enumerate() {
        let Some(pref) = preferred[old] else { continue };
        if pref >= n || !keep[pref] {
            continue;
        }
        let target = new_index[pref];
        if links[new].children.contains(&target) {
            links[new].preferred = Some(target);
        }
    }
    // A dropped ROOT has no surviving ancestor, so `current` legitimately
    // becomes `None` beside nodes that survived. That is left as-is rather
    // than aimed at some other node: the reader's position is genuinely gone,
    // and naming a survivor would be a guess they would then navigate from.
    // `push` treats it as "start a new root here" (see `History::push`).
    let current = current.filter(|&c| c < n).and_then(|c| {
        if keep[c] {
            Some(new_index[c])
        } else {
            surviving_ancestor(c).map(|a| new_index[a])
        }
    });
    Spliced {
        order,
        links,
        current,
    }
}

/// Encode for persistence (relative paths, [`HISTORY_SCHEMA`]); `None` means
/// "delete the store file" (empty tree).
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
    let mut stored = prune_unloadable(Stored {
        schema_version: HISTORY_SCHEMA,
        nodes,
        current: h.current,
    });
    // The same promise for the size: clew never writes a trail past the cap
    // it reads with. Only names made of characters JSON must escape can get
    // there (see `MAX_HISTORY_BYTES`); the oldest visits go first, as at the
    // node cap.
    loop {
        if stored.nodes.is_empty() {
            return None;
        }
        let json = serde_json::to_string(&stored).ok()?;
        if json.len() as u64 <= MAX_HISTORY_BYTES {
            return Some(json);
        }
        let quarter = stored.nodes.len().div_ceil(4);
        stored = drop_oldest(stored, quarter);
    }
}

/// Persist a navigation tree encoded by [`to_text`] (relative paths; `None`
/// = empty: the store file goes), atomically (temp + rename). The app
/// encodes on the thread that owns the [`History`] and writes here on the
/// blocking pool, one write at a time (see `App::save_history`): the encoding
/// is cheap, the write (lock, check, sync, rename) is the part that does not
/// belong on a UI thread.
///
/// Deliberately last-writer-wins, unlike the keyed stores (`bookmarks::edit`,
/// `notes::edit`): a trail is ONE reader's path through the code, and two
/// windows' trees have no common identity to merge on — grafting them together
/// would persist a branching session neither reader took, and `current` can
/// only point into one of them. So the window that navigated last owns the
/// stored trail; nothing the reader authored is lost, only the other window's
/// crumbs.
///
/// Last-writer-wins among histories THIS build understands: a file that exists
/// but cannot be read, is not a history, or comes from a newer schema is left
/// alone and the save fails with the reason (see [`load_checked`]). That
/// check is paid once per file, not per save: this runs on every navigation,
/// and re-reading and parsing the stored trail each time was the cost. A file
/// clew itself wrote — or already checked — and that is still the same file
/// on disk (same inode, size and modification time, see [`FileStamp`]) is
/// known to be replaceable; any other writer changes the stamp, and the next
/// save checks again. The check and the write run under the store's file
/// lock, so another clew process cannot slip a newer file in between.
pub fn save_text(root: &Path, text: Option<&str>) -> std::io::Result<()> {
    let path = store_path(root);
    let _exclusive = clew_core::statefile::lock(&path)?;
    check_replaceable(&path)?;
    match text {
        None => {
            clew_core::statefile::remove(&path)?;
            replaceable().remove(&path);
        }
        Some(json) => {
            clew_core::statefile::write_atomic(&path, json.as_bytes())?;
            remember_replaceable(&path);
        }
    }
    Ok(())
}

/// What identifies one version of a file on disk without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    dev: u64,
    ino: u64,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl FileStamp {
    /// The stamp of the plain file at `path`; `None` when there is none (or
    /// it is not a plain file — which the full check then refuses).
    fn of(path: &Path) -> Option<FileStamp> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = (0, 0);
        Some(FileStamp {
            dev,
            ino,
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// Per history file, the stamp of the version known to be replaceable (see
/// [`save_text`]).
fn replaceable() -> std::sync::MutexGuard<'static, std::collections::HashMap<PathBuf, FileStamp>> {
    static KNOWN: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, FileStamp>>,
    > = std::sync::OnceLock::new();
    KNOWN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn remember_replaceable(path: &Path) {
    match FileStamp::of(path) {
        Some(stamp) => {
            replaceable().insert(path.to_path_buf(), stamp);
        }
        None => {
            replaceable().remove(path);
        }
    }
}

/// Whether the history at `path` may be replaced: there is none, it is the
/// version already known to be replaceable, or — read and parsed, capped at
/// [`MAX_HISTORY_BYTES`] — it is a history this build understands.
fn check_replaceable(path: &Path) -> Result<(), StoreError> {
    if let Some(stamp) = FileStamp::of(path)
        && replaceable().get(path) == Some(&stamp)
    {
        return Ok(());
    }
    #[cfg(test)]
    FULL_CHECKS.with(|n| n.set(n.get() + 1));
    match clew_core::statefile::read_capped_checked(path, MAX_HISTORY_BYTES) {
        Ok(None) => Ok(()),
        Ok(Some(text)) => {
            parse_stored(&text)?;
            remember_replaceable(path);
            Ok(())
        }
        Err(e) => Err(StoreError::Refused(e)),
    }
}

#[cfg(test)]
thread_local! {
    /// How many saves on this thread had to read and parse the stored trail.
    static FULL_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Save `h` the way the app does: encoded, then written.
    fn save(root: &Path, h: &History) -> std::io::Result<()> {
        save_text(root, to_text(root, h).as_deref())
    }
    use clew_core::testutil::TempDir;

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
        let scratch = TempDir::new("history-test");
        let root = scratch.to_path_buf();
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
    /// `App::open_file` exists for it), and `open_file` pushes the visit before it
    /// decides the target is external. `to_text` then has no `root` to strip
    /// and records the ABSOLUTE path, which `from_text` refuses — so clew wrote
    /// a file it could not read back, and one such visit discarded the whole
    /// trail on the next launch. The out-of-project stop is still dropped; what
    /// must not happen is losing everything around it.
    #[test]
    fn a_visit_outside_the_project_does_not_cost_the_whole_trail() {
        let scratch = TempDir::new("history-external");
        let root = scratch.to_path_buf();
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
        let scratch = TempDir::new("history-pruned-current");
        let root = scratch.to_path_buf();
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
        let scratch = TempDir::new("history-hostile");
        let root = scratch.to_path_buf();
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

        // A valid but oversized tree (a chain past MAX_NODES): the cap must
        // hold on load, not only on push — by dropping the oldest visits, as
        // push does, not the whole trail.
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
            format!(r#"{{"nodes":[{}],"current":{}}}"#, nodes.join(","), n - 1),
        )
        .unwrap();
        let trimmed = load(&root).flatten();
        assert_eq!(trimmed.len(), MAX_NODES, "an over-cap tree is trimmed");
        assert!(
            trimmed.iter().all(|v| v.loc.path != root.join("f0.rs")),
            "the oldest visit went"
        );
        assert!(
            trimmed
                .iter()
                .any(|v| v.is_current && v.loc.path == root.join(format!("f{}.rs", n - 1))),
            "the newest — and current — visit stayed"
        );

        // Absurdly many visits are not processed at all (and, being nothing
        // this version wrote, not overwritten either).
        let huge: Vec<String> = (0..MAX_STORED_NODES + 1)
            .map(|i| {
                format!(r#"{{"rel":"f{i}.rs","line":null,"parent":null,"children":[],"preferred":null}}"#)
            })
            .collect();
        std::fs::write(
            &store,
            format!(r#"{{"nodes":[{}],"current":null}}"#, huge.join(",")),
        )
        .unwrap();
        assert!(load_checked(&root).is_err());
        assert!(load(&root).flatten().is_empty());

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

    /// Reaching the cap used to CLEAR the trail (and the next save persisted
    /// the wipe). Now the oldest visits go and the recent path stays
    /// walkable; views keyed by node id learn their ids moved.
    #[test]
    fn reaching_the_cap_drops_the_oldest_visits_not_the_trail() {
        let mut h = History::default();
        for i in 0..MAX_NODES {
            push(&mut h, &format!("f{i}"), None);
        }
        assert_eq!(h.flatten().len(), MAX_NODES);
        let before = h.renumbering();

        push(&mut h, "newest", None);
        let visits = h.flatten();
        assert_eq!(visits.len(), MAX_NODES - EVICT_BATCH + 1);
        assert!(visits.iter().all(|v| v.loc.path != Path::new("f0")));
        assert!(
            visits
                .iter()
                .any(|v| v.is_current && v.loc.path == Path::new("newest")),
            "the reader is where they just went"
        );
        assert_ne!(h.renumbering(), before, "node ids were renumbered");
        // The recent past is still one `back` away, in order.
        assert_eq!(h.back(), Some(loc(&format!("f{}", MAX_NODES - 1), None)));
        assert_eq!(h.back(), Some(loc(&format!("f{}", MAX_NODES - 2), None)));
    }

    /// A history this build cannot understand — unparseable, a newer schema,
    /// unreadable — is never replaced by `save`; one without the field (every
    /// file written before it existed) is schema 1, and what `save` writes
    /// carries it.
    #[test]
    fn a_history_this_build_cannot_understand_is_never_overwritten() {
        let scratch = TempDir::new("history-refuse");
        let root = scratch.to_path_buf();
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        let store = root.join(".clew").join("history.json");
        let mut h = History::default();
        h.push(
            Loc {
                path: root.join("src/a.rs"),
                line: Some(1),
            },
            None,
        );

        for bytes in [
            b"{\"nodes\": [ <<<<<<< HEAD".to_vec(),
            br#"{"schema_version":2,"nodes":[],"current":null}"#.to_vec(),
            b"{\"nodes\":[],\"current\":null,\"x\":\"\xff\"}".to_vec(),
        ] {
            std::fs::write(&store, &bytes).unwrap();
            assert!(load_checked(&root).is_err());
            assert!(save(&root, &h).is_err(), "must refuse over {bytes:?}");
            assert_eq!(std::fs::read(&store).unwrap(), bytes);
        }

        // Legacy (no schema_version) loads and may be replaced; what is
        // written names its schema.
        std::fs::write(
            &store,
            r#"{"nodes":[{"rel":"b.rs","line":2,"parent":null,"children":[],"preferred":null}],"current":0}"#,
        )
        .unwrap();
        assert_eq!(load_checked(&root).unwrap().flatten().len(), 1);
        save(&root, &h).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(written["schema_version"], HISTORY_SCHEMA);
        // And the trail is not committed by default: the first write into a
        // `.clew/` without an ignore file leaves one that lists it.
        let fresh = TempDir::new("history-gitignore");
        save(&fresh, &h).unwrap();
        let ignore = std::fs::read_to_string(fresh.join(".clew/.gitignore"))
            .expect("the first save leaves an ignore file");
        assert!(ignore.lines().any(|l| l == "history.json"), "{ignore}");
    }

    /// Nothing a label or a path carries can grow the stored trail without
    /// bound: labels are cut at push and at load, over-long paths are dropped
    /// like any path clew cannot load, and a file past the history's own cap
    /// is refused — and so never overwritten.
    #[test]
    fn labels_paths_and_the_file_are_capped() {
        let scratch = TempDir::new("history-caps");
        let root = scratch.to_path_buf();
        let mut h = History::default();
        h.push(
            Loc {
                path: root.join("a.rs"),
                line: Some(1),
            },
            Some("\u{e9}".repeat(MAX_LABEL_CHARS + 50)),
        );
        let label = h.flatten()[0].label.clone().unwrap();
        assert_eq!(label.chars().count(), MAX_LABEL_CHARS);

        let long_label = "x".repeat(10_000);
        let long_rel = "d/".repeat(MAX_REL_BYTES / 2 + 1) + "f.rs";
        let text = format!(
            r#"{{"nodes":[
                {{"rel":"a.rs","line":1,"label":"{long_label}","parent":null,"children":[1],"preferred":null}},
                {{"rel":"{long_rel}","line":1,"parent":0,"children":[],"preferred":null}}
            ],"current":0}}"#
        );
        let loaded = try_from_text(&root, &text).unwrap();
        let visits = loaded.flatten();
        assert_eq!(visits.len(), 1, "the over-long path is dropped");
        assert_eq!(
            visits[0].label.as_deref().map(|l| l.chars().count()),
            Some(MAX_LABEL_CHARS)
        );

        let store = root.join(".clew/history.json");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        let padded = format!(
            r#"{{"nodes":[],"current":null,"pad":"{}"}}"#,
            "x".repeat(MAX_HISTORY_BYTES as usize)
        );
        std::fs::write(&store, &padded).unwrap();
        assert!(matches!(load_checked(&root), Err(StoreError::Refused(_))));
        assert!(save(&root, &h).is_err());
        assert_eq!(std::fs::read(&store).unwrap().len(), padded.len());
    }

    /// The trail can be encoded where it lives and written elsewhere: the
    /// write is the same save, checks included.
    #[test]
    fn a_trail_encoded_here_can_be_saved_on_another_thread() {
        let scratch = TempDir::new("history-save-text");
        let root = scratch.to_path_buf();
        let mut h = History::default();
        h.push(
            Loc {
                path: root.join("src/a.rs"),
                line: Some(7),
            },
            Some("f".into()),
        );
        let text = to_text(&root, &h);
        let r = root.clone();
        std::thread::spawn(move || save_text(&r, text.as_deref()))
            .join()
            .unwrap()
            .unwrap();
        let loaded = load(&root).flatten();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].loc.line, Some(7));
        // An empty trail removes the file, from anywhere.
        save_text(&root, None).unwrap();
        assert!(!root.join(".clew/history.json").exists());
    }

    /// What clew writes it can read back, whatever the names: a trail whose
    /// JSON would pass the read cap loses its oldest visits instead.
    #[test]
    fn a_trail_is_never_written_past_the_cap_it_is_read_with() {
        let scratch = TempDir::new("history-write-cap");
        let root = scratch.to_path_buf();
        // Every byte of these names is escaped six-fold in JSON.
        let name = "\u{1}".repeat(MAX_REL_BYTES - 8);
        let mut h = History::default();
        for i in 0..MAX_NODES {
            h.push(
                Loc {
                    path: root.join(format!("{name}{i:04}")),
                    line: Some(1),
                },
                None,
            );
        }
        let text = to_text(&root, &h).expect("a trail");
        assert!(text.len() as u64 <= MAX_HISTORY_BYTES, "{}", text.len());
        let back = try_from_text(&root, &text).unwrap();
        let visits = back.flatten();
        assert!(!visits.is_empty() && visits.len() < MAX_NODES);
        assert!(
            visits
                .iter()
                .any(|v| v.is_current
                    && v.loc.path == root.join(format!("{name}{:04}", MAX_NODES - 1))),
            "the newest visit, where the reader is, stays"
        );
    }

    /// A save reads and parses the stored trail only when it is not the
    /// version clew itself wrote (or already checked): every navigation
    /// saves, and re-reading the file each time was the cost. A file written
    /// by anyone else is checked again — and refused when it must be.
    #[test]
    fn a_save_checks_the_stored_trail_only_when_someone_else_wrote_it() {
        let scratch = TempDir::new("history-replaceable");
        let root = scratch.to_path_buf();
        let store = root.join(".clew/history.json");
        let mut h = History::default();
        h.push(
            Loc {
                path: root.join("a.rs"),
                line: Some(1),
            },
            None,
        );
        let checks = || FULL_CHECKS.with(|n| n.get());

        // Nothing there yet; then our own file, again and again.
        save(&root, &h).unwrap();
        let after_first = checks();
        for i in 2..6 {
            h.push(
                Loc {
                    path: root.join("a.rs"),
                    line: Some(i),
                },
                None,
            );
            save(&root, &h).unwrap();
        }
        assert_eq!(checks(), after_first, "our own file is not re-read");

        // Another writer: checked again, and fine when it is a history.
        std::fs::write(
            &store,
            r#"{"nodes":[{"rel":"b.rs","line":2,"parent":null,"children":[],"preferred":null}],"current":0}"#,
        )
        .unwrap();
        save(&root, &h).unwrap();
        assert_eq!(checks(), after_first + 1);
        save(&root, &h).unwrap();
        assert_eq!(checks(), after_first + 1);

        // A newer clew's file: checked, refused, left alone.
        let newer = r#"{"schema_version":9,"nodes":[],"current":null}"#;
        std::fs::write(&store, newer).unwrap();
        assert!(save(&root, &h).is_err());
        assert_eq!(std::fs::read_to_string(&store).unwrap(), newer);
    }
}
