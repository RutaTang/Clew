//! The LLM code-explanation engine.
//!
//! Explanations are built **bottom-up** over two axes that compose:
//!   * the **call graph** — a function is explained after the functions it
//!     calls, so its prompt can include their summaries (not their bodies), and
//!   * the **containment tree** — a file is explained after its functions, a
//!     folder after its files and subfolders — giving architecture-level
//!     summaries.
//!
//! Both reduce to one dependency DAG: an edge `X → Y` means "Y must be explained
//! first; Y's summary feeds X's prompt". A dependencies-first topological order
//! (with call cycles condensed into strongly-connected groups) drives the pass.
//!
//! Incrementality falls out for free: each node's cache key is the **hash of its
//! prompt**. A prompt embeds the node's own content plus every dependency's
//! summary, so changing one function re-explains it, then anything whose prompt
//! transitively contained its summary (its callers, its file, that file's folder
//! chain) — and nothing else, since unchanged prompts hash the same and hit the
//! cache. This is the payoff of the incremental foundation.


use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::incremental::{Version, content_hash};

/// A thing that can be explained.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Node {
    Function { file: PathBuf, name: String },
    File(PathBuf),
    Folder(PathBuf),
}

impl Node {
    /// The file this node lives in (its own path for files, its dir for folders),
    /// used to decide which nodes a changed file invalidates.
    pub fn path(&self) -> &std::path::Path {
        match self {
            Node::Function { file, .. } => file,
            Node::File(p) | Node::Folder(p) => p,
        }
    }
}

/// A function to summarize: its text and the project-internal functions it calls.
#[derive(Debug, Clone)]
pub struct FnInput {
    pub file: PathBuf,
    pub name: String,
    pub signature: String,
    pub body: String,
    /// Call-graph out-edges kept to the project (external callees are dropped).
    pub callees: Vec<(PathBuf, String)>,
}

/// A file to summarize: its functions and a rendering of its top-level structure
/// (types, exports, imports, module doc).
#[derive(Debug, Clone)]
pub struct FileInput {
    pub path: PathBuf,
    pub functions: Vec<(PathBuf, String)>,
    pub structure: String,
}

/// A folder to summarize: its direct files and subfolders.
#[derive(Debug, Clone)]
pub struct FolderInput {
    pub path: PathBuf,
    pub files: Vec<PathBuf>,
    pub subfolders: Vec<PathBuf>,
}

#[derive(Debug, Default, Clone)]
pub struct Inputs {
    pub functions: Vec<FnInput>,
    pub files: Vec<FileInput>,
    pub folders: Vec<FolderInput>,
}

/// A cached explanation: the summary plus the hash of the prompt that produced
/// it, so a changed prompt (own content or any dependency's summary) misses.
/// `detail` is the optional, on-demand block-by-block walkthrough of a function
/// (see [`detail_prompt`]); it is dropped whenever the entry is regenerated, so
/// it never outlives the summary it belongs to.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cached {
    pub summary: String,
    pub prompt_hash: Version,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Explanations keyed by node.
pub type Cache = HashMap<Node, Cached>;

/// The placeholder produced when an LLM explanation call fails. Such a summary
/// must never be cached as if it were real, reused, or shown as an explanation —
/// see the guards in the explain orchestrator and the reading-context UI.
pub const FAILED_SUMMARY_PREFIX: &str = "(explanation unavailable";

/// Whether a summary is a failure placeholder rather than a real explanation.
pub fn is_error_summary(s: &str) -> bool {
    s.trim_start().starts_with(FAILED_SUMMARY_PREFIX)
}

/// Serialize a cache to `(Node, Cached)` pairs (a map with enum keys isn't valid
/// JSON) for persistence under `.clew/`.
pub fn cache_to_pairs(cache: &Cache) -> Vec<(Node, Cached)> {
    cache.iter().map(|(n, c)| (n.clone(), c.clone())).collect()
}

pub fn cache_from_pairs(pairs: Vec<(Node, Cached)>) -> Cache {
    pairs.into_iter().collect()
}

fn cache_path(root: &Path) -> PathBuf {
    root.join(".clew").join("cache").join("explain.json")
}

/// Load the persisted explanation cache (empty on any error).
pub fn load(root: &Path) -> Cache {
    std::fs::read_to_string(cache_path(root))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<(Node, Cached)>>(&s).ok())
        .map(cache_from_pairs)
        .unwrap_or_default()
}

/// Persist the explanation cache (atomic temp+rename).
pub fn save(root: &Path, cache: &Cache) -> std::io::Result<()> {
    let path = cache_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(&cache_to_pairs(cache))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// How many summaries a pass reused from cache vs. (re)generated. (Produced by
/// the sequential [`run`] reference; the live pass streams progress instead.)
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct Stats {
    pub reused: usize,
    pub generated: usize,
}

/// One unit of work: a strongly-connected group of nodes (a lone node, or
/// mutually-recursive functions) plus the indices of the groups it depends on
/// (all lower — the schedule is dependencies-first).
#[derive(Debug, Clone)]
pub struct Group {
    pub nodes: Vec<Node>,
    pub deps: Vec<usize>,
}

impl Group {
    /// The node whose cache entry represents this group (its first node).
    pub fn key(&self) -> &Node {
        &self.nodes[0]
    }
}

impl Inputs {
    fn fn_key(f: &FnInput) -> Node {
        Node::Function { file: f.file.clone(), name: f.name.clone() }
    }
}

/// Build the dependencies-first group schedule over the call graph + containment
/// tree. Each group's `deps` reference earlier groups, so a scheduler can run
/// groups whose dependencies are done — in parallel across independent groups.
pub fn schedule(inputs: &Inputs) -> Vec<Group> {
    // Index every node.
    let mut nodes: Vec<Node> = Vec::new();
    let mut index: HashMap<Node, usize> = HashMap::new();
    let add = |n: Node, nodes: &mut Vec<Node>, index: &mut HashMap<Node, usize>| {
        *index.entry(n.clone()).or_insert_with(|| {
            nodes.push(n);
            nodes.len() - 1
        })
    };
    for f in &inputs.functions {
        add(Inputs::fn_key(f), &mut nodes, &mut index);
    }
    for f in &inputs.files {
        add(Node::File(f.path.clone()), &mut nodes, &mut index);
    }
    for d in &inputs.folders {
        add(Node::Folder(d.path.clone()), &mut nodes, &mut index);
    }

    // Dependency edges: X → Y means Y is explained before X.
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let edge = |from: &Node, to: &Node, deps: &mut Vec<Vec<usize>>| {
        if let (Some(&a), Some(&b)) = (index.get(from), index.get(to))
            && a != b
        {
            deps[a].push(b);
        }
    };
    for f in &inputs.functions {
        let from = Inputs::fn_key(f);
        for (cf, cn) in &f.callees {
            edge(&from, &Node::Function { file: cf.clone(), name: cn.clone() }, &mut deps);
        }
    }
    for f in &inputs.files {
        let from = Node::File(f.path.clone());
        for (ff, fnn) in &f.functions {
            edge(&from, &Node::Function { file: ff.clone(), name: fnn.clone() }, &mut deps);
        }
    }
    for d in &inputs.folders {
        let from = Node::Folder(d.path.clone());
        for file in &d.files {
            edge(&from, &Node::File(file.clone()), &mut deps);
        }
        for sub in &d.subfolders {
            edge(&from, &Node::Folder(sub.clone()), &mut deps);
        }
    }

    // Tarjan emits SCCs in reverse-topological order of the condensation — i.e.
    // dependencies (sinks) first — which is exactly the order we explain in.
    let sccs = tarjan_scc(&deps);
    let mut group_of = vec![0usize; nodes.len()];
    for (gi, scc) in sccs.iter().enumerate() {
        for &n in scc {
            group_of[n] = gi;
        }
    }
    sccs.iter()
        .enumerate()
        .map(|(gi, scc)| {
            let mut group_deps: Vec<usize> = scc
                .iter()
                .flat_map(|&n| deps[n].iter())
                .map(|&d| group_of[d])
                .filter(|&g| g != gi)
                .collect();
            group_deps.sort_unstable();
            group_deps.dedup();
            Group {
                nodes: scc.iter().map(|&i| nodes[i].clone()).collect(),
                deps: group_deps,
            }
        })
        .collect()
}

/// Group indices bucketed by dependency depth: every group in level `k` depends
/// only on groups in levels `< k`, so all groups in one level can run
/// concurrently (their prompts read only already-finished summaries).
pub fn levels(groups: &[Group]) -> Vec<Vec<usize>> {
    if groups.is_empty() {
        return Vec::new();
    }
    // deps have lower indices (dependencies-first), so this single pass suffices.
    let mut level = vec![0usize; groups.len()];
    for (i, g) in groups.iter().enumerate() {
        level[i] = g.deps.iter().map(|&d| level[d] + 1).max().unwrap_or(0);
    }
    let max = *level.iter().max().unwrap();
    let mut out = vec![Vec::new(); max + 1];
    for (i, &l) in level.iter().enumerate() {
        out[l].push(i);
    }
    out
}

/// The prompt to explain `group`, embedding its dependencies' summaries from
/// `cache`. Public so a parallel scheduler can build prompts the same way.
pub fn prompt_for(group: &Group, inputs: &Inputs, cache: &Cache) -> String {
    let by_fn: HashMap<(&std::path::Path, &str), &FnInput> = inputs
        .functions
        .iter()
        .map(|f| ((f.file.as_path(), f.name.as_str()), f))
        .collect();
    if group.nodes.iter().all(|n| matches!(n, Node::Function { .. })) {
        let fns: Vec<&FnInput> = group
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Function { file, name } => {
                    by_fn.get(&(file.as_path(), name.as_str())).copied()
                }
                _ => None,
            })
            .collect();
        return function_prompt(&fns, cache);
    }
    // File / folder groups are singletons.
    match &group.nodes[0] {
        Node::File(p) => inputs
            .files
            .iter()
            .find(|f| f.path == *p)
            .map(|f| file_prompt(f, cache))
            .unwrap_or_default(),
        Node::Folder(p) => inputs
            .folders
            .iter()
            .find(|d| d.path == *p)
            .map(|d| folder_prompt(d, cache))
            .unwrap_or_default(),
        Node::Function { .. } => String::new(),
    }
}

/// Sequentially run the schedule, reusing `prev`'s summaries where the prompt is
/// unchanged and calling `explain` otherwise. This is the tested reference for
/// the incremental logic; the live pass streams progress and runs levels
/// concurrently (see the explain orchestrator).
#[allow(dead_code)]
pub fn run(
    inputs: &Inputs,
    prev: &Cache,
    mut explain: impl FnMut(&str) -> String,
) -> (Cache, Stats) {
    let groups = schedule(inputs);
    let mut cache: Cache = Cache::new();
    let mut stats = Stats::default();
    for group in &groups {
        let prompt = prompt_for(group, inputs, &cache);
        let hash = content_hash(prompt.as_bytes());
        let summary = if let Some(c) = prev.get(group.key())
            && c.prompt_hash == hash
        {
            stats.reused += 1;
            c.summary.clone()
        } else {
            stats.generated += 1;
            explain(&prompt)
        };
        for n in &group.nodes {
            cache.insert(
                n.clone(),
                Cached { summary: summary.clone(), prompt_hash: hash, detail: None },
            );
        }
    }
    (cache, stats)
}

fn function_prompt(group: &[&FnInput], summaries: &Cache) -> String {
    let mut p = String::new();
    if group.len() > 1 {
        p.push_str("These functions are mutually recursive — explain them together.\n\n");
    }
    for f in group {
        p.push_str(&format!("Function `{}`:\n{}\n{}\n\n", f.name, f.signature, f.body));
    }
    // Summaries of the functions this group calls (outside the group).
    let in_group = |cf: &std::path::Path, cn: &str| {
        group.iter().any(|g| g.file.as_path() == cf && g.name == cn)
    };
    let mut seen: HashSet<(&std::path::Path, &str)> = HashSet::new();
    let mut calls = String::new();
    for f in group {
        for (cf, cn) in &f.callees {
            if in_group(cf, cn) || !seen.insert((cf.as_path(), cn.as_str())) {
                continue;
            }
            if let Some(c) =
                summaries.get(&Node::Function { file: cf.clone(), name: cn.clone() })
            {
                calls.push_str(&format!("- `{cn}` — {}\n", c.summary));
            }
        }
    }
    if !calls.is_empty() {
        p.push_str("It calls these (already summarized):\n");
        p.push_str(&calls);
        p.push('\n');
    }
    p.push_str("Explain concisely what this code does and why.");
    p
}

fn file_prompt(f: &FileInput, summaries: &Cache) -> String {
    let mut p = format!("File `{}`.\n", f.path.display());
    if !f.structure.is_empty() {
        p.push_str(&format!("Structure:\n{}\n", f.structure));
    }
    p.push_str("\nFunctions (summarized):\n");
    for (ff, fnn) in &f.functions {
        if let Some(c) =
            summaries.get(&Node::Function { file: ff.clone(), name: fnn.clone() })
        {
            p.push_str(&format!("- `{fnn}`: {}\n", c.summary));
        }
    }
    p.push_str("\nSummarize this file's architectural role concisely.");
    p
}

fn folder_prompt(d: &FolderInput, summaries: &Cache) -> String {
    let mut p = format!("Folder `{}`.\n\nContains:\n", d.path.display());
    for sub in &d.subfolders {
        if let Some(c) = summaries.get(&Node::Folder(sub.clone())) {
            p.push_str(&format!("- subfolder `{}`: {}\n", sub.display(), c.summary));
        }
    }
    for file in &d.files {
        if let Some(c) = summaries.get(&Node::File(file.clone())) {
            p.push_str(&format!("- file `{}`: {}\n", file.display(), c.summary));
        }
    }
    p.push_str("\nSummarize this folder/subsystem's architecture concisely.");
    p
}

/// Build the on-demand **block-by-block** detail prompt for a single function.
/// Unlike [`function_prompt`] (which asks for a terse whole-function summary),
/// this asks the model to walk the body in execution order and explain each
/// logical block, so it renders as a structured, per-block markdown walkthrough.
/// `callee_summaries` are `(name, summary)` pairs for the functions it calls,
/// included as context (bodies stay out — the same discipline as the summaries).
pub fn detail_prompt(
    name: &str,
    signature: &str,
    body: &str,
    callee_summaries: &[(String, String)],
) -> String {
    let mut p = format!("Function `{name}`:\n{signature}\n{body}\n\n");
    if !callee_summaries.is_empty() {
        p.push_str("It calls these (already summarized, for context):\n");
        for (cn, sum) in callee_summaries {
            p.push_str(&format!("- `{cn}` — {sum}\n"));
        }
        p.push('\n');
    }
    p.push_str(
        "Walk through this function block by block, in execution order. For each \
         logical block (a guard/early return, a loop, a branch, a group of \
         related statements) give a short bold heading naming what it does, then \
         one or two sentences on how and why. Quote the key line(s) with inline \
         code. Cover every block; don't restate trivial lines.",
    );
    p
}

/// Iterative Tarjan SCC over a dependency adjacency list. Returns the components
/// in reverse-topological order of the condensation (sinks — the leaves we
/// explain first — come first). Explicit-stack so a deep graph can't overflow.
fn tarjan_scc(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next = 0usize;
    let mut out: Vec<Vec<usize>> = Vec::new();

    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        // Work item: (node, next-neighbor-cursor).
        let mut call: Vec<(usize, usize)> = vec![(start, 0)];
        index[start] = next;
        low[start] = next;
        next += 1;
        stack.push(start);
        on_stack[start] = true;

        while let Some(&(v, ci)) = call.last() {
            if ci < adj[v].len() {
                let w = adj[v][ci];
                call.last_mut().unwrap().1 += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    call.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    out.push(comp);
                }
                call.pop();
                if let Some(&(p, _)) = call.last() {
                    low[p] = low[p].min(low[v]);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(file: &str, name: &str, callees: &[(&str, &str)]) -> FnInput {
        FnInput {
            file: PathBuf::from(file),
            name: name.into(),
            signature: format!("fn {name}()"),
            body: format!("{{ body of {name} }}"),
            callees: callees.iter().map(|(a, b)| (PathBuf::from(*a), b.to_string())).collect(),
        }
    }
    fn fnode(file: &str, name: &str) -> Node {
        Node::Function { file: PathBuf::from(file), name: name.into() }
    }
    /// A deterministic mock explainer: the summary echoes the prompt hash so a
    /// changed prompt yields a changed summary (as a real LLM would).
    fn echo() -> impl FnMut(&str) -> String {
        |p: &str| format!("sum:{}", content_hash(p.as_bytes()))
    }

    #[test]
    fn explains_callees_before_callers() {
        // main → helper → leaf.
        let inputs = Inputs {
            functions: vec![
                f("/p/a.rs", "leaf", &[]),
                f("/p/a.rs", "helper", &[("/p/a.rs", "leaf")]),
                f("/p/a.rs", "main", &[("/p/a.rs", "helper")]),
            ],
            ..Default::default()
        };
        let groups = schedule(&inputs);
        let pos = |n: &Node| groups.iter().position(|g| g.nodes.contains(n)).unwrap();
        assert!(pos(&fnode("/p/a.rs", "leaf")) < pos(&fnode("/p/a.rs", "helper")));
        assert!(pos(&fnode("/p/a.rs", "helper")) < pos(&fnode("/p/a.rs", "main")));
    }

    #[test]
    fn mutual_recursion_is_one_group() {
        // a ↔ b are mutually recursive.
        let inputs = Inputs {
            functions: vec![
                f("/p/a.rs", "a", &[("/p/a.rs", "b")]),
                f("/p/a.rs", "b", &[("/p/a.rs", "a")]),
            ],
            ..Default::default()
        };
        let groups = schedule(&inputs);
        let group = groups.iter().find(|g| g.nodes.contains(&fnode("/p/a.rs", "a"))).unwrap();
        assert_eq!(group.nodes.len(), 2, "a and b explained together: {groups:?}");
    }

    #[test]
    fn containment_orders_folder_after_file_after_functions() {
        let inputs = Inputs {
            functions: vec![f("/p/src/a.rs", "foo", &[])],
            files: vec![FileInput {
                path: PathBuf::from("/p/src/a.rs"),
                functions: vec![(PathBuf::from("/p/src/a.rs"), "foo".into())],
                structure: "struct S".into(),
            }],
            folders: vec![FolderInput {
                path: PathBuf::from("/p/src"),
                files: vec![PathBuf::from("/p/src/a.rs")],
                subfolders: vec![],
            }],
        };
        let groups = schedule(&inputs);
        let pos = |n: &Node| groups.iter().position(|g| g.nodes.contains(n)).unwrap();
        assert!(pos(&fnode("/p/src/a.rs", "foo")) < pos(&Node::File("/p/src/a.rs".into())));
        assert!(pos(&Node::File("/p/src/a.rs".into())) < pos(&Node::Folder("/p/src".into())));
    }

    #[test]
    fn levels_bucket_by_dependency_depth() {
        // leaf ← helper ← main (a 3-deep chain) → 3 levels, each concurrent-able.
        let inputs = Inputs {
            functions: vec![
                f("/p/a.rs", "leaf", &[]),
                f("/p/a.rs", "helper", &[("/p/a.rs", "leaf")]),
                f("/p/a.rs", "main", &[("/p/a.rs", "helper")]),
                f("/p/a.rs", "sibling", &[("/p/a.rs", "leaf")]), // also depends only on leaf
            ],
            ..Default::default()
        };
        let groups = schedule(&inputs);
        let lv = levels(&groups);
        // leaf at level 0; helper and sibling both at level 1 (parallel); main at 2.
        let name = |gi: usize| match &groups[gi].nodes[0] {
            Node::Function { name, .. } => name.clone(),
            _ => String::new(),
        };
        assert_eq!(lv[0].iter().map(|&g| name(g)).collect::<Vec<_>>(), vec!["leaf"]);
        let l1: HashSet<String> = lv[1].iter().map(|&g| name(g)).collect();
        assert_eq!(l1, HashSet::from(["helper".into(), "sibling".into()]));
        assert_eq!(lv[2].iter().map(|&g| name(g)).collect::<Vec<_>>(), vec!["main"]);
    }

    #[test]
    fn function_prompt_includes_callee_summaries_not_bodies() {
        let inputs = Inputs {
            functions: vec![
                f("/p/a.rs", "leaf", &[]),
                f("/p/a.rs", "caller", &[("/p/a.rs", "leaf")]),
            ],
            ..Default::default()
        };
        let (cache, _) = run(&inputs, &Cache::new(), echo());
        // Rebuild the caller's prompt with the produced summaries to inspect it.
        let leaf_sum = cache.get(&fnode("/p/a.rs", "leaf")).unwrap().summary.clone();
        let caller = f("/p/a.rs", "caller", &[("/p/a.rs", "leaf")]);
        let prompt = function_prompt(&[&caller], &cache);
        assert!(prompt.contains("body of caller"), "own body present");
        assert!(prompt.contains(&leaf_sum), "callee summary present");
        assert!(!prompt.contains("body of leaf"), "callee body NOT inlined");
    }

    #[test]
    fn detail_prompt_includes_body_and_callee_context() {
        let callees = vec![("leaf".to_string(), "does the leaf thing".to_string())];
        let p = detail_prompt("caller", "fn caller()", "{ body of caller }", &callees);
        assert!(p.contains("body of caller"), "own body present");
        assert!(p.contains("does the leaf thing"), "callee summary present");
        assert!(p.contains("block by block"), "asks for a per-block walkthrough");
    }

    #[test]
    fn incremental_reexplains_up_both_axes_and_caches_the_rest() {
        let build = |leaf_body: &str| Inputs {
            functions: vec![
                FnInput {
                    file: "/p/src/a.rs".into(),
                    name: "leaf".into(),
                    signature: "fn leaf()".into(),
                    body: leaf_body.into(),
                    callees: vec![],
                },
                f("/p/src/a.rs", "caller", &[("/p/src/a.rs", "leaf")]),
                f("/p/src/b.rs", "unrelated", &[]),
            ],
            files: vec![
                FileInput {
                    path: "/p/src/a.rs".into(),
                    functions: vec![
                        ("/p/src/a.rs".into(), "leaf".into()),
                        ("/p/src/a.rs".into(), "caller".into()),
                    ],
                    structure: String::new(),
                },
                FileInput {
                    path: "/p/src/b.rs".into(),
                    functions: vec![("/p/src/b.rs".into(), "unrelated".into())],
                    structure: String::new(),
                },
            ],
            folders: vec![FolderInput {
                path: "/p/src".into(),
                files: vec!["/p/src/a.rs".into(), "/p/src/b.rs".into()],
                subfolders: vec![],
            }],
        };

        let (first, s1) = run(&build("{ v1 }"), &Cache::new(), echo());
        assert_eq!(s1.reused, 0, "cold build generates everything");

        // Change only `leaf`'s body → re-explain leaf, its caller, file a.rs, and
        // the src folder; b.rs and `unrelated` stay cached.
        let (second, s2) = run(&build("{ v2 changed }"), &first, echo());
        assert_eq!(s2.generated, 4, "leaf + caller + a.rs + src folder: {s2:?}");
        assert_eq!(s2.reused, 2, "unrelated fn + b.rs file reused: {s2:?}");

        // Same inputs against the fresh cache → everything reused.
        let (_, s3) = run(&build("{ v2 changed }"), &second, echo());
        assert_eq!(s3.generated, 0, "identical inputs are a full cache hit: {s3:?}");
    }
}
