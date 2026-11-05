//! Zig's maps:
//!
//! - **comptime generics** — each function with `comptime` parameters (a
//!   generic type constructor like `fn List(comptime T: type) type`, or a
//!   generic function) and each instantiation the project makes of it —
//!   `List(u8)`, `max(i32)` — where it is first made;
//! - **error sets** — each named error set and its errors, the sets it is
//!   merged from (`A || B`); each function and the set its return type
//!   names, the errors it returns (`return error.X`), the functions whose
//!   errors it passes on (`try f()`) and the errors it handles (`catch`);
//! - **build graph** — what `build.zig` builds: each executable, library,
//!   test and module, its root source file, the libraries it links, the
//!   modules it imports, and the steps (`zig build run`) that run it.

use std::collections::HashMap;

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, Source, all_of, ancestor, line, text, unquote};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let sources: Vec<(&Source, tree_sitter::Tree)> = corpus
        .in_langs(&[Lang::Zig])
        .filter_map(|s| Some((s, s.tree()?)))
        .collect();
    let mut out = Vec::new();
    out.extend(generics(&sources));
    out.extend(errors(&sources));
    out.extend(build(corpus, &sources));
    out
}

fn name_of<'s>(node: Node<'_>, src: &'s str) -> &'s str {
    node.child_by_field_name("name")
        .map(|n| text(n, src))
        .unwrap_or("")
}

/// The arguments of a call: its named children after the callee.
fn args<'t>(call: Node<'t>) -> Vec<Node<'t>> {
    let callee = call.child_by_field_name("function");
    let mut c = call.walk();
    call.named_children(&mut c)
        .filter(|n| Some(*n) != callee)
        .collect()
}

/// The last name a callee names: `List` for `List`, `std.ArrayList` too.
fn callee_name<'s>(call: Node<'_>, src: &'s str) -> Option<&'s str> {
    let f = call.child_by_field_name("function")?;
    match f.kind() {
        "identifier" => Some(text(f, src)),
        "field_expression" => f.child_by_field_name("member").map(|m| text(m, src)),
        _ => None,
    }
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// -- comptime generics ------------------------------------------------------------------------

struct Generic {
    rel: String,
    line: usize,
    /// The positions of its `comptime` parameters.
    comptime: Vec<usize>,
    params: String,
    returns_type: bool,
}

fn generics(sources: &[(&Source, tree_sitter::Tree)]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "zig.generics",
        "Comptime generics",
        "each function with comptime parameters — a generic type constructor or a generic function — and an arrow to each instantiation the project makes of it, where it is first made",
    );
    let mut found: HashMap<String, Generic> = HashMap::new();
    for (source, tree) in sources {
        let src = source.text.as_str();
        for f in all_of(tree.root_node(), "function_declaration") {
            let Some(params) = f
                .named_children(&mut f.walk())
                .find(|c| c.kind() == "parameters")
            else {
                continue;
            };
            let list: Vec<Node> = params
                .named_children(&mut params.walk())
                .filter(|p| p.kind() == "parameter")
                .collect();
            let comptime: Vec<usize> = list
                .iter()
                .enumerate()
                .filter(|(_, p)| text(**p, src).trim_start().starts_with("comptime"))
                .map(|(i, _)| i)
                .collect();
            if comptime.is_empty() {
                continue;
            }
            let returns_type = f
                .child_by_field_name("type")
                .is_some_and(|t| text(t, src) == "type");
            let params = comptime
                .iter()
                .map(|i| squash(text(list[*i], src).trim_start_matches("comptime").trim()))
                .collect::<Vec<_>>()
                .join(", ");
            found.entry(name_of(f, src).to_string()).or_insert(Generic {
                rel: source.rel.clone(),
                line: line(f),
                comptime,
                params,
                returns_type,
            });
        }
    }
    for (source, tree) in sources {
        let src = source.text.as_str();
        for call in all_of(tree.root_node(), "call_expression") {
            let Some(name) = callee_name(call, src) else {
                continue;
            };
            let Some(generic) = found.get(name) else {
                continue;
            };
            let args = args(call);
            let picked: Vec<String> = generic
                .comptime
                .iter()
                .filter_map(|i| args.get(*i).map(|a| squash(text(*a, src))))
                .collect();
            if picked.len() != generic.comptime.len() {
                continue;
            }
            let g = map.node("generic", name, name);
            map.place(g, &generic.rel, generic.line);
            let what = if generic.returns_type {
                "type"
            } else {
                "function"
            };
            map.detail(g, format!("generic {what} · comptime {}", generic.params));
            let label = format!("{name}({})", picked.join(", "));
            let at = map.node("instance", &label, &label);
            map.place(at, &source.rel, line(call));
            map.edge(g, at, "instance");
        }
    }
    map.finish()
}

// -- error sets -------------------------------------------------------------------------------

fn errors(sources: &[(&Source, tree_sitter::Tree)]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "zig.errors",
        "Error sets",
        "each error set and the errors in it; each function and the set its return type names, the errors it returns, the functions whose errors try passes on, and the errors it catches",
    );
    // The named sets first, so a function's return type finds its set.
    let mut sets: HashMap<String, (String, usize)> = HashMap::new();
    for (source, tree) in sources {
        let src = source.text.as_str();
        for decl in all_of(tree.root_node(), "variable_declaration") {
            let Some(name) = decl.named_child(0).filter(|n| n.kind() == "identifier") else {
                continue;
            };
            let Some(value) = decl.named_child(1) else {
                continue;
            };
            // `error{…}`, or `A || B` — in Zig, `||` only merges error sets.
            let merge = |n: Node<'_>| {
                n.kind() == "binary_expression"
                    && (0..n.child_count())
                        .any(|i| n.child(i as u32).is_some_and(|c| c.kind() == "||"))
            };
            let is_set = value.kind() == "error_set_declaration" || merge(value);
            if !is_set {
                continue;
            }
            let set = text(name, src).to_string();
            sets.insert(set.clone(), (source.rel.clone(), line(decl)));
            let at = map.node("set", &set, &set);
            map.place(at, &source.rel, line(decl));
            for literal in std::iter::once(value).chain(all_of(value, "error_set_declaration")) {
                if literal.kind() != "error_set_declaration" {
                    continue;
                }
                for member in literal
                    .named_children(&mut literal.walk())
                    .filter(|m| m.kind() == "identifier")
                {
                    let e = format!("error.{}", text(member, src));
                    let to = map.node("error", &e, &e);
                    map.place(to, &source.rel, line(member));
                    map.edge(at, to, "contains");
                }
            }
            if value.kind() == "binary_expression" {
                for side in ["left", "right"] {
                    let mut n = value.child_by_field_name(side);
                    // `A || B || C` nests to the left.
                    while let Some(x) = n {
                        match x.kind() {
                            "identifier" => {
                                let other = text(x, src).to_string();
                                let to = map.node("set", &other, &other);
                                map.edge(at, to, "includes");
                                n = None;
                            }
                            "binary_expression" => {
                                for inner in ["left", "right"] {
                                    if let Some(i) = x
                                        .child_by_field_name(inner)
                                        .filter(|i| i.kind() == "identifier")
                                    {
                                        let other = text(i, src).to_string();
                                        let to = map.node("set", &other, &other);
                                        map.edge(at, to, "includes");
                                    }
                                }
                                n = None;
                            }
                            _ => n = None,
                        }
                    }
                }
            }
        }
    }
    // Which functions can fail, so `try f()` links only to those.
    let mut failing: HashMap<String, (String, usize)> = HashMap::new();
    for (source, tree) in sources {
        let src = source.text.as_str();
        for f in all_of(tree.root_node(), "function_declaration") {
            if f.child_by_field_name("type")
                .is_some_and(|t| t.kind() == "error_union_type")
            {
                failing
                    .entry(name_of(f, src).to_string())
                    .or_insert((source.rel.clone(), line(f)));
            }
        }
    }
    for (source, tree) in sources {
        let src = source.text.as_str();
        for f in all_of(tree.root_node(), "function_declaration") {
            let name = name_of(f, src).to_string();
            let fn_node = |map: &mut MapBuilder| {
                let at = map.node("function", &format!("fn {name}"), &format!("{name}()"));
                map.place(at, &source.rel, line(f));
                at
            };
            if let Some(ty) = f
                .child_by_field_name("type")
                .filter(|t| t.kind() == "error_union_type")
            {
                let me = fn_node(&mut map);
                match ty.child_by_field_name("error") {
                    Some(set) if set.kind() == "identifier" => {
                        let s = text(set, src).to_string();
                        let to = map.node("set", &s, &s);
                        if let Some((rel, at)) = sets.get(&s) {
                            map.place(to, rel, *at);
                        }
                        map.edge(me, to, "returns");
                    }
                    Some(set) if set.kind() == "error_set_declaration" => {
                        for member in set
                            .named_children(&mut set.walk())
                            .filter(|m| m.kind() == "identifier")
                        {
                            let e = format!("error.{}", text(member, src));
                            let to = map.node("error", &e, &e);
                            map.edge(me, to, "returns");
                        }
                    }
                    Some(_) => {}
                    None => map.detail(me, "inferred error set"),
                }
            }
            let Some(body) = f.child_by_field_name("body") else {
                continue;
            };
            for e in all_of(body, "error_type") {
                // The innermost function it is in is this one.
                if ancestor(e, &["function_declaration"]) != Some(f) {
                    continue;
                }
                let Some(member) = e.named_child(0) else {
                    continue;
                };
                let label = format!("error.{}", text(member, src));
                let how = match e.parent().map(|p| p.kind()) {
                    Some("return_expression") => "returns",
                    Some("switch_case")
                        if ancestor(e, &["catch_expression", "function_declaration"])
                            .is_some_and(|c| c.kind() == "catch_expression") =>
                    {
                        "catches"
                    }
                    _ => continue,
                };
                let me = fn_node(&mut map);
                let to = map.node("error", &label, &label);
                map.edge(me, to, how);
            }
            for t in all_of(body, "try_expression") {
                let Some(callee) = all_of(t, "call_expression")
                    .into_iter()
                    .next()
                    .and_then(|c| callee_name(c, src))
                else {
                    continue;
                };
                let Some((rel, at)) = failing.get(callee) else {
                    continue;
                };
                let me = fn_node(&mut map);
                let to = map.node("function", &format!("fn {callee}"), &format!("{callee}()"));
                map.place(to, rel, *at);
                map.edge(me, to, "try");
            }
        }
    }
    map.finish()
}

// -- build graph ------------------------------------------------------------------------------

/// What `b.addX` makes, by the method's name.
fn artifact_kind(method: &str) -> Option<&'static str> {
    Some(match method {
        "addExecutable" => "executable",
        "addStaticLibrary" => "static library",
        "addSharedLibrary" => "shared library",
        "addLibrary" => "library",
        "addObject" => "object",
        "addTest" => "test",
        "addModule" | "createModule" => "module",
        "addRunArtifact" => "run",
        "addSystemCommand" => "command",
        "dependency" => "dependency",
        "addInstallArtifact" => "install",
        _ => return None,
    })
}

/// The string a `.field = "…"` initializer gives, anywhere in `init`.
fn field_string(init: Node<'_>, field: &str, src: &str) -> Option<String> {
    all_of(init, "assignment_expression")
        .into_iter()
        .find_map(|a| {
            let left = a.child_by_field_name("left")?;
            if text(left, src).trim_start_matches('.') != field {
                return None;
            }
            let right = a.child_by_field_name("right")?;
            let literal = if right.kind() == "string" {
                right
            } else {
                all_of(right, "string").into_iter().next()?
            };
            unquote(text(literal, src))
        })
}

fn build(corpus: &Corpus, sources: &[(&Source, tree_sitter::Tree)]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "zig.build",
        "Build graph",
        "what build.zig builds: each executable, library, test and module and its root source file, the libraries it links, the modules it imports, and the build steps that run it",
    );
    for (source, tree) in sources.iter().filter(|(s, _)| s.name() == "build.zig") {
        let src = source.text.as_str();
        let dir = source.rel.strip_suffix("build.zig").unwrap_or("");
        // Each `const x = …` in build(), by the name it binds.
        let mut vars: HashMap<String, Option<usize>> = HashMap::new();
        let node_for = |map: &mut MapBuilder,
                        vars: &HashMap<String, Option<usize>>,
                        n: Node<'_>|
         -> Option<usize> {
            // `exe`, `&run.step`, `dep.module("x")`, `exe.root_module`.
            let mut n = n;
            loop {
                match n.kind() {
                    "identifier" => return vars.get(text(n, src)).copied().flatten(),
                    "unary_expression" => n = n.child_by_field_name("argument")?,
                    "field_expression" => n = n.child_by_field_name("object")?,
                    "call_expression" => {
                        let f = n.child_by_field_name("function")?;
                        let object = f.child_by_field_name("object")?;
                        let owner = vars.get(text(object, src)).copied().flatten();
                        let what = callee_name(n, src)?;
                        if matches!(what, "module" | "artifact") {
                            let name = args(n).first().and_then(|a| unquote(text(*a, src)))?;
                            let dep = owner
                                .and_then(|o| map.get(o))
                                .map(|d| d.label.clone())
                                .unwrap_or_default();
                            let label = format!("{dep}:{name}");
                            let at = map.node(what, &label, &label);
                            map.place(at, &source.rel, line(n));
                            map.edge(at, owner, "from");
                            return at;
                        }
                        n = object;
                    }
                    _ => return None,
                }
            }
        };
        for decl in all_of(tree.root_node(), "variable_declaration") {
            let (Some(name), Some(value)) = (decl.named_child(0), decl.named_child(1)) else {
                continue;
            };
            if value.kind() != "call_expression" {
                continue;
            }
            let var = text(name, src).to_string();
            let Some(method) = callee_name(value, src) else {
                continue;
            };
            let a = args(value);
            if method == "step" {
                let step = a
                    .first()
                    .and_then(|s| unquote(text(*s, src)))
                    .unwrap_or_else(|| var.clone());
                let label = format!("zig build {step}");
                let at = map.node("step", &label, &label);
                map.place(at, &source.rel, line(decl));
                if let Some(d) = a.get(1).and_then(|d| unquote(text(*d, src))) {
                    map.detail(at, d);
                }
                vars.insert(var, at);
                continue;
            }
            let Some(kind) = artifact_kind(method) else {
                continue;
            };
            let init = a
                .iter()
                .copied()
                .find(|x| x.kind() == "anonymous_struct_initializer");
            let label = match kind {
                "module" if method == "addModule" => a.first().and_then(|s| unquote(text(*s, src))),
                "dependency" => a.first().and_then(|s| unquote(text(*s, src))),
                _ => init.and_then(|i| field_string(i, "name", src)),
            }
            .unwrap_or_else(|| var.clone());
            let at = match kind {
                "run" | "command" | "install" => {
                    let target = a.first().and_then(|t| node_for(&mut map, &vars, *t));
                    let what = target
                        .and_then(|t| map.get(t))
                        .map(|t| t.label.clone())
                        .unwrap_or(label.clone());
                    let shown = format!("{kind} {what}");
                    let at = map.node(kind, &format!("{}:{var}", source.rel), &shown);
                    map.place(at, &source.rel, line(decl));
                    map.edge(at, target, kind);
                    at
                }
                _ => {
                    let at = map.node(
                        if kind == "dependency" {
                            "dependency"
                        } else {
                            "artifact"
                        },
                        &format!("{}:{var}", source.rel),
                        &label,
                    );
                    map.place(at, &source.rel, line(decl));
                    map.detail(at, kind);
                    if let Some(root) = init.and_then(|i| field_string(i, "root_source_file", src))
                    {
                        let rel = format!("{dir}{}", root.trim_start_matches("./"));
                        let file = map.node("file", &rel, &rel);
                        if corpus.get(&rel).is_some() {
                            map.place(file, &rel, 1);
                        }
                        map.edge(at, file, "root");
                    }
                    if let Some(module) = init.and_then(|i| {
                        all_of(i, "assignment_expression").into_iter().find(|x| {
                            x.child_by_field_name("left")
                                .is_some_and(|l| text(l, src) == ".root_module")
                        })
                    }) && let Some(right) = module
                        .child_by_field_name("right")
                        .filter(|r| r.kind() == "identifier")
                    {
                        let to = vars.get(text(right, src)).copied().flatten();
                        map.edge(at, to, "root module");
                    }
                    at
                }
            };
            vars.insert(var, at);
        }
        for call in all_of(tree.root_node(), "call_expression") {
            let Some(method) = callee_name(call, src) else {
                continue;
            };
            let Some(object) = call
                .child_by_field_name("function")
                .and_then(|f| f.child_by_field_name("object"))
            else {
                continue;
            };
            let a = args(call);
            match method {
                "linkLibrary" | "linkLibC" | "linkSystemLibrary" | "linkSystemLibrary2"
                | "linkFramework" => {
                    let from = node_for(&mut map, &vars, object);
                    let to = match a.first() {
                        Some(x) if x.kind() == "string" => {
                            let name = unquote(text(*x, src)).unwrap_or_default();
                            let at = map.node("system", &format!("system:{name}"), &name);
                            map.detail(at, "system library");
                            at
                        }
                        Some(x) => node_for(&mut map, &vars, *x),
                        None => {
                            let at = map.node("system", "system:c", "libc");
                            map.detail(at, "system library");
                            at
                        }
                    };
                    map.edge(from, to, "links");
                }
                "addImport" | "addModule" if a.len() >= 2 => {
                    let from = node_for(&mut map, &vars, object);
                    let to = node_for(&mut map, &vars, a[1]);
                    let alias = unquote(text(a[0], src)).unwrap_or_default();
                    map.edge(from, to, &format!("imports \"{alias}\""));
                }
                "installArtifact" => {
                    let to = a.first().and_then(|x| node_for(&mut map, &vars, *x));
                    let install = map.node("step", "zig build install", "zig build install");
                    map.edge(install, to, "installs");
                }
                "dependOn" => {
                    let from = node_for(&mut map, &vars, object);
                    let to = a.first().and_then(|x| node_for(&mut map, &vars, *x));
                    map.edge(from, to, "depends on");
                }
                _ => {}
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

    #[test]
    fn generics_and_their_instantiations() {
        let maps = maps(&Corpus::of(&[
            (
                "src/list.zig",
                "pub fn List(comptime T: type) type {\n    return struct { items: []T };\n}\npub fn max(comptime T: type, a: T, b: T) T {\n    return if (a > b) a else b;\n}\nfn plain(x: u8) u8 { return x; }\n",
            ),
            (
                "src/main.zig",
                "const list = @import(\"list.zig\");\nconst Ints = list.List(i32);\npub fn main() void {\n    var bytes = list.List(u8){ .items = &.{} };\n    _ = list.max(f64, 1.0, 2.0);\n    _ = list.List(i32);\n    _ = plain(1);\n    _ = bytes;\n}\n",
            ),
        ]));
        let g = maps.iter().find(|m| m.id == "zig.generics").unwrap();
        assert_eq!(
            edges(g),
            [
                e("List", "List(i32)", "instance"),
                e("List", "List(u8)", "instance"),
                e("max", "max(f64)", "instance")
            ]
        );
        let list = g.nodes.iter().find(|n| n.label == "List").unwrap();
        assert_eq!(
            (list.file.as_str(), list.line, list.detail.as_str()),
            ("src/list.zig", 1, "generic type · comptime T: type")
        );
        let max = g.nodes.iter().find(|n| n.label == "max").unwrap();
        assert_eq!(max.detail, "generic function · comptime T: type");
        let first = g.nodes.iter().find(|n| n.label == "List(i32)").unwrap();
        assert_eq!((first.file.as_str(), first.line), ("src/main.zig", 2));
    }

    #[test]
    fn error_sets_returns_tries_and_catches() {
        let maps = maps(&Corpus::of(&[(
            "src/io.zig",
            "const FileError = error{ NotFound, Denied };\nconst IoError = FileError || error{Eof};\nfn open() FileError!u8 {\n    return error.NotFound;\n}\nfn read() !u8 {\n    const fd = try open();\n    if (fd == 0) return error.Eof;\n    return fd;\n}\nfn safe() u8 {\n    return read() catch |err| switch (err) {\n        error.Eof => 0,\n        else => 1,\n    };\n}\nfn noisy() void {}\n",
        )]));
        let errs = maps.iter().find(|m| m.id == "zig.errors").unwrap();
        let got = edges(errs);
        for want in [
            e("FileError", "error.NotFound", "contains"),
            e("FileError", "error.Denied", "contains"),
            e("IoError", "error.Eof", "contains"),
            e("IoError", "FileError", "includes"),
            e("open()", "FileError", "returns"),
            e("open()", "error.NotFound", "returns"),
            e("read()", "open()", "try"),
            e("read()", "error.Eof", "returns"),
            e("safe()", "error.Eof", "catches"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(a, b, _)| a == "noisy()" || b == "noisy()"),
            "{got:?}"
        );
        let read = errs.nodes.iter().find(|n| n.label == "read()").unwrap();
        assert_eq!((read.line, read.detail.as_str()), (6, "inferred error set"));
    }

    #[test]
    fn the_build_graph() {
        let maps = maps(&Corpus::of(&[
            (
                "build.zig",
                "const std = @import(\"std\");\npub fn build(b: *std.Build) void {\n    const util = b.addModule(\"util\", .{ .root_source_file = b.path(\"src/util.zig\") });\n    const core = b.addStaticLibrary(.{ .name = \"core\", .root_source_file = b.path(\"src/core.zig\") });\n    const exe = b.addExecutable(.{ .name = \"app\", .root_source_file = b.path(\"src/main.zig\") });\n    const zap = b.dependency(\"zap\", .{});\n    exe.linkLibrary(core);\n    exe.linkLibC();\n    exe.root_module.addImport(\"util\", util);\n    exe.root_module.addImport(\"zap\", zap.module(\"zap\"));\n    b.installArtifact(exe);\n    const run = b.addRunArtifact(exe);\n    const run_step = b.step(\"run\", \"Run the app\");\n    run_step.dependOn(&run.step);\n    const tests = b.addTest(.{ .root_source_file = b.path(\"src/main.zig\") });\n    const test_step = b.step(\"test\", \"Run tests\");\n    test_step.dependOn(&b.addRunArtifact(tests).step);\n}\n",
            ),
            ("src/main.zig", "pub fn main() void {}\n"),
            ("src/core.zig", "pub fn f() void {}\n"),
        ]));
        let b = maps.iter().find(|m| m.id == "zig.build").unwrap();
        let got = edges(b);
        for want in [
            e("util", "src/util.zig", "root"),
            e("core", "src/core.zig", "root"),
            e("app", "src/main.zig", "root"),
            e("app", "core", "links"),
            e("app", "libc", "links"),
            e("app", "util", "imports \"util\""),
            e("app", "zap:zap", "imports \"zap\""),
            e("zap:zap", "zap", "from"),
            e("zig build install", "app", "installs"),
            e("run app", "app", "run"),
            e("zig build run", "run app", "depends on"),
            e("tests", "src/main.zig", "root"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let app = b.nodes.iter().find(|n| n.label == "app").unwrap();
        assert_eq!(
            (app.file.as_str(), app.line, app.detail.as_str()),
            ("build.zig", 5, "executable")
        );
        let main = b.nodes.iter().find(|n| n.label == "src/main.zig").unwrap();
        assert_eq!((main.file.as_str(), main.line), ("src/main.zig", 1));
        let util = b.nodes.iter().find(|n| n.label == "src/util.zig").unwrap();
        assert_eq!(util.file, "", "not in the project");
        let run = b.nodes.iter().find(|n| n.label == "zig build run").unwrap();
        assert_eq!(run.detail, "Run the app");
    }
}
