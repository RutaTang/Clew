//! Project-wide Rust type structure: which traits each type implements, and
//! which types implement each trait, read off `impl` blocks with tree-sitter.
//!
//! Like [`the project call graph`], this trades exactness for a whole-project view
//! computed offline and instantly. It answers, for the type or trait under the
//! cursor, "what does this implement / what implements this" — the relations you
//! want while reading, without jumping to every `impl` block. Resolution is by
//! name (so a `Foo` in two crates would merge), which is fine for the aggregate
//! signal this feeds into the hover peek.
//!
//! Rust only for now; Go interface implementors and JS/TS exports can follow.
//!
//! Lives in clew-core so it builds where the files live: the client calls
//! [`build`] for a local project; clew-server includes the (serialized)
//! index in the full `ProjectSymbols` snapshot for a remote one.

use std::collections::HashMap;

use tree_sitter::{Node, Parser};

use crate::fs_scan::FileEntry;

/// What a single type implements, aggregated across the project.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TypeStructure {
    /// Names of traits implemented for this type (`impl Trait for Type`).
    pub traits: Vec<String>,
    /// Inherent method names (`impl Type { fn … }`).
    pub methods: Vec<String>,
}

/// The project's type/trait relations.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
/// impl-shaped), so this is a separate, background pass — under the SAME
/// limits as the symbol indexer (file count, per-file size, confinement to
/// `root`): this pass must not read what the indexer would refuse to.
pub fn build(root: &std::path::Path, files: &[FileEntry]) -> StructureIndex {
    // Mirrors the symbol indexer's caps.
    const MAX_STRUCT_FILES: usize = 20_000;
    const MAX_STRUCT_FILE_BYTES: u64 = 512 * 1024;
    let mut idx = StructureIndex::default();
    let Some(lang) = crate::highlight::language_for("rust") else {
        return idx;
    };
    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() {
        return idx;
    }
    for f in files.iter().take(MAX_STRUCT_FILES) {
        if crate::highlight::detect(&f.abs) != Some("rust") {
            continue;
        }
        // Regular files really inside the project only: a symlink would pull
        // outside content into the hover peek. Checked and capped on the OPEN
        // HANDLE — re-resolving the path for the read let a swapped FIFO block
        // this thread, which runs under the publication lock.
        let Some(src) = crate::fs_scan::read_confined_capped(root, &f.abs, MAX_STRUCT_FILE_BYTES)
        else {
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

/// Walk the tree with an explicit stack rather than the call stack.
///
/// The recursion this replaces went as deep as the syntax tree, and a deeply
/// nested expression costs about two bytes per level in the source — so a file
/// well inside the size cap reaches a depth that overflows the stack. That is
/// a SIGSEGV, not a catchable panic, and this runs automatically on every
/// project open over whatever source the repository contains.
///
/// Order is preserved (children pushed in reverse so they pop front-to-back),
/// though nothing here depends on it: the index is sorted and deduped after.
fn collect_impls(root: Node, src: &[u8], idx: &mut StructureIndex) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "impl_item" {
            handle_impl(node, src, idx);
        }
        let mut cursor = node.walk();
        let before = stack.len();
        stack.extend(node.children(&mut cursor));
        stack[before..].reverse();
    }
}

fn handle_impl(node: Node, src: &[u8], idx: &mut StructureIndex) {
    let Some(type_name) = node
        .child_by_field_name("type")
        .and_then(|n| base_ident(n, src))
    else {
        return;
    };
    let trait_name = node
        .child_by_field_name("trait")
        .and_then(|n| base_ident(n, src));
    if let Some(trait_name) = trait_name {
        idx.by_type
            .entry(type_name.clone())
            .or_default()
            .traits
            .push(trait_name.clone());
        idx.implementors
            .entry(trait_name)
            .or_default()
            .push(type_name);
    } else {
        let methods = method_names(node, src);
        idx.by_type
            .entry(type_name)
            .or_default()
            .methods
            .extend(methods);
    }
}

/// The base type name of a (possibly generic / scoped) type node: its first
/// `type_identifier` descendant (`Vec<T>` -> `Vec`, `a::B<'x>` -> `B`).
///
/// Iterative for the same reason `collect_impls` above is: the depth of this
/// walk is the depth of the repository's syntax tree, and a reference type
/// costs ONE source byte per level (`impl &&&&…Foo {}`), so the 512 KiB
/// per-file cap still admits hundreds of thousands of levels. Recursing there
/// is a stack overflow, which is a SIGSEGV rather than a catchable panic — no
/// join handler, no `unwrap_or_default`, no epoch check runs — and this builds
/// automatically on every project open over whatever source the repo contains.
/// Locally that kills the GUI process and every window it hosts; remotely it
/// kills the `clew-server` behind that transport.
///
/// The explicit stack reproduces the recursion exactly: preorder DFS returning
/// the first `type_identifier` in document order, which is why the children go
/// on reversed.
fn base_ident(node: Node, src: &[u8]) -> Option<String> {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "type_identifier" {
            return n.utf8_text(src).ok().map(str::to_string);
        }
        let mut cursor = n.walk();
        let before = stack.len();
        stack.extend(n.children(&mut cursor));
        stack[before..].reverse();
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
            && let Some(name) = item
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
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

    /// A reference type costs ONE source byte per level, so the 512 KiB
    /// per-file cap admits a type node hundreds of thousands deep — well past
    /// what a recursive descendant walk survives. Overflowing there is a
    /// SIGSEGV, not a catchable panic, and this builds on every project open
    /// over whatever the repository holds. Same shape as the two regression
    /// tests guarding `inactive.rs`.
    #[test]
    fn a_deeply_nested_impl_type_does_not_overflow_the_stack() {
        const DEPTH: usize = 50_000;
        let src = format!("impl {}Foo {{}}\n", "&".repeat(DEPTH));
        // `build`'s own per-file cap, so this is a file it would really read.
        assert!(src.len() < 512 * 1024, "stays inside the cap");
        // A deliberately small stack: the recursion this replaced needed one
        // frame per level and died here; the explicit stack does not care.
        // Completing at all is the whole assertion. Verified by reverting
        // `base_ident` to its recursive form and re-running: the process died
        // with `fatal runtime error: stack overflow` and SIGABRT, taking the
        // test harness with it. Nothing is asserted about the RESULT because
        // tree-sitter does not resolve a type through this many references —
        // it produces no `type_identifier`, which is a fine answer. The defect
        // was never a wrong answer; it was not surviving the question.
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                let _ = index_of(&src);
            })
            .expect("spawn")
            .join()
            .expect("the type walk must not overflow the stack");
    }
}
