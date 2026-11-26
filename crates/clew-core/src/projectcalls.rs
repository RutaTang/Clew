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

/// Whether a definition of `kind` is a node of the graph: a callable, or a
/// test or group of tests a runner's call opens (`it "adds" do`, `test
/// "adds" {`, `TEST(Cart, Adds)`, `describe`) — which no call reaches, but
/// whose calls are made from it. Without them a call in a test block had
/// no caller, and a function only tests call read "No callers".
pub fn is_node_kind(kind: &str) -> bool {
    is_callable(kind) || calls_only(kind)
}

/// A test or a group of tests: a node that calls, never called — no call
/// of a name lands on one, as a test's title is no function's name.
pub fn calls_only(kind: &str) -> bool {
    matches!(kind, "test" | "suite")
}

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
    /// What a method call's receiver is known to be, where the code says:
    /// the caller's own class (`self.f()`, `this.f()`), or a type — a
    /// variable built by its constructor in the caller (`cart = Cart()`,
    /// `let c = Cart::new()`, `c := &Cart{}`), declared or annotated with it
    /// (`Cart c`, `c: Cart`), or the type itself (`Cart.create()`). `None`
    /// when unknown: the call then links to every method of the name.
    pub receiver: Option<Receiver>,
    /// The name a Scala method call is made on where its receiver is
    /// unknown: `os` in `os.read(p)` — a package, whose object `read` the
    /// call applies, or a value of no known type.
    pub qualifier: Option<String>,
    /// How many arguments an Erlang or Elixir call passes (`f(a, b)` two;
    /// `x |> f(a)` two; `&f/2`, `fun f/2` two): which of the name's
    /// functions it is. `None` elsewhere.
    pub arity: Option<u8>,
    /// 1-based line of the call.
    pub line: usize,
}

/// The type a method call's receiver is known to be (see
/// [`CallSite::receiver`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Receiver {
    /// The class the calling method belongs to: `self`, `this`, `$this`.
    Own,
    /// A type, by its name as its definition gives it.
    Type(String),
    /// A type the receiver is declared as — a parameter's, a field's, a
    /// local's (`Owner owner`, `owners: OwnerRepository`) — in a language
    /// with inheritance, where it may hold a subtype: the call is the
    /// type's method, its own or the one it inherits, or a subtype's that
    /// overrides it.
    Declared(String),
}

/// What a file says of where its code lives and of the types it declares:
/// its package (Go's, Java's, Kotlin's, Scala's) or namespace (C#'s), whose
/// other files' names it reaches with no import; and each type it declares
/// with the names its declaration says it extends, implements or embeds
/// ([`crate::heritage::inherited_names`]) — the methods a value of the type
/// has from its bases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileContext {
    pub package: Option<String>,
    pub types: Vec<(String, Vec<String>)>,
    /// Definition lines of extension methods — C#'s `this` parameter's,
    /// Kotlin's `fun Type.name`, Dart's `extension … on`, Scala's
    /// `extension (x: T)` and implicit class's: a method a value of
    /// another type (a library's too) is called with.
    pub extensions: Vec<usize>,
    /// The modules an Elixir file `use`s (`use Hexpm.Context`, `use
    /// HexpmWeb, :controller`): what their `__using__` imports and aliases
    /// the file has too.
    pub uses: Vec<String>,
    /// The modules an Elixir file `import`s (in a `quote` too, as a
    /// `__using__` does): the only ones whose functions it calls bare — an
    /// `alias` names a module, and brings none of its functions.
    pub imports: Vec<String>,
    /// Each Erlang or Elixir function's arities, by its definition line:
    /// fewest and most arguments it takes (an Elixir default lets a call
    /// leave one out). A BEAM function is its name and its arity: `f/1` is
    /// no call of `f/2`.
    pub arities: Vec<(usize, u8, u8)>,
}

/// The kinds of definition that are types another can extend or a value be
/// declared as.
fn is_type_kind(kind: &str, lang: Lang) -> bool {
    matches!(
        kind,
        "class" | "interface" | "struct" | "trait" | "type" | "enum" | "object" | "record"
    ) || (kind == "module" && matches!(lang, Lang::Ruby | Lang::Elixir))
}

/// The modules an Elixir file names after `keyword` (`use `, `import `) at
/// the start of a line: `import Shop.Cart` and the multi-alias form
/// `import Shop.{Cart, Order}` (over several lines too). A module computed
/// at compile time (`import unquote(__MODULE__)`) names none.
fn elixir_modules_after(src: &str, keyword: &str) -> Vec<String> {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let is_name = |c: char| c.is_alphanumeric() || matches!(c, '_' | '.');
    let mut out = Vec::new();
    for (at, line) in src.split('\n').scan(0usize, |offset, line| {
        let at = *offset;
        *offset += line.len() + 1;
        Some((at, line))
    }) {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix(keyword) else {
            continue;
        };
        let rest = rest.trim_start();
        let head: String = rest.chars().take_while(|&c| is_name(c)).collect();
        if !head.starts_with(|c: char| c.is_uppercase()) {
            continue;
        }
        let after = &rest[head.len()..];
        if let Some(prefix) = head.strip_suffix('.')
            && after.starts_with('{')
        {
            // `Shop.{Cart, Order}`: up to the closing brace, which may be
            // lines below.
            let start = at + (line.len() - trimmed.len()) + keyword.len();
            let tail = &src[start..];
            let Some(open) = tail.find('{') else { continue };
            let Some(close) = tail[open..].find('}') else {
                continue;
            };
            for part in tail[open + 1..open + close].split(',') {
                let part = part.trim();
                if part.starts_with(|c: char| c.is_uppercase()) && part.chars().all(is_name) {
                    out.push(format!("{prefix}.{part}"));
                }
            }
        } else if !head.ends_with('.') {
            out.push(head);
        }
    }
    out
}

/// Whether a class is named as a test class is: `CartTest`, `CartTests`,
/// `CartSpec`, `CartSuite`, `CartIT`, `TestCart` (`Shop.CartTest` too).
fn is_test_class_name(owner: &str) -> bool {
    let name = owner.rsplit(['.', ':']).next().unwrap_or(owner);
    let after_test = name.strip_prefix("Test").map(|rest| rest.chars().next());
    matches!(after_test, Some(None)) // `Test` itself
        || matches!(after_test, Some(Some(c)) if c.is_uppercase() || c == '_')
        || ["Test", "Tests", "Spec", "Specs", "Suite"]
            .iter()
            .any(|suffix| name.len() > suffix.len() && name.ends_with(suffix))
        || name
            .strip_suffix("IT")
            .is_some_and(|rest| rest.ends_with(|c: char| c.is_lowercase()))
}

/// The modules a Ruby class or module mixes in: its body's `include`,
/// `extend` and `prepend` (`include Comparable, Helpers`) — whose methods
/// its own call with no receiver.
fn ruby_mixins(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        let Some(rest) = ["include ", "extend ", "prepend "]
            .iter()
            .find_map(|k| line.strip_prefix(k))
        else {
            continue;
        };
        for part in rest.split(',') {
            let name = part.trim().trim_end_matches(';');
            if !name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_uppercase())
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
            {
                out.push(name.rsplit("::").next().unwrap_or(name).to_string());
            }
        }
    }
    out
}

/// A file's [`FileContext`], read off its source and outline.
pub fn context_in(src: &str, lang: Lang, symbols: &[Located]) -> FileContext {
    let types = symbols
        .iter()
        .filter(|l| is_type_kind(&l.symbol.kind, lang))
        .filter_map(|l| {
            // Go's embedded types are its body's members; elsewhere the
            // bases are in the header.
            let end = match lang {
                Lang::Go => l.declaration.1,
                _ => l.signature_end.max(l.declaration.0),
            };
            let text = src.get(l.declaration.0..end)?;
            let text = match lang {
                Lang::Go => text.replace('\n', ";"),
                _ => text.to_string(),
            };
            // An Elixir module inherits nothing: it is kept for its name.
            let mut bases: Vec<String> = match lang {
                Lang::Elixir => Vec::new(),
                _ => crate::heritage::inherited_names(&text, Some(lang.key())),
            };
            if lang == Lang::Ruby
                && let Some(body) = src.get(l.declaration.0..l.declaration.1)
            {
                bases.extend(ruby_mixins(body));
            }
            bases.retain(|b| *b != l.symbol.name);
            bases.dedup();
            Some((l.symbol.name.clone(), bases))
        })
        .collect();
    // The types whose methods extend another: Dart's `extension … on`,
    // Scala's `implicit class`.
    let extension_spans: Vec<(usize, usize)> = symbols
        .iter()
        .filter(|l| is_type_kind(&l.symbol.kind, lang) || l.symbol.kind == "extension")
        .filter(|l| {
            let head = src
                .get(l.declaration.0..l.signature_end.max(l.declaration.0))
                .unwrap_or("");
            match lang {
                Lang::Dart => head.trim_start().starts_with("extension"),
                Lang::Scala => head.contains("implicit class"),
                _ => false,
            }
        })
        .map(|l| l.declaration)
        .collect();
    let extensions = symbols
        .iter()
        .filter(|l| is_callable(&l.symbol.kind))
        .filter(|l| {
            let head = src
                .get(l.declaration.0..l.signature_end.max(l.declaration.0))
                .unwrap_or("");
            let within = |(a, b): &(usize, usize)| *a <= l.declaration.0 && l.declaration.1 <= *b;
            extension_spans.iter().any(within)
                || match lang {
                    Lang::CSharp => head.contains("(this ") || head.contains("( this "),
                    // `fun Type.name(`, `fun <T> List<T>.name(`.
                    Lang::Kotlin => head
                        .find(&format!(".{}", l.symbol.name))
                        .is_some_and(|at| head[..at].contains("fun ")),
                    // Scala 3's `extension (c: Circle)` above the `def`,
                    // with no top-level definition between them.
                    Lang::Scala => {
                        let from = src.floor_char_boundary(l.declaration.0.saturating_sub(400));
                        src.get(from..l.declaration.0)
                            .and_then(|before| before.rfind("extension").map(|at| &before[at..]))
                            .is_some_and(|since| {
                                !since.contains("\ndef ") && !since.contains("\nclass ")
                            })
                    }
                    _ => false,
                }
        })
        .map(|l| l.symbol.line)
        .collect();
    let modules_after = |keyword: &str| -> Vec<String> {
        match lang {
            Lang::Elixir => elixir_modules_after(src, keyword),
            _ => Vec::new(),
        }
    };
    let arities = match lang {
        Lang::Erlang | Lang::Elixir => symbols
            .iter()
            .filter(|l| is_callable(&l.symbol.kind))
            .filter_map(|l| {
                let most = match lang {
                    Lang::Erlang => crate::outline::clause_arity(src, l)?,
                    _ => crate::outline::elixir_arity(src, l)?,
                };
                let defaults = match lang {
                    Lang::Elixir => crate::outline::elixir_defaults(src, l),
                    _ => 0,
                };
                let most = u8::try_from(most).ok()?;
                let fewest = most.saturating_sub(u8::try_from(defaults).unwrap_or(0));
                Some((l.symbol.line, fewest, most))
            })
            .collect(),
        _ => Vec::new(),
    };
    FileContext {
        package: package_of(src, lang),
        types,
        extensions,
        uses: modules_after("use "),
        imports: modules_after("import "),
        arities,
    }
}

/// The package (namespace) a file declares: Go's `package cart`, Java's,
/// Kotlin's and Scala's `package shop.cart` (Scala's chained clauses
/// joined), C#'s `namespace Shop.Cart` (block or file-scoped).
fn package_of(src: &str, lang: Lang) -> Option<String> {
    let keyword = match lang {
        Lang::Go | Lang::Java | Lang::Kotlin | Lang::Scala => "package",
        Lang::CSharp => "namespace",
        _ => return None,
    };
    // A byte-order mark before the first line (Visual Studio writes one)
    // is no part of its text: `\u{feff}using System;` is a `using`.
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let mut parts: Vec<String> = Vec::new();
    let mut in_block_comment = false;
    for line in src.lines().take(2000) {
        let t = line.trim();
        if in_block_comment {
            in_block_comment = !t.contains("*/");
            continue;
        }
        if t.starts_with("/*") {
            in_block_comment = !t.contains("*/");
            continue;
        }
        if t.is_empty() || t.starts_with("//") || t.starts_with('@') || t.starts_with('#') {
            continue;
        }
        let Some(rest) = t
            .strip_prefix(keyword)
            .filter(|r| r.starts_with(char::is_whitespace))
        else {
            // Past the header: C#'s namespace may follow its usings.
            if lang == Lang::CSharp && (t.starts_with("using ") || t.starts_with("global using ")) {
                continue;
            }
            if lang == Lang::Scala && t.starts_with("import ") && parts.is_empty() {
                continue;
            }
            break;
        };
        let rest = rest.trim_start();
        // Scala's `package object cart` is the package's own object.
        let rest = rest.strip_prefix("object ").unwrap_or(rest);
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '`'))
            .filter(|c| *c != '`')
            .collect();
        if name.is_empty() {
            break;
        }
        parts.push(name);
        if lang != Lang::Scala {
            break;
        }
    }
    (!parts.is_empty()).then(|| parts.join("."))
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
#[derive(Default, Clone)]
pub struct ProjectCallGraph {
    nodes: Vec<SymNode>,
    /// Each node's ordinal among its file's same-name callables — the third
    /// part of its [`SymKey`] — computed once, when the node set is fixed.
    ordinals: Vec<u32>,
    /// The nodes by their key's hash ([`key_hash`]), in node order: a lookup
    /// by key ([`ProjectCallGraph::id_of_key`]) read every node, and a view
    /// makes one per stack frame on every draw.
    by_key: HashMap<u64, Vec<usize>>,
}

/// The graph as its nodes say it: the index is theirs, and in no order a
/// graph built again would repeat.
impl std::fmt::Debug for ProjectCallGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectCallGraph")
            .field("nodes", &self.nodes)
            .field("ordinals", &self.ordinals)
            .finish()
    }
}

/// The hash a node's `(file, name, ordinal)` key is indexed under.
fn key_hash(file: &Path, name: &str, ordinal: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (file, name, ordinal).hash(&mut h);
    h.finish()
}

impl ProjectCallGraph {
    /// The graph over `nodes`, which the constructors below have finished.
    fn with_nodes(nodes: Vec<SymNode>) -> Self {
        let ordinals = ordinals_of(
            nodes
                .iter()
                .map(|n| (n.file.as_path(), n.name.as_str(), n.kind.as_str(), n.line)),
        );
        let mut by_key: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            by_key
                .entry(key_hash(&n.file, &n.name, ordinals[i]))
                .or_default()
                .push(i);
        }
        ProjectCallGraph {
            nodes,
            ordinals,
            by_key,
        }
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
        if c < nodes.len() && e < nodes.len() {
            nodes[c].callees.push(e);
            // A function calling itself is no caller of its own: a
            // recursive function nothing else calls is still uncalled. It
            // lists itself among what it calls.
            if c != e {
                nodes[e].callers.push(c);
            }
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
    // Tests and groups are numbered among their own kind: a test titled
    // as a function is named shifts no function's key.
    let ordinals = crate::outline::Ordinals::new(items.iter().map(|&(file, name, kind, line)| {
        ((file, name, calls_only(kind)), is_node_kind(kind), line)
    }));
    items
        .iter()
        .map(|&(file, name, kind, line)| ordinals.of(&(file, name, calls_only(kind)), line))
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
    /// The class (or type, impl, trait) each method belongs to: definition
    /// line → its name. What narrows a call whose receiver's type is known
    /// to that type's method of the name ([`CallSite::receiver`]).
    pub owners: HashMap<usize, String>,
    /// Definition lines of the methods read as attributes, never called (a
    /// Python `@property`; see [`crate::outline::Located::accessed`]): no
    /// call reaches them, and none needs to for them to be used.
    pub accessed: HashSet<usize>,
    /// Definition lines of C and C++ functions of the file alone (`static`;
    /// see [`crate::outline::Located::internal`]): no other file's call
    /// reaches them.
    pub internal: HashSet<usize>,
    /// The file's package and the types it declares with their bases
    /// ([`FileContext`]).
    pub context: FileContext,
}

impl FileCalls {
    /// Parse `source` once and read off its call sites. `None` for a file in
    /// a language without a call model.
    pub fn read(file: &Path, source: &str) -> Option<FileCalls> {
        let (lang, source) = crate::highlight::read_as(crate::highlight::detect(file)?, source)?;
        let source = &*source;
        lang_spec(lang)?;
        let tree = crate::highlight::parse(source, lang)?;
        let symbols = crate::outline::located_in(&tree, source, lang);
        let calls = calls_in(&tree, source, lang, &symbols);
        // A Lean declaration's is its namespace, read as `analyze` reads it.
        let owners = match lang {
            Lang::Lean => crate::outline::lean_owners(source, &symbols),
            _ => owners_in(&tree, source, lang, &symbols),
        };
        let context = context_in(source, lang, &symbols);
        FileCalls::of(file.to_path_buf(), lang, calls, &symbols, owners)
            .map(|calls| calls.with_context(context))
    }

    /// These facts with the file's [`FileContext`].
    pub fn with_context(mut self, context: FileContext) -> Self {
        self.context = context;
        self
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
        owners: HashMap<usize, String>,
    ) -> Option<FileCalls> {
        lang_spec(lang)?;
        Some(FileCalls::new(file, lang, calls, symbols, owners))
    }

    /// Whether this file contributes nothing to a graph: no call site, no
    /// bodyless declaration to tell a definition from, and no method whose
    /// class a call elsewhere may name.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
            && self.declarations.is_empty()
            && self.owners.is_empty()
            && self.accessed.is_empty()
            && self.internal.is_empty()
            && self.context.types.iter().all(|(_, bases)| bases.is_empty())
    }

    /// Assemble from facts already read off the file's tree (see
    /// `outline::analyze`).
    pub fn new(
        file: PathBuf,
        lang: Lang,
        calls: Vec<CallSite>,
        symbols: &[Located],
        owners: HashMap<usize, String>,
    ) -> FileCalls {
        let defined = || {
            symbols
                .iter()
                .filter(|l| is_node_kind(&l.symbol.kind))
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
            owners,
            accessed: symbols
                .iter()
                .filter(|l| l.accessed && is_callable(&l.symbol.kind))
                .map(|l| l.symbol.line)
                .collect(),
            internal: symbols
                .iter()
                .filter(|l| l.internal && is_callable(&l.symbol.kind))
                .map(|l| l.symbol.line)
                .collect(),
            context: FileContext::default(),
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
            .filter(|d| is_node_kind(&d.kind))
            .map(|d| SymNode {
                name: d.name,
                kind: d.kind,
                file: d.file,
                line: d.line,
                callers: Vec::new(),
                callees: Vec::new(),
            })
            .collect();

        let declarations: HashMap<&Path, (&HashSet<usize>, &HashSet<usize>)> = files
            .iter()
            .map(|f| (f.file.as_path(), (&f.declarations, &f.internal)))
            .collect();
        let arities: HashMap<(&Path, usize), (u8, u8)> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .arities
                    .iter()
                    .map(|&(line, fewest, most)| ((f.file.as_path(), line), (fewest, most)))
            })
            .collect();
        let facts: Vec<NodeFacts> = nodes
            .iter()
            .map(|n| NodeFacts {
                arity: arities.get(&(n.file.as_path(), n.line)).copied(),
                bodyless: declarations
                    .get(n.file.as_path())
                    .is_some_and(|(d, _)| d.contains(&n.line)),
                internal: declarations
                    .get(n.file.as_path())
                    .is_some_and(|(_, i)| i.contains(&n.line)),
                lang: crate::highlight::detect(&n.file).and_then(Lang::from_key),
            })
            .collect();

        // The definitions looked up by name, indexed so a lookup costs what
        // it finds: scanning every same-named definition, or every
        // definition of the file, per call cost calls × definitions.
        let mut defs = Defs::default();
        for (i, n) in nodes.iter().enumerate() {
            // A test calls; no call lands on one.
            if calls_only(&n.kind) {
                defs.callers
                    .entry((n.file.as_path(), n.name.as_str()))
                    .or_default()
                    .push(i);
                continue;
            }
            let name = logical_name(&n.name, facts[i].lang);
            let named = defs.named.entry(name).or_default();
            named.all.push(i);
            if !facts[i].bodyless {
                named.defined.push(i);
            }
            let file = n.file.as_path();
            defs.in_file.entry((file, name)).or_default().push(i);
            defs.callers
                .entry((file, n.name.as_str()))
                .or_default()
                .push(i);
        }
        // Definitions within a file, ordered by line, so a call resolves to the
        // nearest preceding same-named definition.
        for v in defs.callers.values_mut() {
            v.sort_by_key(|&i| nodes[i].line);
        }

        // The class each method belongs to, by node: what a call whose
        // receiver's type is known narrows to.
        let owner_at: HashMap<(&Path, usize), &str> = files
            .iter()
            .flat_map(|f| {
                f.owners
                    .iter()
                    .map(move |(&line, name)| ((f.file.as_path(), line), name.as_str()))
            })
            .collect();
        let owner: Vec<Option<&str>> = nodes
            .iter()
            .map(|n| owner_at.get(&(n.file.as_path(), n.line)).copied())
            .collect();
        // The project's own types (and traits): what defines a method here.
        let project_types: HashSet<&str> = owner.iter().flatten().copied().collect();
        // Each owned definition by its owner's last part and its name: a
        // type's (a module's) function, looked up wherever it is defined.
        let mut by_owner: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (i, o) in owner.iter().enumerate() {
            if let Some(o) = o
                && !calls_only(&nodes[i].kind)
            {
                let last = o.rsplit('.').next().unwrap_or(o);
                by_owner
                    .entry((last, logical_name(&nodes[i].name, facts[i].lang)))
                    .or_default()
                    .push(i);
            }
        }

        // Python runs a class's `__init__` when the class is called:
        // `Inventory(...)`, `raise OutOfStock(...)`. By the class's name.
        let mut inits: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            if n.name == "__init__"
                && facts[i].lang == Some(Lang::Python)
                && let Some(class) = owner[i]
            {
                inits.entry(class).or_default().push(i);
            }
        }

        // The extension methods, by node: what a value of a type they
        // extend — a library's — is called with.
        let extension_lines: HashSet<(&Path, usize)> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .extensions
                    .iter()
                    .map(move |&line| (f.file.as_path(), line))
            })
            .collect();
        let extension_nodes: HashSet<usize> = (0..nodes.len())
            .filter(|&i| extension_lines.contains(&(nodes[i].file.as_path(), nodes[i].line)))
            .collect();
        // The type hierarchy, by name: each type's bases, each base's
        // subtypes, and every type the project declares.
        let mut bases_of: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut subtypes_of: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut declared_types: HashSet<&str> = HashSet::new();
        for f in files.iter().copied() {
            for (ty, bases) in &f.context.types {
                declared_types.insert(ty.as_str());
                for base in bases {
                    bases_of.entry(ty.as_str()).or_default().push(base.as_str());
                    subtypes_of
                        .entry(base.as_str())
                        .or_default()
                        .push(ty.as_str());
                }
            }
        }
        for list in bases_of.values_mut().chain(subtypes_of.values_mut()) {
            list.sort_unstable();
            list.dedup();
        }
        // The methods named `callee` a value of `ty` may run: the type's own
        // or, failing that, its nearest bases' (what it inherits; Go's
        // embedded types'), and — `overrides` — its subtypes' that override
        // it, as a value declared as `ty` may be one of them.
        let family = |ty: &str, callee: &str, overrides: bool| -> Vec<usize> {
            let last = ty.rsplit(['.', ':']).next().unwrap_or(ty);
            let mut out: Vec<usize> = Vec::new();
            let mut level: Vec<&str> = vec![last];
            let mut seen: HashSet<&str> = HashSet::from([last]);
            for _ in 0..12 {
                let found: Vec<usize> = level
                    .iter()
                    .flat_map(|t| by_owner.get(&(*t, callee)))
                    .flatten()
                    .copied()
                    .filter(|&c| !calls_only(&nodes[c].kind))
                    .collect();
                if !found.is_empty() {
                    out.extend(found);
                    break;
                }
                level = level
                    .iter()
                    .flat_map(|t| bases_of.get(t))
                    .flatten()
                    .copied()
                    .filter(|b| seen.insert(b))
                    .collect();
                if level.is_empty() {
                    break;
                }
            }
            if overrides {
                let mut stack: Vec<&str> = vec![last];
                let mut seen: HashSet<&str> = HashSet::from([last]);
                while let Some(t) = stack.pop() {
                    for &sub in subtypes_of.get(t).into_iter().flatten() {
                        if seen.len() > 2000 || !seen.insert(sub) {
                            continue;
                        }
                        out.extend(by_owner.get(&(sub, callee)).into_iter().flatten().copied());
                        stack.push(sub);
                    }
                }
            }
            out.sort_unstable();
            out.dedup();
            // A definition over a declaration of it (a C++ header's, an
            // interface's abstract one) where there is one.
            if out.iter().any(|&c| !facts[c].bodyless) {
                out.retain(|&c| !facts[c].bodyless);
            }
            out
        };
        // Whether the type `child` has `base` above it.
        let descends = |child: &str, base: &str| -> bool {
            let mut level: Vec<&str> = vec![child];
            let mut seen: HashSet<&str> = HashSet::from([child]);
            for _ in 0..12 {
                let next: Vec<&str> = level
                    .iter()
                    .flat_map(|t| bases_of.get(t))
                    .flatten()
                    .copied()
                    .filter(|b| seen.insert(b))
                    .collect();
                if next.contains(&base) {
                    return true;
                }
                if next.is_empty() {
                    return false;
                }
                level = next;
            }
            false
        };
        // The Elixir and Erlang modules the project defines, by each name a
        // call may give one: `Shop.Cart`, and `Cart` as an alias says it.
        let module_names: HashSet<&str> = owner
            .iter()
            .flatten()
            .flat_map(|o| {
                std::iter::once(*o).chain(o.match_indices('.').map(move |(at, _)| &o[at + 1..]))
            })
            .collect();
        // Each package's (namespace's) files: what a file of it reaches with
        // no import — Go's in its folder, the JVM languages' wherever they
        // are (a test beside its class under `src/test`), C#'s.
        let package_key = |fc: &FileCalls, package: &str| -> Option<String> {
            match fc.lang {
                Lang::Go => Some(format!("go\0{}\0{package}", fc.file.parent()?.display())),
                Lang::Java | Lang::Kotlin | Lang::Scala => Some(format!("jvm\0{package}")),
                Lang::CSharp => Some(format!("cs\0{package}")),
                _ => None,
            }
        };
        let mut packages: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        for fc in &files {
            if let Some(package) = fc.context.package.as_deref()
                && let Some(key) = package_key(fc, package)
            {
                packages.entry(key).or_default().insert(fc.file.clone());
            }
        }
        // A Rails app's autoloaded code: every class under its `app/` and
        // `lib/` (and its tests) is named anywhere with no `require`, as
        // Zeitwerk loads it. By the app's root, where its
        // `config/application.rb` is.
        let rails_roots: Vec<&Path> = files
            .iter()
            .filter_map(|f| {
                f.file
                    .ends_with("config/application.rb")
                    .then(|| f.file.parent()?.parent())
                    .flatten()
            })
            .collect();
        let rails_root_of = |file: &Path| -> Option<&Path> {
            rails_roots.iter().copied().find(|root| {
                ["app", "lib", "test", "spec"]
                    .iter()
                    .any(|d| file.starts_with(root.join(d)))
            })
        };
        let mut rails: HashMap<&Path, HashSet<PathBuf>> = HashMap::new();
        for fc in &files {
            if fc.lang == Lang::Ruby
                && let Some(root) = rails_root_of(&fc.file)
            {
                rails.entry(root).or_default().insert(fc.file.clone());
            }
        }
        // An Elixir module's file, by the module's name: what a `use` or an
        // `import` names — one with a `__using__` macro alone too.
        let mut module_files: HashMap<&str, &Path> = HashMap::new();
        for f in files.iter().copied() {
            if f.lang == Lang::Elixir {
                let declared = f.context.types.iter().map(|(name, _)| name);
                for module in f.owners.values().chain(declared) {
                    module_files
                        .entry(module.as_str())
                        .or_insert(f.file.as_path());
                }
            }
        }
        // What an Elixir file reaches with no alias of its own: what each
        // module it `use`s injects — that module's own file (a `def` in its
        // `__using__`), the modules it imports and aliases, and in turn what
        // the modules it uses inject.
        let context_of: HashMap<&Path, &FileContext> = files
            .iter()
            .map(|f| (f.file.as_path(), &f.context))
            .collect();
        // Its bare calls reach only its own file, the modules it imports,
        // and the files the modules it uses inject.
        let mut bare_scopes: HashMap<&Path, HashSet<&Path>> = HashMap::new();
        let mut used_scopes: HashMap<&Path, HashSet<PathBuf>> = HashMap::new();
        for fc in files.iter().copied() {
            if fc.lang != Lang::Elixir {
                continue;
            }
            let mut injected: HashSet<&Path> = HashSet::new();
            let mut imported: Vec<&str> = fc.context.imports.iter().map(String::as_str).collect();
            let mut pending: Vec<&str> = fc.context.uses.iter().map(String::as_str).collect();
            let mut used: HashSet<&str> = HashSet::new();
            while let Some(module) = pending.pop() {
                if !used.insert(module) {
                    continue;
                }
                let Some(&file) = module_files.get(module) else {
                    continue;
                };
                injected.insert(file);
                if let Some(ctx) = context_of.get(file) {
                    imported.extend(ctx.imports.iter().map(String::as_str));
                    pending.extend(ctx.uses.iter().map(String::as_str));
                }
            }
            let mut reached = injected.clone();
            reached.extend(
                imported
                    .into_iter()
                    .filter_map(|m| module_files.get(m).copied()),
            );
            let mut seen: HashSet<PathBuf> = reached.iter().map(|f| f.to_path_buf()).collect();
            for file in &injected {
                seen.extend(scope.get(*file).into_iter().flatten().cloned());
            }
            seen.remove(&fc.file);
            if !seen.is_empty() {
                used_scopes.insert(fc.file.as_path(), seen);
            }
            bare_scopes.insert(fc.file.as_path(), reached);
        }
        // What RSpec loads before each spec under a `spec/` folder: its
        // `spec_helper.rb` and `rails_helper.rb` (`.rspec`'s `--require`),
        // and the `spec/support/` files they require by a glob.
        let mut spec_roots: HashMap<&Path, HashSet<PathBuf>> = HashMap::new();
        for f in files.iter().copied() {
            if f.lang == Lang::Ruby
                && matches!(
                    f.file.file_name().and_then(|n| n.to_str()),
                    Some("spec_helper.rb" | "rails_helper.rb")
                )
                && let Some(root) = f.file.parent()
                && root.file_name().is_some_and(|n| n == "spec")
            {
                spec_roots.entry(root).or_default().insert(f.file.clone());
            }
        }
        if !spec_roots.is_empty() {
            for f in files.iter().copied() {
                if f.lang != Lang::Ruby {
                    continue;
                }
                for (root, loaded) in spec_roots.iter_mut() {
                    if f.file.starts_with(root.join("support")) {
                        loaded.insert(f.file.clone());
                    }
                }
            }
        }
        // A Scala package's files by the package's last name (`os` for
        // `package os`, and for `package object os`): what `os.read(p)`
        // names.
        let mut scala_packages: HashMap<&str, HashSet<&Path>> = HashMap::new();
        for f in files.iter().copied() {
            if f.lang == Lang::Scala
                && let Some(package) = f.context.package.as_deref()
            {
                let last = package.rsplit('.').next().unwrap_or(package);
                scala_packages
                    .entry(last)
                    .or_default()
                    .insert(f.file.as_path());
            }
        }
        // A Scala object's `apply`: what calling the object runs
        // (`RelPath("a")`, `os.read(p)` for `object read`).
        let applies_of = |object: &str| -> Vec<usize> {
            by_owner
                .get(&(object, "apply"))
                .into_iter()
                .flatten()
                .copied()
                .filter(|&i| facts[i].lang == Some(Lang::Scala))
                .collect()
        };
        let siblings_of = |fc: &FileCalls| -> Vec<&HashSet<PathBuf>> {
            let mut extra: Vec<&HashSet<PathBuf>> = Vec::new();
            if fc.lang == Lang::Ruby
                && let Some(group) = rails_root_of(&fc.file).and_then(|root| rails.get(root))
            {
                extra.push(group);
            }
            if let Some(used) = used_scopes.get(fc.file.as_path()) {
                extra.push(used);
            }
            let Some(package) = fc.context.package.as_deref() else {
                return extra;
            };
            // C# sees its enclosing namespaces' types too: `Shop.Cart`'s
            // code names `Shop`'s with no `using`.
            let mut names = vec![package];
            if fc.lang == Lang::CSharp {
                let mut rest = package;
                while let Some((outer, _)) = rest.rsplit_once('.') {
                    names.push(outer);
                    rest = outer;
                }
            }
            extra.extend(
                names
                    .into_iter()
                    .filter_map(|name| packages.get(&package_key(fc, name)?)),
            );
            extra
        };

        // Which files are tests', by their path under the files' common
        // folder (the project's): a test calls the code, never the code a
        // test — a helper a test file defines is no callee of the app's.
        let common: PathBuf = {
            let mut paths = nodes.iter().map(|n| n.file.as_path());
            let mut common = paths
                .next()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            for p in paths {
                while !p.starts_with(&common) && common.pop() {}
            }
            common
        };
        let is_test_file = |file: &Path| {
            file.strip_prefix(&common)
                .ok()
                .and_then(|rel| rel.to_str())
                .is_some_and(crate::weakspots::is_test_path)
        };
        let test_node: Vec<bool> = nodes.iter().map(|n| is_test_file(&n.file)).collect();
        // A test class's methods (`StatusCodesTest.get`): the runner holds
        // its instance, so no other code calls them on a value.
        let test_method: Vec<bool> = (0..nodes.len())
            .map(|i| test_node[i] && owner[i].is_some_and(is_test_class_name))
            .collect();
        let empty_scope: HashSet<PathBuf> = HashSet::new();
        let mut edges: HashSet<(usize, usize)> = HashSet::new();
        for fc in files {
            let in_test = is_test_file(&fc.file);
            let direct = scope.get(&fc.file).unwrap_or(&empty_scope);
            // A C or C++ file sees what every header it includes includes,
            // as the preprocessor pastes them in: `main.cpp` includes
            // `shop.hpp`, which includes `cart.hpp`, and calls `Cart`'s
            // methods.
            // So does a Ruby file see what the files it requires require: a
            // `require` loads a file once, for the whole program. And a Dart
            // library what the libraries it imports export — a package's
            // `shelf.dart` is its `src/` files re-exported.
            // A spec sees what RSpec loaded before it, and what that requires.
            let spec_loaded: Option<HashSet<PathBuf>> = (fc.lang == Lang::Ruby
                && !fc.calls.is_empty())
            .then(|| {
                spec_roots
                    .iter()
                    .filter(|(root, _)| fc.file.starts_with(root))
                    .flat_map(|(_, loaded)| loaded.iter())
                    .filter(|f| **f != fc.file)
                    .cloned()
                    .collect::<HashSet<PathBuf>>()
            })
            .filter(|loaded| !loaded.is_empty())
            .map(|mut loaded| {
                loaded.extend(direct.iter().cloned());
                loaded
            });
            let direct = spec_loaded.as_ref().unwrap_or(direct);
            let included = (matches!(fc.lang, Lang::C | Lang::Cpp | Lang::Ruby | Lang::Dart)
                && !fc.calls.is_empty())
            .then(|| include_closure(&fc.file, direct, scope));
            let imported = included.as_ref().unwrap_or(direct);
            let siblings = siblings_of(fc);
            let site = Site {
                file: &fc.file,
                lang: fc.lang,
                imported,
                siblings: &siblings,
            };
            // What a name called from this file resolves to depends on the
            // name and the call's shape only: worked out once per file.
            let mut resolved: HashMap<(&str, bool, Option<u8>), Vec<usize>> = HashMap::new();
            for cs in &fc.calls {
                // Bare calls to language builtins (`len(x)`, `make(...)`) are not
                // project functions; skip them so they don't resolve to some
                // same-named definition and inflate the graph.
                if is_library_call(fc.lang, cs) {
                    continue;
                }
                let Some(caller_name) = cs.caller.as_deref() else {
                    continue; // a top-level call has no caller function node
                };
                let Some(caller) = resolve_caller(
                    &defs.callers,
                    &fc.file,
                    caller_name,
                    cs.line,
                    &nodes,
                    &fc.bodies,
                ) else {
                    continue;
                };
                let callees = resolved
                    .entry((cs.callee.as_str(), cs.method, cs.arity))
                    .or_insert_with(|| resolve_callees(&defs, &nodes, &facts, &owner, cs, &site));
                // Scala's `pkg.member(…)`: the package's own function (a
                // package object's) or its object's `apply` — and nothing
                // else, as the package names the member.
                if fc.lang == Lang::Scala
                    && let Some(package) = cs.qualifier.as_deref()
                    && let Some(package_files) = scala_packages.get(package)
                {
                    let callee_name = logical_name(&cs.callee, Some(fc.lang));
                    let in_package = |i: &usize| package_files.contains(nodes[*i].file.as_path());
                    let mut found: Vec<usize> = defs
                        .named
                        .get(callee_name)
                        .into_iter()
                        .flat_map(|named| named.all.iter().copied())
                        .filter(in_package)
                        .filter(|&i| owner[i].is_none_or(|o| o.rsplit('.').next() == Some(package)))
                        .collect();
                    found.extend(applies_of(callee_name).into_iter().filter(in_package));
                    if found.len() <= MAX_CANDIDATES {
                        for callee in found {
                            if edges.len() < MAX_EDGES && (in_test || !test_node[callee]) {
                                edges.insert((caller, callee));
                            }
                        }
                    }
                    continue;
                }
                // A Scala object called as a function (`RelPath("a")`), where
                // no function in scope has the name: its `apply`.
                if fc.lang == Lang::Scala && !cs.method && callees.is_empty() {
                    let callee_name = logical_name(&cs.callee, Some(fc.lang));
                    let found: Vec<usize> = applies_of(callee_name)
                        .into_iter()
                        .filter(|&i| nodes[i].file == fc.file || site.sees(&nodes[i].file))
                        .collect();
                    if !found.is_empty() && found.len() <= MAX_CANDIDATES {
                        for callee in found {
                            if edges.len() < MAX_EDGES && (in_test || !test_node[callee]) {
                                edges.insert((caller, callee));
                            }
                        }
                        continue;
                    }
                }
                // A receiver of a known type calls that type's method of the
                // name: `cart.total()` after `cart = Cart()` is `Cart.total`,
                // not every `total` there is. A type with no method of the
                // name (an inherited one) keeps them all.
                let wanted = match &cs.receiver {
                    Some(Receiver::Own) => owner[caller],
                    Some(Receiver::Type(ty) | Receiver::Declared(ty)) => Some(ty.as_str()),
                    None => None,
                };
                let callee_name = logical_name(&cs.callee, Some(fc.lang));
                // A language whose types inherit (or, Go, embed) methods: a
                // receiver declared as a type, or the caller's own class,
                // calls the type's method, its own or its nearest base's,
                // or a subtype's override — wherever each is defined.
                let inherits = has_inheritance(fc.lang);
                if inherits
                    && let Some(ty) = wanted
                    && matches!(cs.receiver, Some(Receiver::Own | Receiver::Declared(_)))
                {
                    let found = family(ty, callee_name, true);
                    let declared_here = declared_types
                        .contains(ty.rsplit(['.', ':']).next().unwrap_or(ty))
                        || project_types.contains(ty);
                    let targets: Vec<usize> = match (found.is_empty(), declared_here) {
                        (false, _) if found.len() <= MAX_CANDIDATES => found,
                        (false, _) => Vec::new(),
                        // A type the project does not declare (`List`,
                        // `String`): its methods are not the project's,
                        // but for the project's extensions of it.
                        (true, false) if matches!(cs.receiver, Some(Receiver::Declared(_))) => {
                            callees
                                .iter()
                                .copied()
                                .filter(|c| extension_nodes.contains(c))
                                .collect()
                        }
                        // No method of the name on the type or above it
                        // that this can see (a mixin, a library's base): by
                        // the name in scope, as with no type.
                        (true, _) => callees.clone(),
                    };
                    for callee in targets {
                        if edges.len() < MAX_EDGES && (in_test || !test_node[callee]) {
                            edges.insert((caller, callee));
                        }
                    }
                    continue;
                }
                // A Lean namespace is named from where the call is: `Voice`
                // for `Clew.Extracted.Voice`, opened.
                // So is an Elixir module by the last of its parts, as an
                // `alias Shop.Cart` lets `Cart.add` say it.
                let owned_by = |c: usize, ty: &str| match owner[c] {
                    Some(o) if matches!(fc.lang, Lang::Lean | Lang::Elixir) => {
                        o == ty || o.strip_suffix(ty).is_some_and(|o| o.ends_with('.'))
                    }
                    o => o == Some(ty),
                };
                let mut narrowed: Vec<usize> = wanted
                    .map(|ty| {
                        callees
                            .iter()
                            .copied()
                            .filter(|&c| owned_by(c, ty))
                            .collect()
                    })
                    .unwrap_or_default();
                // A Rust type's function is its own wherever it is defined:
                // `Builder::new()` in a file with other `new`s, `Builder`'s
                // defined in another — the name's nearest were not it. So
                // is an Elixir or Erlang module's (`Cart.add(…)`,
                // `cart:add(…)`), which a call reaches with no import: a
                // test module's every call of the module it tests had no
                // callee.
                // Java's, Kotlin's and Scala's classes in the same package,
                // C#'s in the same namespace and Ruby's once required are
                // named with no import either (`Cart.add(…)` from the
                // test beside it): where nothing in scope is the type's,
                // its own function is.
                if narrowed.is_empty()
                    && matches!(
                        fc.lang,
                        Lang::Rust
                            | Lang::Elixir
                            | Lang::Erlang
                            | Lang::Java
                            | Lang::Kotlin
                            | Lang::Scala
                            | Lang::CSharp
                            | Lang::Ruby
                            | Lang::Go
                            | Lang::Dart
                    )
                    && let Some(ty) = wanted
                {
                    let last = ty.rsplit('.').next().unwrap_or(ty);
                    narrowed = by_owner
                        .get(&(last, callee_name))
                        .into_iter()
                        .flatten()
                        .copied()
                        .filter(|&c| owned_by(c, ty) && facts[c].takes(cs.arity))
                        .collect();
                }
                // An exact type with no method of the name: the one it
                // inherits (or, in Go, has from an embedded type).
                if narrowed.is_empty()
                    && inherits
                    && let Some(Receiver::Type(ty)) = &cs.receiver
                {
                    narrowed = family(ty, callee_name, false);
                    if narrowed.len() > MAX_CANDIDATES {
                        narrowed.clear();
                    }
                }
                // A bare call in a method that names nothing in scope: the
                // class's own method, or the one it inherits (`getName()`
                // in `Owner` is `Person`'s).
                if !cs.method
                    && callees.is_empty()
                    && implicit_self(fc.lang)
                    && let Some(own) = owner[caller]
                {
                    let mut found = family(own, callee_name, true);
                    if found.len() > MAX_CANDIDATES {
                        found.clear();
                    }
                    for callee in found {
                        if edges.len() < MAX_EDGES && (in_test || !test_node[callee]) {
                            edges.insert((caller, callee));
                        }
                    }
                    continue;
                }
                // A Rust receiver of a type the project defines nothing of —
                // `Vec`, `str`, a type parameter — calls none of its
                // functions: `haystack.len()` on a `&str` drew an edge to
                // every `len` there was. (A language with inheritance keeps
                // them: the method may be a base class's.)
                // So does a Lean name in a namespace the project has none
                // of: `Nat.succ` is no project `succ`.
                let foreign = match (&cs.receiver, fc.lang) {
                    (Some(Receiver::Type(ty)), Lang::Rust) => !project_types.contains(ty.as_str()),
                    (Some(Receiver::Type(ty)), Lang::Lean) => {
                        !(0..nodes.len()).any(|c| owner[c].is_some() && owned_by(c, ty))
                    }
                    // A JavaScript runtime's object, as no project class of
                    // that name is.
                    (Some(Receiver::Type(ty)), Lang::JavaScript | Lang::TypeScript | Lang::Tsx) => {
                        JS_GLOBALS.contains(&ty.as_str()) && !project_types.contains(ty.as_str())
                    }
                    // A module the project defines none of (`Map.put`,
                    // `:crypto.hash`, `lists:map`): the library's.
                    (Some(Receiver::Type(ty)), Lang::Elixir | Lang::Erlang) => {
                        !module_names.contains(ty.as_str())
                    }
                    // A value of a type exactly known and the project's
                    // none (`new AtomicReference(…)`, `std::vector`): the
                    // library's methods, but for the project's extensions.
                    (
                        Some(Receiver::Type(ty)),
                        Lang::Java
                        | Lang::Kotlin
                        | Lang::Scala
                        | Lang::CSharp
                        | Lang::Dart
                        | Lang::Cpp,
                    ) => {
                        let last = ty.rsplit(['.', ':']).next().unwrap_or(ty);
                        !project_types.contains(last)
                            && !project_types.contains(ty.as_str())
                            && !declared_types.contains(last)
                    }
                    // Zig's standard library's (`writer: *std.Io.Writer`).
                    (Some(Receiver::Type(ty)), Lang::Zig) => ty == "std" || ty.starts_with("std."),
                    _ => false,
                };
                let extensions_only: Vec<usize>;
                let targets: &[usize] = if foreign {
                    extensions_only = callees
                        .iter()
                        .copied()
                        .filter(|c| extension_nodes.contains(c))
                        .collect();
                    &extensions_only
                } else if narrowed.is_empty() {
                    callees
                } else {
                    &narrowed
                };
                // A method called on a value of no known type in Java,
                // Kotlin, Scala or C# is some class's: never a top-level
                // function, but for an extension of a type.
                let methods_only = cs.method
                    && cs.receiver.is_none()
                    && matches!(
                        fc.lang,
                        Lang::Java | Lang::Kotlin | Lang::Scala | Lang::CSharp
                    );
                // A bare call in Dart, JavaScript, Python or PHP is a
                // function's, or one of the caller's own class (or its
                // bases): another class's method needs its object.
                let own_or_free = !cs.method
                    && matches!(
                        fc.lang,
                        Lang::Dart
                            | Lang::JavaScript
                            | Lang::TypeScript
                            | Lang::Tsx
                            | Lang::Python
                            | Lang::Php
                    );
                // An Elixir bare call: a function of its own file, or of a
                // module it imports.
                let elixir_bare = !cs.method && fc.lang == Lang::Elixir;
                let kept: Vec<usize>;
                let targets: &[usize] = if elixir_bare {
                    let reached = bare_scopes.get(fc.file.as_path());
                    kept = targets
                        .iter()
                        .copied()
                        .filter(|&c| {
                            nodes[c].file == fc.file
                                || reached.is_some_and(|r| r.contains(nodes[c].file.as_path()))
                        })
                        .collect();
                    &kept
                } else if methods_only || own_or_free {
                    kept = targets
                        .iter()
                        .copied()
                        .filter(|&c| match (methods_only, owner[c]) {
                            (true, None) => extension_nodes.contains(&c),
                            (true, Some(_)) => true,
                            (false, None) => true,
                            // A constructor (`CounterCubit()`) is called
                            // bare from anywhere.
                            (false, Some(o))
                                if nodes[c].name == o.rsplit('.').next().unwrap_or(o) =>
                            {
                                true
                            }
                            (false, Some(o)) => {
                                owner[caller].is_some_and(|own| own == o || descends(own, o))
                            }
                        })
                        .collect();
                    &kept
                } else {
                    targets
                };
                // A class called: its `__init__`, of a class this file
                // defines or imports.
                let constructed = (fc.lang == Lang::Python)
                    .then(|| inits.get(cs.callee.as_str()))
                    .flatten()
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|&i| nodes[i].file == fc.file || imported.contains(&nodes[i].file));
                for callee in targets.iter().copied().chain(constructed) {
                    // `f(a)` is `f/1`: no call of `f/2`.
                    if !facts[callee].takes(cs.arity) {
                        continue;
                    }
                    // `client.get(…)`: no value is a test class's, its own
                    // class's methods called on another value included.
                    if cs.method && cs.receiver.is_none() && test_method[callee] {
                        continue;
                    }
                    // A self-call is kept as a callee only (see `finalize`).
                    if edges.len() < MAX_EDGES && (in_test || !test_node[callee]) {
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

    /// The functions among the nodes: not the tests that call them.
    pub fn function_count(&self) -> usize {
        self.nodes.iter().filter(|n| !calls_only(&n.kind)).count()
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
            // A test's key may be a same-named function's.
            if calls_only(&n.kind) {
                continue;
            }
            let callees = out.entry(key(i)).or_default();
            // Not itself: what it waits on to be explained first.
            callees.extend(n.callees.iter().filter(|&&c| c != i).map(|&c| key(c)));
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
        self.by_key
            .get(&key_hash(file, name, ordinal))?
            .iter()
            .copied()
            .find(|&i| {
                self.ordinals[i] == ordinal
                    && self.nodes[i].name == name
                    && self.nodes[i].file == file
                    && !calls_only(&self.nodes[i].kind)
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
            .filter(|&i| self.nodes[i].callers.is_empty() && !calls_only(&self.nodes[i].kind))
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
    /// A C or C++ function of its file alone (see [`FileCalls::internal`]).
    internal: bool,
    /// The language of the node's file, by extension.
    lang: Option<Lang>,
    /// An Erlang or Elixir function's arities ([`FileContext::arities`]).
    arity: Option<(u8, u8)>,
}

impl NodeFacts {
    /// Whether a call passing `passed` arguments can be of this function:
    /// of a BEAM function, one of its arities; of any other, yes.
    fn takes(&self, passed: Option<u8>) -> bool {
        match (passed, self.arity) {
            (Some(n), Some((fewest, most))) => (fewest..=most).contains(&n),
            _ => true,
        }
    }
}

/// The project's definitions, by name (see `ProjectCallGraph::build`).
#[derive(Default)]
struct Defs<'a> {
    /// By logical name ([`logical_name`]).
    named: HashMap<&'a str, Named>,
    /// By file and logical name.
    in_file: HashMap<(&'a Path, &'a str), Vec<usize>>,
    /// By file and name as written, in line order: a call's enclosing
    /// function ([`resolve_caller`]).
    callers: HashMap<(&'a Path, &'a str), Vec<usize>>,
}

/// The definitions of one name: all of them, and those with a body.
#[derive(Default)]
struct Named {
    all: Vec<usize>,
    defined: Vec<usize>,
}

/// Where a call was made.
struct Site<'a> {
    file: &'a Path,
    lang: Lang,
    imported: &'a HashSet<PathBuf>,
    /// The other files of the call's package (namespace), whose names it
    /// reaches with no import ([`FileContext::package`]); a C# file's
    /// enclosing namespaces' too.
    siblings: &'a [&'a HashSet<PathBuf>],
}

impl Site<'_> {
    /// Whether a definition in `file` is in the call's scope.
    fn sees(&self, file: &Path) -> bool {
        self.imported.contains(file) || self.siblings.iter().any(|s| s.contains(file))
    }

    /// How many files are in scope (counting a file twice where it is both
    /// imported and a sibling).
    fn scope_len(&self) -> usize {
        self.imported.len() + self.siblings.iter().map(|s| s.len()).sum::<usize>()
    }

    /// The files in scope.
    fn scope(&self) -> impl Iterator<Item = &PathBuf> {
        self.imported
            .iter()
            .chain(self.siblings.iter().flat_map(|s| s.iter()))
    }
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
            // A component rendered (`<Button …/>`) is a call of it, as
            // React calls it.
            &[
                "call_expression",
                "new_expression",
                "jsx_opening_element",
                "jsx_self_closing_element",
            ],
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
        // A closure (`function () use ($x) {…}`, `fn($x) => …`) is no scope:
        // the outline does not list it, so its calls are the method's.
        Lang::Php => spec(
            &["function_definition", "method_declaration"],
            &[
                "function_call_expression",
                "member_call_expression",
                "nullsafe_member_call_expression",
                "scoped_call_expression",
                "object_creation_expression",
            ],
        ),
        // Only `call` nodes: a bare identifier statement (`setup`) is a call
        // to a method or a read of a local variable, and only the method's
        // locals tell which.
        Lang::Ruby => spec(&["method", "singleton_method"], &["call"]),
        Lang::CSharp => spec(
            &[
                "method_declaration",
                "constructor_declaration",
                "local_function_statement",
            ],
            &["invocation_expression", "object_creation_expression"],
        ),
        // Each clause of a function is a `function_clause` naming it (the
        // outline merges a function's clauses into one item); the clauses of
        // an anonymous `fun` are `fun_clause`s, so its calls are the
        // enclosing function's.
        // `fun add/2` and `fun cart:add/2` name a function to be called
        // (`lists:map(fun cart:add/2, …)`): a use of it, as a call is.
        Lang::Erlang => spec(
            &["function_clause"],
            &["call", "internal_fun", "external_fun"],
        ),
        // A `bind` is a scope as well as a `function`: `main = do …` and
        // `f = \x -> …` are binds, and the outline lists every top-level one
        // as a function — without it `main`'s calls had no caller. A bind
        // in a `do` block (`x <- act`) has a pattern, not a name, so it is
        // none.
        // A function passed as a value (`foldr add 0 xs`, `show . add 1`)
        // is used where it is passed, as the function it is passed to calls
        // it: higher-order and point-free code use most functions so.
        Lang::Haskell => spec(
            &["function", "bind"],
            &["apply", "variable", "qualified", "infix", "prefix_id"],
        ),
        Lang::Zig => spec(&["function_declaration"], &["call_expression"]),
        // `Foo(…)` constructs a class as a function is called; `a.b(…)`
        // is a `navigation_expression` callee.
        Lang::Kotlin => spec(&["function_declaration"], &["call_expression"]),
        // `new Foo(…)` is an `instance_expression`; `Foo(…)` (a case class,
        // an `apply`) a call.
        Lang::Scala => spec(
            &["function_definition"],
            &["call_expression", "instance_expression", "field_expression"],
        ),
        // A `def` is a call to the grammar too: a `call` is a function's
        // scope when it defines one (see `fn_names`), and a call otherwise.
        // `&add/2` captures a function to be called (`&Cart.add/2`'s
        // `Cart.add` is a call node of its own).
        Lang::Elixir => spec(&["call"], &["call", "unary_operator"]),
        // A schema has no calls; a Lean proof names the lemmas it uses inside
        // tactics (`rw [h]`, `simp [f]`) far more often than it applies them,
        // so a call graph of applications alone would say a proof uses
        // nothing.
        Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Scss
        | Lang::Less
        | Lang::Vue
        | Lang::Svelte
        | Lang::Astro
        | Lang::Sql => {
            return None;
        }
        // Read from the text (see `crate::lean::calls`): no tree's kinds.
        Lang::Lean => spec(&[], &[]),
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
    if lang == Lang::Lean {
        return crate::lean::calls(source, symbols);
    }
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
    in_test_blocks(&mut out, symbols);
    out
}

/// Each call made inside a test or a group of tests (`it "adds" do … end`,
/// `test "adds" { … }`, `describe`) made from it: from the innermost one
/// around it — unless a function defined inside that test holds it. A test
/// block is no function to the grammar, so its calls had no caller (or, in
/// a Dart or hspec file, the `main` or `spec` its tests sit in).
fn in_test_blocks(calls: &mut [CallSite], symbols: &[Located]) {
    let span = |l: &Located| (l.symbol.line, l.symbol.end_line.max(l.symbol.line));
    let blocks: Vec<(usize, usize, &str)> = symbols
        .iter()
        .filter(|l| calls_only(&l.symbol.kind))
        .map(|l| {
            let (first, last) = span(l);
            (first, last, l.symbol.name.as_str())
        })
        .collect();
    if blocks.is_empty() {
        return;
    }
    let innermost = |spans: &mut dyn Iterator<Item = (usize, usize)>, line: usize| {
        spans
            .filter(|&(first, last)| first <= line && line <= last)
            .min_by_key(|&(first, last)| (last - first, std::cmp::Reverse(first)))
    };
    for call in calls.iter_mut() {
        let Some(block) = blocks
            .iter()
            .filter(|&&(first, last, _)| first <= call.line && call.line <= last)
            .min_by_key(|&&(first, last, _)| (last - first, std::cmp::Reverse(first)))
        else {
            continue;
        };
        // The function the walk found around it, if the file defines it
        // there: a test inside it (Dart's `main`) is nearer; one it is
        // inside (a helper the test defines) is not.
        let function = call.caller.as_deref().and_then(|caller| {
            innermost(
                &mut symbols
                    .iter()
                    .filter(|l| is_callable(&l.symbol.kind) && l.symbol.name == caller)
                    .map(span),
                call.line,
            )
        });
        let in_function = function.is_some_and(|(first, last)| block.0 < first || last < block.1);
        if !in_function {
            call.caller = Some(block.2.to_string());
        }
    }
}

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

/// One node of [`walk`]: the node, its parent and the sibling before it
/// (carried, as tree-sitter finds a parent by walking down from the root, a
/// cost of the depth per call, and a sibling through its parent), and the
/// enclosing function in scope for it: its name, and the byte its
/// definition starts at (where a receiver's binding is looked for).
type WalkItem<'t> = (
    Node<'t>,
    Option<Node<'t>>,
    Option<Node<'t>>,
    Option<(Rc<str>, usize)>,
    // Where the innermost function around the node starts, named or not:
    // the scope a receiver's name is bound in (a closure's own parameter).
    Option<usize>,
);

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
    let mut stack: Vec<WalkItem> = vec![(root, None, None, None, None)];
    // What each function's names are bound to, read once as far as its
    // calls reach (`Bindings`): reading the function's text again for
    // every call made the walk quadratic in a long function.
    let mut bindings: HashMap<(usize, String), Bindings> = HashMap::new();
    // What each class's fields are declared as, by the class's start and
    // the field's name, read once.
    let mut fields: HashMap<(usize, String), Option<String>> = HashMap::new();
    // Each function's (or closure's) parameters' names, by its node.
    let mut parameters: HashMap<usize, Vec<String>> = HashMap::new();
    let places = Places::read(root, spec.fn_kinds);
    while let Some((node, parent, before, enclosing, inner)) = stack.pop() {
        let kind = node.kind();
        if spec.skip_kinds.contains(&kind) {
            continue;
        }
        // A function definition becomes the enclosing scope for its subtree —
        // under the first of its names that the file's outline has.
        let own: Option<(Rc<str>, usize)> = if spec.fn_kinds.contains(&kind) {
            fn_names(node, parent, before, src, lang)
                .into_iter()
                .find(|name| callable.is_none_or(|c| c.contains(name.as_str())))
                .map(|s| (Rc::from(s.as_str()), node.start_byte()))
        } else {
            None
        };
        let current = own.or(enclosing);
        let inner = if spec.fn_kinds.contains(&kind) {
            Some(node.start_byte())
        } else {
            inner
        };

        if spec.call_kinds.contains(&kind)
            && let Some((callee, method)) = callee_name(node, parent, src, lang)
            // A parameter called (`emit('done')` in `(event, emit) { … }`,
            // `cb()`) is the value passed, no function of the project's.
            && (method || !calls_a_parameter(node, &callee, src, lang, &mut parameters))
        {
            // The receiver's name where nothing binds it (Scala's `os`).
            let mut unbound: Option<String> = None;
            let receiver = match &current {
                // `Cart.add(…)`, `cart:add(…)`: the module named.
                _ if method && matches!(lang, Lang::Elixir | Lang::Erlang) => {
                    module_receiver(node, parent, src, lang)
                }
                Some((_, from)) if method => {
                    let scope = Scope {
                        places: &places,
                        from: inner.unwrap_or(*from),
                    };
                    receiver_of(
                        node,
                        &callee,
                        &scope,
                        src,
                        lang,
                        &mut bindings,
                        &mut fields,
                        &mut unbound,
                    )
                }
                // In no function: in a test block (`test("adds") { … }`),
                // whose calls are its own (`in_test_blocks`), its names
                // bound from the file's start.
                None if method && lang != Lang::Rust => {
                    let scope = Scope {
                        places: &places,
                        from: inner.unwrap_or(0),
                    };
                    receiver_of(
                        node,
                        &callee,
                        &scope,
                        src,
                        lang,
                        &mut bindings,
                        &mut fields,
                        &mut unbound,
                    )
                }
                _ if lang == Lang::Rust => rust_path_receiver(node, src),
                _ => None,
            };
            let qualifier = unbound.filter(|_| receiver.is_none());
            let arity = call_arity(node, parent, src, lang);
            out.push(CallSite {
                caller: current.as_ref().map(|(name, _)| name.to_string()),
                callee,
                method,
                receiver,
                qualifier,
                arity,
                line: node.start_position().row + 1,
            });
        }
        // Ruby calls a method with no arguments by its bare name, which
        // its grammar cannot tell from a variable: a name standing as a
        // statement that the method has not bound is a call.
        if lang == Lang::Ruby
            && kind == "identifier"
            && ruby_bare_call(node, parent, src, current.as_ref().map(|(_, from)| *from))
        {
            out.push(CallSite {
                caller: current.as_ref().map(|(name, _)| name.to_string()),
                callee: node_text(node, src).to_string(),
                method: false,
                receiver: None,
                qualifier: None,
                arity: None,
                line: node.start_position().row + 1,
            });
        }
        if spec.macro_calls && kind == "token_tree" {
            macro_calls(node, src, current.as_ref().map(|(name, _)| &**name), out);
        }

        let mut cursor = node.walk();
        let mut before = None;
        for child in node.children(&mut cursor) {
            stack.push((child, Some(node), before, current.clone(), inner));
            before = Some(child);
        }
    }
}

/// Whether the Ruby `identifier` `node` is a method called by its bare name:
/// a statement of its own (`greet`), a modifier's body (`log_it if ok`), a
/// block's — and not a name the method around it binds before it (a local
/// assigned, a parameter, a block's parameter), which is its value: a
/// method's last line `total` returns the local.
fn ruby_bare_call(node: Node, parent: Option<Node>, src: &str, from: Option<usize>) -> bool {
    let Some(parent) = parent else {
        return false;
    };
    let statement = match parent.kind() {
        "body_statement"
        | "block_body"
        | "then"
        | "else"
        | "begin"
        | "ensure"
        | "do"
        | "program"
        | "parenthesized_statements" => true,
        "if_modifier" | "unless_modifier" | "while_modifier" | "until_modifier"
        | "rescue_modifier" => parent
            .child_by_field_name("body")
            .is_some_and(|body| body.id() == node.id()),
        _ => false,
    };
    if !statement {
        return false;
    }
    let name = node_text(node, src);
    let Some(before) = src.get(from.unwrap_or(0)..node.start_byte()) else {
        return false;
    };
    let word = |c: char| c == '_' || c.is_alphanumeric();
    let mut at = 0;
    while let Some(i) = before[at..].find(name) {
        let start = at + i;
        let end = start + name.len();
        at = end;
        let head = &before[..start];
        if head.ends_with(|c: char| word(c) || matches!(c, '.' | '@' | '$' | ':'))
            || before[end..].starts_with(|c: char| word(c) || matches!(c, '?' | '!'))
        {
            continue;
        }
        let after = before[end..].trim_start_matches([' ', '\t']);
        let line_rest = &after[..after.find('\n').unwrap_or(after.len())];
        let is_assign = |text: &str| {
            text.starts_with('=')
                && !text.starts_with("==")
                && !text.starts_with("=~")
                && !text.starts_with("=>")
        };
        // `name = …` (not `==`, `=~`, `=>`), `name += …`, and one of
        // several assigned at once (`name, rest = …`).
        let assigned = is_assign(after)
            || ["+=", "-=", "*=", "/=", "||=", "&&=", "<<="]
                .iter()
                .any(|op| after.starts_with(op))
            || (after.starts_with(',')
                && line_rest.match_indices('=').any(|(i, _)| {
                    is_assign(&line_rest[i..]) && !line_rest[..i].ends_with(['=', '!', '<', '>'])
                }));
        let line_head = &head[head.rfind('\n').map_or(0, |n| n + 1)..];
        // A parameter: in the `def`'s list, or a block's `|…|`.
        let parameter = (line_head.trim_start().starts_with("def ")
            && (line_head.contains('(') || line_head.trim_end().ends_with(',')))
            || line_head.matches('|').count() % 2 == 1;
        if assigned || parameter {
            return false;
        }
    }
    true
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
            receiver: None,
            qualifier: None,
            arity: None,
            line: token.start_position().row + 1,
        });
    }
}

/// How many arguments an Erlang or Elixir call passes ([`CallSite::arity`]).
fn call_arity(call: Node, parent: Option<Node>, src: &str, lang: Lang) -> Option<u8> {
    let count = |args: Node| {
        args.named_children(&mut args.walk())
            .filter(|a| a.kind() != "comment")
            .count()
    };
    let n = match (lang, call.kind()) {
        // `fun f/2`, `fun m:f/2`.
        (Lang::Erlang, "internal_fun" | "external_fun") => {
            node_text(call.child_by_field_name("arity")?, src)
                .trim()
                .trim_start_matches('/')
                .trim()
                .parse()
                .ok()?
        }
        (Lang::Erlang, "call") => count(call.child_by_field_name("args")?),
        // `&f/2`, `&Cart.add/2`.
        (Lang::Elixir, "unary_operator") => {
            let operand = call.child_by_field_name("operand")?;
            node_text(operand.child_by_field_name("right")?, src)
                .trim()
                .parse()
                .ok()?
        }
        (Lang::Elixir, "call") => {
            // `&Cart.add/2`: the capture's arity, not the name's none.
            if let Some(p) = parent
                && p.kind() == "binary_operator"
                && p.child_by_field_name("left")
                    .is_some_and(|l| l.id() == call.id())
                && p.child_by_field_name("operator")
                    .is_some_and(|o| node_text(o, src) == "/")
                && p.parent().is_some_and(|g| g.kind() == "unary_operator")
            {
                return node_text(p.child_by_field_name("right")?, src)
                    .trim()
                    .parse()
                    .ok();
            }
            let mut cursor = call.walk();
            let mut n = 0;
            for child in call.named_children(&mut cursor) {
                match child.kind() {
                    "arguments" => n += count(child),
                    // `with_tx do … end`: the block is its last argument.
                    "do_block" => n += 1,
                    _ => {}
                }
            }
            // `x |> f(a)`: `x` is the first.
            let piped = parent.is_some_and(|p| {
                p.kind() == "binary_operator"
                    && p.child_by_field_name("right")
                        .is_some_and(|r| r.id() == call.id())
                    && p.child_by_field_name("operator")
                        .is_some_and(|o| node_text(o, src) == "|>")
            });
            n + usize::from(piped)
        }
        _ => return None,
    };
    u8::try_from(n).ok()
}

/// Whether the bare call `call` of `callee` calls a parameter of a function
/// or closure around it — in a language whose functions are values, called
/// by name like a function.
fn calls_a_parameter(
    call: Node,
    callee: &str,
    src: &str,
    lang: Lang,
    parameters: &mut HashMap<usize, Vec<String>>,
) -> bool {
    if !matches!(
        lang,
        Lang::Dart
            | Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Python
            | Lang::Kotlin
            | Lang::Scala
            | Lang::Go
            | Lang::C
            | Lang::Cpp
            | Lang::CSharp
            | Lang::Zig
    ) {
        return false;
    }
    let mut at = call.parent();
    while let Some(node) = at {
        let names = parameters
            .entry(node.id())
            .or_insert_with(|| parameter_names(node, src, lang));
        if names.iter().any(|n| n == callee) {
            return true;
        }
        at = node.parent();
    }
    false
}

/// The parameter-list kinds of the grammars [`calls_a_parameter`] reads.
const PARAMETER_LISTS: &[&str] = &[
    "formal_parameter_list",
    "formal_parameters",
    "parameters",
    "parameter_list",
    "function_value_parameters",
    "lambda_parameters",
    "bindings",
    "implicit_parameter",
];

/// The names of the parameters `node` declares, if it is a function or a
/// closure: its own parameter list's, or its signature's or declarator's
/// (Dart's `function_signature`, C's `function_declarator`), or an arrow
/// function's lone parameter (`x => …`).
fn parameter_names(node: Node, src: &str, lang: Lang) -> Vec<String> {
    let mut lists: Vec<Node> = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if PARAMETER_LISTS.contains(&kind) {
            lists.push(child);
        } else if kind.ends_with("signature") || kind.ends_with("declarator") {
            let mut inner = child.walk();
            lists.extend(
                child
                    .named_children(&mut inner)
                    .filter(|c| PARAMETER_LISTS.contains(&c.kind())),
            );
        }
    }
    let mut names = Vec::new();
    if node.kind() == "arrow_function"
        && let Some(lone) = node.child_by_field_name("parameter")
    {
        names.push(node_text(lone, src).to_string());
    }
    for list in lists {
        let text = node_text(list, src);
        let text = text
            .strip_prefix('(')
            .and_then(|t| t.strip_suffix(')'))
            .unwrap_or(text);
        names.extend(split_top_level(text).filter_map(|p| parameter_name(p, lang)));
    }
    names
}

/// `text` split at its commas outside brackets.
fn split_top_level(text: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0i32;
    let mut start = 0;
    let mut parts = Vec::new();
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            ',' if depth <= 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts.into_iter()
}

/// The name one parameter declares: before its type in a language that
/// writes the name first (`cb: () -> Unit`, `cb func()`, `*args`), after
/// it in one that writes the type first (`Emitter<String> emit`, `Action
/// cb`, `void (*cb)(int)`).
fn parameter_name(param: &str, lang: Lang) -> Option<String> {
    let is_name = |c: char| c == '_' || c == '$' || c.is_alphanumeric();
    // Its default is no part of it.
    let mut depth = 0i32;
    let mut end = param.len();
    for (i, c) in param.char_indices() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            '=' if depth <= 0 && !param[i + 1..].starts_with(['=', '>']) => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let param = param[..end]
        .trim()
        .trim_start_matches(['{', '['])
        .trim_end_matches(['}', ']']);
    let name_first = matches!(
        lang,
        Lang::Python
            | Lang::Kotlin
            | Lang::Scala
            | Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Go
            | Lang::Zig
    );
    let name = if name_first {
        let head = param.split(':').next().unwrap_or(param);
        head.split_whitespace()
            .map(|w| w.trim_start_matches(['*', '.', '&']))
            .find(|w| {
                !w.is_empty()
                    && !matches!(
                        *w,
                        "val"
                            | "var"
                            | "vararg"
                            | "noinline"
                            | "crossinline"
                            | "comptime"
                            | "noalias"
                            | "readonly"
                            | "private"
                            | "public"
                            | "protected"
                            | "override"
                            | "implicit"
                            | "using"
                            | "lazy"
                    )
            })?
            .trim_end_matches('?')
    } else if let Some(at) = param.find("(*") {
        // A C function pointer: `void (*cb)(int)`.
        let rest = &param[at + 2..];
        &rest[..rest.find(|c: char| !is_name(c)).unwrap_or(rest.len())]
    } else {
        let end = param.trim_end_matches(|c: char| !is_name(c));
        let start = end
            .char_indices()
            .rev()
            .find(|&(_, c)| !is_name(c))
            .map_or(0, |(i, c)| i + c.len_utf8());
        &end[start..]
    };
    (!name.is_empty() && name.starts_with(|c: char| !c.is_ascii_digit())).then(|| name.to_string())
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
///
/// `parent` is the node's parent and `before` the sibling before it, both
/// carried by the walk.
fn fn_names(
    node: Node,
    parent: Option<Node>,
    before: Option<Node>,
    src: &str,
    lang: Lang,
) -> Vec<String> {
    match lang {
        // `function_definition` names its function inside the declarator.
        Lang::C | Lang::Cpp => return c_function_name(node, src).into_iter().collect(),
        // Dart: a `function_body` is a bare sibling after its signature, so
        // its name comes from the preceding signature (a lambda/arrow body's
        // signature has no name, so those calls fall through to the enclosing
        // named function).
        // `def name(…) do`: the name is the call its first argument is.
        Lang::Elixir => return elixir_def_name(node, src).into_iter().collect(),
        Lang::Haskell => return haskell_fn_name(node, src).into_iter().collect(),
        Lang::Dart if node.kind() == "function_body" => {
            // The sibling the walk carried: `prev_sibling` finds it through
            // `parent()`, a walk down from the root, and a scan of the
            // parent's children cost their number per body.
            return before
                .and_then(|sig| dart_sig_name(sig, src))
                .into_iter()
                .collect();
        }
        _ => {}
    }
    let own = node
        .child_by_field_name("name")
        .map(|n| node_text(n, src).to_string());
    binding_name(parent, src).into_iter().chain(own).collect()
}

/// A Haskell function's name: `add` of `add a b = …`, the operator of
/// `(<+>) a b = …` and of `a <+> b = …`, the function of ``x `plus` y = …``
/// — as the outline names them, the operator without its parentheses.
fn haskell_fn_name(node: Node, src: &str) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        let name = match name.kind() {
            "prefix_id" => name.named_child(0)?,
            _ => name,
        };
        return Some(node_text(name, src).to_string());
    }
    let head = node.named_child(0).filter(|h| h.kind() == "infix")?;
    haskell_infix_name(head, src).map(|(name, _)| name)
}

/// What an infix application `a <+> b` (or ``x `plus` y``) applies: the
/// operator, or the function between backticks, and whether it is another
/// module's (``x `M.plus` y``). A constructor (`x : xs`, `a :+: b`) applies
/// no function.
fn haskell_infix_name(infix: Node, src: &str) -> Option<(String, bool)> {
    let op = infix.child_by_field_name("operator")?;
    match op.kind() {
        "operator" => Some((node_text(op, src).to_string(), false)),
        "infix_id" => {
            let inner = op.named_child(0)?;
            match inner.kind() {
                "variable" => Some((node_text(inner, src).to_string(), false)),
                "qualified" => {
                    let id = inner.child_by_field_name("id")?;
                    (id.kind() == "variable").then(|| (node_text(id, src).to_string(), true))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The base library's operators, which no project defines: an infix use of
/// one is no call of the project's, and is not kept as one.
const HASKELL_BASE_OPERATORS: &[&str] = &[
    "+", "-", "*", "/", "^", "^^", "**", "==", "/=", "<", "<=", ">", ">=", "&&", "||", "$", "$!",
    ".", "<$>", "<$", "$>", "<&>", "<*>", "<*", "*>", ">>=", ">>", "=<<", ">=>", "<=<", "++", "!!",
    "<>", "<|>", "&", "@", "~", "|", "=", "->", "<-", "=>", "::", "\\",
];

/// The function an Elixir `def` (or `defp`, `defmacro`, …) call defines:
/// its first argument's name — `f(x)`, `f(x) when …` or a bare `f`.
fn elixir_def_name(call: Node, src: &str) -> Option<String> {
    let target = call.child_by_field_name("target")?;
    if !matches!(
        node_text(target, src),
        "def" | "defp" | "defmacro" | "defmacrop" | "defguard" | "defguardp" | "defn" | "defnp"
    ) {
        return None;
    }
    let args = call
        .named_children(&mut call.walk())
        .find(|c| c.kind() == "arguments")?;
    let mut head = args.named_child(0)?;
    if head.kind() == "binary_operator" {
        head = head.child_by_field_name("left")?;
    }
    let name = match head.kind() {
        "call" => head.child_by_field_name("target")?,
        "identifier" => head,
        _ => return None,
    };
    (name.kind() == "identifier").then(|| node_text(name, src).to_string())
}

/// The Haskell operators whose operands are functions applied or composed:
/// a function named as one is used there (`show . add 1`, `f <$> xs`).
const HASKELL_FUNCTION_OPERATORS: &[&str] = &[
    "$", "&", ".", "<$>", "<$", "$>", "<&>", "<*>", "<*", "*>", ">>=", "=<<", ">=>", "<=<",
];

/// Whether an Elixir `call` (whose parent is `parent`) is the head of a
/// `def` — `add(a, b)` in `def add(a, b) do`, guarded (`when …`) or not:
/// the function it defines, which it does not call. Read as a call, every
/// function called itself.
fn elixir_def_head(call: Node, parent: Option<Node>, src: &str) -> bool {
    let Some(mut up) = parent else {
        return false;
    };
    if up.kind() == "binary_operator" {
        if up.child_by_field_name("left") != Some(call) {
            return false;
        }
        let Some(next) = up.parent() else {
            return false;
        };
        up = next;
    }
    up.kind() == "arguments"
        && up
            .named_child(0)
            .is_some_and(|first| first == call || Some(first) == parent)
        && up
            .parent()
            .is_some_and(|def| elixir_def_name(def, src).is_some())
}

/// The name of the binding a function expression is assigned to, if it is
/// assigned to one (see [`fn_names`]); `parent` is the expression's parent.
fn binding_name(parent: Option<Node>, src: &str) -> Option<String> {
    let parent = parent?;
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
                // `~Foo` reads as `Foo` to `last_identifier`, which attributed
                // a destructor's calls to the constructor. The outline lists
                // no destructor, so there is no node for them.
                if name.kind() == "destructor_name" {
                    return None;
                }
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

/// The type a Rust path call names its function through: `Self::make()`
/// the impl's own, `Builder::new()` and `Vec::<u8>::new()` the type's. A
/// module's path (`fs::read`, `crate::x::f`) names none: its function is
/// found by the imports.
fn rust_path_receiver(call: Node, src: &str) -> Option<Receiver> {
    let mut function = call.child_by_field_name("function")?;
    if function.kind() == "generic_function" {
        function = function.child_by_field_name("function")?;
    }
    if function.kind() != "scoped_identifier" {
        return None;
    }
    let path = function.child_by_field_name("path")?;
    let path = match path.kind() {
        "generic_type" => path.child_by_field_name("type")?,
        _ => path,
    };
    let last = match path.kind() {
        "scoped_identifier" | "scoped_type_identifier" => path.child_by_field_name("name")?,
        _ => path,
    };
    let name = node_text(last, src);
    match name {
        "Self" => Some(Receiver::Own),
        _ if name.starts_with(|c: char| c.is_uppercase()) => Some(Receiver::Type(name.to_string())),
        _ => None,
    }
}

/// Descend through wrappers that end in a name — a turbofish or template
/// argument list, a `ns::` qualification — to the name node itself, so
/// `ns::f<int>` yields `f`, not `int`.
fn innermost_name(mut node: Node) -> Node {
    loop {
        let inner = match node.kind() {
            "generic_function" => node.child_by_field_name("function"),
            "template_function"
            | "template_method"
            | "template_type"
            | "qualified_identifier"
            | "scoped_identifier" => node.child_by_field_name("name"),
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
/// that cannot name a project function. `parent` is the call's parent,
/// carried by the walk: in Erlang and Ruby it says what the call is.
fn callee_name(call: Node, parent: Option<Node>, src: &str, lang: Lang) -> Option<(String, bool)> {
    match (lang, call.kind()) {
        // PHP calls a function by name, or by a qualified one
        // (`\App\helper()`). A name held in a variable (`$f()`, `$o->$m()`,
        // `new $class`) is none the reader can follow.
        (Lang::Php, "function_call_expression") => {
            let function = call.child_by_field_name("function")?;
            return Some((php_name(function, src)?, false));
        }
        // `$o->m()`, `$o?->m()`, and `Foo::m()` / `self::m()` /
        // `parent::m()`, whose receiver is a class: a method each, like
        // Java's `Foo.m()`.
        (
            Lang::Php,
            "member_call_expression" | "nullsafe_member_call_expression" | "scoped_call_expression",
        ) => {
            let name = call.child_by_field_name("name")?;
            return Some((php_name(name, src)?, true));
        }
        // `new Foo(…)` names the class. `new self`, `new static` and
        // `new parent` name the class around the call, which is no name of
        // their own.
        (Lang::Php, "object_creation_expression") => {
            let class = php_name(call.named_child(0)?, src)?;
            if matches!(class.as_str(), "self" | "static" | "parent") {
                return None;
            }
            return Some((class, false));
        }
        // A Ruby call carries its method name in a field, and a receiver
        // when it is not on `self` implicitly. The name is read whole:
        // `valid?` and `save!` are names of their own, not `valid` and
        // `save`. Assigning through a call (`self.total = 0`) calls the
        // setter, which is defined as `total=`.
        (Lang::Ruby, "call") => {
            let mut name = node_text(call.child_by_field_name("method")?, src).to_string();
            let method = call.child_by_field_name("receiver").is_some();
            let assigned = parent.is_some_and(|p| {
                p.kind() == "assignment"
                    && p.child_by_field_name("left")
                        .is_some_and(|left| left.id() == call.id())
            });
            if assigned {
                name.push('=');
            }
            return Some((name, method));
        }
        // C#: `F()`, `F<T>()`, `x.F()`, `x?.F()`. `nameof(x)` is an
        // operator, though it is written as a call.
        (Lang::CSharp, "invocation_expression") => {
            let function = call.child_by_field_name("function")?;
            let (name, method) = match function.kind() {
                "identifier" | "generic_name" => (function, false),
                "member_access_expression" | "member_binding_expression" => {
                    (function.child_by_field_name("name")?, true)
                }
                // `x?.F()`: the member is bound after the condition.
                "conditional_access_expression" => {
                    let mut cursor = function.walk();
                    let bound = function
                        .named_children(&mut cursor)
                        .filter(|n| n.kind() == "member_binding_expression")
                        .last()?;
                    (bound.child_by_field_name("name")?, true)
                }
                _ => return None,
            };
            let name = csharp_name(name, src)?;
            if !method && name == "nameof" {
                return None;
            }
            return Some((name, method));
        }
        // `new Foo(…)`, `new Ns.Foo<T>(…)`: the constructor is the type's
        // base name, as the outline names it. A target-typed `new()` is an
        // `implicit_object_creation_expression`, naming no type.
        (Lang::CSharp, "object_creation_expression") => {
            let ty = call.child_by_field_name("type")?;
            return Some((csharp_name(ty, src)?, false));
        }
        // Erlang's `f(…)` is a local call. In `m:f(…)` the call is the `fun`
        // of a `remote`: another module's function, so method-like. A fun
        // held in a variable (`F(x)`) or written in place names nothing.
        // The atom is taken as written, as the outline takes it: a quoted
        // `'f'` or a `node@host` is no identifier run.
        // `fun add/2`, `fun cart:add/2`: the function named, to be called.
        (Lang::Erlang, "internal_fun" | "external_fun") => {
            let function = call.child_by_field_name("fun")?;
            return (function.kind() == "atom").then(|| {
                (
                    node_text(function, src).to_string(),
                    call.kind() == "external_fun",
                )
            });
        }
        // `&add/2`: the function captured (`&Cart.add/2` is a call node).
        (Lang::Elixir, "unary_operator") => {
            let capture = call.child(0).is_some_and(|op| node_text(op, src) == "&");
            let operand = call.child_by_field_name("operand")?;
            let left = operand.child_by_field_name("left")?;
            return (capture && operand.kind() == "binary_operator" && left.kind() == "identifier")
                .then(|| (node_text(left, src).to_string(), false));
        }
        // A function passed as a value: an application's argument
        // (`foldr add 0`), or the operand of an operator that applies or
        // composes it (`show . add 1`, `f <$> xs`, `print $ x`). As an
        // application's function it is that application's callee.
        // An operator applied infix (`a <+> b`), a function written between
        // backticks (``x `plus` y``): calls of them — the project's own, as
        // the base library's (`+`, `$`, `<$>`) name none of its functions. A
        // definition's head (`a <+> b = …`) defines what it names.
        (Lang::Haskell, "infix") => {
            if parent.is_some_and(|p| p.kind() == "function") {
                return None;
            }
            let (name, other_module) = haskell_infix_name(call, src)?;
            return (!HASKELL_BASE_OPERATORS.contains(&name.as_str()))
                .then_some((name, other_module));
        }
        // An operator passed as a value (`foldr (<+>) 0 xs`): used there,
        // as a function is. As an application's function it is that
        // application's callee.
        (Lang::Haskell, "prefix_id") => {
            let parent = parent?;
            let passed = match parent.kind() {
                "apply" => parent.child_by_field_name("argument") == Some(call),
                "infix" => parent
                    .child_by_field_name("operator")
                    .is_some_and(|op| HASKELL_FUNCTION_OPERATORS.contains(&node_text(op, src))),
                _ => false,
            };
            let op = call.named_child(0).filter(|op| op.kind() == "operator")?;
            let name = node_text(op, src);
            return (passed && !HASKELL_BASE_OPERATORS.contains(&name))
                .then(|| (name.to_string(), false));
        }
        (Lang::Haskell, "variable" | "qualified") => {
            let parent = parent?;
            let passed = match parent.kind() {
                "apply" => parent.child_by_field_name("argument") == Some(call),
                "infix" => parent
                    .child_by_field_name("operator")
                    .is_some_and(|op| HASKELL_FUNCTION_OPERATORS.contains(&node_text(op, src))),
                _ => false,
            };
            if !passed {
                return None;
            }
            return match call.kind() {
                "variable" => Some((node_text(call, src).to_string(), false)),
                _ => {
                    let id = call.child_by_field_name("id")?;
                    (id.kind() == "variable").then(|| (node_text(id, src).to_string(), true))
                }
            };
        }
        (Lang::Erlang, "call") => {
            let function = call.child_by_field_name("expr")?;
            if function.kind() != "atom" {
                return None;
            }
            let remote = parent.is_some_and(|p| p.kind() == "remote");
            return Some((node_text(function, src).to_string(), remote));
        }
        // Haskell's `f x y` is `apply (apply f x) y`: only the innermost
        // application names the function, the outer ones are the same call
        // taking its further arguments. A data constructor (`Just x`) is no
        // call, and a type's `IO ()` is an `apply` of a `constructor`. A
        // qualified `M.f` is another module's, like Python's `mod.f`.
        (Lang::Haskell, "apply") => {
            let function = call.child_by_field_name("function")?;
            return match function.kind() {
                "variable" => Some((node_text(function, src).to_string(), false)),
                // `(<+>) 1 2`.
                "prefix_id" => {
                    let op = function
                        .named_child(0)
                        .filter(|op| op.kind() == "operator")?;
                    let name = node_text(op, src);
                    (!HASKELL_BASE_OPERATORS.contains(&name)).then(|| (name.to_string(), false))
                }
                "qualified" => {
                    let id = function.child_by_field_name("id")?;
                    (id.kind() == "variable").then(|| (node_text(id, src).to_string(), true))
                }
                _ => None,
            };
        }
        // Zig: `f(…)`, or `x.f(…)` through a field — a method, or a
        // namespace's function (`std.debug.print`). A builtin (`@import(…)`)
        // is a `builtin_function`, never a call expression.
        (Lang::Zig, "call_expression") => {
            let function = call.child_by_field_name("function")?;
            return match function.kind() {
                "identifier" => Some((node_text(function, src).to_string(), false)),
                "field_expression" => {
                    let member = function.child_by_field_name("member")?;
                    Some((node_text(member, src).to_string(), true))
                }
                _ => None,
            };
        }
        // Kotlin: the callee is the call's first child — a name, or a
        // navigation whose last part is the method (`repo.findAll()`).
        (Lang::Kotlin, "call_expression") => {
            let callee = call.named_child(0)?;
            return match callee.kind() {
                "identifier" => Some((node_text(callee, src).to_string(), false)),
                "navigation_expression" => {
                    let last =
                        callee.named_child(callee.named_child_count().checked_sub(1)? as u32)?;
                    (last.kind() == "identifier").then(|| (node_text(last, src).to_string(), true))
                }
                _ => None,
            };
        }
        // `new Foo[T](…)`: the class it makes.
        // Scala calls a method with no parameters without parentheses
        // (`ref.get`, `io.void`): a member read that no call's parentheses
        // follow, nor an assignment's left side. A field read so links to
        // nothing, as no method has its name.
        (Lang::Scala, "field_expression") => {
            let called = parent.is_some_and(|p| {
                (p.kind() == "call_expression"
                    && p.child_by_field_name("function")
                        .is_some_and(|f| f.id() == call.id()))
                    || (p.kind() == "assignment_expression"
                        && p.child_by_field_name("left")
                            .is_some_and(|l| l.id() == call.id()))
                    || p.kind() == "generic_function"
            });
            if called {
                return None;
            }
            let field = call.child_by_field_name("field")?;
            let name = node_text(field, src);
            return (name.starts_with(char::is_lowercase) && field.kind() == "identifier")
                .then(|| (name.to_string(), true));
        }
        (Lang::Scala, "instance_expression") => {
            let mut cursor = call.walk();
            let ty = call
                .named_children(&mut cursor)
                .find(|n| matches!(n.kind(), "type_identifier" | "generic_type"))?;
            let ty = match ty.kind() {
                "generic_type" => ty.named_child(0)?,
                _ => ty,
            };
            return Some((last_identifier(node_text(ty, src))?, false));
        }
        // Elixir: `f(…)`, or another module's `Mod.f(…)` (like Erlang's
        // `mod:f(…)`). A `def` defines rather than calls (`fn_names`).
        (Lang::Elixir, "call") => {
            if elixir_def_name(call, src).is_some() || elixir_def_head(call, parent, src) {
                return None;
            }
            let target = call.child_by_field_name("target")?;
            return match target.kind() {
                "identifier" => Some((node_text(target, src).to_string(), false)),
                "dot" => {
                    let right = target.child_by_field_name("right")?;
                    // `user.name` with no parentheses reads a map's or a
                    // struct's field; only a module's function is called
                    // so (`Cart.total`).
                    let field = target
                        .child_by_field_name("left")
                        .is_some_and(|left| left.kind() != "alias")
                        && !call
                            .named_children(&mut call.walk())
                            .any(|c| c.kind() == "arguments");
                    (right.kind() == "identifier" && !field)
                        .then(|| (node_text(right, src).to_string(), true))
                }
                _ => None,
            };
        }
        // Java names the method in a field of its own; `obj.f()` carries the
        // receiver as `object`.
        (Lang::Java, "method_invocation") => {
            let name = call.child_by_field_name("name")?;
            let method = call.child_by_field_name("object").is_some();
            return Some((node_text(name, src).to_string(), method));
        }
        // `<Button …>` renders the component `Button`; `<Select.Item>` the
        // `Item` of `Select`, as a method is called on its object. A
        // lower-case element (`<div>`) is the page's own, no project
        // code.
        (
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx,
            "jsx_opening_element" | "jsx_self_closing_element",
        ) => {
            let name = call.child_by_field_name("name")?;
            return match name.kind() {
                "identifier" => {
                    let text = node_text(name, src);
                    text.starts_with(|c: char| c.is_ascii_uppercase())
                        .then(|| (text.to_string(), false))
                }
                "member_expression" => {
                    let property = name.child_by_field_name("property")?;
                    Some((node_text(property, src).to_string(), true))
                }
                _ => None,
            };
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
    // `f(a)(b)`, Scala's `test("adds") { … }`: the outer call calls what
    // the inner one returned, a function of no name — the inner one is the
    // call of `f`. Read by its text, the outer one called the title (`adds`).
    if matches!(
        target.kind(),
        "call_expression" | "call" | "method_invocation"
    ) {
        return None;
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
    // Dart spells `obj.foo()` / `obj?.foo()` as a method invocation whose
    // function is an (un)conditional assignable selector.
    let method = matches!(
        target.kind(),
        "field_expression"
            | "attribute"
            | "member_expression"
            | "selector_expression"
            | "unconditional_assignable_selector"
            | "conditional_assignable_selector"
    );
    let name_node = match target.kind() {
        "field_expression" | "selector_expression" => target.child_by_field_name("field"),
        "member_expression" => target.child_by_field_name("property"),
        "attribute" => target.child_by_field_name("attribute"),
        _ => None,
    }
    .unwrap_or(target);
    let name_node = innermost_name(name_node);
    // Rust's raw identifier prefix is part of the spelling the outline uses.
    // Splitting `r#type` into identifier runs would discard `r#` and prevent
    // the call from matching its own definition, including qualified calls.
    let name =
        if lang == Lang::Rust && matches!(name_node.kind(), "identifier" | "field_identifier") {
            node_text(name_node, src).to_string()
        } else {
            last_identifier(node_text(name_node, src))?
        };
    Some((name, method))
}

/// A PHP name as written (`helper`), or the last part of a qualified one
/// (`App\Util\helper`); `None` for a variable or an expression.
fn php_name(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "name" => Some(node_text(node, src).to_string()),
        "qualified_name" => last_identifier(node_text(node, src)),
        _ => None,
    }
}

/// The identifier a C# name ends in: `F` of `F`, `F<T>`, `Ns.F` and
/// `global::F`; `None` for anything else (a predefined type, an array).
fn csharp_name(mut node: Node, src: &str) -> Option<String> {
    loop {
        node = match node.kind() {
            "identifier" => return Some(node_text(node, src).to_string()),
            "generic_name" => node.named_child(0)?,
            "qualified_name" | "alias_qualified_name" => node.child_by_field_name("name")?,
            _ => return None,
        };
    }
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

/// What the receiver of the method call `call` (calling `callee`) is known
/// to be ([`CallSite::receiver`]): looked for in the source of the calling
/// function before the call, from `from`. Only a type it is known to be
/// exactly: a type it is declared as may hold a subclass, whose override is
/// the method called — so `self`, `this` and a declared or annotated
/// receiver say nothing in a language with inheritance (Go and Rust have
/// none, and a Rust trait's own methods have no owner to narrow to).
#[allow(clippy::too_many_arguments)]
fn receiver_of(
    call: Node,
    callee: &str,
    scope: &Scope,
    src: &str,
    lang: Lang,
    bindings: &mut HashMap<(usize, String), Bindings>,
    fields: &mut HashMap<(usize, String), Option<String>>,
    unbound: &mut Option<String>,
) -> Option<Receiver> {
    let receiver = receiver_text(node_text(call, src), callee)?.trim();
    if matches!(
        receiver,
        "self" | "this" | "$this" | "Self" | "cls" | "static"
    ) {
        // The caller's class — its method, inherited or overridden, in a
        // language with inheritance (Go names its receiver).
        return (lang == Lang::Rust || (has_inheritance(lang) && lang != Lang::Go))
            .then_some(Receiver::Own);
    }
    // A field of the caller's class, as its class declares it:
    // `this.owners.findById(…)` with `private final OwnerRepository owners;`.
    if has_inheritance(lang)
        && lang != Lang::Go
        && let Some(field) = ["this.", "self.", "$this->"]
            .iter()
            .find_map(|p| receiver.strip_prefix(p))
    {
        return is_plain_name(field)
            .then(|| class_field_type(call, field, src, lang, scope.places, fields))
            .flatten()
            .map(Receiver::Declared);
    }
    // A field of the type's own: `self.items.len()` is `Vec`'s `len`, as
    // its struct declares `items`.
    if lang == Lang::Rust
        && let Some(field) = receiver.strip_prefix("self.")
    {
        return is_plain_name(field)
            .then(|| rust_field_type(call, field, src))
            .flatten()
            .map(Receiver::Type);
    }
    if !is_plain_name(receiver) {
        // A chain on a value just built: `IO.pure(1).flatMap(…)`,
        // `new Cart().add(…)`, `Cart(items).total()`.
        if has_inheritance(lang) && lang != Lang::Go {
            if let Some(ty) = constructed_type(receiver, lang) {
                return Some(Receiver::Type(ty));
            }
            if let Some(ty) = factory_type(receiver, lang) {
                return Some(Receiver::Declared(ty));
            }
        }
        return None;
    }
    let places = scope.places;
    let mut said = |from: usize, to: usize| {
        bindings
            .entry((from, receiver.to_string()))
            .or_insert_with(|| Bindings::from(from, places))
            .said_before(to, receiver, &Scope { places, from }, src, lang)
    };
    let (mut from, mut now) = (scope.from, said(scope.from, call.start_byte()));
    // A name the function the call is in does not bind is the function's
    // around it, if it is nested — a closure's captured `store` in
    // `useEffect(() => store.load())` — as that one says it is bound before
    // it, and still is at its end: a binding after it may be what the
    // nested function sees when it runs. Rust's nested `fn`s capture
    // nothing (its closures are no function here).
    while now == Said::Nothing && lang != Lang::Rust {
        let Some((outer, outer_end)) = places.around(from) else {
            break;
        };
        let at_end = said(outer, outer_end);
        let before = said(outer, from);
        if before != at_end {
            return None;
        }
        (from, now) = (outer, before);
    }
    match now {
        Said::Type(ty, exact) => {
            // Go, Rust and Zig have no subtypes: a declared type is exact.
            if exact || matches!(lang, Lang::Go | Lang::Rust | Lang::Zig) {
                return Some(Receiver::Type(ty));
            }
            // Declared as the type: it or a subtype.
            return has_inheritance(lang).then_some(Receiver::Declared(ty));
        }
        Said::Unknown => return None,
        Said::Nothing => {}
    }
    // Not bound in the function: a field of its class, used bare
    // (`owners.findById(…)` in Java, Kotlin, C#, Dart, C++).
    if implicit_self(lang)
        && !receiver.starts_with(|c: char| c.is_uppercase())
        && let Some(ty) = class_field_type(call, receiver, src, lang, scope.places, fields)
    {
        return Some(Receiver::Declared(ty));
    }
    // Nor a field: in Scala, a package or an object (`os` in `os.read(p)`),
    // or a member inherited from a trait.
    if lang == Lang::Scala && !receiver.starts_with(|c: char| c.is_uppercase()) {
        *unbound = Some(receiver.to_string());
    }
    // A JavaScript runtime's own object, not bound here (`console`,
    // `document`, `localStorage`): its methods are the runtime's.
    if matches!(lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx)
        && JS_GLOBALS.contains(&receiver)
    {
        return Some(Receiver::Type(receiver.to_string()));
    }
    // A type as the receiver: its own (static) method. Not in Go, where an
    // exported name is as likely a variable.
    (lang != Lang::Go && receiver.starts_with(|c: char| c.is_uppercase()))
        .then(|| Receiver::Type(receiver.to_string()))
}

/// The type the class around `call` declares its field `field` as —
/// `private final OwnerRepository owners;`, `private val owners:
/// OwnerRepository` (a Kotlin or Scala constructor's property too),
/// `owners: OwnerRepository;`, TypeScript's `constructor(private repo:
/// Repo)` — read once per class and field.
fn class_field_type(
    call: Node,
    field: &str,
    src: &str,
    lang: Lang,
    places: &Places,
    fields: &mut HashMap<(usize, String), Option<String>>,
) -> Option<String> {
    let mut class = call.parent();
    while let Some(n) = class {
        if CLASS_KINDS.contains(&n.kind()) {
            break;
        }
        class = n.parent();
    }
    let class = class?;
    let key = (class.start_byte(), field.to_string());
    if let Some(known) = fields.get(&key) {
        return known.clone();
    }
    let (start, end) = (class.start_byte(), class.end_byte());
    let text = src.get(start..end)?;
    let word = |c: char| c == '_' || c.is_alphanumeric();
    let mut found = None;
    let mut at = 0;
    while let Some(i) = text[at..].find(field) {
        let begin = at + i;
        let stop = begin + field.len();
        at = stop;
        let head = &text[..begin];
        if head.ends_with(|c: char| word(c) || matches!(c, '.' | '$' | '@' | '>'))
            || text[stop..].starts_with(word)
            || places.opaque(start + begin)
        {
            continue;
        }
        // A method's own name is no field; TypeScript's constructor
        // parameter is one when an access word makes it a property.
        if places.in_fn(start + begin).is_some() {
            let property = ["private", "public", "protected", "readonly"]
                .iter()
                .any(|w| head.trim_end().ends_with(w));
            if !property {
                continue;
            }
        }
        if let Binding::Typed(ty, _) = binding_at(head, &text[stop..], lang) {
            found = Some(ty);
            break;
        }
    }
    fields.insert(key, found.clone());
    found
}

/// The objects a JavaScript runtime gives every script — the language's,
/// the browser's, Node's and Bun's: `console.log`, `JSON.parse`,
/// `document.querySelector` are theirs, never a project's `log`, `parse`
/// or `querySelector`.
const JS_GLOBALS: &[&str] = &[
    "console",
    "Math",
    "JSON",
    "Object",
    "Array",
    "Promise",
    "Number",
    "String",
    "Boolean",
    "Date",
    "Reflect",
    "Intl",
    "Symbol",
    "Atomics",
    "BigInt",
    "Error",
    "RegExp",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "ArrayBuffer",
    "Proxy",
    "globalThis",
    "window",
    "document",
    "navigator",
    "location",
    "history",
    "localStorage",
    "sessionStorage",
    "indexedDB",
    "crypto",
    "performance",
    "screen",
    "customElements",
    "process",
    "Buffer",
    "require",
    "module",
    "exports",
    "Bun",
    "Deno",
    "URL",
    "URLSearchParams",
    "Response",
    "Request",
    "Headers",
    "Notification",
    "Worker",
    "self",
];

/// The receiver expression of a call written `receiver<sep>name(…)`: what
/// precedes the first `.name`, `->name`, `?.name` or `::name` standing as a
/// whole name in `call`'s text. `None` for an empty name (a call tree-sitter
/// recovered from half-typed code), which every position would match.
fn receiver_text<'a>(call: &'a str, name: &str) -> Option<&'a str> {
    if name.is_empty() {
        return None;
    }
    let word = |c: char| c == '_' || c.is_alphanumeric();
    let mut at = 0;
    while let Some(found) = call[at..].find(name) {
        let start = at + found;
        let end = start + name.len();
        let whole = !call[end..].starts_with(word);
        let head = &call[..start];
        let sep = ["?->", "->", "?.", "::", "."]
            .into_iter()
            .find(|sep| head.ends_with(sep));
        if whole && let Some(sep) = sep {
            return Some(&head[..head.len() - sep.len()]);
        }
        at = end;
    }
    None
}

/// A name standing alone — `cart`, `$cart`, `@cart` — not an expression.
fn is_plain_name(text: &str) -> bool {
    let text = text.strip_prefix(['$', '@']).unwrap_or(text);
    text.starts_with(|c: char| c == '_' || c.is_alphabetic())
        && text.chars().all(|c| c == '_' || c.is_alphanumeric())
}

/// The function a call is made in: where it starts, and the file's
/// [`Places`], which tell the functions nested in it.
struct Scope<'p> {
    places: &'p Places,
    from: usize,
}

/// Where a file's functions are, nested ones too, and the text no name is
/// bound in — comments and strings — by byte range, read once per file:
/// telling an occurrence's place by walking the tree down to it and back
/// up, for every occurrence, made a long function's reading quadratic.
struct Places {
    /// Each function's start and end, by start, with the index of the one
    /// around it.
    fns: Vec<(usize, usize, Option<usize>)>,
    /// Comments and strings, none inside another, by start.
    opaque: Vec<(usize, usize)>,
}

impl Places {
    fn read(root: Node, fn_kinds: &[&str]) -> Places {
        let mut fns = Vec::new();
        let mut opaque = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let kind = node.kind();
            if kind.contains("comment") || kind.contains("string") {
                opaque.push((node.start_byte(), node.end_byte()));
                continue;
            }
            if fn_kinds.contains(&kind) {
                fns.push((node.start_byte(), node.end_byte()));
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        fns.sort_unstable();
        opaque.sort_unstable();
        // Each one's parent: the nearest before it still open at its start.
        let mut open: Vec<usize> = Vec::new();
        let fns = fns
            .iter()
            .enumerate()
            .map(|(i, &(start, end))| {
                while open.last().is_some_and(|&o| fns[o].1 <= start) {
                    open.pop();
                }
                let parent = open.last().copied();
                open.push(i);
                (start, end, parent)
            })
            .collect();
        Places { fns, opaque }
    }

    /// Whether `at` is in a comment or a string.
    fn opaque(&self, at: usize) -> bool {
        let i = self.opaque.partition_point(|&(start, _)| start <= at);
        i > 0 && self.opaque[i - 1].1 > at
    }

    /// The innermost function `at` is in: where it starts and ends.
    fn in_fn(&self, at: usize) -> Option<(usize, usize)> {
        let mut i = self
            .fns
            .partition_point(|&(start, _, _)| start <= at)
            .checked_sub(1)?;
        loop {
            let (start, end, parent) = self.fns[i];
            if start <= at && at < end {
                return Some((start, end));
            }
            i = parent?;
        }
    }

    /// The function around the one that starts at `from`: where it starts
    /// and ends.
    fn around(&self, from: usize) -> Option<(usize, usize)> {
        let i = self.fns.partition_point(|&(start, _, _)| start < from);
        let (start, _, parent) = *self.fns.get(i)?;
        if start != from {
            return None;
        }
        parent.map(|p| (self.fns[p].0, self.fns[p].1))
    }
}

/// One name's bindings in one function, read once as far as its calls
/// reach: what they say at each of them, so a call is told what the
/// bindings before it say, whichever of the function's calls is read first.
pub(crate) struct Bindings {
    /// Read up to here (a byte offset in the file).
    upto: usize,
    /// Where the function starts: a declaration's type is looked for back
    /// to here.
    from: usize,
    /// At each binding (where it takes effect: its statement's end), what
    /// the bindings up to it say.
    said: Vec<(usize, Said)>,
    /// The next of the file's functions ([`Places::fns`]) not yet come to,
    /// and those come to and still open — the functions nested in this
    /// one around what is being read.
    next_fn: usize,
    open: Vec<usize>,
}

/// What a name's bindings say ([`Bindings::said_before`]).
#[derive(Debug, Clone, PartialEq)]
enum Said {
    /// Bound to this type — exactly (built), or as declared.
    Type(String, bool),
    /// Bound to something that cannot be told.
    Unknown,
    /// Not bound in the function (a parameter with no type, a global).
    Nothing,
}

impl Bindings {
    fn from(from: usize, places: &Places) -> Self {
        Self {
            upto: from,
            from,
            said: Vec::new(),
            next_fn: places.fns.partition_point(|&(start, _, _)| start <= from),
            open: Vec::new(),
        }
    }

    /// The function nested in this one that `at` is in, if any — `at` never
    /// less than the last asked about.
    fn nested_at(&mut self, at: usize, places: &Places) -> Option<(usize, usize)> {
        while let Some(&(start, _, _)) = places.fns.get(self.next_fn)
            && start <= at
        {
            self.open.push(self.next_fn);
            self.next_fn += 1;
        }
        while self.open.last().is_some_and(|&o| places.fns[o].1 <= at) {
            self.open.pop();
        }
        self.open
            .last()
            .map(|&o| (places.fns[o].0, places.fns[o].1))
    }

    /// What `name`'s bindings before `to` in `src` say.
    fn said_before(&mut self, to: usize, name: &str, scope: &Scope, src: &str, lang: Lang) -> Said {
        self.read_to(to, name, scope, src, lang);
        let before = self.said.partition_point(|&(at, _)| at < to);
        match before.checked_sub(1) {
            Some(last) => self.said[last].1.clone(),
            None => Said::Nothing,
        }
    }

    /// Read `name`'s occurrences up to `to` in `src`, once each.
    fn read_to(&mut self, to: usize, name: &str, scope: &Scope, src: &str, lang: Lang) {
        let settled = matches!(self.said.last(), Some((_, Said::Unknown)));
        if to <= self.upto || settled {
            self.upto = self.upto.max(to);
            return;
        }
        let Some(text) = src.get(self.from..to) else {
            return;
        };
        let word = |c: char| c == '_' || c.is_alphanumeric();
        let mut at = self.upto - self.from;
        while let Some(i) = text[at..].find(name) {
            let start = at + i;
            let end = start + name.len();
            at = end;
            let head = &text[..start];
            if head.ends_with(|c: char| word(c) || matches!(c, '.' | '$' | '@' | '>' | ':'))
                || text[end..].starts_with(word)
                || scope.places.opaque(self.from + start)
            {
                continue; // part of a longer name, a member, a comment or a string
            }
            // What follows it in the file, past what is read so far: a
            // statement read up to a call in it goes on after the call.
            let after = &src[self.from + end..];
            let binding = binding_at(head, after, lang);
            if binding == Binding::Not {
                continue;
            }
            let now = if let Some((nested, nested_end)) =
                self.nested_at(self.from + start, scope.places)
            {
                // In a function nested in this one: a name it declares is its
                // own; one it assigns is this one's, given a value when it
                // runs, which cannot be told. Python's assignment declares,
                // but where it says the name is not its own.
                let said_not_own = |kw: &str| {
                    src[nested..nested_end]
                        .split(kw)
                        .skip(1)
                        .any(|rest| rest.trim_start().starts_with(name))
                };
                let own = if lang == Lang::Python {
                    !(said_not_own("nonlocal ") || said_not_own("global "))
                } else {
                    declares_its_own(head, after, lang)
                };
                if own {
                    continue;
                }
                Said::Unknown
            } else {
                match binding {
                    Binding::Not => continue,
                    Binding::Unknown => Said::Unknown,
                    Binding::Typed(ty, exact) => match self.said.last().map(|(_, s)| s) {
                        Some(Said::Type(seen, _)) if *seen != ty => Said::Unknown,
                        // Built and declared alike: as exact as either says.
                        Some(Said::Type(_, was)) => Said::Type(ty, exact || *was),
                        _ => Said::Type(ty, exact),
                    },
                }
            };
            let unknown = now == Said::Unknown;
            // An assignment is in effect once its statement is over: `item`
            // in `item = Wrapper(item.unwrap())` is still the one before. A
            // parameter, a loop's name, a declaration is at once: `cart` in
            // `cart => cart.total()` is the closure's.
            let value = after.trim_start_matches([' ', '\t']);
            let assigned = value.starts_with(":=")
                || (value.starts_with('=') && !value.starts_with("==") && !value.starts_with("=>"));
            let in_effect = if assigned {
                after.find([';', '\n']).unwrap_or(after.len())
            } else {
                0
            };
            let effect =
                (self.from + end + in_effect).max(self.said.last().map_or(0, |&(at, _)| at));
            self.said.push((effect, now));
            if unknown {
                break;
            }
        }
        self.upto = to;
    }
}

/// What an occurrence of a name says it is bound to.
#[derive(Debug, PartialEq)]
enum Binding {
    /// It is not bound here: a use of it.
    Not,
    /// Bound to this type: exactly (`true`, built by a constructor), or as
    /// declared or annotated (`false`).
    Typed(String, bool),
    /// Bound to what cannot be told: a function's result, a loop's or a
    /// closure's parameter, one of several unpacked.
    Unknown,
}

/// What an occurrence of a name, with `head` before it in its function and
/// `after` after it, says it is bound to: built by a constructor (`name =
/// Cart()`, `name := &Cart{}`, `let name = Cart::new()`, `name = new
/// Cart()`, `name = Cart.new`), declared with it (`Cart name = …`, `Cart*
/// name;`, `(Cart name)`), or annotated with it (`name: Cart`, Go's `name
/// *Cart`) — or bound to what cannot be told.
fn binding_at(head: &str, after: &str, lang: Lang) -> Binding {
    let after = after.trim_start_matches([' ', '\t']);
    let before = head.trim_end();
    let ends_with_word = |w: &str| {
        before.ends_with(w)
            && !before[..before.len() - w.len()]
                .ends_with(|c: char| c == '_' || c.is_alphanumeric())
    };
    // In parentheses (or brackets) opened on its line: a call's argument,
    // keyword or not, binds nothing of the caller's (`show(c=Cart())`); a
    // parameter's default (`def f(c=Cart())`) is what the caller may well
    // pass otherwise; one of a pattern's names (`let (c, d) = …`, `if let
    // Some(c) = …`, `for (i, c) in …`) is bound to what cannot be told.
    let line = &head[head.rfind('\n').map_or(0, |i| i + 1)..];
    // The innermost bracket left open on the line: in `(` or `[`, an
    // argument or a pattern; in a `{` — a block — or none, a statement.
    let mut open: Vec<char> = Vec::new();
    for c in line.chars() {
        match c {
            '(' | '[' | '{' => open.push(c),
            ')' | ']' | '}' => {
                open.pop();
            }
            _ => {}
        }
    }
    let depth = i32::from(matches!(open.last(), Some('(' | '[')));
    let line_rest = &after[..after.find('\n').unwrap_or(after.len())];
    let assigns = |text: &str| {
        let bytes = text.as_bytes();
        bytes.iter().enumerate().any(|(i, &b)| {
            b == b'='
                && !matches!(bytes.get(i + 1), Some(b'=' | b'>'))
                && !matches!(
                    i.checked_sub(1).map(|j| bytes[j]),
                    Some(b'=' | b'!' | b'<' | b'>' | b':')
                )
        })
    };
    let keyword_line = {
        let line = line.trim_start();
        [
            "def ",
            "fn ",
            "function ",
            "func ",
            "async def ",
            "pub fn ",
            "lambda",
        ]
        .iter()
        .any(|k| line.starts_with(k) || line.contains(&format!(" {k}")))
    };
    let pattern_line = {
        let line = line.trim_start();
        [
            "let ",
            "if let ",
            "while let ",
            "for ",
            "const ",
            "var ",
            "val ",
            "match ",
        ]
        .iter()
        .any(|k| line.starts_with(k) || line.contains(&format!(" {k}")))
    };
    if depth > 0 {
        if after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=>") {
            return if keyword_line || pattern_line {
                Binding::Unknown
            } else {
                Binding::Not
            };
        }
        if pattern_line
            && (after.starts_with([',', ')', ']', '}']))
            && (assigns(line_rest) || line_rest.contains(" in "))
        {
            return Binding::Unknown;
        }
    } else if after.starts_with(',') && assigns(line_rest) {
        // `c, rest = split()`: the first of several unpacked.
        return Binding::Unknown;
    }
    // `for i, c in …`: a loop's names after the first.
    if (before.ends_with(',') || before.ends_with('('))
        && after.trim_start_matches([')', ' ']).starts_with("in ")
    {
        return Binding::Unknown;
    }
    // A loop's, a closure's, a `with`'s or an `except`'s name, or one of
    // several unpacked: bound to what this cannot tell.
    if after.starts_with("=>")
        || before.ends_with('|')
        || (after.starts_with('|') && !after.starts_with("||"))
        || ["for", "lambda", "as", "foreach"]
            .iter()
            .any(|w| ends_with_word(w))
        || (["const", "let", "var", "val"]
            .iter()
            .any(|w| ends_with_word(w))
            && (after.starts_with("of ") || after.starts_with("in ")))
        || (depth <= 0
            && before.ends_with(',')
            && after.starts_with('=')
            && !after.starts_with("=="))
    {
        return Binding::Unknown;
    }
    let assigned = if let Some(rest) = after.strip_prefix(":=") {
        Some(rest)
    } else if after.starts_with('=') && !after.starts_with("==") {
        Some(&after[1..])
    } else {
        None
    };
    // A Zig value the standard library makes (`std.ArrayList(T).init(a)`,
    // `try std.json.parseFromSlice(…)`): its methods are the library's.
    if lang == Lang::Zig
        && let Some(rhs) = assigned
        && rhs
            .trim_start()
            .trim_start_matches("try ")
            .trim_start()
            .starts_with("std.")
    {
        return Binding::Typed("std".to_string(), true);
    }
    if let Some(rhs) = assigned {
        return match constructed_type(rhs, lang) {
            Some(ty) => Binding::Typed(ty, true),
            // A declaration with a value of no known type still says its
            // own: `Cart cart = make();`.
            None => match declared_type(head, lang) {
                Some(ty) if declares(head, lang) => Binding::Typed(ty, false),
                _ => match factory_type(rhs, lang) {
                    Some(ty) => Binding::Typed(ty, false),
                    None => Binding::Unknown,
                },
            },
        };
    }
    if let Some(rest) = after.strip_prefix(':').filter(|r| !r.starts_with(':')) {
        // An annotation; a Rust trait object or `impl Trait` names no type
        // its methods are owned by.
        let text = rest.trim_start().trim_start_matches(['&', '*']);
        let text = text.strip_prefix("mut ").unwrap_or(text).trim_start();
        if text.starts_with("dyn ") || text.starts_with("impl ") {
            return Binding::Unknown;
        }
        // Zig's type is a path, the standard library's whole
        // (`*std.Io.Writer`), a project's by its struct (`*Analyser`).
        if lang == Lang::Zig {
            return match zig_type_name(rest) {
                Some(ty) => Binding::Typed(ty, false),
                None => Binding::Unknown,
            };
        }
        // Rust's own types are named in lower case (`&str`, `u8`), and a
        // path names its last segment.
        if lang == Lang::Rust {
            return match rust_type_name(rest) {
                Some(ty) => Binding::Typed(ty, false),
                None => Binding::Unknown,
            };
        }
        return match leading_type(rest, lang) {
            Some(ty) => Binding::Typed(ty, false),
            None => Binding::Not,
        };
    }
    // A declaration names the type before the name: `Cart cart;`.
    if after.starts_with([';', ',', ')'])
        && declares(head, lang)
        && let Some(ty) = declared_type(head, lang)
    {
        return Binding::Typed(ty, false);
    }
    // C++ builds a value where it declares it: `Formatter f("%+");`,
    // `std::vector<int> v{1, 2};` — exactly that type.
    if lang == Lang::Cpp
        && after.starts_with(['(', '{'])
        && let Some(ty) = declared_type(head, lang)
    {
        return Binding::Typed(ty, true);
    }
    // Go writes the type after the name: `(c *Cart)`, `var c Cart`.
    if lang == Lang::Go
        && (before.ends_with(['(', ',']) || ends_with_word("var"))
        && let Some(ty) = leading_type(after, lang)
    {
        return Binding::Typed(ty, false);
    }
    Binding::Not
}

/// Whether the occurrence between `head` and `after` declares a name of
/// its own — `let`, `const`, `var`, `:=`, a declared type, a parameter of
/// a closure or a loop — rather than assigning one from around it.
fn declares_its_own(head: &str, after: &str, lang: Lang) -> bool {
    let before = head.trim_end();
    let after = after.trim_start_matches([' ', '\t']);
    let ends_with_word = |w: &str| {
        before.ends_with(w)
            && !before[..before.len() - w.len()]
                .ends_with(|c: char| c == '_' || c.is_alphanumeric())
    };
    [
        "let", "const", "var", "val", "my", "local", "auto", "for", "foreach", "as", "lambda",
    ]
    .iter()
    .any(|w| ends_with_word(w))
        || after.starts_with(":=")
        || after.starts_with("=>")
        || before.ends_with('|')
        || (after.starts_with('|') && !after.starts_with("||"))
        || declares(head, lang)
}

/// Whether `head` ends with a declaration's type, as a C-like language
/// writes one before the name (`Cart`, `final Cart`, `List<Cart>`).
fn declares(head: &str, lang: Lang) -> bool {
    !matches!(lang, Lang::Python | Lang::Ruby | Lang::Go | Lang::Rust)
        && declared_type(head, lang).is_some()
}

/// The type a declaration names before its name ([`trailing_type`]) — in
/// C++ also one in lower case where it is a namespace's or a template's
/// (`std::vector<int>`, `spdlog::pattern_formatter`), as the standard
/// library and many projects name their types.
fn declared_type(head: &str, lang: Lang) -> Option<String> {
    trailing_type(head).or_else(|| match lang {
        Lang::Cpp => cpp_trailing_type(head),
        _ => None,
    })
}

/// A C++ type before a declared name, written in lower case: qualified by
/// a namespace or given template arguments, so no keyword or variable —
/// `std::vector<basic_format_arg<Context>>` (`vector`), `const
/// fmt::memory_buffer&` (`memory_buffer`).
fn cpp_trailing_type(head: &str) -> Option<String> {
    let mut head = head.trim_end().trim_end_matches(['*', '&', ' ']);
    let templated = head.ends_with('>');
    if templated {
        let mut depth = 0usize;
        let mut cut = None;
        for (i, c) in head.char_indices().rev() {
            match c {
                '>' => depth += 1,
                '<' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        cut = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        head = head[..cut?].trim_end();
    }
    let start = head
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c == '_' || c == ':' || c.is_alphanumeric()))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let qualified = head[start..].trim_start_matches(':');
    let last = qualified.rsplit("::").next()?;
    let named = last.starts_with(|c: char| c == '_' || c.is_alphabetic())
        && !matches!(
            last,
            "return"
                | "delete"
                | "new"
                | "throw"
                | "case"
                | "else"
                | "goto"
                | "sizeof"
                | "co_return"
                | "co_yield"
                | "co_await"
                | "typename"
                | "template"
        );
    // Named by its last segment, as its methods' class is.
    (named && (qualified.contains("::") || templated)).then(|| last.to_string())
}

/// The type a constructor expression builds: `Cart(…)`, `Cart { … }`,
/// `Cart::new(…)`, `new Cart(…)`, `&Cart{…}`, `Cart.new`, `Cart<T>(…)`,
/// `models.Cart(…)` — when it is the whole expression: what is called or
/// read off it (`Cart(p).load()`) is of a type of its own. A function of a
/// type other than `new` (`Config::builder()`) returns what it returns, and
/// `Shape::Circle(c)` is a value of `Shape`, not a `Circle`.
fn constructed_type(rhs: &str, lang: Lang) -> Option<String> {
    let mut rhs = rhs.trim_start();
    for prefix in ["new ", "&mut ", "&"] {
        if let Some(rest) = rhs.strip_prefix(prefix) {
            rhs = rest.trim_start();
        }
    }
    let path_len = rhs
        .find(|c: char| !(c == '_' || c == ':' || c == '.' || c.is_alphanumeric()))
        .unwrap_or(rhs.len());
    let path = &rhs[..path_len];
    let rest = rhs[path_len..].trim_start();
    let segments: Vec<&str> = path.split([':', '.']).filter(|s| !s.is_empty()).collect();
    let typed = |s: &&str| type_like(s, lang);
    // Go builds a value with a composite literal, `Cart{…}`: `newCart(…)`
    // and `pkg.New(…)` are functions, whatever they return.
    let opens: &[char] = if lang == Lang::Go {
        &['{']
    } else {
        &['(', '{', '<']
    };
    let ty = match segments.as_slice() {
        // `Cart::new(…)`, `Cart.new`: the type before its constructor.
        [.., ty, "new"] if lang != Lang::Go && typed(ty) => *ty,
        // `Type::Variant(…)`, `Type::make(…)`, `Outer.Inner(…)`: a value of
        // the type before it, what a function returns, or a type this
        // cannot place. (In Go, `pkg.Cart{…}`, whose package is any name.)
        [.., before, _] if lang != Lang::Go && typed(before) => return None,
        // `Cart(…)`, `Cart { … }`, `Cart<T>(…)`, `models.Cart(…)`.
        [.., last] if typed(last) && rest.starts_with(opens) => *last,
        _ => return None,
    };
    whole_expression(rest).then(|| ty.to_string())
}

/// The type a JVM, C# or Dart factory names, its value taken as of that
/// type (or a subtype): Scala's `IO.pure(1)` and `IO.unit`, Kotlin's
/// `Cart.create()`, Java's `List.of(…)`, C#'s `Cart.Create()` — a type's
/// own function called on it, the whole expression. A guess, as a factory
/// may return another type (a builder); so it is a declared type, whose
/// method is the type's own or an override, never exact.
fn factory_type(rhs: &str, lang: Lang) -> Option<String> {
    if !matches!(
        lang,
        Lang::Scala | Lang::Kotlin | Lang::Java | Lang::CSharp | Lang::Dart
    ) {
        return None;
    }
    let rhs = rhs
        .trim_start()
        .strip_prefix("await ")
        .unwrap_or(rhs.trim_start());
    let path_len = rhs
        .find(|c: char| !(c == '_' || c == '.' || c.is_alphanumeric()))
        .unwrap_or(rhs.len());
    let segments: Vec<&str> = rhs[..path_len].split('.').collect();
    let [.., ty, function] = segments.as_slice() else {
        return None;
    };
    let factory = match lang {
        // C# names its methods in Pascal case.
        Lang::CSharp => function.starts_with(char::is_alphabetic),
        _ => function.starts_with(char::is_lowercase),
    };
    let rest = rhs[path_len..].trim_start();
    let ends = rest.is_empty() || rest.starts_with([';', '\n', ')', ',', '}']);
    (type_like(ty, lang) && factory && (ends || whole_expression(rest))).then(|| ty.to_string())
}

/// Whether what follows a constructor's name — its generic arguments, its
/// arguments or fields, or nothing — ends the expression: nothing is called
/// or read off the value it builds. Text that ends before its brackets
/// close (a binding read up to a call inside it) is taken as whole.
fn whole_expression(rest: &str) -> bool {
    let mut rest = rest.trim_start();
    for (open, close) in [('<', '>'), ('(', ')'), ('{', '}')] {
        if !rest.starts_with(open) {
            continue;
        }
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in rest.char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + c.len_utf8());
                    break;
                }
            }
        }
        match end {
            Some(end) => rest = rest[end..].trim_start(),
            None => return true,
        }
    }
    let rest = rest.strip_prefix('?').unwrap_or(rest);
    !(rest.starts_with('.')
        || rest.starts_with("->")
        || rest.starts_with('[')
        || rest.starts_with('(')
        || rest.starts_with("::"))
}

/// The type an annotation starts with: `Cart`, `&mut Cart`, `*Cart`.
fn leading_type(text: &str, lang: Lang) -> Option<String> {
    let mut text = text.trim_start();
    for prefix in ["&mut ", "&", "*", "mut ", "dyn "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim_start();
        }
    }
    let end = text
        .find(|c: char| !(c == '_' || c.is_alphanumeric()))
        .unwrap_or(text.len());
    let ty = &text[..end];
    type_like(ty, lang).then(|| ty.to_string())
}

/// The type a Zig annotation names: past pointers, optionals, slices and
/// `const` (`*const std.Io.Writer`, `?*Analyser`, `[]const u8`), its path —
/// the standard library's whole (`std.Io.Writer`), a project's struct
/// (`Analyser`, `DocumentStore.Handle`). `None` for a builtin (`u8`,
/// `anytype`) or a type computed (`@This()`).
fn zig_type_name(text: &str) -> Option<String> {
    let mut t = text.trim_start();
    loop {
        let before = t;
        for prefix in ["*", "?", "[]", "const ", "volatile "] {
            if let Some(rest) = t.strip_prefix(prefix) {
                t = rest.trim_start();
            }
        }
        if let Some(rest) = t.strip_prefix('[')
            && let Some(end) = rest.find(']')
        {
            t = rest[end + 1..].trim_start();
        }
        if t == before {
            break;
        }
    }
    let end = t
        .find(|c: char| !(c == '_' || c == '.' || c.is_alphanumeric()))
        .unwrap_or(t.len());
    let path = t[..end].trim_end_matches('.');
    let last = path.rsplit('.').next().unwrap_or(path);
    (path == "std" || path.starts_with("std.") || last.starts_with(char::is_uppercase))
        .then(|| path.to_string())
}

/// The type a Rust type names, as the one whose methods a value of it has:
/// `&'a mut Vec<u8>` is `Vec`, `crate::a::Set` is `Set`, `&str` is `str`;
/// a `Box<T>`, `Rc<T>` or `Arc<T>` hands on `T`'s methods, so it is `T`.
/// `None` for no one type: a trait object, an `impl Trait`, a tuple, a
/// slice or an array.
fn rust_type_name(text: &str) -> Option<String> {
    let mut t = text.trim();
    // A pointer within a pointer within …: as deep as anyone writes.
    for _ in 0..16 {
        loop {
            let before = t;
            t = t.trim_start_matches('&').trim_start();
            if let Some(rest) = t.strip_prefix('\'') {
                t = rest
                    .trim_start_matches(|c: char| c == '_' || c.is_alphanumeric())
                    .trim_start();
            }
            for prefix in ["*const ", "*mut ", "mut "] {
                if let Some(rest) = t.strip_prefix(prefix) {
                    t = rest.trim_start();
                }
            }
            if t == before {
                break;
            }
        }
        if t.starts_with("dyn ") || t.starts_with("impl ") || t.starts_with(['(', '[']) {
            return None;
        }
        let end = t
            .find(|c: char| !(c == '_' || c == ':' || c.is_alphanumeric()))
            .unwrap_or(t.len());
        let name = t[..end].rsplit("::").next()?;
        if !name.starts_with(|c: char| c == '_' || c.is_alphabetic()) {
            return None;
        }
        if !matches!(name, "Box" | "Rc" | "Arc") {
            return Some(name.to_string());
        }
        // The pointer's content: its first type argument.
        let inner = t[end..].trim_start().strip_prefix('<')?;
        let mut depth = 0usize;
        let mut close = None;
        for (i, c) in inner.char_indices() {
            match c {
                '<' | '(' | '[' => depth += 1,
                '>' | ')' | ']' | ',' if depth == 0 => {
                    close = Some(i);
                    break;
                }
                '>' | ')' | ']' => depth -= 1,
                _ => {}
            }
        }
        t = inner[..close?].trim();
    }
    None
}

/// The type of the field `field` of the type whose `impl` the Rust call
/// `call` is in, as its struct declares it beside that `impl` (in the same
/// module of the file): `items` of `struct Order { items: Vec<u8> }` is
/// `Vec`. `None` where it is not found there.
fn rust_field_type(call: Node, field: &str, src: &str) -> Option<String> {
    // Up to the `impl`: a method's body is a few levels down; a call deeper
    // than this in one is not one anyone wrote.
    let mut at = call;
    let imp = (0..64).find_map(|_| {
        at = at.parent()?;
        (at.kind() == "impl_item").then_some(at)
    })?;
    let owner = rust_type_name(node_text(imp.child_by_field_name("type")?, src))?;
    let named = |n: &Node, name: &str| {
        n.child_by_field_name("name")
            .is_some_and(|x| node_text(x, src) == name)
    };
    let scope = imp.parent()?;
    let mut cursor = scope.walk();
    let declared = scope
        .named_children(&mut cursor)
        .find(|n| n.kind() == "struct_item" && named(n, &owner))?;
    let body = declared.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let decl = body
        .named_children(&mut cursor)
        .find(|n| n.kind() == "field_declaration" && named(n, field))?;
    rust_type_name(node_text(decl.child_by_field_name("type")?, src))
}

/// The type a declaration ends with before its name: `Cart`, `final Cart`,
/// `Cart*`, `List<Cart>` → `List`.
fn trailing_type(head: &str) -> Option<String> {
    let mut head = head.trim_end().trim_end_matches(['*', '&', ' ']);
    if head.ends_with('>') {
        let mut depth = 0usize;
        let mut cut = None;
        for (i, c) in head.char_indices().rev() {
            match c {
                '>' => depth += 1,
                '<' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        cut = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        head = head[..cut?].trim_end();
    }
    // After the last character not of a name — however many bytes it is:
    // a line before ending in `。` or `→` put the cut inside it.
    let start = head
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c == '_' || c.is_alphanumeric()))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let ty = &head[start..];
    ty.starts_with(|c: char| c.is_uppercase())
        .then(|| ty.to_string())
}

/// Whether `name` reads as a type: capitalized, as types are written —
/// in Go, any name, as an unexported type is lowercase.
fn type_like(name: &str, lang: Lang) -> bool {
    let first = name.chars().next();
    let identifier = first.is_some_and(|c| c == '_' || c.is_alphabetic())
        && name.chars().all(|c| c == '_' || c.is_alphanumeric());
    identifier && (lang == Lang::Go || first.is_some_and(char::is_uppercase))
}

/// Containers a method belongs to, by node kind, named by their `name`
/// field. Traits, interfaces, protocols and mixin modules are left out: a
/// call on `self` in one of their methods may reach any implementation.
const CLASS_KINDS: &[&str] = &[
    "class_definition",
    "class_declaration",
    "class",
    "class_specifier",
    "struct_specifier",
    "union_specifier",
    "enum_declaration",
    "record_declaration",
    "struct_declaration",
    "object_declaration",
    "abstract_class_declaration",
    // Scala's `object Cart` and `trait Pricing`.
    "object_definition",
    "trait_definition",
];

/// The class (type, impl) each callable of a file belongs to, by its
/// definition line ([`FileCalls::owners`]).
pub fn owners_in(
    tree: &Tree,
    src: &str,
    lang: Lang,
    symbols: &[Located],
) -> HashMap<usize, String> {
    let root = tree.root_node();
    symbols
        .iter()
        .filter(|l| is_callable(&l.symbol.kind))
        .filter_map(|l| {
            // Inside the signature, past any decorator or doc comment the
            // declaration's span takes in.
            let at = l.signature_end.saturating_sub(1).max(l.declaration.0);
            let leaf = root.descendant_for_byte_range(at, at)?;
            Some((l.symbol.line, owner_from(leaf, src, lang)?))
        })
        .collect()
}

/// The class a definition at `node` (any node inside it) belongs to.
fn owner_from(node: Node, src: &str, lang: Lang) -> Option<String> {
    match lang {
        Lang::Elixir => return elixir_module_of(node, src),
        Lang::Erlang => return erlang_module_of(node, src),
        _ => {}
    }
    let mut cur = Some(node);
    while let Some(n) = cur {
        match n.kind() {
            // Go: `func (c *Cart) Total()` belongs to `Cart`.
            "method_declaration" if lang == Lang::Go => {
                let receiver = n.child_by_field_name("receiver")?;
                let mut cursor = receiver.walk();
                let param = receiver.named_children(&mut cursor).next()?;
                return base_type_name(node_text(param.child_by_field_name("type")?, src));
            }
            // Rust: `impl Cart { … }`, `impl Trait for Cart { … }`.
            "impl_item" => return base_type_name(node_text(n.child_by_field_name("type")?, src)),
            // C++: `int Cart::total() { … }`, defined out of its class.
            "function_definition" if lang == Lang::Cpp => {
                let declarator = node_text(n.child_by_field_name("declarator")?, src);
                let name = declarator.split('(').next().unwrap_or("");
                if let Some((scope, _)) = name.rsplit_once("::") {
                    return base_type_name(scope);
                }
            }
            // Ruby's `module Shop` (no other grammar's `module`: Python's
            // is its file).
            "module" if lang == Lang::Ruby => {
                if let Some(name) = n.child_by_field_name("name") {
                    return base_type_name(node_text(name, src));
                }
            }
            kind if CLASS_KINDS.contains(&kind) => {
                if let Some(name) = n.child_by_field_name("name") {
                    return base_type_name(node_text(name, src));
                }
            }
            // A class the grammar could not finish (Kotlin's grammar takes
            // a property named `where` for its keyword): its header is the
            // error's first tokens, and its body's open brace still open.
            "ERROR" => {
                if let Some(name) = unfinished_class(n, node, src) {
                    return Some(name);
                }
            }
            _ => {}
        }
        cur = n.parent();
    }
    None
}

/// The class whose header a parse error kept as its own tokens —
/// `class Query (…) : Base {` — and whose body is still open where
/// `inside` is: no `}` of the error's own closed it before.
fn unfinished_class(error: Node, inside: Node, src: &str) -> Option<String> {
    let mut cursor = error.walk();
    let tokens: Vec<Node> = error.children(&mut cursor).collect();
    let mut open: Option<String> = None;
    let mut named: Option<String> = None;
    for (i, token) in tokens.iter().enumerate() {
        if token.start_byte() >= inside.start_byte() {
            break;
        }
        match token.kind() {
            "class" | "object" | "struct" | "enum" | "record" => {
                named = tokens[i + 1..]
                    .iter()
                    .find(|t| t.is_named())
                    .filter(|t| t.kind().ends_with("identifier"))
                    .map(|t| node_text(*t, src).to_string());
            }
            "{" => open = named.take().or(open),
            "}" => open = None,
            _ => {}
        }
    }
    open.and_then(|name| base_type_name(&name))
}

/// The Elixir module a definition at `node` is in, by its full name: each
/// `defmodule` (or `defprotocol`) around it, outermost first —
/// `Cart.Inner` for a function of `defmodule Inner` inside `defmodule
/// Cart`. What another module's `Cart.Inner.f(…)` names.
fn elixir_module_of(node: Node, src: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.kind() == "call"
            && n.child_by_field_name("target")
                .is_some_and(|t| matches!(node_text(t, src), "defmodule" | "defprotocol"))
            && let Some(name) = n
                .named_children(&mut n.walk())
                .find(|c| c.kind() == "arguments")
                .and_then(|a| a.named_child(0))
                .filter(|a| a.kind() == "alias")
        {
            parts.push(node_text(name, src));
        }
        cur = n.parent();
    }
    (!parts.is_empty()).then(|| {
        parts.reverse();
        parts.join(".")
    })
}

/// The Erlang module a definition at `node` is in: its file's
/// `-module(cart).` — what another module's `cart:add(…)` names.
fn erlang_module_of(node: Node, src: &str) -> Option<String> {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut cursor = root.walk();
    let attribute = root
        .named_children(&mut cursor)
        .find(|n| n.kind() == "module_attribute")?;
    let name = attribute.child_by_field_name("name")?;
    Some(node_text(name, src).trim_matches('\'').to_string())
}

/// The module an Elixir `Cart.add(…)` or an Erlang `cart:add(…)` names —
/// written out, not one a variable holds (`mod.f()`, `?MODULE:f()`).
fn module_receiver(call: Node, parent: Option<Node>, src: &str, lang: Lang) -> Option<Receiver> {
    let module = match lang {
        Lang::Elixir => {
            let target = call.child_by_field_name("target")?;
            let left = target.child_by_field_name("left")?;
            // `:crypto.hash(…)`: an Erlang module, by its atom.
            if target.kind() == "dot" && left.kind() == "atom" {
                let name = node_text(left, src).trim_start_matches(':');
                return Some(Receiver::Type(name.to_string()));
            }
            (target.kind() == "dot" && left.kind() == "alias").then_some(left)?
        }
        // `fun cart:add/2` names its module itself.
        Lang::Erlang if call.kind() == "external_fun" => {
            let module = call.child_by_field_name("module")?;
            let atom = module.child_by_field_name("name")?;
            (atom.kind() == "atom").then_some(atom)?
        }
        Lang::Erlang => {
            let remote = parent.filter(|p| p.kind() == "remote")?;
            let module = remote.child_by_field_name("module")?;
            let atom = module.child_by_field_name("module")?;
            (atom.kind() == "atom").then_some(atom)?
        }
        _ => return None,
    };
    let name: String = node_text(module, src)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    Some(Receiver::Type(name.trim_matches('\'').to_string()))
}

/// A type's own name in how it is written: `*Cart[T]` → `Cart`,
/// `ns::Cart<T>` → `Cart`, `&'a mut Cart` → `Cart`.
fn base_type_name(text: &str) -> Option<String> {
    let text = text.split(['<', '[', '(']).next()?.trim();
    let name = text
        .rsplit(|c: char| !(c == '_' || c.is_alphanumeric()))
        .find(|s| !s.is_empty())?;
    (name.starts_with(|c: char| c == '_' || c.is_alphabetic())).then(|| name.to_string())
}

/// Raw spelling is useful for display, but `foo` and `r#foo` name the same
/// Rust item. Normalize both sides of resolution without changing node keys.
fn logical_name(name: &str, lang: Option<Lang>) -> &str {
    if lang == Some(Lang::Rust) {
        name.strip_prefix("r#").unwrap_or(name)
    } else {
        name
    }
}

/// Most files a C or C++ file's includes reach, through one another
/// ([`include_closure`]): past it, the rest are not looked in.
const MAX_INCLUDED: usize = 4_096;

/// Every file `file` includes, through the headers it includes too: what
/// the preprocessor pastes into it. `direct` is its own includes, `scope`
/// every file's.
fn include_closure(
    file: &Path,
    direct: &HashSet<PathBuf>,
    scope: &HashMap<PathBuf, HashSet<PathBuf>>,
) -> HashSet<PathBuf> {
    let mut seen: HashSet<PathBuf> = direct.clone();
    let mut queue: Vec<&PathBuf> = direct.iter().collect();
    while let Some(next) = queue.pop() {
        for inner in scope.get(next).into_iter().flatten() {
            if seen.len() >= MAX_INCLUDED {
                return seen;
            }
            if inner.as_path() != file && seen.insert(inner.clone()) {
                queue.push(inner);
            }
        }
    }
    seen
}

/// Most definitions one call resolves to (see [`resolve_callees`]): more
/// than any overload set, or than the `new` of each module a crate root
/// declares.
const MAX_CANDIDATES: usize = 64;

/// Most edges a graph holds: past it, further calls are left out.
const MAX_EDGES: usize = 2_000_000;

/// Resolve a call to the project definitions it plausibly refers to, narrowing
/// by scope so a name isn't sprayed across every same-named definition:
///   1. a definition in the **same file** (a local helper), else
///   2. definitions in files the caller **imports** (in scope via `use`), else
///   3. in C and C++, the definitions a **declaration in scope** names — a
///      header's `int add(int, int);` in `class Cart`, a file's own
///      prototype — with that declaration's owner, wherever they are (the
///      program links its files into one), else
///   4. for a *free* call only, a **globally unique** definition of that name
///      in a language that shares a namespace with the call site (a `.tsx`
///      component calling a `.ts` helper, C++ calling C).
///
/// A definition with a body always beats a bodyless declaration of the same
/// name (a C prototype in a header, an overload signature), which stands in
/// only when nothing defines the name. A method call that matches none of 1–2
/// resolves to nothing rather than guessing (its name belongs to a receiver
/// type we can't see).
fn resolve_callees(
    defs: &Defs,
    nodes: &[SymNode],
    facts: &[NodeFacts],
    owner: &[Option<&str>],
    call: &CallSite,
    site: &Site,
) -> Vec<usize> {
    let name = logical_name(&call.callee, Some(site.lang));
    let Some(named) = defs.named.get(name) else {
        return Vec::new();
    };
    let with_body = !named.defined.is_empty();
    let cands: &[usize] = if with_body {
        &named.defined
    } else {
        &named.all
    };
    // The candidates in `file`, with a body when any definition has one —
    // and, a BEAM function's, of the call's arity.
    fn in_file<'d>(
        defs: &'d Defs<'d>,
        facts: &'d [NodeFacts],
        file: &'d Path,
        name: &'d str,
        with_body: bool,
        arity: Option<u8>,
    ) -> impl Iterator<Item = usize> + 'd {
        defs.in_file
            .get(&(file, name))
            .into_iter()
            .flatten()
            .copied()
            .filter(move |&i| (!with_body || !facts[i].bodyless) && facts[i].takes(arity))
    }
    // A name defined more often than any real overload set is ambiguous: it
    // resolves to none of them. Linking every one made a file of N
    // same-named definitions calling each other N² edges — tens of GB from
    // one generated file within the index's size cap.
    let local: Vec<usize> = in_file(defs, facts, site.file, name, with_body, call.arity)
        .take(MAX_CANDIDATES + 1)
        .collect();
    if local.len() > MAX_CANDIDATES {
        return Vec::new();
    }
    if !local.is_empty() {
        return local;
    }
    // From whichever side is smaller: the candidates, or the files in scope.
    let scoped: Vec<usize> = match cands.len() <= site.scope_len() {
        true => cands
            .iter()
            .copied()
            .filter(|&i| facts[i].takes(call.arity) && site.sees(&nodes[i].file))
            .take(MAX_CANDIDATES + 1)
            .collect(),
        false => {
            let mut seen: HashSet<usize> = HashSet::new();
            site.scope()
                .flat_map(|file| in_file(defs, facts, file, name, with_body, call.arity))
                .filter(|&i| seen.insert(i))
                .take(MAX_CANDIDATES + 1)
                .collect()
        }
    };
    if scoped.len() > MAX_CANDIDATES {
        return Vec::new();
    }
    if !scoped.is_empty() {
        let mut scoped = scoped;
        // In the candidates' order, whichever side they came from.
        scoped.sort_unstable();
        return scoped;
    }
    // C and C++: what a header in scope (or the file's own prototype)
    // declares, another file defines — the include brings the declaration
    // only. Its definitions are the ones of its owner: `Cart`'s `add` for a
    // declaration in `class Cart`, a free function's for a free one; never
    // another file's `static` one, which is that file's alone.
    // Looked up from the name's declarations, few, not from the files in
    // scope, which a C++ file's includes make hundreds.
    if matches!(site.lang, Lang::C | Lang::Cpp) && with_body {
        let declared: HashSet<Option<&str>> = named
            .all
            .iter()
            .copied()
            .filter(|&i| facts[i].bodyless)
            .filter(|&i| nodes[i].file == site.file || site.sees(&nodes[i].file))
            .map(|i| owner[i])
            .collect();
        if !declared.is_empty() {
            let linked: Vec<usize> = named
                .defined
                .iter()
                .copied()
                .filter(|&i| {
                    matches!(facts[i].lang, Some(Lang::C | Lang::Cpp))
                        && !facts[i].internal
                        && declared.contains(&owner[i])
                })
                .take(MAX_CANDIDATES + 1)
                .collect();
            if linked.len() > MAX_CANDIDATES {
                return Vec::new();
            }
            if !linked.is_empty() {
                return linked;
            }
        }
    }
    // A lone global definition of this name — accepted only when its language
    // can be reached from the call site's. Otherwise a Go `Foo()` would resolve
    // to a JS function named `Foo`, a spurious cross-language edge.
    if !call.method
        // An Elixir or Erlang name is called bare only where it is defined
        // or imported: one the file reaches neither way is a library's
        // macro (Ecto's `execute`, ExMachina's `insert`), not the
        // project's one function of the name.
        && !matches!(site.lang, Lang::Elixir | Lang::Erlang)
        && let [only] = cands
        && !facts[*only].internal
        && facts[*only]
            .lang
            .is_some_and(|l| l.shares_namespace_with(site.lang))
    {
        return vec![*only];
    }
    Vec::new()
}

/// Whether a language's types have methods from others — a base class's,
/// a trait's or interface's default, Go's embedded type's — so a call on a
/// type may run one it does not define, or a subtype's override.
fn has_inheritance(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Java
            | Lang::Kotlin
            | Lang::Scala
            | Lang::CSharp
            | Lang::Dart
            | Lang::Cpp
            | Lang::Python
            | Lang::Php
            | Lang::Ruby
            | Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Go
    )
}

/// Whether a bare call in a method may be one of its own class's methods
/// (an implicit `this`): Java's, Kotlin's, Scala's, C#'s, Dart's, C++'s and
/// Ruby's — not Python's or JavaScript's, which say `self.`/`this.`, nor
/// Go's, whose methods always name their receiver.
fn implicit_self(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Java
            | Lang::Kotlin
            | Lang::Scala
            | Lang::CSharp
            | Lang::Dart
            | Lang::Cpp
            | Lang::Ruby
    )
}

/// Whether `call` is the language's own, never one of the project's: a
/// bare call of a builtin (`len(x)`, `make(…)`, ExUnit's `test`), or a Ruby
/// value of no known type called with a core method (`items.each`,
/// `user.save`, `scope.where`) — the core library's or Rails', not a
/// project method that happens to share the name.
pub fn is_library_call(lang: Lang, call: &CallSite) -> bool {
    (!call.method && is_builtin(lang, &call.callee))
        || (call.method
            && call.receiver.is_none()
            && lang == Lang::Ruby
            && RUBY_CORE_METHODS.contains(&call.callee.as_str()))
        // Every object's own (`x.toString()`, `x.Equals(y)`) on a value
        // of no known type: whichever class it is, not one the reader can
        // pick.
        || (call.method
            && call.receiver.is_none()
            && matches!(
                lang,
                Lang::Java | Lang::Kotlin | Lang::Scala | Lang::CSharp | Lang::Dart
            )
            && matches!(
                call.callee.as_str(),
                "toString"
                    | "equals"
                    | "hashCode"
                    | "getClass"
                    | "compareTo"
                    | "ToString"
                    | "Equals"
                    | "GetHashCode"
                    | "GetType"
                    | "CompareTo"
                    | "noSuchMethod"
            ))
}

/// The methods Ruby's core classes and Rails' models give every value: a
/// call of one on a value whose type is not known is theirs.
const RUBY_CORE_METHODS: &[&str] = &[
    "each",
    "each_with_index",
    "each_with_object",
    "each_pair",
    "each_key",
    "each_value",
    "map",
    "flat_map",
    "collect",
    "select",
    "filter",
    "filter_map",
    "reject",
    "find",
    "detect",
    "find_index",
    "count",
    "size",
    "length",
    "first",
    "last",
    "empty?",
    "any?",
    "all?",
    "none?",
    "include?",
    "member?",
    "key?",
    "has_key?",
    "merge",
    "merge!",
    "keys",
    "values",
    "to_s",
    "to_h",
    "to_a",
    "to_i",
    "to_f",
    "to_sym",
    "to_json",
    "as_json",
    "inspect",
    "join",
    "split",
    "strip",
    "chomp",
    "gsub",
    "sub",
    "push",
    "pop",
    "shift",
    "unshift",
    "delete",
    "delete_if",
    "fetch",
    "dig",
    "sort",
    "sort_by",
    "uniq",
    "compact",
    "flatten",
    "reverse",
    "min",
    "max",
    "sum",
    "inject",
    "reduce",
    "group_by",
    "partition",
    "zip",
    "take",
    "drop",
    "slice",
    "tap",
    "then",
    "freeze",
    "dup",
    "clone",
    "nil?",
    "present?",
    "blank?",
    "presence",
    "try",
    "send",
    "public_send",
    "respond_to?",
    "is_a?",
    "kind_of?",
    "instance_of?",
    "equal?",
    "eql?",
    "hash",
    "where",
    "find_by",
    "find_each",
    "order",
    "limit",
    "offset",
    "pluck",
    "includes",
    "joins",
    "group",
    "exists?",
    "update",
    "update!",
    "save",
    "save!",
    "destroy",
    "destroy!",
    "reload",
    "touch",
    "valid?",
    "errors",
    "attributes",
    "assign_attributes",
    "new_record?",
    "persisted?",
    "changed?",
    "increment!",
    "decrement!",
    "deliver_now",
    "deliver_later",
    "perform_later",
    "perform_now",
    // `.new` runs `initialize`: never a method of the name (a Rails
    // controller's `new` action).
    "new",
];

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
        // A method is called through `$this->`, never bare, so a bare
        // `count($x)` is PHP's own even in a class that defines `count()` —
        // and a `Countable`'s `count`, an iterator's `current`/`key`/`next`,
        // a money type's `round`, are what it would resolve to otherwise.
        // `isset`, `empty` and `die` are language constructs the grammar
        // reads as calls.
        Lang::Php => matches!(
            name,
            "abs"
                | "ceil"
                | "count"
                | "current"
                | "date"
                | "die"
                | "empty"
                | "end"
                | "floor"
                | "implode"
                | "isset"
                | "key"
                | "max"
                | "min"
                | "next"
                | "reset"
                | "round"
                | "sort"
                | "trim"
        ),
        // `Kernel`'s methods, called bare everywhere: an IO wrapper's own
        // `puts` or a formatter's `format` would otherwise take every one of
        // them. (A class that defines one of these and calls its own bare
        // loses that edge; the name alone cannot tell.)
        Lang::Ruby => matches!(
            name,
            "format"
                | "loop"
                | "p"
                | "pp"
                | "print"
                | "printf"
                | "puts"
                | "raise"
                | "require"
                | "require_relative"
                | "sleep"
                | "sprintf"
        ),
        // Erlang's auto-imported BIFs. A module that defines one under the
        // same name and arity cannot call its own bare (unless it declares
        // `no_auto_import`), so a bare `get(K)` or `size(T)` is the BIF;
        // other modules call the project's as `kv:get(K)`, a remote call.
        // A local function of another arity called bare (`get/2`) loses its
        // edge: the name alone cannot tell.
        Lang::Erlang => matches!(
            name,
            "apply"
                | "element"
                | "error"
                | "exit"
                | "get"
                | "length"
                | "max"
                | "min"
                | "node"
                | "put"
                | "round"
                | "self"
                | "size"
                | "spawn"
                | "throw"
        ),
        // The Prelude's functions, in scope unqualified in every module: a
        // project's container module defines its own `lookup` or `filter`
        // and is imported qualified for them, so a bare call elsewhere is
        // the Prelude's. (The module itself, hiding the Prelude's to call
        // its own bare, loses those edges; the name alone cannot tell.)
        Lang::Haskell => matches!(
            name,
            "concat"
                | "elem"
                | "filter"
                | "head"
                | "length"
                | "lookup"
                | "map"
                | "null"
                | "print"
                | "reverse"
                | "show"
                | "sum"
                | "tail"
                | "zip"
        ),
        // No bare-name builtins that a project would also define: a project
        // `malloc` wrapper IS what a C call to `malloc` links against, and
        // C++'s `std::` calls are excluded where they are read. C# has no
        // free functions — a bare `F()` is a method of the class or a local
        // function — and its `nameof` is dropped where it is read; Zig spells
        // its builtins `@name`, and those are no call expressions.
        // Kotlin's standard functions a project is as likely to define.
        Lang::Kotlin => matches!(
            name,
            "println"
                | "print"
                | "listOf"
                | "mutableListOf"
                | "setOf"
                | "mutableSetOf"
                | "mapOf"
                | "mutableMapOf"
                | "arrayOf"
                | "require"
                | "requireNotNull"
                | "check"
                | "checkNotNull"
                | "error"
                | "repeat"
                | "lazy"
                | "run"
                | "let"
                | "also"
                | "apply"
                | "with"
                | "TODO"
        ),
        Lang::Scala => matches!(
            name,
            "println" | "print" | "require" | "assert" | "List" | "Seq" | "Map" | "Set" | "Some"
        ),
        // Elixir's special forms and Kernel macros, ExUnit's too: what a
        // module is written with, never a call of the project's.
        // dart:core's top-level functions, and package:test's (with
        // flutter_test's and bloc_test's) runner and its distinctive
        // matchers: a test's `expect(…)` is the library's.
        Lang::Dart => matches!(
            name,
            "print"
                | "identical"
                | "identityHashCode"
                | "expect"
                | "expectLater"
                | "expectAsync0"
                | "expectAsync1"
                | "expectAsync2"
                | "test"
                | "group"
                | "setUp"
                | "setUpAll"
                | "tearDown"
                | "tearDownAll"
                | "addTearDown"
                | "fail"
                | "markTestSkipped"
                | "printOnFailure"
                | "testWidgets"
                | "blocTest"
                | "isA"
                | "throwsA"
                | "completion"
                | "emits"
                | "emitsInOrder"
                | "emitsDone"
                | "emitsError"
                | "emitsThrough"
                | "neverEmits"
        ),
        Lang::Elixir => matches!(
            name,
            "alias"
                | "import"
                | "use"
                | "require"
                | "if"
                | "unless"
                | "case"
                | "cond"
                | "with"
                | "for"
                | "receive"
                | "try"
                | "quote"
                | "unquote"
                | "raise"
                | "reraise"
                | "throw"
                | "send"
                | "defmodule"
                | "defstruct"
                | "defexception"
                | "defimpl"
                | "defprotocol"
                | "defdelegate"
                | "defoverridable"
                | "describe"
                | "test"
                | "setup"
                | "assert"
                | "refute"
                | "is_nil"
                | "to_string"
                | "inspect"
                | "length"
                | "elem"
                | "hd"
                | "tl"
                | "map_size"
                | "setup_all"
                | "assert_raise"
                | "assert_receive"
                | "assert_received"
                | "refute_receive"
                | "refute_received"
                | "assert_in_delta"
                | "catch_error"
                | "catch_exit"
                | "catch_throw"
                | "flunk"
                | "doctest"
                | "on_exit"
                | "start_supervised"
                | "start_supervised!"
                | "is_atom"
                | "is_binary"
                | "is_list"
                | "is_map"
                | "is_integer"
                | "is_function"
                | "is_struct"
                | "is_tuple"
                | "is_boolean"
                | "is_float"
                | "is_number"
                | "is_pid"
                | "apply"
                | "spawn"
                | "spawn_link"
                | "self"
                | "struct"
                | "struct!"
                | "get_in"
                | "put_in"
                | "update_in"
                | "pop_in"
                | "then"
                | "tap"
                | "dbg"
                | "put_elem"
                | "tuple_size"
                | "byte_size"
                | "div"
                | "rem"
                | "defguard"
        ),
        Lang::Rust
        | Lang::C
        | Lang::Cpp
        | Lang::Java
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Scss
        | Lang::Less
        | Lang::Vue
        | Lang::Svelte
        | Lang::Astro
        | Lang::Zig
        | Lang::Sql
        | Lang::Lean
        | Lang::CSharp => false,
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
///
/// `callers` holds each file's definitions by name, in line order
/// ([`Defs::callers`]). The innermost body is searched for among the closest
/// [`MAX_CALLER_SCAN`] definitions before the call: past them, the closest
/// one stands in.
fn resolve_caller(
    callers: &HashMap<(&Path, &str), Vec<usize>>,
    file: &Path,
    name: &str,
    call_line: usize,
    nodes: &[SymNode],
    bodies: &HashMap<usize, (usize, usize)>,
) -> Option<usize> {
    let ids = callers.get(&(file, name))?;
    let first = *ids.first()?;
    // The ones defined at or before the call: only those can hold it.
    let before = &ids[..ids.partition_point(|&id| nodes[id].line <= call_line)];
    let best = before.last().copied();
    let mut innermost: Option<(usize, usize)> = None; // (id, span length)
    for &id in before.iter().rev().take(MAX_CALLER_SCAN) {
        if let Some(&(lo, hi)) = bodies.get(&nodes[id].line)
            && (lo..=hi).contains(&call_line)
            && innermost.is_none_or(|(_, len)| hi - lo < len)
        {
            innermost = Some((id, hi - lo));
        }
    }
    innermost.map(|(id, _)| id).or(best).or(Some(first))
}

/// How many same-named definitions before a call [`resolve_caller`] looks
/// through for the body holding it. Real nesting reuses a name a few times;
/// a file of thousands of them made each call cost their number.
const MAX_CALLER_SCAN: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    /// A component rendered is a call of it: `<Layout>` and `<Button/>`
    /// from `App`, `<Select.Item>` the `Item` of `Select`; a page's own
    /// element (`<div>`) is none, and `new Store()` calls `Store`.
    #[test]
    fn jsx_renders_are_calls() {
        let src = "function App() {\n  const store = new Store();\n  return (\n    <Layout title=\"x\">\n      <div>\n        <Button onClick={() => go()} />\n        <Select.Item value=\"a\" />\n      </div>\n    </Layout>\n  );\n}\n";
        let calls = calls_of(src, "tsx");
        let got: Vec<(&str, &str, bool)> = calls
            .iter()
            .map(|c| {
                (
                    c.caller.as_deref().unwrap_or(""),
                    c.callee.as_str(),
                    c.method,
                )
            })
            .collect();
        for want in [
            ("App", "Store", false),
            ("App", "Layout", false),
            ("App", "Button", false),
            ("App", "Item", true),
            ("App", "go", false),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(_, callee, _)| *callee == "div"),
            "{got:?}"
        );
        assert_eq!(
            got.iter().filter(|(_, c, _)| *c == "Layout").count(),
            1,
            "its opening tag once"
        );
    }

    /// A JavaScript runtime's objects call none of the project's functions:
    /// `console.log`, `JSON.parse` and `document.querySelector` reach no
    /// project `log`, `parse` or `querySelector`, though the project has
    /// them; a call on the project's own `logger` reaches its `log`.
    #[test]
    fn js_globals_call_none_of_the_projects_functions() {
        let util = "export function log(m) {}\nexport function parse(s) {}\n";
        let app = "import { log } from './util';\nconst logger = { log };\nfunction run() {\n  console.log('a');\n  JSON.parse('{}');\n  document.querySelector('#x');\n  logger.log('b');\n}\n";
        let util_path = PathBuf::from("/p/util.js");
        let app_path = PathBuf::from("/p/app.js");
        let sources: Vec<(PathBuf, String)> = vec![
            (util_path.clone(), util.into()),
            (app_path.clone(), app.into()),
        ];
        let mut defs = Vec::new();
        for (file, src) in &sources {
            for l in crate::outline::extract_located(src, "javascript") {
                defs.push(Def {
                    name: l.symbol.name,
                    kind: l.symbol.kind,
                    file: file.clone(),
                    line: l.symbol.line,
                });
            }
        }
        let scope: HashMap<PathBuf, HashSet<PathBuf>> =
            HashMap::from([(app_path.clone(), HashSet::from([util_path.clone()]))]);
        let graph = ProjectCallGraph::build(defs, &sources, &scope);
        let callers_of = |name: &str| -> Vec<String> {
            let id = graph.id_of(&util_path, name).expect(name);
            graph
                .node(id)
                .callers
                .iter()
                .map(|&c| graph.node(c).name.clone())
                .collect()
        };
        assert!(
            callers_of("parse").is_empty(),
            "JSON.parse is no project parse"
        );
        assert_eq!(callers_of("log"), ["run"], "logger.log is");
    }

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
        // `Greeter().hello()` is a method call, so it never takes the "only
        // definition of that name" fallback; a bare `_build()` is not.
        let method = |callee: &str| calls.iter().find(|c| c.callee == callee).unwrap().method;
        assert!(method("hello"), "{calls:?}");
        assert!(!method("_build"), "{calls:?}");
    }

    /// A body's name comes from the signature before it, carried by the
    /// walk: looked up among its parent's children, a file of many
    /// top-level functions cost their number per function.
    #[test]
    fn a_wide_dart_file_is_named_in_one_walk() {
        const N: usize = 20_000;
        let src: String = (0..N)
            .map(|i| format!("void f{i}() {{ g{i}(); }}\n"))
            .collect();
        let started = std::time::Instant::now();
        let calls = calls_of(&src, "dart");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(calls.len(), N);
        for i in [0, N / 2, N - 1] {
            let call = calls.iter().find(|c| c.callee == format!("g{i}"));
            assert_eq!(
                call.and_then(|c| c.caller.as_deref()),
                Some(format!("f{i}").as_str())
            );
        }
    }

    #[test]
    fn dart_receiver_calls_are_method_calls() {
        let src = "void f(List<int> xs, Cart? c) {\n  xs.add(1);\n  c?.checkout();\n}\n";
        let calls = calls_of(src, "dart");
        for callee in ["add", "checkout"] {
            let call = calls.iter().find(|c| c.callee == callee);
            assert!(call.is_some_and(|c| c.method), "{callee}: {calls:?}");
        }
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

    /// Lean's calls: across the files a file imports, a namespace's name
    /// called by its namespace (`Voice.toModel`, `Voice` being
    /// `Clew.Voice`), and nothing for a name in a namespace the project has
    /// none of (`Nat.succ` is no project `succ`).
    #[test]
    fn lean_calls_link_across_imports_and_namespaces() {
        let basic = PathBuf::from("/p/Demo/Basic.lean");
        let voice = PathBuf::from("/p/Demo/Voice.lean");
        let usef = PathBuf::from("/p/Demo/Use.lean");
        let sources = vec![
            (
                basic.clone(),
                "def double (n : Nat) : Nat := n + n\n\ndef succ (n : Nat) : Nat := n\n".to_string(),
            ),
            (
                voice.clone(),
                "namespace Clew.Voice\n\ndef toModel (n : Nat) : Nat := n\n\nend Clew.Voice\n".to_string(),
            ),
            (
                usef.clone(),
                "import Demo.Basic\nimport Demo.Voice\n\ndef quad (n : Nat) : Nat :=\n  double (double n)\n\ndef speak (n : Nat) : Nat :=\n  Voice.toModel (Nat.succ n)\n".to_string(),
            ),
        ];
        let mut defs = Vec::new();
        for (file, src) in &sources {
            for l in crate::outline::extract_located(src, "lean") {
                defs.push(Def {
                    name: l.symbol.name,
                    kind: l.symbol.kind,
                    file: file.clone(),
                    line: l.symbol.line,
                });
            }
        }
        let scope: HashMap<PathBuf, HashSet<PathBuf>> =
            HashMap::from([(usef.clone(), HashSet::from([basic.clone(), voice.clone()]))]);
        let g = ProjectCallGraph::build(defs, &sources, &scope);
        let callees = |name: &str| -> Vec<String> {
            let id = g.id_of(&usef, name).expect(name);
            let mut out: Vec<String> = g
                .node(id)
                .callees
                .iter()
                .map(|&c| g.node(c).name.clone())
                .collect();
            out.sort();
            out
        };
        assert_eq!(callees("quad"), ["double"]);
        assert_eq!(callees("speak"), ["toModel"]);
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

    /// A name defined many times in one file is ambiguous: its calls link to
    /// none of the definitions. Linking all of them made a file of N
    /// same-named functions calling that name N² edges.
    #[test]
    fn a_name_defined_many_times_resolves_to_none() {
        const N: usize = 3_000;
        let src: String = (0..N).map(|_| "function f(){f()}\n").collect();
        let defs: Vec<_> = (0..N).map(|i| def("f", "/p/a.js", i + 1)).collect();
        let sources = vec![(PathBuf::from("/p/a.js"), src)];
        let started = std::time::Instant::now();
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(g.edge_count(), 0);
        // A small overload set still resolves.
        let defs = vec![
            def("caller", "/p/b.rs", 1),
            def("g", "/p/b.rs", 4),
            def("g", "/p/b.rs", 5),
        ];
        let sources = vec![(
            PathBuf::from("/p/b.rs"),
            "fn caller() {\n    g();\n}\nfn g() {}\nfn g() {}\n".to_string(),
        )];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.edge_count(), 2);
    }

    /// Real overload sets, and a crate root's modules each defining `new`,
    /// are not ambiguous: their calls link to every definition.
    #[test]
    fn an_overload_set_or_a_crate_roots_modules_resolve_to_every_definition() {
        // Twelve overloads of `of` in one file, Guava style.
        let mut defs = vec![def("caller", "/p/A.java", 1)];
        defs.extend((0..12).map(|i| def("of", "/p/A.java", i + 3)));
        let src = format!(
            "class A {{\n void caller() {{ of(); }}\n{}}}\n",
            "  void of() {}\n".repeat(12)
        );
        let sources = vec![(PathBuf::from("/p/A.java"), src)];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.edge_count(), 12);

        // `lib.rs` declares nine modules, each with a `new`.
        let mut defs = vec![def("start", "/p/lib.rs", 1)];
        let mut sources = vec![(
            PathBuf::from("/p/lib.rs"),
            "fn start() {\n    new();\n}\n".to_string(),
        )];
        let mut scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        for m in 0..9 {
            let file = format!("/p/m{m}.rs");
            defs.push(def("new", &file, 1));
            sources.push((PathBuf::from(&file), "fn new() {}\n".to_string()));
            scope
                .entry(PathBuf::from("/p/lib.rs"))
                .or_default()
                .insert(PathBuf::from(file));
        }
        let g = ProjectCallGraph::build(defs, &sources, &scope);
        assert_eq!(g.edge_count(), 9);
    }

    /// What a call resolves to is looked up, not searched for: many calls
    /// of a name defined in many files, or a file of many functions, cost
    /// neither calls × definitions nor calls × the file's functions.
    #[test]
    fn many_calls_of_a_common_name_resolve_at_once() {
        const FILES: usize = 3_000;
        const CALLS: usize = 3_000;
        let mut defs = Vec::new();
        let mut sources = Vec::new();
        for f in 0..FILES {
            let file = format!("/p/m{f}.rs");
            defs.push(def("new", &file, 1));
            sources.push((PathBuf::from(&file), "fn new() {}\n".to_string()));
        }
        let mut src = String::new();
        for c in 0..CALLS {
            defs.push(def(&format!("c{c}"), "/p/main.rs", c + 1));
            src.push_str(&format!("fn c{c}() {{ new(); }}\n"));
        }
        sources.push((PathBuf::from("/p/main.rs"), src));
        let started = std::time::Instant::now();
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(g.edge_count(), 0, "a name defined everywhere is no one's");
    }

    #[test]
    fn self_recursion_is_not_a_caller() {
        let defs = vec![def("fac", "/p/a.rs", 1)];
        let sources = vec![(
            PathBuf::from("/p/a.rs"),
            "fn fac(n: u64) -> u64 {\n    fac(n - 1)\n}\n".to_string(),
        )];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        // The self-call is no caller, so `fac` still counts as uncalled; it
        // is among what `fac` calls.
        assert_eq!(g.node(g.uncalled()[0]).name, "fac");
        assert_eq!(g.callees_of(0), [0]);
        assert!(g.callee_keys().values().all(Vec::is_empty));
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
        // a→b, a→c (explicit, e.g. LSP-resolved), plus b calling itself.
        let edges = HashSet::from([(0, 1), (0, 2), (1, 1)]);
        let g = ProjectCallGraph::from_callable_defs(defs, edges);
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.node(id_named(&g, "a")).callee_count(), 2);
        // b calls itself, and is no caller of its own: a is its one caller.
        let b = id_named(&g, "b");
        assert_eq!(g.node(b).caller_count(), 1);
        assert_eq!(g.callees_of(b), [b]);
        assert_eq!(g.node(id_named(&g, "c")).caller_count(), 1);
        assert_eq!(g.edge_count(), 3);
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

    #[test]
    fn methods_named_after_builtins_keep_their_project_edges() {
        let fixtures = [
            (
                "/p/reader.py",
                "class Reader:\n    def open(self):\n        pass\n    def run(self):\n        self.open()\n",
                "open",
            ),
            (
                "/p/client.ts",
                "class Client {\n  fetch() {}\n  run() { this.fetch(); }\n}\n",
                "fetch",
            ),
            (
                "/p/client.js",
                "class Client {\n  fetch() {}\n  run() { this.fetch(); }\n}\n",
                "fetch",
            ),
            (
                "/p/reader.go",
                "package p\ntype Reader struct {}\nfunc (r Reader) len() int { return 1 }\nfunc (r Reader) run() { r.len() }\n",
                "len",
            ),
        ];
        for (file, source, method) in fixtures {
            let g = graph(&[(file, source)]);
            assert!(
                has(&g, &format!("{file}:run"), &format!("{file}:{method}")),
                "{file}: {:?}",
                edges(&g)
            );
            assert_eq!(g.edge_count(), 1);
        }
    }

    #[test]
    fn rust_raw_calls_match_the_outline_spelling() {
        let file = "/p/raw.rs";
        let source = "fn r#type<T>() {}\nstruct Reader;\nimpl Reader {\n    fn r#match(&self) {}\n}\nfn r#loop() {\n    r#type::<()>();\n    crate::r#type::<()>();\n}\nfn main() {\n    r#loop();\n    Reader.r#match();\n}\n";
        let calls = calls_of(source, "rust");
        assert_eq!(calls.iter().filter(|c| c.callee == "r#type").count(), 2);
        assert!(calls.iter().any(|c| c.callee == "r#match" && c.method));
        let g = graph(&[(file, source)]);
        for (from, to) in [
            ("main", "r#loop"),
            ("r#loop", "r#type"),
            ("main", "r#match"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{:?}",
                edges(&g)
            );
        }
        assert_eq!(g.edge_count(), 3);
    }

    #[test]
    fn raw_and_plain_rust_spellings_resolve_to_the_same_item() {
        let file = "/p/spelling.rs";
        let source = "fn r#first() {}\nfn second() {}\nfn run() { first(); r#first(); second(); r#second(); }\n";
        let g = graph(&[(file, source)]);
        assert!(has(&g, "/p/spelling.rs:run", "/p/spelling.rs:r#first"));
        assert!(has(&g, "/p/spelling.rs:run", "/p/spelling.rs:second"));
        assert_eq!(g.edge_count(), 2, "{:?}", edges(&g));
    }

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

    /// A function only tests call is called — by each test, in every
    /// language whose tests are blocks a runner's call opens rather than
    /// functions: ExUnit's `test "adds" do`, RSpec's `it`, Zig's `test`
    /// (a declaration's too), MUnit's `test("adds") {`, Kotest's,
    /// GoogleTest's `TEST(Cart, Adds)`, Catch2's `TEST_CASE`, package:test's
    /// `test('adds', () {` (in `main`) and hspec's `it "adds" $` (in
    /// `spec`). Such a call had no caller — or `main`'s, `spec`'s — and the
    /// function read "No callers", listed as possibly dead. The tests call;
    /// none is called, nor counted among the functions, nor uncalled.
    #[test]
    fn a_test_block_is_the_caller_of_what_it_calls() {
        let cases: &[(&str, &str, &str, &str, &[&str])] = &[
            (
                "/p/lib/cart.ex",
                "defmodule Cart do\n  def add(a, b), do: a + b\nend\n",
                "/p/test/cart_test.exs",
                "defmodule CartTest do\n  use ExUnit.Case\n  describe \"cart\" do\n    test \"adds\" do\n      assert Cart.add(2, 3) == 5\n    end\n  end\n  test \"breaks\", %{x: _} do\n    Cart.add(1, 1)\n  end\nend\n",
                &["adds@4", "breaks@8"],
            ),
            (
                "/p/lib/cart.rb",
                "def add(a, b)\n  a + b\nend\n",
                "/p/spec/cart_spec.rb",
                "RSpec.describe Cart do\n  before { add(0, 0) }\n  it 'adds' do\n    expect(add(2, 3)).to eq(5)\n  end\nend\n",
                &["Cart@1", "adds@3"],
            ),
            (
                "/p/cart.zig",
                "fn add(a: i32, b: i32) i32 {\n    return a + b;\n}\n",
                "/p/cart_test.zig",
                "test \"adds\" {\n    _ = add(2, 3);\n}\ntest add {\n    _ = add(1, 1);\n}\n",
                &["add@4", "adds@1"],
            ),
            (
                "/p/src/main/scala/Cart.scala",
                "object Cart {\n  def add(a: Int, b: Int): Int = a + b\n}\n",
                "/p/src/test/scala/CartSuite.scala",
                "class CartSuite extends munit.FunSuite {\n  test(\"adds\") {\n    assertEquals(Cart.add(2, 3), 5)\n  }\n}\n",
                &["adds@2"],
            ),
            (
                "/p/src/main/kotlin/Cart.kt",
                "fun add(a: Int, b: Int): Int = a + b\n",
                "/p/src/test/kotlin/CartTest.kt",
                "class CartTest : FunSpec({\n  test(\"adds\") {\n    add(2, 3) shouldBe 5\n  }\n})\n",
                &["adds@2"],
            ),
            (
                "/p/cart.cpp",
                "int add(int a, int b) { return a + b; }\n",
                "/p/cart_test.cpp",
                "TEST(Cart, Adds) {\n  EXPECT_EQ(5, add(2, 3));\n}\nTEST_CASE(\"adds again\", \"[cart]\") {\n  REQUIRE(add(2, 2) == 4);\n}\n",
                &["Cart.Adds@1", "adds again@4"],
            ),
            (
                "/p/lib/cart.dart",
                "int add(int a, int b) => a + b;\n",
                "/p/test/cart_test.dart",
                "void main() {\n  group('cart', () {\n    test('adds', () {\n      expect(add(2, 3), 5);\n    });\n  });\n}\n",
                &["adds@3"],
            ),
            (
                "/p/src/Cart.hs",
                "module Cart where\nadd :: Int -> Int -> Int\nadd a b = a + b\n",
                "/p/test/CartSpec.hs",
                "module CartSpec where\nspec :: Spec\nspec = describe \"Cart\" $ do\n  it \"adds\" $ add 2 3 `shouldBe` 5\n",
                &["adds@4"],
            ),
        ];
        for &(lib, lib_src, test_file, test_src, want) in cases {
            // The test file reaches the library's, as the import graph has
            // it (an Elixir module named, a Scala object): what a qualified
            // call (`Cart.add`) resolves in.
            let files = [(lib, lib_src), (test_file, test_src)];
            let sources: Vec<(PathBuf, String)> = files
                .iter()
                .map(|(f, s)| (PathBuf::from(f), s.to_string()))
                .collect();
            let scope = HashMap::from([(
                PathBuf::from(test_file),
                HashSet::from([PathBuf::from(lib)]),
            )]);
            let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
            assert_eq!(callers_named(&g, lib, "add", 0), want, "{test_file}");
            let tests = (0..g.node_count()).filter(|&i| calls_only(&g.node(i).kind));
            for t in tests {
                assert!(
                    g.node(t).callers.is_empty(),
                    "{test_file}: {:?} called",
                    g.node(t)
                );
                assert!(
                    !g.uncalled().contains(&t),
                    "{test_file}: a test is uncalled"
                );
            }
            assert!(
                !g.uncalled()
                    .contains(&g.id_of(Path::new(lib), "add").unwrap()),
                "{test_file}: add is uncalled"
            );
            assert!(g.function_count() < g.node_count(), "{test_file}");
        }
    }

    /// An Elixir module's function is reached as `Cart.add(…)` and an
    /// Erlang module's as `cart:add(…)` from any module, with no import —
    /// by the module's full name, or an Elixir alias's last part — and is
    /// the function of that module, not a same-named one of another. No
    /// call of another module's function had a callee: a function only
    /// other modules call (a test module's every call) read "No callers".
    #[test]
    fn a_modules_function_is_reached_by_the_modules_name() {
        let g = graph(&[
            (
                "/p/lib/shop/cart.ex",
                "defmodule Shop.Cart do\n  def add(a, b), do: a + b\n  defmodule Line do\n    def add(l), do: l\n  end\nend\n",
            ),
            (
                "/p/lib/other.ex",
                "defmodule Other do\n  def add(a, b), do: a - b\nend\n",
            ),
            (
                "/p/test/cart_test.exs",
                "defmodule CartTest do\n  alias Shop.Cart\n  test \"adds\" do\n    Cart.add(2, 3)\n  end\n  def line, do: Shop.Cart.Line.add(1)\nend\n",
            ),
        ]);
        assert_eq!(
            callers_named(&g, "/p/lib/shop/cart.ex", "add", 0),
            ["adds@3"]
        );
        assert_eq!(
            callers_named(&g, "/p/lib/shop/cart.ex", "add", 1),
            ["line@6"]
        );
        assert!(callers_named(&g, "/p/lib/other.ex", "add", 0).is_empty());

        let g = graph(&[
            (
                "/p/src/cart.erl",
                "-module(cart).\n-export([add/2]).\nadd(A, B) -> A + B.\n",
            ),
            ("/p/src/other.erl", "-module(other).\nadd(A, B) -> A - B.\n"),
            (
                "/p/test/cart_tests.erl",
                "-module(cart_tests).\nadd_test() -> 5 = cart:add(2, 3).\n",
            ),
        ]);
        assert_eq!(
            callers_named(&g, "/p/src/cart.erl", "add", 0),
            ["add_test@2"]
        );
        assert!(callers_named(&g, "/p/src/other.erl", "add", 0).is_empty());
    }

    /// A class's static function is reached by the class's name from the
    /// test beside it with no import, as the language reaches it: Java's,
    /// Kotlin's and Scala's in the same package, C#'s in the same
    /// namespace, Ruby's once required — Scala's from a test block, whose
    /// calls had no receiver, being in no function. Each had no callee.
    #[test]
    fn a_types_function_is_reached_by_the_types_name_from_beside_it() {
        // The library's file, its source and its function's name; the
        // test's file and source; who calls the function.
        type Case<'a> = (&'a str, &'a str, &'a str, &'a str, &'a str, &'a [&'a str]);
        let cases: &[Case] = &[
            (
                "/p/src/main/java/com/acme/Cart.java",
                "package com.acme;\npublic class Cart {\n    public static int add(int a, int b) { return a + b; }\n}\n",
                "add",
                "/p/src/test/java/com/acme/CartTest.java",
                "package com.acme;\nclass CartTest {\n    @Test\n    void adds() {\n        assertEquals(5, Cart.add(2, 3));\n    }\n}\n",
                &["adds@3"],
            ),
            (
                "/p/src/main/scala/shop/Cart.scala",
                "package shop\n\nobject Cart:\n  def total(items: List[Int]): Int =\n    items.sum\n",
                "total",
                "/p/src/test/scala/shop/CartSuite.scala",
                "package shop\n\nclass CartSuite extends munit.FunSuite:\n  test(\"adds\") {\n    assertEquals(Cart.total(List(2, 3)), 5)\n  }\n",
                &["adds@4"],
            ),
            (
                "/p/src/main/kotlin/shop/Cart.kt",
                "package shop\nobject Cart {\n    fun add(a: Int, b: Int): Int = a + b\n}\n",
                "add",
                "/p/src/test/kotlin/shop/CartTest.kt",
                "package shop\nclass CartTest {\n    @Test\n    fun adds() {\n        assertEquals(5, Cart.add(2, 3))\n    }\n}\n",
                &["adds@3"],
            ),
            (
                "/p/Cart.cs",
                "namespace Shop;\npublic static class Cart {\n  public static int Add(int a, int b) => a + b;\n}\n",
                "Add",
                "/p/Tests/CartTests.cs",
                "namespace Shop.Tests;\npublic class CartTests {\n  [Fact]\n  public void Adds() { Assert.Equal(5, Cart.Add(2, 3)); }\n}\n",
                &["Adds@3"],
            ),
            (
                "/p/lib/cart.rb",
                "class Cart\n  def self.add(a, b) = a + b\nend\n",
                "add",
                "/p/test/cart_test.rb",
                "class CartTest < Minitest::Test\n  def test_adds\n    assert_equal 5, Cart.add(2, 3)\n  end\nend\n",
                &["test_adds@2"],
            ),
        ];
        for &(lib, lib_src, name, test_file, test_src, want) in cases {
            let other = lib.replace("Cart", "Other").replace("cart", "other");
            let other_src = lib_src.replace("Cart", "Other");
            let g = graph(&[(lib, lib_src), (&other, &other_src), (test_file, test_src)]);
            assert_eq!(callers_named(&g, lib, name, 0), want, "{test_file}");
            assert!(
                callers_named(&g, &other, name, 0).is_empty(),
                "{test_file}: another type's {name}"
            );
        }
    }

    /// A function passed as a value is used where it is passed: Haskell's
    /// `foldr add 0 xs` and `show . add 1 <$> ys`, Erlang's `fun add/2`
    /// and `fun cart:add/2`, Elixir's `&add/2`. Only applications counted,
    /// a Haskell program's `main` reached nothing it folded with, and the
    /// function read as reached from no entry point. A function named as
    /// an application's function is that application's call, not two.
    /// A Haskell operator of the project's is called where it is applied
    /// — infix (`1 <+> 2`), in parentheses (`(<+>) 3 4`) or passed
    /// (`foldr (<+>) 0`) — and a function between backticks (``x `plus`
    /// y``) where it stands. An operator's own definition, `a <+> b = …`,
    /// calls nothing by its head, and the base library's operators (`+`,
    /// `$`) are no call of the project's.
    #[test]
    fn a_haskell_operator_and_a_backticked_function_are_called() {
        let src = "\
module Ops where

(<+>) :: Int -> Int -> Int
a <+> b = a + b

x `plus` y = x + y

infixed = 1 <+> 2
prefixed = (<+>) 3 4
folded xs = foldr (<+>) 0 xs
ticked = 5 `plus` 6
base = show $ 1 + 2
";
        let g = graph(&[("/p/src/Ops.hs", src)]);
        let edges = edges(&g);
        for from in ["infixed", "prefixed", "folded"] {
            assert!(
                has(&g, &format!("/p/src/Ops.hs:{from}"), "/p/src/Ops.hs:<+>"),
                "{from} -> <+>: {edges:?}"
            );
        }
        assert!(
            has(&g, "/p/src/Ops.hs:ticked", "/p/src/Ops.hs:plus"),
            "{edges:?}"
        );
        assert!(
            !has(&g, "/p/src/Ops.hs:<+>", "/p/src/Ops.hs:<+>"),
            "a definition's head is no call: {edges:?}"
        );
        assert!(
            edges.iter().all(|(from, _)| !from.ends_with(":base")),
            "{edges:?}"
        );
    }

    #[test]
    fn a_function_passed_as_a_value_is_used_where_it_is_passed() {
        let g = graph(&[
            (
                "/p/src/Cart.hs",
                "module Cart where\nadd :: Int -> Int -> Int\nadd a b = a + b\ndouble :: Int -> Int\ndouble = (* 2)\n",
            ),
            (
                "/p/app/Main.hs",
                "module Main where\nimport Cart\nmain :: IO ()\nmain = do\n  print (foldr add 0 [1, 2])\n  print $ double . add 1 <$> [3]\n",
            ),
        ]);
        assert_eq!(callers_named(&g, "/p/src/Cart.hs", "add", 0), ["main@4"]);
        assert_eq!(callers_named(&g, "/p/src/Cart.hs", "double", 0), ["main@4"]);
        let calls = calls_of("main = print (add 1 2)\n", "haskell");
        assert_eq!(
            calls.iter().filter(|c| c.callee == "add").count(),
            1,
            "{calls:?}"
        );

        let g = graph(&[
            (
                "/p/src/cart.erl",
                "-module(cart).\n-export([add/2]).\nadd(A, B) -> A + B.\n",
            ),
            (
                "/p/src/shop.erl",
                "-module(shop).\ntotal(Xs) -> lists:foldl(fun cart:add/2, 0, Xs).\nsum(Xs) -> lists:foldl(fun plus/2, 0, Xs).\nplus(A, B) -> A + B.\n",
            ),
        ]);
        assert_eq!(callers_named(&g, "/p/src/cart.erl", "add", 0), ["total@2"]);
        assert_eq!(callers_named(&g, "/p/src/shop.erl", "plus", 0), ["sum@3"]);

        let g = graph(&[(
            "/p/lib/cart.ex",
            "defmodule Cart do\n  def add(a), do: a + 1\n  def all(xs), do: Enum.map(xs, &add/1)\nend\n",
        )]);
        assert_eq!(callers_named(&g, "/p/lib/cart.ex", "add", 0), ["all@3"]);
    }

    /// A test titled as a function is named is no call's target, takes no
    /// key of the function's, and is no `callee_keys` entry over it: in
    /// one file, `function adds` and `it('adds', …)`.
    #[test]
    fn a_test_titled_as_a_function_is_not_that_function() {
        let src = "it('adds', () => {\n  adds(1);\n});\nfunction adds(n) {\n  return n + 1;\n}\nfunction other() {\n  adds(2);\n}\n";
        let g = graph(&[("/p/a.test.js", src)]);
        let id = g.id_of(Path::new("/p/a.test.js"), "adds").unwrap();
        assert_eq!(g.node(id).kind, "function");
        assert_eq!(
            callers_named(&g, "/p/a.test.js", "adds", 0),
            ["adds@1", "other@7"]
        );
        let keys = g.callee_keys();
        let key = g.key_of(id).unwrap();
        assert!(keys[&key].is_empty(), "{keys:?}");
    }

    /// An Elixir function's head (`add(a, b)` of `def add(a, b) do`, a
    /// guarded one, a `defp`'s) defines it: no call. Every function called
    /// itself, and listed itself among what it calls.
    #[test]
    fn an_elixir_functions_head_is_no_call_of_it() {
        let src = "defmodule Cart do\n  def add(a, b), do: a + b\n  defp twice(x) when x > 0 do\n    add(x, x)\n  end\n  def count(n) do\n    count(n - 1)\n  end\nend\n";
        let mut sites = calls_of(src, "elixir");
        sites.sort_by_key(|c| c.line);
        let calls: Vec<(Option<String>, String)> = sites
            .into_iter()
            .filter(|c| c.callee != "defmodule")
            .map(|c| (c.caller, c.callee))
            .collect();
        assert_eq!(
            calls,
            [
                (Some("twice".to_string()), "add".to_string()),
                (Some("count".to_string()), "count".to_string()),
            ],
            "only the calls in the bodies, a recursion included"
        );
    }

    /// `f(a)(b)` calls what `f(a)` returned: no function of a name. MUnit's
    /// `test("adds") { … }` read as a call of `adds`, its title.
    #[test]
    fn a_call_of_what_a_call_returned_names_no_function() {
        let calls = calls_of(
            "class S extends munit.FunSuite {\n  test(\"adds\") {\n    run()\n  }\n}\n",
            "scala",
        );
        assert!(!calls.iter().any(|c| c.callee == "adds"), "{calls:?}");
        assert_eq!(
            calls.iter().filter(|c| c.callee == "test").count(),
            1,
            "{calls:?}"
        );
        let js = calls_of("make(1)(2);\n", "javascript");
        assert_eq!(
            js.iter().map(|c| c.callee.as_str()).collect::<Vec<_>>(),
            ["make"]
        );
    }

    /// Who calls the `ordinal`th `name` of `file`, as `caller@line`.
    fn callers_named(g: &ProjectCallGraph, file: &str, name: &str, ordinal: u32) -> Vec<String> {
        let id = g
            .id_of_key(Path::new(file), name, ordinal)
            .unwrap_or_else(|| panic!("no {name} in {file}"));
        let mut out: Vec<String> = g
            .callers_of(id)
            .iter()
            .map(|&c| format!("{}@{}", g.node(c).name, g.node(c).line))
            .collect();
        out.sort();
        out
    }

    /// Calling a Python class runs its `__init__`: `Inventory()` and
    /// `raise OutOfStock(...)` call them, of a class the file defines or
    /// imports; a class of the same name elsewhere is not the one called.
    /// A property is read, not called: it is kept apart.
    #[test]
    fn calling_a_python_class_calls_its_init() {
        let shop = "\
class OutOfStock(Exception):
    def __init__(self, tea):
        self.tea = tea

class Inventory:
    def __init__(self):
        self.tins = {}

    @property
    def count(self):
        return len(self.tins)

    def take(self, tea):
        if tea not in self.tins:
            raise OutOfStock(tea)
";
        let main = "\
from shop import Inventory

def main():
    inventory = Inventory()
    inventory.take('sencha')
";
        let other = "\
class Inventory:
    def __init__(self):
        pass
";
        let files = [
            ("/p/shop.py", shop),
            ("/p/main.py", main),
            ("/p/other.py", other),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let scope = HashMap::from([(
            PathBuf::from("/p/main.py"),
            HashSet::from([PathBuf::from("/p/shop.py")]),
        )]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        assert_eq!(callers_named(&g, "/p/shop.py", "__init__", 0), ["take@13"]);
        assert_eq!(callers_named(&g, "/p/shop.py", "__init__", 1), ["main@3"]);
        assert!(callers_named(&g, "/p/other.py", "__init__", 0).is_empty());
        let calls = FileCalls::read(Path::new("/p/shop.py"), shop).unwrap();
        assert_eq!(calls.accessed, HashSet::from([10]));
    }

    /// A method called on a receiver whose type the caller states links to
    /// that type's method, not every method of the name: `main` builds a
    /// `Cart` and calls its `total`, and `Item.total` was listed as called
    /// by `main`. `self.f()` is the caller's own class's `f`. A receiver
    /// whose type is not known (a loop variable) still links to them all.
    #[test]
    fn a_typed_receiver_calls_its_own_types_method() {
        let cart = "\
class Item:
    def total(self):
        return 1

class Cart:
    def subtotal(self):
        return sum(item.total() for item in self.items)

    def total(self):
        return self.subtotal()
";
        let main = "\
from cart import Cart

def main():
    cart = Cart()
    cart.total()
";
        let files = [("/p/cart.py", cart), ("/p/main.py", main)];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        // `main.py` imports `cart.py`.
        let scope = HashMap::from([(
            PathBuf::from("/p/main.py"),
            HashSet::from([PathBuf::from("/p/cart.py")]),
        )]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        // `Item.total` is the file's first `total`, `Cart.total` its second.
        assert_eq!(callers_named(&g, "/p/cart.py", "total", 0), ["subtotal@6"]);
        assert_eq!(
            callers_named(&g, "/p/cart.py", "total", 1),
            ["main@3", "subtotal@6"],
            "`item` has no stated type, so either total may be the one"
        );
        assert_eq!(callers_named(&g, "/p/cart.py", "subtotal", 0), ["total@9"]);
    }

    /// The class each method belongs to, however the language writes it:
    /// inside its class, in a Rust `impl`, by a Go receiver, or out of its
    /// C++ class as `Cart::total`.
    #[test]
    fn each_language_says_whose_method_it_is() {
        let owners = |file: &str, src: &str| {
            let call = FileCalls::read(Path::new(file), src).expect(file);
            let mut owners: Vec<(usize, String)> = call.owners.into_iter().collect();
            owners.sort();
            owners
        };
        let cart = |line: usize| vec![(line, "Cart".to_string())];
        assert_eq!(
            owners(
                "a.py",
                "class Cart:\n    def total(self):\n        pass\n\ndef main():\n    pass\n"
            ),
            cart(2)
        );
        assert_eq!(
            owners(
                "a.rs",
                "struct Cart;\nimpl Cart {\n    fn total(&self) {}\n}\nfn main() {}\n"
            ),
            cart(3)
        );
        assert_eq!(
            owners(
                "a.rs",
                "struct Cart<T>(T);\nimpl<T> Summed for Cart<T> {\n    fn total(&self) {}\n}\n"
            ),
            cart(3)
        );
        assert_eq!(
            owners(
                "a.go",
                "package m\ntype Cart struct{}\nfunc (c *Cart) Total() int { return 1 }\nfunc main() {}\n"
            ),
            cart(3)
        );
        assert_eq!(
            owners(
                "a.ts",
                "class Cart {\n  total() { return 1 }\n}\nfunction main() {}\n"
            ),
            cart(2)
        );
        assert_eq!(
            owners("A.java", "class Cart {\n  int total() { return 1; }\n}\n"),
            cart(2)
        );
        assert_eq!(
            owners("a.rb", "class Cart\n  def total\n    1\n  end\nend\n"),
            cart(2)
        );
        assert_eq!(
            owners("a.cs", "class Cart {\n  int Total() { return 1; }\n}\n"),
            cart(2)
        );
        assert_eq!(
            owners(
                "a.php",
                "<?php\nclass Cart {\n  function total() { return 1; }\n}\n"
            ),
            cart(3)
        );
        assert_eq!(
            owners(
                "a.cpp",
                "class Cart {\n public:\n  int total() { return 1; }\n};\nint Cart::other() { return 2; }\n"
            ),
            vec![(3, "Cart".to_string()), (5, "Cart".to_string())]
        );
        // A trait's own methods are any implementation's: no owner.
        assert_eq!(
            owners(
                "b.rs",
                "trait Summed {\n    fn total(&self) -> u32 { 0 }\n}\n"
            ),
            vec![]
        );
    }

    /// What a receiver is known to be, as each language binds it.
    #[test]
    fn receivers_are_typed_as_their_bindings_say() {
        let receiver = |file: &str, src: &str, callee: &str| {
            let calls = FileCalls::read(Path::new(file), src).expect(file).calls;
            calls
                .into_iter()
                .find(|c| c.callee == callee)
                .unwrap_or_else(|| panic!("no call of {callee} in {file}"))
                .receiver
        };
        let cart = Some(Receiver::Type("Cart".into()));
        assert_eq!(
            receiver("a.py", "def f():\n    c = Cart()\n    c.total()\n", "total"),
            cart
        );
        // Declared or annotated, or the class's own: it may be a subclass,
        // whose override is the method called — the type's method and its
        // subtypes' overrides, not one known exactly.
        let declared = Some(Receiver::Declared("Cart".into()));
        assert_eq!(
            receiver("a.py", "def f(c: Cart):\n    c.total()\n", "total"),
            declared
        );
        assert_eq!(
            receiver(
                "a.py",
                "class A:\n    def f(self):\n        self.total()\n",
                "total"
            ),
            Some(Receiver::Own)
        );
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  void f() {\n    Cart c = make();\n    c.total();\n  }\n}\n",
                "total"
            ),
            declared
        );
        // A value a type's factory makes, as that type: Scala's `IO.pure`,
        // Kotlin's, Java's, C#'s; an annotation says otherwise first.
        let io = Some(Receiver::Declared("IO".into()));
        assert_eq!(
            receiver(
                "a.scala",
                "def f() = {\n  val io = IO.pure(1)\n  io.flatMap(g)\n}\n",
                "flatMap"
            ),
            io
        );
        assert_eq!(
            receiver(
                "a.scala",
                "def f() = {\n  val io = IO.unit\n  io.flatMap(g)\n}\n",
                "flatMap"
            ),
            io
        );
        assert_eq!(
            receiver(
                "a.kt",
                "fun f() {\n  val c = Cart.create()\n  c.total()\n}\n",
                "total"
            ),
            declared
        );
        assert_eq!(
            receiver(
                "a.kt",
                "fun f() {\n  val c: Order = Cart.create()\n  c.total()\n}\n",
                "total"
            ),
            Some(Receiver::Declared("Order".into()))
        );
        assert_eq!(
            receiver(
                "A.cs",
                "class A {\n  void F() {\n    var c = Cart.Create();\n    c.Total();\n  }\n}\n",
                "Total"
            ),
            declared
        );
        // A field, by what its class declares it as: Java's, Kotlin's
        // constructor property, TypeScript's parameter property; with
        // `this.` or bare.
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  private final Cart cart;\n  void f() {\n    this.cart.total();\n  }\n}\n",
                "total"
            ),
            declared
        );
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  private final Cart cart;\n  void f() {\n    cart.total();\n  }\n}\n",
                "total"
            ),
            declared
        );
        assert_eq!(
            receiver(
                "a.kt",
                "class A(private val cart: Cart) {\n  fun f() {\n    cart.total()\n  }\n}\n",
                "total"
            ),
            declared
        );
        assert_eq!(
            receiver(
                "a.ts",
                "class A {\n  constructor(private cart: Cart) {}\n  f() {\n    this.cart.total();\n  }\n}\n",
                "total"
            ),
            declared
        );
        // In Rust, with no inheritance, the impl's own type.
        assert_eq!(
            receiver(
                "a.rs",
                "struct A;\nimpl A {\n    fn f(&self) {\n        self.total();\n    }\n}\n",
                "total"
            ),
            Some(Receiver::Own)
        );
        assert_eq!(
            receiver(
                "a.rs",
                "fn f() {\n    let c = Cart::new();\n    c.total();\n}\n",
                "total"
            ),
            cart
        );
        assert_eq!(
            receiver("a.rs", "fn f(c: &mut Cart) {\n    c.total();\n}\n", "total"),
            cart
        );
        // A trait object is any type implementing it.
        assert_eq!(
            receiver("a.rs", "fn f(c: &dyn Cart) {\n    c.total();\n}\n", "total"),
            None
        );
        assert_eq!(
            receiver(
                "a.go",
                "package m\nfunc f() {\n\tc := &Cart{}\n\tc.Total()\n}\n",
                "Total"
            ),
            cart
        );
        assert_eq!(
            receiver(
                "a.go",
                "package m\nfunc (c *Cart) f() {\n\tc.Total()\n}\n",
                "Total"
            ),
            cart
        );
        assert_eq!(
            receiver(
                "a.ts",
                "function f() {\n  const c = new Cart();\n  c.total();\n}\n",
                "total"
            ),
            cart
        );
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  void f() {\n    Cart c = new Cart();\n    c.total();\n  }\n}\n",
                "total"
            ),
            cart
        );
        assert_eq!(
            receiver("a.rb", "def f\n  c = Cart.new\n  c.total\nend\n", "total"),
            cart
        );
        assert_eq!(
            receiver(
                "a.php",
                "<?php\nfunction f() {\n  $c = new Cart();\n  $c->total();\n}\n",
                "total"
            ),
            cart
        );
        assert_eq!(
            receiver(
                "a.cs",
                "class A {\n  void F() {\n    var c = new Cart();\n    c.Total();\n  }\n}\n",
                "Total"
            ),
            cart
        );
        // A type called on itself.
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  void f() {\n    Cart.total();\n  }\n}\n",
                "total"
            ),
            cart
        );
        // Unknown: no binding, a function's result, an expression, two
        // bindings that disagree, a Go function that builds one.
        assert_eq!(
            receiver("a.py", "def f(c):\n    c.total()\n", "total"),
            None
        );
        assert_eq!(
            receiver("a.py", "def f():\n    c = make()\n    c.total()\n", "total"),
            None
        );
        assert_eq!(
            receiver("a.py", "def f(self):\n    self.c.total()\n", "total"),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(x):\n    c = Cart()\n    c = Item()\n    c.total()\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.go",
                "package m\nfunc f() {\n\tc := newCart()\n\tc.Total()\n}\n",
                "Total"
            ),
            None
        );
        // What a type's function other than `new` returns, a variant of an
        // enum, and what is called off a value built: of types of their own.
        assert_eq!(
            receiver(
                "a.rs",
                "fn f() {\n    let b = Config::builder();\n    b.port(80);\n}\n",
                "port"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.rs",
                "fn f(c: Circle) {\n    let s = Shape::Circle(c);\n    s.area();\n}\n",
                "area"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(p):\n    x = Config(p).load()\n    x.run()\n",
                "run"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.ts",
                "function f(p) {\n  const x = new Config(p).load();\n  x.run();\n}\n",
                "run"
            ),
            None
        );
        // Built once, then bound again to what cannot be told: a function's
        // result, a closure's parameter, a loop's.
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    s = Shape()\n    s = make_circle()\n    s.area()\n",
                "area"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.ts",
                "function f(orders) {\n  const cart = new Cart();\n  orders.forEach(cart => cart.total());\n}\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(carts):\n    c = Cart()\n    for c in carts:\n        c.total()\n",
                "total"
            ),
            None
        );
    }

    /// A call is told what the bindings before it say, not those after it,
    /// whichever of the function's calls is read first; and a name bound in
    /// a function nested in the caller is that function's own.
    #[test]
    fn a_receiver_is_typed_by_the_bindings_before_its_call() {
        let receivers = |file: &str, src: &str, callee: &str| {
            let mut calls: Vec<CallSite> = FileCalls::read(Path::new(file), src)
                .expect(file)
                .calls
                .into_iter()
                .filter(|c| c.callee == callee)
                .collect();
            calls.sort_by_key(|c| c.line);
            calls.into_iter().map(|c| c.receiver).collect::<Vec<_>>()
        };
        let item = Some(Receiver::Type("Item".into()));
        assert_eq!(
            receivers(
                "a.py",
                "def f(c):\n    c.total()\n    c = Item()\n    c.total()\n",
                "total"
            ),
            [None, item.clone()]
        );
        assert_eq!(
            receivers(
                "a.py",
                "def f(c):\n    def h():\n        c = Item()\n    c.total()\n",
                "total"
            ),
            [None]
        );
    }

    /// A receiver's bindings are read where they are: a closure's captured
    /// name as the function around it binds it; an assignment in a nested
    /// function leaves the name untold, a declaration there is its own; a
    /// binding is in effect after its statement; a pattern's names, a
    /// keyword argument, a parameter's default, a comment are what they are
    /// — and a character of several bytes before a name breaks nothing.
    #[test]
    fn a_receivers_bindings_are_read_where_they_are() {
        let receiver = |file: &str, src: &str, callee: &str| {
            let calls = FileCalls::read(Path::new(file), src).expect(file).calls;
            calls
                .into_iter()
                .find(|c| c.callee == callee)
                .unwrap_or_else(|| panic!("no call of {callee} in {file}"))
                .receiver
        };
        let typed = |ty: &str| Some(Receiver::Type(ty.into()));
        // The line before ends in a character of several bytes.
        assert_eq!(
            receiver(
                "a.py",
                "def load(path):\n    # 读取配置文件。\n    cfg = read(path)\n    cfg.validate()\n",
                "validate"
            ),
            None
        );
        // Captured by a closure.
        assert_eq!(
            receiver(
                "a.js",
                "function f() {\n  const store = new Store();\n  useEffect(() => store.load());\n}\n",
                "load"
            ),
            typed("Store")
        );
        // ...but bound again after it: what the closure sees when it runs.
        assert_eq!(
            receiver(
                "a.js",
                "function f() {\n  let store = new Store();\n  const cb = () => store.load();\n  store = make();\n}\n",
                "load"
            ),
            None
        );
        // Assigned in a nested function; declared there, its own.
        assert_eq!(
            receiver(
                "a.js",
                "function f() {\n  let c = new Cart();\n  reset(() => { c = new Item(); });\n  c.total();\n}\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.js",
                "function f() {\n  const c = new Cart();\n  reset(() => { const c = new Item(); });\n  c.total();\n}\n",
                "total"
            ),
            typed("Cart")
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    c = Cart()\n    def g():\n        c = Item()\n    c.total()\n",
                "total"
            ),
            typed("Cart")
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    c = Cart()\n    def g():\n        nonlocal c\n        c = Item()\n    c.total()\n",
                "total"
            ),
            None
        );
        // In the value of its own binding, the name is still the one before.
        assert_eq!(
            receiver(
                "a.py",
                "def wrap(item):\n    item = Wrapper(item.unwrap())\n",
                "unwrap"
            ),
            None
        );
        // One of a pattern's names.
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    c = Cart()\n    c, rest = split()\n    c.total()\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(xs):\n    c = Cart()\n    for i, c in enumerate(xs):\n        c.total()\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.rs",
                "fn f(x: Option<Item>) {\n    let c = Cart::new();\n    if let Some(c) = x {\n        c.total();\n    }\n}\n",
                "total"
            ),
            None
        );
        // A comment, a keyword argument, a parameter's default.
        assert_eq!(
            receiver(
                "a.py",
                "def f(c):\n    # c = Cart() usually\n    c.total()\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(c):\n    show(c=Cart())\n    c.total()\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver("a.py", "def f(c=Cart()):\n    c.total()\n", "total"),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    c = Cart()\n    show(c=c)\n    c.total()\n",
                "total"
            ),
            typed("Cart")
        );
    }

    /// A call tree-sitter recovered from half-typed code can name nothing:
    /// no receiver is looked for, where every position matched the empty
    /// name and the search never moved on.
    #[test]
    fn an_empty_callee_has_no_receiver() {
        assert_eq!(receiver_text("x.(y)", ""), None);
        assert_eq!(receiver_text("cart.total()", "total"), Some("cart"));
    }

    /// A method that overrides another is called where its base's is, on
    /// `self` or on a value typed as the base: the base's caller is the
    /// override's too.
    #[test]
    fn an_override_keeps_its_bases_callers() {
        let src = "\
class Shape:
    def describe(self):
        return self.area()

    def area(self):
        raise NotImplementedError

class Circle(Shape):
    def area(self):
        return 3.14

def show(s: Shape):
    s.area()
";
        let files = [("/p/shapes.py", src)];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &HashMap::new());
        // `Shape.area` is the file's first `area`, `Circle.area` its second.
        assert_eq!(
            callers_named(&g, "/p/shapes.py", "area", 1),
            ["describe@2", "show@12"]
        );
    }

    /// The bindings of a receiver are read once in a function, however many
    /// calls are made on it: read again for every call, a long generated
    /// function took the time of its length times its calls.
    #[test]
    fn a_long_functions_receivers_are_read_once() {
        let mut src = String::from("def f():\n    r = Runner()\n");
        for _ in 0..20_000 {
            src.push_str("    r.step(a, b, c)\n");
        }
        let started = std::time::Instant::now();
        let calls: Vec<CallSite> = FileCalls::read(Path::new("gen.py"), &src)
            .unwrap()
            .calls
            .into_iter()
            .filter(|c| c.callee == "step")
            .collect();
        let took = started.elapsed();
        assert_eq!(calls.len(), 20_000);
        assert!(
            calls
                .iter()
                .all(|c| c.receiver == Some(Receiver::Type("Runner".into())))
        );
        assert!(took < std::time::Duration::from_secs(5), "{took:?}");
    }

    /// A known type with no method of the name calls the one it inherits,
    /// not another class's of the name.
    #[test]
    fn a_type_without_the_method_calls_the_one_it_inherits() {
        let src = "\
class Base:
    def run(self):
        pass

class Other:
    def run(self):
        pass

class Sub(Base):
    pass

def main():
    s = Sub()
    s.run()
";
        let g = graph(&[("/p/a.py", src)]);
        assert_eq!(callers_named(&g, "/p/a.py", "run", 0), ["main@12"]);
        assert!(callers_named(&g, "/p/a.py", "run", 1).is_empty());
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
            let of = FileCalls::of(PathBuf::from(file), a.lang, a.calls, &a.symbols, a.owners);
            let read = FileCalls::read(Path::new(file), src);
            assert_eq!(
                of.as_ref()
                    .map(|c| (&c.file, c.lang, &c.calls, &c.declarations, &c.owners)),
                read.as_ref()
                    .map(|c| (&c.file, c.lang, &c.calls, &c.declarations, &c.owners)),
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

    /// A Go package's files call one another with no import: a method on a
    /// parameter's type, defined in the package's other file, is the call's.
    #[test]
    fn a_go_package_calls_across_its_files() {
        let g = graph(&[
            (
                "/p/context.go",
                "package gin\n\ntype Context struct{}\n\nfunc (c *Context) AbortWithStatus(code int) {}\n",
            ),
            (
                "/p/auth.go",
                "package gin\n\nfunc BasicAuth() HandlerFunc {\n\treturn func(c *Context) {\n\t\tc.AbortWithStatus(401)\n\t}\n}\n",
            ),
            (
                "/p/other/ctx.go",
                "package other\n\ntype Context struct{}\n\nfunc (c *Context) AbortWithStatus(code int) {}\n",
            ),
        ]);
        assert!(
            has(&g, "/p/auth.go:BasicAuth", "/p/context.go:AbortWithStatus"),
            "{:?}",
            edges(&g)
        );
        assert!(!has(
            &g,
            "/p/auth.go:BasicAuth",
            "/p/other/ctx.go:AbortWithStatus"
        ));
    }

    /// Java's classes of one package name one another with no import, and a
    /// value is what it is declared as — a field, a parameter, a local — or
    /// a subtype: `this.owners.findById(…)` on an `OwnerRepository` field
    /// calls the class that implements it; `owner.getLastName()` on an
    /// `Owner` the `Person` method it inherits; a bare `getLastName()` in
    /// `Owner` the same. A value of a library's type (`List`) calls none of
    /// the project's methods of the name.
    #[test]
    fn java_calls_resolve_by_package_and_declared_type() {
        let g = graph(&[
            (
                "/p/model/Person.java",
                "package app.model;\n\npublic class Person {\n  public String getLastName() { return \"\"; }\n}\n",
            ),
            (
                "/p/owner/Owner.java",
                "package app.owner;\n\nimport app.model.Person;\n\npublic class Owner extends Person {\n  public String label() { return getLastName(); }\n  public void add(Object o) {}\n}\n",
            ),
            (
                "/p/owner/OwnerRepository.java",
                "package app.owner;\n\npublic interface OwnerRepository {\n  Owner findById(int id);\n}\n",
            ),
            (
                "/p/owner/JpaOwners.java",
                "package app.owner;\n\nclass JpaOwners implements OwnerRepository {\n  public Owner findById(int id) { return null; }\n}\n",
            ),
            (
                "/p/owner/OwnerController.java",
                "package app.owner;\n\nimport java.util.List;\n\nclass OwnerController {\n  private final OwnerRepository owners;\n  String show(int id, Owner owner) {\n    this.owners.findById(id);\n    List<String> names = List.of();\n    names.add(\"x\");\n    return owner.getLastName();\n  }\n}\n",
            ),
        ]);
        let show = "/p/owner/OwnerController.java:show";
        assert!(
            has(&g, show, "/p/owner/JpaOwners.java:findById"),
            "{:?}",
            edges(&g)
        );
        assert!(
            !has(&g, show, "/p/owner/OwnerRepository.java:findById"),
            "the definition, not the interface's declaration"
        );
        assert!(has(&g, show, "/p/model/Person.java:getLastName"));
        assert!(!has(&g, show, "/p/owner/Owner.java:add"), "a List's add");
        assert!(has(
            &g,
            "/p/owner/Owner.java:label",
            "/p/model/Person.java:getLastName"
        ));
    }

    /// A Dart constructor is a definition, with a body or none, plain,
    /// `const`, named or `factory`: `CounterCubit()` calls it, from a file
    /// that imports its class's.
    #[test]
    fn dart_constructors_are_called() {
        let src = "class Cart {\n  const Cart({this.items = const []});\n  Cart.named() : items = const [];\n  factory Cart.empty() = _EmptyCart;\n  final List<int> items;\n}\nclass CounterCubit extends Cubit<int> {\n  CounterCubit() : super(0);\n}\n";
        let names: Vec<String> = crate::outline::extract(src, "dart")
            .into_iter()
            .filter(|s| s.kind == "method")
            .map(|s| format!("{}@{}", s.name, s.line))
            .collect();
        assert!(names.contains(&"Cart@2".to_string()), "{names:?}");
        assert!(names.contains(&"named@3".to_string()), "{names:?}");
        assert!(names.contains(&"empty@4".to_string()), "{names:?}");
        assert!(names.contains(&"CounterCubit@8".to_string()), "{names:?}");
        let g = graph(&[
            ("/p/lib/counter_cubit.dart", src),
            (
                "/p/lib/page.dart",
                "import 'counter_cubit.dart';\nWidget build() {\n  final c = CounterCubit();\n  return Cart.named();\n}\n",
            ),
        ]);
        assert!(
            has(
                &g,
                "/p/lib/page.dart:build",
                "/p/lib/counter_cubit.dart:CounterCubit"
            ),
            "{:?}",
            edges(&g)
        );
        assert!(has(
            &g,
            "/p/lib/page.dart:build",
            "/p/lib/counter_cubit.dart:named"
        ));
    }

    /// Scala calls a method with no parameters without parentheses
    /// (`ref.get`), and a chain on a value a factory just made is that
    /// type's (`IO.pure(1).flatMap(…)`); a field read so links to nothing.
    #[test]
    fn scala_parameterless_calls_and_factory_chains_link() {
        let g = graph(&[
            (
                "/p/Ref.scala",
                "package fx\n\nclass Ref[A](var value: A) {\n  def get: A = value\n}\n\nclass IO[A] {\n  def flatMap[B](f: A => IO[B]): IO[B] = ???\n}\nobject IO {\n  def pure[A](a: A): IO[A] = ???\n}\n",
            ),
            (
                "/p/Use.scala",
                "package fx\n\nobject Use {\n  def run(ref: Ref[Int]): Int = {\n    val now = ref.get\n    IO.pure(now).flatMap(n => IO.pure(n))\n    ref.value\n  }\n}\n",
            ),
        ]);
        assert!(
            has(&g, "/p/Use.scala:run", "/p/Ref.scala:get"),
            "{:?}",
            edges(&g)
        );
        assert!(has(&g, "/p/Use.scala:run", "/p/Ref.scala:flatMap"));
        assert!(has(&g, "/p/Use.scala:run", "/p/Ref.scala:pure"));
        let reads = FileCalls::read(
            Path::new("/p/Use.scala"),
            "object Use {\n  def run(ref: Ref[Int]) = {\n    ref.get\n    f(ref.value)\n  }\n}\n",
        )
        .unwrap();
        assert!(reads.calls.iter().any(|c| c.callee == "get" && c.method));
        assert!(
            !reads.calls.iter().any(|c| c.callee == "f" && c.method),
            "a call's own function is no member read"
        );
    }

    /// A test calls the code; the code never calls a test's helpers: an
    /// app's call reaches the app's method of the name, not a test file's,
    /// while a test's call reaches both.
    #[test]
    fn app_code_does_not_call_into_tests() {
        let files = [
            (
                "/p/app/models/user.rb",
                "class User\n  def email_time\n    1\n  end\nend\n",
            ),
            (
                "/p/app/jobs/mailer_job.rb",
                "class MailerJob\n  def perform(user)\n    user.email_time\n  end\nend\n",
            ),
            (
                "/p/test/support/helpers.rb",
                "module Helpers\n  def email_time\n    2\n  end\nend\n",
            ),
            (
                "/p/test/mailer_job_test.rb",
                "class MailerJobTest\n  def test_it\n    email_time\n  end\nend\n",
            ),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = [
            (
                PathBuf::from("/p/app/jobs/mailer_job.rb"),
                HashSet::from([
                    PathBuf::from("/p/app/models/user.rb"),
                    PathBuf::from("/p/test/support/helpers.rb"),
                ]),
            ),
            (
                PathBuf::from("/p/test/mailer_job_test.rb"),
                HashSet::from([PathBuf::from("/p/test/support/helpers.rb")]),
            ),
        ]
        .into_iter()
        .collect();
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        let perform = "/p/app/jobs/mailer_job.rb:perform";
        assert!(
            !has(&g, perform, "/p/test/support/helpers.rb:email_time"),
            "{:?}",
            edges(&g)
        );
        assert!(has(
            &g,
            "/p/test/mailer_job_test.rb:test_it",
            "/p/test/support/helpers.rb:email_time"
        ));
    }

    /// What a call cannot be, by each language's rules: an Elixir bare name
    /// neither defined nor imported is a library macro's, not the project's
    /// one function of the name; an Erlang module (`:crypto.hash`) or one the
    /// project defines none of (`Map.put`) calls none of its functions; a
    /// Kotlin method on a value of no known type is no top-level function,
    /// nor is `x.toString()` some class's override; a Dart bare call is no
    /// other class's method.
    #[test]
    fn calls_reach_only_what_their_language_lets_them() {
        let g = graph(&[
            (
                "/p/lib/limits.ex",
                "defmodule Shop.Limits do\n  def sum(xs), do: Enum.sum(xs)\n  def hash(x), do: x\n  def put(m, k, v), do: m\nend\n",
            ),
            (
                "/p/lib/metrics.ex",
                "defmodule Shop.Metrics do\n  def metrics do\n    sum(\"bytes\")\n    :crypto.hash(:sha256, \"x\")\n    Map.put(%{}, :a, 1)\n  end\nend\n",
            ),
            (
                "/p/src/Helpers.kt",
                "package shop\n\nfun get(path: String): String = path\n\nclass Model {\n  override fun toString(): String = \"m\"\n}\n",
            ),
            (
                "/p/src/Client.kt",
                "package shop\n\nfun fetch(client: Any) {\n  client.get(\"/x\")\n  client.toString()\n  get(\"/y\")\n}\n",
            ),
            (
                "/p/lib/cache.dart",
                "class Cache {\n  bool contains(String key) => false;\n}\n",
            ),
            (
                "/p/lib/test_it.dart",
                "import 'cache.dart';\nvoid main() {\n  expect(1, contains(1));\n}\n",
            ),
        ]);
        let metrics = "/p/lib/metrics.ex:metrics";
        for to in ["sum", "hash", "put"] {
            assert!(
                !has(&g, metrics, &format!("/p/lib/limits.ex:{to}")),
                "{to}: {:?}",
                edges(&g)
            );
        }
        let fetch = "/p/src/Client.kt:fetch";
        assert!(
            has(&g, fetch, "/p/src/Helpers.kt:get"),
            "the bare call: {:?}",
            edges(&g)
        );
        let calls_get = g
            .callees_of(g.id_of(Path::new("/p/src/Client.kt"), "fetch").unwrap())
            .iter()
            .filter(|&&c| g.node(c).name == "get")
            .count();
        assert_eq!(calls_get, 1, "only the bare call reaches `get`");
        assert!(!has(&g, fetch, "/p/src/Helpers.kt:toString"));
        assert!(!has(
            &g,
            "/p/lib/test_it.dart:main",
            "/p/lib/cache.dart:contains"
        ));
    }

    /// A test class's method is the runner's to call: `client.get(…)` in
    /// one test class is no call of another's `get` test, while a test
    /// helper class's method still is.
    #[test]
    fn a_test_classs_methods_are_no_value_s() {
        let g = graph(&[
            (
                "/p/src/main/kotlin/shop/Server.kt",
                "package shop\n\nfun module() {}\n",
            ),
            (
                "/p/src/test/kotlin/shop/StatusCodesTest.kt",
                "package shop\n\nclass StatusCodesTest {\n    @Test\n    fun get() {\n        module()\n    }\n}\n",
            ),
            (
                "/p/src/test/kotlin/shop/FakeClient.kt",
                "package shop\n\nclass FakeClient {\n    fun post(path: String) {}\n}\n",
            ),
            (
                "/p/src/test/kotlin/shop/RedirectsTest.kt",
                "package shop\n\nclass RedirectsTest {\n    @Test\n    fun follows() {\n        client.get(\"/x\")\n        client.post(\"/y\")\n    }\n}\n",
            ),
            (
                "/p/test/cart_test.exs",
                "defmodule Shop.CartTest do\n  test \"adds\" do\n    make_cart()\n  end\n\n  defp make_cart, do: %{}\nend\n",
            ),
        ]);
        // A test's own helper, called bare from the test's block.
        assert!(
            has(
                &g,
                "/p/test/cart_test.exs:adds",
                "/p/test/cart_test.exs:make_cart"
            ),
            "{:?}",
            edges(&g)
        );
        let follows = "/p/src/test/kotlin/shop/RedirectsTest.kt:follows";
        assert!(
            !has(
                &g,
                follows,
                "/p/src/test/kotlin/shop/StatusCodesTest.kt:get"
            ),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, follows, "/p/src/test/kotlin/shop/FakeClient.kt:post"),
            "{:?}",
            edges(&g)
        );
        for name in [
            "CartTest",
            "Shop.CartTests",
            "CartSpec",
            "CartIT",
            "TestCart",
            "Test",
        ] {
            assert!(is_test_class_name(name), "{name}");
        }
        for name in ["Contest", "Latest", "Testimony", "AUDIT", "Spec"] {
            assert!(!is_test_class_name(name), "{name}");
        }
    }

    /// A Scala class calls, bare, a method of a trait it mixes in, the
    /// trait mixed in through another trait (`with DequeueTests`, which
    /// `extends QueueTests[Dequeue]`), each in its own file.
    #[test]
    fn a_scala_bare_call_reaches_a_mixed_in_traits_method() {
        let g = graph(&[
            (
                "/p/tests/src/test/scala/shop/QueueSuite.scala",
                "package shop\n\ntrait QueueTests[Q[_[_], _]] { self: BaseSuite =>\n  def batchTests(name: String): Unit = ()\n}\n",
            ),
            (
                "/p/tests/src/test/scala/shop/DequeueSuite.scala",
                "package shop\n\nclass BoundedDequeueSuite extends BaseSuite with DequeueTests {\n  private def boundedTests(name: String) = {\n    batchTests(name)\n  }\n}\n\ntrait DequeueTests extends QueueTests[Dequeue] { self: BaseSuite =>\n  def reverse(name: String) = ()\n}\n",
            ),
        ]);
        assert!(
            has(
                &g,
                "/p/tests/src/test/scala/shop/DequeueSuite.scala:boundedTests",
                "/p/tests/src/test/scala/shop/QueueSuite.scala:batchTests"
            ),
            "{:?}",
            edges(&g)
        );
    }

    /// A Scala package's member is called through the package: `os.read(p)`
    /// is `object read`'s `apply`, `os.pwd` the package object's `pwd` —
    /// never another class's `read`; and an object called as a function
    /// (`RelPath("a")`) runs its `apply`. A local value of the package's
    /// name is the value, not the package.
    #[test]
    fn a_scala_package_member_and_an_objects_apply_are_called() {
        let g = graph(&[
            (
                "/p/os/src/package.scala",
                "package object os {\n  def pwd: Path = ???\n}\n",
            ),
            (
                "/p/os/src/ReadWriteOps.scala",
                "package os\n\nobject read extends Function1[Path, String] {\n  def apply(p: Path): String = \"\"\n}\n",
            ),
            (
                "/p/os/src/Model.scala",
                "package os\n\nobject RelPath {\n  def apply(s: String): RelPath = ???\n}\n\nclass Reader {\n  def read(): Int = 0\n  def close(): Unit = ()\n}\n",
            ),
            (
                "/p/os/src/Use.scala",
                "package os\n\nobject Use {\n  def go() = RelPath(\"a\")\n}\n",
            ),
            (
                "/p/app/src/Conn.scala",
                "package app\n\nclass Conn {\n  def close(): Unit = ()\n}\n",
            ),
            (
                "/p/app/src/Main.scala",
                "package app\n\nobject Main {\n  def run(): Unit = {\n    os.read(p)\n    os.pwd\n  }\n  def shut(): Unit = {\n    val os = open()\n    os.close()\n  }\n}\n",
            ),
        ]);
        let run = "/p/app/src/Main.scala:run";
        let e = edges(&g);
        assert!(has(&g, run, "/p/os/src/ReadWriteOps.scala:apply"), "{e:?}");
        assert!(has(&g, run, "/p/os/src/package.scala:pwd"), "{e:?}");
        assert!(!has(&g, run, "/p/os/src/Model.scala:read"), "{e:?}");
        assert!(
            has(&g, "/p/os/src/Use.scala:go", "/p/os/src/Model.scala:apply"),
            "{e:?}"
        );
        // `os` bound here is a value: its `close` is a method's, as before.
        assert!(
            has(
                &g,
                "/p/app/src/Main.scala:shut",
                "/p/app/src/Conn.scala:close"
            ),
            "{e:?}"
        );
    }

    /// A spec sees what RSpec loads before it: `spec_helper.rb` (which
    /// `.rspec` requires), what that requires, and `spec/support/`'s files
    /// (required by a glob) — a helper `config.include`s, called bare.
    #[test]
    fn a_spec_sees_what_rspec_loads_before_it() {
        let g = graph(&[
            (
                "/p/spec/spec_helper.rb",
                "require 'shop'\nRSpec.configure do |config|\n  config.include SpecHelpers\nend\n",
            ),
            (
                "/p/spec/support/spec_helpers.rb",
                "module SpecHelpers\n  def mock_app(app)\n    app\n  end\nend\n",
            ),
            (
                "/p/spec/lib/app_spec.rb",
                "describe 'app' do\n  it 'serves' do\n    mock_app(1)\n  end\nend\n",
            ),
            // Another `mock_app`, which nothing loads.
            (
                "/p/contrib/lib/helpers.rb",
                "module Contrib\n  def mock_app(app)\n    app\n  end\nend\n",
            ),
        ]);
        let e = edges(&g);
        assert!(
            e.iter()
                .any(|(a, b)| a.starts_with("/p/spec/lib/app_spec.rb:")
                    && b == "/p/spec/support/spec_helpers.rb:mock_app"),
            "{e:?}"
        );
        assert!(
            !e.iter()
                .any(|(_, b)| b == "/p/contrib/lib/helpers.rb:mock_app"),
            "{e:?}"
        );
    }

    /// A class the grammar could not finish still owns its methods: Kotlin's
    /// grammar reads a property named `where` as its keyword, and the whole
    /// class became an error, every method no class's — so `q.limit(1)` on a
    /// value of no known type, a class's method, reached none.
    #[test]
    fn an_unfinished_class_still_owns_its_methods() {
        let query = "package shop\n\nopen class Query(where: Int?) {\n    fun limit(n: Int): Query = this\n\n    fun where(predicate: Int): Query {\n        where?.let { error(\"twice\") }\n        where = predicate\n        return this\n    }\n}\n";
        // The grammar's error, which the owner is recovered through.
        let tree = crate::highlight::parse(query, Lang::Kotlin).unwrap();
        assert!(tree.root_node().has_error());
        let calls = FileCalls::read(Path::new("/p/src/Query.kt"), query).unwrap();
        assert_eq!(calls.owners.get(&4).map(String::as_str), Some("Query"));
        let g = graph(&[
            ("/p/src/Query.kt", query),
            (
                "/p/src/Run.kt",
                "package shop\n\nfun run() {\n    val q = open()\n    q.limit(1)\n}\n",
            ),
        ]);
        assert!(
            has(&g, "/p/src/Run.kt:run", "/p/src/Query.kt:limit"),
            "{:?}",
            edges(&g)
        );
    }

    /// A C# file Visual Studio saved starts with a byte-order mark: still
    /// in its namespace, so `new CacheEntry<T>(…)` beside it is a call of the
    /// constructor there.
    #[test]
    fn a_file_with_a_byte_order_mark_is_in_its_namespace() {
        let entry = "\u{feff}using System;\n\nnamespace Shop.Services;\n\npublic class CacheEntry<T>\n{\n    public CacheEntry(T item)\n    {\n        Value = item;\n    }\n    public T Value { get; set; }\n}\n";
        assert_eq!(
            package_of(entry, Lang::CSharp).as_deref(),
            Some("Shop.Services")
        );
        let g = graph(&[
            ("/p/src/Services/CacheEntry.cs", entry),
            (
                "/p/src/Services/Decorator.cs",
                "\u{feff}using System;\n\nnamespace Shop.Services;\n\npublic class Decorator\n{\n    public void Refresh(int items)\n    {\n        var entry = new CacheEntry<int>(items);\n    }\n}\n",
            ),
        ]);
        assert!(
            has(
                &g,
                "/p/src/Services/Decorator.cs:Refresh",
                "/p/src/Services/CacheEntry.cs:CacheEntry"
            ),
            "{:?}",
            edges(&g)
        );
    }

    /// A parameter called is the value passed, no function of the
    /// project's that shares its name: bloc's `emit('done')` in an event
    /// handler `(event, emit) { … }` is the handler's emitter, not
    /// `Bloc.emit`; so is a callback in every language whose functions are
    /// values.
    #[test]
    fn a_parameter_called_is_no_project_function() {
        let cases: &[(&str, &str)] = &[
            (
                "/p/lib/counter.dart",
                "class Bloc {\n  void emit(int s) {}\n}\nclass Counter extends Bloc {\n  void setUp() {\n    on((event, emit) {\n      emit(1);\n    });\n  }\n}\n",
            ),
            (
                "/p/src/run.ts",
                "function emit(s: number) {}\nexport function run(emit: (s: number) => void) {\n  emit(1);\n}\nexport const go = (emit) => emit(2);\n",
            ),
            (
                "/p/src/run.py",
                "def emit(s):\n    pass\n\ndef run(emit, *rest):\n    emit(1)\n",
            ),
            (
                "/p/src/Run.kt",
                "package p\n\nfun emit(s: Int) {}\n\nfun run(emit: (Int) -> Unit) {\n    emit(1)\n}\n",
            ),
            (
                "/p/src/run.go",
                "package p\n\nfunc emit(s int) {}\n\nfunc run(emit func(int)) {\n\temit(1)\n}\n",
            ),
            (
                "/p/src/run.c",
                "void emit(int s) {}\n\nvoid run(void (*emit)(int)) {\n    emit(1);\n}\n",
            ),
            (
                "/p/src/Run.cs",
                "namespace P;\n\nclass Run\n{\n    void Emit(int s) {}\n    void Go(Action<int> Emit)\n    {\n        Emit(1);\n    }\n}\n",
            ),
        ];
        for (file, src) in cases {
            let g = graph(&[(file, src)]);
            let e = edges(&g);
            assert!(
                !e.iter()
                    .any(|(_, b)| b.ends_with(":emit") || b.ends_with(":Emit")),
                "{file}: {e:?}"
            );
        }
        // A parameter's default is no parameter: `compute()` in it, and in
        // the body, is the function's.
        let g = graph(&[(
            "/p/src/defaults.py",
            "def compute():\n    return 1\n\ndef run(x=compute()):\n    compute()\n",
        )]);
        assert!(has(
            &g,
            "/p/src/defaults.py:run",
            "/p/src/defaults.py:compute"
        ));
    }

    /// A value of a type the project does not declare has the library's
    /// methods: a C++ field declared as `std::vector<…>`, a Zig parameter
    /// of `*std.Io.Writer`, a Java `AtomicReference` built — none reaches
    /// the project's `size`, `writeAll` or `get`. A C++ local built where it is declared
    /// (`spdlog::pattern_formatter f("%+");`) is its class's exactly. And a
    /// test class's own method is never another value's.
    #[test]
    fn a_library_typed_value_calls_no_project_method() {
        let g = graph(&[
            (
                "/p/include/store.h",
                "#include <vector>\nclass store {\n  std::vector<int> data_;\n public:\n  int size() const { return data_.size(); }\n};\n",
            ),
            (
                "/p/include/pattern_formatter.h",
                "namespace spdlog {\nclass pattern_formatter {\n public:\n  void format(int msg) {}\n};\nclass custom_flag {\n public:\n  void format(int msg) {}\n};\n}\nvoid use() {\n  spdlog::pattern_formatter formatter(\"%+\");\n  formatter.format(1);\n}\n",
            ),
            (
                "/p/src/print.zig",
                "const std = @import(\"std\");\nconst Escaping = struct {\n    fn writeAll(self: *Escaping, s: []const u8) void {}\n};\nfn render(writer: *std.Io.Writer) !void {\n    try writer.writeAll(\"x\");\n}\n",
            ),
            (
                "/p/src/main/java/p/Pool.java",
                "package p;\n\nclass Pool {\n  Object get() { return null; }\n  void park() {\n    AtomicReference<Object> signal = new AtomicReference<>();\n    signal.get();\n  }\n}\n",
            ),
            (
                "/p/src/test/kotlin/p/StatusCodesTest.kt",
                "package p\n\nclass StatusCodesTest {\n    @Test\n    fun put() {\n        client.put(\"/x\")\n    }\n}\n",
            ),
        ]);
        let e = edges(&g);
        for (from, to) in [
            ("/p/include/store.h:size", "/p/include/store.h:size"),
            ("/p/src/print.zig:render", "/p/src/print.zig:writeAll"),
            (
                "/p/src/main/java/p/Pool.java:park",
                "/p/src/main/java/p/Pool.java:get",
            ),
            (
                "/p/src/test/kotlin/p/StatusCodesTest.kt:put",
                "/p/src/test/kotlin/p/StatusCodesTest.kt:put",
            ),
        ] {
            assert!(!has(&g, from, to), "{from} -> {to}: {e:?}");
        }
        let formats: Vec<usize> = g
            .callees_of(
                g.id_of(Path::new("/p/include/pattern_formatter.h"), "use")
                    .unwrap(),
            )
            .iter()
            .copied()
            .filter(|&c| g.node(c).name == "format")
            .collect();
        assert_eq!(formats.len(), 1, "{e:?}");
        assert_eq!(g.node(formats[0]).line, 4, "{e:?}");
    }

    /// A BEAM function is its name and its arity: `parse(A, B)` is
    /// `parse/2`, never `parse/5` — nor is `x |> parse(y)` (two, the piped
    /// value first), `&parse/2` or `fun parse/2`; an Elixir default lets a
    /// call leave its argument out.
    #[test]
    fn a_beam_call_reaches_the_function_of_its_arity() {
        let g = graph(&[
            (
                "/p/src/getopt.erl",
                "-module(getopt).\n-export([run/1]).\n\nparse(A, B) -> parse(A, B, [], [], []).\n\nparse(A, B, C, D, E) -> {A, B, C, D, E}.\n\nrun(X) -> parse(X, 1), F = fun parse/2, F.\n",
            ),
            (
                "/p/lib/cart.ex",
                "defmodule Cart do\n  def total(items), do: items\n  def total(items, tax), do: {items, tax}\n  def price(item, qty \\\\ 1), do: {item, qty}\n  def run(items) do\n    items |> total(0.2)\n    price(:a)\n    Enum.map(items, &total/1)\n  end\nend\n",
            ),
            (
                "/p/lib/checkout.ex",
                "defmodule Checkout do\n  def go(items), do: Enum.map(items, &Cart.total/2)\nend\n",
            ),
        ]);
        let e = edges(&g);
        let calls = |from: &str, to_line: usize| {
            let from = g.id_of(
                Path::new(from.split(':').next().unwrap()),
                from.split(':').nth(1).unwrap(),
            );
            from.is_some_and(|f| g.callees_of(f).iter().any(|&c| g.node(c).line == to_line))
        };
        // `parse/2` calls `parse/5`; `run` calls `parse/2` only.
        assert!(calls("/p/src/getopt.erl:run", 4), "{e:?}");
        assert!(!calls("/p/src/getopt.erl:run", 6), "{e:?}");
        // The pipe passes two: `total/2`, and the capture `total/1`.
        assert!(calls("/p/lib/cart.ex:run", 3), "{e:?}");
        assert!(calls("/p/lib/cart.ex:run", 2), "{e:?}");
        // `price(:a)` leaves the default out.
        assert!(calls("/p/lib/cart.ex:run", 4), "{e:?}");
        // `&Cart.total/2` is `total/2`, not `total/1`.
        assert!(calls("/p/lib/checkout.ex:go", 3), "{e:?}");
        assert!(!calls("/p/lib/checkout.ex:go", 2), "{e:?}");
        let calls_line = |line: usize| {
            let run = g.id_of(Path::new("/p/src/getopt.erl"), "parse").unwrap();
            g.callees_of(run).iter().any(|&c| g.node(c).line == line)
        };
        assert!(calls_line(6), "{e:?}");
    }

    /// A function's arity is read from its own head: past a binary
    /// pattern's commas (`<<1:1, X:7, Tl/binary>>`), past the `@doc` before
    /// it, and with every default of a head written over several lines.
    #[test]
    fn beam_arities_are_read_from_the_head() {
        let erl = "-module(pb).\n\n-spec read_gr_b(binary(), integer()) -> integer().\n%% read (a, b, c)\nread_gr_b(<<1:1, X:7, Tl/binary>>, N)\n  when N < (32-7) ->\n    read_gr_b(Tl, N + X);\nread_gr_b(<<0:1, _:7, _/binary>>, $,) -> 0.\n";
        let fc = FileCalls::read(Path::new("/p/src/pb.erl"), erl).unwrap();
        assert_eq!(fc.context.arities, [(5, 2, 2)], "{:?}", fc.context.arities);
        let ex = "defmodule Plug.Conn.Query do\n  @doc \"\"\"\n  Decodes (a query), with defaults.\n  \"\"\"\n  def decode(\n        query,\n        initial \\\\ [],\n        invalid \\\\ Plug.Conn.InvalidQueryError,\n        validate_utf8 \\\\ true\n      ) do\n    {query, initial, invalid, validate_utf8}\n  end\nend\n";
        let fc = FileCalls::read(Path::new("/p/lib/query.ex"), ex).unwrap();
        let decode = fc.context.arities.iter().find(|a| a.2 == 4);
        assert_eq!(
            decode.map(|a| (a.1, a.2)),
            Some((1, 4)),
            "{:?}",
            fc.context.arities
        );
        // A default that is an anonymous function: its commas are its own.
        let stub = "defmodule SsoTest do\n  defp stub_uri(\n         assertion \\\\ fn _c, _t, _r, _s -> :ok end\n       ) do\n    assertion\n  end\nend\n";
        let fc = FileCalls::read(Path::new("/p/test/sso_test.exs"), stub).unwrap();
        assert_eq!(fc.context.arities, [(2, 0, 1)], "{:?}", fc.context.arities);
    }

    /// An Elixir file's `use`s and `import`s, the multi-alias form too.
    #[test]
    fn elixir_imports_are_read_in_every_form() {
        let src = "defmodule Shop.Case do\n  use ExUnit.CaseTemplate\n  quote do\n    import Ecto.Query, only: [from: 2]\n    import Shop.{Factory, TestHelpers}\n    import Shop.Web.{\n      Auth,\n      Forms\n    }\n    import unquote(__MODULE__)\n  end\nend\n";
        assert_eq!(elixir_modules_after(src, "use "), ["ExUnit.CaseTemplate"]);
        assert_eq!(
            elixir_modules_after(src, "import "),
            [
                "Ecto.Query",
                "Shop.Factory",
                "Shop.TestHelpers",
                "Shop.Web.Auth",
                "Shop.Web.Forms"
            ]
        );
    }

    /// An Elixir bare call reaches its own module, the modules it imports,
    /// and what a `use`d module's `__using__` injects (its quoted imports,
    /// its own `def`s, and through the modules it uses in turn) — never
    /// another module's function that only shares the name.
    #[test]
    fn an_elixir_bare_call_reaches_only_what_is_imported() {
        let g = graph(&[
            (
                "/p/lib/invites.ex",
                "defmodule Shop.Invites do\n  def insert(org, user), do: {org, user}\n  def get(id), do: id\nend\n",
            ),
            (
                "/p/test/support/factory.ex",
                "defmodule Shop.Factory do\n  def insert(kind), do: kind\nend\n",
            ),
            (
                "/p/test/support/helpers.ex",
                "defmodule Shop.Helpers do\n  def login(conn), do: conn\nend\n",
            ),
            (
                "/p/test/support/data_case.ex",
                "defmodule Shop.DataCase do\n  defmacro __using__(_) do\n    quote do\n      import Shop.Factory\n      def fixture, do: 1\n    end\n  end\nend\n",
            ),
            (
                "/p/test/support/conn_case.ex",
                "defmodule Shop.ConnCase do\n  defmacro __using__(_) do\n    quote do\n      use Shop.DataCase\n      import Shop.{\n        Helpers,\n        Missing\n      }\n    end\n  end\nend\n",
            ),
            (
                "/p/test/invite_test.exs",
                "defmodule Shop.InviteTest do\n  use Shop.ConnCase\n  test \"invites\" do\n    insert(:user)\n    login(conn)\n    fixture()\n    get(1)\n  end\nend\n",
            ),
        ]);
        let edges = edges(&g);
        let from = |to: &str| {
            edges
                .iter()
                .any(|(a, b)| a.starts_with("/p/test/invite_test.exs:") && b == to)
        };
        assert!(from("/p/test/support/factory.ex:insert"), "{edges:?}");
        assert!(from("/p/test/support/helpers.ex:login"), "{edges:?}");
        assert!(from("/p/test/support/data_case.ex:fixture"), "{edges:?}");
        assert!(!from("/p/lib/invites.ex:insert"), "{edges:?}");
        assert!(!from("/p/lib/invites.ex:get"), "{edges:?}");
    }

    /// An extension method is called on a value of the type it extends,
    /// a library's too: C#'s `this` parameter's, Kotlin's `fun Type.name`.
    #[test]
    fn extension_methods_are_called_on_library_types() {
        let g = graph(&[
            (
                "/p/Ext.cs",
                "namespace Shop\n{\n  public static class ServiceExtensions\n  {\n    public static IServiceCollection AddShop(this IServiceCollection services) { return services; }\n  }\n}\n",
            ),
            (
                "/p/Startup.cs",
                "namespace Shop\n{\n  public class Startup\n  {\n    public void Configure(IServiceCollection services) { services.AddShop(); }\n  }\n}\n",
            ),
            (
                "/p/text.kt",
                "package shop\n\nfun String.shout(): String = uppercase()\n",
            ),
            (
                "/p/use.kt",
                "package shop\n\nfun greet(name: String): String = name.shout()\n",
            ),
        ]);
        assert!(
            has(&g, "/p/Startup.cs:Configure", "/p/Ext.cs:AddShop"),
            "{:?}",
            edges(&g)
        );
        assert!(has(&g, "/p/use.kt:greet", "/p/text.kt:shout"));
    }

    /// Kotlin's files of one package, wherever they are filed, and a
    /// constructor's property, typed: the class's method. C#'s namespace
    /// and the namespaces around it, with no `using`. A Ruby class's
    /// mixed-in module's method, called bare.
    #[test]
    fn kotlin_csharp_and_ruby_reach_their_packages_and_mixins() {
        let g = graph(&[
            (
                "/p/a/Repo.kt",
                "package shop\n\nclass Repo {\n  fun load(): Int = 1\n}\n",
            ),
            (
                "/p/b/Service.kt",
                "package shop\n\nclass Service(private val repo: Repo) {\n  fun run() = repo.load()\n}\n",
            ),
            (
                "/p/Core/Clock.cs",
                "namespace Shop\n{\n  public class Clock\n  {\n    public int Now() { return 0; }\n  }\n}\n",
            ),
            (
                "/p/Web/Home.cs",
                "namespace Shop.Web\n{\n  public class Home\n  {\n    private readonly Clock clock;\n    public int Index() { return clock.Now(); }\n  }\n}\n",
            ),
            (
                "/p/greeting.rb",
                "module Greeting\n  def greet\n    'hi'\n  end\nend\n",
            ),
            (
                "/p/shop.rb",
                "class Shop\n  include Greeting\n\n  def run\n    greet\n  end\nend\n",
            ),
        ]);
        assert!(
            has(&g, "/p/b/Service.kt:run", "/p/a/Repo.kt:load"),
            "{:?}",
            edges(&g)
        );
        assert!(has(&g, "/p/Web/Home.cs:Index", "/p/Core/Clock.cs:Now"));
        assert!(
            has(&g, "/p/shop.rb:run", "/p/greeting.rb:greet"),
            "{:?}",
            edges(&g)
        );
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
    /// across files by the unique-name rule — a GNU-style definition (its
    /// type on the line above its name) too. (A `static` one is its file's
    /// alone: see `a_c_or_cpp_function_declared_in_a_header_is_reached_through_it`.)
    #[test]
    fn c_calls_link_definitions_not_prototypes() {
        let header = "int helper(void);\nint unused_decl(int);\n";
        let lib = "int\nhelper(void)\n{\n  return 1;\n}\n";
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

    /// C and C++ link a program's files into one: a header declares what
    /// another file defines (`int add(int, int);` in `class Cart`, defined
    /// as `int Cart::add` in `cart.cpp`), and a file that includes it calls
    /// the definition — the include brings the declaration only. `c.add(…)`
    /// in `main.cpp` read "No callers" on `Cart::add`. A declaration in
    /// scope (the header included, or the file's own prototype) reaches the
    /// definitions of its name with its owner: `Cart`'s `add`, not
    /// `Order`'s, which no header in scope declares.
    #[test]
    fn a_c_or_cpp_function_declared_in_a_header_is_reached_through_it() {
        let header = "#pragma once\n\nclass Cart {\npublic:\n    int add(int a, int b);\n    static Cart make();\n};\n";
        let source = "#include \"cart.hpp\"\n\nint Cart::add(int a, int b) { return a + b; }\n\nCart Cart::make() { return Cart(); }\n";
        let order_h = "#pragma once\nstruct Order { int add(int a); };\n";
        let order = "#include \"order.hpp\"\nint Order::add(int a) { return a; }\n";
        let main = "#include <cstdio>\n#include \"cart.hpp\"\n\nint total(Cart &c) { return c.add(1, 2) + c.add(3, 4); }\n\nint main() {\n    Cart c = Cart::make();\n    std::printf(\"%d\\n\", total(c) + c.add(5, 6));\n    return 0;\n}\n";
        // Through a header that includes the class's.
        let shop_h = "#pragma once\n#include \"cart.hpp\"\n";
        let app = "#include \"shop.hpp\"\nint run_shop(Cart &c) { return c.add(7, 8); }\n";
        let c_h = "int twice(int x);\n";
        let c_lib = "#include \"calc.h\"\nint twice(int x) { return x * 2; }\n";
        let c_main = "#include \"calc.h\"\nint run(void) { return twice(2); }\n";
        // In C, the other program's `twice` is no header's in scope.
        let other = "static int twice(int x) { return x; }\nint other(void) { return twice(1); }\n";
        let files: &[(&str, &str)] = &[
            ("/p/cart.hpp", header),
            ("/p/cart.cpp", source),
            ("/p/order.hpp", order_h),
            ("/p/order.cpp", order),
            ("/p/main.cpp", main),
            ("/p/shop.hpp", shop_h),
            ("/p/app.cpp", app),
            ("/p/calc.h", c_h),
            ("/p/calc.c", c_lib),
            ("/p/run.c", c_main),
            ("/p/tool/other.c", other),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let includes = |from: &str, to: &[&str]| {
            (
                PathBuf::from(from),
                to.iter().map(PathBuf::from).collect::<HashSet<PathBuf>>(),
            )
        };
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = [
            includes("/p/cart.cpp", &["/p/cart.hpp"]),
            includes("/p/order.cpp", &["/p/order.hpp"]),
            includes("/p/main.cpp", &["/p/cart.hpp"]),
            includes("/p/shop.hpp", &["/p/cart.hpp"]),
            includes("/p/app.cpp", &["/p/shop.hpp"]),
            includes("/p/calc.c", &["/p/calc.h"]),
            includes("/p/run.c", &["/p/calc.h"]),
        ]
        .into_iter()
        .collect();
        let g = ProjectCallGraph::build(defs_of(files), &sources, &scope);
        for (from, to) in [
            ("/p/main.cpp:total", "/p/cart.cpp:add"),
            ("/p/main.cpp:main", "/p/cart.cpp:add"),
            ("/p/main.cpp:main", "/p/cart.cpp:make"),
            ("/p/main.cpp:main", "/p/main.cpp:total"),
            ("/p/app.cpp:run_shop", "/p/cart.cpp:add"),
            ("/p/run.c:run", "/p/calc.c:twice"),
            ("/p/tool/other.c:other", "/p/tool/other.c:twice"),
        ] {
            assert!(has(&g, from, to), "{from} -> {to}: {:?}", edges(&g));
        }
        for (from, to) in [
            ("/p/main.cpp:total", "/p/order.cpp:add"),
            ("/p/main.cpp:main", "/p/order.cpp:add"),
            ("/p/main.cpp:total", "/p/cart.hpp:add"),
            ("/p/run.c:run", "/p/tool/other.c:twice"),
        ] {
            assert!(!has(&g, from, to), "not {from} -> {to}: {:?}", edges(&g));
        }
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
    fn a_cpp_destructor_s_calls_are_not_the_constructor_s() {
        let src = "\
void cleanup() {}
void setup() {}
struct Foo {
  Foo() { setup(); }
  ~Foo() { cleanup(); }
};
";
        let g = graph(&[("/p/foo.cpp", src)]);
        assert!(
            has(&g, "/p/foo.cpp:Foo", "/p/foo.cpp:setup"),
            "{:?}",
            edges(&g)
        );
        assert!(
            !has(&g, "/p/foo.cpp:Foo", "/p/foo.cpp:cleanup"),
            "{:?}",
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

    /// PHP: functions, methods through `->`, `?->` and `::`, `new`, and a
    /// closure's calls as its method's. A call through a variable and a
    /// `new static` name nothing, and a bare `count()` is PHP's own even
    /// beside a class's `count` method, which only `$this->` reaches.
    #[test]
    fn php_functions_methods_and_constructors_are_linked() {
        let file = "/p/Cart.php";
        let src = "\
<?php
function helper($x) { return $x; }
class Cart {
  public function __construct() { $this->reset(); }
  public function reset() {}
  public function count() { return 0; }
  public function total($o) {
    helper(1);
    $o?->reset();
    self::audit();
    $copy = new Cart();
    $same = new static();
    $f = 'helper';
    $f(2);
    $g = function () { audit_log(); };
    return count([]);
  }
  public static function audit() {}
}
function audit_log() {}
";
        let g = graph(&[(file, src)]);
        for (from, to) in [
            ("__construct", "reset"),
            ("total", "helper"),
            ("total", "reset"),
            ("total", "audit"),
            ("total", "audit_log"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        assert!(
            !has(&g, &format!("{file}:total"), &format!("{file}:count")),
            "a bare count() is PHP's: {:?}",
            edges(&g)
        );
        let calls = calls_of(src, "php");
        let call = |callee: &str| calls.iter().find(|c| c.callee == callee);
        assert!(call("reset").is_some_and(|c| c.method), "{calls:?}");
        assert!(call("audit").is_some_and(|c| c.method), "{calls:?}");
        assert!(call("helper").is_some_and(|c| !c.method), "{calls:?}");
        assert!(
            call("Cart").is_some_and(|c| !c.method),
            "new Cart: {calls:?}"
        );
        assert!(call("f").is_none(), "a variable call: {calls:?}");
        assert!(call("static").is_none(), "new static: {calls:?}");
    }

    /// Ruby: a call with a receiver is a method call, one without is on
    /// `self`; `valid?` keeps its `?`, and `self.total = 0` calls the
    /// setter. A bare name standing as a statement (`setup`) is a call of
    /// the method — unless the method binds it first (`sum = 0 … sum`, a
    /// parameter, a block's), when it is the local's value; and a bare
    /// `puts` is `Kernel`'s, not a project `puts` in another class.
    #[test]
    fn ruby_calls_with_and_without_receivers_are_linked() {
        let file = "/p/account.rb";
        let account = "\
class Account
  def initialize
    setup
    self.total = 0
  end
  def setup; end
  def total; 0; end
  def total=(v); end
  def valid?; true; end
  def deposit(x)
    audit(x)
    return unless valid?
    [x].each { |i| audit(i) }
    puts 'deposited'
  end
  def sum; end
  def balance(x)
    sum = 0
    x
    sum
  end
  def self.open
    audit(1)
  end
  def audit(x); end
end
";
        let shell = "class Shell\n  def puts(msg); end\nend\n";
        let g = graph(&[(file, account), ("/p/shell.rb", shell)]);
        for (from, to) in [
            ("initialize", "total="),
            ("initialize", "setup"),
            ("deposit", "audit"),
            ("deposit", "valid?"),
            ("open", "audit"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        for (from, to) in [
            ("/p/account.rb:initialize", "/p/account.rb:total"),
            ("/p/account.rb:balance", "/p/account.rb:sum"),
            ("/p/account.rb:deposit", "/p/shell.rb:puts"),
        ] {
            assert!(!has(&g, from, to), "{from} -> {to}: {:?}", edges(&g));
        }
        let calls = calls_of(account, "ruby");
        assert!(
            calls.iter().any(|c| c.callee == "total=" && c.method),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "audit" && !c.method),
            "{calls:?}"
        );
    }

    /// C#: methods, constructors through `new`, generic and `?.` calls, and
    /// local functions as scopes of their own; a lambda's calls are its
    /// method's. `nameof(x)` is no call.
    #[test]
    fn csharp_methods_constructors_and_local_functions_are_linked() {
        let file = "/p/Account.cs";
        let src = "\
class Account {
  Account() { Init(); }
  void Init() {}
  void Transfer(Account other) {
    Audit();
    other?.Init();
    var copy = new Account();
    Make<int>();
    var name = nameof(Audit);
    int Local(int v) { return Helper(v); }
    Local(1);
    System.Func<int> f = () => Helper(2);
  }
  static void Audit() {}
  static T Make<T>() => default;
  static int Helper(int v) => v;
}
";
        let g = graph(&[(file, src)]);
        for (from, to) in [
            ("Account", "Init"),
            ("Transfer", "Audit"),
            ("Transfer", "Init"),
            ("Transfer", "Account"),
            ("Transfer", "Make"),
            ("Transfer", "Local"),
            ("Transfer", "Helper"),
            ("Local", "Helper"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        let calls = calls_of(src, "csharp");
        assert!(
            !calls.iter().any(|c| c.callee == "nameof"),
            "nameof is an operator: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "Init" && c.method),
            "other?.Init(): {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "Make" && !c.method),
            "{calls:?}"
        );
    }

    /// Erlang: local and remote calls, a `fun`'s calls as its function's,
    /// each clause attributed to its own arity's function; a fun in a
    /// variable names nothing, and a bare `size(X)` is the BIF, not
    /// another module's `size`.
    #[test]
    fn erlang_local_and_remote_calls_are_linked_per_function() {
        let file = "/p/shop.erl";
        let src = "\
-module(shop).
-export([run/1]).
run([]) -> helper(0);
run(X) ->
    lists:map(fun(Y) -> inner(Y) end, X),
    shop:helper(X),
    F = fun helper/1,
    F(2),
    size(X).
helper(X) -> X.
inner(X) -> X.
price(X) -> price(X, 1).
price(X, Y) -> tax(X * Y).
tax(X) -> X.
";
        let util = "-module(util).\nsize(_) -> 0.\n";
        let g = graph(&[(file, src), ("/p/util.erl", util)]);
        for (from, to) in [("run", "helper"), ("run", "inner")] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        assert!(
            !has(&g, &format!("{file}:run"), "/p/util.erl:size"),
            "size(X) is the BIF: {:?}",
            edges(&g)
        );
        // `price/1` calls `price/2`, which calls `tax`: each clause is its
        // own arity's.
        let key = |name: &str, ordinal: u32| (PathBuf::from(file), name.to_string(), ordinal);
        let keys = g.callee_keys();
        assert_eq!(keys[&key("price", 0)], vec![key("price", 1)], "{keys:?}");
        assert_eq!(keys[&key("price", 1)], vec![key("tax", 0)], "{keys:?}");

        let mut calls = calls_of(src, "erlang");
        calls.sort_by_key(|c| c.line);
        // `helper(0)`, then `shop:helper(X)`, then `fun helper/1` (a use:
        // what the fun is called with calls it).
        let method = |callee: &str| -> Vec<bool> {
            calls
                .iter()
                .filter(|c| c.callee == callee)
                .map(|c| c.method)
                .collect()
        };
        assert_eq!(method("helper"), [false, true, false]);
        assert_eq!(method("map"), [true]);
        assert!(!calls.iter().any(|c| c.callee == "F"), "{calls:?}");
    }

    /// Haskell: an application is a call of its head, once however many
    /// arguments it takes; a function passed as an argument (`map price`)
    /// is used there, as `map` calls it; a constructor (`Just`) or a type
    /// (`IO ()`) is no call,
    /// and a `where`-bound helper's calls are its function's. `main = do …`
    /// is a scope. A bare `lookup` is the Prelude's, not another module's.
    #[test]
    fn haskell_applications_link_by_their_head() {
        let file = "/p/Shop.hs";
        let src = "\
module Shop where
import qualified Data.Map as Map

total :: [Int] -> Int
total xs = sum (map price xs) + helper 1 2
  where
    go y = inner y

price :: Int -> Int
price x = Map.findWithDefault 0 x table + lookup x

wrap :: Int -> Maybe Int
wrap x = Just (inner x)

helper :: Int -> Int -> Int
helper a b = a + b

inner :: Int -> Int
inner = id

main :: IO ()
main = do
  print (total [1])
  let r = helper 1 2
  pure ()

table = Map.fromList []
";
        let store = "module Store where\n\nlookup :: Int -> Int\nlookup k = k\n";
        let g = graph(&[(file, src), ("/p/Store.hs", store)]);
        for (from, to) in [
            ("total", "helper"),
            ("total", "inner"),
            ("wrap", "inner"),
            ("main", "total"),
            ("main", "helper"),
            ("total", "price"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        assert!(
            !has(&g, "/p/Shop.hs:price", "/p/Store.hs:lookup"),
            "price -> lookup: {:?}",
            edges(&g)
        );
        let calls = calls_of(src, "haskell");
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.callee == "helper" && c.caller.as_deref() == Some("total"))
                .count(),
            1,
            "{calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c.callee.as_str(), "Just" | "IO" | "Maybe")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.callee == "findWithDefault" && c.method),
            "{calls:?}"
        );
    }

    /// Zig: functions, and methods through a field; `@import` and `@as` are
    /// builtins, not calls.
    #[test]
    fn zig_functions_and_methods_are_linked_and_builtins_are_not_calls() {
        let file = "/p/main.zig";
        let src = "\
const std = @import(\"std\");
const Account = struct {
    fn deposit(self: *Account) void {
        self.audit();
    }
    fn audit(self: *Account) void {
        _ = self;
    }
};
fn helper() void {}
pub fn main() void {
    helper();
    var a = Account{};
    a.deposit();
    std.debug.print(\"x\", .{});
    const n = @as(u8, 2);
    _ = n;
}
";
        let g = graph(&[(file, src)]);
        for (from, to) in [
            ("main", "helper"),
            ("main", "deposit"),
            ("deposit", "audit"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        let calls = calls_of(src, "zig");
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c.callee.as_str(), "import" | "as")),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "print" && c.method),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "helper" && !c.method),
            "{calls:?}"
        );
    }

    /// A Rust path call is its type's function: `Self::make()` the impl's
    /// own, `B::make()` B's — even where B's is in another file and this one
    /// has a `make` of its own — not every `make` there is. A call on a
    /// type the project defines nothing of (`Vec::new()`, `v.len()` on a
    /// `Vec`) is none of the project's functions.
    #[test]
    fn rust_path_calls_go_to_their_types_function() {
        let lib = "\
struct A;
struct Order;
impl A {
    fn make() -> A { A }
    fn build() -> A { Self::make() }
    fn other() { let _ = b::B::make(); }
    fn len(&self) -> usize { 0 }
}
impl Order {
    fn len(&self) -> usize { 1 }
}
fn count() -> usize { let v: Vec<u8> = Vec::new(); v.len() }
fn fresh() -> Vec<u8> { Vec::new() }
fn new() {}
";
        let b = "pub struct B;\nimpl B {\n    pub fn make() -> B { B }\n}\n";
        let g = graph(&[("/p/lib.rs", lib), ("/p/b.rs", b)]);
        assert_eq!(callers_named(&g, "/p/lib.rs", "make", 0), ["build@5"]);
        assert_eq!(callers_named(&g, "/p/b.rs", "make", 0), ["other@6"]);
        for (name, ordinal) in [("len", 0), ("len", 1), ("new", 0)] {
            assert!(
                callers_named(&g, "/p/lib.rs", name, ordinal).is_empty(),
                "{name}#{ordinal}: {:?}",
                edges(&g)
            );
        }
    }

    /// A method called on a parameter or a field of a type the project does
    /// not define (`&str`, `Vec`) is none of the project's; on one of a
    /// project type, that type's. Direct recursion is an edge of its own.
    #[test]
    fn rust_receivers_are_typed_by_parameters_and_fields() {
        let lib = "\
struct RegexSet;
struct SetMatches;
struct Order { items: Vec<u8>, set: RegexSet }
impl RegexSet {
    fn len(&self) -> usize { 0 }
}
impl SetMatches {
    fn len(&self) -> usize { 1 }
}
impl Order {
    fn size(&self) -> usize { self.items.len() }
    fn count(&self) -> usize { self.set.len() }
}
fn find(haystack: &str) -> usize { haystack.len() }
fn of_set(set: &RegexSet) -> usize { set.len() }
fn fact(n: u64) -> u64 { if n == 0 { 1 } else { n * fact(n - 1) } }
";
        let g = graph(&[("/p/lib.rs", lib)]);
        assert_eq!(
            callers_named(&g, "/p/lib.rs", "len", 0),
            ["count@12", "of_set@15"],
            "{:?}",
            edges(&g)
        );
        assert!(
            callers_named(&g, "/p/lib.rs", "len", 1).is_empty(),
            "{:?}",
            edges(&g)
        );
        let fact = g.id_of_key(Path::new("/p/lib.rs"), "fact", 0).unwrap();
        assert_eq!(g.callees_of(fact), [fact]);
        assert!(g.callers_of(fact).is_empty());
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
            analysis.owners.clone(),
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
