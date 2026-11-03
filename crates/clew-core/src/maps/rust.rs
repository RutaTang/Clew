//! Rust's maps:
//!
//! - **traits** — each type and the traits it implements (`impl Trait for
//!   Type`) or derives (`#[derive(…)]`);
//! - **unsafe code** — each `unsafe fn`, `unsafe impl`, `unsafe trait` and
//!   `unsafe { … }` block, and the project's unsafe functions each block
//!   calls;
//! - **public API** — each module (file) and the items it makes `pub`
//!   (`pub(crate)` and the like are not the API), and what it re-exports
//!   (`pub use`).

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, all_of, ancestor, line, text};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut out = Vec::new();
    out.extend(traits(corpus));
    out.extend(unsafe_code(corpus));
    out.extend(public_api(corpus));
    out
}

/// A type as an impl names it, its generics dropped (`Vec<T>` → `Vec`,
/// `&'a mut Foo` → `Foo`).
fn type_name(written: &str) -> String {
    let t = written
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("'_ ")
        .trim();
    let t = t.strip_prefix("mut ").unwrap_or(t);
    let t = t.split('<').next().unwrap_or(t);
    t.rsplit("::").next().unwrap_or(t).trim().to_string()
}

fn traits(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "rust.traits",
        "Traits and derives",
        "each type and an arrow to each trait it implements (impl Trait for Type) or derives",
    );
    for source in corpus.in_langs(&[Lang::Rust]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        for item in all_of(root, "impl_item") {
            let (Some(trait_), Some(ty)) = (
                item.child_by_field_name("trait"),
                item.child_by_field_name("type"),
            ) else {
                continue;
            };
            let t = type_name(text(ty, src));
            let tr = type_name(text(trait_, src));
            let from = map.node("type", &t, &t);
            let to = map.node("trait", &tr, &tr);
            let label = if text(item, src).trim_start().starts_with("unsafe") {
                "unsafe impl"
            } else {
                "impl"
            };
            map.edge(from, to, label);
            map.add_detail(
                to,
                &format!("impl for {t} at {}:{}", source.rel, line(item)),
            );
        }
        for attribute in all_of(root, "attribute_item") {
            let body = text(attribute, src);
            let Some(inner) = body
                .trim_start_matches("#[")
                .trim_end_matches(']')
                .trim()
                .strip_prefix("derive")
                .map(|r| r.trim().trim_start_matches('(').trim_end_matches(')'))
            else {
                continue;
            };
            // The item the attribute is on: the next sibling that is no
            // attribute or comment.
            let mut next = attribute.next_named_sibling();
            while let Some(n) = next {
                if !matches!(
                    n.kind(),
                    "attribute_item" | "line_comment" | "block_comment"
                ) {
                    break;
                }
                next = n.next_named_sibling();
            }
            let Some(item) =
                next.filter(|n| matches!(n.kind(), "struct_item" | "enum_item" | "union_item"))
            else {
                continue;
            };
            let Some(name) = item.child_by_field_name("name") else {
                continue;
            };
            let t = text(name, src).to_string();
            let from = map.node("type", &t, &t);
            map.place(from, &source.rel, line(item));
            for derived in inner
                .split(',')
                .map(type_name)
                .filter(|d| !d.is_empty())
            {
                let to = map.node("trait", &derived, &derived);
                map.edge(from, to, "derive");
            }
        }
    }
    // Where each type and trait is defined.
    let defs = corpus.defs();
    for at in 0.. {
        let Some(node) = map.get(at) else {
            break;
        };
        if node.file.is_empty() {
            let label = node.label.clone();
            if let Some(def) = defs.named(&label).iter().find(|d| {
                d.rel.ends_with(".rs")
                    && matches!(
                        d.kind.as_str(),
                        "struct" | "enum" | "trait" | "union" | "type"
                    )
            }) {
                let (rel, line) = (def.rel.clone(), def.line);
                map.place(Some(at), &rel, line);
            }
        }
    }
    map.finish()
}

fn fn_name(node: Node<'_>, src: &str) -> String {
    ancestor(node, &["function_item"])
        .and_then(|f| f.child_by_field_name("name"))
        .map(|n| text(n, src).to_string())
        .unwrap_or_else(|| "<item>".into())
}

fn unsafe_code(corpus: &Corpus) -> Option<ProjectMap> {
    // The project's unsafe functions, by name.
    let mut unsafe_fns: std::collections::BTreeMap<String, (String, usize)> = Default::default();
    for source in corpus.in_langs(&[Lang::Rust]) {
        if let Some(tree) = source.tree() {
            for f in all_of(tree.root_node(), "function_item") {
                let modifiers = all_of(f, "function_modifiers");
                let is_unsafe = modifiers.first().is_some_and(|m| {
                    m.parent() == Some(f) && text(*m, &source.text).contains("unsafe")
                });
                if is_unsafe && let Some(name) = f.child_by_field_name("name") {
                    unsafe_fns.insert(
                        text(name, &source.text).to_string(),
                        (source.rel.clone(), line(f)),
                    );
                }
            }
        }
    }
    let mut map = MapBuilder::new(
        "rust.unsafe",
        "Unsafe code",
        "each unsafe fn, unsafe impl, unsafe trait and unsafe block; an arrow from a block to each of the project's unsafe functions it calls",
    );
    for (name, (rel, at)) in &unsafe_fns {
        let n = map.node("unsafe fn", &format!("{rel}:{at}"), name);
        map.place(n, rel, *at);
    }
    for source in corpus.in_langs(&[Lang::Rust]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        for block in all_of(root, "unsafe_block") {
            let at = map.node(
                "unsafe block",
                &format!("{}:{}", source.rel, line(block)),
                &format!("unsafe in {}", fn_name(block, src)),
            );
            map.place(at, &source.rel, line(block));
            map.detail(
                at,
                text(block, src)
                    .lines()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            for call in all_of(block, "call_expression") {
                let Some(function) = call.child_by_field_name("function") else {
                    continue;
                };
                let callee = super::last_name(text(function, src));
                if let Some((rel, line)) = unsafe_fns.get(&callee) {
                    let to = map.node("unsafe fn", &format!("{rel}:{line}"), &callee);
                    map.edge(at, to, "calls");
                }
            }
        }
        for item in all_of(root, "impl_item")
            .into_iter()
            .chain(all_of(root, "trait_item"))
        {
            let written = text(item, src);
            // `unsafe impl`, `pub unsafe trait`: the keyword before the item's.
            let head_words: Vec<&str> = written
                .split('{')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .collect();
            let keyword = head_words
                .iter()
                .position(|w| *w == "impl" || *w == "trait" || w.starts_with("impl<"));
            if !keyword.is_some_and(|k| head_words[..k].contains(&"unsafe")) {
                continue;
            }
            let head = written
                .split('{')
                .next()
                .unwrap_or(written)
                .trim()
                .to_string();
            let kind = if item.kind() == "impl_item" {
                "unsafe impl"
            } else {
                "unsafe trait"
            };
            let at = map.node(kind, &format!("{}:{}", source.rel, line(item)), &head);
            map.place(at, &source.rel, line(item));
        }
    }
    map.finish()
}

fn public_api(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "rust.api",
        "Public API",
        "each module and an arrow to each item it makes pub (not pub(crate)), and to what it re-exports with pub use",
    );
    for source in corpus.in_langs(&[Lang::Rust]) {
        if source.rel.contains("tests/")
            || source.rel.contains("examples/")
            || source.rel.contains("benches/")
        {
            continue;
        }
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        let module = source
            .rel
            .trim_end_matches(".rs")
            .trim_end_matches("/mod")
            .to_string();
        let mut cursor = root.walk();
        for item in root.named_children(&mut cursor) {
            let Some(vis) = all_of(item, "visibility_modifier")
                .into_iter()
                .find(|v| v.parent() == Some(item))
            else {
                continue;
            };
            if text(vis, src) != "pub" {
                continue;
            }
            let m = map.node("module", &module, &module);
            map.place(m, &source.rel, 1);
            if item.kind() == "use_declaration" {
                let path = text(item, src)
                    .trim_start_matches("pub")
                    .trim()
                    .trim_start_matches("use")
                    .trim()
                    .trim_end_matches(';')
                    .to_string();
                let to = map.node("re-export", &format!("{module}:{path}"), &path);
                map.place(to, &source.rel, line(item));
                map.edge(m, to, "pub use");
                continue;
            }
            let Some(name) = item.child_by_field_name("name") else {
                continue;
            };
            let kind = item.kind().trim_end_matches("_item").replace('_', " ");
            let label = text(name, src).to_string();
            let at = map.node(&kind, &format!("{module}::{label}"), &label);
            map.place(at, &source.rel, line(item));
            let signature = text(item, src)
                .split(['{', ';'])
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            map.detail(at, signature);
            map.edge(m, at, "pub");
        }
    }
    map.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(map: &ProjectMap) -> Vec<(String, String, String)> {
        let mut out: Vec<_> = map
            .edges
            .iter()
            .map(|e| {
                (
                    map.nodes[e.from].label.clone(),
                    map.nodes[e.to].label.clone(),
                    e.label.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn e(a: &str, b: &str, l: &str) -> (String, String, String) {
        (a.into(), b.into(), l.into())
    }

    const LIB: &[(&str, &str)] = &[
        (
            "src/lib.rs",
            "pub mod shapes;\n/// A shape.\n#[derive(Debug, Clone, PartialEq)]\n#[serde(rename_all = \"camelCase\")]\npub struct Circle { r: f64 }\n\nimpl std::fmt::Display for Circle {}\nimpl<T: Shape> Area for Vec<T> {}\nunsafe impl Send for Circle {}\npub(crate) fn helper() {}\npub fn area(c: &Circle) -> f64 { unsafe { raw_area(c) } }\npub use shapes::Square;\nfn private() {}\n",
        ),
        (
            "src/shapes.rs",
            "pub trait Area {}\npub struct Square;\npub unsafe fn raw_area(c: *const Circle) -> f64 { 0.0 }\npub unsafe trait Zeroable {}\n",
        ),
    ];

    #[test]
    fn traits_implemented_and_derived() {
        let maps = maps(&Corpus::of(LIB));
        let traits = maps.iter().find(|m| m.id == "rust.traits").unwrap();
        let got = edges(traits);
        for want in [
            e("Circle", "Debug", "derive"),
            e("Circle", "Clone", "derive"),
            e("Circle", "PartialEq", "derive"),
            e("Circle", "Display", "impl"),
            e("Circle", "Send", "unsafe impl"),
            e("Vec", "Area", "impl"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let circle = traits.nodes.iter().find(|n| n.label == "Circle").unwrap();
        assert_eq!((circle.file.as_str(), circle.line), ("src/lib.rs", 5));
        let area = traits.nodes.iter().find(|n| n.label == "Area").unwrap();
        assert_eq!(area.file, "src/shapes.rs", "the trait where it is defined");
    }

    #[test]
    fn unsafe_code_and_what_it_calls() {
        let maps = maps(&Corpus::of(LIB));
        let u = maps.iter().find(|m| m.id == "rust.unsafe").unwrap();
        let got = edges(u);
        assert_eq!(got, [e("unsafe in area", "raw_area", "calls")]);
        let kinds: Vec<(&str, &str)> = u
            .nodes
            .iter()
            .map(|n| (n.kind.as_str(), n.label.as_str()))
            .collect();
        assert!(
            kinds.contains(&("unsafe impl", "unsafe impl Send for Circle")),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("unsafe trait", "pub unsafe trait Zeroable"))
                || kinds
                    .iter()
                    .any(|(k, l)| *k == "unsafe trait" && l.contains("Zeroable")),
            "{kinds:?}"
        );
        assert!(kinds.contains(&("unsafe fn", "raw_area")), "{kinds:?}");
    }

    #[test]
    fn the_public_api_by_module() {
        let maps = maps(&Corpus::of(LIB));
        let api = maps.iter().find(|m| m.id == "rust.api").unwrap();
        let got = edges(api);
        for want in [
            e("src/lib", "Circle", "pub"),
            e("src/lib", "area", "pub"),
            e("src/lib", "shapes", "pub"),
            e("src/lib", "shapes::Square", "pub use"),
            e("src/shapes", "Area", "pub"),
            e("src/shapes", "raw_area", "pub"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(_, b, _)| b == "helper" || b == "private"),
            "{got:?}"
        );
        let area = api.nodes.iter().find(|n| n.label == "area").unwrap();
        assert_eq!(area.detail, "pub fn area(c: &Circle) -> f64");
        assert_eq!(type_name("&'a mut std::vec::Vec<T>"), "Vec");
    }
}
