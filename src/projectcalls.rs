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

use tree_sitter::{Node, Parser};

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

impl ProjectCallGraph {
    /// Build the graph from the project's callable definitions and the current
    /// source of every file (so call sites reflect what's on disk right now).
    pub fn build(defs: Vec<Def>, sources: &[(PathBuf, String)]) -> Self {
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

        let mut edges: HashSet<(usize, usize)> = HashSet::new();
        for (file, content) in sources {
            let Some(lang) = crate::highlight::detect(file) else {
                continue;
            };
            for cs in calls_of(content, lang) {
                let Some(caller_name) = cs.caller.as_deref() else {
                    continue; // a top-level call has no caller function node
                };
                let Some(caller) =
                    resolve_caller(&by_file, file, caller_name, cs.line, &nodes)
                else {
                    continue;
                };
                if let Some(callees) = name_to.get(cs.callee.as_str()) {
                    for &callee in callees {
                        // Skip self-edges so a recursive function with no other
                        // callers still reads as "uncalled".
                        if callee != caller {
                            edges.insert((caller, callee));
                        }
                    }
                }
            }
        }

        let mut nodes = nodes;
        for (c, e) in edges {
            nodes[c].callees.push(e);
            nodes[e].callers.push(c);
        }
        for n in &mut nodes {
            n.callers.sort_unstable();
            n.callees.sort_unstable();
        }
        ProjectCallGraph { nodes }
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
            ],
            call_kinds: &["call_expression", "new_expression"],
        },
        "go" => LangSpec {
            fn_kinds: &["function_declaration", "method_declaration"],
            call_kinds: &["call_expression"],
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
    walk(tree.root_node(), source, &spec, None, &mut out);
    out
}

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

/// Depth-first walk carrying the nearest enclosing function name.
fn walk(node: Node, src: &str, spec: &LangSpec, enclosing: Option<&str>, out: &mut Vec<CallSite>) {
    // A named function definition becomes the enclosing scope for its subtree.
    let own_name: Option<String> = if spec.fn_kinds.contains(&node.kind()) {
        node.child_by_field_name("name")
            .map(|n| node_text(n, src).to_string())
    } else {
        None
    };
    let current: Option<&str> = own_name.as_deref().or(enclosing);

    if spec.call_kinds.contains(&node.kind())
        && let Some(callee) = callee_name(node, src)
    {
        out.push(CallSite {
            caller: current.map(str::to_string),
            callee,
            line: node.start_position().row + 1,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, spec, current, out);
    }
}

/// The called name: the trailing identifier of a call's callee expression
/// (`self.foo.bar` → `bar`, `Vec::<u8>::with_capacity` → `with_capacity`).
fn callee_name(call: Node, src: &str) -> Option<String> {
    let target = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("constructor"))
        .or_else(|| call.child(0))?;
    last_identifier(node_text(target, src))
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
        let g = ProjectCallGraph::build(defs, &sources);
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
        let g = ProjectCallGraph::build(defs, &sources);
        // The self-call is dropped, so `fac` still counts as uncalled.
        assert_eq!(g.node(g.uncalled()[0]).name, "fac");
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn last_identifier_handles_paths_and_rejects_numbers() {
        assert_eq!(last_identifier("self.foo.bar").as_deref(), Some("bar"));
        assert_eq!(last_identifier("Vec::<u8>::with_capacity").as_deref(), Some("with_capacity"));
        assert_eq!(last_identifier("42").as_deref(), None);
    }
}
