//! Symbol outline extraction using tree-sitter tags queries.
//!
//! [`extract`] returns the wire [`Symbol`]s; [`extract_located`] and
//! [`located_in`] add what the syntax tree knows and the wire type does not —
//! where a declaration really starts, whether a function has a body (and its
//! span), and which container a Rust method sits in. [`analyze`] reads the
//! outline, the imports and the call sites off ONE parse, for callers that
//! need all three.

use std::collections::HashMap;
use std::hash::Hash;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, QueryCursor, Tree};

use crate::highlight::{Lang, QueryError};

// The outline entry is the protocol's wire type: `name`, `kind` ("function",
// "struct", "class", …), `line` (1-based line the definition's tag starts on —
// its name's line) and `end_line` (1-based last line of the WHOLE definition,
// body included, so "which symbol is the caret in" works anywhere in a body;
// see `located_in` for the grammars whose tag stops short of the body).
// Shared so there is no conversion at the wire.
pub use clew_protocol::Symbol;

/// Ordinal of `target` among same-name functions/methods in `symbols`: how
/// many same-name callables appear before it (by line). This is the identity
/// component `explain::Node::Function` uses to keep a file's same-name methods
/// (different impls' `new`, `default`, …) apart.
///
/// The reference definition, for ONE symbol (a linear scan). Everything that
/// numbers many items at once goes through [`Ordinals`], which is tested to
/// agree with this at every index.
pub fn fn_ordinal(symbols: &[Symbol], target: &Symbol) -> u32 {
    work::add(symbols.len());
    symbols
        .iter()
        .filter(|s| is_callable(&s.kind) && s.name == target.name && s.line < target.line)
        .count() as u32
}

/// `fn_ordinal` for every entry of `symbols`, in one pass: `out[i]` equals
/// `fn_ordinal(symbols, &symbols[i])` for EVERY `i`, with no exception carved
/// out for non-callables — a divergence there would be a silent drift between
/// the two definitions, and the ordinal is a cache identity (it keys
/// `explain::Node::Function`, chosen over the line number precisely so the
/// explanation cache survives edits). A wrong ordinal does not look wrong, it
/// just mis-keys or invalidates a cached explanation.
///
/// Exists because calling `fn_ordinal` inside a loop over the same list is
/// O(symbols²): on a file with thousands of callables the per-frame outline
/// view stalled the UI thread for ~100 ms per frame. This is O(n log n) and
/// allocates once per distinct name.
///
/// This is THE way to number a whole outline: the explain gatherer assigns its
/// `Node::Function` ordinals with it, and the outline view is meant to hoist it
/// above its loop instead of calling `fn_ordinal` per row. The equality
/// asserted above is what makes that substitution safe — the ordinal keys a
/// cache, so a substitution that shifted it would silently orphan every stored
/// explanation for the file.
///
/// Makes no assumption about `symbols` being ordered — `extract` sorts by line,
/// but this must not silently break for a list that arrives any other way, so
/// the per-name lines are sorted here rather than assumed ascending.
pub fn fn_ordinals(symbols: &[Symbol]) -> Vec<u32> {
    let ordinals = Ordinals::new(
        symbols
            .iter()
            .map(|s| (s.name.as_str(), is_callable(&s.kind), s.line)),
    );
    symbols
        .iter()
        .map(|s| ordinals.of(&s.name.as_str(), s.line))
        .collect()
}

/// The same-name ordinal over any list of items, grouped by any key: the
/// numbering behind [`fn_ordinals`] (grouped by name, within one file) and the
/// call graph's `SymKey`s (grouped by file and name, across a project).
///
/// An item's ordinal is how many of its group's CALLABLE items sit on a
/// strictly earlier line — so callables sharing a line share an ordinal, and
/// an item that is not callable itself is numbered against the callables
/// around it, exactly as [`fn_ordinal`] counts. There is one implementation
/// because the ordinal is a cache and edge identity: two that disagreed would
/// not look wrong, they would silently orphan stored explanations and move
/// call-graph edges onto the wrong function.
pub(crate) struct Ordinals<K> {
    /// Per group, the lines of its callables, ascending.
    lines: HashMap<K, Vec<usize>>,
}

impl<K: Hash + Eq> Ordinals<K> {
    /// Index `items`, each `(group, is_callable, line)`, in any order.
    pub(crate) fn new(items: impl IntoIterator<Item = (K, bool, usize)>) -> Self {
        let mut lines: HashMap<K, Vec<usize>> = HashMap::new();
        for (group, callable, line) in items {
            if callable {
                lines.entry(group).or_default().push(line);
            }
        }
        for l in lines.values_mut() {
            l.sort_unstable();
        }
        Ordinals { lines }
    }

    /// The ordinal of an item of `group` declared on `line`.
    pub(crate) fn of(&self, group: &K, line: usize) -> u32 {
        self.lines.get(group).map_or(0, |lines| {
            // Strictly-less, so same-line callables share an ordinal.
            lines.partition_point(|l| {
                work::add(1);
                *l < line
            }) as u32
        })
    }
}

/// Whether a symbol kind is a callable (a function or a method).
pub fn is_callable(kind: &str) -> bool {
    matches!(kind, "function" | "method")
}

/// A work counter for the complexity tests: a hot loop reports its units of
/// work, and a test asserts a bound on the COUNT — which a loaded CI machine
/// cannot blow the way it can blow a wall-clock budget, and which fails for
/// the real reason (the loop went quadratic) rather than a slow box. Outside
/// test builds `add` is an empty inline function.
pub(crate) mod work {
    #[cfg(test)]
    thread_local! {
        static DONE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Record `n` units of work on this thread.
    #[inline(always)]
    pub(crate) fn add(n: usize) {
        #[cfg(test)]
        DONE.with(|d| d.set(d.get().saturating_add(n)));
        #[cfg(not(test))]
        let _ = n;
    }

    /// Run `f`, returning its result and the work it recorded. Per thread, so
    /// tests running in parallel do not count each other's work.
    #[cfg(test)]
    pub(crate) fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let before = DONE.with(std::cell::Cell::get);
        let out = f();
        (out, DONE.with(std::cell::Cell::get) - before)
    }
}

/// The syntactic container a Rust method is declared in, where it changes
/// what the method means for the API surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// Declared in a `trait { … }` body: as public as the trait.
    Trait,
    /// Declared in an `impl Trait for Type { … }` block: part of the type's
    /// API through the trait, although it carries no `pub`.
    TraitImpl,
    /// Declared in an inherent `impl Type { … }` block.
    Impl,
}

/// Field-wise equality for the wire [`Symbol`], which derives no `PartialEq`
/// (the protocol crate keeps its types minimal).
pub fn same_symbol(a: &Symbol, b: &Symbol) -> bool {
    a.name == b.name && a.kind == b.kind && a.line == b.line && a.end_line == b.end_line
}

/// A symbol plus the structural facts the wire [`Symbol`] does not carry,
/// read off the same syntax tree.
#[derive(Debug, Clone)]
pub struct Located {
    pub symbol: Symbol,
    /// 1-based line where the complete declaration starts — where its doc
    /// comment ends and its modifiers sit. For a GNU-style C definition
    /// (`static int` on one line, `foo(void)` on the next) that is the line
    /// ABOVE the name; for a C++ template the `template <…>` line; otherwise
    /// `symbol.line`.
    pub decl_line: usize,
    /// 1-based inclusive `(first, last)` lines of the whole definition
    /// including its body, or `None` for a declaration that has no body: a
    /// C/C++ prototype, an abstract or interface method, a TypeScript
    /// overload signature. Read off the tree, so unlike brace counting it is
    /// not fooled by braces in strings or comments, and a prototype never
    /// borrows the body of whatever function follows it.
    pub body: Option<(usize, usize)>,
    /// The container that changes a member's meaning (Rust methods only).
    pub container: Option<Container>,
    /// Who the definition belongs to (see [`Owner`]).
    pub owner: Owner,
}

/// Where a definition is declared, to tell same-name definitions of
/// DIFFERENT owners apart (see [`Owner::same_as`]): overloads share an owner;
/// `A::draw` and `B::draw`, or the `run` entries of two object literals, do
/// not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Owner {
    /// Byte span of the nearest enclosing body — a class, interface, enum or
    /// object-literal body, a namespace or function body, or the whole file.
    pub scope: (usize, usize),
    /// C++ only: the class or namespace the definition is a member of, when
    /// the syntax names it — the innermost qualifier of an out-of-class
    /// definition (`A` in `void ns::A::draw()`, template arguments dropped),
    /// or the class or namespace whose body holds the declaration. C++ lets
    /// one member be written in two places (declared in its class, defined
    /// after it), and only the name ties the two together; the scope alone
    /// would also call `A::draw` and `B::draw`, defined side by side, one
    /// owner's.
    pub member_of: Option<String>,
}

impl Owner {
    /// Whether two definitions belong to one owner: the same named class or
    /// namespace where the syntax names one for both, otherwise the same
    /// enclosing body. A named member and an unnamed one never match.
    pub fn same_as(&self, other: &Owner) -> bool {
        match (&self.member_of, &other.member_of) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.scope == other.scope,
            _ => false,
        }
    }
}

impl PartialEq for Located {
    fn eq(&self, other: &Self) -> bool {
        same_symbol(&self.symbol, &other.symbol)
            && self.decl_line == other.decl_line
            && self.body == other.body
            && self.container == other.container
            && self.owner == other.owner
    }
}

impl Eq for Located {}

impl Located {
    /// Whether this is a callable that exists only as a declaration.
    pub fn is_bodyless_callable(&self) -> bool {
        is_callable(&self.symbol.kind) && self.body.is_none()
    }
}

/// Extract definition symbols from `source`. Returns an empty list when the
/// language has no tags query or parsing fails. Blocking; run off the UI thread.
pub fn extract(source: &str, lang_key: &str) -> Vec<Symbol> {
    extract_located(source, lang_key)
        .into_iter()
        .map(|l| l.symbol)
        .collect()
}

/// [`extract`] with the structural facts of [`Located`].
pub fn extract_located(source: &str, lang_key: &str) -> Vec<Located> {
    try_extract_located(source, lang_key).unwrap_or_default()
}

/// [`extract_located`], telling "this language has no outline" and "its query
/// is broken" apart from "this file declares nothing".
pub fn try_extract_located(source: &str, lang_key: &str) -> Result<Vec<Located>, QueryError> {
    let lang = Lang::for_source(lang_key, source).ok_or(QueryError::Unsupported)?;
    // Compile (or fetch) the query before parsing, so a language without an
    // outline costs nothing.
    crate::highlight::tags_query(lang)?;
    let Some(tree) = crate::highlight::parse(source, lang) else {
        return Ok(Vec::new());
    };
    Ok(located_in(&tree, source, lang))
}

/// The outline of an already-parsed file. `tree` must be `source` parsed with
/// `lang`'s grammar.
pub fn located_in(tree: &Tree, source: &str, lang: Lang) -> Vec<Located> {
    let Ok(query) = crate::highlight::tags_query(lang) else {
        return Vec::new();
    };
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut out: Vec<Located> = Vec::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let mut kind: Option<&str> = None;
        let mut name: Option<&str> = None;
        let mut def: Option<Node> = None;
        for capture in m.captures {
            let capture_name = capture_names[capture.index as usize];
            if let Some(k) = capture_name.strip_prefix("definition.") {
                kind = Some(k);
                def = Some(capture.node);
            } else if capture_name == "name" {
                name = source.get(capture.node.byte_range()).or(name);
            }
        }
        let (Some(kind), Some(name), Some(def)) = (kind, name, def) else {
            continue;
        };
        let line = def.start_position().row + 1;
        let (decl_line, body) = shape_of(def, lang);
        // The whole definition's last line. For C/C++ the tag is the
        // `function_declarator` (`foo(int x)`), and for Dart the signature:
        // neither node reaches the body, so `end_line` stopped at the `)` and
        // a caret anywhere in the body was "in no function" to every lookup
        // that asks which symbol spans a line. The body span says where it
        // really ends.
        let end_line = body
            .map_or(0, |(_, last)| last)
            .max(def.end_position().row + 1)
            .max(line);
        out.push(Located {
            symbol: Symbol {
                name: name.to_string(),
                kind: refine_kind(def, kind, lang).to_string(),
                line,
                end_line,
            },
            decl_line: decl_line.min(line),
            body,
            container: container_of(def, lang),
            owner: owner_of(def, source, lang),
        });
    }
    // Stable, so of two patterns naming the same symbol the first match wins
    // (e.g. the Rust method pattern over the plain function one).
    out.sort_by(|a, b| {
        a.symbol
            .line
            .cmp(&b.symbol.line)
            .then_with(|| a.symbol.name.cmp(&b.symbol.name))
    });
    out.dedup_by(|a, b| a.symbol.line == b.symbol.line && a.symbol.name == b.symbol.name);
    out
}

/// A TypeScript class field whose value is a function (`handler = () => {…}`)
/// is a method in all but syntax — report it as one, so it is a call-graph
/// node and an explainable function rather than a data member.
fn refine_kind<'a>(def: Node, kind: &'a str, lang: Lang) -> &'a str {
    let function_valued = || {
        def.child_by_field_name("value")
            .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression"))
    };
    match lang {
        Lang::TypeScript | Lang::Tsx if kind == "field" && function_valued() => "method",
        _ => kind,
    }
}

/// `(decl_line, body)` for a definition node (see [`Located`]).
fn shape_of(def: Node, lang: Lang) -> (usize, Option<(usize, usize)>) {
    let first = |n: Node| n.start_position().row + 1;
    let last = |n: Node| n.end_position().row + 1;
    let whole = Some((first(def), last(def)));
    match lang {
        // The tag names the `function_declarator` alone (`foo(void)`), for a
        // definition and a prototype alike; the statement holding it decides
        // which one this is, and where the declaration starts.
        Lang::C | Lang::Cpp => {
            if def.kind() != "function_declarator" {
                let top = template_wrapper(def);
                return (first(top), whole);
            }
            let mut node = def;
            while let Some(parent) = node.parent() {
                match parent.kind() {
                    "pointer_declarator"
                    | "reference_declarator"
                    | "parenthesized_declarator"
                    | "attributed_declarator"
                    | "function_declarator" => node = parent,
                    "function_definition" => {
                        let top = template_wrapper(parent);
                        return (first(top), Some((first(top), last(parent))));
                    }
                    "declaration" | "field_declaration" => {
                        return (first(template_wrapper(parent)), None);
                    }
                    // A declarator in a parameter list, a typedef, … : not a
                    // function this file defines or declares at top level.
                    _ => return (first(def), None),
                }
            }
            (first(def), None)
        }
        // Dart has no node spanning a signature and its body: the body is the
        // next sibling of the (method) signature, and an abstract method has
        // none.
        Lang::Dart => {
            let anchor = match def.parent() {
                Some(p) if p.kind() == "method_signature" => p,
                _ => def,
            };
            if !matches!(
                anchor.kind(),
                "method_signature" | "function_signature" | "getter_signature" | "setter_signature"
            ) {
                return (first(def), whole);
            }
            let body = anchor
                .next_named_sibling()
                .filter(|n| n.kind() == "function_body")
                .map(|b| (first(anchor), last(b)));
            (first(anchor), body)
        }
        Lang::TypeScript | Lang::Tsx => match def.kind() {
            "function_signature" | "method_signature" | "abstract_method_signature" => {
                (first(def), None)
            }
            _ => (first(def), whole),
        },
        // An external (assembly-backed) function has no body.
        Lang::Go => match def.kind() {
            "function_declaration" | "method_declaration"
                if def.child_by_field_name("body").is_none() =>
            {
                (first(def), None)
            }
            _ => (first(def), whole),
        },
        // Interface and abstract methods have no body.
        Lang::Java => match def.kind() {
            "method_declaration" if def.child_by_field_name("body").is_none() => (first(def), None),
            _ => (first(def), whole),
        },
        Lang::Rust | Lang::Python | Lang::JavaScript => (first(def), whole),
        Lang::Json | Lang::Bash | Lang::Yaml | Lang::Toml | Lang::Html | Lang::Css | Lang::Zig => {
            (first(def), whole)
        }
    }
}

/// Most ancestors [`owner_of`] climbs looking for an enclosing body. A
/// definition sits a few levels below its body; the bound only matters for a
/// pathological nesting (a minified bundle), where it keeps the outline linear.
const MAX_OWNER_CLIMB: usize = 256;

/// The [`Owner`] of a definition node.
fn owner_of(def: Node, source: &str, lang: Lang) -> Owner {
    let mut node = def;
    let mut scope = None;
    for _ in 0..MAX_OWNER_CLIMB {
        let Some(parent) = node.parent() else {
            break;
        };
        node = parent;
        if is_scope(parent.kind()) {
            scope = Some(parent);
            break;
        }
    }
    let scope = scope.unwrap_or(node);
    Owner {
        scope: (scope.start_byte(), scope.end_byte()),
        member_of: match lang {
            Lang::Cpp => cpp_member_of(def, scope, source),
            _ => None,
        },
    }
}

/// See [`Owner::member_of`].
fn cpp_member_of(def: Node, scope: Node, source: &str) -> Option<String> {
    let text = |n: Node| source.get(n.byte_range());
    // Out of class: the qualifier right before the name. `a::b::c` nests to
    // the right (`a` :: (`b` :: `c`)), so descend to the innermost one.
    if def.kind() == "function_declarator"
        && let Some(mut q) = def
            .child_by_field_name("declarator")
            .filter(|d| d.kind() == "qualified_identifier")
    {
        while let Some(inner) = q
            .child_by_field_name("name")
            .filter(|n| n.kind() == "qualified_identifier")
        {
            q = inner;
        }
        return q.child_by_field_name("scope").and_then(text).map(type_name);
    }
    // In a class or namespace body: that class's or namespace's name.
    let holder = scope.parent()?;
    matches!(
        holder.kind(),
        "class_specifier" | "struct_specifier" | "union_specifier" | "namespace_definition"
    )
    .then(|| holder.child_by_field_name("name"))
    .flatten()
    .and_then(text)
    .map(type_name)
}

/// The last `::` segment of a C++ name, template arguments dropped:
/// `ns::Box<T>` → `Box`.
fn type_name(qualified: &str) -> String {
    let last = qualified.rsplit("::").next().unwrap_or(qualified);
    last.split('<').next().unwrap_or(last).trim().to_string()
}

/// Whether a node kind is a body that owns the definitions directly inside
/// it, across the grammars (class/interface/enum bodies, blocks, object
/// literals and types, namespace and field lists, file roots).
fn is_scope(kind: &str) -> bool {
    kind.ends_with("_body")
        || matches!(
            kind,
            "block"
                | "statement_block"
                | "compound_statement"
                | "declaration_list"
                | "field_declaration_list"
                | "object"
                | "object_type"
                | "program"
                | "module"
                | "translation_unit"
                | "source_file"
                | "compilation_unit"
        )
}

/// `node`, or the `template <…>` declaration(s) wrapping it.
fn template_wrapper(node: Node) -> Node {
    let mut top = node;
    while let Some(parent) = top.parent() {
        if parent.kind() == "template_declaration" {
            top = parent;
        } else {
            break;
        }
    }
    top
}

/// The container of a Rust method or associated type (see [`Container`]):
/// a `type Item = …;` inside an `impl` is the impl's, not a type of the
/// module — the docs index names it an associated type for that reason.
fn container_of(def: Node, lang: Lang) -> Option<Container> {
    match lang {
        Lang::Rust => {
            if !matches!(def.kind(), "function_item" | "type_item") {
                return None;
            }
            let list = def.parent().filter(|p| p.kind() == "declaration_list")?;
            let owner = list.parent()?;
            match owner.kind() {
                "trait_item" => Some(Container::Trait),
                "impl_item" if owner.child_by_field_name("trait").is_some() => {
                    Some(Container::TraitImpl)
                }
                "impl_item" => Some(Container::Impl),
                _ => None,
            }
        }
        Lang::Python
        | Lang::JavaScript
        | Lang::TypeScript
        | Lang::Tsx
        | Lang::Go
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
        | Lang::Zig => None,
    }
}

/// Everything clew reads off one file's syntax tree — outline, imports, call
/// sites — from a SINGLE parse. The explain gatherer used to parse every file
/// four times for these (twice for the outline alone).
#[derive(Debug, Clone)]
pub struct Analysis {
    /// The language the file was parsed as (a C++ `.h` reads as C++).
    pub lang: Lang,
    pub symbols: Vec<Located>,
    pub imports: Vec<crate::imports::RawImport>,
    pub calls: Vec<crate::projectcalls::CallSite>,
}

impl Analysis {
    /// The callable named `name` with same-name ordinal `ordinal` (numbered
    /// as [`fn_ordinals`] numbers them) — the definition an
    /// `explain::Node::Function` / `projectcalls::SymKey` identity names.
    pub fn function(&self, name: &str, ordinal: u32) -> Option<&Located> {
        let same: Vec<&Located> = self
            .symbols
            .iter()
            .filter(|l| l.symbol.name == name)
            .collect();
        let ordinals = Ordinals::new(
            same.iter()
                .map(|l| (name, is_callable(&l.symbol.kind), l.symbol.line)),
        );
        same.into_iter()
            .find(|l| is_callable(&l.symbol.kind) && ordinals.of(&name, l.symbol.line) == ordinal)
    }

    /// The call sites `item`'s OWN body makes: attributed to its name, on a
    /// line of its body, and not inside a same-name function nested in it.
    ///
    /// The name alone is not an identity. A file's second `new` (another
    /// impl's) is attributed under the same name as the first, so filtering by
    /// name listed the first `new`'s callees as the second's. Nothing for a
    /// declaration without a body.
    pub fn calls_made_by<'a>(
        &'a self,
        item: &'a Located,
    ) -> impl Iterator<Item = &'a crate::projectcalls::CallSite> + 'a {
        let name = item.symbol.name.as_str();
        let span = item.body;
        // Same-name callables whose bodies sit strictly inside this one's:
        // their calls carry the same caller name but are theirs.
        let nested: Vec<(usize, usize)> = span
            .map(|(first, last)| {
                self.symbols
                    .iter()
                    .filter(|l| {
                        l.symbol.name == name
                            && is_callable(&l.symbol.kind)
                            && !std::ptr::eq(*l, item)
                    })
                    .filter_map(|l| l.body)
                    .filter(|&(f, e)| first <= f && e <= last && (f, e) != (first, last))
                    .collect()
            })
            .unwrap_or_default();
        self.calls.iter().filter(move |c| {
            c.caller.as_deref() == Some(name)
                && span.is_some_and(|(first, last)| (first..=last).contains(&c.line))
                && !nested.iter().any(|&(f, e)| (f..=e).contains(&c.line))
        })
    }
}

/// Parse `source` once and derive its [`Analysis`]. `None` for an unknown
/// language key or a parser failure. Blocking; run off the UI thread.
pub fn analyze(source: &str, lang_key: &str) -> Option<Analysis> {
    let lang = Lang::for_source(lang_key, source)?;
    let tree = crate::highlight::parse(source, lang)?;
    let symbols = located_in(&tree, source, lang);
    let imports = crate::imports::imports_in(&tree, source, lang);
    let calls = crate::projectcalls::calls_in(&tree, source, lang, &symbols);
    Some(Analysis {
        lang,
        symbols,
        imports,
        calls,
    })
}

/// [`analyze`] for a caller that needs only the definitions (and their body
/// spans): one parse, no import or call-site pass — `imports` and `calls` are
/// left empty. `None` for an unknown language key or a parser failure.
pub fn analyze_definitions(source: &str, lang_key: &str) -> Option<Analysis> {
    let lang = Lang::for_source(lang_key, source)?;
    let tree = crate::highlight::parse(source, lang)?;
    Some(Analysis {
        lang,
        symbols: located_in(&tree, source, lang),
        imports: Vec::new(),
        calls: Vec::new(),
    })
}

/// Whether the function/method named `name` at 1-based `line1` in `lines` (the
/// file split into lines) is a test. Rust: a `#[…test…]` attribute above the
/// definition (`#[test]`, `#[tokio::test]`, `#[rstest]`, `#[test_case(…)]`, …).
/// Java: a JUnit test annotation (`@Test`, `@ParameterizedTest`, …).
/// Go/Python: the standard test-name convention. Pure text, no tree-sitter.
/// Lives in clew-core so the client's index and the server's project-symbol
/// snapshot classify identically.
pub fn is_test_fn(lines: &[&str], line1: usize, name: &str, lang: &str) -> bool {
    let Some(lang) = Lang::from_key(lang) else {
        return false;
    };
    match lang {
        Lang::Rust => {
            if line1 == 0 || line1 > lines.len() {
                return false;
            }
            // Scan upward over attributes, doc-comments and blank lines; a test
            // attribute anywhere in that run marks it. Stop at the first real line.
            let mut i = line1 - 1; // 0-based index of the definition line
            while i > 0 {
                i -= 1;
                let t = lines[i].trim();
                if t.is_empty() || t.starts_with("//") || t.starts_with("#!") {
                    continue;
                }
                if let Some(rest) = t.strip_prefix("#[") {
                    if attr_marks_test(rest) {
                        return true;
                    }
                    continue; // another attribute (e.g. #[cfg(...)]) — keep scanning
                }
                break; // a code line — the attribute run has ended
            }
            false
        }
        Lang::Go => {
            name.starts_with("Test") || name.starts_with("Benchmark") || name.starts_with("Fuzz")
        }
        Lang::Python => name.starts_with("test") || name.starts_with("Test"),
        Lang::Java => java_has_test_annotation(lines, line1, name),
        // Tests in these are calls (`it(…)`, `test(…)`, `TEST(…)`), not named
        // functions, or the language has no functions at all.
        Lang::JavaScript
        | Lang::TypeScript
        | Lang::Tsx
        | Lang::Dart
        | Lang::C
        | Lang::Cpp
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Zig => false,
    }
}

/// Whether a Java method carries a JUnit test annotation. A method's symbol
/// line is its FIRST modifier, annotations included, so the annotations sit
/// between that line and the one naming the method; any directly above it are
/// scanned too.
fn java_has_test_annotation(lines: &[&str], line1: usize, name: &str) -> bool {
    const TEST_ANNOTATIONS: &[&str] = &[
        "Test",
        "ParameterizedTest",
        "RepeatedTest",
        "TestFactory",
        "TestTemplate",
    ];
    let marks = |t: &str| {
        t.split_whitespace().any(|word| {
            word.strip_prefix('@').is_some_and(|a| {
                let a = a.split('(').next().unwrap_or(a);
                let a = a.rsplit('.').next().unwrap_or(a);
                TEST_ANNOTATIONS.contains(&a)
            })
        })
    };
    if line1 == 0 || line1 > lines.len() {
        return false;
    }
    let start = line1 - 1;
    // Down from the symbol line to the line that names the method (bounded:
    // a modifier run is never long).
    for t in lines.iter().skip(start).take(16) {
        if marks(t) {
            return true;
        }
        if t.contains(&format!("{name}(")) {
            break;
        }
    }
    // Up over a run of annotation and blank lines.
    let mut i = start;
    while i > 0 {
        i -= 1;
        let t = lines[i].trim();
        if t.is_empty() {
            continue;
        }
        if !t.starts_with('@') {
            break;
        }
        if marks(t) {
            return true;
        }
    }
    false
}

/// Whether one Rust attribute marks a test. `rest` is its text after `#[`.
///
/// Only the attribute PATH decides, plus the two attributes that carry a
/// condition. A `test` substring anywhere else belongs to somebody's feature
/// name or string literal — `#[cfg(feature = "contest")]`,
/// `#[serde(rename = "latest")]` — and matching those filed ordinary
/// functions under Tests and dropped them from the uncalled-function analysis.
fn attr_marks_test(rest: &str) -> bool {
    // Blank the string literals FIRST, so no later step can read inside one.
    // Doing it with a bare-token scan instead was not enough: the tokenizer
    // split on every non-word character, so `feature = "test-utils"` still
    // yielded a `test` token off the hyphen. Double quotes only: a `'` in an
    // attribute is a lifetime far more often than a char literal. (A raw
    // string's `\` is not an escape, so `r"a\"` is blanked one character
    // short — harmless, since the result is only scanned for bare words.)
    let rest = crate::highlight::without_string_literals(rest, &['"']);
    let path = rest
        .split(['(', '=', ']', ' ', '\t'])
        .next()
        .unwrap_or_default()
        .trim();
    // `#[tokio::test]`, `#[test_log::test]`: the final segment is the marker.
    let seg = path.rsplit("::").next().unwrap_or(path);
    // `#[cfg(test)]` directly on the item — it exists only in a test build.
    if seg == "cfg" {
        return attr_args(&rest).is_some_and(mentions_cfg_test);
    }
    // `#[cfg_attr(<condition>, <attr>, …)]` applies the attributes when the
    // condition holds, so only the attributes decide. Reading the condition
    // too would call `#[cfg_attr(test, derive(Debug))]` a test.
    if seg == "cfg_attr" {
        let Some(args) = attr_args(&rest) else {
            return false;
        };
        return top_level_parts(args)
            .into_iter()
            .skip(1)
            .any(attr_marks_test);
    }
    // `test`, `test_case`, `wasm_bindgen_test`, `traced_test`, `rstest`.
    seg == "test" || seg.starts_with("test_") || seg.ends_with("_test") || seg == "rstest"
}

/// What sits between an attribute's first `(` and its matching `)`, or `None`
/// when it takes no arguments.
fn attr_args(rest: &str) -> Option<&str> {
    let open = rest.find('(')?;
    let mut depth = 0usize;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `args` split on the commas at paren depth 0, each part trimmed.
fn top_level_parts(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in args.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(args[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(args[start..].trim());
    out
}

/// Whether a cfg predicate holds only in a test build: a bare `test` token
/// under an EVEN number of enclosing `not( … )` groups. `#[cfg(not(test))]`
/// marks the opposite — an item that exists everywhere BUT a test build — and
/// reading it as a marker filed ordinary functions under Tests.
fn mentions_cfg_test(args: &str) -> bool {
    let hit = |token: &str, groups: &[bool]| {
        token == "test" && groups.iter().filter(|negated| **negated).count() % 2 == 0
    };
    // One entry per open group, saying whether it is a `not( … )`.
    let mut groups: Vec<bool> = Vec::new();
    let mut token = String::new();
    for c in args.chars() {
        if c.is_alphanumeric() || c == '_' {
            token.push(c);
            continue;
        }
        if hit(&token, &groups) {
            return true;
        }
        let opens_negation = token == "not";
        token.clear();
        match c {
            '(' => groups.push(opens_negation),
            ')' => {
                groups.pop();
            }
            _ => {}
        }
    }
    hit(&token, &groups)
}

/// What makes a function an entry point: a place where execution enters the
/// project from OUTSIDE its own code — the process start, a request, a
/// command, a callback the runtime invokes — rather than a function project
/// code calls. Where a reader's "how does execution get here?" starts.
/// Tests are entries of their own kind and keep their own classification
/// ([`is_test_fn`]); a test is never an `EntryKind`.
///
/// Ordered strongest first: a `#[tokio::main] async fn main` is `Main`,
/// whatever else its attributes say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryKind {
    /// The program's start: `main` (in any language), and the framework
    /// mains that wrap it (`#[tokio::main]`, `#[rocket::launch]`, …).
    Main,
    /// A request handler: an HTTP route, a websocket, an RPC method, a
    /// message pattern — marked by its framework's decorator, attribute or
    /// annotation, or (Go) by its handler signature.
    Route,
    /// A command-line command or subcommand handler (`@click.command`,
    /// `@app.command`, `#[tauri::command]`, `@ShellMethod`, …).
    Command,
    /// A callback the runtime calls: a task, a scheduled job, an event
    /// listener, a cloud function, a native/FFI export.
    Handler,
}

impl EntryKind {
    /// Every kind, strongest first.
    pub const ALL: &'static [EntryKind] = &[
        EntryKind::Main,
        EntryKind::Route,
        EntryKind::Command,
        EntryKind::Handler,
    ];

    /// The stable key used on the wire and in the index cache.
    pub const fn key(self) -> &'static str {
        match self {
            EntryKind::Main => "main",
            EntryKind::Route => "route",
            EntryKind::Command => "command",
            EntryKind::Handler => "handler",
        }
    }

    /// The kind a [`key`](Self::key) names; `None` for a key this build does
    /// not know (a newer peer's), which reads as "not an entry".
    pub fn from_key(key: &str) -> Option<EntryKind> {
        EntryKind::ALL.iter().copied().find(|k| k.key() == key)
    }

    /// A short lowercase label for the UI and the model ("route", "command").
    pub const fn label(self) -> &'static str {
        match self {
            EntryKind::Main => "main",
            EntryKind::Route => "route",
            EntryKind::Command => "command",
            EntryKind::Handler => "handler",
        }
    }
}

/// Whether the function/method `name`, defined at 1-based `line1` of a file
/// (`lines`, split into lines) at project-relative `rel`, is an entry point,
/// and of which [`EntryKind`]. Pure text, like [`is_test_fn`], and for the
/// same reason: the client's index and the server's project-symbol snapshot
/// must classify identically.
///
/// What is read, per language:
/// - the name: `main` everywhere (`WinMain`/`wmain` in C and C++ too), and a
///   lone `handler`/`lambda_handler` function (the cloud-function shape);
/// - Rust: the attributes above the definition (`#[get("/")]`,
///   `#[tokio::main]`, `#[tauri::command]`, `#[no_mangle]`, …);
/// - Python, JavaScript, TypeScript: the decorators above it (`@app.route`,
///   `@router.get`, `@click.command`, `@shared_task`, `@Get()`, `@Cron()`,
///   …), and for TypeScript also the ones sharing its line; the Next.js
///   conventions (`pages/api/` handlers, `app/**/route.ts` method exports);
/// - Java: the annotations from the symbol's first modifier line down to the
///   line naming the method, and those directly above (`@GetMapping`,
///   `@Scheduled`, `@KafkaListener`, `@ShellMethod`, …);
/// - Go: the handler signatures on the definition line
///   (`http.ResponseWriter`, `*gin.Context`, `echo.Context`, `*fiber.Ctx`).
///
/// A test is never an entry (a `#[test] fn main` stays a test), and neither
/// is anything but a function or method.
pub fn entry_kind(
    lines: &[&str],
    line1: usize,
    name: &str,
    kind: &str,
    lang: &str,
    rel: &str,
) -> Option<EntryKind> {
    if !matches!(kind, "function" | "method") {
        return None;
    }
    let lang = Lang::from_key(lang)?;
    if is_test_fn(lines, line1, name, lang.key()) {
        return None;
    }
    let mut found: Option<EntryKind> = None;
    let mut mark = |k: EntryKind| {
        if found.is_none_or(|f| k < f) {
            found = Some(k);
        }
    };
    if name == "main"
        || (matches!(lang, Lang::C | Lang::Cpp) && matches!(name, "WinMain" | "wmain"))
    {
        mark(EntryKind::Main);
    }
    let def_line = line1
        .checked_sub(1)
        .and_then(|i| lines.get(i).copied())
        .unwrap_or("");
    match lang {
        Lang::Rust => {
            for attr in rust_attributes_above(lines, line1) {
                if let Some(k) = marker_kind(&attr) {
                    mark(k);
                }
            }
        }
        Lang::Python => {
            for head in decorators_above(lines, line1) {
                if let Some(k) = marker_kind(&head) {
                    mark(k);
                }
            }
            if kind == "function" && matches!(name, "handler" | "lambda_handler") {
                mark(EntryKind::Handler);
            }
        }
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            for head in decorators_above(lines, line1)
                .into_iter()
                .chain(leading_decorators(def_line).0)
            {
                if let Some(k) = marker_kind(&head) {
                    mark(k);
                }
            }
            let path = rel.replace('\\', "/");
            let in_pages_api = path.starts_with("pages/api/") || path.contains("/pages/api/");
            let is_route_file =
                path.rsplit('/').next().is_some_and(|f| {
                    matches!(f, "route.ts" | "route.js" | "route.tsx" | "route.jsx")
                }) && (path.starts_with("app/") || path.contains("/app/"));
            let nextjs_route = (in_pages_api && matches!(name, "handler" | "default"))
                || (is_route_file
                    && matches!(
                        name,
                        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
                    ));
            if nextjs_route {
                mark(EntryKind::Route);
            } else if kind == "function" && name == "handler" {
                mark(EntryKind::Handler);
            }
        }
        Lang::Java => {
            for head in java_annotations(lines, line1, name) {
                if let Some(k) = marker_kind(&head) {
                    mark(k);
                }
            }
        }
        Lang::Go => {
            // A handler is one by its signature; the parameter list may
            // continue on the next lines.
            let signature: String = lines
                .iter()
                .skip(line1.saturating_sub(1))
                .take(3)
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            if [
                "http.ResponseWriter",
                "*gin.Context",
                "echo.Context",
                "*fiber.Ctx",
                "*http.Request",
            ]
            .iter()
            .any(|needle| signature.contains(needle))
            {
                mark(EntryKind::Route);
            } else if kind == "function" && matches!(name, "handler" | "Handler" | "HandleRequest")
            {
                mark(EntryKind::Handler);
            }
        }
        Lang::Dart
        | Lang::C
        | Lang::Cpp
        | Lang::Zig
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css => {}
    }
    found
}

/// The heads of the Rust attributes in the run directly above `line1`
/// (`#[a::b(c)]` → `b`), blank and comment lines skipped, stopping at the
/// first code line. String literals are blanked first so nothing inside one
/// is read as a path.
fn rust_attributes_above(lines: &[&str], line1: usize) -> Vec<String> {
    let mut heads = Vec::new();
    if line1 == 0 || line1 > lines.len() {
        return heads;
    }
    let mut i = line1 - 1;
    while i > 0 {
        i -= 1;
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with("#!") {
            continue;
        }
        let Some(rest) = t.strip_prefix("#[") else {
            break;
        };
        let rest = crate::highlight::without_string_literals(rest, &['"']);
        let path = rest
            .split(['(', '=', ']', ' ', '\t'])
            .next()
            .unwrap_or_default()
            .trim();
        heads.push(path.rsplit("::").next().unwrap_or(path).to_string());
    }
    heads
}

/// The heads of the decorators in the run directly above `line1`
/// (`@a.b.c(d)` → `c`), blank and comment lines skipped, stopping at the
/// first other line. A decorator whose argument list runs over several lines
/// is read whole (the lines below its `@` line are its continuation while
/// they close more parentheses than they open); a line that carries a
/// declaration after its decorators (`@Post() create() {}`) is another
/// member's, and ends the run.
fn decorators_above(lines: &[&str], line1: usize) -> Vec<String> {
    let mut heads = Vec::new();
    if line1 == 0 || line1 > lines.len() {
        return heads;
    }
    let balance = |t: &str| {
        let t = crate::highlight::without_string_literals(t, &['"', '\'']);
        t.matches(')').count() as i64 - t.matches('(').count() as i64
    };
    // Parentheses closed on lines below the `@` line that opened them.
    let mut pending: i64 = 0;
    let mut i = line1 - 1;
    while i > 0 {
        i -= 1;
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("//") {
            continue;
        }
        if let Some(rest) = t.strip_prefix('@') {
            if pending > 0 {
                pending = (pending + balance(t)).max(0);
                heads.push(decorator_head(rest));
                continue;
            }
            let (own, tail) = leading_decorators(t);
            if !tail.is_empty() {
                break;
            }
            heads.extend(own);
            continue;
        }
        let closed = balance(t);
        if pending + closed > 0 {
            pending += closed;
            continue;
        }
        break;
    }
    heads
}

/// The decorators at the START of a line (`@Get() findAll() {`), for the
/// TypeScript style that keeps a decorator on the method's own line, and
/// what follows them on the line (empty when the line is decorators only).
fn leading_decorators(line: &str) -> (Vec<String>, &str) {
    let mut heads = Vec::new();
    let mut rest = line.trim_start();
    while let Some(after) = rest.strip_prefix('@') {
        heads.push(decorator_head(after));
        // Past this decorator's name and its argument list, if any.
        let name_end = after
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '$'))
            .unwrap_or(after.len());
        let mut cut = name_end;
        if after[name_end..].starts_with('(') {
            let mut depth = 0usize;
            for (i, c) in after[name_end..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            cut = name_end + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if cut == name_end {
                return (heads, ""); // an unclosed argument list: the line is the decorator's
            }
        }
        rest = after[cut..].trim_start();
    }
    (heads, rest)
}

/// `a.b.c(d)` → `c`: the last dotted segment of a decorator's name, before
/// its arguments.
fn decorator_head(rest: &str) -> String {
    let name = rest
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '$'))
        .next()
        .unwrap_or("");
    name.rsplit('.').next().unwrap_or(name).to_string()
}

/// The heads of a Java method's annotations: from the symbol's first
/// modifier line down to the line naming the method (bounded), and the run of
/// annotation lines directly above (see [`java_has_test_annotation`]).
fn java_annotations(lines: &[&str], line1: usize, name: &str) -> Vec<String> {
    let mut heads = Vec::new();
    if line1 == 0 || line1 > lines.len() {
        return heads;
    }
    let collect = |t: &str, heads: &mut Vec<String>| {
        for word in t.split_whitespace() {
            if let Some(a) = word.strip_prefix('@') {
                heads.push(decorator_head(a));
            }
        }
    };
    let start = line1 - 1;
    let needle = format!("{name}(");
    for t in lines.iter().skip(start).take(16) {
        collect(t, &mut heads);
        if t.contains(&needle) {
            break;
        }
    }
    let mut i = start;
    while i > 0 {
        i -= 1;
        let t = lines[i].trim();
        if t.is_empty() {
            continue;
        }
        if !t.starts_with('@') {
            break;
        }
        // Annotations only; a line that declares another member after its
        // annotations (`@Scheduled(...) public void tick()`) is that member's.
        let (own, tail) = leading_decorators(t);
        if !tail.is_empty() {
            break;
        }
        heads.extend(own);
    }
    heads
}

/// The entry kind a decorator/attribute/annotation head marks, by its name
/// alone, case-insensitively (`Get` in NestJS, `get` in FastAPI and Rocket).
/// Framework-neutral on purpose: the names below are the ones the common web,
/// CLI, task and FFI frameworks use, and a name no framework uses that way is
/// simply not here.
fn marker_kind(head: &str) -> Option<EntryKind> {
    const MAIN: &[&str] = &["main", "launch"];
    const ROUTE: &[&str] = &[
        "get",
        "post",
        "put",
        "delete",
        "patch",
        "head",
        "options",
        "all",
        "route",
        "api_route",
        "websocket",
        "ws",
        "handler",
        "debug_handler",
        "getmapping",
        "postmapping",
        "putmapping",
        "deletemapping",
        "patchmapping",
        "requestmapping",
        "messagemapping",
        "messagepattern",
        "eventpattern",
        "subscribemessage",
        "grpcmethod",
        "query",
        "update",
    ];
    const COMMAND: &[&str] = &[
        "command",
        "group",
        "subcommand",
        "shellmethod",
        "slash_command",
        "hybrid_command",
    ];
    const HANDLER: &[&str] = &[
        "task",
        "shared_task",
        "periodic_task",
        "flow",
        "dag",
        "on",
        "on_event",
        "listener",
        "event",
        "receiver",
        "message_handler",
        "callback",
        "cron",
        "interval",
        "timeout",
        "process",
        "scheduled",
        "scheduled_job",
        "eventlistener",
        "kafkalistener",
        "rabbitlistener",
        "jmslistener",
        "sqslistener",
        "http",
        "function_name",
        "errorhandler",
        "exception_handler",
        "before_request",
        "after_request",
        "no_mangle",
        "wasm_bindgen",
        "pyfunction",
        "pymodule",
        "export_name",
        "init",
        "pre_upgrade",
        "post_upgrade",
    ];
    let head = head.to_ascii_lowercase();
    let head = head.as_str();
    if MAIN.contains(&head) {
        Some(EntryKind::Main)
    } else if ROUTE.contains(&head) {
        Some(EntryKind::Route)
    } else if COMMAND.contains(&head) {
        Some(EntryKind::Command)
    } else if HANDLER.contains(&head) {
        Some(EntryKind::Handler)
    } else {
        None
    }
}

#[cfg(test)]
mod entry_tests {
    use super::*;

    fn kind_of(src: &str, line1: usize, name: &str, lang: &str) -> Option<EntryKind> {
        let lines: Vec<&str> = src.lines().collect();
        entry_kind(&lines, line1, name, "function", lang, "src/x")
    }

    #[test]
    fn main_is_an_entry_in_every_language_and_the_strongest_kind() {
        for lang in [
            "rust",
            "python",
            "go",
            "dart",
            "c",
            "cpp",
            "java",
            "javascript",
            "zig",
        ] {
            assert_eq!(
                kind_of("main() {}\n", 1, "main", lang),
                Some(EntryKind::Main),
                "{lang}"
            );
        }
        assert_eq!(
            kind_of("int WinMain() {}\n", 1, "WinMain", "cpp"),
            Some(EntryKind::Main)
        );
        assert_eq!(kind_of("int WinMain() {}\n", 1, "WinMain", "python"), None);
        // A framework main keeps its rank whatever else is on it.
        let src = "#[tokio::main]\n#[tracing::instrument]\nasync fn main() {}\n";
        assert_eq!(kind_of(src, 3, "main", "rust"), Some(EntryKind::Main));
        let src = "#[rocket::launch]\nfn rocket() -> _ {}\n";
        assert_eq!(kind_of(src, 2, "rocket", "rust"), Some(EntryKind::Main));
    }

    #[test]
    fn only_functions_and_methods_and_never_tests() {
        let lines = ["fn main() {}"];
        assert_eq!(
            entry_kind(&lines, 1, "main", "struct", "rust", "a.rs"),
            None
        );
        assert_eq!(
            entry_kind(&lines, 1, "main", "function", "unknown-lang", "a.rs"),
            None
        );
        let src = "#[test]\nfn main() {}\n";
        assert_eq!(kind_of(src, 2, "main", "rust"), None);
        assert_eq!(
            kind_of("def test_main(): pass\n", 1, "test_main", "python"),
            None
        );
        assert_eq!(
            kind_of(
                "func TestHandler(t *testing.T) {}\n",
                1,
                "TestHandler",
                "go"
            ),
            None
        );
    }

    #[test]
    fn rust_attributes_mark_routes_commands_and_handlers() {
        let src = "#[get(\"/users/<id>\")]\nfn user(id: u32) {}\n\
                   #[actix_web::post(\"/x\")]\nasync fn create() {}\n\
                   #[tauri::command]\nfn greet() {}\n\
                   #[no_mangle]\npub extern \"C\" fn plugin_init() {}\n\
                   #[derive(Debug)]\n#[inline]\nfn plain() {}\n\
                   // a comment between\n#[handler]\n\nfn poem_handler() {}\n\
                   #[cfg(feature = \"route\")]\nfn gated() {}\n";
        assert_eq!(kind_of(src, 2, "user", "rust"), Some(EntryKind::Route));
        assert_eq!(kind_of(src, 4, "create", "rust"), Some(EntryKind::Route));
        assert_eq!(kind_of(src, 6, "greet", "rust"), Some(EntryKind::Command));
        assert_eq!(
            kind_of(src, 8, "plugin_init", "rust"),
            Some(EntryKind::Handler)
        );
        assert_eq!(kind_of(src, 11, "plain", "rust"), None);
        assert_eq!(
            kind_of(src, 15, "poem_handler", "rust"),
            Some(EntryKind::Route)
        );
        // A string literal naming a marker is not a marker.
        assert_eq!(kind_of(src, 17, "gated", "rust"), None);
    }

    #[test]
    fn python_decorators_and_the_cloud_function_name() {
        let src = "@app.route(\"/\")\ndef index(): pass\n\
                   @router.get(\"/items\", response_model=Item)\nasync def items(): pass\n\
                   @cli.command()\n@click.option(\"--n\")\ndef sync(n): pass\n\
                   @shared_task\ndef send_mail(): pass\n\
                   @property\ndef size(self): pass\n\
                   def lambda_handler(event, context): pass\n\
                   @bot.event\nasync def on_message(m): pass\n\
                   @app.on_event(\"startup\")\ndef boot(): pass\n\
                   @pytest.fixture\ndef db(): pass\n";
        assert_eq!(kind_of(src, 2, "index", "python"), Some(EntryKind::Route));
        assert_eq!(kind_of(src, 4, "items", "python"), Some(EntryKind::Route));
        assert_eq!(kind_of(src, 7, "sync", "python"), Some(EntryKind::Command));
        assert_eq!(
            kind_of(src, 9, "send_mail", "python"),
            Some(EntryKind::Handler)
        );
        assert_eq!(kind_of(src, 11, "size", "python"), None);
        assert_eq!(
            kind_of(src, 12, "lambda_handler", "python"),
            Some(EntryKind::Handler)
        );
        assert_eq!(
            kind_of(src, 14, "on_message", "python"),
            Some(EntryKind::Handler)
        );
        assert_eq!(kind_of(src, 16, "boot", "python"), Some(EntryKind::Handler));
        assert_eq!(kind_of(src, 18, "db", "python"), None);
        // A decorator whose arguments span lines is read whole; a call that
        // happens to end above a def is not one.
        let src = "@app.route(\n    \"/multi\",\n    methods=[\"GET\", \"POST\"],\n)\n\
                   def multi(): pass\n\nx = call(1,\n    2)\ndef after_call(): pass\n";
        assert_eq!(kind_of(src, 5, "multi", "python"), Some(EntryKind::Route));
        assert_eq!(kind_of(src, 9, "after_call", "python"), None);
        // A method named handler is not the cloud-function shape.
        let lines: Vec<&str> = src.lines().collect();
        assert_eq!(
            entry_kind(&lines, 12, "lambda_handler", "method", "python", "x.py"),
            None
        );
    }

    #[test]
    fn typescript_decorators_on_their_own_line_or_the_methods_and_nextjs_files() {
        let src = "@Controller('cats')\nexport class Cats {\n  @Get()\n  findAll() {}\n\
                   @Post(':id') create() {}\n  @Cron('* * * * *') tick() {}\n\
                   @Injectable() helper() {}\n}\n";
        let lines: Vec<&str> = src.lines().collect();
        let of = |line1: usize, name: &str| {
            entry_kind(&lines, line1, name, "method", "typescript", "cats.ts")
        };
        assert_eq!(of(4, "findAll"), Some(EntryKind::Route));
        assert_eq!(of(5, "create"), Some(EntryKind::Route));
        assert_eq!(of(6, "tick"), Some(EntryKind::Handler));
        assert_eq!(of(7, "helper"), None);
        let lines = ["export default function handler(req, res) {}"];
        assert_eq!(
            entry_kind(
                &lines,
                1,
                "handler",
                "function",
                "javascript",
                "pages/api/hello.js"
            ),
            Some(EntryKind::Route)
        );
        assert_eq!(
            entry_kind(
                &lines,
                1,
                "handler",
                "function",
                "javascript",
                "src/lambda.js"
            ),
            Some(EntryKind::Handler)
        );
        let lines = ["export async function GET(req) {}", "function helper() {}"];
        assert_eq!(
            entry_kind(
                &lines,
                1,
                "GET",
                "function",
                "typescript",
                "app/api/users/route.ts"
            ),
            Some(EntryKind::Route)
        );
        assert_eq!(
            entry_kind(
                &lines,
                2,
                "helper",
                "function",
                "typescript",
                "app/api/users/route.ts"
            ),
            None
        );
        assert_eq!(
            entry_kind(&lines, 1, "GET", "function", "typescript", "lib/route.ts"),
            None
        );
    }

    #[test]
    fn java_annotations_on_the_modifier_lines_and_go_handler_signatures() {
        let src = "@RestController\npublic class C {\n  @GetMapping(\"/x\")\n  public List<X> all() {}\n\
                   @Scheduled(fixedRate = 5) public void tick() {}\n\
                   @Override\n  public String toString() {}\n\
                   @ShellMethod(\"do\")\n  public void run() {}\n\
                   public static void main(String[] a) {}\n}\n";
        let lines: Vec<&str> = src.lines().collect();
        let of =
            |line1: usize, name: &str| entry_kind(&lines, line1, name, "method", "java", "C.java");
        assert_eq!(of(3, "all"), Some(EntryKind::Route));
        assert_eq!(of(5, "tick"), Some(EntryKind::Handler));
        assert_eq!(of(6, "toString"), None);
        assert_eq!(of(8, "run"), Some(EntryKind::Command));
        assert_eq!(of(10, "main"), Some(EntryKind::Main));
        let src = "func hello(w http.ResponseWriter,\n\tr *http.Request) {}\n\
                   func ping(c *gin.Context) {}\nfunc add(a, b int) int {}\nfunc Handler() {}\n";
        assert_eq!(kind_of(src, 1, "hello", "go"), Some(EntryKind::Route));
        assert_eq!(kind_of(src, 3, "ping", "go"), Some(EntryKind::Route));
        assert_eq!(kind_of(src, 4, "add", "go"), None);
        assert_eq!(kind_of(src, 5, "Handler", "go"), Some(EntryKind::Handler));
    }

    #[test]
    fn keys_round_trip_and_unknown_keys_read_as_no_entry() {
        for k in EntryKind::ALL {
            assert_eq!(EntryKind::from_key(k.key()), Some(*k));
            assert_eq!(k.label(), k.key());
        }
        assert_eq!(EntryKind::from_key("teleport"), None);
        assert!(EntryKind::Main < EntryKind::Route && EntryKind::Route < EntryKind::Handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_symbols() {
        let src =
            "pub struct Point { x: f64 }\n\npub fn origin() -> Point {\n    Point { x: 0.0 }\n}\n";
        let symbols = extract(src, "rust");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Point"), "symbols: {symbols:?}");
        assert!(names.contains(&"origin"), "symbols: {symbols:?}");
        let origin = symbols.iter().find(|s| s.name == "origin").unwrap();
        assert_eq!(origin.line, 3);
    }

    #[test]
    fn extracts_dart_symbols() {
        let src = "class Point {\n  final double x;\n  Point(this.x);\n  double get magnitude => x;\n}\n\nPoint origin() => Point(0.0);\n";
        let symbols = extract(src, "dart");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Point"), "symbols: {symbols:?}");
        assert!(names.contains(&"origin"), "symbols: {symbols:?}");
    }

    #[test]
    fn extracts_typescript_source_symbols() {
        let src = "export type Kind = \"a\" | \"b\";\n\
                   export interface Token { kind: Kind }\n\
                   export function tokenize(s: string): Token[] { return []; }\n\
                   const isDigit = (c: string): boolean => c >= \"0\";\n\
                   export class Parser {\n  parse(): number { return 0; }\n}\n";
        let symbols = extract(src, "typescript");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        // The bundled query only found `Token`; the source-oriented one gets all.
        for want in ["Kind", "Token", "tokenize", "isDigit", "Parser", "parse"] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
    }

    #[test]
    fn extracts_typescript_interface_members_and_class_fields() {
        // Interface members (incl. function-typed properties, where libraries put
        // JSDoc) and class fields are surfaced — but an inline object-type
        // property in a parameter annotation is NOT swept in.
        let src = "export interface Chalk {\n\
                   \x20 rgb: (r: number, g: number, b: number) => Chalk;\n\
                   \x20 level: number;\n\
                   \x20 apply(opts: { inline: boolean }): void;\n\
                   }\n\
                   export class Styler {\n  cache = new Map();\n  build() {}\n}\n";
        let symbols = extract(src, "typescript");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        // Interface property members and the class field are present.
        for want in ["rgb", "level", "apply", "cache", "build"] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
        // The inline object-type property `inline` must NOT be captured.
        assert!(
            !names.contains(&"inline"),
            "over-captured inline type prop: {names:?}"
        );
        // Kinds are tagged correctly.
        let kind = |n: &str| {
            symbols
                .iter()
                .find(|s| s.name == n)
                .map(|s| s.kind.as_str())
        };
        assert_eq!(kind("rgb"), Some("property"));
        assert_eq!(kind("level"), Some("property"));
        assert_eq!(kind("cache"), Some("field"));
    }

    #[test]
    fn language_without_tags_query_yields_empty() {
        assert!(extract("{\"a\": 1}", "json").is_empty());
    }
}

#[cfg(test)]
mod ordinal_tests {
    use super::*;

    fn sym(name: &str, kind: &str, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: kind.to_string(),
            line,
            end_line: line,
        }
    }

    /// A list exercising every case the two definitions could disagree on:
    /// same-name callables of both kinds, a same-name NON-callable before and
    /// after them, two same-name callables on one line, and a name that only
    /// ever appears as a type.
    fn representative() -> Vec<Symbol> {
        vec![
            sym("helper", "function", 1),
            sym("new", "struct", 5),
            sym("new", "method", 10),
            sym("new", "function", 20),
            sym("new", "struct", 25),
            sym("new", "method", 30),
            sym("default", "method", 40),
            sym("dup", "method", 50),
            sym("dup", "function", 50),
            sym("dup", "method", 60),
            sym("Point", "struct", 70),
        ]
    }

    /// The ordinal is a cache identity, so `fn_ordinals` must reproduce the
    /// exact numbers the one-at-a-time scan produced, not merely agree with
    /// itself. These literals were captured by running the pre-existing
    /// `fn_ordinal` over `representative()` before `fn_ordinals` existed.
    #[test]
    fn batch_ordinals_match_the_captured_pre_change_output() {
        let syms = representative();
        assert_eq!(fn_ordinals(&syms), vec![0, 0, 0, 1, 2, 2, 0, 0, 0, 2, 0]);

        // Same multiset in the opposite order: `fn_ordinals` groups and sorts
        // rather than trusting `extract`'s line ordering, so a differently
        // ordered list must still reproduce the per-symbol scan.
        let mut rev = representative();
        rev.reverse();
        assert_eq!(fn_ordinals(&rev), vec![0, 2, 0, 0, 0, 2, 2, 1, 0, 0, 0]);
    }

    /// The invariant that keeps the two from drifting: equality at EVERY index,
    /// including the non-callables.
    #[test]
    fn batch_ordinals_equal_the_per_symbol_scan_at_every_index() {
        for syms in [
            representative(),
            extract(
                "impl A { fn new() {} fn go(&self) {} }\nimpl B { fn new() {} }\nfn new() {}\n",
                "rust",
            ),
        ] {
            let batch = fn_ordinals(&syms);
            let one_by_one: Vec<u32> = syms.iter().map(|s| fn_ordinal(&syms, s)).collect();
            assert_eq!(batch, one_by_one, "symbols: {syms:?}");
        }
    }

    /// The worst case for a per-symbol rescan — every symbol the same name —
    /// counted rather than timed: one binary search per symbol is
    /// N·(log₂N + 1) comparisons, where the quadratic form this replaced does
    /// N²/2 (1.8·10⁹ here). A count cannot flake on a loaded CI box, and it
    /// fails for the real reason.
    #[test]
    fn all_same_name_callables_stay_subquadratic() {
        const N: usize = 60_000;
        let syms: Vec<Symbol> = (1..=N).map(|i| sym("same", "method", i)).collect();
        let (ordinals, work) = work::measure(|| fn_ordinals(&syms));
        // Every entry is preceded by exactly the ones on lower lines.
        assert!(
            ordinals.iter().enumerate().all(|(i, o)| *o == i as u32),
            "ordinals diverged at large N"
        );
        // ⌈log₂N⌉ + 2 per symbol: a binary search's worst case plus slack.
        let log2 = (usize::BITS - N.leading_zeros()) as usize;
        let bound = N * (log2 + 2);
        assert!(
            work <= bound,
            "fn_ordinals did {work} comparisons for {N} symbols (bound {bound}) — the per-symbol rescan is back"
        );
    }
}

#[cfg(test)]
mod test_attr_tests {
    use super::*;

    /// A `test` substring in an attribute's ARGUMENTS is not a test marker.
    /// Matching it filed ordinary functions under Tests and excluded them
    /// from the uncalled-function analysis.
    #[test]
    fn arguments_containing_test_do_not_mark_a_test() {
        let src = "\
#[cfg(feature = \"contest\")]
fn helper() {}

#[serde(rename = \"latest\")]
fn renamed() {}
";
        let lines: Vec<&str> = src.lines().collect();
        assert!(!is_test_fn(&lines, 2, "helper", "rust"));
        assert!(!is_test_fn(&lines, 5, "renamed", "rust"));
    }

    /// The shapes that must keep matching.
    #[test]
    fn test_attribute_paths_still_match() {
        for attr in [
            "#[test]",
            "#[tokio::test]",
            "#[test_log::test]",
            "#[rstest]",
            "#[test_case(1, 2)]",
            "#[wasm_bindgen_test]",
            "#[cfg(test)]",
            "#[cfg(any(test, feature = \"x\"))]",
            "#[cfg(all(test, unix))]",
            // Double negation is still a test build.
            "#[cfg(not(not(test)))]",
            // `cfg_attr` applies the attribute, so the attribute decides.
            "#[cfg_attr(feature = \"e2e\", test)]",
            "#[cfg_attr(not(target_arch = \"wasm32\"), tokio::test)]",
        ] {
            let src = format!("{attr}\nfn f() {{}}\n");
            let lines: Vec<&str> = src.lines().collect();
            assert!(
                is_test_fn(&lines, 2, "f", "rust"),
                "{attr} must mark a test"
            );
        }
    }

    /// A `test` the predicate NEGATES, or one that only ever appears inside a
    /// string literal, marks the opposite of a test. Both used to match: the
    /// bare-token scan split `"test-utils"` on the hyphen, and it could not
    /// see `not(…)` at all.
    #[test]
    fn negated_and_quoted_tests_do_not_mark_a_test() {
        for attr in [
            "#[cfg(not(test))]",
            "#[cfg(all(not(test), unix))]",
            "#[cfg(feature = \"test-utils\")]",
            "#[cfg(feature = \"test\")]",
            "#[cfg(feature = \"integration-test\")]",
            // The CONDITION of a cfg_attr is not what gets applied.
            "#[cfg_attr(test, derive(Debug))]",
            "#[cfg_attr(all(test, unix), ignore)]",
        ] {
            let src = format!("{attr}\nfn f() {{}}\n");
            let lines: Vec<&str> = src.lines().collect();
            assert!(
                !is_test_fn(&lines, 2, "f", "rust"),
                "{attr} must NOT mark a test"
            );
        }
    }
}

#[cfg(test)]
mod located_tests {
    use super::*;

    fn find<'a>(items: &'a [Located], name: &str) -> &'a Located {
        items
            .iter()
            .find(|l| l.symbol.name == name)
            .unwrap_or_else(|| panic!("no {name} in {items:?}"))
    }

    /// The tree, not brace counting, says where a function's body is — and a
    /// C prototype has none, so it can never borrow the next function's body.
    /// A GNU-style definition starts on the return-type line above its name.
    #[test]
    fn c_definitions_have_bodies_and_prototypes_do_not() {
        let src = "/** Doc. */\nstatic int\nfoo(void)\n{\n  return 1;\n}\nint bar(int);\nint *baz(void);\n";
        let items = extract_located(src, "c");
        let foo = find(&items, "foo");
        assert_eq!(foo.symbol.line, 3, "the name's line");
        assert_eq!(foo.decl_line, 2, "the declaration starts at `static int`");
        assert_eq!(foo.body, Some((2, 6)));
        assert!(find(&items, "bar").is_bodyless_callable());
        assert!(
            find(&items, "baz").is_bodyless_callable(),
            "pointer declarator"
        );
    }

    /// The innermost symbol whose `[line, end_line]` holds `line1` — the
    /// lookup the explanation panel, the Ask context and the "reading
    /// context" make for the caret.
    fn at_caret(symbols: &[Symbol], line1: usize) -> Option<&str> {
        symbols
            .iter()
            .filter(|s| s.line <= line1 && line1 <= s.end_line)
            .min_by_key(|s| s.end_line - s.line)
            .map(|s| s.name.as_str())
    }

    /// `end_line` is the end of the whole definition. For C, C++ and Dart the
    /// tag is the declarator / signature, which stops before the body, so a
    /// caret inside the body used to be "in no function".
    #[test]
    fn end_line_covers_the_body_where_the_tag_does_not() {
        let c = "static int\nfoo(void)\n{\n  return 1;\n}\nint bar(int);\n";
        let symbols = extract(c, "c");
        let foo = symbols.iter().find(|s| s.name == "foo").unwrap();
        assert_eq!((foo.line, foo.end_line), (2, 5), "{symbols:?}");
        assert_eq!(at_caret(&symbols, 4), Some("foo"));
        let bar = symbols.iter().find(|s| s.name == "bar").unwrap();
        assert_eq!(bar.end_line, 6, "a prototype has no body to reach");

        let cpp =
            "class A {\n  void draw() {\n    paint();\n  }\n};\nvoid A::draw2() {\n  paint();\n}\n";
        let symbols = extract(cpp, "cpp");
        assert_eq!(at_caret(&symbols, 3), Some("draw"), "{symbols:?}");
        assert_eq!(at_caret(&symbols, 7), Some("draw2"), "{symbols:?}");

        let dart = "class A {\n  void g() {\n    f();\n  }\n}\n";
        let symbols = extract(dart, "dart");
        assert_eq!(at_caret(&symbols, 3), Some("g"), "{symbols:?}");

        // Where the tag already spans the body, nothing changes.
        let rust = "fn f() {\n    g();\n}\n";
        let f = &extract(rust, "rust")[0];
        assert_eq!((f.line, f.end_line), (1, 3));
    }

    /// A function's own calls are the calls in its body. Filtering by the
    /// caller NAME gave a file's second `new` the first one's callees too.
    #[test]
    fn calls_made_by_a_function_are_its_bodys_calls() {
        let src = "\
struct A;
struct B;
impl A {
    fn new() -> A { build_a() }
}
impl B {
    fn new() -> B {
        fn new() -> B { build_inner() }
        build_b()
    }
}
fn build_a() -> A { A }
fn build_b() -> B { B }
fn build_inner() -> B { B }
";
        let a = analyze(src, "rust").unwrap();
        let callees = |ordinal: u32| -> Vec<&str> {
            let f = a.function("new", ordinal).expect("the function exists");
            let mut v: Vec<&str> = a.calls_made_by(f).map(|c| c.callee.as_str()).collect();
            v.sort();
            v
        };
        assert_eq!(callees(0), vec!["build_a"]);
        assert_eq!(
            callees(1),
            vec!["build_b"],
            "the nested `new` keeps its own call"
        );
        assert_eq!(callees(2), vec!["build_inner"]);
        assert!(a.function("new", 3).is_none());
        assert!(a.function("A", 0).is_none(), "a type is not a function");

        // A declaration without a body makes no calls.
        let c = analyze("int f(void);\nint g(void) { return f(); }\n", "c").unwrap();
        let f = c.function("f", 0).unwrap();
        assert_eq!(c.calls_made_by(f).count(), 0);
    }

    /// A C++ member is owned by its class whether it is declared inside the
    /// class or defined outside it under a qualifier. Two classes' `draw`s
    /// are two owners; a namespace member is owned by its namespace.
    #[test]
    fn cpp_members_are_owned_by_their_class_wherever_written() {
        let src = "struct A { void draw(); };\nvoid A::draw() {}\nvoid B::draw() {}\nvoid free_fn() {}\n\
                   namespace geo {\ndouble area(double r);\n}\ndouble geo::area(double r) { return r; }\n";
        let items = extract_located(src, "cpp");
        let at = |line: usize| {
            &items
                .iter()
                .find(|l| l.symbol.line == line && is_callable(&l.symbol.kind))
                .unwrap_or_else(|| panic!("no callable on line {line}: {items:?}"))
                .owner
        };
        assert_eq!(at(1).member_of.as_deref(), Some("A"));
        assert_eq!(at(2).member_of.as_deref(), Some("A"));
        assert_eq!(at(3).member_of.as_deref(), Some("B"));
        assert_eq!(at(4).member_of, None);
        assert!(at(1).same_as(at(2)), "declared in A, defined as A::draw");
        assert!(!at(2).same_as(at(3)), "A::draw and B::draw");
        assert!(!at(3).same_as(at(4)), "a member and a free function");
        assert!(at(6).same_as(at(8)), "geo::area, declared and defined");

        // The qualifier that counts is the innermost, template arguments
        // dropped. (The bundled C++ tags query does not list these forms
        // today; the owner is read off the declarator all the same.)
        for (src, want) in [
            ("void ns::A::draw() {}\n", "A"),
            ("template <class T> void Box<T>::put(T) {}\n", "Box"),
        ] {
            let tree = crate::highlight::parse(src, Lang::Cpp).unwrap();
            let mut stack = vec![tree.root_node()];
            let mut declarator = None;
            while let Some(n) = stack.pop() {
                if n.kind() == "function_declarator" {
                    declarator = Some(n);
                    break;
                }
                let mut cursor = n.walk();
                stack.extend(n.children(&mut cursor));
            }
            let owner = owner_of(declarator.expect("a declarator"), src, Lang::Cpp);
            assert_eq!(owner.member_of.as_deref(), Some(want), "{src}");
        }

        // Elsewhere the enclosing body decides: two object literals' entries
        // are two owners, a class's methods one.
        let ts = "const a = { run: () => 1 };\nconst b = { run: () => 2 };\nclass K {\n  f(): void {}\n  g(): void {}\n}\n";
        let items = extract_located(ts, "typescript");
        let owner = |name: &str, line: usize| {
            items
                .iter()
                .find(|l| l.symbol.name == name && l.symbol.line == line)
                .map(|l| l.owner.clone())
                .unwrap_or_else(|| panic!("no {name}@{line}: {items:?}"))
        };
        assert!(!owner("run", 1).same_as(&owner("run", 2)));
        assert!(owner("f", 4).same_as(&owner("g", 5)));
    }

    #[test]
    fn a_cpp_template_declaration_starts_at_its_template_line() {
        let src = "template <typename T>\nT biggest(T a, T b) {\n  return a;\n}\n";
        let items = extract_located(src, "cpp");
        let f = find(&items, "biggest");
        assert_eq!(f.decl_line, 1);
        assert_eq!(f.body, Some((1, 4)));
    }

    /// Overload signatures, interface members and abstract methods declare a
    /// function without defining one.
    #[test]
    fn bodyless_declarations_across_languages() {
        let ts = "export function make<T>(x: T): T;\nexport function make(x: unknown) {\n  return x;\n}\ninterface I { run(): void; }\n";
        let items = extract_located(ts, "typescript");
        let makes: Vec<&Located> = items.iter().filter(|l| l.symbol.name == "make").collect();
        assert_eq!(makes.len(), 2, "{items:?}");
        assert_eq!(makes[0].body, None, "the overload signature");
        assert_eq!(makes[1].body, Some((2, 4)), "the implementation");
        assert!(find(&items, "run").is_bodyless_callable());

        let java = "abstract class A {\n  abstract void f();\n  void g() {\n    f();\n  }\n}\n";
        let items = extract_located(java, "java");
        assert!(find(&items, "f").is_bodyless_callable());
        assert_eq!(find(&items, "g").body, Some((3, 5)));

        let dart =
            "abstract class A {\n  void f();\n  void g() {\n    f();\n  }\n}\nvoid top() {}\n";
        let items = extract_located(dart, "dart");
        assert!(find(&items, "f").is_bodyless_callable(), "{items:?}");
        assert_eq!(find(&items, "g").body, Some((3, 5)), "{items:?}");
        assert_eq!(find(&items, "top").body, Some((7, 7)), "{items:?}");

        let go = "package p\nfunc F() {\n\tG()\n}\n";
        assert_eq!(find(&extract_located(go, "go"), "F").body, Some((2, 4)));
    }

    /// A function-valued class field is a method (TypeScript and JavaScript),
    /// and a Java constructor is an outline entry the call graph can use.
    #[test]
    fn function_valued_fields_and_constructors_are_callables() {
        let ts = "class K {\n  handler = () => { go(); };\n  count = 0;\n}\n";
        let items = extract_located(ts, "typescript");
        assert_eq!(find(&items, "handler").symbol.kind, "method");
        assert_eq!(find(&items, "count").symbol.kind, "field");

        let js = "class K {\n  handler = () => { go(); };\n}\n";
        assert_eq!(
            find(&extract_located(js, "javascript"), "handler")
                .symbol
                .kind,
            "method"
        );

        let java = "class A {\n  A() { init(); }\n  void init() {}\n}\n";
        let items = extract_located(java, "java");
        assert!(
            items
                .iter()
                .any(|l| l.symbol.name == "A" && l.symbol.kind == "method"),
            "{items:?}"
        );
    }

    #[test]
    fn rust_methods_know_their_container() {
        let src = "trait T {\n  fn provided(&self) {}\n  fn required(&self);\n}\nstruct S;\nimpl T for S {\n  fn required(&self) {}\n}\nimpl S {\n  fn inherent(&self) {}\n}\nfn free() {}\n";
        let items = extract_located(src, "rust");
        assert_eq!(find(&items, "provided").container, Some(Container::Trait));
        let required: Vec<&Located> = items
            .iter()
            .filter(|l| l.symbol.name == "required")
            .collect();
        // The trait's own `fn required(&self);` is not a function_item, so only
        // the implementation is an outline entry.
        assert_eq!(required.len(), 1, "{items:?}");
        assert_eq!(required[0].container, Some(Container::TraitImpl));
        assert_eq!(find(&items, "inherent").container, Some(Container::Impl));
        assert_eq!(find(&items, "free").container, None);

        // An impl's associated type is the impl's; a module's alias is nobody's.
        let src = "struct S;\nimpl Iterator for S {\n  type Item = u8;\n  fn next(&mut self) -> Option<u8> { None }\n}\nimpl S {\n  type Own = u16;\n}\ntype Free = u32;\n";
        let items = extract_located(src, "rust");
        assert_eq!(find(&items, "Item").container, Some(Container::TraitImpl));
        assert_eq!(find(&items, "Own").container, Some(Container::Impl));
        assert_eq!(find(&items, "Free").container, None);
    }

    /// One parse yields all three facts, identical to the separate extractors.
    #[test]
    fn analyze_matches_the_separate_extractors() {
        let src = "use crate::a::b;\nfn helper() {}\nfn caller() {\n    helper();\n}\n";
        let a = analyze(src, "rust").expect("rust parses");
        assert_eq!(a.lang, Lang::Rust);
        let separate = extract(src, "rust");
        assert_eq!(a.symbols.len(), separate.len());
        for (l, s) in a.symbols.iter().zip(&separate) {
            assert!(same_symbol(&l.symbol, s), "{l:?} vs {s:?}");
        }
        assert_eq!(a.imports, crate::imports::imports_of(src, "rust"));
        assert_eq!(a.calls, crate::projectcalls::calls_of(src, "rust"));
        assert!(analyze(src, "klingon").is_none());
    }

    #[test]
    fn a_language_without_an_outline_is_an_error_not_an_empty_file() {
        assert_eq!(
            try_extract_located("{}", "json"),
            Err(QueryError::Unsupported)
        );
        assert_eq!(
            try_extract_located("x", "klingon"),
            Err(QueryError::Unsupported)
        );
        assert_eq!(try_extract_located("", "rust"), Ok(Vec::new()));
    }

    #[test]
    fn junit_annotations_mark_java_tests() {
        let src = "class T {\n  @Test\n  void checks() {}\n  @ParameterizedTest @ValueSource(ints = {1})\n  void many(int x) {}\n  void helper() {}\n}\n";
        let lines: Vec<&str> = src.lines().collect();
        let items = extract(src, "java");
        let line = |n: &str| items.iter().find(|s| s.name == n).unwrap().line;
        assert!(is_test_fn(&lines, line("checks"), "checks", "java"));
        assert!(is_test_fn(&lines, line("many"), "many", "java"));
        assert!(!is_test_fn(&lines, line("helper"), "helper", "java"));
    }
}
