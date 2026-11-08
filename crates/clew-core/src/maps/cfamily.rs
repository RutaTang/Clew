//! C and C++'s maps:
//!
//! - **include cost** — each file and the files it includes, and in its
//!   detail how many it pulls in all told (what including it costs every
//!   file that does);
//! - **virtual overrides** — each method that overrides a virtual one of a
//!   base class, an arrow to the one it overrides;
//! - **template instantiations** — each of the project's templates and the
//!   arguments it is instantiated with.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use clew_protocol::ProjectMap;

use super::{Corpus, MapBuilder, all_of, line, text, unquote};
use crate::highlight::Lang;

const LANGS: &[Lang] = &[Lang::C, Lang::Cpp];

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut out = Vec::new();
    out.extend(includes(corpus));
    out.extend(overrides(corpus));
    out.extend(templates(corpus));
    out
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

/// `a/./b/../c` folded to `a/c`.
fn normal(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

fn includes(corpus: &Corpus) -> Option<ProjectMap> {
    let files: BTreeSet<&str> = corpus.in_langs(LANGS).map(|s| s.rel.as_str()).collect();
    // A header by its trailing path, for `<lib/x.h>` and include paths.
    let resolve = |from: &str, path: &str, quoted: bool| -> Option<String> {
        if quoted {
            let near = normal(&format!("{}/{path}", dir_of(from)));
            if files.contains(near.as_str()) {
                return Some(near);
            }
        }
        files
            .iter()
            .filter(|f| f.ends_with(&format!("/{path}")) || **f == path)
            .min_by_key(|f| f.len())
            .map(|f| f.to_string())
    };
    let mut graph: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    let mut lines: HashMap<(String, String), usize> = HashMap::new();
    for source in corpus.in_langs(LANGS) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let entry = graph.entry(source.rel.clone()).or_default();
        for include in all_of(tree.root_node(), "preproc_include") {
            let Some(path) = include.child_by_field_name("path") else {
                continue;
            };
            let written = text(path, src);
            let quoted = written.starts_with('"');
            let raw = match quoted {
                true => unquote(written).unwrap_or_default(),
                false => written.trim_matches(['<', '>']).to_string(),
            };
            match resolve(&source.rel, &raw, quoted) {
                Some(target) => {
                    lines.insert((source.rel.clone(), target.clone()), line(include));
                    entry.push((target, true));
                }
                None => {
                    lines.insert((source.rel.clone(), raw.clone()), line(include));
                    entry.push((raw, false));
                }
            }
        }
    }
    if !graph.values().any(|v| !v.is_empty()) {
        return None;
    }
    // What a file pulls in, all told: its project includes' closure.
    let closure = |start: &str| -> (usize, usize) {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut system: BTreeSet<String> = BTreeSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(f) = stack.pop() {
            for (t, project) in graph.get(&f).into_iter().flatten() {
                match project {
                    true => {
                        if t != start && seen.insert(t.clone()) {
                            stack.push(t.clone());
                        }
                    }
                    false => {
                        system.insert(t.clone());
                    }
                }
            }
        }
        (seen.len(), system.len())
    };
    let mut map = MapBuilder::new(
        "c.includes",
        "Include cost",
        "each file and an arrow to each file it includes; its detail counts every file it pulls in, directly or not — which every file that includes it pulls in too",
    );
    for (file, targets) in &graph {
        let at = map.node("file", file, file);
        map.place(at, file, 1);
        let (project, system) = closure(file);
        map.detail(
            at,
            format!("pulls in {project} project files and {system} system headers"),
        );
        for (t, project) in targets {
            let to = match project {
                true => map.node("file", t, t),
                false => map.node("system", t, &format!("<{t}>")),
            };
            let label = match lines.get(&(file.clone(), t.clone())) {
                Some(l) => format!("line {l}"),
                None => String::new(),
            };
            map.edge(at, to, &label);
        }
    }
    map.finish()
}

fn overrides(corpus: &Corpus) -> Option<ProjectMap> {
    struct Class {
        rel: String,
        line: usize,
        bases: Vec<String>,
        /// Each method: its name, whether it is virtual (or says override),
        /// its line.
        methods: Vec<(String, bool, usize)>,
    }
    let mut classes: BTreeMap<String, Class> = BTreeMap::new();
    for source in corpus.in_langs(&[Lang::Cpp]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        for class in all_of(root, "class_specifier")
            .into_iter()
            .chain(all_of(root, "struct_specifier"))
        {
            let (Some(name), Some(body)) = (
                class.child_by_field_name("name"),
                class.child_by_field_name("body"),
            ) else {
                continue;
            };
            let bases = all_of(class, "base_class_clause")
                .first()
                .map(|b| {
                    all_of(*b, "type_identifier")
                        .into_iter()
                        .chain(all_of(*b, "qualified_identifier"))
                        .map(|t| super::last_name(text(t, src)))
                        .collect()
                })
                .unwrap_or_default();
            let mut methods = Vec::new();
            let mut cursor = body.walk();
            for member in body.named_children(&mut cursor) {
                if !matches!(
                    member.kind(),
                    "field_declaration" | "function_definition" | "declaration"
                ) {
                    continue;
                }
                let Some(declarator) = all_of(member, "function_declarator").into_iter().next()
                else {
                    continue;
                };
                let Some(fname) = declarator.child_by_field_name("declarator") else {
                    continue;
                };
                let written = text(member, src);
                let virtual_ = written.trim_start().starts_with("virtual")
                    || all_of(member, "virtual_specifier")
                        .iter()
                        .any(|v| matches!(text(*v, src), "override" | "final"));
                methods.push((
                    super::last_name(text(fname, src))
                        .trim_start_matches('~')
                        .to_string(),
                    virtual_,
                    line(member),
                ));
            }
            classes.insert(
                text(name, src).to_string(),
                Class {
                    rel: source.rel.clone(),
                    line: line(class),
                    bases,
                    methods,
                },
            );
        }
    }
    let mut map = MapBuilder::new(
        "cpp.overrides",
        "Virtual overrides",
        "each method that overrides a virtual method of a base class, with an arrow to the one it overrides",
    );
    // The virtual `method` nearest up `class`'s bases.
    fn find_virtual<'c>(
        class: &str,
        method: &str,
        classes: &'c BTreeMap<String, Class>,
        depth: usize,
    ) -> Option<(&'c str, &'c Class, usize)> {
        if depth > 16 {
            return None;
        }
        let c = classes.get(class)?;
        for base in &c.bases {
            if let Some((name, b)) = classes.get_key_value(base.as_str()) {
                if let Some((_, _, line)) = b.methods.iter().find(|(m, v, _)| m == method && *v) {
                    return Some((name.as_str(), b, *line));
                }
                if let Some(found) = find_virtual(base, method, classes, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }
    for (name, class) in &classes {
        for (method, _, line) in &class.methods {
            let Some((base, b, base_line)) = find_virtual(name, method, &classes, 0) else {
                continue;
            };
            let from = map.node(
                "method",
                &format!("{name}::{method}"),
                &format!("{name}::{method}"),
            );
            map.place(from, &class.rel, *line);
            let to = map.node(
                "method",
                &format!("{base}::{method}"),
                &format!("{base}::{method}"),
            );
            map.place(to, &b.rel, base_line);
            map.edge(from, to, "overrides");
        }
        let _ = class.line;
    }
    map.finish()
}

fn templates(corpus: &Corpus) -> Option<ProjectMap> {
    let mut declared: BTreeMap<String, (String, usize)> = BTreeMap::new();
    for source in corpus.in_langs(&[Lang::Cpp]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        for template in all_of(tree.root_node(), "template_declaration") {
            let mut cursor = template.walk();
            for child in template.named_children(&mut cursor) {
                let name = match child.kind() {
                    "class_specifier" | "struct_specifier" | "alias_declaration" => {
                        child.child_by_field_name("name")
                    }
                    "function_definition" | "declaration" => all_of(child, "function_declarator")
                        .first()
                        .and_then(|d| d.child_by_field_name("declarator")),
                    _ => None,
                };
                if let Some(name) = name {
                    declared.insert(
                        super::last_name(text(name, src)),
                        (source.rel.clone(), line(template)),
                    );
                }
            }
        }
    }
    if declared.is_empty() {
        return None;
    }
    let mut map = MapBuilder::new(
        "cpp.templates",
        "Template instantiations",
        "each of the project's templates and the arguments it is instantiated with, an arrow from each instantiation to it",
    );
    for source in corpus.in_langs(&[Lang::Cpp]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        for use_ in all_of(root, "template_type")
            .into_iter()
            .chain(all_of(root, "template_function"))
        {
            let (Some(name), Some(args)) = (
                use_.child_by_field_name("name"),
                use_.child_by_field_name("arguments"),
            ) else {
                continue;
            };
            let name = super::last_name(text(name, src));
            let Some((rel, at)) = declared.get(&name) else {
                continue;
            };
            let args: String = text(args, src)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let label = format!("{name}{args}");
            let t = map.node("template", &name, &name);
            map.place(t, rel, *at);
            let i = map.node("instantiation", &label, &label);
            map.place(i, &source.rel, line(use_));
            map.edge(i, t, "instantiates");
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

    #[test]
    fn includes_and_what_each_pulls_in() {
        let maps = maps(&Corpus::of(&[
            (
                "src/main.c",
                "#include <stdio.h>\n#include \"util/str.h\"\n#include \"config.h\"\nint main(void) { return 0; }\n",
            ),
            (
                "src/util/str.h",
                "#include \"../config.h\"\n#include <string.h>\n",
            ),
            ("src/config.h", "#define X 1\n"),
            ("include/lib/api.h", "#include \"lib/types.h\"\n"),
            ("include/lib/types.h", ""),
            ("src/use.c", "#include <lib/api.h>\n"),
        ]));
        let inc = maps.iter().find(|m| m.id == "c.includes").unwrap();
        let got = edges(inc);
        for want in [
            e("src/main.c", "<stdio.h>", "line 1"),
            e("src/main.c", "src/util/str.h", "line 2"),
            e("src/util/str.h", "src/config.h", "line 1"),
            e("src/use.c", "include/lib/api.h", "line 1"),
            e("include/lib/api.h", "include/lib/types.h", "line 1"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let main = inc.nodes.iter().find(|n| n.label == "src/main.c").unwrap();
        assert_eq!(
            main.detail, "pulls in 2 project files and 2 system headers",
            "string.h through str.h"
        );
        let use_ = inc.nodes.iter().find(|n| n.label == "src/use.c").unwrap();
        assert_eq!(use_.detail, "pulls in 2 project files and 0 system headers");
        assert_eq!(normal("a/./b/../c"), "a/c");
    }

    #[test]
    fn overrides_reach_the_virtual_they_override() {
        let maps = maps(&Corpus::of(&[
            (
                "shape.hpp",
                "class Shape {\npublic:\n  virtual double area() const = 0;\n  virtual ~Shape();\n  void name();\n};\n",
            ),
            (
                "circle.hpp",
                "class Circle : public Shape {\n  double area() const override;\n  void name();\n};\nclass Unit : public Circle {\n  double area() const final;\n};\n",
            ),
        ]));
        let o = maps.iter().find(|m| m.id == "cpp.overrides").unwrap();
        let got = edges(o);
        assert!(
            got.contains(&e("Circle::area", "Shape::area", "overrides")),
            "{got:?}"
        );
        assert!(
            got.contains(&e("Unit::area", "Circle::area", "overrides")),
            "{got:?}"
        );
        assert!(
            !got.iter().any(|(a, _, _)| a == "Circle::name"),
            "name is not virtual: {got:?}"
        );
        let base = o.nodes.iter().find(|n| n.label == "Shape::area").unwrap();
        assert_eq!((base.file.as_str(), base.line), ("shape.hpp", 3));
    }

    #[test]
    fn templates_and_their_instantiations() {
        let maps = maps(&Corpus::of(&[
            (
                "vec.hpp",
                "template <typename T>\nclass Vec {};\ntemplate <typename T>\nT max_of(T a, T b) { return a; }\n",
            ),
            (
                "use.cpp",
                "#include \"vec.hpp\"\nVec<int> a;\nVec< double > b;\nstd::vector<int> c;\nint m = max_of<int>(1, 2);\n",
            ),
        ]));
        let t = maps.iter().find(|m| m.id == "cpp.templates").unwrap();
        let got = edges(t);
        assert!(
            got.contains(&e("Vec<int>", "Vec", "instantiates")),
            "{got:?}"
        );
        assert!(
            got.contains(&e("Vec< double >", "Vec", "instantiates"))
                || got.contains(&e("Vec<double>", "Vec", "instantiates"))
                || got.iter().any(|(a, _, _)| a.contains("double")),
            "{got:?}"
        );
        assert!(
            got.contains(&e("max_of<int>", "max_of", "instantiates")),
            "{got:?}"
        );
        assert!(
            !got.iter().any(|(_, b, _)| b == "vector"),
            "not the project's: {got:?}"
        );
        let vec = t.nodes.iter().find(|n| n.label == "Vec").unwrap();
        assert_eq!((vec.file.as_str(), vec.line), ("vec.hpp", 1));
    }
}
