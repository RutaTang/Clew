//! What a Rust `use` means beyond its raw path — the facts the import graph
//! needs that extraction alone cannot state:
//!
//!   * **Scope** ([`scope_rust_imports`]): a `use` written inside an inline
//!     `mod name { … }` block is relative to that block, not to the file.
//!     Extraction sees every `use` in the file but not the block it sits in;
//!     this pass re-expresses each path relative to the FILE's module, which
//!     is what resolution works in.
//!   * **Re-exports and globs** ([`RustUse`]): whether a declaration re-exports
//!     what it names (`pub use`) and whether it is a glob (`use a::*`). The
//!     graph counts neither a re-export nor a glob import of an enclosing
//!     module as a dependency, so a crate root that re-exports its modules and
//!     children that `use crate::*` / `use super::*` back do not read as one
//!     big import cycle. Both facts are carried IN the specifier string
//!     ([`REEXPORT`], [`GLOB`]), so the index cache and the protocol carry them
//!     unchanged.
//!
//! Lives in clew-core so the client's own indexer and the server's
//! project-symbol snapshot hand the resolver identical specifiers: a remote
//! project's test-module `use super::*` must not read as "this file depends
//! on its parent" any more than a local one's.

use std::collections::{HashMap, VecDeque};

use crate::highlight::Lang;
use crate::imports::RawImport;

/// Prefix of a `use` specifier whose declaration re-exports it: `pub use`,
/// `pub(crate) use`, `pub(super) use`, `pub(in …) use` — any visibility wider
/// than the declaring module (`pub(self)` is private). Paths are extracted
/// without whitespace, so no real path can begin with `pub `.
pub const REEXPORT: &str = "pub ";

/// The last segment of a glob import's specifier: `use a::b::*` is recorded as
/// `a::b::*` — the module the glob reads from, plus the star.
pub const GLOB: &str = "*";

/// A Rust `use` specifier, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustUse<'a> {
    /// The path, without the re-export marker and without a glob's `::*`: the
    /// module a glob reads from, else the module or item imported.
    pub path: &'a str,
    /// `use path::*`.
    pub glob: bool,
    /// `pub use …` (see [`REEXPORT`]).
    pub reexport: bool,
}

impl<'a> RustUse<'a> {
    /// Decode a specifier as extraction records it.
    pub fn parse(module: &'a str) -> Self {
        let (reexport, rest) = match module.strip_prefix(REEXPORT) {
            Some(rest) => (true, rest),
            None => (false, module),
        };
        let (glob, path) = match rest.strip_suffix(GLOB) {
            Some("") => (true, ""),
            Some(head) => match head.strip_suffix("::") {
                Some(path) => (true, path),
                None => (false, rest),
            },
            None => (false, rest),
        };
        RustUse {
            path,
            glob,
            reexport,
        }
    }

    /// The specifier for `path` with these markers — the inverse of
    /// [`RustUse::parse`].
    pub fn encode(path: &str, glob: bool, reexport: bool) -> String {
        let mut out = String::with_capacity(path.len() + 7);
        if reexport {
            out.push_str(REEXPORT);
        }
        out.push_str(path);
        if glob {
            if !path.is_empty() {
                out.push_str("::");
            }
            out.push_str(GLOB);
        }
        out
    }

    /// Whether the path names only an enclosing module of the importing one:
    /// `crate`, `super`, `super::super`, `self`. A plain `use` cannot import
    /// such a path, so it can only be a glob — including one recorded by an
    /// older extraction that dropped the `::*`.
    pub fn names_an_enclosing_module(&self) -> bool {
        !self.path.is_empty()
            && self
                .path
                .split("::")
                .all(|s| matches!(s, "crate" | "super" | "self"))
    }
}

/// [`scope_rust_imports`] for a file of language `lang` (a `highlight::detect`
/// key); every other language's imports are already file-relative.
pub fn scope_imports(source: &str, lang: &str, raw: Vec<RawImport>) -> Vec<RawImport> {
    if lang == "rust" {
        scope_rust_imports(source, raw)
    } else {
        raw
    }
}

/// Rewrite Rust imports extracted from inside inline `mod name { … }` blocks
/// so they are relative to the FILE's module, which is what resolution works
/// in. The block changes what a path means:
///   * `mod tests { use super::*; }` imports the FILE itself (its parent), not
///     the file's parent module — read file-relative, every test module made
///     its file depend on the crate root, and with the root's `mod` edge that
///     made nearly every Rust file part of an import "cycle";
///   * `super::super::x` two blocks deep is `x` in the file's own module;
///   * `self::x` inside `mod a` is `self::a::x`;
///   * `mod b;` inside `mod a { … }` lives at `a/b.rs` (recorded as `a::b`).
///
/// `crate::` and extern paths mean the same everywhere and are left alone.
/// The re-export marker and a glob's star survive the rewrite.
pub fn scope_rust_imports(source: &str, raw: Vec<RawImport>) -> Vec<RawImport> {
    if raw.is_empty() || !source.contains("mod") {
        return raw;
    }
    let Some(tree) = crate::highlight::parse(source, Lang::Rust) else {
        return raw;
    };
    scope_rust_imports_in(&tree, source, raw)
}

/// [`scope_rust_imports`] using the existing Rust syntax tree, for an indexer
/// that extracts all facts from a single parse of the file.
pub fn scope_rust_imports_in(
    tree: &tree_sitter::Tree,
    source: &str,
    raw: Vec<RawImport>,
) -> Vec<RawImport> {
    if raw.is_empty() || !source.contains("mod") {
        return raw;
    }
    let Ok(query) = crate::imports::import_query(Lang::Rust) else {
        return raw;
    };
    // A grouped use emits several imports, sometimes on different lines, and
    // identical uses can occur in different inline modules on the same line.
    // Match extraction's entries in source order, retaining each declaration's
    // actual parent chain rather than a single chain per line. No source
    // positions need to be added to the cache or the wire representation.
    let mut scopes: HashMap<(usize, bool, String), VecDeque<Vec<String>>> = HashMap::new();
    crate::imports::for_each_rust_import(tree.root_node(), source, query, |r, node| {
        let mut chain = Vec::new();
        let mut ancestor = node.parent();
        while let Some(parent) = ancestor {
            if parent.kind() == "mod_item"
                && parent.child_by_field_name("body").is_some()
                && let Some(name) = parent.child_by_field_name("name")
                && let Some(text) = source.get(name.byte_range())
            {
                chain.push(text.to_string());
            }
            ancestor = parent.parent();
        }
        chain.reverse();
        scopes
            .entry((r.line, r.is_mod_decl, r.module))
            .or_default()
            .push_back(chain);
    });
    raw.into_iter()
        .map(|mut r| {
            let chain = scopes
                .get_mut(&(r.line, r.is_mod_decl, r.module.clone()))
                .and_then(VecDeque::pop_front)
                .unwrap_or_default();
            if !chain.is_empty() {
                r.module = if r.is_mod_decl {
                    format!("{}::{}", chain.join("::"), r.module)
                } else {
                    let u = RustUse::parse(&r.module);
                    RustUse::encode(&rescope_use_path(u.path, &chain), u.glob, u.reexport)
                };
            }
            r
        })
        .collect()
}

/// `path` as written inside the inline modules `chain` (outermost first),
/// re-expressed relative to the file's module.
fn rescope_use_path(path: &str, chain: &[String]) -> String {
    let segs: Vec<&str> = path.split("::").filter(|s| !s.is_empty()).collect();
    let d = chain.len();
    let join = |head: Vec<&str>, rest: &[&str]| {
        let mut all = head;
        all.extend_from_slice(rest);
        all.join("::")
    };
    match segs.first() {
        Some(&"super") => {
            let k = segs.iter().take_while(|s| **s == "super").count();
            let rest = &segs[k..];
            if k > d {
                // Climbs out of the file: `super` × (k - d) from the file.
                join(vec!["super"; k - d], rest)
            } else {
                // Lands on an enclosing block of this file (or the file's own
                // module when k == d).
                let mut head = vec!["self"];
                head.extend(chain[..d - k].iter().map(String::as_str));
                join(head, rest)
            }
        }
        Some(&"self") => {
            let mut head = vec!["self"];
            head.extend(chain.iter().map(String::as_str));
            join(head, &segs[1..])
        }
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::imports_of;

    fn scoped(src: &str) -> Vec<(String, bool)> {
        scope_rust_imports(src, imports_of(src, "rust"))
            .into_iter()
            .map(|r| (r.module, r.is_mod_decl))
            .collect()
    }

    #[test]
    fn inline_module_paths_are_rescoped_to_the_file() {
        let src = "use super::outer;\n\
                   fn f<'a>(x: &'a str) -> char { let _ = \"}\"; let _ = '}'; '{' }\n\
                   mod a {\n\
                   \x20   // a brace in a comment: }\n\
                   \x20   /* nested /* } */ } */\n\
                   \x20   const S: &str = r#\"}}\"#;\n\
                   \x20   mod b {\n\
                   \x20       use super::super::x;\n\
                   \x20       use super::y;\n\
                   \x20       use self::z;\n\
                   \x20       use super::super::super::w;\n\
                   \x20       use crate::k;\n\
                   \x20       mod c;\n\
                   \x20   }\n\
                   \x20   use super::after_b;\n\
                   }\n\
                   use self::top;\n\
                   #[cfg(test)]\n\
                   mod tests { use super::*; }\n";
        let got = scoped(src);
        for (want, decl) in [
            ("super::outer", false), // top level: unchanged
            ("self::x", false),      // super × 2 from a::b is the file itself
            ("self::a::y", false),   // super × 1 from a::b is a
            ("self::a::b::z", false),
            ("super::w", false), // one level past the file
            ("crate::k", false),
            ("a::b::c", true), // `mod c;` inside a::b lives at a/b/c.rs
            ("self::after_b", false),
            ("self::top", false),
            ("self::*", false), // the test module's `use super::*`: the file
        ] {
            assert!(
                got.contains(&(want.to_string(), decl)),
                "missing {want:?} in {got:?}"
            );
        }
    }

    #[test]
    fn separate_inline_modules_on_one_line_keep_their_own_import_scope() {
        let src = "pub mod a { pub mod inner { pub struct Item; } pub use self::inner::Item; } pub mod b { pub mod inner { pub struct Item; } pub use self::inner::Item; }";
        assert_eq!(
            scoped(src),
            [
                ("pub self::a::inner::Item".into(), false),
                ("pub self::b::inner::Item".into(), false),
            ]
        );
        // A line break between the modules cannot change what they import.
        assert_eq!(
            scoped(src),
            scoped(&src.replace("} pub mod b", "}\npub mod b"))
        );
    }

    #[test]
    fn same_line_file_modules_and_multiline_groups_use_the_declaration_scope() {
        let src = "pub mod a { mod inner; pub use self::inner::{A,\nC}; } pub mod b { mod inner; pub use self::inner::{B,\nD}; }";
        assert_eq!(
            scoped(src),
            [
                ("a::inner".into(), true),
                ("pub self::a::inner::A".into(), false),
                ("pub self::a::inner::C".into(), false),
                ("b::inner".into(), true),
                ("pub self::b::inner::B".into(), false),
                ("pub self::b::inner::D".into(), false),
            ]
        );
    }

    #[test]
    fn an_inline_raw_identifier_keeps_its_source_spelling() {
        assert_eq!(
            scoped("mod r#type { mod child; use self::child::Item; }"),
            [
                ("r#type::child".into(), true),
                ("self::r#type::child::Item".into(), false),
            ]
        );
    }

    /// The re-export marker and the glob star are part of the specifier, and
    /// rescoping rewrites the path between them without losing either.
    #[test]
    fn rescoping_keeps_the_reexport_marker_and_the_glob() {
        let src = "pub(crate) use a::*;\n\
                   mod inner {\n\
                   \x20   pub use super::helper;\n\
                   \x20   pub(crate) use super::*;\n\
                   \x20   use self::deep::*;\n\
                   }\n";
        let got: Vec<String> = scoped(src).into_iter().map(|(m, _)| m).collect();
        assert_eq!(
            got,
            [
                "pub a::*",
                "pub self::helper",
                "pub self::*",
                "self::inner::deep::*"
            ]
        );
    }

    #[test]
    fn a_specifier_decodes_and_encodes_back() {
        for (spec, path, glob, reexport) in [
            ("crate::a::B", "crate::a::B", false, false),
            ("pub crate::a::B", "crate::a::B", false, true),
            ("super::*", "super", true, false),
            ("pub app::model::*", "app::model", true, true),
            ("crate", "crate", false, false),
        ] {
            let u = RustUse::parse(spec);
            assert_eq!(
                (u.path, u.glob, u.reexport),
                (path, glob, reexport),
                "{spec}"
            );
            assert_eq!(RustUse::encode(u.path, u.glob, u.reexport), spec);
        }
        // Only a whole `::*` segment is a glob.
        assert!(!RustUse::parse("a::b*").glob);
        // Paths that can only have come from a glob.
        for spec in ["super::*", "super", "super::super", "crate", "self::*"] {
            assert!(RustUse::parse(spec).names_an_enclosing_module(), "{spec}");
        }
        for spec in ["crate::a", "super::x::*", "std"] {
            assert!(!RustUse::parse(spec).names_an_enclosing_module(), "{spec}");
        }
    }

    #[test]
    fn only_rust_is_rescoped() {
        let raw = vec![RawImport {
            module: "super::x".into(),
            line: 2,
            is_mod_decl: false,
        }];
        let src = "mod m {\n    use super::x;\n}\n";
        assert_eq!(scope_imports(src, "python", raw.clone()), raw);
        assert_eq!(scope_imports(src, "rust", raw)[0].module, "self::x");
    }
}
