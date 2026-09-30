//! Project-wide call graph, built offline from tree-sitter call sites resolved
//! by name against the symbol index.
//!
//! The LSP call hierarchy (the client's `callgraph`) is exact but per-symbol and
//! lazy — asking a server about every function in a large project would be far
//! too slow. This module trades that exactness for a whole-project view that is
//! computed locally and instantly: it finds each call *site* with tree-sitter,
//! reads off the enclosing function and the called name, and links caller to
//! callee by matching that name against the project's symbols.
//!
//! Name resolution is deliberately approximate — a call to `new` links to every
//! `new` in the project — so the graph is best read for its *aggregate* signal:
//! which functions nothing calls (entry points / dead-code candidates) and which
//! are called the most (hubs). Per-symbol precision is the LSP call graph's job.
//!
//! A function is identified everywhere by `(file, name, ordinal)` ([`SymKey`]):
//! the ordinal is its rank among the file's same-name callables by line, the
//! numbering `outline::fn_ordinals` defines and `explain::Node::Function`
//! uses. So two `new`s in one file are two nodes with two keys, in this graph,
//! in the LSP-precise edge set and in the explain engine alike.
//!
//! Lives in clew-core so it builds where the files live: the client builds it
//! directly for a local project; clew-server answers the `ProjectCalls`
//! request with it for a remote one, in its wire form
//! ([`clew_protocol::CallGraph`], project-relative paths — see
//! [`ProjectCallGraph::to_wire`] and [`ProjectCallGraph::from_wire`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tree_sitter::{Node, Tree};

use crate::highlight::Lang;
use crate::outline::{Located, is_callable};

/// A stable identity for a function across edits: its file, its name, and
/// its ordinal among the file's same-name callables (by line). The line itself
/// is deliberately excluded so an edge survives lines shifting above it.
pub type SymKey = (PathBuf, String, u32);

/// The LSP-precise edge set, symbol-keyed so it can be patched incrementally as
/// files change without a full re-query.
pub type SymEdges = HashSet<(SymKey, SymKey)>;

/// A function/type definition the graph can link to (a filtered symbol-index
/// entry).
#[derive(Debug, Clone)]
pub struct Def {
    pub name: String,
    pub kind: String,
    pub file: PathBuf,
    pub line: usize,
}

/// A call site found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    /// The innermost enclosing function that is a callable symbol of the file
    /// (see [`calls_in`]), if the call is inside one.
    pub caller: Option<String>,
    /// The called function/method name (the trailing identifier of the callee).
    pub callee: String,
    /// True when the callee is a `receiver.name(…)` access (a method call). Such
    /// a name belongs to the receiver's type, so it must not fall back to a
    /// globally-unique project function — that's how `.get()` on a std type used
    /// to be mis-attributed to a project `get`.
    pub method: bool,
    /// 1-based line of the call.
    pub line: usize,
}

/// A node in the project call graph: one function/method definition plus the
/// nodes that call it and that it calls.
#[derive(Debug, Clone)]
pub struct SymNode {
    pub name: String,
    pub kind: String,
    pub file: PathBuf,
    pub line: usize,
    callers: Vec<usize>,
    callees: Vec<usize>,
}

/// The graph. Its adjacency lists are indices into `nodes`, which every
/// accessor trusts — so a graph that comes from outside (the wire) is checked
/// on the way in ([`ProjectCallGraph::from_wire`]).
#[derive(Debug, Default, Clone)]
pub struct ProjectCallGraph {
    nodes: Vec<SymNode>,
    /// Each node's ordinal among its file's same-name callables — the third
    /// part of its [`SymKey`] — computed once, when the node set is fixed.
    ordinals: Vec<u32>,
}

impl ProjectCallGraph {
    /// The graph over `nodes`, which the constructors below have finished.
    fn with_nodes(nodes: Vec<SymNode>) -> Self {
        let ordinals = ordinals_of(
            nodes
                .iter()
                .map(|n| (n.file.as_path(), n.name.as_str(), n.kind.as_str(), n.line)),
        );
        ProjectCallGraph { nodes, ordinals }
    }
}

impl ProjectCallGraph {
    /// The wire form of this graph, each node's file mapped through `rel` —
    /// to the project-relative path the graph crosses the protocol in (a
    /// remote client must never hold a remote absolute path as anything but
    /// an identity).
    pub fn to_wire(&self, rel: impl Fn(&Path) -> String) -> clew_protocol::CallGraph {
        clew_protocol::CallGraph {
            nodes: self
                .nodes
                .iter()
                .map(|n| clew_protocol::CallGraphNode {
                    name: n.name.clone(),
                    kind: n.kind.clone(),
                    file: rel(&n.file),
                    line: n.line,
                    callers: n.callers.clone(),
                    callees: n.callees.clone(),
                })
                .collect(),
        }
    }

    /// The graph a wire [`clew_protocol::CallGraph`] describes, each node's
    /// file mapped through `abs` (back to this client's identities) —
    /// VALIDATED first. The graph comes from another process, and an
    /// out-of-range adjacency index used to panic the UI on the first frame
    /// that drew it; now it is an error the caller reports.
    pub fn from_wire(
        wire: clew_protocol::CallGraph,
        abs: impl Fn(&str) -> PathBuf,
    ) -> Result<Self, String> {
        let n = wire.nodes.len();
        for (i, node) in wire.nodes.iter().enumerate() {
            if let Some(bad) = node.callers.iter().chain(&node.callees).find(|&&j| j >= n) {
                return Err(format!(
                    "call graph node {i} links to node {bad}, but the graph has {n} nodes"
                ));
            }
        }
        Ok(ProjectCallGraph::with_nodes(
            wire.nodes
                .into_iter()
                .map(|w| SymNode {
                    file: abs(&w.file),
                    name: w.name,
                    kind: w.kind,
                    line: w.line,
                    callers: w.callers,
                    callees: w.callees,
                })
                .collect(),
        ))
    }
}

/// Attach edges to nodes' adjacency lists (self-edges dropped, duplicates
/// collapsed) and finish the graph.
fn finalize(mut nodes: Vec<SymNode>, edges: HashSet<(usize, usize)>) -> ProjectCallGraph {
    for (c, e) in edges {
        if c < nodes.len() && e < nodes.len() && c != e {
            nodes[c].callees.push(e);
            nodes[e].callers.push(c);
        }
    }
    for n in &mut nodes {
        n.callers.sort_unstable();
        n.callers.dedup();
        n.callees.sort_unstable();
        n.callees.dedup();
    }
    ProjectCallGraph::with_nodes(nodes)
}

/// Each item's ordinal among its file's same-name CALLABLES, by line: the
/// numbering `outline::fn_ordinals` gives within one file, applied per file
/// by the one shared implementation (`outline::Ordinals`). Items are
/// `(file, name, kind, line)`; one that is not a callable is numbered against
/// the callables around it rather than counted among them — so a list that
/// strays a type into it cannot shift the keys of the functions.
fn ordinals_of<'a>(items: impl Iterator<Item = (&'a Path, &'a str, &'a str, usize)>) -> Vec<u32> {
    let items: Vec<_> = items.collect();
    let ordinals = crate::outline::Ordinals::new(
        items
            .iter()
            .map(|&(file, name, kind, line)| ((file, name), is_callable(kind), line)),
    );
    items
        .iter()
        .map(|&(file, name, _, line)| ordinals.of(&(file, name), line))
        .collect()
}

/// One file's call sites read off ONE parse, plus the lines of its callables
/// that are bodyless declarations — a C prototype, a TypeScript overload
/// signature, an interface or abstract method. A call lands on a definition;
/// such a declaration stands in only for a name nothing defines.
#[derive(Debug, Clone)]
pub struct FileCalls {
    pub file: PathBuf,
    /// The language the file was parsed as (a C++ `.h` reads as C++).
    pub lang: Lang,
    pub calls: Vec<CallSite>,
    /// 1-based lines of callables declared without a body.
    pub declarations: HashSet<usize>,
    /// Where the definitions of a name more than one callable of the file
    /// shares lie: definition line → the whole definition's 1-based
    /// inclusive `(first, last)` lines. A call site names its enclosing
    /// function only by name, and for such a name the name does not say
    /// which definition the call is in — a call after a nested `f` inside
    /// `f` is the outer one's. Only these names are kept (see
    /// [`resolve_caller`]); every other name resolves by itself.
    pub bodies: HashMap<usize, (usize, usize)>,
}

impl FileCalls {
    /// Parse `source` once and read off its call sites. `None` for a file in
    /// a language without a call model.
    pub fn read(file: &Path, source: &str) -> Option<FileCalls> {
        let lang = Lang::for_source(crate::highlight::detect(file)?, source)?;
        lang_spec(lang)?;
        let tree = crate::highlight::parse(source, lang)?;
        let symbols = crate::outline::located_in(&tree, source, lang);
        let calls = calls_in(&tree, source, lang, &symbols);
        FileCalls::of(file.to_path_buf(), lang, calls, &symbols)
    }

    /// The call facts of a file parsed for other reasons as well — what
    /// [`read`](Self::read) would have produced, from `calls` and `symbols`
    /// read off a tree of `lang` ([`crate::outline::analyze`] yields all
    /// three from one parse). `None` for a language without a call model,
    /// exactly as `read` answers. This is how the client's symbol index, and
    /// the server's call-graph build (one parse per file, with its symbol
    /// snapshot's extraction), hand their call sites to
    /// [`ProjectCallGraph::build_from_calls`] instead of the graph parsing
    /// every file a second time.
    pub fn of(
        file: PathBuf,
        lang: Lang,
        calls: Vec<CallSite>,
        symbols: &[Located],
    ) -> Option<FileCalls> {
        lang_spec(lang)?;
        Some(FileCalls::new(file, lang, calls, symbols))
    }

    /// Whether this file contributes nothing to a graph: no call site, and no
    /// bodyless declaration to tell a definition from.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty() && self.declarations.is_empty()
    }

    /// Assemble from facts already read off the file's tree (see
    /// `outline::analyze`).
    pub fn new(file: PathBuf, lang: Lang, calls: Vec<CallSite>, symbols: &[Located]) -> FileCalls {
        let defined = || {
            symbols
                .iter()
                .filter(|l| is_callable(&l.symbol.kind))
                .filter_map(|l| Some((l.symbol.name.as_str(), l.symbol.line, l.body?)))
        };
        let mut defined_as: HashMap<&str, usize> = HashMap::new();
        for (name, _, _) in defined() {
            *defined_as.entry(name).or_default() += 1;
        }
        FileCalls {
            file,
            lang,
            calls,
            declarations: symbols
                .iter()
                .filter(|l| l.is_bodyless_callable())
                .map(|l| l.symbol.line)
                .collect(),
            bodies: defined()
                .filter(|(name, _, _)| defined_as.get(name).is_some_and(|&n| n > 1))
                .map(|(_, line, span)| (line, span))
                .collect(),
        }
    }
}

impl ProjectCallGraph {
    /// Build the graph from the project's callable definitions, the current
    /// source of every file (so call sites reflect what's on disk right now),
    /// and each file's import scope (the internal files it imports — used to
    /// resolve a called name to the definition actually in scope).
    ///
    /// Parses every file. A caller that has parsed them already — a symbol
    /// index — uses [`build_from_calls`](Self::build_from_calls).
    pub fn build(
        defs: Vec<Def>,
        sources: &[(PathBuf, String)],
        scope: &HashMap<PathBuf, HashSet<PathBuf>>,
    ) -> Self {
        let files: Vec<FileCalls> = sources
            .iter()
            .filter_map(|(file, content)| FileCalls::read(file, content))
            .collect();
        Self::build_from_calls(defs, &files, scope)
    }

    /// [`build`](Self::build) over call sites that were already extracted,
    /// for a caller that parsed each file once for several purposes: the
    /// client's symbol index keeps each file's [`FileCalls`] (and caches them
    /// across sessions), and the server reads its definitions and call sites
    /// off one parse per file, with the extraction its symbol snapshot uses.
    /// The same graph as `build` over the same files, with no file read or
    /// parsed. A file that contributes nothing ([`FileCalls::is_empty`]) may
    /// be left out.
    pub fn build_from_calls<'a>(
        defs: Vec<Def>,
        files: impl IntoIterator<Item = &'a FileCalls>,
        scope: &HashMap<PathBuf, HashSet<PathBuf>>,
    ) -> Self {
        let files: Vec<&FileCalls> = files.into_iter().collect();
        let nodes: Vec<SymNode> = defs
            .into_iter()
            .filter(|d| is_callable(&d.kind))
            .map(|d| SymNode {
                name: d.name,
                kind: d.kind,
                file: d.file,
                line: d.line,
                callers: Vec::new(),
                callees: Vec::new(),
            })
            .collect();

        let declarations: HashMap<&Path, &HashSet<usize>> = files
            .iter()
            .map(|f| (f.file.as_path(), &f.declarations))
            .collect();
        let facts: Vec<NodeFacts> = nodes
            .iter()
            .map(|n| NodeFacts {
                bodyless: declarations
                    .get(n.file.as_path())
                    .is_some_and(|d| d.contains(&n.line)),
                lang: crate::highlight::detect(&n.file).and_then(Lang::from_key),
            })
            .collect();

        let mut name_to: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut by_file: HashMap<&Path, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            name_to.entry(n.name.as_str()).or_default().push(i);
            by_file.entry(n.file.as_path()).or_default().push(i);
        }
        // Definitions within a file, ordered by line, so a call resolves to the
        // nearest preceding same-named definition.
        for v in by_file.values_mut() {
            v.sort_by_key(|&i| nodes[i].line);
        }

        let empty_scope: HashSet<PathBuf> = HashSet::new();
        let mut edges: HashSet<(usize, usize)> = HashSet::new();
        for fc in files {
            let imported = scope.get(&fc.file).unwrap_or(&empty_scope);
            let site = Site {
                file: &fc.file,
                lang: fc.lang,
                imported,
            };
            for cs in &fc.calls {
                // Bare calls to language builtins (`len(x)`, `make(...)`) are not
                // project functions; skip them so they don't resolve to some
                // same-named definition and inflate the graph.
                if is_builtin(fc.lang, &cs.callee) {
                    continue;
                }
                let Some(caller_name) = cs.caller.as_deref() else {
                    continue; // a top-level call has no caller function node
                };
                let Some(caller) =
                    resolve_caller(&by_file, &fc.file, caller_name, cs.line, &nodes, &fc.bodies)
                else {
                    continue;
                };
                for callee in resolve_callees(&name_to, &nodes, &facts, cs, &site) {
                    // Skip self-edges so a recursive function with no other
                    // callers still reads as "uncalled".
                    if callee != caller {
                        edges.insert((caller, callee));
                    }
                }
            }
        }

        finalize(nodes, edges)
    }

    /// Build from explicit caller→callee edges — used by the LSP-precise pass,
    /// which resolves calls exactly instead of by name. `defs` must already be
    /// the callable definitions (edges index into them).
    pub fn from_callable_defs(defs: Vec<Def>, edges: HashSet<(usize, usize)>) -> Self {
        let nodes = defs
            .into_iter()
            .map(|d| SymNode {
                name: d.name,
                kind: d.kind,
                file: d.file,
                line: d.line,
                callers: Vec::new(),
                callees: Vec::new(),
            })
            .collect();
        finalize(nodes, edges)
    }

    /// The project's callable definitions (functions/methods), in a stable order
    /// — the node set the LSP-precise pass maps call-hierarchy results back
    /// onto.
    pub fn callable(defs: &[Def]) -> Vec<Def> {
        defs.iter()
            .filter(|d| is_callable(&d.kind))
            .cloned()
            .collect()
    }

    /// Every def's [`SymKey`], in `defs` order.
    pub fn keys_of(defs: &[Def]) -> Vec<SymKey> {
        let ordinals = ordinals_of(
            defs.iter()
                .map(|d| (d.file.as_path(), d.name.as_str(), d.kind.as_str(), d.line)),
        );
        defs.iter()
            .zip(ordinals)
            .map(|(d, o)| (d.file.clone(), d.name.clone(), o))
            .collect()
    }

    /// Build the display graph from the full callable node set and symbol-keyed
    /// edges (the LSP-precise edge set, kept stable across edits by keying on
    /// `(file, name, ordinal)` rather than node index). Edges whose endpoints
    /// aren't in `defs` are dropped. `defs` must be the callable definitions.
    pub fn graph_from_sym_edges(defs: Vec<Def>, edges: &SymEdges) -> Self {
        let keys = Self::keys_of(&defs);
        let idx_edges: HashSet<(usize, usize)> = {
            let mut lookup: HashMap<&SymKey, usize> = HashMap::new();
            for (i, k) in keys.iter().enumerate() {
                lookup.entry(k).or_insert(i);
            }
            edges
                .iter()
                .filter_map(|(c, e)| Some((*lookup.get(c)?, *lookup.get(e)?)))
                .collect()
        };
        Self::from_callable_defs(defs, idx_edges)
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The node `id`. Panics on an id that is not from this graph; see
    /// [`get`](Self::get) for a checked lookup.
    pub fn node(&self, id: usize) -> &SymNode {
        &self.nodes[id]
    }

    /// The node `id`, or `None` when no such node exists (an id kept across a
    /// rebuild, say).
    pub fn get(&self, id: usize) -> Option<&SymNode> {
        self.nodes.get(id)
    }

    /// Total number of distinct caller→callee edges.
    pub fn edge_count(&self) -> usize {
        self.nodes.iter().map(|n| n.callees.len()).sum()
    }

    /// Every node's ordinal among its file's same-name callables.
    pub fn ordinals(&self) -> &[u32] {
        &self.ordinals
    }

    /// The [`SymKey`] of node `id`.
    pub fn key_of(&self, id: usize) -> Option<SymKey> {
        let n = self.nodes.get(id)?;
        Some((n.file.clone(), n.name.clone(), self.ordinals[id]))
    }

    /// Each function's callees as [`SymKey`]s — the call-graph dependency
    /// edges the explain engine orders by. Keyed by the full identity, so two
    /// same-name functions of one file keep their own callee lists (a
    /// `(file, name)` key kept whichever came last).
    pub fn callee_keys(&self) -> HashMap<SymKey, Vec<SymKey>> {
        let key = |i: usize| {
            (
                self.nodes[i].file.clone(),
                self.nodes[i].name.clone(),
                self.ordinals[i],
            )
        };
        let mut out: HashMap<SymKey, Vec<SymKey>> = HashMap::new();
        for (i, n) in self.nodes.iter().enumerate() {
            let callees = out.entry(key(i)).or_default();
            callees.extend(n.callees.iter().map(|&c| key(c)));
            callees.sort();
            callees.dedup();
        }
        out
    }

    /// Aggregate the symbol-level graph to file level: the files that hold any
    /// function, and an edge A→B when a function in A calls one in B. This is the
    /// readable "module call-flow" view (601 functions collapse to ~30 files).
    pub fn file_graph(&self) -> (Vec<PathBuf>, Vec<(usize, usize)>) {
        let mut files: Vec<PathBuf> = self.nodes.iter().map(|n| n.file.clone()).collect();
        files.sort();
        files.dedup();
        let idx: HashMap<&Path, usize> = files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.as_path(), i))
            .collect();
        let mut edge_set: HashSet<(usize, usize)> = HashSet::new();
        for n in &self.nodes {
            let a = idx[n.file.as_path()];
            for &c in &n.callees {
                let b = idx[self.nodes[c].file.as_path()];
                if a != b {
                    edge_set.insert((a, b));
                }
            }
        }
        let mut edges: Vec<(usize, usize)> = edge_set.into_iter().collect();
        edges.sort_unstable();
        (files, edges)
    }

    /// The node id for the FIRST (ordinal 0) callable named `name` in `file`,
    /// if present. Prefer [`id_of_key`](Self::id_of_key) when the ordinal is
    /// known: a file's second `new` is not its first.
    pub fn id_of(&self, file: &Path, name: &str) -> Option<usize> {
        self.id_of_key(file, name, 0)
    }

    /// The node id for `(file, name, ordinal)`. Same-line callables share an
    /// ordinal (see `ordinals_of`); of those, the first node is the answer.
    pub fn id_of_key(&self, file: &Path, name: &str, ordinal: u32) -> Option<usize> {
        (0..self.nodes.len()).find(|&i| {
            self.ordinals[i] == ordinal && self.nodes[i].name == name && self.nodes[i].file == file
        })
    }

    pub fn callers_of(&self, id: usize) -> &[usize] {
        &self.nodes[id].callers
    }

    pub fn callees_of(&self, id: usize) -> &[usize] {
        &self.nodes[id].callees
    }

    /// Functions nothing else calls — entry points, public API, or dead code.
    /// Sorted by name for stable display.
    pub fn uncalled(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| self.nodes[i].callers.is_empty())
            .collect();
        v.sort_by_key(|&i| self.sort_key(i));
        v
    }

    /// The most-called functions (hubs), most callers first, capped at `limit`.
    /// Restricted to functions with a project-unique name: a call to a name
    /// shared by many definitions (`new`, `len`) links to all of them, so their
    /// caller counts are inflated and meaningless — filtering to unique names
    /// keeps the list a real measure of a specific function's importance.
    pub fn most_called(&self, limit: usize) -> Vec<usize> {
        let mut name_counts: HashMap<&str, usize> = HashMap::new();
        for n in &self.nodes {
            *name_counts.entry(n.name.as_str()).or_default() += 1;
        }
        let mut v: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| {
                !self.nodes[i].callers.is_empty()
                    && name_counts.get(self.nodes[i].name.as_str()) == Some(&1)
            })
            .collect();
        v.sort_by_key(|&i| {
            (
                std::cmp::Reverse(self.nodes[i].callers.len()),
                self.sort_key(i),
            )
        });
        v.truncate(limit);
        v
    }

    /// Display order: by name, then line. Borrowed, so sorting allocates
    /// nothing per comparison.
    fn sort_key(&self, id: usize) -> (&str, usize) {
        (self.nodes[id].name.as_str(), self.nodes[id].line)
    }

    /// How execution reaches `target` from the project's entry points: for
    /// each entry (a node `entry_class` ranks — `Some(0)` for a main, route,
    /// command or handler, `Some(1)` for a test, `None` for the rest) that
    /// reaches it through at most `max_depth` calls, the shortest chain of
    /// calls from that entry down to `target`, `target` last. Chains are
    /// ordered by class, then length, then the entry's name and line, and
    /// at most `limit` are returned. Empty when `target` is itself an entry
    /// (the reader is already there), or nothing ranked reaches it — a
    /// library's API, a function called through an interface or a callback
    /// the graph does not see.
    ///
    /// A breadth-first walk over callers, so each chain is a shortest one;
    /// recursion and cycles end at the visited set.
    pub fn paths_from_entries(
        &self,
        target: usize,
        entry_class: impl Fn(&SymNode) -> Option<u8>,
        limit: usize,
        max_depth: usize,
    ) -> Vec<Vec<usize>> {
        if target >= self.nodes.len() || limit == 0 || entry_class(&self.nodes[target]).is_some() {
            return Vec::new();
        }
        let mut parent: HashMap<usize, usize> = HashMap::new();
        let mut depth: HashMap<usize, usize> = HashMap::new();
        let mut queue = std::collections::VecDeque::new();
        let mut found: Vec<(u8, usize, usize)> = Vec::new(); // (class, depth, entry)
        depth.insert(target, 0);
        queue.push_back(target);
        while let Some(id) = queue.pop_front() {
            let d = depth[&id];
            if d >= max_depth {
                continue;
            }
            for &caller in &self.nodes[id].callers {
                if depth.contains_key(&caller) {
                    continue;
                }
                depth.insert(caller, d + 1);
                parent.insert(caller, id);
                if let Some(class) = entry_class(&self.nodes[caller]) {
                    found.push((class, d + 1, caller));
                    // An entry is where a chain starts; what calls an entry
                    // (a test of the main, say) is a chain of its own.
                }
                queue.push_back(caller);
            }
        }
        found.sort_by_key(|&(class, d, entry)| (class, d, self.sort_key(entry)));
        found
            .into_iter()
            .take(limit)
            .map(|(_, _, entry)| {
                let mut chain = vec![entry];
                let mut at = entry;
                while let Some(&next) = parent.get(&at) {
                    chain.push(next);
                    at = next;
                }
                chain
            })
            .collect()
    }
}

impl SymNode {
    pub fn caller_count(&self) -> usize {
        self.callers.len()
    }

    pub fn callee_count(&self) -> usize {
        self.callees.len()
    }
}

/// Per-node facts the resolver needs beyond the node itself.
struct NodeFacts {
    /// A declaration without a body (see [`FileCalls::declarations`]).
    bodyless: bool,
    /// The language of the node's file, by extension.
    lang: Option<Lang>,
}

/// Where a call was made.
struct Site<'a> {
    file: &'a Path,
    lang: Lang,
    imported: &'a HashSet<PathBuf>,
}

/// Per-language node kinds for the call-site walk.
struct LangSpec {
    /// Nodes that are function scopes.
    fn_kinds: &'static [&'static str],
    /// Nodes that are call sites.
    call_kinds: &'static [&'static str],
    /// Subtrees holding no calls of their own, skipped whole (a Rust
    /// `macro_rules!` body or attribute is token soup, not code).
    skip_kinds: &'static [&'static str],
    /// Rust: read `name(…)` call shapes out of macro token trees, whose
    /// arguments tree-sitter does not parse as expressions — without this
    /// `assert_eq!(parse(x), …)` and `vec![build()]` hid their calls.
    macro_calls: bool,
}

fn lang_spec(lang: Lang) -> Option<LangSpec> {
    let spec = |fn_kinds, call_kinds| LangSpec {
        fn_kinds,
        call_kinds,
        skip_kinds: &[],
        macro_calls: false,
    };
    Some(match lang {
        Lang::Rust => LangSpec {
            fn_kinds: &["function_item"],
            call_kinds: &["call_expression"],
            skip_kinds: &["macro_definition", "attribute_item", "inner_attribute_item"],
            macro_calls: true,
        },
        Lang::Python => spec(&["function_definition"], &["call"]),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => spec(
            &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "function_expression",
                // Arrow functions (`const x = () => …`, class fields, object
                // entries, callbacks). Each is a scope only when its inferred
                // name is one of the file's callables (see `calls_in`);
                // otherwise — an anonymous callback, a binding the outline
                // does not list — its calls belong to the enclosing function.
                "arrow_function",
            ],
            &["call_expression", "new_expression"],
        ),
        Lang::Go => spec(
            &["function_declaration", "method_declaration"],
            &["call_expression"],
        ),
        // Dart has no node wrapping a signature and its body — they are flat
        // siblings (`method_signature`/`function_signature` then `function_body`).
        // The body holds the calls, so it is the enclosing scope; its name is
        // recovered from the preceding signature (see `fn_name`).
        Lang::Dart => spec(
            &["function_body"],
            &["method_invocation", "constructor_invocation"],
        ),
        Lang::C => spec(&["function_definition"], &["call_expression"]),
        Lang::Cpp => spec(
            &["function_definition"],
            &["call_expression", "new_expression"],
        ),
        Lang::Java => spec(
            &["method_declaration", "constructor_declaration"],
            &["method_invocation", "object_creation_expression"],
        ),
        Lang::Json | Lang::Bash | Lang::Yaml | Lang::Toml | Lang::Html | Lang::Css | Lang::Zig => {
            return None;
        }
    })
}

/// Extract every call site from one file's `source`.
pub fn calls_of(source: &str, lang: &str) -> Vec<CallSite> {
    let Some(lang) = Lang::for_source(lang, source) else {
        return Vec::new();
    };
    if lang_spec(lang).is_none() {
        return Vec::new();
    }
    let Some(tree) = crate::highlight::parse(source, lang) else {
        return Vec::new();
    };
    let symbols = crate::outline::located_in(&tree, source, lang);
    calls_in(&tree, source, lang, &symbols)
}

/// The call sites of an already-parsed file, attributed with its outline
/// `symbols` (from the same tree).
///
/// A call's caller is its innermost enclosing function THAT IS ONE OF THE
/// FILE'S CALLABLES — the nodes the graph has. An anonymous function is named
/// after the binding it is assigned to (`const handler = () => …`,
/// `a.b = function () {}`, `{ onClick: () => … }`), and when that binding is
/// not a callable the outline lists, the function is transparent: its calls
/// belong to the named function around it. Attributing them to the unlisted
/// name dropped them instead, since no node carries it. For a language
/// without an outline every function name is accepted.
pub fn calls_in(tree: &Tree, source: &str, lang: Lang, symbols: &[Located]) -> Vec<CallSite> {
    let Some(spec) = lang_spec(lang) else {
        return Vec::new();
    };
    let callable: Option<HashSet<&str>> = crate::highlight::tags_query(lang).is_ok().then(|| {
        symbols
            .iter()
            .filter(|l| is_callable(&l.symbol.kind))
            .map(|l| l.symbol.name.as_str())
            .collect()
    });
    let mut out = Vec::new();
    walk(
        tree.root_node(),
        source,
        lang,
        &spec,
        callable.as_ref(),
        &mut out,
    );
    out
}

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

/// Iterative depth-first walk carrying the nearest enclosing function name. It is
/// explicit-stack (not recursive) so a pathologically deep tree — e.g. a checked-
/// in minified bundle with 100k-deep nested expressions — can't overflow the
/// stack and abort the process.
fn walk(
    root: Node,
    src: &str,
    lang: Lang,
    spec: &LangSpec,
    callable: Option<&HashSet<&str>>,
    out: &mut Vec<CallSite>,
) {
    // Each item is a node plus the enclosing function name in scope for it.
    let mut stack: Vec<(Node, Option<Rc<str>>)> = vec![(root, None)];
    while let Some((node, enclosing)) = stack.pop() {
        let kind = node.kind();
        if spec.skip_kinds.contains(&kind) {
            continue;
        }
        // A function definition becomes the enclosing scope for its subtree —
        // under the first of its names that the file's outline has.
        let own: Option<Rc<str>> = if spec.fn_kinds.contains(&kind) {
            fn_names(node, src, lang)
                .into_iter()
                .find(|name| callable.is_none_or(|c| c.contains(name.as_str())))
                .map(|s| Rc::from(s.as_str()))
        } else {
            None
        };
        let current = own.or(enclosing);

        if spec.call_kinds.contains(&kind)
            && let Some((callee, method)) = callee_name(node, src, lang)
        {
            out.push(CallSite {
                caller: current.as_deref().map(str::to_string),
                callee,
                method,
                line: node.start_position().row + 1,
            });
        }
        if spec.macro_calls && kind == "token_tree" {
            macro_calls(node, src, current.as_deref(), out);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push((child, current.clone()));
        }
    }
}

/// The `name(…)` call shapes among one macro token tree's direct tokens: an
/// identifier immediately followed by a parenthesized token tree. `a.b(…)` is
/// a method call; `foo!(…)` (a nested macro: its `!` sits between the two) and
/// `Foo { … }` / `x[…]` are not calls. Keywords are not identifiers inside a
/// token tree, so `if (x)` never matches. Nested token trees are visited by
/// the walk itself.
fn macro_calls(tree: Node, src: &str, caller: Option<&str>, out: &mut Vec<CallSite>) {
    let mut cursor = tree.walk();
    let tokens: Vec<Node> = tree.children(&mut cursor).collect();
    for (i, token) in tokens.iter().enumerate() {
        if token.kind() != "identifier" {
            continue;
        }
        let Some(next) = tokens.get(i + 1) else {
            continue;
        };
        if next.kind() != "token_tree" || !node_text(*next, src).starts_with('(') {
            continue;
        }
        let prev = i.checked_sub(1).map(|j| tokens[j].kind());
        if prev == Some("fn") {
            continue; // a function declared inside the macro input
        }
        out.push(CallSite {
            caller: caller.map(str::to_string),
            callee: node_text(*token, src).to_string(),
            method: prev == Some("."),
            line: token.start_position().row + 1,
        });
    }
}

/// The names a function definition goes by, most preferred first. A
/// declaration has one: its `name` field. A function EXPRESSION is known by
/// the binding it is assigned to (`const handler = () => …` → `handler`,
/// `a.b = function` → `b`, a class field → the field, an object entry → the
/// key) — and a named one (`const x = function named() {}`) also by its own
/// name, which is in scope only inside its own body, for recursion. Callers
/// and the outline know it by the binding, so the binding comes first: taking
/// the own name first attributed its calls to `named`, which a TypeScript
/// outline does not list, so they fell to whatever function enclosed it.
fn fn_names(node: Node, src: &str, lang: Lang) -> Vec<String> {
    match lang {
        // `function_definition` names its function inside the declarator.
        Lang::C | Lang::Cpp => return c_function_name(node, src).into_iter().collect(),
        // Dart: a `function_body` is a bare sibling after its signature, so
        // its name comes from the preceding signature (a lambda/arrow body's
        // signature has no name, so those calls fall through to the enclosing
        // named function).
        Lang::Dart if node.kind() == "function_body" => {
            return node
                .prev_sibling()
                .and_then(|sig| dart_sig_name(sig, src))
                .into_iter()
                .collect();
        }
        _ => {}
    }
    let own = node
        .child_by_field_name("name")
        .map(|n| node_text(n, src).to_string());
    binding_name(node, src).into_iter().chain(own).collect()
}

/// The name of the binding a function expression is assigned to, if it is
/// assigned to one (see [`fn_names`]).
fn binding_name(node: Node, src: &str) -> Option<String> {
    let parent = node.parent()?;
    let named = match parent.kind() {
        "variable_declarator" | "public_field_definition" => parent.child_by_field_name("name"),
        // JavaScript's class field names its property `property`.
        "field_definition" => parent.child_by_field_name("property"),
        // `a.b = function () {}` and `Foo.prototype.bar = …` name the member,
        // as the outline does — not the whole `a.b` expression.
        "assignment_expression" => {
            parent
                .child_by_field_name("left")
                .map(|left| match left.kind() {
                    "member_expression" => left.child_by_field_name("property").unwrap_or(left),
                    _ => left,
                })
        }
        "pair" => parent.child_by_field_name("key"),
        _ => None,
    }?;
    Some(node_text(named, src).to_string())
}

/// The function a C/C++ `function_definition` defines: its declarator chain
/// (`*foo(void)`, `(&bar)(int)`, `Ns::Cls::baz() const`) down to the name.
fn c_function_name(def: Node, src: &str) -> Option<String> {
    let mut node = def.child_by_field_name("declarator")?;
    for _ in 0..64 {
        node = match node.kind() {
            "function_declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "attributed_declarator" => node.child_by_field_name("declarator")?,
            "parenthesized_declarator" => node.named_child(0)?,
            _ => {
                let name = innermost_name(node);
                return last_identifier(node_text(name, src));
            }
        };
    }
    None
}

/// The name from a Dart signature preceding a `function_body`. Direct signatures
/// (`function_`/`getter_`/`setter_signature`) carry a `name` field; a
/// `method_signature` wraps one of those, so look one level in.
fn dart_sig_name(sig: Node, src: &str) -> Option<String> {
    if let Some(n) = sig.child_by_field_name("name") {
        return Some(node_text(n, src).to_string());
    }
    let mut cursor = sig.walk();
    sig.children(&mut cursor).find_map(|c| {
        c.child_by_field_name("name")
            .map(|n| node_text(n, src).to_string())
    })
}

/// Descend through wrappers that end in a name — a turbofish or template
/// argument list, a `ns::` qualification — to the name node itself, so
/// `ns::f<int>` yields `f`, not `int`.
fn innermost_name(mut node: Node) -> Node {
    loop {
        let inner = match node.kind() {
            "generic_function" => node.child_by_field_name("function"),
            "template_function" | "template_method" | "template_type" | "qualified_identifier" => {
                node.child_by_field_name("name")
            }
            _ => None,
        };
        match inner {
            Some(n) => node = n,
            None => return node,
        }
    }
}

/// The called name (trailing identifier of the callee expression —
/// `self.foo.bar` → `bar`, `Vec::<u8>::with_capacity` → `with_capacity`) plus
/// whether the call is a `receiver.name(…)` method access. `None` for a call
/// that cannot name a project function.
fn callee_name(call: Node, src: &str, lang: Lang) -> Option<(String, bool)> {
    match (lang, call.kind()) {
        // Java names the method in a field of its own; `obj.f()` carries the
        // receiver as `object`.
        (Lang::Java, "method_invocation") => {
            let name = call.child_by_field_name("name")?;
            let method = call.child_by_field_name("object").is_some();
            return Some((node_text(name, src).to_string(), method));
        }
        // `new Foo<Bar>(…)`: the constructor is the type's base name.
        (Lang::Java, "object_creation_expression") | (Lang::Cpp, "new_expression") => {
            let mut ty = call.child_by_field_name("type")?;
            if ty.kind() == "generic_type" {
                ty = ty.named_child(0)?;
            }
            let ty = innermost_name(ty);
            return Some((last_identifier(node_text(ty, src))?, false));
        }
        // Dart constructor calls name a type via a child, not a `function` field.
        (Lang::Dart, "constructor_invocation") => {
            let mut cursor = call.walk();
            let ty = call
                .children(&mut cursor)
                .find(|n| matches!(n.kind(), "type_identifier" | "identifier"))?;
            return Some((last_identifier(node_text(ty, src))?, false));
        }
        _ => {}
    }
    let mut target = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("constructor"))
        .or_else(|| call.child(0))?;
    // A trailing turbofish / template argument list wraps the callee; unwrap to
    // the real function so the name and method-flag come from `parse`, not
    // from `i32`.
    while matches!(target.kind(), "generic_function" | "template_function") {
        let inner = match target.kind() {
            "generic_function" => target.child_by_field_name("function"),
            _ => target.child_by_field_name("name"),
        };
        match inner {
            Some(inner) => target = inner,
            None => break,
        }
    }
    // `std::move(x)` is the standard library, never a project function.
    if lang == Lang::Cpp
        && target.kind() == "qualified_identifier"
        && target
            .child_by_field_name("scope")
            .is_some_and(|s| node_text(s, src) == "std")
    {
        return None;
    }
    // A dotted access (`.`) is a method call; a `::` path is not. In Python/JS/Go
    // module access also uses `.`, so those count as "method-like" here — a safe
    // over-approximation (they still resolve via same-file / import scope).
    let method = matches!(
        target.kind(),
        "field_expression" | "attribute" | "member_expression" | "selector_expression"
    );
    let name_node = match target.kind() {
        "field_expression" | "selector_expression" => target.child_by_field_name("field"),
        "member_expression" => target.child_by_field_name("property"),
        "attribute" => target.child_by_field_name("attribute"),
        _ => None,
    }
    .unwrap_or(target);
    let name = last_identifier(node_text(innermost_name(name_node), src))?;
    Some((name, method))
}

/// The last `[A-Za-z_][A-Za-z0-9_]*` run in `text`.
fn last_identifier(text: &str) -> Option<String> {
    let mut last: Option<String> = None;
    let mut cur = String::new();
    for ch in text.chars() {
        if ch == '_' || ch.is_alphanumeric() {
            cur.push(ch);
        } else if !cur.is_empty() {
            last = Some(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        last = Some(cur);
    }
    // A leading digit means it wasn't an identifier (e.g. a numeric literal).
    last.filter(|s| !s.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

/// Resolve a call to the project definitions it plausibly refers to, narrowing
/// by scope so a name isn't sprayed across every same-named definition:
///   1. a definition in the **same file** (a local helper), else
///   2. definitions in files the caller **imports** (in scope via `use`), else
///   3. for a *free* call only, a **globally unique** definition of that name
///      in a language that shares a namespace with the call site (a `.tsx`
///      component calling a `.ts` helper, C++ calling C).
///
/// A definition with a body always beats a bodyless declaration of the same
/// name (a C prototype in a header, an overload signature), which stands in
/// only when nothing defines the name. A method call that matches none of 1–2
/// resolves to nothing rather than guessing (its name belongs to a receiver
/// type we can't see).
fn resolve_callees(
    name_to: &HashMap<&str, Vec<usize>>,
    nodes: &[SymNode],
    facts: &[NodeFacts],
    call: &CallSite,
    site: &Site,
) -> Vec<usize> {
    let Some(all) = name_to.get(call.callee.as_str()) else {
        return Vec::new();
    };
    let defined: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&i| !facts[i].bodyless)
        .collect();
    let cands: &[usize] = if defined.is_empty() { all } else { &defined };
    let local: Vec<usize> = cands
        .iter()
        .copied()
        .filter(|&i| nodes[i].file == site.file)
        .collect();
    if !local.is_empty() {
        return local;
    }
    let scoped: Vec<usize> = cands
        .iter()
        .copied()
        .filter(|&i| site.imported.contains(&nodes[i].file))
        .collect();
    if !scoped.is_empty() {
        return scoped;
    }
    // A lone global definition of this name — accepted only when its language
    // can be reached from the call site's. Otherwise a Go `Foo()` would resolve
    // to a JS function named `Foo`, a spurious cross-language edge.
    if !call.method
        && let [only] = cands
        && facts[*only]
            .lang
            .is_some_and(|l| l.shares_namespace_with(site.lang))
    {
        return vec![*only];
    }
    Vec::new()
}

/// Bare-name calls to language builtins are not project functions. Without this
/// they resolve to any same-named definition — even one in another language
/// (a JS `make` in a vendored bundle) — and dominate the "most called" ranking.
fn is_builtin(lang: Lang, name: &str) -> bool {
    match lang {
        Lang::Go => matches!(
            name,
            "append"
                | "cap"
                | "clear"
                | "close"
                | "complex"
                | "copy"
                | "delete"
                | "imag"
                | "len"
                | "make"
                | "max"
                | "min"
                | "new"
                | "panic"
                | "print"
                | "println"
                | "real"
                | "recover"
        ),
        Lang::Python => matches!(
            name,
            "abs"
                | "all"
                | "any"
                | "bool"
                | "bytearray"
                | "bytes"
                | "callable"
                | "chr"
                | "dict"
                | "dir"
                | "enumerate"
                | "filter"
                | "float"
                | "format"
                | "frozenset"
                | "getattr"
                | "hasattr"
                | "hash"
                | "hex"
                | "id"
                | "input"
                | "int"
                | "isinstance"
                | "issubclass"
                | "iter"
                | "len"
                | "list"
                | "map"
                | "max"
                | "min"
                | "next"
                | "object"
                | "oct"
                | "open"
                | "ord"
                | "pow"
                | "print"
                | "range"
                | "repr"
                | "reversed"
                | "round"
                | "set"
                | "setattr"
                | "sorted"
                | "str"
                | "sum"
                | "super"
                | "tuple"
                | "type"
                | "vars"
                | "zip"
        ),
        // JS/TS global functions and constructors. A bare `parseInt(...)` or
        // `decodeURIComponent(...)` is a runtime builtin, not a project function
        // — without this it can resolve to a same-named helper (e.g. a polyfill
        // in a test file) and inflate the "most called" ranking.
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
            name,
            "Array"
                | "Boolean"
                | "Date"
                | "Error"
                | "Map"
                | "Number"
                | "Object"
                | "Promise"
                | "Proxy"
                | "RegExp"
                | "Set"
                | "String"
                | "Symbol"
                | "WeakMap"
                | "WeakSet"
                | "clearInterval"
                | "clearTimeout"
                | "decodeURI"
                | "decodeURIComponent"
                | "encodeURI"
                | "encodeURIComponent"
                | "fetch"
                | "isFinite"
                | "isNaN"
                | "parseFloat"
                | "parseInt"
                | "queueMicrotask"
                | "require"
                | "setInterval"
                | "setTimeout"
                | "structuredClone"
        ),
        // No bare-name builtins that a project would also define: a project
        // `malloc` wrapper IS what a C call to `malloc` links against, and
        // C++'s `std::` calls are excluded where they are read.
        Lang::Rust
        | Lang::Dart
        | Lang::C
        | Lang::Cpp
        | Lang::Java
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Zig => false,
    }
}

/// Resolve the caller a call site names — its enclosing function, by name —
/// to that function's definition in `file`.
///
/// Where several definitions share the name, the one the call is IN: the
/// innermost whose definition span (`bodies`, see [`FileCalls::bodies`])
/// holds the call's line — the rule `outline::Analysis::calls_made_by`
/// reads a function's calls by, so the graph and the reading context agree.
/// "The nearest preceding same-named definition" did not: a call made after
/// a nested same-name function closed went to that nested one. Without a
/// span that holds the call, that nearest preceding definition still
/// decides (falling back to the first when none precedes the call).
fn resolve_caller(
    by_file: &HashMap<&Path, Vec<usize>>,
    file: &Path,
    name: &str,
    call_line: usize,
    nodes: &[SymNode],
    bodies: &HashMap<usize, (usize, usize)>,
) -> Option<usize> {
    let ids = by_file.get(file)?;
    let mut innermost: Option<(usize, usize)> = None; // (id, span length)
    let mut best: Option<usize> = None;
    let mut first: Option<usize> = None;
    for &id in ids {
        if nodes[id].name != name {
            continue;
        }
        if first.is_none() {
            first = Some(id);
        }
        if nodes[id].line <= call_line {
            best = Some(id); // ids are line-sorted, so this keeps the closest
        }
        if let Some(&(lo, hi)) = bodies.get(&nodes[id].line)
            && (lo..=hi).contains(&call_line)
            && innermost.is_none_or(|(_, len)| hi - lo < len)
        {
            innermost = Some((id, hi - lo));
        }
    }
    innermost.map(|(id, _)| id).or(best).or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_call_sites_with_enclosing_fn() {
        let src = "\
fn helper() {}
fn caller() {
    helper();
    let v = Vec::<u8>::with_capacity(4);
    self.method_call();
}
";
        let calls = calls_of(src, "rust");
        // helper() call inside caller.
        let helper = calls.iter().find(|c| c.callee == "helper").unwrap();
        assert_eq!(helper.caller.as_deref(), Some("caller"));
        // Trailing identifier of a scoped/generic call.
        assert!(calls.iter().any(|c| c.callee == "with_capacity"));
        // Method call resolves to the trailing name.
        assert!(calls.iter().any(|c| c.callee == "method_call"));
    }

    #[test]
    fn turbofish_callee_is_the_function_not_the_type_arg() {
        let src = "\
fn caller() {
    let n = parse::<i32>(\"1\");
    let v = data.collect::<Vec<_>>();
}
";
        let calls = calls_of(src, "rust");
        let names: Vec<&str> = calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(names.contains(&"parse"), "turbofish free call: {names:?}");
        assert!(
            names.contains(&"collect"),
            "turbofish method call: {names:?}"
        );
        assert!(
            !names.contains(&"i32"),
            "type arg must not be the callee: {names:?}"
        );
        // The generic method call is still flagged as a method (no unique fallback).
        assert!(calls.iter().find(|c| c.callee == "collect").unwrap().method);
    }

    #[test]
    fn extracts_dart_call_sites_with_enclosing_fn() {
        // A class method and a top-level function, each calling others. Dart's
        // signature/body split means the caller name must be recovered from the
        // signature preceding each `function_body`.
        let src = "\
class Greeter {
  void hello() {
    _build();
    print('hi');
  }
  void _build() {}
}
void main() {
  Greeter().hello();
}
";
        let calls = calls_of(src, "dart");
        // Calls inside the method are attributed to `hello`, not dropped.
        assert!(
            calls
                .iter()
                .any(|c| c.caller.as_deref() == Some("hello") && c.callee == "_build"),
            "hello -> _build: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.caller.as_deref() == Some("hello") && c.callee == "print"),
            "hello -> print: {calls:?}"
        );
        // The top-level function's method call is attributed to `main`.
        assert!(
            calls
                .iter()
                .any(|c| c.caller.as_deref() == Some("main") && c.callee == "hello"),
            "main -> hello: {calls:?}"
        );
    }

    #[test]
    fn arrow_functions_are_enclosing_scopes() {
        let src = "const handler = () => { doThing(); };\nfunction main() { other(); }\n";
        let calls = calls_of(src, "typescript");
        // The call inside the arrow is attributed to its binding name, not dropped.
        let do_thing = calls.iter().find(|c| c.callee == "doThing").unwrap();
        assert_eq!(do_thing.caller.as_deref(), Some("handler"));
        assert_eq!(
            calls
                .iter()
                .find(|c| c.callee == "other")
                .unwrap()
                .caller
                .as_deref(),
            Some("main")
        );
    }

    #[test]
    fn python_and_js_call_sites() {
        let py = "def a():\n    b()\n    obj.c()\n";
        let calls = calls_of(py, "python");
        assert_eq!(
            calls
                .iter()
                .find(|c| c.callee == "b")
                .unwrap()
                .caller
                .as_deref(),
            Some("a")
        );
        assert!(calls.iter().any(|c| c.callee == "c"));

        let js = "function f() { g(); this.h(); new Widget(); }";
        let calls = calls_of(js, "typescript");
        assert_eq!(
            calls
                .iter()
                .find(|c| c.callee == "g")
                .unwrap()
                .caller
                .as_deref(),
            Some("f")
        );
        assert!(calls.iter().any(|c| c.callee == "Widget"));
    }

    fn def(name: &str, file: &str, line: usize) -> Def {
        Def {
            name: name.into(),
            kind: "function".into(),
            file: PathBuf::from(file),
            line,
        }
    }

    #[test]
    fn builds_graph_and_finds_hubs_and_uncalled() {
        let defs = vec![
            def("main", "/p/a.rs", 1),
            def("used", "/p/a.rs", 10),
            def("lonely", "/p/b.rs", 1),
        ];
        let sources = vec![
            (
                PathBuf::from("/p/a.rs"),
                "fn main() {\n    used();\n    used();\n}\nfn used() {}\n".to_string(),
            ),
            (PathBuf::from("/p/b.rs"), "fn lonely() {}\n".to_string()),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node_count(), 3);

        // `used` is called (edge exists, deduped to one); it is a hub.
        let hubs = g.most_called(10);
        assert_eq!(hubs.len(), 1);
        assert_eq!(g.node(hubs[0]).name, "used");
        assert_eq!(g.node(hubs[0]).caller_count(), 1); // main→used, deduped

        // main and lonely have no callers.
        let uncalled: Vec<&str> = g
            .uncalled()
            .iter()
            .map(|&i| g.node(i).name.as_str())
            .collect();
        assert!(uncalled.contains(&"main"));
        assert!(uncalled.contains(&"lonely"));
        assert!(!uncalled.contains(&"used"));
    }

    #[test]
    fn self_recursion_is_not_a_caller() {
        let defs = vec![def("fac", "/p/a.rs", 1)];
        let sources = vec![(
            PathBuf::from("/p/a.rs"),
            "fn fac(n: u64) -> u64 {\n    fac(n - 1)\n}\n".to_string(),
        )];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        // The self-call is dropped, so `fac` still counts as uncalled.
        assert_eq!(g.node(g.uncalled()[0]).name, "fac");
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn method_call_does_not_spray_across_unimported_files() {
        // Only one project function is named `get`, but the call `x.get()` is a
        // method on some type in an unrelated file that doesn't import it.
        let defs = vec![
            def("get", "/p/store.rs", 1),
            def("caller", "/p/other.rs", 1),
        ];
        let sources = vec![
            (PathBuf::from("/p/store.rs"), "fn get() {}\n".to_string()),
            (
                PathBuf::from("/p/other.rs"),
                "fn caller() {\n    let m = make();\n    m.get();\n}\n".to_string(),
            ),
        ];
        // other.rs does NOT import store.rs.
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        // The method call is not attributed to the unrelated `get`.
        assert_eq!(g.node(id_named(&g, "get")).caller_count(), 0);
    }

    #[test]
    fn call_resolves_to_an_imported_file() {
        // `caller` in a.rs calls a free function `helper` defined in b.rs, which
        // a.rs imports — so it resolves across files.
        let defs = vec![def("caller", "/p/a.rs", 1), def("helper", "/p/b.rs", 1)];
        let sources = vec![
            (
                PathBuf::from("/p/a.rs"),
                "fn caller() {\n    helper();\n}\n".to_string(),
            ),
            (PathBuf::from("/p/b.rs"), "fn helper() {}\n".to_string()),
        ];
        let mut scope = HashMap::new();
        scope.insert(
            PathBuf::from("/p/a.rs"),
            HashSet::from([PathBuf::from("/p/b.rs")]),
        );
        let g = ProjectCallGraph::build(defs, &sources, &scope);
        assert_eq!(g.node(id_named(&g, "helper")).caller_count(), 1);

        // Without the import in scope, a free call to a unique name still links
        // (globally-unique fallback), but a shared name would not.
        let g2 = ProjectCallGraph::build(
            vec![def("caller", "/p/a.rs", 1), def("helper", "/p/b.rs", 1)],
            &sources,
            &HashMap::new(),
        );
        assert_eq!(g2.node(id_named(&g2, "helper")).caller_count(), 1);
    }

    fn id_named(g: &ProjectCallGraph, name: &str) -> usize {
        (0..g.node_count())
            .find(|&i| g.node(i).name == name)
            .unwrap()
    }

    #[test]
    fn builtin_calls_do_not_link_to_same_named_definitions() {
        // A Go `make(...)` builtin call must not resolve to a project function
        // named `make` (here in a vendored JS bundle) — that inflated the graph.
        let defs = vec![def("run", "/p/a.go", 1), def("make", "/p/vendor.js", 1)];
        let sources = vec![
            (
                PathBuf::from("/p/a.go"),
                "func run() {\n\tmake([]int, 0)\n}\n".to_string(),
            ),
            (
                PathBuf::from("/p/vendor.js"),
                "function make() {}\n".to_string(),
            ),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node(id_named(&g, "make")).caller_count(), 0);
    }

    #[test]
    fn js_builtin_calls_do_not_link_to_a_same_named_polyfill() {
        // A bare `parseInt(...)` is a JS runtime builtin, not this project's
        // `parseInt` polyfill defined in a helper file.
        let defs = vec![
            def("run", "/p/a.js", 1),
            def("parseInt", "/p/polyfill.js", 1),
        ];
        let sources = vec![
            (
                PathBuf::from("/p/a.js"),
                "function run() {\n  parseInt('10', 10)\n}\n".to_string(),
            ),
            (
                PathBuf::from("/p/polyfill.js"),
                "function parseInt(s, r) { return 0 }\n".to_string(),
            ),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node(id_named(&g, "parseInt")).caller_count(), 0);
    }

    #[test]
    fn lone_global_fallback_is_restricted_to_the_same_language() {
        // A Go `Widget()` call has one globally-unique definition — but it's in a
        // JS file. A Go call can't invoke a JS function by bare name, so no edge.
        let defs = vec![def("run", "/p/a.go", 1), def("Widget", "/p/vendor.js", 1)];
        let sources = vec![
            (
                PathBuf::from("/p/a.go"),
                "func run() {\n\tWidget()\n}\n".to_string(),
            ),
            (
                PathBuf::from("/p/vendor.js"),
                "function Widget() {}\n".to_string(),
            ),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node(id_named(&g, "Widget")).caller_count(), 0);
    }

    #[test]
    fn graph_from_sym_edges_maps_and_survives_line_shifts() {
        let defs = vec![
            def("a", "/p/x.rs", 1),
            def("b", "/p/x.rs", 5),
            def("c", "/p/y.rs", 1),
        ];
        let key = |file: &str, name: &str| (PathBuf::from(file), name.to_string(), 0u32);
        let edges: SymEdges = HashSet::from([
            (key("/p/x.rs", "a"), key("/p/x.rs", "b")),    // a → b
            (key("/p/y.rs", "c"), key("/p/x.rs", "a")),    // c → a
            (key("/p/x.rs", "a"), key("/p/gone.rs", "z")), // dangling → dropped
        ]);
        let g = ProjectCallGraph::graph_from_sym_edges(defs, &edges);
        assert_eq!(g.node(id_named(&g, "b")).caller_count(), 1);
        assert_eq!(g.node(id_named(&g, "a")).caller_count(), 1);
        assert_eq!(g.edge_count(), 2, "dangling edge dropped");

        // The same edges resolve even after `a`/`b` move to different lines —
        // the key is (file, name, ordinal), never the line.
        let shifted = vec![
            def("a", "/p/x.rs", 40),
            def("b", "/p/x.rs", 88),
            def("c", "/p/y.rs", 3),
        ];
        let g2 = ProjectCallGraph::graph_from_sym_edges(shifted, &edges);
        assert_eq!(g2.edge_count(), 2);
        assert_eq!(g2.node(id_named(&g2, "b")).caller_count(), 1);
    }

    #[test]
    fn from_callable_defs_builds_from_explicit_edges() {
        let defs = vec![
            def("a", "/p/x.rs", 1),
            def("b", "/p/x.rs", 5),
            def("c", "/p/y.rs", 1),
        ];
        // a→b, a→c (explicit, e.g. LSP-resolved), plus a self-edge that's dropped.
        let edges = HashSet::from([(0, 1), (0, 2), (1, 1)]);
        let g = ProjectCallGraph::from_callable_defs(defs, edges);
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.node(id_named(&g, "a")).callee_count(), 2);
        assert_eq!(g.node(id_named(&g, "b")).caller_count(), 1);
        assert_eq!(g.node(id_named(&g, "c")).caller_count(), 1);
        // The dropped self-edge means b still has no *other* caller than a.
        assert_eq!(g.edge_count(), 2);
    }

    #[test]
    fn paths_from_entries_are_shortest_ordered_bounded_and_end_at_entries() {
        // main → run → handle → work; route → handle; test_work → work;
        // work → work (recursion); helper → main (a caller of an entry).
        let names = [
            "main",
            "run",
            "handle",
            "work",
            "route",
            "test_work",
            "helper",
            "lonely",
        ];
        let defs: Vec<Def> = names
            .iter()
            .enumerate()
            .map(|(i, n)| def(n, "a.rs", i + 1))
            .collect();
        let edges: HashSet<(usize, usize)> =
            [(0, 1), (1, 2), (2, 3), (4, 2), (5, 3), (3, 3), (6, 0)]
                .into_iter()
                .collect();
        let g = ProjectCallGraph::from_callable_defs(defs, edges);
        let class = |n: &SymNode| match n.name.as_str() {
            "main" | "route" => Some(0),
            "test_work" => Some(1),
            _ => None,
        };
        let name = |chain: &[usize]| {
            chain
                .iter()
                .map(|&i| g.node(i).name.as_str())
                .collect::<Vec<_>>()
                .join(" > ")
        };
        let work = g.id_of(Path::new("a.rs"), "work").unwrap();
        let paths = g.paths_from_entries(work, class, 8, 12);
        assert_eq!(
            paths.iter().map(|p| name(p)).collect::<Vec<_>>(),
            [
                "route > handle > work",
                "main > run > handle > work",
                "test_work > work"
            ]
        );
        // The limit and the depth bound each cut the list.
        assert_eq!(g.paths_from_entries(work, class, 1, 12).len(), 1);
        assert_eq!(
            g.paths_from_entries(work, class, 8, 2)
                .iter()
                .map(|p| name(p))
                .collect::<Vec<_>>(),
            ["route > handle > work", "test_work > work"]
        );
        // An entry itself, and a function nothing ranked reaches, have none.
        let main = g.id_of(Path::new("a.rs"), "main").unwrap();
        assert!(g.paths_from_entries(main, class, 8, 12).is_empty());
        let lonely = g.id_of(Path::new("a.rs"), "lonely").unwrap();
        assert!(g.paths_from_entries(lonely, class, 8, 12).is_empty());
        assert!(g.paths_from_entries(999, class, 8, 12).is_empty());
    }

    #[test]
    fn last_identifier_handles_paths_and_rejects_numbers() {
        assert_eq!(last_identifier("self.foo.bar").as_deref(), Some("bar"));
        assert_eq!(
            last_identifier("Vec::<u8>::with_capacity").as_deref(),
            Some("with_capacity")
        );
        assert_eq!(last_identifier("42").as_deref(), None);
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    /// Defs exactly as the indexers produce them: every callable in each
    /// file's outline.
    fn defs_of(files: &[(&str, &str)]) -> Vec<Def> {
        let mut defs = Vec::new();
        for (file, src) in files {
            let lang = crate::highlight::detect(Path::new(file)).unwrap();
            for s in crate::outline::extract(src, lang) {
                defs.push(Def {
                    name: s.name,
                    kind: s.kind,
                    file: PathBuf::from(file),
                    line: s.line,
                });
            }
        }
        defs
    }

    fn graph(files: &[(&str, &str)]) -> ProjectCallGraph {
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        ProjectCallGraph::build(defs_of(files), &sources, &HashMap::new())
    }

    /// The call facts a symbol index keeps — `FileCalls::of` over the one
    /// `outline::analyze` it makes of each file — are exactly what
    /// `FileCalls::read` parses for, and `build_from_calls` over them is the
    /// graph `build` gets by parsing every file again: for no parse at all.
    #[test]
    fn calls_kept_from_the_index_parse_build_the_parsed_graph_without_a_parse() {
        let files: &[(&str, &str)] = &[
            (
                "/p/a.rs",
                "fn helper() {}\nfn run() { helper(); other(); }\n\
                 struct S;\nimpl S {\n    fn new() -> S { helper(); S }\n}\n",
            ),
            (
                "/p/b.py",
                "def other():\n    run()\n\ndef run():\n    other()\n",
            ),
            (
                "/p/c.h",
                "int proto(int x);\nint use_it(void) { return proto(1); }\n\
                 int proto(int x) { return x; }\n",
            ),
            ("/p/d.js", "function a() { b(); }\nconst b = () => a();\n"),
            ("/p/e.toml", "a = 1\n"),
        ];
        let defs = defs_of(files);
        let mut kept = Vec::new();
        for (file, src) in files {
            let lang = crate::highlight::detect(Path::new(file)).unwrap();
            let Some(a) = crate::outline::analyze(src, lang) else {
                continue;
            };
            let of = FileCalls::of(PathBuf::from(file), a.lang, a.calls, &a.symbols);
            let read = FileCalls::read(Path::new(file), src);
            assert_eq!(
                of.as_ref()
                    .map(|c| (&c.file, c.lang, &c.calls, &c.declarations)),
                read.as_ref()
                    .map(|c| (&c.file, c.lang, &c.calls, &c.declarations)),
                "{file}"
            );
            kept.extend(of);
        }
        assert!(
            kept.iter().any(|c| !c.declarations.is_empty()),
            "a prototype is kept as a declaration"
        );
        let reads = crate::highlight::parses_on_this_thread();
        let kept_graph = ProjectCallGraph::build_from_calls(defs.clone(), &kept, &HashMap::new());
        assert_eq!(
            crate::highlight::parses_on_this_thread(),
            reads,
            "linking kept call sites parses nothing"
        );
        let parsed = graph(files);
        let wire = |g: &ProjectCallGraph| g.to_wire(|p| p.to_string_lossy().into_owned());
        assert_eq!(wire(&kept_graph), wire(&parsed));
        assert!(kept_graph.edge_count() >= 6, "{:?}", edges(&kept_graph));
    }

    /// Every `caller -> callee` edge as `(caller file:name, callee file:name)`.
    fn edges(g: &ProjectCallGraph) -> Vec<(String, String)> {
        let label = |i: usize| format!("{}:{}", g.node(i).file.display(), g.node(i).name);
        let mut out = Vec::new();
        for i in 0..g.node_count() {
            for &c in g.callees_of(i) {
                out.push((label(i), label(c)));
            }
        }
        out.sort();
        out
    }

    fn has(g: &ProjectCallGraph, from: &str, to: &str) -> bool {
        edges(g).contains(&(from.to_string(), to.to_string()))
    }

    /// Calls inside unnamed functions belong to the nearest function the
    /// outline has. They used to be attributed to the binding's name — a
    /// class field, `a.b`, `Foo.prototype.bar`, a computed key — and dropped,
    /// since no node carried that name.
    #[test]
    fn calls_in_anonymous_functions_reach_a_real_node() {
        let js = "\
function helper() {}
class Widget {
  onClick = () => { helper(); };
}
const api = {};
api.load = function () { helper(); };
function Legacy() {}
Legacy.prototype.render = function () { helper(); };
function build() {
  return { [KEY]: () => helper() };
}
";
        let g = graph(&[("/p/w.js", js)]);
        for from in ["onClick", "load", "render", "build"] {
            assert!(
                has(&g, &format!("/p/w.js:{from}"), "/p/w.js:helper"),
                "{from} -> helper missing: {:?}",
                edges(&g)
            );
        }

        let ts = "\
function helper(): void {}
export class Widget {
  onClick = (): void => { helper(); };
}
export const handlers = {
  save: () => helper(),
};
function build() {
  return { [KEY]: () => helper() };
}
";
        let g = graph(&[("/p/w.ts", ts)]);
        for from in ["onClick", "save", "build"] {
            assert!(
                has(&g, &format!("/p/w.ts:{from}"), "/p/w.ts:helper"),
                "{from} -> helper missing: {:?}",
                edges(&g)
            );
        }
    }

    /// C: calls resolve to the DEFINITION, never to a header prototype, and
    /// across files by the unique-name rule.
    #[test]
    fn c_calls_link_definitions_not_prototypes() {
        let header = "int helper(void);\nint unused_decl(int);\n";
        let lib = "static int\nhelper(void)\n{\n  return 1;\n}\n";
        let main = "#include \"util.h\"\nint main(void) {\n  return helper() + local();\n}\nint local(void) { return 0; }\n";
        let g = graph(&[
            ("/p/util.h", header),
            ("/p/util.c", lib),
            ("/p/main.c", main),
        ]);
        assert!(
            has(&g, "/p/main.c:main", "/p/util.c:helper"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/main.c:main", "/p/main.c:local"),
            "{:?}",
            edges(&g)
        );
        assert!(
            !has(&g, "/p/main.c:main", "/p/util.h:helper"),
            "a call landed on the prototype: {:?}",
            edges(&g)
        );
    }

    #[test]
    fn cpp_calls_through_qualified_template_and_member_forms() {
        let src = "\
namespace util { template <typename T> T twice(T x) { return x; } }
void helper() {}
struct Box { void open(); };
void Box::open() {
  util::twice<int>(1);
  helper();
  std::move(1);
}
void run() {
  Box b;
  b.open();
  Box *p = new Box();
}
void move(int) {}
";
        let g = graph(&[("/p/box.cpp", src)]);
        assert!(
            has(&g, "/p/box.cpp:open", "/p/box.cpp:twice"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/box.cpp:open", "/p/box.cpp:helper"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/box.cpp:run", "/p/box.cpp:open"),
            "{:?}",
            edges(&g)
        );
        assert!(
            !has(&g, "/p/box.cpp:open", "/p/box.cpp:move"),
            "std::move is not the project's move: {:?}",
            edges(&g)
        );
    }

    #[test]
    fn java_methods_and_constructors_are_linked() {
        let src = "\
class Account {
  Account() { init(); }
  void init() {}
  void transfer(Account other) {
    audit();
    other.init();
    Account copy = new Account();
  }
  static void audit() {}
}
";
        let g = graph(&[("/p/Account.java", src)]);
        assert!(
            has(&g, "/p/Account.java:Account", "/p/Account.java:init"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/Account.java:transfer", "/p/Account.java:audit"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/Account.java:transfer", "/p/Account.java:init"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/Account.java:transfer", "/p/Account.java:Account"),
            "`new Account()` reaches the constructor: {:?}",
            edges(&g)
        );
    }

    /// Macro arguments are token trees, not expressions; the calls in them
    /// are read off the tokens. Attributes and `macro_rules!` bodies are not
    /// calls.
    #[test]
    fn rust_calls_inside_macro_arguments_are_edges() {
        let src = "\
fn helper() -> i32 { 1 }
fn compute(x: i32) -> i32 { x }
fn check(v: &[i32]) -> bool { true }
#[cfg(all(test, unix))]
fn run() {
    println!(\"{}\", helper());
    assert_eq!(compute(1), 2);
    let v = vec![helper(), compute(2)];
    assert!(check(&v));
}
macro_rules! m { () => { compute(3) }; }
";
        let g = graph(&[("/p/lib.rs", src)]);
        for callee in ["helper", "compute", "check"] {
            assert!(
                has(&g, "/p/lib.rs:run", &format!("/p/lib.rs:{callee}")),
                "run -> {callee} missing: {:?}",
                edges(&g)
            );
        }
        let calls = calls_of(src, "rust");
        assert!(
            !calls.iter().any(|c| c.callee == "all"),
            "an attribute is not a call: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.callee == "compute" && c.caller.is_none() && c.line == 11),
            "a macro_rules! body is not a call site: {calls:?}"
        );
    }

    /// A bare call reaches a unique definition in any language that links
    /// with the caller's: `.tsx` → `.ts`, C++ → C. Never Go → JavaScript.
    #[test]
    fn the_unique_name_fallback_crosses_into_the_same_runtime_only() {
        let g = graph(&[
            (
                "/p/App.tsx",
                "export function App() { return formatName(); }\n",
            ),
            (
                "/p/names.ts",
                "export function formatName(): string { return ''; }\n",
            ),
        ]);
        assert!(
            has(&g, "/p/App.tsx:App", "/p/names.ts:formatName"),
            "{:?}",
            edges(&g)
        );

        let g = graph(&[
            ("/p/a.cpp", "void run() { c_helper(); }\n"),
            ("/p/b.c", "void c_helper(void) {}\n"),
        ]);
        assert!(
            has(&g, "/p/a.cpp:run", "/p/b.c:c_helper"),
            "{:?}",
            edges(&g)
        );

        let g = graph(&[
            ("/p/a.go", "package p\nfunc run() {\n\tWidget()\n}\n"),
            ("/p/w.js", "function Widget() {}\n"),
        ]);
        assert!(edges(&g).is_empty(), "{:?}", edges(&g));
    }

    /// Same-name functions in one file are distinct identities everywhere:
    /// `callee_keys` used to keep only the last one's callees, and
    /// `graph_from_sym_edges` mapped every key onto the first.
    #[test]
    fn same_name_functions_keep_their_own_identity() {
        let src = "\
struct A;
struct B;
impl A {
    fn new() -> A { build_a() }
}
impl B {
    fn new() -> B { build_b() }
}
fn build_a() -> A { A }
fn build_b() -> B { B }
";
        let g = graph(&[("/p/ab.rs", src)]);
        let keys = g.callee_keys();
        let key = |name: &str, ordinal: u32| (PathBuf::from("/p/ab.rs"), name.to_string(), ordinal);
        assert_eq!(keys[&key("new", 0)], vec![key("build_a", 0)], "{keys:?}");
        assert_eq!(keys[&key("new", 1)], vec![key("build_b", 0)], "{keys:?}");

        let second = g.id_of_key(Path::new("/p/ab.rs"), "new", 1).unwrap();
        assert_eq!(g.node(second).line, 7);
        assert_eq!(g.key_of(second), Some(key("new", 1)));
        assert_eq!(
            g.id_of(Path::new("/p/ab.rs"), "new")
                .map(|i| g.node(i).line),
            Some(4)
        );
        assert_eq!(g.id_of_key(Path::new("/p/ab.rs"), "new", 2), None);

        // The LSP-precise path keys the same way.
        let defs = ProjectCallGraph::callable(&defs_of(&[("/p/ab.rs", src)]));
        let edges: SymEdges = HashSet::from([(key("new", 1), key("build_b", 0))]);
        let g2 = ProjectCallGraph::graph_from_sym_edges(defs, &edges);
        let from = g2.id_of_key(Path::new("/p/ab.rs"), "new", 1).unwrap();
        assert_eq!(g2.callees_of(from).len(), 1);
        let first = g2.id_of_key(Path::new("/p/ab.rs"), "new", 0).unwrap();
        assert!(
            g2.callees_of(first).is_empty(),
            "the edge landed on the other `new`"
        );
    }

    /// Nested same-name functions: a call's caller is the definition it is
    /// IN — the innermost whose span holds it — not the nearest one above
    /// it. So a call the outer `f` makes after the inner `f` closed is the
    /// outer's, and the graph lists for each `f` exactly the calls the
    /// reading context (`calls_made_by`) lists for it.
    #[test]
    fn a_call_after_a_nested_same_name_function_is_the_outer_ones() {
        let file = "/p/n.js";
        let src = "\
function f() {
  function f() {
    inner();
  }
  outer();
}
function inner() {}
function outer() {}
";
        let g = graph(&[(file, src)]);
        let key = |name: &str, ordinal: u32| (PathBuf::from(file), name.to_string(), ordinal);
        let keys = g.callee_keys();
        assert_eq!(keys[&key("f", 0)], vec![key("outer", 0)], "{keys:?}");
        assert_eq!(keys[&key("f", 1)], vec![key("inner", 0)], "{keys:?}");

        let analysis = crate::outline::analyze(src, "javascript").unwrap();
        for (ordinal, expected) in [(0, "outer"), (1, "inner")] {
            let item = analysis.function("f", ordinal).unwrap();
            let read: Vec<&str> = analysis
                .calls_made_by(item)
                .map(|c| c.callee.as_str())
                .collect();
            assert_eq!(
                read,
                [expected],
                "the reading context agrees for f#{ordinal}"
            );
        }
        // Only a shared name needs its spans kept.
        let kept = FileCalls::of(
            PathBuf::from(file),
            analysis.lang,
            analysis.calls.clone(),
            &analysis.symbols,
        )
        .unwrap();
        assert_eq!(kept.bodies.len(), 2, "{:?}", kept.bodies);
    }

    /// A named function expression is known by its binding: its calls belong
    /// to `x`, the node callers reach, not to `named`, which only its own body
    /// can see — and which the TypeScript outline does not list, so the calls
    /// used to fall to whatever function enclosed it (or be dropped).
    #[test]
    fn a_named_function_expression_is_scoped_by_its_binding() {
        for (file, src) in [
            (
                "/p/a.ts",
                "function helper(): void {}\nfunction outer() {\n  const x = function named() { helper(); };\n  x();\n}\n",
            ),
            (
                "/p/a.js",
                "function helper() {}\nfunction outer() {\n  const x = function named() { helper(); };\n  x();\n}\n",
            ),
        ] {
            let g = graph(&[(file, src)]);
            assert!(
                has(&g, &format!("{file}:x"), &format!("{file}:helper")),
                "{file}: {:?}",
                edges(&g)
            );
            assert!(
                !has(&g, &format!("{file}:outer"), &format!("{file}:helper")),
                "{file}: the call was attributed to the enclosing function: {:?}",
                edges(&g)
            );
        }
        // An object entry's function is known by its key the same way.
        let src = "function helper(): void {}\nexport const api = {\n  save: function saveImpl() { helper(); },\n};\n";
        let g = graph(&[("/p/b.ts", src)]);
        assert!(has(&g, "/p/b.ts:save", "/p/b.ts:helper"), "{:?}", edges(&g));
    }

    /// One numbering everywhere: the graph's keys are the outline's ordinals
    /// (`outline::fn_ordinals`, which keys the explain cache), including when
    /// a same-name TYPE sits among the functions, and `key_of` / `id_of_key`
    /// invert each other — also for callables that share a line.
    #[test]
    fn graph_keys_are_the_outline_ordinals() {
        let src = "\
struct new;
impl A {
    fn new() -> A { A }
}
impl B {
    fn new() -> B { B }
}
";
        let symbols = crate::outline::extract(src, "rust");
        let outline = crate::outline::fn_ordinals(&symbols);
        let all_defs: Vec<Def> = symbols
            .iter()
            .map(|s| Def {
                name: s.name.clone(),
                kind: s.kind.clone(),
                file: PathBuf::from("/p/k.rs"),
                line: s.line,
            })
            .collect();
        // `keys_of` over every def, the type included: the callables keep
        // the outline's numbers (the type used to count as a `new`).
        let keys = ProjectCallGraph::keys_of(&all_defs);
        for ((s, key), want) in symbols.iter().zip(&keys).zip(&outline) {
            if crate::outline::is_callable(&s.kind) {
                assert_eq!(key.2, *want, "{s:?}");
            }
        }

        let g = graph(&[("/p/k.rs", src)]);
        for id in 0..g.node_count() {
            let n = g.node(id);
            let s = symbols
                .iter()
                .position(|s| s.name == n.name && s.line == n.line)
                .expect("every node is an outline entry");
            assert_eq!(g.ordinals()[id], outline[s], "{n:?}");
            let key = g.key_of(id).unwrap();
            let back = g.id_of_key(&key.0, &key.1, key.2).unwrap();
            assert_eq!(
                (g.node(back).name.as_str(), g.node(back).line),
                (n.name.as_str(), n.line),
                "{key:?}"
            );
        }
        assert!(g.id_of_key(Path::new("/p/k.rs"), "new", 1).is_some());
        assert_eq!(g.id_of_key(Path::new("/p/k.rs"), "new", 2), None);

        // Two callables on one line share an ordinal, and the next one counts
        // both: 0, 0, 2 — and ordinal 0 names the first of the two.
        let def = |line: usize| Def {
            name: "pair".into(),
            kind: "function".into(),
            file: PathBuf::from("/p/k.rs"),
            line,
        };
        let g = ProjectCallGraph::from_callable_defs(vec![def(8), def(8), def(9)], HashSet::new());
        assert_eq!(g.ordinals(), &[0, 0, 2]);
        let file = Path::new("/p/k.rs");
        assert_eq!(g.id_of_key(file, "pair", 0), Some(0));
        assert_eq!(g.id_of_key(file, "pair", 1), None);
        assert_eq!(g.id_of_key(file, "pair", 2), Some(2));
    }

    /// A graph from the wire is checked before anything indexes with it, and
    /// a good one survives the trip with its edges and identities intact.
    #[test]
    fn a_malformed_graph_is_rejected_not_trusted() {
        let good = graph(&[("/p/a.rs", "fn a() { b(); }\nfn b() {}\n")]);
        let wire = good.to_wire(|p| p.strip_prefix("/p").unwrap().to_string_lossy().into_owned());
        assert!(wire.nodes.iter().all(|n| n.file == "a.rs"));
        let back = ProjectCallGraph::from_wire(wire, |rel| Path::new("/q").join(rel))
            .expect("a real graph converts");
        assert_eq!(back.edge_count(), 1);
        assert!(back.id_of(Path::new("/q/a.rs"), "b").is_some());

        let node = |callers: Vec<usize>, callees: Vec<usize>| clew_protocol::CallGraphNode {
            name: "a".into(),
            kind: "function".into(),
            file: "a.rs".into(),
            line: 1,
            callers,
            callees,
        };
        let bad = clew_protocol::CallGraph {
            nodes: vec![node(vec![], vec![7])],
        };
        let err = ProjectCallGraph::from_wire(bad, |rel| PathBuf::from(rel))
            .expect_err("index 7 of 1 node");
        assert!(err.contains("links to node 7"), "{err}");
        let bad = clew_protocol::CallGraph {
            nodes: vec![node(vec![1], vec![])],
        };
        assert!(ProjectCallGraph::from_wire(bad, |rel| PathBuf::from(rel)).is_err());
        assert!(back.get(5).is_none());
    }
}
