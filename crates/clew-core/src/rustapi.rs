//! A Rust crate's API as rustdoc reads it, past what one item's declaration
//! says: what its attributes say of it (`#[doc(hidden)]`,
//! `#[macro_export]`), what a module re-exports (`pub use`), which modules
//! the crate root reaches, and the docs a trait implementation's methods
//! take from the trait.
//!
//! [`crate::apidoc::build_file`] reads one file; [`link_crates`] reads the
//! project's files together, as crates.

use std::collections::HashMap;

use clew_protocol::{DocFile, DocItem};
use tree_sitter::Node;

use crate::highlight::Lang;

/// The kind a `pub use` is listed under in the Docs view.
pub const REEXPORT: &str = "re-export";

/// Most re-exports one file lists, a glob's names included: a generated
/// prelude of a few hundred names is the most a real one has.
const MAX_REEXPORTS: usize = 2_000;

/// How many `pub use` steps a name is followed through to its item —
/// `lib.rs` re-exporting `prelude::*` re-exporting `a::B` — and the guard
/// against a cycle of them.
const MAX_HOPS: usize = 4;

/// Most lines above an item read for its attributes: rustfmt's
/// `#[cfg_attr(` … `)]` runs over a few; a run longer than this is not one.
const MAX_ATTRIBUTE_LINES: usize = 64;

/// What an item's outer attributes say of its place in the API.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Attrs {
    /// `#[doc(hidden)]`: rustdoc leaves it out, however `pub` it is.
    pub hidden: bool,
    /// `#[macro_export]`: a `macro_rules!` exported at the crate's root,
    /// from whatever module declares it.
    pub macro_export: bool,
}

/// The outer attributes of the item whose definition starts on `line`
/// (1-based) of `lines`: those on the lines above it — past its doc
/// comments, over the lines an attribute's arguments run on — and any
/// written on its own line before it (`#[doc(hidden)] pub fn f()`).
pub(crate) fn attrs_above(lines: &[&str], line: usize) -> Attrs {
    let Some(at) = line.checked_sub(1).filter(|&i| i < lines.len()) else {
        return Attrs::default();
    };
    let mut text = String::new();
    let own = lines[at].trim();
    if own.starts_with("#[") {
        text.push_str(own);
    }
    // Read upwards, so an attribute's closing `)]` comes before its `#[`:
    // `open` counts the brackets closed below and not yet opened, and while
    // it is positive the line is an attribute's argument, not an item.
    let mut open: i64 = 0;
    for i in (at.saturating_sub(MAX_ATTRIBUTE_LINES)..at).rev() {
        let t = lines[i].trim();
        let comment =
            t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*');
        if open == 0 && comment {
            continue;
        }
        if open == 0 && !t.starts_with("#[") && !t.ends_with([')', ']']) {
            break;
        }
        open += t.matches([')', ']']).count() as i64 - t.matches(['(', '[']).count() as i64;
        if open < 0 {
            break;
        }
        text.push(' ');
        text.push_str(t);
        if open == 0 && !t.starts_with("#[") {
            // A line that closes what it opens and is no attribute: the
            // previous item's last line (`fn f() -> X<(A, B)>`), not ours.
            break;
        }
    }
    let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    Attrs {
        hidden: squeezed.contains("#[doc(hidden"),
        macro_export: squeezed.contains("#[macro_export"),
    }
}

/// One path a `pub use` re-exports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reexport {
    /// The path as written, its segments joined by `::`: `crate::a::B`, or a
    /// glob's module, `crate::a`.
    pub path: String,
    /// The name it is exported as: its last segment, or what `as` renames it
    /// to. `None` for a glob, which exports every name of its module.
    pub name: Option<String>,
    /// The 1-based line it is written on.
    pub line: usize,
}

/// The paths Rust `source` re-exports from its top level: every `pub use`'s
/// tree, one path each — `pub use a::{b::C, d::*}` is `a::b::C` and the
/// glob of `a::d`. Only `pub`: a `pub(crate) use` is the crate's own, no API.
/// `as _` exports no name, so it is left out.
pub(crate) fn reexports(source: &str) -> Vec<Reexport> {
    let Some(tree) = crate::highlight::parse(source, Lang::Rust) else {
        return Vec::new();
    };
    let text = |n: Node| -> String {
        source
            .get(n.byte_range())
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    };
    let join = |prefix: &str, path: &str| match (prefix.is_empty(), path.is_empty()) {
        (true, _) => path.to_string(),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{prefix}::{path}"),
    };
    let last = |path: &str| path.rsplit("::").next().unwrap_or(path).to_string();
    let root = tree.root_node();
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for decl in root.named_children(&mut cursor) {
        if decl.kind() != "use_declaration" {
            continue;
        }
        let mut c = decl.walk();
        let public = decl
            .children(&mut c)
            .any(|v| v.kind() == "visibility_modifier" && text(v) == "pub");
        let Some(arg) = decl.child_by_field_name("argument").filter(|_| public) else {
            continue;
        };
        // Walked with a stack: the nesting is the repository's.
        let mut stack: Vec<(Node, String)> = vec![(arg, String::new())];
        while let Some((node, prefix)) = stack.pop() {
            if out.len() >= MAX_REEXPORTS {
                return out;
            }
            let line = node.start_position().row + 1;
            match node.kind() {
                "scoped_use_list" => {
                    let path = node
                        .child_by_field_name("path")
                        .map(text)
                        .unwrap_or_default();
                    if let Some(list) = node.child_by_field_name("list") {
                        stack.push((list, join(&prefix, &path)));
                    }
                }
                "use_list" => {
                    let mut lc = node.walk();
                    let entries: Vec<Node> = node.named_children(&mut lc).collect();
                    for entry in entries.into_iter().rev() {
                        stack.push((entry, prefix.clone()));
                    }
                }
                "use_as_clause" => {
                    let path = node.child_by_field_name("path").map(text);
                    let alias = node.child_by_field_name("alias").map(text);
                    if let (Some(path), Some(alias)) = (path, alias)
                        && alias != "_"
                    {
                        out.push(Reexport {
                            path: join(&prefix, &path),
                            name: Some(alias),
                            line,
                        });
                    }
                }
                "use_wildcard" => {
                    let path = node.named_child(0).map(text).unwrap_or_default();
                    let path = join(&prefix, &path);
                    if !path.is_empty() {
                        out.push(Reexport {
                            path,
                            name: None,
                            line,
                        });
                    }
                }
                "self" if !prefix.is_empty() => out.push(Reexport {
                    name: Some(last(&prefix)),
                    path: prefix,
                    line,
                }),
                "line_comment" | "block_comment" => {}
                _ => {
                    let path = join(&prefix, &text(node));
                    if !path.is_empty() {
                        out.push(Reexport {
                            name: Some(last(&path)),
                            path,
                            line,
                        });
                    }
                }
            }
        }
    }
    out
}

/// A re-export as the Docs view lists it before the crate is read
/// ([`link_crates`] fills in what it names): its name, `pub use` and line.
pub(crate) fn reexport_item(r: &Reexport) -> DocItem {
    let (name, signature) = match &r.name {
        Some(name) if *name == last_segment(&r.path) => {
            (name.clone(), format!("pub use {}", r.path))
        }
        // Joined, not formatted: code, not a message for the catalogs.
        Some(name) => (name.clone(), ["pub use ", &r.path, " as ", name].concat()),
        None => (format!("{}::*", r.path), format!("pub use {}::*", r.path)),
    };
    DocItem {
        name,
        kind: REEXPORT.to_string(),
        signature,
        doc: String::new(),
        line: r.line,
        public: true,
        children: Vec::new(),
        refs: Vec::new(),
    }
}

fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// What [`link_crates`] needs of a Rust file besides its doc items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// Each trait implementation in the file: its first and last line, and
    /// the trait's name (`Display` for `impl fmt::Display for X`).
    pub trait_impls: Vec<(usize, usize, String)>,
}

/// The [`Facts`] of Rust `source`.
pub fn facts(source: &str) -> Facts {
    let trait_impls = crate::outline::rust_impl_blocks(source)
        .into_iter()
        .filter_map(|b| {
            let (tr, _) = b.label.split_once(" for ")?;
            // A negative impl (`!Send for X`) has no items.
            if tr.starts_with('!') {
                return None;
            }
            let name = tr.split('<').next()?;
            let name = name.rsplit("::").next()?.trim();
            (!name.is_empty()).then(|| (b.line, b.end_line, name.to_string()))
        })
        .collect();
    Facts { trait_impls }
}

/// A module of a library crate: its crate (an index into the crates found)
/// and its path from the crate's root, `["builders", "string"]`.
type ModulePath = (usize, Vec<String>);

/// The project's Rust files read together, as rustdoc reads a crate:
///
/// - a re-export (`pub use`) lists what it names, with that item's doc and
///   members — `Regex` in regex's `lib.rs`, re-exported from a private
///   module — and a glob (`pub use a::*`) every public name of its module;
/// - an item of a module the crate's root does not reach (`mod internal;`,
///   not `pub mod`) is not public API, however `pub` it is — but a
///   `#[macro_export]` macro, exported at the root from anywhere;
/// - a trait implementation's method with no doc of its own has the trait
///   method's, as rustdoc shows it.
///
/// A library crate is a `src/lib.rs` and the files under its `src/`,
/// their modules by where they sit (`src/a/b.rs` or `src/a/b/mod.rs` is
/// `a::b`); a binary's (`src/main.rs`, `src/bin/`) are documented whole, as
/// `cargo doc` documents a binary. `facts` is each file's [`Facts`] by its
/// path.
pub fn link_crates(files: &mut [DocFile], facts: &HashMap<String, Facts>) {
    let owned: Vec<String> = files.iter().map(|f| f.rel.clone()).collect();
    let rels: Vec<&str> = owned.iter().map(String::as_str).collect();
    let modules = crate_modules(&rels);
    let by_path: HashMap<ModulePath, usize> =
        modules.iter().map(|(&f, m)| (m.clone(), f)).collect();

    // What re-exports resolve to, worked out before anything changes.
    let mut replaced: Vec<(usize, usize, Vec<DocItem>)> = Vec::new();
    for (&fi, module) in &modules {
        for (ii, item) in files[fi].items.iter().enumerate() {
            if item.kind != REEXPORT || !item.children.is_empty() {
                continue;
            }
            let found = resolve_item(files, &by_path, module, item, MAX_HOPS);
            if !found.is_empty() {
                replaced.push((fi, ii, found));
            }
        }
    }
    // Back to front, so the indices still to come stand.
    replaced.sort_by_key(|&(fi, ii, _)| (fi, std::cmp::Reverse(ii)));
    for (fi, ii, found) in replaced {
        files[fi].items.splice(ii..=ii, found);
    }

    let hidden = unreachable(&rels, |fi, name| {
        files[fi]
            .items
            .iter()
            .find(|i| i.kind == "module" && i.name == name)
            .map(|i| i.public)
    });
    for fi in hidden {
        for item in &mut files[fi].items {
            if item.kind != "macro" {
                item.public = false;
            }
        }
    }

    inherit_trait_docs(files, facts);
}

/// The files of `rels` (by index) whose modules their library crate's root
/// does not reach: on the way down, a module declared `mod`, not `pub mod`
/// (`declared` answers, for a file and a module name, whether that file
/// declares the module and whether `pub`). Their items are the crate's own,
/// whatever they say. A module its parent does not declare at its top
/// level (a `#[path]` module, one declared inside a macro) is not held
/// against it.
pub(crate) fn unreachable(
    rels: &[&str],
    declared: impl Fn(usize, &str) -> Option<bool>,
) -> Vec<usize> {
    let modules = crate_modules(rels);
    let by_path: HashMap<&ModulePath, usize> = modules.iter().map(|(&f, m)| (m, f)).collect();
    let private = |krate: usize, path: &[String]| -> bool {
        let Some((name, parent)) = path.split_last() else {
            return false;
        };
        by_path
            .get(&(krate, parent.to_vec()))
            .and_then(|&p| declared(p, name))
            == Some(false)
    };
    let mut out: Vec<usize> = modules
        .iter()
        .filter(|(_, (krate, path))| (1..=path.len()).any(|n| private(*krate, &path[..n])))
        .map(|(&fi, _)| fi)
        .collect();
    out.sort_unstable();
    out
}

/// The module each file of a library crate is, by where it sits.
fn crate_modules(rels: &[&str]) -> HashMap<usize, ModulePath> {
    // The crates' `src/` folders, the deepest first: a crate nested in
    // another's folder owns its own files.
    let mut roots: Vec<&str> = rels
        .iter()
        .filter(|rel| **rel == "src/lib.rs" || rel.ends_with("/src/lib.rs"))
        .map(|rel| &rel[..rel.len() - "lib.rs".len()])
        .collect();
    roots.sort_by_key(|b| std::cmp::Reverse(b.len()));
    let mut out = HashMap::new();
    for (fi, rel) in rels.iter().enumerate() {
        let Some(stem) = rel.strip_suffix(".rs") else {
            continue;
        };
        let Some((krate, base)) = roots.iter().enumerate().find(|(_, b)| rel.starts_with(**b))
        else {
            continue;
        };
        let rest = &stem[base.len()..];
        if rest == "main" || rest.starts_with("bin/") {
            continue;
        }
        let mut path: Vec<String> = match rest {
            "lib" => Vec::new(),
            _ => rest.split('/').map(str::to_string).collect(),
        };
        if path.last().is_some_and(|l| l == "mod") {
            path.pop();
        }
        out.insert(fi, (krate, path));
    }
    out
}

/// The module a `use` path's leading segments name, from `module`: `crate`
/// is the root, `self` the module itself, each `super` its parent; a bare
/// name is the module's own child first (a path is relative in a `use`, as
/// of the 2018 edition), then the root's.
fn resolve_module(
    by_path: &HashMap<ModulePath, usize>,
    module: &ModulePath,
    segments: &[&str],
) -> Option<ModulePath> {
    let (krate, here) = module;
    let walk = |mut at: Vec<String>, rest: &[&str]| -> Option<ModulePath> {
        for s in rest {
            match *s {
                "self" => {}
                "super" => {
                    at.pop()?;
                }
                s => at.push(s.to_string()),
            }
        }
        let key = (*krate, at);
        by_path.contains_key(&key).then_some(key)
    };
    match segments.first() {
        Some(&"crate") => walk(Vec::new(), &segments[1..]),
        Some(&"self" | &"super") => walk(here.clone(), segments),
        Some(_) => walk(here.clone(), segments).or_else(|| walk(Vec::new(), segments)),
        None => by_path.contains_key(module).then(|| module.clone()),
    }
}

/// What the re-export `item` of a file at `module` lists, each as a
/// re-export of its own with the doc and members of what it names; empty
/// when the path leaves the crate (another crate's item) or names nothing
/// found.
fn resolve_item(
    files: &[DocFile],
    by_path: &HashMap<ModulePath, usize>,
    module: &ModulePath,
    item: &DocItem,
    hops: usize,
) -> Vec<DocItem> {
    if hops == 0 {
        return Vec::new();
    }
    let Some(statement) = item.signature.strip_prefix("pub use ") else {
        return Vec::new();
    };
    let statement = statement.lines().next().unwrap_or(statement);
    let (path, alias) = match statement.split_once(" as ") {
        Some((p, a)) => (p, Some(a)),
        None => (statement, None),
    };
    let segments: Vec<&str> = path.split("::").collect();
    let Some((&last, init)) = segments.split_last() else {
        return Vec::new();
    };
    let as_reexport = |name: &str, full: String, target: &DocItem| DocItem {
        name: name.to_string(),
        kind: REEXPORT.to_string(),
        signature: match target.kind.as_str() {
            REEXPORT => target.signature.clone(),
            _ => format!("pub use {full}\n{}", target.signature),
        },
        doc: target.doc.clone(),
        line: item.line,
        public: true,
        children: target.children.clone(),
        refs: Vec::new(),
    };
    if last == "*" {
        let Some(from) = resolve_module(by_path, module, init) else {
            return Vec::new();
        };
        let file = &files[by_path[&from]];
        let mut out = Vec::new();
        for named in file.items.iter().filter(|i| i.public) {
            if out.len() >= MAX_REEXPORTS {
                break;
            }
            if named.kind == REEXPORT {
                for inner in resolve_item(files, by_path, &from, named, hops - 1) {
                    out.push(DocItem {
                        line: item.line,
                        ..inner
                    });
                }
                continue;
            }
            out.push(as_reexport(
                &named.name,
                format!("{}::{}", init.join("::"), named.name),
                named,
            ));
        }
        return out;
    }
    let Some(from) = resolve_module(by_path, module, init) else {
        return Vec::new();
    };
    let name = alias.unwrap_or(last);
    let file = &files[by_path[&from]];
    if let Some(target) = file
        .items
        .iter()
        .find(|i| i.name == last && i.kind != REEXPORT)
    {
        return vec![as_reexport(name, path.to_string(), target)];
    }
    // Re-exported there in turn: followed to the item.
    if let Some(next) = file
        .items
        .iter()
        .find(|i| i.kind == REEXPORT && i.name == last)
    {
        return resolve_item(files, by_path, &from, next, hops - 1)
            .into_iter()
            .take(1)
            .map(|inner| DocItem {
                name: name.to_string(),
                line: item.line,
                signature: format!("pub use {path}"),
                ..inner
            })
            .collect();
    }
    Vec::new()
}

/// A trait implementation's undocumented items given the trait's docs, as
/// rustdoc shows them: the trait found by name, in the same file first,
/// then anywhere in the project where only one trait has that name.
fn inherit_trait_docs(files: &mut [DocFile], facts: &HashMap<String, Facts>) {
    // (trait, item) → (file, doc), every trait of the project's.
    let mut docs: HashMap<(String, String), Vec<(usize, String)>> = HashMap::new();
    for (fi, f) in files.iter().enumerate() {
        if !f.rel.ends_with(".rs") {
            continue;
        }
        let mut stack: Vec<&DocItem> = f.items.iter().collect();
        while let Some(item) = stack.pop() {
            if item.kind == "trait" {
                for member in &item.children {
                    if !member.doc.is_empty() {
                        docs.entry((item.name.clone(), member.name.clone()))
                            .or_default()
                            .push((fi, member.doc.clone()));
                    }
                }
            }
            if item.kind == "module" {
                stack.extend(&item.children);
            }
        }
    }
    if docs.is_empty() {
        return;
    }
    for (fi, f) in files.iter_mut().enumerate() {
        let Some(impls) = facts.get(&f.rel).filter(|x| !x.trait_impls.is_empty()) else {
            continue;
        };
        let mut stack: Vec<&mut DocItem> = f.items.iter_mut().collect();
        while let Some(item) = stack.pop() {
            if item.doc.is_empty()
                && let Some((_, _, tr)) = impls
                    .trait_impls
                    .iter()
                    .filter(|(a, b, _)| *a < item.line && item.line <= *b)
                    .min_by_key(|(a, b, _)| b - a)
                && let Some(found) = docs.get(&(tr.clone(), item.name.clone()))
            {
                let pick = found
                    .iter()
                    .find(|(from, _)| *from == fi)
                    .or_else(|| (found.len() == 1).then(|| &found[0]));
                if let Some((_, doc)) = pick {
                    item.doc = doc.clone();
                }
            }
            stack.extend(item.children.iter_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(rel: &str, src: &str) -> DocFile {
        DocFile {
            rel: rel.to_string(),
            items: crate::apidoc::build_file(src, "rust"),
            doc: String::new(),
        }
    }

    fn find<'a>(items: &'a [DocItem], name: &str) -> &'a DocItem {
        items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("no {name} in {items:#?}"))
    }

    /// `#[doc(hidden)]` and `#[macro_export]`, above an item over lines, on
    /// its line, past its doc — and never the attribute of the item above.
    #[test]
    fn an_items_attributes_are_read_above_it() {
        let src = "#[doc(hidden)]\npub fn a() {}\n/// Doc.\n#[cfg_attr(\n    feature = \"x\",\n    derive(Debug)\n)]\n#[doc(hidden)]\npub struct B;\n#[doc(hidden)] pub fn c() {}\n#[doc(hidden)]\npub x: u32,\npub y: u32,\n#[macro_export]\nmacro_rules! m { () => {} }\nfn d() -> Option<(u8, u8)> { None }\npub fn e() {}\n";
        let lines: Vec<&str> = src.lines().collect();
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap() + 1;
        assert!(attrs_above(&lines, at("pub fn a")).hidden);
        assert!(attrs_above(&lines, at("pub struct B")).hidden);
        assert!(attrs_above(&lines, at("pub fn c")).hidden);
        assert!(attrs_above(&lines, at("pub x")).hidden);
        assert!(!attrs_above(&lines, at("pub y")).hidden);
        assert!(attrs_above(&lines, at("macro_rules")).macro_export);
        assert_eq!(attrs_above(&lines, at("pub fn e")), Attrs::default());
    }

    /// Every path of a `pub use` tree, renames and globs as written; a
    /// `pub(crate) use` and an `as _` export nothing.
    #[test]
    fn re_exports_are_every_path_a_pub_use_names() {
        let src = "pub use crate::{builders::string::*, error::Error};\npub use self::a::B as C;\npub(crate) use x::Y;\nuse z::W;\npub use t::Trait as _;\npub use m::{self, n::{O, P}};\n";
        let got: Vec<(String, Option<String>, usize)> = reexports(src)
            .into_iter()
            .map(|r| (r.path, r.name, r.line))
            .collect();
        let want = |p: &str, n: Option<&str>, l: usize| (p.to_string(), n.map(str::to_string), l);
        assert_eq!(
            got,
            [
                want("crate::builders::string", None, 1),
                want("crate::error::Error", Some("Error"), 1),
                want("self::a::B", Some("C"), 2),
                want("m", Some("m"), 6),
                want("m::n::O", Some("O"), 6),
                want("m::n::P", Some("P"), 6),
            ]
        );
    }

    /// regex's shape: its types live in a private module and are re-exported
    /// from the root. The root lists them with their docs and members; the
    /// private module's own listing is not public API; a public module's is.
    #[test]
    fn a_crates_root_lists_what_it_re_exports() {
        let mut files = vec![
            file(
                "regex/src/lib.rs",
                "mod builders;\npub mod bytes;\npub use crate::builders::string::*;\npub use crate::error::Error as RegexError;\nmod error;\npub use serde::Serialize;\n",
            ),
            file("regex/src/builders.rs", "pub mod string;\n"),
            file(
                "regex/src/builders/string.rs",
                "/// A compiled regex.\npub struct Regex {\n    /// Its pattern.\n    pub pattern: String,\n}\nimpl Regex {\n    /// Compiles one.\n    pub fn new() -> Regex { todo!() }\n}\n/// Options.\npub struct RegexBuilder;\nfn private() {}\n#[macro_export]\nmacro_rules! regex { () => {} }\n",
            ),
            file(
                "regex/src/error/mod.rs",
                "/// What went wrong.\npub enum Error {\n    /// Too big.\n    Size,\n}\n",
            ),
            file("regex/src/bytes.rs", "/// Bytes.\npub struct Regex;\n"),
        ];
        link_crates(&mut files, &HashMap::new());
        let root = &files[0].items;
        let regex = find(root, "Regex");
        assert_eq!(regex.kind, REEXPORT);
        assert_eq!(regex.doc, "A compiled regex.");
        assert_eq!(regex.line, 3);
        assert_eq!(
            regex.signature,
            "pub use crate::builders::string::Regex\npub struct Regex"
        );
        assert!(regex.public);
        let names: Vec<&str> = regex.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["pattern", "new"]);
        assert_eq!(find(root, "RegexBuilder").doc, "Options.");
        assert!(root.iter().all(|i| i.name != "private"));
        let error = find(root, "RegexError");
        assert_eq!(error.doc, "What went wrong.");
        assert_eq!(find(&error.children, "Size").doc, "Too big.");
        // Another crate's item: listed as written.
        assert_eq!(
            find(root, "Serialize").signature,
            "pub use serde::Serialize"
        );
        // The private module's items are no API of their own; its exported
        // macro is, from the root.
        let string = &files[2].items;
        assert!(!find(string, "Regex").public);
        assert!(find(string, "regex").public, "{string:#?}");
        assert!(!find(&files[3].items, "Error").public);
        // A `pub mod`'s items are.
        assert!(find(&files[4].items, "Regex").public);
    }

    /// A glob of a module that itself re-exports, and a name re-exported
    /// twice over, reach the item.
    #[test]
    fn re_exports_are_followed_to_their_item() {
        let mut files = vec![
            file(
                "src/lib.rs",
                "mod prelude;\nmod inner;\npub use prelude::*;\npub use inner::Deep;\n",
            ),
            file("src/prelude.rs", "pub use crate::inner::Thing;\n"),
            file(
                "src/inner.rs",
                "/// A thing.\npub struct Thing;\npub use self::Thing as Deep;\n",
            ),
        ];
        link_crates(&mut files, &HashMap::new());
        let root = &files[0].items;
        assert_eq!(find(root, "Thing").doc, "A thing.");
        assert_eq!(find(root, "Deep").doc, "A thing.");
        assert_eq!(find(root, "Deep").signature, "pub use inner::Deep");
    }

    /// A binary's items are documented as they are; so is a file of no
    /// crate found.
    #[test]
    fn a_binary_and_a_loose_file_keep_their_visibility() {
        let mut files = vec![
            file("src/main.rs", "mod cli;\npub fn run() {}\n"),
            file("src/cli.rs", "pub fn parse() {}\n"),
            file("scripts/x.rs", "pub fn y() {}\n"),
        ];
        link_crates(&mut files, &HashMap::new());
        assert!(find(&files[0].items, "run").public);
        assert!(find(&files[1].items, "parse").public);
        assert!(find(&files[2].items, "y").public);
    }

    /// An implementation's method with no doc has its trait method's, from
    /// the same file or the one trait of that name elsewhere.
    #[test]
    fn trait_implementations_inherit_the_traits_docs() {
        let shape = "pub trait Shape {\n    /// The area.\n    fn area(&self) -> f64;\n    /// Its name.\n    fn name(&self) -> String;\n}\n";
        let sq = "pub struct Sq;\nimpl crate::shape::Shape for Sq {\n    fn area(&self) -> f64 { 1.0 }\n    /// Always square.\n    fn name(&self) -> String { todo!() }\n}\nimpl Sq {\n    pub fn area(&self) -> f64 { 1.0 }\n}\n";
        let mut files = vec![file("src/shape.rs", shape), file("src/sq.rs", sq)];
        let facts: HashMap<String, Facts> = [
            ("src/shape.rs".to_string(), super::facts(shape)),
            ("src/sq.rs".to_string(), super::facts(sq)),
        ]
        .into();
        assert_eq!(
            facts["src/sq.rs"].trait_impls,
            [(2, 6, "Shape".to_string())]
        );
        link_crates(&mut files, &facts);
        let sq = find(&files[1].items, "Sq");
        let docs: Vec<(&str, &str)> = sq
            .children
            .iter()
            .map(|c| (c.name.as_str(), c.doc.as_str()))
            .collect();
        assert_eq!(
            docs,
            [
                ("area", "The area."),
                ("name", "Always square."),
                ("area", "")
            ]
        );
    }
}
