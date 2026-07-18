//! Project-wide Rust type structure: which traits each type implements, and
//! which types implement each trait, read off `impl` blocks with tree-sitter.
//!
//! Like [`crate::projectcalls`], this trades exactness for a whole-project view
//! computed offline and instantly. It answers, for the type or trait under the
//! cursor, "what does this implement / what implements this" — the relations you
//! want while reading, without jumping to every `impl` block. Resolution is by
//! name (so a `Foo` in two crates would merge), which is fine for the aggregate
//! signal this feeds into the hover peek.
//!
//! Rust only for now; Go interface implementors and JS/TS exports can follow.

use std::collections::HashMap;

use tree_sitter::{Node, Parser};

use crate::fs_scan::FileEntry;

/// What a single type implements, aggregated across the project.
#[derive(Debug, Clone, Default)]
pub struct TypeStructure {
    /// Names of traits implemented for this type (`impl Trait for Type`).
    pub traits: Vec<String>,
    /// Inherent method names (`impl Type { fn … }`).
    pub methods: Vec<String>,
}

/// The project's type/trait relations.
#[derive(Debug, Clone, Default)]
pub struct StructureIndex {
    by_type: HashMap<String, TypeStructure>,
    /// Trait name -> the types that implement it.
    implementors: HashMap<String, Vec<String>>,
}

impl StructureIndex {
    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty() && self.implementors.is_empty()
    }

    /// A one-line structure summary for the type or trait named `name`, or
    /// `None` if it is neither. Traits win the tie (a name is one or the other).
    pub fn summary_line(&self, name: &str) -> Option<String> {
        if let Some(impls) = self.implementors.get(name) {
            return Some(list_line("Implementors", impls));
        }
        let ts = self.by_type.get(name)?;
        let mut bits = Vec::new();
        if !ts.traits.is_empty() {
            bits.push(list_line("impl", &ts.traits));
        }
        if !ts.methods.is_empty() {
            let n = ts.methods.len();
            bits.push(format!("{n} method{}", if n == 1 { "" } else { "s" }));
        }
        (!bits.is_empty()).then(|| bits.join(" · "))
    }
}

/// `"impl A, B, C (+2)"` — at most 8 names, then a `(+n)` overflow.
fn list_line(label: &str, names: &[String]) -> String {
    const MAX: usize = 8;
    let shown: Vec<&str> = names.iter().take(MAX).map(String::as_str).collect();
    let more = names.len().saturating_sub(shown.len());
    let mut s = format!("{label} {}", shown.join(", "));
    if more > 0 {
        s.push_str(&format!(" (+{more})"));
    }
    s
}

/// Build the index by parsing every Rust file's `impl` blocks. Blocking; run off
/// the UI thread. Reads files from disk (the index cache is symbol-shaped, not
/// impl-shaped), so this is a separate, background pass.
pub fn build(files: &[FileEntry]) -> StructureIndex {
    let mut idx = StructureIndex::default();
    let Some(lang) = crate::highlight::language_for("rust") else {
        return idx;
    };
    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() {
        return idx;
    }
    for f in files {
        if crate::highlight::detect(&f.abs) != Some("rust") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&f.abs) else {
            continue;
        };
        if let Some(tree) = parser.parse(&src, None) {
            collect_impls(tree.root_node(), src.as_bytes(), &mut idx);
        }
    }
    for ts in idx.by_type.values_mut() {
        ts.traits.sort();
        ts.traits.dedup();
        ts.methods.sort();
        ts.methods.dedup();
    }
    for v in idx.implementors.values_mut() {
        v.sort();
        v.dedup();
    }
    idx
}

fn collect_impls(node: Node, src: &[u8], idx: &mut StructureIndex) {
    if node.kind() == "impl_item" {
        handle_impl(node, src, idx);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_impls(child, src, idx);
    }
}

fn handle_impl(node: Node, src: &[u8], idx: &mut StructureIndex) {
    let Some(type_name) = node.child_by_field_name("type").and_then(|n| base_ident(n, src)) else {
        return;
    };
    let trait_name = node.child_by_field_name("trait").and_then(|n| base_ident(n, src));
    if let Some(trait_name) = trait_name {
        idx.by_type.entry(type_name.clone()).or_default().traits.push(trait_name.clone());
        idx.implementors.entry(trait_name).or_default().push(type_name);
    } else {
        let methods = method_names(node, src);
        idx.by_type.entry(type_name).or_default().methods.extend(methods);
    }
}

/// The base type name of a (possibly generic / scoped) type node: its first
/// `type_identifier` descendant (`Vec<T>` -> `Vec`, `a::B<'x>` -> `B`).
fn base_ident(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "type_identifier" {
        return node.utf8_text(src).ok().map(str::to_string);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(name) = base_ident(child, src) {
            return Some(name);
        }
    }
    None
}

fn method_names(impl_node: Node, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let Some(body) = impl_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for item in body.children(&mut cursor) {
        if item.kind() == "function_item"
            && let Some(name) = item.child_by_field_name("name").and_then(|n| n.utf8_text(src).ok())
        {
            out.push(name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_of(src: &str) -> StructureIndex {
        let lang = crate::highlight::language_for("rust").unwrap();
        let mut parser = Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut idx = StructureIndex::default();
        collect_impls(tree.root_node(), src.as_bytes(), &mut idx);
        idx
    }

    #[test]
    fn records_trait_impls_and_inherent_methods() {
        let src = "\
struct Point { x: f64 }
impl Point { fn new() -> Self { Point { x: 0.0 } } fn norm(&self) -> f64 { 0.0 } }
impl Clone for Point { fn clone(&self) -> Self { Point { x: self.x } } }
impl std::fmt::Debug for Point { fn fmt(&self) -> () {} }
";
        let idx = index_of(src);
        let line = idx.summary_line("Point").unwrap();
        assert!(line.contains("Clone"), "{line}");
        assert!(line.contains("Debug"), "{line}");
        assert!(line.contains("2 methods"), "{line}");
    }

    #[test]
    fn records_implementors_for_a_trait() {
        let src = "\
trait Shape {}
struct Circle;
struct Square;
impl Shape for Circle {}
impl Shape for Square {}
";
        let idx = index_of(src);
        let line = idx.summary_line("Shape").unwrap();
        assert!(line.starts_with("Implementors"), "{line}");
        assert!(line.contains("Circle") && line.contains("Square"), "{line}");
    }

    #[test]
    fn unknown_name_has_no_summary() {
        assert!(index_of("fn free() {}\n").summary_line("Nope").is_none());
    }
}
