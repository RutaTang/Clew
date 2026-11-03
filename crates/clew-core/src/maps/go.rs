//! Go's maps:
//!
//! - **interface satisfaction** — Go types satisfy interfaces without
//!   saying so: each type and each interface whose method set it has (by
//!   method name and number of parameters, embedded interfaces expanded,
//!   methods promoted from embedded structs included), the common standard
//!   library interfaces (`error`, `fmt.Stringer`, `io.Reader`,
//!   `http.Handler`, …) among them;
//! - **goroutines and channels** — which functions start which goroutines,
//!   and which send on, receive from and close which channels;
//! - **packages** — the project's packages and what each imports of the
//!   others, the import cycles and the layers.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, all_of, ancestor, line, text, unquote};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut out = Vec::new();
    out.extend(interfaces(corpus));
    out.extend(goroutines(corpus));
    out.extend(packages(corpus));
    out
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

/// The number of parameters a parameter list declares.
fn arity(params: Option<Node<'_>>) -> usize {
    let Some(params) = params else {
        return 0;
    };
    let mut cursor = params.walk();
    params
        .named_children(&mut cursor)
        .map(|p| match p.kind() {
            "parameter_declaration" => {
                let mut c = p.walk();
                p.children_by_field_name("name", &mut c).count().max(1)
            }
            "variadic_parameter_declaration" => 1,
            _ => 0,
        })
        .sum()
}

/// The common standard library interfaces: name and `(method, arity)`s.
const STD: &[(&str, &[(&str, usize)])] = &[
    ("error", &[("Error", 0)]),
    ("fmt.Stringer", &[("String", 0)]),
    ("io.Reader", &[("Read", 1)]),
    ("io.Writer", &[("Write", 1)]),
    ("io.Closer", &[("Close", 0)]),
    ("io.ReadCloser", &[("Read", 1), ("Close", 0)]),
    ("io.WriteCloser", &[("Write", 1), ("Close", 0)]),
    ("io.ReadWriter", &[("Read", 1), ("Write", 1)]),
    ("http.Handler", &[("ServeHTTP", 2)]),
    ("sort.Interface", &[("Len", 0), ("Less", 2), ("Swap", 2)]),
    ("json.Marshaler", &[("MarshalJSON", 0)]),
    ("json.Unmarshaler", &[("UnmarshalJSON", 1)]),
];

/// An interface a type may satisfy: its name, whole method set, and where
/// it is declared (none for the standard library's).
type Satisfiable = (String, BTreeSet<(String, usize)>, Option<(String, usize)>);

struct Interface {
    rel: String,
    line: usize,
    methods: BTreeSet<(String, usize)>,
    embeds: Vec<String>,
}

#[derive(Default)]
struct Type {
    rel: String,
    line: usize,
    /// `(name, arity) → whether its receiver is a pointer`.
    methods: BTreeMap<(String, usize), bool>,
    embeds: Vec<String>,
}

fn interfaces(corpus: &Corpus) -> Option<ProjectMap> {
    let mut ifaces: BTreeMap<String, Interface> = BTreeMap::new();
    let mut types: BTreeMap<String, Type> = BTreeMap::new();
    for source in corpus.in_langs(&[Lang::Go]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        for spec in all_of(root, "type_spec") {
            let (Some(name), Some(ty)) = (
                spec.child_by_field_name("name"),
                spec.child_by_field_name("type"),
            ) else {
                continue;
            };
            let name = text(name, src).to_string();
            match ty.kind() {
                "interface_type" => {
                    let mut methods = BTreeSet::new();
                    let mut embeds = Vec::new();
                    let mut cursor = ty.walk();
                    for member in ty.named_children(&mut cursor) {
                        match member.kind() {
                            "method_elem" => {
                                if let Some(m) = member.child_by_field_name("name") {
                                    methods.insert((
                                        text(m, src).to_string(),
                                        arity(member.child_by_field_name("parameters")),
                                    ));
                                }
                            }
                            "type_elem" => embeds.push(text(member, src).trim().to_string()),
                            _ => {}
                        }
                    }
                    ifaces.insert(
                        name,
                        Interface {
                            rel: source.rel.clone(),
                            line: line(spec),
                            methods,
                            embeds,
                        },
                    );
                }
                _ => {
                    let entry = types.entry(name).or_default();
                    entry.rel = source.rel.clone();
                    entry.line = line(spec);
                    if ty.kind() == "struct_type" {
                        for field in all_of(ty, "field_declaration") {
                            if field.child_by_field_name("name").is_none()
                                && let Some(t) = field.child_by_field_name("type")
                            {
                                entry
                                    .embeds
                                    .push(text(t, src).trim_start_matches('*').to_string());
                            }
                        }
                    }
                }
            }
        }
        for method in all_of(root, "method_declaration") {
            let (Some(receiver), Some(name)) = (
                method.child_by_field_name("receiver"),
                method.child_by_field_name("name"),
            ) else {
                continue;
            };
            let Some(t) = all_of(receiver, "type_identifier").into_iter().next() else {
                continue;
            };
            let pointer = !all_of(receiver, "pointer_type").is_empty();
            types
                .entry(text(t, src).to_string())
                .or_default()
                .methods
                .insert(
                    (
                        text(name, src).to_string(),
                        arity(method.child_by_field_name("parameters")),
                    ),
                    pointer,
                );
        }
    }
    // An interface's whole method set: its embeds' too; `None` when it
    // embeds one Clew cannot see.
    fn method_set(
        name: &str,
        ifaces: &BTreeMap<String, Interface>,
        depth: usize,
    ) -> Option<BTreeSet<(String, usize)>> {
        if depth > 16 {
            return None;
        }
        if let Some((_, methods)) = STD.iter().find(|(n, _)| *n == name) {
            return Some(methods.iter().map(|(m, a)| (m.to_string(), *a)).collect());
        }
        let iface = ifaces.get(name)?;
        let mut out = iface.methods.clone();
        for embed in &iface.embeds {
            out.extend(method_set(embed, ifaces, depth + 1)?);
        }
        Some(out)
    }
    // A type's methods: its own, then those promoted from what it embeds.
    fn methods_of(
        name: &str,
        types: &BTreeMap<String, Type>,
        depth: usize,
    ) -> BTreeMap<(String, usize), bool> {
        let Some(t) = types.get(name) else {
            return BTreeMap::new();
        };
        let mut out = t.methods.clone();
        if depth < 8 {
            for embed in &t.embeds {
                for (m, p) in methods_of(embed, types, depth + 1) {
                    out.entry(m).or_insert(p);
                }
            }
        }
        out
    }
    let mut map = MapBuilder::new(
        "go.interfaces",
        "Interface satisfaction",
        "each type and the interfaces whose methods it has — Go never says so in the code; matched by method name and parameter count, embedded interfaces and promoted methods included",
    );
    let mut all: Vec<Satisfiable> = ifaces
        .iter()
        .filter_map(|(n, i)| {
            method_set(n, &ifaces, 0).map(|m| (n.clone(), m, Some((i.rel.clone(), i.line))))
        })
        .collect();
    all.extend(STD.iter().map(|(n, m)| {
        (
            n.to_string(),
            m.iter().map(|(a, b)| (a.to_string(), *b)).collect(),
            None,
        )
    }));
    for (tname, t) in &types {
        let methods = methods_of(tname, &types, 0);
        if methods.is_empty() {
            continue;
        }
        for (iname, set, place) in &all {
            if set.is_empty() || !set.iter().all(|m| methods.contains_key(m)) {
                continue;
            }
            let pointer = set.iter().any(|m| methods.get(m) == Some(&true));
            let from = map.node("type", tname, tname);
            map.place(from, &t.rel, t.line);
            let kind = if place.is_some() {
                "interface"
            } else {
                "std interface"
            };
            let to = map.node(kind, iname, iname);
            if let Some((rel, line)) = place {
                map.place(to, rel, *line);
                map.detail(
                    to,
                    set.iter()
                        .map(|(m, a)| format!("{m}({a})"))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            map.edge(
                from,
                to,
                if pointer {
                    "satisfies (as *T)"
                } else {
                    "satisfies"
                },
            );
        }
    }
    map.finish()
}

// -- goroutines and channels --------------------------------------------------------------

fn goroutines(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "go.goroutines",
        "Goroutines and channels",
        "an arrow from a function to each goroutine it starts (go), and to each channel it sends on or closes; from a channel to each function that receives from it",
    );
    let defs = corpus.defs();
    for source in corpus.in_langs(&[Lang::Go]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let pkg = dir_of(&source.rel).to_string();
        let func_of = |n: Node<'_>| -> Option<(String, usize)> {
            let f = ancestor(n, &["function_declaration", "method_declaration"])?;
            Some((
                text(f.child_by_field_name("name")?, src).to_string(),
                line(f),
            ))
        };
        let touch = |map: &mut MapBuilder, n: Node<'_>| -> Option<usize> {
            let (name, at) = func_of(n)?;
            let f = map.node("function", &format!("{}:{at}", source.rel), &name);
            map.place(f, &source.rel, at);
            f
        };
        let channel = |map: &mut MapBuilder, expr: &str, at: usize| -> Option<usize> {
            let c = map.node("channel", &format!("{pkg}:{expr}"), expr);
            map.place(c, &source.rel, at);
            c
        };
        let root = tree.root_node();
        // `ch := make(chan T)`: the channel's type.
        for call in all_of(root, "call_expression") {
            let callee = call
                .child_by_field_name("function")
                .map(|f| text(f, src))
                .unwrap_or("");
            let args = call.child_by_field_name("arguments");
            match callee {
                "make" => {
                    let Some(first) = args
                        .and_then(|a| a.named_child(0))
                        .filter(|t| t.kind() == "channel_type")
                    else {
                        continue;
                    };
                    let Some(target) = ancestor(
                        call,
                        &["short_var_declaration", "assignment_statement", "var_spec"],
                    )
                    .and_then(|d| {
                        d.child_by_field_name("left")
                            .or_else(|| d.child_by_field_name("name"))
                    }) else {
                        continue;
                    };
                    let name = text(target, src)
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let c = channel(&mut map, &name, line(call));
                    map.detail(
                        c,
                        format!("{} made at line {}", text(first, src), line(call)),
                    );
                }
                "close" => {
                    if let Some(ch) = args.and_then(|a| a.named_child(0)) {
                        let f = touch(&mut map, call);
                        let c = channel(&mut map, text(ch, src), line(call));
                        map.edge(f, c, "closes");
                    }
                }
                _ => {}
            }
        }
        for statement in all_of(root, "go_statement") {
            let Some(call) = statement.named_child(0) else {
                continue;
            };
            let f = touch(&mut map, statement);
            let target = match call.child_by_field_name("function") {
                Some(func) if func.kind() == "func_literal" => {
                    let at = map.node(
                        "goroutine",
                        &format!("{}:{}", source.rel, line(statement)),
                        "func literal",
                    );
                    map.place(at, &source.rel, line(statement));
                    at
                }
                Some(func) => {
                    let name = super::last_name(text(func, src));
                    match defs
                        .find(&name, None, &source.rel)
                        .filter(|d| d.rel.ends_with(".go"))
                    {
                        Some(d) => {
                            let at =
                                map.node("function", &format!("{}:{}", d.rel, d.line), &d.name);
                            map.place(at, &d.rel.clone(), d.line);
                            at
                        }
                        None => map.node("function", &name, &name),
                    }
                }
                None => continue,
            };
            map.edge(f, target, "go");
        }
        for send in all_of(root, "send_statement") {
            let Some(ch) = send.child_by_field_name("channel") else {
                continue;
            };
            let f = touch(&mut map, send);
            let c = channel(&mut map, text(ch, src), line(send));
            map.edge(f, c, "sends");
        }
        for unary in all_of(root, "unary_expression") {
            if !text(unary, src).starts_with("<-") {
                continue;
            }
            let Some(ch) = unary.child_by_field_name("operand") else {
                continue;
            };
            let f = touch(&mut map, unary);
            let c = channel(&mut map, text(ch, src), line(unary));
            map.edge(c, f, "received by");
        }
    }
    map.finish()
}

// -- packages ------------------------------------------------------------------------------

fn packages(corpus: &Corpus) -> Option<ProjectMap> {
    let module = corpus
        .files
        .iter()
        .find(|s| s.rel == "go.mod" || s.rel.ends_with("/go.mod"))
        .and_then(|s| {
            s.text
                .lines()
                .find_map(|l| {
                    l.trim()
                        .strip_prefix("module ")
                        .map(|m| m.trim().trim_matches('"').to_string())
                })
                .map(|m| (dir_of(&s.rel).to_string(), m))
        });
    let (base, module) = module?;
    let mut imports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut files: HashMap<String, (String, usize)> = HashMap::new();
    for source in corpus.in_langs(&[Lang::Go]) {
        let pkg = dir_of(&source.rel).to_string();
        let entry = files.entry(pkg.clone()).or_insert((source.rel.clone(), 0));
        entry.1 += 1;
        imports.entry(pkg.clone()).or_default();
        let Some(tree) = source.tree() else {
            continue;
        };
        for spec in all_of(tree.root_node(), "import_spec") {
            let Some(path) = spec
                .child_by_field_name("path")
                .and_then(|p| unquote(text(p, &source.text)))
            else {
                continue;
            };
            let Some(rest) = path.strip_prefix(&module) else {
                continue;
            };
            let target = format!("{base}{}", rest)
                .trim_start_matches('/')
                .to_string();
            if target != pkg {
                imports.entry(pkg.clone()).or_default().insert(target);
            }
        }
    }
    if imports.len() < 2 {
        return None;
    }
    // The strongly connected components: an import cycle each that has more
    // than one package.
    let names: Vec<String> = imports.keys().cloned().collect();
    let index: HashMap<&str, usize> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    let edges: Vec<Vec<usize>> = names
        .iter()
        .map(|n| {
            imports[n]
                .iter()
                .filter_map(|t| index.get(t.as_str()).copied())
                .collect()
        })
        .collect();
    let components = tarjan(&edges);
    let mut component_of = vec![0usize; names.len()];
    for (c, members) in components.iter().enumerate() {
        for &m in members {
            component_of[m] = c;
        }
    }
    // The layer: 0 for what imports none of the others, else one above the
    // highest it imports (a cycle's members share one).
    let mut layer: Vec<Option<usize>> = vec![None; components.len()];
    fn layer_of(
        c: usize,
        components: &[Vec<usize>],
        edges: &[Vec<usize>],
        component_of: &[usize],
        layer: &mut Vec<Option<usize>>,
    ) -> usize {
        if let Some(l) = layer[c] {
            return l;
        }
        layer[c] = Some(0);
        let mut best = 0;
        for &m in &components[c] {
            for &t in &edges[m] {
                let tc = component_of[t];
                if tc != c {
                    best = best.max(layer_of(tc, components, edges, component_of, layer) + 1);
                }
            }
        }
        layer[c] = Some(best);
        best
    }
    let mut map = MapBuilder::new(
        "go.packages",
        "Go packages",
        "each of the project's packages and an arrow to each of the others it imports; the detail gives its layer (0 imports none of them) and any import cycle it is in",
    );
    for (i, name) in names.iter().enumerate() {
        let label = if name.is_empty() {
            module.clone()
        } else {
            format!(
                "{module}/{}",
                name.strip_prefix(&base)
                    .unwrap_or(name)
                    .trim_start_matches('/')
            )
        };
        let at = map.node("package", name, &label);
        if let Some((rel, count)) = files.get(name) {
            map.place(at, rel, 1);
            let c = component_of[i];
            let l = layer_of(c, &components, &edges, &component_of, &mut layer);
            let mut detail = format!("{count} files · layer {l}");
            if components[c].len() > 1 {
                let others: Vec<&str> = components[c]
                    .iter()
                    .filter(|&&m| m != i)
                    .map(|&m| names[m].as_str())
                    .collect();
                detail.push_str(&format!("\nin an import cycle with {}", others.join(", ")));
            }
            map.detail(at, detail);
        }
    }
    for (i, targets) in edges.iter().enumerate() {
        for &t in targets {
            let from = map.node("package", &names[i], &names[i]);
            let to = map.node("package", &names[t], &names[t]);
            let label = if component_of[i] == component_of[t] {
                "imports (cycle)"
            } else {
                "imports"
            };
            map.edge(from, to, label);
        }
    }
    map.finish()
}

/// Tarjan's strongly connected components of a graph given as adjacency
/// lists (iterative, so a deep graph cannot overflow the stack).
pub(crate) fn tarjan(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = edges.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut out = Vec::new();
    let mut next = 0;
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        let mut work: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&mut (v, ref mut i)) = work.last_mut() {
            if *i == 0 {
                index[v] = next;
                low[v] = next;
                next += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if *i < edges[v].len() {
                let w = edges[v][*i];
                *i += 1;
                if index[w] == usize::MAX {
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let mut component = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                out.push(component);
            }
        }
    }
    out
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
        maps.iter().find(|m| m.id == id).unwrap_or_else(|| {
            panic!(
                "{id} in {:?}",
                maps.iter().map(|m| &m.id).collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn types_satisfy_interfaces_without_saying_so() {
        let maps = maps(&Corpus::of(&[
            (
                "store/store.go",
                "package store\n\ntype Getter interface {\n\tGet(key string) ([]byte, error)\n}\n\ntype Store interface {\n\tGetter\n\tPut(key string, value []byte) error\n\tio.Closer\n}\n",
            ),
            (
                "store/mem.go",
                "package store\n\ntype Mem struct{ base }\n\nfunc (m *Mem) Put(k string, v []byte) error { return nil }\nfunc (m *Mem) Close() error { return nil }\n\ntype base struct{}\n\nfunc (b base) Get(k string) ([]byte, error) { return nil, nil }\nfunc (b base) String() string { return \"\" }\n\ntype Half struct{}\n\nfunc (h Half) Put(a, b string) error { return nil }\n",
            ),
        ]));
        let ifaces = find(&maps, "go.interfaces");
        let got = edges(ifaces);
        for want in [
            e("Mem", "Store", "satisfies (as *T)"),
            e("Mem", "Getter", "satisfies"),
            e("Mem", "io.Closer", "satisfies (as *T)"),
            e("base", "Getter", "satisfies"),
            e("base", "fmt.Stringer", "satisfies"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(a, b, _)| a == "Half" && b == "Store"),
            "Put takes two, Get missing: {got:?}"
        );
        assert!(
            !got.iter().any(|(a, b, _)| a == "base" && b == "Store"),
            "{got:?}"
        );
        let store = ifaces.nodes.iter().find(|n| n.label == "Store").unwrap();
        assert_eq!(store.line, 7);
        assert_eq!(store.detail, "Close(0), Get(1), Put(2)");
    }

    #[test]
    fn goroutines_and_the_channels_they_share() {
        let maps = maps(&Corpus::of(&[(
            "worker/pool.go",
            "package worker\n\nfunc Run() {\n\tjobs := make(chan Job, 8)\n\tgo work(jobs)\n\tgo func() { jobs <- Job{} }()\n\tjobs <- Job{}\n\tclose(jobs)\n}\n\nfunc work(jobs chan Job) {\n\tfor {\n\t\tj := <-jobs\n\t\t_ = j\n\t}\n}\n",
        )]));
        let g = find(&maps, "go.goroutines");
        let got = edges(g);
        for want in [
            e("Run", "work", "go"),
            e("Run", "func literal", "go"),
            e("Run", "jobs", "sends"),
            e("Run", "jobs", "closes"),
            e("jobs", "work", "received by"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let jobs = g.nodes.iter().find(|n| n.label == "jobs").unwrap();
        assert_eq!(jobs.detail, "chan Job made at line 4");
        assert!(
            got.iter()
                .filter(|(a, b, l)| a == "Run" && b == "jobs" && l == "sends")
                .count()
                == 1
        );
    }

    #[test]
    fn packages_layers_and_cycles() {
        let maps = maps(&Corpus::of(&[
            ("go.mod", "module example.com/shop\n\ngo 1.22\n"),
            (
                "main.go",
                "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/shop/orders\"\n)\n",
            ),
            (
                "orders/orders.go",
                "package orders\n\nimport \"example.com/shop/billing\"\n",
            ),
            (
                "billing/billing.go",
                "package billing\n\nimport \"example.com/shop/orders\"\n",
            ),
            ("util/util.go", "package util\n"),
        ]));
        let p = find(&maps, "go.packages");
        let got = edges(p);
        assert!(
            got.contains(&e("example.com/shop", "example.com/shop/orders", "imports")),
            "{got:?}"
        );
        assert!(
            got.contains(&e(
                "example.com/shop/orders",
                "example.com/shop/billing",
                "imports (cycle)"
            )),
            "{got:?}"
        );
        let detail = |rel: &str| {
            p.nodes
                .iter()
                .find(|n| n.file == rel)
                .unwrap()
                .detail
                .clone()
        };
        assert!(
            detail("orders/orders.go").contains("in an import cycle with billing"),
            "{}",
            detail("orders/orders.go")
        );
        assert!(detail("util/util.go").contains("layer 0"));
        assert!(
            detail("main.go").contains("layer 1"),
            "{}",
            detail("main.go")
        );
        assert_eq!(
            p.nodes.iter().find(|n| n.file == "main.go").unwrap().label,
            "example.com/shop"
        );
        assert_eq!(tarjan(&[vec![1], vec![0], vec![]]).len(), 2);
    }
}
