//! Python's maps:
//!
//! - **class hierarchy** — each class, its bases in order, and its method
//!   resolution order (C3, as Python computes it), a mixin marked;
//! - **pytest fixtures** — each fixture, the tests and fixtures that ask
//!   for it (by parameter name, or `usefixtures`), its scope;
//! - **dynamic dispatch** — what a reader cannot follow by name: `getattr`
//!   with a computed name, dict registries of callables, functions a
//!   decorator registers.

use std::collections::{BTreeMap, HashMap};

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, Source, all_of, ancestor, line, text, unquote};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut out = Vec::new();
    out.extend(hierarchy(corpus));
    out.extend(fixtures(corpus));
    out.extend(dispatch(corpus));
    out
}

fn name_of(node: Node<'_>, src: &str) -> String {
    node.child_by_field_name("name")
        .map(|n| text(n, src).to_string())
        .unwrap_or_default()
}

// -- class hierarchy --------------------------------------------------------------------

struct Class {
    rel: String,
    line: usize,
    bases: Vec<String>,
}

/// The C3 linearization of `class`: `None` where Python would refuse one
/// ("Cannot create a consistent method resolution order").
fn mro(class: &str, classes: &BTreeMap<String, Class>, depth: usize) -> Option<Vec<String>> {
    if depth > 64 {
        return None;
    }
    let bases = match classes.get(class) {
        Some(c) => c.bases.clone(),
        None => return Some(vec![class.to_string()]),
    };
    let mut seqs: Vec<Vec<String>> = Vec::new();
    for base in &bases {
        seqs.push(mro(base, classes, depth + 1)?);
    }
    seqs.push(bases.clone());
    let mut out = vec![class.to_string()];
    loop {
        seqs.retain(|s| !s.is_empty());
        if seqs.is_empty() {
            return Some(out);
        }
        // The first head that is in no other sequence's tail.
        let head = seqs
            .iter()
            .map(|s| s[0].clone())
            .find(|h| !seqs.iter().any(|s| s[1..].contains(h)))?;
        out.push(head.clone());
        for s in &mut seqs {
            if s.first() == Some(&head) {
                s.remove(0);
            }
        }
    }
}

fn hierarchy(corpus: &Corpus) -> Option<ProjectMap> {
    let mut classes: BTreeMap<String, Class> = BTreeMap::new();
    for source in corpus.in_langs(&[Lang::Python]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        for class in all_of(tree.root_node(), "class_definition") {
            let name = name_of(class, src);
            let bases = class
                .child_by_field_name("superclasses")
                .map(|list| {
                    let mut cursor = list.walk();
                    list.named_children(&mut cursor)
                        .filter(|b| matches!(b.kind(), "identifier" | "attribute"))
                        .map(|b| super::last_name(text(b, src)))
                        .collect()
                })
                .unwrap_or_default();
            classes.entry(name).or_insert(Class {
                rel: source.rel.clone(),
                line: line(class),
                bases,
            });
        }
    }
    if !classes.values().any(|c| !c.bases.is_empty()) {
        return None;
    }
    let mut map = MapBuilder::new(
        "python.classes",
        "Class hierarchy (MRO)",
        "each class and the bases it names, in order; its detail is the method resolution order Python looks a method up in",
    );
    for (name, class) in &classes {
        let kind = if name.ends_with("Mixin") {
            "mixin"
        } else {
            "class"
        };
        let at = map.node(kind, name, name);
        map.place(at, &class.rel, class.line);
        let detail = match mro(name, &classes, 0) {
            Some(order) => format!("MRO: {} → object", order.join(" → ")),
            None => "no consistent method resolution order: Python refuses this class".to_string(),
        };
        map.detail(at, detail);
        for (i, base) in class.bases.iter().enumerate() {
            let kind = match (classes.contains_key(base), base.ends_with("Mixin")) {
                (_, true) => "mixin",
                (true, false) => "class",
                (false, false) => "external",
            };
            let to = map.node(kind, base, base);
            let label = match class.bases.len() {
                1 => "inherits".to_string(),
                _ => format!("base {}", i + 1),
            };
            map.edge(at, to, &label);
        }
    }
    map.finish()
}

// -- pytest fixtures ----------------------------------------------------------------------------

fn decorator_names<'t>(def: Node<'t>, src: &str) -> Vec<(String, Node<'t>)> {
    let Some(decorated) = def.parent().filter(|p| p.kind() == "decorated_definition") else {
        return Vec::new();
    };
    let mut cursor = decorated.walk();
    decorated
        .named_children(&mut cursor)
        .filter(|d| d.kind() == "decorator")
        .filter_map(|d| {
            let inner = d.named_child(0)?;
            let callee = match inner.kind() {
                "call" => inner.child_by_field_name("function")?,
                _ => inner,
            };
            Some((text(callee, src).to_string(), inner))
        })
        .collect()
}

fn parameters(def: Node<'_>, src: &str) -> Vec<String> {
    let Some(params) = def.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut cursor = params.walk();
    params
        .named_children(&mut cursor)
        .filter_map(|p| match p.kind() {
            "identifier" => Some(text(p, src).to_string()),
            "default_parameter" | "typed_parameter" | "typed_default_parameter" => p
                .child_by_field_name("name")
                .or_else(|| p.named_child(0))
                .map(|n| text(n, src).to_string()),
            _ => None,
        })
        .filter(|n| n != "self" && n != "cls" && n != "request")
        .collect()
}

fn fixtures(corpus: &Corpus) -> Option<ProjectMap> {
    struct Fn<'a> {
        source: &'a Source,
        name: String,
        line: usize,
        params: Vec<String>,
        fixture: Option<String>,
        uses: Vec<String>,
    }
    let mut functions: Vec<Fn> = Vec::new();
    for source in corpus.in_langs(&[Lang::Python]) {
        let name = source.name();
        let testy =
            name.starts_with("test_") || name.ends_with("_test.py") || name == "conftest.py";
        if !testy && !source.text.contains("pytest") {
            continue;
        }
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        for def in all_of(tree.root_node(), "function_definition") {
            let decorators = decorator_names(def, src);
            let fixture = decorators
                .iter()
                .find(|(n, _)| n == "pytest.fixture" || n == "fixture" || n.ends_with(".fixture"))
                .map(|(_, call)| {
                    let scope = all_of(*call, "keyword_argument").into_iter().find_map(|k| {
                        let key = k.child_by_field_name("name").map(|n| text(n, src))?;
                        (key == "scope")
                            .then(|| {
                                k.child_by_field_name("value")
                                    .and_then(|v| unquote(text(v, src)))
                            })
                            .flatten()
                    });
                    scope.unwrap_or_else(|| "function".into())
                });
            let uses: Vec<String> = decorators
                .iter()
                .filter(|(n, _)| n.ends_with("usefixtures"))
                .flat_map(|(_, call)| {
                    all_of(*call, "string")
                        .into_iter()
                        .filter_map(|s| unquote(text(s, src)))
                })
                .collect();
            functions.push(Fn {
                source,
                name: name_of(def, src),
                line: line(def),
                params: parameters(def, src),
                fixture,
                uses,
            });
        }
    }
    let fixtures: HashMap<&str, usize> = functions
        .iter()
        .enumerate()
        .filter(|(_, f)| f.fixture.is_some())
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    if fixtures.is_empty() {
        return None;
    }
    let mut map = MapBuilder::new(
        "pytest.fixtures",
        "pytest fixtures",
        "each fixture (its scope in its detail) and an arrow to it from each test or fixture that asks for it — by parameter name, or with usefixtures",
    );
    for f in &functions {
        let is_test = f.name.starts_with("test");
        let asks: Vec<&String> = f
            .params
            .iter()
            .chain(f.uses.iter())
            .filter(|p| fixtures.contains_key(p.as_str()))
            .collect();
        if f.fixture.is_none() && (!is_test || asks.is_empty()) {
            continue;
        }
        let kind = if f.fixture.is_some() {
            "fixture"
        } else {
            "test"
        };
        let key = format!("{}:{}", f.source.rel, f.line);
        let at = map.node(kind, &key, &f.name);
        map.place(at, &f.source.rel, f.line);
        if let Some(scope) = &f.fixture {
            map.detail(at, format!("scope: {scope}"));
        }
        for p in asks {
            // A conftest's fixture the nearest one by path; one in the same
            // file first.
            let candidates: Vec<&Fn> = functions
                .iter()
                .filter(|g| g.fixture.is_some() && &g.name == p)
                .collect();
            let target = candidates
                .iter()
                .max_by_key(|g| {
                    let shared = g
                        .source
                        .rel
                        .split('/')
                        .zip(f.source.rel.split('/'))
                        .take_while(|(a, b)| a == b)
                        .count();
                    (g.source.rel == f.source.rel, shared)
                })
                .copied();
            let Some(g) = target else {
                continue;
            };
            let to = map.node("fixture", &format!("{}:{}", g.source.rel, g.line), &g.name);
            map.place(to, &g.source.rel, g.line);
            map.edge(at, to, "uses");
        }
    }
    map.finish()
}

// -- dynamic dispatch ------------------------------------------------------------------------------

fn dispatch(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "python.dispatch",
        "Dynamic dispatch",
        "where Python picks what to call at run time: getattr with a computed name, dict registries of callables, functions a decorator registers — and the functions they can reach",
    );
    let defs = corpus.defs();
    for source in corpus.in_langs(&[Lang::Python]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        let enclosing = |n: Node<'_>| -> String {
            ancestor(n, &["function_definition", "class_definition"])
                .map(|d| name_of(d, src))
                .unwrap_or_else(|| "<module>".into())
        };
        // `getattr(obj, name)` with a name not written out.
        for call in all_of(root, "call") {
            let function = call
                .child_by_field_name("function")
                .map(|f| text(f, src))
                .unwrap_or("");
            if function != "getattr" {
                continue;
            }
            let args = call.child_by_field_name("arguments");
            let name = args.and_then(|a| a.named_child(1));
            if name.is_none_or(|n| n.kind() == "string") {
                continue;
            }
            let key = format!("{}:{}", source.rel, line(call));
            let at = map.node("getattr", &key, &format!("getattr in {}", enclosing(call)));
            map.place(at, &source.rel, line(call));
            map.detail(
                at,
                text(call, src)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            );
        }
        // `def __getattr__`: every missing attribute goes through it.
        for def in all_of(root, "function_definition") {
            if name_of(def, src) == "__getattr__" {
                let class = ancestor(def, &["class_definition"])
                    .map(|c| name_of(c, src))
                    .unwrap_or_else(|| "<module>".into());
                let at = map.node(
                    "getattr",
                    &format!("{}:{}", source.rel, line(def)),
                    &format!("{class}.__getattr__"),
                );
                map.place(at, &source.rel, line(def));
                map.detail(at, "every attribute the class lacks is looked up here");
            }
        }
        // `HANDLERS = {"a": handle_a, …}`: a registry.
        for assignment in all_of(root, "assignment") {
            let (Some(left), Some(right)) = (
                assignment.child_by_field_name("left"),
                assignment.child_by_field_name("right"),
            ) else {
                continue;
            };
            if right.kind() != "dictionary" {
                continue;
            }
            let pairs: Vec<(String, String)> = all_of(right, "pair")
                .into_iter()
                .filter_map(|p| {
                    let key = p.child_by_field_name("key")?;
                    let value = p.child_by_field_name("value")?;
                    matches!(value.kind(), "identifier" | "attribute").then(|| {
                        (
                            text(key, src).to_string(),
                            super::last_name(text(value, src)),
                        )
                    })
                })
                .filter(|(_, v)| defs.named(v).iter().any(|d| d.kind != "variable"))
                .collect();
            if pairs.len() < 2 {
                continue;
            }
            let registry = map.node(
                "registry",
                &format!("{}:{}", source.rel, line(assignment)),
                text(left, src),
            );
            map.place(registry, &source.rel, line(assignment));
            for (key, value) in pairs {
                if let Some(def) = defs.find(&value, None, &source.rel) {
                    let to = map.node("function", &format!("{}:{}", def.rel, def.line), &def.name);
                    map.place(to, &def.rel.clone(), def.line);
                    map.edge(registry, to, &key);
                }
            }
        }
        // `@register("x")`, `@registry.register`: registered by a decorator.
        for def in all_of(root, "function_definition")
            .into_iter()
            .chain(all_of(root, "class_definition"))
        {
            for (decorator, call) in decorator_names(def, src) {
                let last = super::last_name(&decorator);
                if !(last.starts_with("register") || last.ends_with("handler") || last == "on") {
                    continue;
                }
                let label = match call.kind() {
                    "call" => {
                        let arg = call
                            .child_by_field_name("arguments")
                            .and_then(|a| a.named_child(0))
                            .map(|a| text(a, src).to_string())
                            .unwrap_or_default();
                        format!("@{decorator}({arg})")
                    }
                    _ => format!("@{decorator}"),
                };
                let registry = map.node("registry", &decorator, &format!("@{decorator}"));
                let to = map.node(
                    "function",
                    &format!("{}:{}", source.rel, line(def)),
                    &name_of(def, src),
                );
                map.place(to, &source.rel, line(def));
                if map
                    .get(registry.unwrap_or(usize::MAX))
                    .is_some_and(|n| n.file.is_empty())
                {
                    map.place(registry, &source.rel, line(def));
                }
                map.edge(
                    registry,
                    to,
                    label.trim_start_matches(&format!("@{decorator}")),
                );
            }
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

    fn find<'a>(maps: &'a [ProjectMap], id: &str) -> &'a ProjectMap {
        maps.iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("{id}"))
    }

    #[test]
    fn classes_have_their_method_resolution_order() {
        let maps = maps(&Corpus::of(&[(
            "shapes.py",
            "class A:\n    pass\n\nclass B(A):\n    pass\n\nclass C(A):\n    pass\n\nclass LogMixin:\n    pass\n\nclass D(LogMixin, B, C):\n    pass\n\nclass E(abc.ABC):\n    pass\n\nclass X(A, B):\n    pass\n",
        )]));
        let classes = find(&maps, "python.classes");
        let detail = |name: &str| {
            classes
                .nodes
                .iter()
                .find(|n| n.label == name)
                .unwrap()
                .detail
                .clone()
        };
        assert_eq!(detail("D"), "MRO: D → LogMixin → B → C → A → object");
        assert_eq!(detail("E"), "MRO: E → ABC → object");
        assert!(
            detail("X").contains("Python refuses"),
            "A before B, B before A: {}",
            detail("X")
        );
        let got = edges(classes);
        assert!(got.contains(&e("D", "LogMixin", "base 1")));
        assert!(got.contains(&e("D", "C", "base 3")));
        assert!(got.contains(&e("B", "A", "inherits")));
        assert_eq!(
            classes
                .nodes
                .iter()
                .find(|n| n.label == "LogMixin")
                .unwrap()
                .kind,
            "mixin"
        );
        assert_eq!(
            classes
                .nodes
                .iter()
                .find(|n| n.label == "ABC")
                .unwrap()
                .kind,
            "external"
        );
    }

    #[test]
    fn pytest_fixtures_and_who_asks_for_them() {
        let maps = maps(&Corpus::of(&[
            (
                "tests/conftest.py",
                "import pytest\n\n@pytest.fixture(scope=\"session\")\ndef db():\n    pass\n\n@pytest.fixture\ndef user(db):\n    pass\n",
            ),
            (
                "tests/api/test_users.py",
                "import pytest\n\n@pytest.fixture\ndef client():\n    pass\n\ndef test_create(client, user, tmp_path):\n    pass\n\n@pytest.mark.usefixtures(\"db\")\ndef test_migrate():\n    pass\n\ndef helper(user):\n    pass\n",
            ),
        ]));
        let fixtures = find(&maps, "pytest.fixtures");
        let got = edges(fixtures);
        for want in [
            e("test_create", "client", "uses"),
            e("test_create", "user", "uses"),
            e("test_migrate", "db", "uses"),
            e("user", "db", "uses"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(a, _, _)| a == "helper"),
            "not a test: {got:?}"
        );
        let db = fixtures.nodes.iter().find(|n| n.label == "db").unwrap();
        assert_eq!(
            (db.file.as_str(), db.line, db.detail.as_str()),
            ("tests/conftest.py", 4, "scope: session")
        );
    }

    #[test]
    fn dynamic_dispatch_sites_and_registries() {
        let maps = maps(&Corpus::of(&[(
            "app/commands.py",
            "def start():\n    pass\n\ndef stop():\n    pass\n\nCOMMANDS = {\"start\": start, \"stop\": stop}\n\ndef run(obj, name):\n    return getattr(obj, name)()\n\ndef safe(obj):\n    return getattr(obj, 'x')\n\n@register('ping')\ndef ping():\n    pass\n\nclass Proxy:\n    def __getattr__(self, name):\n        pass\n",
        )]));
        let dispatch = find(&maps, "python.dispatch");
        let got = edges(dispatch);
        assert!(
            got.contains(&e("COMMANDS", "start", "\"start\"")),
            "{got:?}"
        );
        assert!(got.contains(&e("COMMANDS", "stop", "\"stop\"")), "{got:?}");
        assert!(got.contains(&e("@register", "ping", "('ping')")), "{got:?}");
        let labels: Vec<&str> = dispatch.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"getattr in run"), "{labels:?}");
        assert!(
            !labels.contains(&"getattr in safe"),
            "a literal name is no dispatch: {labels:?}"
        );
        assert!(labels.contains(&"Proxy.__getattr__"), "{labels:?}");
    }
}
