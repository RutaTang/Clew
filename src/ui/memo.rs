//! Per-project memos for view work that depends only on slowly-changing state.
//!
//! The view is rebuilt after every message — a scroll, a mouse move — and a
//! few panels used to recompute whole-collection results each time: the
//! Explain tab scanned and sorted the entire explanation cache for the open
//! node's children, the DOCS tab regrouped every documented item, and the
//! import overlay re-ranked every file by fan-in and fan-out. Each of those
//! now lives here, keyed by the generation of the data it is derived from
//! (plus the view options that shape it), and is recomputed only when that
//! key changes. The memos live in the project's session, so a generation
//! only has to be unique within one project: a new project starts from an
//! empty set.
//!
//! `view` only gets `&App`, so a memo fills itself through a `RefCell` — the
//! same shape the viewer's own memos use (`viewer::Memo`). Values are handed
//! out as `Arc`s so no borrow of the cell outlives the lookup.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

/// One memoized value and the key it was computed for.
#[derive(Debug)]
pub(crate) struct Memo<K, V> {
    slot: RefCell<Option<(K, Arc<V>)>>,
}

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Memo {
            slot: RefCell::new(None),
        }
    }
}

impl<K: PartialEq, V> Memo<K, V> {
    /// The value for `key`: the held one when it was computed for an equal
    /// key, else `compute()`'s (which then replaces it). `compute` runs with
    /// the cell released, so it may consult other memos freely.
    pub(crate) fn get_or(&self, key: K, compute: impl FnOnce() -> V) -> Arc<V> {
        if let Some((held, value)) = self.slot.borrow().as_ref()
            && *held == key
        {
            return Arc::clone(value);
        }
        let value = Arc::new(compute());
        *self.slot.borrow_mut() = Some((key, Arc::clone(&value)));
        value
    }
}

/// The view's memos (one set per open project, on `ProjectSession::view_memo`).
#[derive(Default, Debug)]
pub struct ViewMemo {
    /// The Explain tab's "CONTAINS" rows for the open node: `(label, node)`,
    /// sorted by label. Keyed by `(ExplainState::cache_seq, node)` — the
    /// cache's generation, bumped by every change to it.
    pub(crate) explain_children: Memo<(u64, crate::explain::Node), Vec<ChildRow>>,
    /// The DOCS tree's groups. Keyed by [`DocsKey`].
    pub(crate) docs_groups: Memo<DocsKey, Vec<DocsGroup>>,
    /// The project-calls overlay's rankings, keyed by
    /// `(ProjectCallsState::graph_rev, ProjectSession::symbol_index_rev)`: the
    /// graph they rank, and the symbol index that says which nodes are tests.
    pub(crate) calls_summary: Memo<(u64, u64), CallsSummary>,
    /// The call-graph languages the symbol index holds a function of, sorted
    /// — what a refinement covering none of theirs leaves out — keyed by
    /// `ProjectSession::symbol_index_rev`.
    pub(crate) function_languages: Memo<u64, Vec<&'static str>>,
}

/// One "CONTAINS" row: what it shows, and the node it opens.
pub(crate) type ChildRow = (String, crate::explain::Node);

/// What the DOCS grouping depends on: the index generation
/// (`DocsState::generation`) and the three view options that shape it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocsKey {
    pub(crate) generation: u64,
    pub(crate) by_module: bool,
    pub(crate) show_all: bool,
    /// The trimmed, lowercased filter.
    pub(crate) query: String,
}

/// One DOCS group: its label (a file rel or a module path) and its visible
/// items as `(file index, item index)` into `DocsState::files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocsGroup {
    pub(crate) label: String,
    pub(crate) items: Vec<(usize, usize)>,
}

/// The import overlay's numbers: counts, external packages, and the two
/// top-12 rankings with each file's `(fan-in, fan-out)`. Computed by the
/// import job ([`crate::ui::import_ranks`]), held on the project session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ImportRanks {
    pub(crate) files: usize,
    pub(crate) internal_edges: usize,
    pub(crate) externals: Vec<String>,
    pub(crate) by_fan_in: Vec<RankedFile>,
    pub(crate) by_fan_out: Vec<RankedFile>,
}

/// A ranked file with its fan-in and fan-out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankedFile {
    pub(crate) path: PathBuf,
    pub(crate) fan_in: usize,
    pub(crate) fan_out: usize,
}

/// The project-calls overlay's rankings: the most-called functions and the
/// uncalled ones (test functions left out), each as `(node id, count)` — the
/// caller count for a hub, the callee count for an uncalled function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CallsSummary {
    pub(crate) hubs: Vec<(usize, usize)>,
    pub(crate) uncalled: Vec<(usize, usize)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_memo_recomputes_only_when_its_key_changes() {
        let memo: Memo<u64, String> = Memo::default();
        let runs = Cell::new(0);
        let compute = |s: &str| {
            runs.set(runs.get() + 1);
            s.to_string()
        };
        assert_eq!(*memo.get_or(1, || compute("one")), "one");
        assert_eq!(*memo.get_or(1, || compute("again")), "one", "same key");
        assert_eq!(runs.get(), 1);
        assert_eq!(*memo.get_or(2, || compute("two")), "two");
        assert_eq!(runs.get(), 2);
        // Going back to an older key recomputes: only the latest is held.
        assert_eq!(*memo.get_or(1, || compute("one'")), "one'");
        assert_eq!(runs.get(), 3);
    }
}
