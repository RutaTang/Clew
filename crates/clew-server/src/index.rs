//! What the server derives from the project's files for the client: the
//! project-symbol snapshot and its patches, the name-based call graph, and
//! the API documentation index.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use clew_core::fs_scan::FileEntry;
// The project-wide indexing caps, shared with the client's own indexer: one
// definition, applied AFTER the language filter (see `indexable`).
pub(crate) use clew_core::fs_scan::{MAX_INDEX_FILE_BYTES, MAX_INDEX_FILES};
use clew_core::{highlight, outline};
use clew_protocol::{Event, Patch, ServerMessage, StructureIndex};
use tokio::sync::mpsc::UnboundedSender;

use crate::transport::{OutputBudget, send_bulk};

/// Whether the indexers take `f` at all — the filter the client's own index
/// applies too (`fs_scan::index_language`). The file cap is applied to the
/// files that pass this, not to the whole scan: capping first spent the
/// budget on files that were then skipped and dropped whatever sorted late —
/// every `src/` file of a large polyglot tree.
fn indexable(f: &FileEntry) -> bool {
    clew_core::fs_scan::index_language(&f.rel, &f.abs).is_some()
}

/// One `ProjectSymbols` payload, as read from disk.
pub(crate) struct SymbolPayload {
    pub(crate) files: Vec<clew_protocol::FileSymbols>,
    pub(crate) go_module: clew_protocol::Patch<String>,
    pub(crate) dart_package: clew_protocol::Patch<String>,
    pub(crate) structure: clew_protocol::Patch<StructureIndex>,
}

/// Build and send one `ProjectSymbols` publication, with `build` running
/// INSIDE the publication lock. `build` returns `None` to publish nothing.
///
/// The read has to be inside the lock, not just the stamp-and-send. Stamping
/// at send time makes `seq` the SEND order, and send order is not read order:
/// a full snapshot reads every file and can take seconds, so a watcher
/// partial published during that build carries fresher content yet a lower
/// seq — and the full, sent afterwards and stamped higher, overwrites it with
/// what the file looked like before the change. Holding the lock across the
/// read makes seq order equal read order, which is the ordering the client's
/// `seq > last applied` test actually needs.
///
/// Sent through the [`OutputBudget`]: a full snapshot of a large project is
/// tens of megabytes, and a burst of watcher republishes over a slow link
/// used to queue them without bound. Blocking — call from a thread, never from
/// the async runtime.
pub(crate) fn publish_project_symbols<F>(
    out: &UnboundedSender<ServerMessage>,
    budget: &OutputBudget,
    seq: &Mutex<u64>,
    root: &Path,
    full: bool,
    build: F,
) where
    F: FnOnce() -> Option<SymbolPayload>,
{
    let mut n = seq.lock().unwrap_or_else(|e| e.into_inner());
    let Some(payload) = build() else {
        return;
    };
    *n += 1;
    send_bulk(
        out,
        budget,
        ServerMessage::Notification {
            event: Event::ProjectSymbols {
                root: root.to_string_lossy().into_owned(),
                seq: *n,
                full,
                files: payload.files,
                go_module: payload.go_module,
                dart_package: payload.dart_package,
                structure: payload.structure,
            },
        },
    );
}

/// Everything a FULL snapshot carries: every file's symbols and all the
/// resolution metadata, recomputed from disk.
pub(crate) fn full_symbol_payload(root: &Path, files: &[FileEntry]) -> SymbolPayload {
    SymbolPayload {
        files: build_project_symbols(root, files),
        go_module: Patch::Set(clew_core::imports::read_go_module(root)),
        dart_package: Patch::Set(clew_core::imports::read_dart_package(root)),
        structure: structure_patch(root, files),
    }
}

/// The type/trait structure index as a `ProjectSymbols` patch: always `Set`,
/// with `None` meaning the project has none (never "not recomputed").
pub(crate) fn structure_patch(root: &Path, files: &[FileEntry]) -> Patch<StructureIndex> {
    let index = clew_core::structure::build(root, files);
    Patch::Set((!index.is_empty()).then_some(index))
}

/// Extract the project-symbol snapshot: per supported file (bounded exactly
/// like the client's own indexer — the same files counted against the same
/// file cap (`indexable`), per-file size, regular files confined to the
/// root), its outline symbols with the test classification and its raw
/// import specifiers. Blocking; run off the request loop.
pub(crate) fn build_project_symbols(
    root: &Path,
    files: &[FileEntry],
) -> Vec<clew_protocol::FileSymbols> {
    let mut snapshot = Vec::new();
    for f in files.iter().filter(|f| indexable(f)).take(MAX_INDEX_FILES) {
        let Some(entry) = file_symbols_for(root, &f.abs, &f.rel) else {
            continue;
        };
        if !entry.symbols.is_empty() || !entry.imports.is_empty() {
            snapshot.push(entry);
        }
    }
    snapshot
}

/// One indexable file, read and parsed ONCE. Everything the server derives
/// from a file's syntax — the snapshot's symbols and imports, the call
/// graph's definitions and call sites — comes from this one
/// [`outline::analyze`], the extraction the client's own index makes of its
/// files (`index::analyze_file`), so a remote project is described exactly as
/// a local one.
struct Analyzed {
    /// The file's language key.
    lang: &'static str,
    content: String,
    /// `None` when the source did not parse.
    analysis: Option<outline::Analysis>,
}

/// [`Analyzed`] for `abs` (at `rel`), or `None` when the file isn't
/// indexable (unsupported language, a noise directory, too large, not a
/// plain in-root file).
fn analyze_indexable(root: &Path, abs: &Path, rel: &str) -> Option<Analyzed> {
    let lang = clew_core::fs_scan::index_language(rel, abs)?;
    // One open, checked and capped on the handle. Checking the path and then
    // reading it again by name resolved the name twice and enforced the size
    // on a stat the read never saw.
    let content = clew_core::fs_scan::read_confined_capped(root, abs, MAX_INDEX_FILE_BYTES)?;
    let analysis = outline::analyze(&content, lang);
    Some(Analyzed {
        lang,
        content,
        analysis,
    })
}

/// One file's `FileSymbols` entry, or `None` when the file isn't indexable
/// (unsupported language, too large, not a plain in-root file). A readable
/// file with no symbols yields an entry with an empty list — for the partial
/// (watcher) updates that means "clear what you had for this rel".
pub(crate) fn file_symbols_for(
    root: &Path,
    abs: &Path,
    rel: &str,
) -> Option<clew_protocol::FileSymbols> {
    let Analyzed {
        lang,
        content,
        analysis,
    } = analyze_indexable(root, abs, rel)?;
    let lines: Vec<&str> = content.lines().collect();
    let (located, raw_imports) = analysis.map(|a| (a.symbols, a.imports)).unwrap_or_default();
    let symbols = located
        .into_iter()
        .map(|l| l.symbol)
        .map(|s| clew_protocol::IndexSymbol {
            is_test: matches!(s.kind.as_str(), "function" | "method")
                && outline::is_test_fn(&lines, s.line, &s.name, lang),
            entry: outline::entry_kind(&lines, s.line, &s.name, &s.kind, lang, rel)
                .map(|k| k.key().to_string()),
            name: s.name,
            kind: s.kind,
            line: s.line,
        })
        .collect();
    // Rescoped exactly like the client's own index (see `clew_core::rustscope`):
    // a test module's `use super::*` names its file, not the file's parent.
    let imports = clew_core::rustscope::scope_imports(&content, lang, raw_imports)
        .into_iter()
        .map(|i| clew_protocol::WireImport {
            module: i.module,
            line: i.line,
            is_mod: i.is_mod_decl,
        })
        .collect();
    Some(clew_protocol::FileSymbols {
        rel: rel.to_string(),
        symbols,
        imports,
    })
}

/// Build the name-based project call graph for a `ProjectCalls` request:
/// callable definitions and call sites come from this host's files (under
/// the indexer's caps), the import scope from the client (rel-based,
/// converted to this host's absolute paths for the build, and back to rels
/// for the wire). Blocking; run off the request loop.
///
/// ONE parse per file, with the snapshot's extraction ([`analyze_indexable`]):
/// the definitions and the call sites are read off the same tree. The build
/// used to parse every file twice — once for its outline, once more inside the
/// graph for its calls.
pub(crate) fn build_project_calls_graph(
    root: &Path,
    files: &[FileEntry],
    scope: &[(String, Vec<String>)],
) -> clew_protocol::CallGraph {
    use clew_core::projectcalls::{Def, FileCalls};
    let mut defs: Vec<Def> = Vec::new();
    let mut calls: Vec<FileCalls> = Vec::new();
    for f in files.iter().filter(|f| indexable(f)).take(MAX_INDEX_FILES) {
        let Some(Analyzed {
            analysis: Some(analysis),
            ..
        }) = analyze_indexable(root, &f.abs, &f.rel)
        else {
            continue;
        };
        defs.extend(
            analysis
                .symbols
                .iter()
                .filter(|l| outline::is_callable(&l.symbol.kind))
                .map(|l| Def {
                    name: l.symbol.name.clone(),
                    kind: l.symbol.kind.clone(),
                    file: f.abs.clone(),
                    line: l.symbol.line,
                }),
        );
        calls.extend(
            FileCalls::of(
                f.abs.clone(),
                analysis.lang,
                analysis.calls,
                &analysis.symbols,
            )
            .filter(|c| !c.is_empty()),
        );
    }
    let scope: std::collections::HashMap<PathBuf, std::collections::HashSet<PathBuf>> = scope
        .iter()
        .map(|(rel, imports)| {
            (
                root.join(rel),
                imports.iter().map(|r| root.join(r)).collect(),
            )
        })
        .collect();
    clew_core::projectcalls::ProjectCallGraph::build_from_calls(defs, &calls, &scope).to_wire(|p| {
        p.strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    })
}

/// Build the project's API documentation index: for every file with a
/// recognized language and a non-empty documented API, its nested doc items.
/// Blocking; run off the async runtime.
pub(crate) fn build_docs(root: &Path, files: &[FileEntry]) -> Vec<clew_protocol::DocFile> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Per-file API extraction is independent, so fan it out across cores — a
    // single-threaded pass takes minutes on a large repo (flutter_rust_bridge is
    // ~5k files). Work-steal from a shared atomic cursor rather than pre-slicing
    // into contiguous chunks: the heavy files (generated, symbol-dense) cluster
    // in one directory, so a contiguous split dumps them all on one thread while
    // the rest idle. Pulling one file at a time keeps every core busy.
    let threads = std::thread::available_parallelism().map_or(4, |p| p.get());
    let next = AtomicUsize::new(0);
    let root = &root;
    let mut out: Vec<clew_protocol::DocFile> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let next = &next;
                scope.spawn(move || {
                    let mut local = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(f) = files.get(i) else { break };
                        if let Some(doc) = build_doc_one(root, f) {
                            local.push(doc);
                        }
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    // Threads finish in nondeterministic order; sort so the DOCS list is stable.
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// The public-API doc items for one file, or `None` if it has no recognized
/// language, is too large, unreadable, or has no documented API. See
/// [`build_docs`].
pub(crate) fn build_doc_one(root: &Path, f: &FileEntry) -> Option<clew_protocol::DocFile> {
    // Skip very large files. A generated / bundled / macro-heavy source (napi's
    // `async_runtime.rs` is ~1 MB) makes the tree-sitter parse + API-surface
    // extraction crawl. The shared indexer cap, which the semantic index's
    // per-file cap matches.
    let lang = highlight::detect(&f.abs)?;
    // Re-verify the path is still a regular file inside the project — the scan
    // can be stale — and enforce the cap on the READ. Sizing it from a
    // separate `metadata` call left a file free to grow past the limit in
    // between, and left the read itself able to block on a FIFO swapped in.
    let source = clew_core::fs_scan::read_confined_capped(root, &f.abs, MAX_INDEX_FILE_BYTES)?;
    // Skip generated code. It isn't the hand-written public API the DOCS view is
    // for, and codegen output (Dart freezed/`.g.dart`, protobuf, flutter_rust_
    // bridge's `frb_generated.*` — thousands of lines of boilerplate each) is the
    // main thing that made the extraction crawl on a big repo.
    if is_generated_source(&source) {
        return None;
    }
    let items = clew_core::apidoc::build_file(&source, lang);
    // A file of no API but with its own doc (a `mod.rs` of `pub mod` lines,
    // a package's `__init__.py`) still defines its module in the glossary.
    let doc = clew_core::apidoc::module_doc(&source, lang);
    (!items.is_empty() || !doc.is_empty()).then(|| clew_protocol::DocFile {
        rel: f.rel.clone(),
        items,
        doc,
    })
}

/// Whether a source file is machine-generated, by the "do not edit" banner that
/// generators (freezed, protobuf, flutter_rust_bridge, prost, …) put at the top.
/// Checked against the first lines only, lower-cased, so it's cheap and robust
/// to a leading license block.
pub(crate) fn is_generated_source(source: &str) -> bool {
    // Normalize `’`/`'` apostrophes so "don't" matches, and lower-case.
    let head = source
        .lines()
        .take(40)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase()
        .replace('\u{2019}', "'");
    const MARKERS: &[&str] = &[
        "do not edit",
        "don't edit",
        "do not modify",
        "don't modify",
        "@generated",
        "generated by",
        "generated file",
        "generated code",
        "code generated",
        "automatically generated",
        "auto-generated",
        "autogenerated",
    ];
    MARKERS.iter().any(|m| head.contains(m))
}

#[cfg(test)]
mod tests {
    use super::is_generated_source;
    use clew_core::fs_scan::FileEntry;

    /// A remote client resolves the snapshot's imports as they arrive, so the
    /// server rescopes them exactly like the client's own index: a test
    /// module's `use super::*` names its own file (not the file's parent), and
    /// the re-export / glob markers the graph needs survive.
    #[test]
    fn snapshot_imports_are_rescoped_like_the_local_index() {
        let root = crate::test_support::Scratch::new("server-scope");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let abs = root.join("src/shell.rs");
        std::fs::write(
            &abs,
            "pub use crate::a::*;\npub fn boot() {}\nfn main() {}\n#[cfg(test)]\nmod tests {\n    use super::*;\n}\n",
        )
        .unwrap();
        let entry = super::file_symbols_for(&root, &abs, "src/shell.rs").expect("indexable");
        let imports: Vec<&str> = entry.imports.iter().map(|i| i.module.as_str()).collect();
        assert_eq!(imports, ["pub crate::a::*", "self::*"]);
        assert!(entry.symbols.iter().any(|s| s.name == "boot"));
        // Entry points are classified where the text is, like tests.
        let entry_of = |name: &str| {
            entry
                .symbols
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.entry.clone())
        };
        assert_eq!(entry_of("main").as_deref(), Some("main"));
        assert_eq!(entry_of("boot"), None);
    }

    /// The server's full builds parse each indexable file ONCE: the symbol
    /// snapshot, and the `ProjectCalls` graph — which read each file's
    /// definitions and its call sites off two separate parses — and the
    /// graph is still the one the two parses built.
    #[test]
    fn full_builds_parse_each_file_once() {
        use clew_core::highlight::parses_on_this_thread as parses;
        use clew_core::projectcalls::{Def, ProjectCallGraph};
        use std::path::PathBuf;
        let root = clew_core::testutil::TempDir::new("server-parses");
        let sources: &[(&str, &str)] = &[
            (
                "src/a.rs",
                "pub fn helper() {}\npub fn run() { helper(); crate::b::other(); }\n",
            ),
            ("src/b.rs", "pub fn other() { super::a::run(); }\n"),
            (
                "src/c.h",
                "int proto(int x);\nint use_it(void) { return proto(1); }\n",
            ),
            ("web/d.py", "def a():\n    b()\n\ndef b():\n    a()\n"),
            ("notes.md", "# not indexed\n"),
        ];
        let mut files = Vec::new();
        for (rel, text) in sources {
            let abs = root.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, text).unwrap();
            files.push(FileEntry {
                abs,
                rel: (*rel).to_string(),
            });
        }
        let indexable: Vec<&FileEntry> = files.iter().filter(|f| super::indexable(f)).collect();
        let per_file = indexable.len() as u64;
        assert_eq!(per_file, 4);

        let start = parses();
        let snapshot = super::build_project_symbols(&root, &files);
        assert_eq!(
            parses() - start,
            per_file,
            "the snapshot: one parse per file"
        );
        assert_eq!(snapshot.len(), 4);

        let start = parses();
        let graph = super::build_project_calls_graph(&root, &files, &[]);
        assert_eq!(
            parses() - start,
            per_file,
            "the call graph: one parse per file"
        );

        // What the two parses built.
        let mut defs = Vec::new();
        let mut reread: Vec<(PathBuf, String)> = Vec::new();
        for f in &indexable {
            let text = std::fs::read_to_string(&f.abs).unwrap();
            let lang = clew_core::highlight::detect(&f.abs).unwrap();
            defs.extend(
                clew_core::outline::extract(&text, lang)
                    .into_iter()
                    .filter(|s| clew_core::outline::is_callable(&s.kind))
                    .map(|s| Def {
                        name: s.name,
                        kind: s.kind,
                        file: f.abs.clone(),
                        line: s.line,
                    }),
            );
            reread.push((f.abs.clone(), text));
        }
        let expected = ProjectCallGraph::build(defs, &reread, &Default::default()).to_wire(|p| {
            p.strip_prefix(&*root)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        });
        assert_eq!(graph, expected);
        assert!(
            graph.nodes.iter().map(|n| n.callees.len()).sum::<usize>() >= 5,
            "{graph:?}"
        );
    }

    #[test]
    fn detects_generated_sources() {
        // Dart freezed / .g.dart, flutter_rust_bridge, protobuf, prost headers.
        assert!(is_generated_source(
            "// coverage:ignore-file\n// GENERATED CODE - DO NOT MODIFY BY HAND\n"
        ));
        assert!(is_generated_source(
            "// This file is automatically generated, so please do not edit it.\n// @generated by `flutter_rust_bridge`\n"
        ));
        assert!(is_generated_source(
            "// Code generated by protoc-gen-go. DO NOT EDIT.\n"
        ));
        assert!(is_generated_source("# @generated by prost-build\n"));
        // ruff style: "generated file" + the "don't" contraction (both misses before).
        assert!(is_generated_source(
            "// This is a generated file. Don't modify it by hand!\n"
        ));
        assert!(is_generated_source(
            "// This is a generated file. Don\u{2019}t modify it by hand!\n"
        ));
        // Hand-written source is not skipped.
        assert!(!is_generated_source(
            "/// Starts the main function in Rust.\npub fn initialize() {}\n"
        ));
        assert!(!is_generated_source("import 'dart:async';\nclass Foo {}\n"));
    }
}
