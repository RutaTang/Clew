//! Reading a real project whole (`CLEW_TEST_PROJECT=path`), for a reader to
//! judge: per language, how its imports resolve and how its calls link to
//! the project's own definitions, with samples of what did not.

use super::*;
use std::collections::{BTreeMap, HashMap};

#[derive(Default)]
struct Tally {
    files: usize,
    symbols: usize,
    internal: usize,
    external: usize,
    unresolved: usize,
    unresolved_samples: Vec<String>,
    externals: HashMap<String, usize>,
    nodes: usize,
    sites: usize,
    project_named: usize,
    linked: usize,
    missed: HashMap<String, Vec<String>>,
    /// Unlinked calls by their shape, and by callee name.
    shapes: HashMap<&'static str, usize>,
    missed_names: HashMap<String, usize>,
    /// Linked calls, each with the definitions it reached, for a reader to
    /// judge (`CLEW_TEST_SAMPLE=n`).
    linked_samples: Vec<String>,
}

/// The project at `CLEW_TEST_PROJECT`, read as Clew reads one: its files
/// scanned and indexed, its imports resolved, its call graph built — and per
/// language what came of it. `CLEW_TEST_LANG` keeps one language;
/// `CLEW_TEST_DUMP` prints every call site and what it reached (`SITE
/// file:line name -> targets`), for two builds' readings to be compared.
#[test]
#[ignore = "reads a project (CLEW_TEST_PROJECT=path); run explicitly"]
fn live_a_projects_reading() {
    let root = PathBuf::from(
        std::env::var("CLEW_TEST_PROJECT").expect("CLEW_TEST_PROJECT names a folder"),
    )
    .canonicalize()
    .unwrap();
    let only = std::env::var("CLEW_TEST_LANG").unwrap_or_default();
    let mut app = blank_app();
    let started = std::time::Instant::now();
    scan_synchronously(&mut app, root.clone());
    let files = app.proj.project.as_ref().unwrap().files.clone();
    let indexed = app.update(Message::Watch(WatchMsg::SymbolIndexDone {
        stamp: app.stamp(),
        indexed: index::build_indexed(&root, files.clone()),
    }));
    settle_imports(&mut app, indexed);
    println!("{} files indexed in {:?}", files.len(), started.elapsed());
    let started = std::time::Instant::now();
    for msg in run_task(app.update(Message::Graph(GraphMsg::OpenOverlay(Overlay::ProjectCalls)))) {
        if matches!(msg, Message::Graph(GraphMsg::ProjectCallsBuilt { .. })) {
            let _ = app.update(msg);
        }
    }
    println!("call graph built in {:?}", started.elapsed());
    let calls = &app.proj.project_calls.graph;
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    // What a call can reach: a test's title is no function's name.
    let mut callable_by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for id in 0..calls.node_count() {
        let node = calls.node(id);
        by_name.entry(node.name.as_str()).or_default().push(id);
        if !clew_core::projectcalls::calls_only(&node.kind) {
            callable_by_name
                .entry(node.name.as_str())
                .or_default()
                .push(id);
        }
    }
    // `CLEW_TEST_SITE=rel:line`: that line's calls, and what they could reach.
    if let Ok(site) = std::env::var("CLEW_TEST_SITE")
        && let Some((rel_path, line)) = site.rsplit_once(':')
    {
        let file = root.join(rel_path);
        let line: usize = line.parse().unwrap();
        let scope = app.proj.import_graph.scope_map();
        let in_scope = scope.get(&file).cloned().unwrap_or_default();
        println!("imports of {rel_path}:");
        for e in app.proj.import_graph.imports(&file) {
            println!("  {:?} {} (line {})", e.target, e.specifier, e.line);
        }
        let mut scoped: Vec<String> = in_scope
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap_or(p).display().to_string())
            .collect();
        scoped.sort();
        println!("scope ({}): {scoped:?}", scoped.len());
        if let Some(fc) = app.proj.calls_by_file.get(&file) {
            for c in fc.calls.iter().filter(|c| c.line == line) {
                println!("call {c:?}");
                for id in 0..calls.node_count() {
                    let n = calls.node(id);
                    if n.name == c.callee {
                        let owner = app
                            .proj
                            .calls_by_file
                            .get(&n.file)
                            .and_then(|f| f.owners.get(&n.line));
                        println!(
                            "  candidate {}:{} {} owner {owner:?} in scope: {}",
                            n.file.strip_prefix(&root).unwrap_or(&n.file).display(),
                            n.line,
                            n.kind,
                            in_scope.contains(&n.file) || n.file == file
                        );
                    }
                }
                if let Some(from) = c.caller.as_deref().and_then(|c| calls.id_of(&file, c)) {
                    let callees: Vec<String> = calls
                        .callees_of(from)
                        .iter()
                        .map(|&id| calls.node(id).name.clone())
                        .collect();
                    println!("  caller's callees: {callees:?}");
                }
            }
        }
    }
    // The types the project declares: a call on a value declared as any
    // other type is a library's.
    let mut project_types: std::collections::HashSet<String> = Default::default();
    // Each class's fields' types: where a chain of fields (`↦Class|f`) ends,
    // a library's type (`std::vector`) makes the call the library's.
    let mut field_types: HashMap<(String, String), String> = HashMap::new();
    for fc in files
        .iter()
        .filter_map(|f| app.proj.calls_by_file.get(&f.abs))
    {
        project_types.extend(fc.context.types.iter().map(|(t, _)| t.clone()));
        project_types.extend(fc.owners.values().cloned());
        for (ty, field, of) in &fc.context.fields {
            field_types
                .entry((ty.clone(), field.clone()))
                .or_insert_with(|| of.clone());
        }
    }
    // Whether a chain of fields ends in a library's value, as the graph
    // reads it: a C++ `std::` type, a Zig `std.` one, a Go package's
    // (`http.Request`) — every field after it the library's too.
    let library_chain = |chain: &str| -> bool {
        let library = |t: &str| t.starts_with("std::") || t.starts_with("std.") || t.contains('.');
        let mut parts = chain.split('|');
        // (`.`: the file's own package, `@file`: a Zig file's variable.)
        let Some(root) = parts.next().filter(|r| *r != "." && !r.starts_with('@')) else {
            return false;
        };
        let mut ty = root.to_string();
        for field in parts {
            if library(&ty) {
                return true;
            }
            let last = ty.rsplit(['.', ':']).next().unwrap_or(&ty).to_string();
            match field_types.get(&(last, field.to_string())) {
                Some(next) => ty = next.clone(),
                None => return false,
            }
        }
        library(&ty)
    };
    let mut library_calls = 0usize;
    let mut tallies: BTreeMap<&'static str, Tally> = BTreeMap::new();
    let rel = |p: &Path| p.strip_prefix(&root).unwrap_or(p).display().to_string();
    for file in files.iter() {
        let Some(lang) = highlight::detect(&file.abs) else {
            continue;
        };
        if !only.is_empty() && lang != only {
            continue;
        }
        let t = tallies.entry(lang).or_default();
        t.files += 1;
        t.symbols += app
            .proj
            .symbol_index_by_file
            .get(&file.abs)
            .map_or(0, |s| s.len());
        for edge in app.proj.import_graph.imports(&file.abs) {
            match &edge.target {
                imports::Target::Internal(_) => t.internal += 1,
                imports::Target::External(name) => {
                    t.external += 1;
                    *t.externals.entry(name.clone()).or_default() += 1;
                }
                imports::Target::Unresolved(spec) => {
                    t.unresolved += 1;
                    if t.unresolved_samples.len() < 25 {
                        t.unresolved_samples.push(format!(
                            "{}:{} {spec}",
                            rel(&file.abs),
                            edge.line
                        ));
                    }
                }
            }
        }
        let Some(file_calls) = app.proj.calls_by_file.get(&file.abs) else {
            continue;
        };
        for site in file_calls.calls.iter() {
            t.sites += 1;
            // The language's own, as the graph takes them.
            if clew_core::projectcalls::is_library_call(file_calls.lang, site) {
                library_calls += 1;
                continue;
            }
            // Only a definition in a language the call can reach counts: a
            // Kotlin call reaches Java and Scala, C reaches C++, and none
            // reaches a minified script's `a`.
            let family = |key: &str| match key {
                "java" | "kotlin" | "scala" => 1,
                "c" | "cpp" => 2,
                "javascript" | "typescript" | "tsx" => 3,
                _ => 0,
            };
            let own = file_calls.lang.key();
            let reaches = |id: &usize| {
                highlight::detect(&calls.node(*id).file).is_some_and(|key| {
                    key == own || (family(key) != 0 && family(key) == family(own))
                })
            };
            let defined: Vec<usize> = callable_by_name
                .get(site.callee.as_str())
                .into_iter()
                .flatten()
                .copied()
                .filter(reaches)
                .collect();
            if defined.is_empty() {
                continue;
            }
            if let Some(
                clew_core::projectcalls::Receiver::Declared(ty)
                | clew_core::projectcalls::Receiver::Type(ty),
            ) = &site.receiver
                && !ty.starts_with(clew_core::projectcalls::RETURNED)
                && !ty.starts_with(clew_core::projectcalls::THROUGH)
                && !project_types.contains(ty.rsplit(['.', ':']).next().unwrap_or(ty))
            {
                library_calls += 1;
                continue;
            }
            // A chain of fields ending in a library's type.
            if let Some(clew_core::projectcalls::Receiver::Type(ty)) = &site.receiver
                && let Some(chain) = ty.strip_prefix(clew_core::projectcalls::THROUGH)
                && library_chain(chain)
            {
                library_calls += 1;
                continue;
            }
            t.project_named += 1;
            let Some(caller) = site.caller.as_deref() else {
                continue;
            };
            // The definition of the caller's name in the file that the call
            // is in: the last to start before it (of a function's clauses or
            // overloads, or same-named methods of several classes, its own).
            let enclosing = |line: usize, caller: &str| -> Vec<usize> {
                let named: Vec<usize> = by_name
                    .get(caller)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|&id| calls.node(id).file == file.abs)
                    .collect();
                let start = named
                    .iter()
                    .map(|&id| calls.node(id).line)
                    .filter(|&l| l <= line)
                    .max();
                match start {
                    Some(start) => named
                        .into_iter()
                        .filter(|&id| calls.node(id).line == start)
                        .collect(),
                    None => named,
                }
            };
            let from = enclosing(site.line, caller);
            if from.is_empty() {
                continue;
            }
            if from
                .iter()
                .any(|&f| calls.callees_of(f).iter().any(|id| defined.contains(id)))
            {
                t.linked += 1;
                let targets: Vec<String> = from
                    .iter()
                    .flat_map(|&f| calls.callees_of(f))
                    .filter(|id| defined.contains(id))
                    .map(|&id| {
                        let n = calls.node(id);
                        format!("{}:{}", rel(&n.file), n.line)
                    })
                    .collect();
                if std::env::var_os("CLEW_TEST_DUMP").is_some() {
                    println!(
                        "SITE {}:{} {} -> {}",
                        rel(&file.abs),
                        site.line,
                        site.callee,
                        targets.join(", ")
                    );
                }
                // The graph links callers, not calls: where the caller calls
                // the name more than once, the targets are all its calls'.
                let others: Vec<String> = file_calls
                    .calls
                    .iter()
                    .filter(|c| {
                        c.line != site.line
                            && c.callee == site.callee
                            && c.caller == site.caller
                            && enclosing(c.line, caller) == from
                    })
                    .map(|c| c.line.to_string())
                    .collect();
                let shared = if others.is_empty() {
                    String::new()
                } else {
                    format!(" (with the caller's calls at {})", others.join(", "))
                };
                t.linked_samples.push(format!(
                    "{}:{} {}{} [{:?}] -> {}{shared}",
                    rel(&file.abs),
                    site.line,
                    if site.method { "." } else { "" },
                    site.callee,
                    site.receiver,
                    targets.join(", ")
                ));
            } else {
                let shape = match (&site.receiver, site.method) {
                    (_, false) => "bare",
                    (None, true) => "method on an unknown value",
                    (Some(clew_core::projectcalls::Receiver::Own), true) => "method on self",
                    (Some(clew_core::projectcalls::Receiver::Type(_)), true) => "method on a type",
                    (Some(clew_core::projectcalls::Receiver::Declared(_)), true) => {
                        "method on a declared type"
                    }
                };
                if std::env::var_os("CLEW_TEST_DUMP").is_some() {
                    println!(
                        "SITE {}:{} {} -> NONE",
                        rel(&file.abs),
                        site.line,
                        site.callee
                    );
                }
                *t.shapes.entry(shape).or_default() += 1;
                *t.missed_names.entry(site.callee.clone()).or_default() += 1;
                let samples = t.missed.entry(site.callee.clone()).or_default();
                if samples.len() < 3 {
                    let at: Vec<String> = defined
                        .iter()
                        .take(2)
                        .map(|&id| rel(&calls.node(id).file))
                        .collect();
                    samples.push(format!(
                        "{}:{} {}{} (defined in {})",
                        rel(&file.abs),
                        site.line,
                        if site.method { "." } else { "" },
                        site.callee,
                        at.join(", ")
                    ));
                }
            }
        }
    }
    for id in 0..calls.node_count() {
        if let Some(lang) = highlight::detect(&calls.node(id).file)
            && let Some(t) = tallies.get_mut(lang)
        {
            t.nodes += 1;
        }
    }
    println!("calls on a library's type, left out: {library_calls}");
    for (lang, t) in &tallies {
        if t.files == 0 {
            continue;
        }
        println!(
            "\n== {lang}: {} files, {} symbols; imports {} internal, {} external, {} unresolved; {} functions; {} calls, {} to project names, {} of those linked from a known caller",
            t.files,
            t.symbols,
            t.internal,
            t.external,
            t.unresolved,
            t.nodes,
            t.sites,
            t.project_named,
            t.linked
        );
        let mut ext: Vec<_> = t.externals.iter().collect();
        ext.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        println!(
            "  external: {}",
            ext.iter()
                .take(25)
                .map(|(n, c)| format!("{n}×{c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        for s in &t.unresolved_samples {
            println!("  unresolved {s}");
        }
        let mut missed: Vec<_> = t.missed.iter().collect();
        missed.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
        println!("  unlinked callee names: {}", missed.len());
        let sample: usize = std::env::var("CLEW_TEST_SAMPLE")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        if sample > 0 && !t.linked_samples.is_empty() {
            let step = (t.linked_samples.len() / sample).max(1);
            for s in t.linked_samples.iter().step_by(step).take(sample) {
                println!("  linked {s}");
            }
        }
        let mut shapes: Vec<_> = t.shapes.iter().collect();
        shapes.sort_by(|a, b| b.1.cmp(a.1));
        println!("  unlinked by shape: {shapes:?}");
        let mut names: Vec<_> = t.missed_names.iter().collect();
        names.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        println!(
            "  most unlinked: {}",
            names
                .iter()
                .take(40)
                .map(|(n, c)| format!("{n}×{c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        for (_, samples) in missed.iter().take(30) {
            for s in samples.iter().take(1) {
                println!("  unlinked {s}");
            }
        }
    }
}
