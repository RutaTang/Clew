//! Project-wide call graph, built offline from tree-sitter call sites resolved
//! by name against the symbol index.
//!
//! The LSP call hierarchy (see [`crate::callgraph`]) is exact but per-symbol and
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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tree_sitter::{Node, Parser};

/// A stable identity for a function across edits: its file and name. (Line is
/// deliberately excluded so an edge survives lines shifting above it.)
pub type SymKey = (PathBuf, String);

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
    /// The enclosing function's name, if the call is inside one.
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

#[derive(Debug, Default, Clone)]
pub struct ProjectCallGraph {
    nodes: Vec<SymNode>,
}

/// Whether a symbol kind is callable (a graph node).
fn is_callable(kind: &str) -> bool {
    matches!(kind, "function" | "method")
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
    ProjectCallGraph { nodes }
}

impl ProjectCallGraph {
    /// Build the graph from the project's callable definitions, the current
    /// source of every file (so call sites reflect what's on disk right now),
    /// and each file's import scope (the internal files it imports — used to
    /// resolve a called name to the definition actually in scope).
    pub fn build(
        defs: Vec<Def>,
        sources: &[(PathBuf, String)],
        scope: &HashMap<PathBuf, HashSet<PathBuf>>,
    ) -> Self {
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
        for (file, content) in sources {
            let Some(lang) = crate::highlight::detect(file) else {
                continue;
            };
            let imported = scope.get(file).unwrap_or(&empty_scope);
            for cs in calls_of(content, lang) {
                // Bare calls to language builtins (`len(x)`, `make(...)`) are not
                // project functions; skip them so they don't resolve to some
                // same-named definition and inflate the graph.
                if is_builtin(lang, &cs.callee) {
                    continue;
                }
                let Some(caller_name) = cs.caller.as_deref() else {
                    continue; // a top-level call has no caller function node
                };
                let Some(caller) =
                    resolve_caller(&by_file, file, caller_name, cs.line, &nodes)
                else {
                    continue;
                };
                for callee in resolve_callees(&name_to, &nodes, &cs, file, imported, lang) {
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

    /// The project's callable definitions (functions/methods), in a stable order,
    /// with a `(file, name) → node index` lookup — the node set the LSP-precise
    /// pass maps call-hierarchy results back onto.
    pub fn callable(defs: &[Def]) -> Vec<Def> {
        defs.iter().filter(|d| is_callable(&d.kind)).cloned().collect()
    }

    /// Build the display graph from the full callable node set and symbol-keyed
    /// edges (the LSP-precise edge set, kept stable across edits by keying on
    /// `(file, name)` rather than node index). Edges whose endpoints aren't in
    /// `defs` are dropped. `defs` must be the callable definitions.
    pub fn graph_from_sym_edges(defs: Vec<Def>, edges: &SymEdges) -> Self {
        let idx_edges: HashSet<(usize, usize)> = {
            let mut lookup: HashMap<(&Path, &str), usize> = HashMap::new();
            for (i, d) in defs.iter().enumerate() {
                lookup.entry((d.file.as_path(), d.name.as_str())).or_insert(i);
            }
            edges
                .iter()
                .filter_map(|((cf, cn), (ef, en))| {
                    let c = *lookup.get(&(cf.as_path(), cn.as_str()))?;
                    let e = *lookup.get(&(ef.as_path(), en.as_str()))?;
                    Some((c, e))
                })
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

    pub fn node(&self, id: usize) -> &SymNode {
        &self.nodes[id]
    }

    /// Total number of distinct caller→callee edges.
    pub fn edge_count(&self) -> usize {
        self.nodes.iter().map(|n| n.callees.len()).sum()
    }

    /// Each function's callees as `(file, name)` keys — the call-graph
    /// dependency edges the explain engine orders by.
    pub fn callee_keys(&self) -> HashMap<(PathBuf, String), Vec<(PathBuf, String)>> {
        self.nodes
            .iter()
            .map(|n| {
                let callees = n
                    .callees
                    .iter()
                    .map(|&c| (self.nodes[c].file.clone(), self.nodes[c].name.clone()))
                    .collect();
                ((n.file.clone(), n.name.clone()), callees)
            })
            .collect()
    }

    /// Aggregate the symbol-level graph to file level: the files that hold any
    /// function, and an edge A→B when a function in A calls one in B. This is the
    /// readable "module call-flow" view (601 functions collapse to ~30 files).
    pub fn file_graph(&self) -> (Vec<PathBuf>, Vec<(usize, usize)>) {
        let mut files: Vec<PathBuf> = self.nodes.iter().map(|n| n.file.clone()).collect();
        files.sort();
        files.dedup();
        let idx: HashMap<&Path, usize> =
            files.iter().enumerate().map(|(i, f)| (f.as_path(), i)).collect();
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

    /// The node id for a callable definition at `(file, name)`, if present — the
    /// entry point for looking up a function's callers/callees.
    pub fn id_of(&self, file: &Path, name: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.name == name && n.file == file)
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
        v.sort_by(|&a, &b| self.sort_key(a).cmp(&self.sort_key(b)));
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
        v.sort_by(|&a, &b| {
            self.nodes[b]
                .callers
                .len()
                .cmp(&self.nodes[a].callers.len())
                .then_with(|| self.sort_key(a).cmp(&self.sort_key(b)))
        });
        v.truncate(limit);
        v
    }

    fn sort_key(&self, id: usize) -> (String, usize) {
        (self.nodes[id].name.clone(), self.nodes[id].line)
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

/// Per-language node kinds for the call-site walk.
struct LangSpec {
    fn_kinds: &'static [&'static str],
    call_kinds: &'static [&'static str],
}

fn lang_spec(lang: &str) -> Option<LangSpec> {
    Some(match lang {
        "rust" => LangSpec {
            fn_kinds: &["function_item"],
            call_kinds: &["call_expression"],
        },
        "python" => LangSpec {
            fn_kinds: &["function_definition"],
            call_kinds: &["call"],
        },
        "javascript" | "typescript" | "tsx" => LangSpec {
            fn_kinds: &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "function_expression",
                // Arrow functions (const x = () => …, class fields, callbacks) are
                // ubiquitous in modern JS/TS; without them calls in their bodies
                // are dropped or misattributed to an outer function.
                "arrow_function",
            ],
            call_kinds: &["call_expression", "new_expression"],
        },
        "go" => LangSpec {
            fn_kinds: &["function_declaration", "method_declaration"],
            call_kinds: &["call_expression"],
        },
        "dart" => LangSpec {
            // Dart has no node wrapping a signature and its body — they are flat
            // siblings (`method_signature`/`function_signature` then `function_body`).
            // The body holds the calls, so it is the enclosing scope; its name is
            // recovered from the preceding signature (see `fn_name`).
            fn_kinds: &["function_body"],
            call_kinds: &["method_invocation", "constructor_invocation"],
        },
        _ => return None,
    })
}

/// Extract every call site from one file's `source`.
pub fn calls_of(source: &str, lang: &str) -> Vec<CallSite> {
    let Some(spec) = lang_spec(lang) else {
        return Vec::new();
    };
    let Some(language) = crate::highlight::language_for(lang) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(tree.root_node(), source, &spec, &mut out);
    out
}

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

/// Iterative depth-first walk carrying the nearest enclosing function name. It is
/// explicit-stack (not recursive) so a pathologically deep tree — e.g. a checked-
/// in minified bundle with 100k-deep nested expressions — can't overflow the
/// stack and abort the process.
fn walk(root: Node, src: &str, spec: &LangSpec, out: &mut Vec<CallSite>) {
    // Each item is a node plus the enclosing function name in scope for it.
    let mut stack: Vec<(Node, Option<Rc<str>>)> = vec![(root, None)];
    while let Some((node, enclosing)) = stack.pop() {
        // A function definition becomes the enclosing scope for its subtree.
        let own: Option<Rc<str>> = if spec.fn_kinds.contains(&node.kind()) {
            fn_name(node, src).map(|s| Rc::from(s.as_str()))
        } else {
            None
        };
        let current = own.or(enclosing);

        if spec.call_kinds.contains(&node.kind())
            && let Some((callee, method)) = callee_name(node, src)
        {
            out.push(CallSite {
                caller: current.as_deref().map(str::to_string),
                callee,
                method,
                line: node.start_position().row + 1,
            });
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push((child, current.clone()));
        }
    }
}

/// The name of a function definition. A named function uses its `name` field; an
/// anonymous one (arrow / function expression) is named after the binding it is
/// assigned to (`const handler = () => …` → `handler`).
fn fn_name(node: Node, src: &str) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        return Some(node_text(n, src).to_string());
    }
    // Dart: a `function_body` is a bare sibling after its signature, so its name
    // comes from the preceding signature (a lambda/arrow body's signature has no
    // name, so those calls fall through to the enclosing named function).
    if node.kind() == "function_body" {
        return node.prev_sibling().and_then(|sig| dart_sig_name(sig, src));
    }
    let parent = node.parent()?;
    let named = match parent.kind() {
        "variable_declarator" | "field_definition" | "public_field_definition" => {
            parent.child_by_field_name("name")
        }
        "assignment_expression" => parent.child_by_field_name("left"),
        "pair" => parent.child_by_field_name("key"),
        _ => None,
    }?;
    Some(node_text(named, src).to_string())
}

/// The called name (trailing identifier of the callee expression —
/// `self.foo.bar` → `bar`, `Vec::<u8>::with_capacity` → `with_capacity`) plus
/// whether the call is a `receiver.name(…)` method access.
/// The name from a Dart signature preceding a `function_body`. Direct signatures
/// (`function_`/`getter_`/`setter_signature`) carry a `name` field; a
/// `method_signature` wraps one of those, so look one level in.
fn dart_sig_name(sig: Node, src: &str) -> Option<String> {
    if let Some(n) = sig.child_by_field_name("name") {
        return Some(node_text(n, src).to_string());
    }
    let mut cursor = sig.walk();
    sig.children(&mut cursor)
        .find_map(|c| c.child_by_field_name("name").map(|n| node_text(n, src).to_string()))
}

fn callee_name(call: Node, src: &str) -> Option<(String, bool)> {
    // Dart constructor calls name a type via a child, not a `function` field.
    if call.kind() == "constructor_invocation" {
        let mut cursor = call.walk();
        let ty = call
            .children(&mut cursor)
            .find(|n| matches!(n.kind(), "type_identifier" | "identifier"))?;
        return Some((last_identifier(node_text(ty, src))?, false));
    }
    let mut target = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("constructor"))
        .or_else(|| call.child(0))?;
    // A trailing turbofish wraps the callee in a `generic_function` whose text
    // ends in the type argument (`parse::<i32>`); unwrap to the real function so
    // the name and method-flag come from `parse`, not `i32`.
    if target.kind() == "generic_function" {
        target = target.child_by_field_name("function").unwrap_or(target);
    }
    // A dotted access (`.`) is a method call; a `::` path is not. In Python/JS/Go
    // module access also uses `.`, so those count as "method-like" here — a safe
    // over-approximation (they still resolve via same-file / import scope).
    let method = matches!(
        target.kind(),
        "field_expression" | "attribute" | "member_expression" | "selector_expression"
    );
    let name = last_identifier(node_text(target, src))?;
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
///   3. for a *free* call only, a **globally unique** definition of that name.
///
/// A method call that matches none of 1–2 resolves to nothing rather than
/// guessing (its name belongs to a receiver type we can't see).
fn resolve_callees(
    name_to: &HashMap<&str, Vec<usize>>,
    nodes: &[SymNode],
    call: &CallSite,
    file: &Path,
    imported: &HashSet<PathBuf>,
    lang: &str,
) -> Vec<usize> {
    let Some(cands) = name_to.get(call.callee.as_str()) else {
        return Vec::new();
    };
    let local: Vec<usize> = cands.iter().copied().filter(|&i| nodes[i].file == file).collect();
    if !local.is_empty() {
        return local;
    }
    let scoped: Vec<usize> = cands
        .iter()
        .copied()
        .filter(|&i| imported.contains(&nodes[i].file))
        .collect();
    if !scoped.is_empty() {
        return scoped;
    }
    // A lone global definition of this name — accept it only if it's the same
    // language as the call site. Otherwise a Go `Foo()` would resolve to a JS
    // function named `Foo`, a spurious cross-language edge.
    if !call.method
        && cands.len() == 1
        && crate::highlight::detect(&nodes[cands[0]].file).is_some_and(|l| l == lang)
    {
        return cands.clone();
    }
    Vec::new()
}

/// Bare-name calls to language builtins are not project functions. Without this
/// they resolve to any same-named definition — even one in another language
/// (a JS `make` in a vendored bundle) — and dominate the "most called" ranking.
fn is_builtin(lang: &str, name: &str) -> bool {
    match lang {
        "go" => matches!(
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
        "python" => matches!(
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
        _ => false,
    }
}

/// Resolve a caller name within a file to the nearest preceding same-named
/// definition (falling back to the first if none precedes the call).
fn resolve_caller(
    by_file: &HashMap<&Path, Vec<usize>>,
    file: &Path,
    name: &str,
    call_line: usize,
    nodes: &[SymNode],
) -> Option<usize> {
    let ids = by_file.get(file)?;
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
    }
    best.or(first)
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
        assert!(names.contains(&"collect"), "turbofish method call: {names:?}");
        assert!(!names.contains(&"i32"), "type arg must not be the callee: {names:?}");
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
            calls.iter().any(|c| c.caller.as_deref() == Some("hello") && c.callee == "_build"),
            "hello -> _build: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.caller.as_deref() == Some("hello") && c.callee == "print"),
            "hello -> print: {calls:?}"
        );
        // The top-level function's method call is attributed to `main`.
        assert!(
            calls.iter().any(|c| c.caller.as_deref() == Some("main") && c.callee == "hello"),
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
        assert_eq!(calls.iter().find(|c| c.callee == "other").unwrap().caller.as_deref(), Some("main"));
    }

    #[test]
    fn python_and_js_call_sites() {
        let py = "def a():\n    b()\n    obj.c()\n";
        let calls = calls_of(py, "python");
        assert_eq!(calls.iter().find(|c| c.callee == "b").unwrap().caller.as_deref(), Some("a"));
        assert!(calls.iter().any(|c| c.callee == "c"));

        let js = "function f() { g(); this.h(); new Widget(); }";
        let calls = calls_of(js, "typescript");
        assert_eq!(calls.iter().find(|c| c.callee == "g").unwrap().caller.as_deref(), Some("f"));
        assert!(calls.iter().any(|c| c.callee == "Widget"));
    }

    fn def(name: &str, file: &str, line: usize) -> Def {
        Def { name: name.into(), kind: "function".into(), file: PathBuf::from(file), line }
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
        let uncalled: Vec<&str> = g.uncalled().iter().map(|&i| g.node(i).name.as_str()).collect();
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
        let defs = vec![def("get", "/p/store.rs", 1), def("caller", "/p/other.rs", 1)];
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
            (PathBuf::from("/p/a.rs"), "fn caller() {\n    helper();\n}\n".to_string()),
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
        (0..g.node_count()).find(|&i| g.node(i).name == name).unwrap()
    }

    #[test]
    fn builtin_calls_do_not_link_to_same_named_definitions() {
        // A Go `make(...)` builtin call must not resolve to a project function
        // named `make` (here in a vendored JS bundle) — that inflated the graph.
        let defs = vec![def("run", "/p/a.go", 1), def("make", "/p/vendor.js", 1)];
        let sources = vec![
            (PathBuf::from("/p/a.go"), "func run() {\n\tmake([]int, 0)\n}\n".to_string()),
            (PathBuf::from("/p/vendor.js"), "function make() {}\n".to_string()),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node(id_named(&g, "make")).caller_count(), 0);
    }

    #[test]
    fn lone_global_fallback_is_restricted_to_the_same_language() {
        // A Go `Widget()` call has one globally-unique definition — but it's in a
        // JS file. A Go call can't invoke a JS function by bare name, so no edge.
        let defs = vec![def("run", "/p/a.go", 1), def("Widget", "/p/vendor.js", 1)];
        let sources = vec![
            (PathBuf::from("/p/a.go"), "func run() {\n\tWidget()\n}\n".to_string()),
            (PathBuf::from("/p/vendor.js"), "function Widget() {}\n".to_string()),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node(id_named(&g, "Widget")).caller_count(), 0);
    }

    #[test]
    fn graph_from_sym_edges_maps_and_survives_line_shifts() {
        let defs = vec![def("a", "/p/x.rs", 1), def("b", "/p/x.rs", 5), def("c", "/p/y.rs", 1)];
        let key = |file: &str, name: &str| (PathBuf::from(file), name.to_string());
        let edges: SymEdges = HashSet::from([
            (key("/p/x.rs", "a"), key("/p/x.rs", "b")), // a → b
            (key("/p/y.rs", "c"), key("/p/x.rs", "a")), // c → a
            (key("/p/x.rs", "a"), key("/p/gone.rs", "z")), // dangling → dropped
        ]);
        let g = ProjectCallGraph::graph_from_sym_edges(defs, &edges);
        assert_eq!(g.node(id_named(&g, "b")).caller_count(), 1);
        assert_eq!(g.node(id_named(&g, "a")).caller_count(), 1);
        assert_eq!(g.edge_count(), 2, "dangling edge dropped");

        // The same edges resolve even after `a`/`b` move to different lines —
        // the key is (file, name), not line.
        let shifted = vec![def("a", "/p/x.rs", 40), def("b", "/p/x.rs", 88), def("c", "/p/y.rs", 3)];
        let g2 = ProjectCallGraph::graph_from_sym_edges(shifted, &edges);
        assert_eq!(g2.edge_count(), 2);
        assert_eq!(g2.node(id_named(&g2, "b")).caller_count(), 1);
    }

    #[test]
    fn from_callable_defs_builds_from_explicit_edges() {
        let defs = vec![def("a", "/p/x.rs", 1), def("b", "/p/x.rs", 5), def("c", "/p/y.rs", 1)];
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
    fn last_identifier_handles_paths_and_rejects_numbers() {
        assert_eq!(last_identifier("self.foo.bar").as_deref(), Some("bar"));
        assert_eq!(last_identifier("Vec::<u8>::with_capacity").as_deref(), Some("with_capacity"));
        assert_eq!(last_identifier("42").as_deref(), None);
    }
}
