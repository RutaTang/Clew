//! The project's types and how they relate: a node per struct, class, enum,
//! interface, trait, union or type alias the API index lists, and an edge
//! where one names another — in its declaration ([`Relation::Inherits`]:
//! `extends`, `implements`, `with`, a Python base, a Rust supertrait or
//! trait impl, a C++ base) or in its own members and their signatures
//! ([`Relation::Uses`]: a field's type, a variant's payload, a method's
//! parameter or return type).
//!
//! Built from the Docs index (`DocItem::refs`, the words a type's text
//! names; see `clew_core::apidoc`) and, for Rust, the structure index (whose
//! `impl Trait for Type` blocks the docs do not list), resolved against the
//! project's own type names: a word that is no project type is dropped, so
//! `Vec`, `String` and a field's name cost nothing. A name resolves only
//! within its language, and only to a kind it can mean: an inherited name
//! to a kind one can inherit from (in Rust, a trait — `impl Read for X`
//! names `std::io::Read`, never a project enum or struct called `Read`).
//! When several project types share a name, the one in the same file wins,
//! then the one in the same folder; past that the name is ambiguous and
//! draws no edge — a wrong edge misleads, a missing one only omits. Types
//! declared inside a function are local to it and are not nodes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use clew_protocol::{DocFile, DocItem, StructureIndex};

/// The item kinds that are types.
pub const TYPE_KINDS: &[&str] = &[
    "struct",
    "class",
    "enum",
    "interface",
    "trait",
    "type",
    "union",
];

/// Whether an item of `kind` is a function or method (whose body's types are
/// its own).
pub(crate) fn is_callable_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function" | "method" | "constructor" | "fn" | "func" | "procedure"
    )
}

/// The languages whose types can name one another's: a TypeScript file and a
/// JavaScript one, a C header and the C++ that includes it.
fn family(lang: Option<&str>) -> Option<&str> {
    match lang? {
        "typescript" | "tsx" | "javascript" | "jsx" => Some("js"),
        "c" | "cpp" => Some("c"),
        other => Some(other),
    }
}

/// Whether a type of `kind` can be inherited from by a type of `lang`: in
/// Rust only a trait is implemented or extended; elsewhere anything but an
/// enum or a union can be a base.
fn inheritable(kind: &str, lang: Option<&str>) -> bool {
    if lang == Some("rust") {
        kind == "trait"
    } else {
        !matches!(kind, "enum" | "union")
    }
}

/// How one type relates to another, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Relation {
    /// The first extends, implements or is a subtype of the second.
    Inherits,
    /// The first names the second in a field, a variant, or a member's
    /// signature.
    Uses,
}

/// One type of the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeNode {
    pub name: String,
    pub kind: String,
    /// Absolute path of the defining file.
    pub file: PathBuf,
    /// Project-relative path of the defining file.
    pub rel: String,
    /// 1-based definition line.
    pub line: usize,
    pub public: bool,
}

/// The types and their relations (see the module docs).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TypeGraph {
    pub nodes: Vec<TypeNode>,
    /// `(from, to, relation)`, each pair once, `Inherits` winning over `Uses`.
    pub edges: Vec<(usize, usize, Relation)>,
    fan_in: Vec<usize>,
    fan_out: Vec<usize>,
    subtypes: Vec<usize>,
}

impl TypeGraph {
    /// From the API index of every file (`files`, as the server or the local
    /// build lists them) and the Rust structure index. Deterministic: nodes
    /// by path then line, edges by their nodes.
    pub fn build(root: &Path, files: &[DocFile], structure: &StructureIndex) -> TypeGraph {
        struct Raw {
            node: TypeNode,
            refs: Vec<String>,
            signature: String,
            lang: Option<&'static str>,
        }
        let mut raws: Vec<Raw> = Vec::new();
        fn collect(
            items: &[DocItem],
            parent_kind: Option<&str>,
            rel: &str,
            file: &Path,
            lang: Option<&'static str>,
            out: &mut Vec<Raw>,
        ) {
            for item in items {
                // A type declared in a function body is that function's.
                let local = parent_kind.is_some_and(is_callable_kind);
                if TYPE_KINDS.contains(&item.kind.as_str()) && !local {
                    out.push(Raw {
                        node: TypeNode {
                            name: item.name.clone(),
                            kind: item.kind.clone(),
                            file: file.to_path_buf(),
                            rel: rel.to_string(),
                            line: item.line,
                            public: item.public,
                        },
                        refs: item.refs.clone(),
                        signature: item.signature.clone(),
                        lang,
                    });
                }
                collect(&item.children, Some(&item.kind), rel, file, lang, out);
            }
        }
        for f in files {
            let file = root.join(&f.rel);
            let lang = crate::highlight::detect(&file);
            collect(&f.items, None, &f.rel, &file, lang, &mut raws);
        }
        raws.sort_by(|a, b| {
            a.node
                .rel
                .cmp(&b.node.rel)
                .then_with(|| a.node.line.cmp(&b.node.line))
                .then_with(|| a.node.name.cmp(&b.node.name))
        });
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, r) in raws.iter().enumerate() {
            by_name.entry(r.node.name.as_str()).or_default().push(i);
        }
        // `name` as type `from` means it, or `None` (see the module docs).
        let resolve = |name: &str, from: usize, inherited: bool| -> Option<usize> {
            let last = name.rsplit(['.', ':']).next().unwrap_or(name);
            let here = &raws[from];
            let fits: Vec<usize> = by_name
                .get(last)?
                .iter()
                .copied()
                .filter(|&j| j != from)
                .filter(|&j| family(raws[j].lang) == family(here.lang))
                .filter(|&j| !inherited || inheritable(&raws[j].node.kind, here.lang))
                .collect();
            let only = |picked: Vec<usize>| (picked.len() == 1).then(|| picked[0]);
            if fits.len() <= 1 {
                return fits.first().copied();
            }
            let dir = |rel: &str| rel.rsplit_once('/').map_or("", |(d, _)| d).to_string();
            only(
                fits.iter()
                    .copied()
                    .filter(|&j| raws[j].node.rel == here.node.rel)
                    .collect(),
            )
            .or_else(|| {
                only(
                    fits.iter()
                        .copied()
                        .filter(|&j| dir(&raws[j].node.rel) == dir(&here.node.rel))
                        .collect(),
                )
            })
        };
        // The structure index knows a Rust type's trait impls by the type's
        // bare name. With two Rust types of one name it cannot say whose an
        // impl is, and giving each all of them drew edges for impls it never
        // had: such a type draws none from it.
        let mut rust_named: HashMap<&str, usize> = HashMap::new();
        for r in raws.iter().filter(|r| r.lang == Some("rust")) {
            *rust_named.entry(r.node.name.as_str()).or_default() += 1;
        }
        let mut inherits: HashSet<(usize, usize)> = HashSet::new();
        let mut uses: HashSet<(usize, usize)> = HashSet::new();
        for (i, r) in raws.iter().enumerate() {
            for base in inherited_names(&r.signature, r.lang) {
                if let Some(j) = resolve(&base, i, true) {
                    inherits.insert((i, j));
                }
            }
            if r.lang == Some("rust")
                && rust_named.get(r.node.name.as_str()) == Some(&1)
                && let Some(ts) = structure.by_type.get(&r.node.name)
            {
                for t in &ts.traits {
                    if let Some(j) = resolve(t, i, true) {
                        inherits.insert((i, j));
                    }
                }
            }
            for name in &r.refs {
                if let Some(j) = resolve(name, i, false)
                    && !inherits.contains(&(i, j))
                {
                    uses.insert((i, j));
                }
            }
        }
        let mut edges: Vec<(usize, usize, Relation)> = inherits
            .into_iter()
            .map(|(a, b)| (a, b, Relation::Inherits))
            .chain(uses.into_iter().map(|(a, b)| (a, b, Relation::Uses)))
            .collect();
        edges.sort();
        let n = raws.len();
        let mut fan_in = vec![0; n];
        let mut fan_out = vec![0; n];
        let mut subtypes = vec![0; n];
        for &(a, b, rel) in &edges {
            fan_out[a] += 1;
            fan_in[b] += 1;
            if rel == Relation::Inherits {
                subtypes[b] += 1;
            }
        }
        TypeGraph {
            nodes: raws.into_iter().map(|r| r.node).collect(),
            edges,
            fan_in,
            fan_out,
            subtypes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// How many edges are inheritance.
    pub fn inherits_count(&self) -> usize {
        self.edges
            .iter()
            .filter(|e| e.2 == Relation::Inherits)
            .count()
    }

    /// How many types name this one.
    pub fn fan_in(&self, id: usize) -> usize {
        self.fan_in.get(id).copied().unwrap_or(0)
    }

    /// How many types this one names.
    pub fn fan_out(&self, id: usize) -> usize {
        self.fan_out.get(id).copied().unwrap_or(0)
    }

    /// How many types inherit from this one.
    pub fn subtypes(&self, id: usize) -> usize {
        self.subtypes.get(id).copied().unwrap_or(0)
    }

    /// The types most others name, most first, then by name; only those
    /// named at all.
    pub fn most_referenced(&self, limit: usize) -> Vec<usize> {
        self.ranked(limit, |id| self.fan_in(id))
    }

    /// The types most others inherit from.
    pub fn most_derived(&self, limit: usize) -> Vec<usize> {
        self.ranked(limit, |id| self.subtypes(id))
    }

    /// The types naming the most others.
    pub fn most_dependent(&self, limit: usize) -> Vec<usize> {
        self.ranked(limit, |id| self.fan_out(id))
    }

    fn ranked(&self, limit: usize, count: impl Fn(usize) -> usize) -> Vec<usize> {
        let mut ids: Vec<usize> = (0..self.nodes.len()).filter(|&id| count(id) > 0).collect();
        ids.sort_by(|&a, &b| {
            count(b)
                .cmp(&count(a))
                .then_with(|| self.nodes[a].name.cmp(&self.nodes[b].name))
                .then_with(|| self.nodes[a].rel.cmp(&self.nodes[b].rel))
        });
        ids.truncate(limit);
        ids
    }

    /// The edges without their relation, each pair once, for a layout.
    pub fn layout_edges(&self) -> Vec<(usize, usize)> {
        let mut pairs: Vec<(usize, usize)> = self.edges.iter().map(|&(a, b, _)| (a, b)).collect();
        pairs.dedup();
        pairs
    }
}

/// The names a type's declaration says it extends, implements or is based
/// on: after `extends`, `implements` and `with` (Java, TypeScript, Dart,
/// PHP, Scala); a Python class's bases; a Rust trait's supertraits and a C++
/// class's bases after `:`. Generic arguments are stripped first, so a bound
/// inside `<…>` is not read as a base; each name's last path segment is what
/// comes back.
pub fn inherited_names(signature: &str, lang: Option<&str>) -> Vec<String> {
    let flat = without_angle_brackets(signature);
    let flat = flat.split('{').next().unwrap_or("").trim();
    let mut names = Vec::new();
    let mut push = |part: &str| {
        let part = part.trim();
        if part.is_empty() || part.contains('=') {
            return;
        }
        let word = part
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == ':'))
            .find(|w| !w.is_empty() && !is_access_word(w));
        if let Some(w) = word {
            let last = w.rsplit(['.', ':']).next().unwrap_or(w);
            if !last.is_empty() && !names.iter().any(|n| n == last) {
                names.push(last.to_string());
            }
        }
    };
    for keyword in ["extends", "implements", "with"] {
        let mut rest = flat;
        while let Some(at) = find_word(rest, keyword) {
            let after = &rest[at + keyword.len()..];
            let end = ["extends", "implements", "with", "where", "permits"]
                .iter()
                .filter_map(|k| find_word(after, k))
                .min()
                .unwrap_or(after.len());
            for part in after[..end].split(',') {
                push(part);
            }
            rest = &after[end..];
        }
    }
    match lang {
        Some("python") => {
            if let Some(open) = flat.find('(')
                && let Some(close) = flat[open..].find(')')
            {
                for part in flat[open + 1..open + close].split(',') {
                    push(part);
                }
            }
        }
        Some("rust") => {
            // Visibility restrictions and `unsafe` precede the trait, and
            // `pub(in crate::m)` itself contains colons. Its syntax field
            // identifies the supertraits without borrowing a generic bound
            // or a predicate from the `where` clause.
            if let Some(bounds) = rust_supertraits(signature) {
                for part in bounds.split('+') {
                    push(part);
                }
            }
        }
        Some("cpp") | Some("c") => {
            if let Some(colon) = flat.find(':')
                && !flat[colon..].starts_with("::")
            {
                for part in flat[colon + 1..].split(',') {
                    push(part);
                }
            }
        }
        _ => {}
    }
    names
}

fn rust_supertraits(signature: &str) -> Option<String> {
    find_word(signature, "trait")?;
    let signature = signature.trim_end();
    let source = if signature.ends_with('{') {
        format!("{signature}}}")
    } else if !signature.contains('{') {
        format!("{signature} {{}}")
    } else {
        signature.to_string()
    };
    let tree = clew_core::highlight::parse(&source, clew_core::highlight::Lang::Rust)?;
    let mut cursor = tree.root_node().walk();
    let item = tree
        .root_node()
        .named_children(&mut cursor)
        .find(|n| n.kind() == "trait_item")?;
    let bounds = item.child_by_field_name("bounds")?;
    let text = source.get(bounds.byte_range())?.trim_start_matches(':');
    Some(without_angle_brackets(text))
}

fn is_access_word(w: &str) -> bool {
    matches!(
        w,
        "public" | "private" | "protected" | "virtual" | "final" | "abstract"
    )
}

/// `text` with every balanced `<…>` removed.
fn without_angle_brackets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Where `word` occurs in `text` as a whole word.
fn find_word(text: &str, word: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = text[from..].find(word) {
        let at = from + at;
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        let bounded = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if bounded(before) && bounded(after) {
            return Some(at);
        }
        from = at + word.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, kind: &str, line: usize, signature: &str, refs: &[&str]) -> DocItem {
        DocItem {
            name: name.into(),
            kind: kind.into(),
            signature: signature.into(),
            doc: String::new(),
            line,
            public: true,
            children: Vec::new(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn inherited_names_come_from_each_languages_declaration() {
        let of = |sig: &str, lang: &str| inherited_names(sig, Some(lang));
        assert_eq!(
            of(
                "export class Cart<T extends Base> extends Shop implements Priced, api.Sized {",
                "typescript"
            ),
            ["Shop", "Priced", "Sized"]
        );
        assert_eq!(
            of("class Order(Base, Mixin, metaclass=Meta):", "python"),
            ["Base", "Mixin"]
        );
        assert_eq!(
            of(
                "pub trait Shape: Drawable + Send where Self: Sized {",
                "rust"
            ),
            ["Drawable", "Send"]
        );
        assert_eq!(
            of("pub struct Point<T: Into<f64>> {", "rust"),
            Vec::<String>::new()
        );
        assert_eq!(
            of("class Circle : public Shape, private Named {", "cpp"),
            ["Shape", "Named"]
        );
        assert_eq!(of("class A extends B with C, D {", "dart"), ["B", "C", "D"]);
    }

    #[test]
    fn rust_trait_modifiers_preserve_supertrait_edges() {
        let source = "pub trait Base {}\n\
                      pub(crate) trait Restricted: Base {}\n\
                      pub unsafe trait Unsafe: Base {}\n\
                      pub trait Ordinary: Base {}\n";
        let items = clew_core::apidoc::build_file(source, "rust");
        let graph = TypeGraph::build(
            Path::new("/p"),
            &[DocFile {
                rel: "src/lib.rs".into(),
                doc: String::new(),
                items,
            }],
            &StructureIndex::default(),
        );
        assert_eq!(graph.inherits_count(), 3);
        assert!(graph.edges.iter().all(|e| e.2 == Relation::Inherits));
        let base = graph.nodes.iter().position(|n| n.name == "Base").unwrap();
        assert_eq!(graph.subtypes(base), 3);
        assert_eq!(graph.most_derived(8), [base]);
        for signature in [
            "pub(super) trait Child<T: Other>: Base<T> where T: Last",
            "pub(in crate::inner) unsafe trait Child<T: Other>: Base<T> where T: Last",
        ] {
            assert_eq!(
                inherited_names(signature, Some("rust")),
                ["Base"],
                "{signature}"
            );
        }
        assert!(inherited_names("pub(crate) struct Child<T: Base>", Some("rust")).is_empty());
        assert!(
            inherited_names(
                "pub(crate) trait Child<T: Base> where T: Other",
                Some("rust")
            )
            .is_empty()
        );
    }

    #[test]
    fn the_graph_resolves_refs_against_project_types_and_ranks_them() {
        let root = Path::new("/p");
        let files = vec![
            DocFile {
                doc: String::new(),
                rel: "src/model.rs".into(),
                items: vec![
                    item(
                        "Order",
                        "struct",
                        1,
                        "pub struct Order {",
                        &["Customer", "Vec", "OrderLine", "Order"],
                    ),
                    item(
                        "OrderLine",
                        "struct",
                        9,
                        "pub struct OrderLine {",
                        &["Product", "u32"],
                    ),
                    item(
                        "Customer",
                        "struct",
                        15,
                        "pub struct Customer {",
                        &["String"],
                    ),
                    item("Product", "struct", 20, "pub struct Product {", &[]),
                    item("Priced", "trait", 30, "pub trait Priced {", &["Money"]),
                    item("total", "function", 40, "pub fn total()", &["Order"]),
                ],
            },
            DocFile {
                doc: String::new(),
                rel: "src/money.rs".into(),
                items: vec![item("Money", "struct", 1, "pub struct Money(u64);", &[])],
            },
        ];
        let mut structure = StructureIndex::default();
        structure
            .by_type
            .entry("Product".into())
            .or_default()
            .traits
            .push("Priced".into());
        let g = TypeGraph::build(root, &files, &structure);
        let name = |id: usize| g.nodes[id].name.as_str();
        assert_eq!(g.node_count(), 6, "{:?}", g.nodes);
        assert_eq!(
            name(0),
            "Order",
            "by path (model.rs before money.rs) then line"
        );
        let money = g.nodes.iter().find(|n| n.name == "Money").unwrap();
        assert_eq!(money.file, Path::new("/p/src/money.rs"));
        let edges: Vec<(&str, &str, Relation)> = g
            .edges
            .iter()
            .map(|&(a, b, r)| (name(a), name(b), r))
            .collect();
        assert_eq!(
            edges,
            [
                ("Order", "OrderLine", Relation::Uses),
                ("Order", "Customer", Relation::Uses),
                ("OrderLine", "Product", Relation::Uses),
                ("Product", "Priced", Relation::Inherits),
                ("Priced", "Money", Relation::Uses),
            ],
            "{edges:?}"
        );
        assert_eq!(g.inherits_count(), 1);
        let order = g.nodes.iter().position(|n| n.name == "Order").unwrap();
        assert_eq!(g.fan_out(order), 2);
        let priced = g.nodes.iter().position(|n| n.name == "Priced").unwrap();
        assert_eq!(g.subtypes(priced), 1);
        assert_eq!(
            g.most_referenced(8)
                .into_iter()
                .map(name)
                .collect::<Vec<_>>(),
            ["Customer", "Money", "OrderLine", "Priced", "Product"]
        );
        assert_eq!(
            g.most_derived(8).into_iter().map(name).collect::<Vec<_>>(),
            ["Priced"]
        );
        assert_eq!(
            g.most_dependent(1)
                .into_iter()
                .map(name)
                .collect::<Vec<_>>(),
            ["Order"]
        );
        assert_eq!(g.layout_edges().len(), 5);
        assert!(TypeGraph::build(root, &[], &StructureIndex::default()).is_empty());
    }

    /// Same-named types: a name resolves within its language, to a kind it
    /// can mean (a Rust impl names a trait), in the same file, then the same
    /// folder — and past that draws no edge. A type local to a function is
    /// no node. Each case is one the type map got wrong on the clew repo.
    #[test]
    fn same_named_types_resolve_to_the_right_one_or_to_none() {
        let root = Path::new("/p");
        let file = |rel: &str, items: Vec<DocItem>| DocFile {
            doc: String::new(),
            rel: rel.into(),
            items,
        };
        let mut read_config = item("read_config", "function", 3, "fn read_config()", &[]);
        read_config.children = vec![item("Read", "type", 4, "type Read = u8;", &[])];
        let files = vec![
            file(
                "src/registry.rs",
                vec![
                    item("Platform", "enum", 1, "pub enum Platform {", &[]),
                    item("Entry", "struct", 9, "pub struct Entry {", &[]),
                ],
            ),
            file(
                "src/shell.rs",
                vec![
                    item("Platform", "trait", 1, "trait Platform {", &[]),
                    item("Native", "struct", 5, "struct Native;", &[]),
                ],
            ),
            file(
                "src/store.rs",
                vec![
                    item("Entry", "trait", 1, "pub trait Entry {", &[]),
                    item("Bookmark", "struct", 8, "pub struct Bookmark {", &["Entry"]),
                ],
            ),
            file(
                "src/imports.rs",
                vec![
                    read_config,
                    item("Reader", "struct", 20, "pub struct Reader {", &[]),
                ],
            ),
            file(
                "src/x/config.rs",
                vec![item("Config", "struct", 1, "pub struct Config {", &[])],
            ),
            file(
                "src/x/uses.rs",
                vec![item(
                    "UsesX",
                    "struct",
                    1,
                    "pub struct UsesX {",
                    &["Config"],
                )],
            ),
            file(
                "src/y/config.rs",
                vec![item("Config", "struct", 1, "pub struct Config {", &[])],
            ),
            file(
                "src/z/neither.rs",
                vec![item(
                    "UsesNeither",
                    "struct",
                    1,
                    "pub struct UsesNeither {",
                    &["Config"],
                )],
            ),
            file(
                "py/order.py",
                vec![item("Order", "class", 1, "class Order:", &[])],
            ),
            file(
                "src/order.rs",
                vec![
                    item("Order", "struct", 1, "pub struct Order {", &[]),
                    item("Invoice", "struct", 5, "pub struct Invoice {", &["Order"]),
                ],
            ),
        ];
        let mut structure = StructureIndex::default();
        for (ty, tr) in [
            ("Native", "Platform"),
            ("Bookmark", "Entry"),
            ("Reader", "Read"),
        ] {
            structure
                .by_type
                .entry(ty.into())
                .or_default()
                .traits
                .push(tr.into());
        }
        let g = TypeGraph::build(root, &files, &structure);
        assert!(
            !g.nodes.iter().any(|n| n.name == "Read"),
            "a function's local alias is no type of the project: {:?}",
            g.nodes
        );
        let label = |id: usize| format!("{}@{}", g.nodes[id].name, g.nodes[id].rel);
        let mut edges: Vec<(String, String, Relation)> = g
            .edges
            .iter()
            .map(|&(a, b, r)| (label(a), label(b), r))
            .collect();
        edges.sort();
        let want = |a: &str, b: &str, r: Relation| (a.to_string(), b.to_string(), r);
        assert_eq!(
            edges,
            [
                want(
                    "Bookmark@src/store.rs",
                    "Entry@src/store.rs",
                    Relation::Inherits
                ),
                want("Invoice@src/order.rs", "Order@src/order.rs", Relation::Uses),
                want(
                    "Native@src/shell.rs",
                    "Platform@src/shell.rs",
                    Relation::Inherits
                ),
                want(
                    "UsesX@src/x/uses.rs",
                    "Config@src/x/config.rs",
                    Relation::Uses
                ),
            ],
            "{edges:?}"
        );
    }

    /// Two Rust types named `Error`, one trait: the structure index says
    /// some `Error` implements it, not which, so neither is drawn as its
    /// subtype; a uniquely named type keeps its impls.
    #[test]
    fn same_named_rust_types_share_no_trait_impls() {
        let root = Path::new("/p");
        let file = |rel: &str, items: Vec<DocItem>| DocFile {
            doc: String::new(),
            rel: rel.into(),
            items,
        };
        let files = vec![
            file(
                "src/net.rs",
                vec![
                    item("Error", "struct", 1, "pub struct Error;", &[]),
                    item("Retryable", "trait", 3, "pub trait Retryable {", &[]),
                ],
            ),
            file(
                "src/db.rs",
                vec![item("Error", "struct", 1, "pub struct Error;", &[])],
            ),
            file(
                "src/io.rs",
                vec![item("Pipe", "struct", 1, "pub struct Pipe;", &[])],
            ),
        ];
        let mut structure = StructureIndex::default();
        for ty in ["Error", "Pipe"] {
            structure
                .by_type
                .entry(ty.into())
                .or_default()
                .traits
                .push("Retryable".into());
        }
        let g = TypeGraph::build(root, &files, &structure);
        let label = |id: usize| format!("{}@{}", g.nodes[id].name, g.nodes[id].rel);
        let edges: Vec<(String, String, Relation)> = g
            .edges
            .iter()
            .map(|&(a, b, r)| (label(a), label(b), r))
            .collect();
        assert_eq!(
            edges,
            [(
                "Pipe@src/io.rs".to_string(),
                "Retryable@src/net.rs".to_string(),
                Relation::Inherits
            )],
            "{edges:?}"
        );
    }
}
