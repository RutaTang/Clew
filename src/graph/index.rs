//! Project-wide symbol index, built in the background after a scan and kept
//! incrementally fresh per file as the codebase changes.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use clew_core::projectcalls::{CallSite, FileCalls};

use crate::cache::{self, CachedCalls, CachedImport, CachedSymbol, FileCache};
use crate::fs_scan::FileEntry;
use crate::imports::RawImport;
use crate::incremental::{Version, content_hash};
use crate::{highlight, outline};

/// Caps keeping index build time bounded on huge repos — the ONE pair every
/// parsing pass shares (the server's symbol snapshot, the type-structure
/// pass), so the client's index and a remote one leave out the same files.
pub use clew_core::fs_scan::{MAX_INDEX_FILE_BYTES, MAX_INDEX_FILES};

#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub name: String,
    pub kind: String,
    pub rel: String,
    pub abs: PathBuf,
    pub line: usize, // 1-based
    /// True when this function/method is a test (see [`is_test_fn`]).
    pub is_test: bool,
    /// The entry-point kind, for a function execution enters the project
    /// through (see [`entry_kind`]); `None` for the rest, tests included.
    pub entry: Option<EntryKind>,
}

// Shared with clew-server's project-symbol snapshot, so both classify tests
// and entry points identically.
pub use clew_core::outline::{EntryKind, entry_kind, is_test_fn};

/// Result of the initial background pass: symbols grouped by file (so a single
/// file can be re-indexed in place) plus each file's content hash (so the
/// incremental registry is seeded from the same single read of the tree), and
/// the files whose content changed versus the persistent cache (i.e. edited
/// while clew was closed).
#[derive(Debug, Default, Clone)]
pub struct Indexed {
    pub by_file: HashMap<PathBuf, Vec<SymbolEntry>>,
    /// Raw (unresolved) imports per file, for building the import graph. Resolved
    /// against the live file set by the caller, not cached in resolved form.
    pub imports_by_file: HashMap<PathBuf, Vec<RawImport>>,
    /// Call sites per file, read off the same parse as its symbols (or
    /// restored from the cache with them) — what the project call graph links
    /// ([`clew_core::projectcalls::ProjectCallGraph::build_from_calls`]), so
    /// it never parses the project again. Only files that contribute
    /// ([`FileCalls::is_empty`] is false). Shared: a graph build takes the
    /// set to another thread without copying it.
    pub calls_by_file: HashMap<PathBuf, Arc<FileCalls>>,
    /// Per Rust file, the importable items it defines as the import graph
    /// compares them (`imports::rust_item_keys`) — hashed here, on the build's
    /// thread, rather than for every symbol of the project on the UI thread
    /// when the build lands. Only files that define any.
    pub rust_items: HashMap<PathBuf, crate::imports::RustItems>,
    pub hashes: Vec<(PathBuf, Version)>,
    pub changed: Vec<PathBuf>,
    /// Indexable (supported-language) files left out because the project
    /// has more than [`MAX_INDEX_FILES`] of them — the UI states the cap
    /// instead of silently missing symbols.
    pub over_cap: usize,
    /// Indexable files skipped for exceeding [`MAX_INDEX_FILE_BYTES`].
    pub too_large: usize,
}

impl Indexed {
    /// A one-line note on what the index left out, or `None` when it is
    /// complete: "Indexed 20000 files; 1234 more not indexed (limit 20000);
    /// 3 too large (over 512 KiB)".
    pub fn cap_note(&self) -> Option<String> {
        if self.over_cap == 0 && self.too_large == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if self.over_cap > 0 {
            parts.push(format!(
                "{} more files not indexed (limit {MAX_INDEX_FILES})",
                self.over_cap
            ));
        }
        if self.too_large > 0 {
            parts.push(format!(
                "{} too large (over {} KiB)",
                self.too_large,
                MAX_INDEX_FILE_BYTES / 1024
            ));
        }
        Some(parts.join("; "))
    }
}

/// What indexing one file produces: its definition symbols, its raw
/// (unresolved) imports, and its call sites.
#[derive(Debug, Default, Clone)]
pub struct FileFacts {
    pub symbols: Vec<SymbolEntry>,
    pub imports: Vec<RawImport>,
    /// `None` for a language without a call model, or a file that neither
    /// makes a call nor declares a bodyless callable (see
    /// [`Indexed::calls_by_file`]).
    pub calls: Option<Arc<FileCalls>>,
}

/// Index one already-read file's `content` from a SINGLE parse
/// ([`outline::analyze`], the extraction the server's snapshot and the
/// explain gatherer share): its definition symbols (each classified as a test
/// or not), its raw imports, and its call sites. The call sites used to be
/// dropped here, and the project call graph then read and parsed every file
/// of the project a second time to find them again; now the graph links what
/// the index kept ([`Indexed::calls_by_file`]). Rust paths written inside
/// inline modules are re-expressed relative to the file (see
/// [`clew_core::rustscope::scope_rust_imports`]), which extraction alone
/// cannot know — exactly as the server's snapshot does, so a local and a
/// remote project resolve the same specifiers.
pub fn analyze_file(abs: &Path, rel: &str, content: &str, lang: &'static str) -> FileFacts {
    let Some(analysis) = outline::analyze(content, lang) else {
        return FileFacts::default();
    };
    let calls = FileCalls::of(
        abs.to_path_buf(),
        analysis.lang,
        analysis.calls,
        &analysis.symbols,
    )
    .filter(|calls| !calls.is_empty())
    .map(Arc::new);
    let symbols = if highlight::tags_for(lang).is_some() {
        let lines: Vec<&str> = content.lines().collect();
        analysis
            .symbols
            .into_iter()
            .map(|located| {
                let symbol = located.symbol;
                let is_test = matches!(symbol.kind.as_str(), "function" | "method")
                    && is_test_fn(&lines, symbol.line, &symbol.name, lang);
                let entry = entry_kind(&lines, symbol.line, &symbol.name, &symbol.kind, lang, rel);
                SymbolEntry {
                    name: symbol.name,
                    kind: symbol.kind,
                    rel: rel.to_string(),
                    abs: abs.to_path_buf(),
                    line: symbol.line,
                    is_test,
                    entry,
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let imports = clew_core::rustscope::scope_imports(content, lang, analysis.imports);
    FileFacts {
        symbols,
        imports,
        calls,
    }
}

/// Raw imports for one already-read file's `content` (see [`analyze_file`]).
#[cfg(test)]
pub fn file_imports(content: &str, lang: &'static str) -> Vec<RawImport> {
    analyze_file(Path::new(""), "", content, lang).imports
}

/// Convert extracted raw imports to their cacheable form.
fn to_cached_imports(raw: &[RawImport]) -> Vec<CachedImport> {
    raw.iter()
        .map(|r| CachedImport {
            module: r.module.clone(),
            line: r.line,
            is_mod: r.is_mod_decl,
        })
        .collect()
}

/// Convert cached imports back to the in-memory raw form.
fn from_cached_imports(cached: &[CachedImport]) -> Vec<RawImport> {
    cached
        .iter()
        .map(|c| RawImport {
            module: c.module.clone(),
            line: c.line,
            is_mod_decl: c.is_mod,
        })
        .collect()
}

/// A file's call sites in their cacheable form (see [`CachedCalls`]).
fn to_cached_calls(calls: &FileCalls) -> CachedCalls {
    /// `name`'s index in `names`, adding it on first sight.
    fn intern<'a>(name: &'a str, names: &mut Vec<String>, seen: &mut HashMap<&'a str, i64>) -> i64 {
        *seen.entry(name).or_insert_with(|| {
            names.push(name.to_string());
            names.len() as i64 - 1
        })
    }
    let mut names = Vec::new();
    let mut seen = HashMap::new();
    let mut sites = Vec::with_capacity(calls.calls.len() * 3);
    let mut previous = 0i64;
    for site in &calls.calls {
        let caller = match site.caller.as_deref() {
            Some(caller) => intern(caller, &mut names, &mut seen) + 1,
            None => 0,
        };
        let callee = intern(&site.callee, &mut names, &mut seen) * 2 + i64::from(site.method);
        let line = site.line as i64;
        sites.extend([caller, callee, line - previous]);
        previous = line;
    }
    let mut declarations: Vec<usize> = calls.declarations.iter().copied().collect();
    declarations.sort_unstable();
    let mut bodies: Vec<(usize, (usize, usize))> = calls
        .bodies
        .iter()
        .map(|(&line, &span)| (line, span))
        .collect();
    bodies.sort_unstable();
    CachedCalls {
        lang: calls.lang.key().to_string(),
        names,
        sites,
        declarations,
        bodies: bodies
            .into_iter()
            .flat_map(|(line, (first, last))| [line, first, last])
            .collect(),
    }
}

/// The call sites a cache entry holds for `abs`, or `None` when the entry is
/// damaged — a language this build does not know, a name index out of range,
/// a line before the first — and must not be reused: the cache is an
/// accelerator, never the source of a wrong graph (or of a panic on a bad
/// index).
fn from_cached_calls(abs: &Path, cached: &CachedCalls) -> Option<FileCalls> {
    let lang = highlight::Lang::from_key(&cached.lang)?;
    let name = |i: i64| cached.names.get(usize::try_from(i).ok()?).cloned();
    if !cached.sites.len().is_multiple_of(3) {
        return None;
    }
    let mut previous = 0i64;
    let mut calls = Vec::with_capacity(cached.sites.len() / 3);
    for site in cached.sites.chunks_exact(3) {
        let &[caller, callee, delta] = site else {
            return None;
        };
        if caller < 0 || callee < 0 {
            return None;
        }
        let line = previous.checked_add(delta).filter(|&l| l >= 1)?;
        previous = line;
        calls.push(CallSite {
            caller: match caller {
                0 => None,
                n => Some(name(n - 1)?),
            },
            callee: name(callee / 2)?,
            method: callee % 2 == 1,
            line: usize::try_from(line).ok()?,
        });
    }
    if !cached.bodies.len().is_multiple_of(3) {
        return None;
    }
    let mut bodies = HashMap::with_capacity(cached.bodies.len() / 3);
    for body in cached.bodies.chunks_exact(3) {
        let &[line, first, last] = body else {
            return None;
        };
        if first > last {
            return None;
        }
        bodies.insert(line, (first, last));
    }
    Some(FileCalls {
        file: abs.to_path_buf(),
        lang,
        calls,
        declarations: cached.declarations.iter().copied().collect(),
        bodies,
    })
}

/// A cache entry's call sites as the index hands them out: `Some(None)` when
/// the file has none, `None` when the entry is damaged (see
/// [`from_cached_calls`]) — then the entry is not reused at all, and the file
/// is parsed again.
fn cached_calls(abs: &Path, entry: &FileCache) -> Option<Option<FileCalls>> {
    match &entry.calls {
        None => Some(None),
        Some(cached) => from_cached_calls(abs, cached).map(Some),
    }
}

/// File modification time in nanoseconds since the epoch, or `0` when it can't
/// be read (which disables the mtime fast path for that file — it is then
/// confirmed by content hash instead).
fn mtime_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Read an already-open project file, enforcing `max` on the READ.
///
/// The size the caller checked came from a stat, and the file can grow between
/// that stat and this read — `take(max + 1)` is what tells "exactly at the
/// limit" apart from "grew past it since", and an oversized file is dropped
/// rather than indexed in part. Takes the open handle rather than the path so
/// the leaf is resolved once (see [`clew_core::statefile::open_plain`]): a path
/// re-opened here could be a different file than the one that passed the check.
fn read_capped(f: &std::fs::File, max: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    f.take(max + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= max).then_some(bytes)
}

/// Cold index build (no cache): read + parse every supported file. Blocking.
/// Retained for tests; the runtime always warm-starts via [`build_indexed_warm`].
#[cfg(test)]
pub fn build_indexed(root: &Path, files: Arc<Vec<FileEntry>>) -> Indexed {
    build_core(root, &files, &cache::Store::default()).0
}

/// Warm index build: reuse the persistent cache for files confirmed unchanged
/// (mtime+size fast path, content-hash fallback), re-parsing only what changed
/// while clew was closed, then persist the refreshed cache. Blocking.
pub fn build_indexed_warm(
    root: &Path,
    store: Option<&Path>,
    files: Arc<Vec<FileEntry>>,
) -> Indexed {
    // `store` is clew's own derived directory, NOT the project's `.clew/`:
    // an entry is reused on a content hash the repository can compute for its
    // own files, so a cache the repository ships would be accepted as clew's
    // own index. Without a store the build runs cold and persists nothing.
    let old = store.map(cache::Store::load).unwrap_or_default();
    let (indexed, fresh) = build_core(root, &files, &old);
    // Best-effort persist; a failure only means a colder start next time.
    if let Some(store) = store {
        let _ = fresh.save(store);
    }
    indexed
}

/// Shared index build. Reuses a file's cached hash + symbols only after
/// confirming its content is unchanged; otherwise reads, hashes, and (on a real
/// change) re-parses. Returns the index plus the freshly rebuilt cache.
fn build_core(root: &Path, files: &[FileEntry], old: &cache::Store) -> (Indexed, cache::Store) {
    build_core_capped(root, files, old, MAX_INDEX_FILES)
}

/// [`build_core`] with an explicit file cap (tests use a small one).
fn build_core_capped(
    root: &Path,
    files: &[FileEntry],
    old: &cache::Store,
    cap: usize,
) -> (Indexed, cache::Store) {
    let mut indexed = Indexed::default();
    let mut fresh = cache::Store::default();
    // The cap counts INDEXABLE files: applied to the raw list, a project whose
    // assets, lockfiles and docs sort first used the whole budget before
    // reaching its sources, and those were dropped without a word.
    // The noise directories stay out as well (`.clew/` is in the tree for its
    // config files, which are not source to index), by the filter the
    // server's snapshot applies too (`fs_scan::index_language`).
    let indexable = files
        .iter()
        .filter_map(|f| clew_core::fs_scan::index_language(&f.rel, &f.abs).map(|lang| (f, lang)));
    let mut taken = 0usize;
    for (file, lang) in indexable {
        if taken == cap {
            indexed.over_cap += 1;
            continue;
        }
        taken += 1;
        // Only index regular files that are really inside the project: a
        // symlink would pull outside content into the symbol index.
        if !clew_core::fs_scan::is_inside(root, &file.abs) {
            continue;
        }
        // Open ONCE and take every decision from the open handle. Stat the
        // name, then read the name, and the leaf is resolved twice: a file
        // that grew past the cap in between was read in full, and one swapped
        // for a FIFO blocked `read` forever, wedging this build thread. Same
        // shape as `fs_scan::read_confined_capped`, spelled out here because
        // this path needs the raw bytes (binaries are hashed too) and the
        // mtime from that very stat.
        let Some(f) = clew_core::statefile::open_plain(&file.abs) else {
            continue;
        };
        let Ok(meta) = f.metadata() else {
            continue;
        };
        if meta.len() > MAX_INDEX_FILE_BYTES {
            indexed.too_large += 1;
            continue;
        }
        let size = meta.len();
        let mtime = mtime_ns(&meta);
        let cached = old.get(&file.rel);

        // The entry, and its call sites as the graph takes them. A cached
        // entry whose call sites do not decode is not reused at all.
        let (entry, calls): (FileCache, Option<FileCalls>) = if let Some(c) = cached
            && mtime != 0
            && c.mtime_ns == mtime
            && c.size == size
            && let Some(calls) = cached_calls(&file.abs, c)
        {
            // Fast path: stat says unchanged — reuse cached hash + symbols.
            (c.clone(), calls)
        } else {
            // Read from the handle already open above, through the cap.
            let Some(bytes) = read_capped(&f, MAX_INDEX_FILE_BYTES) else {
                continue;
            };
            let hash = content_hash(&bytes);
            if let Some(c) = cached
                && c.hash == hash
                && let Some(calls) = cached_calls(&file.abs, c)
            {
                // Content is identical; only the mtime changed. Reuse the derived
                // artifacts, refresh the stat metadata so the fast path hits next.
                let entry = FileCache {
                    mtime_ns: mtime,
                    size,
                    hash,
                    symbols: c.symbols.clone(),
                    imports: c.imports.clone(),
                    calls: c.calls.clone(),
                };
                (entry, calls)
            } else {
                // Genuinely changed (or new): re-parse. A prior cache entry with
                // a different hash means it changed while clew was closed.
                if cached.is_some_and(|c| c.hash != hash) {
                    indexed.changed.push(file.abs.clone());
                }
                let facts = match String::from_utf8(bytes) {
                    // One parse for all three (see `analyze_file`).
                    Ok(content) => analyze_file(&file.abs, &file.rel, &content, lang),
                    Err(_) => FileFacts::default(), // binary — nothing to extract
                };
                let symbols = facts
                    .symbols
                    .into_iter()
                    .map(|s| CachedSymbol {
                        name: s.name,
                        kind: s.kind,
                        line: s.line,
                        is_test: s.is_test,
                        entry: s.entry.map(|k| k.key().to_string()),
                    })
                    .collect();
                let entry = FileCache {
                    mtime_ns: mtime,
                    size,
                    hash,
                    symbols,
                    imports: to_cached_imports(&facts.imports),
                    calls: facts.calls.as_deref().map(to_cached_calls),
                };
                let calls = facts.calls.map(Arc::unwrap_or_clone);
                (entry, calls)
            }
        };

        indexed.hashes.push((file.abs.clone(), entry.hash));
        if !entry.symbols.is_empty() {
            let syms = entry
                .symbols
                .iter()
                .map(|c| SymbolEntry {
                    name: c.name.clone(),
                    kind: c.kind.clone(),
                    rel: file.rel.clone(),
                    abs: file.abs.clone(),
                    line: c.line,
                    is_test: c.is_test,
                    entry: c.entry.as_deref().and_then(EntryKind::from_key),
                })
                .collect::<Vec<_>>();
            let items = crate::imports::rust_item_keys(&file.abs, &syms);
            if !items.is_empty() {
                indexed.rust_items.insert(file.abs.clone(), items);
            }
            indexed.by_file.insert(file.abs.clone(), syms);
        }
        if !entry.imports.is_empty() {
            indexed
                .imports_by_file
                .insert(file.abs.clone(), from_cached_imports(&entry.imports));
        }
        if let Some(calls) = calls.filter(|c| !c.is_empty()) {
            indexed
                .calls_by_file
                .insert(file.abs.clone(), Arc::new(calls));
        }
        fresh.insert(file.rel.clone(), entry);
    }
    (indexed, fresh)
}

/// Flatten per-file symbols into one list (stable order by path then line) for
/// the fuzzy symbol finder.
pub fn flatten<V: std::borrow::Borrow<Vec<SymbolEntry>>>(
    by_file: &HashMap<PathBuf, V>,
) -> Vec<SymbolEntry> {
    let mut files: Vec<&PathBuf> = by_file.keys().collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        out.extend(by_file[f].borrow().iter().cloned());
    }
    out
}

/// Extract definition symbols from every supported file (flat list). Retained
/// for tests and callers that don't need the per-file grouping.
#[cfg(test)]
pub fn build(root: &Path, files: Arc<Vec<FileEntry>>) -> Vec<SymbolEntry> {
    flatten(&build_indexed(root, files).by_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size gate has to hold for the bytes actually read. A file that
    /// grows between the stat that admitted it and the read — the whole reason
    /// the read goes through the OPEN handle — must be dropped, not indexed in
    /// full. Growing the file after the open reproduces that window exactly.
    #[test]
    fn a_file_that_grows_after_the_stat_is_dropped_by_the_read_cap() {
        use std::io::Write;
        let root = clew_core::testutil::TempDir::new("index-read-cap");
        let path = root.join("a.rs");
        std::fs::write(&path, "x".repeat(64)).unwrap();

        let f = clew_core::statefile::open_plain(&path).expect("a plain file inside the project");
        // Stat-time size (64) is under the cap; by the time the read runs the
        // file is over it.
        assert_eq!(f.metadata().unwrap().len(), 64);
        let mut w = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        w.write_all(&b"y".repeat(1024)).unwrap();
        w.flush().unwrap();

        assert!(
            read_capped(&f, 100).is_none(),
            "a file that grew past the cap since the stat must be refused, not read in full"
        );
        // Under the cap the same handle still yields the whole file.
        let g = clew_core::statefile::open_plain(&path).unwrap();
        assert_eq!(read_capped(&g, 4096).map(|b| b.len()), Some(1088));
    }

    /// The callable definitions a graph links, from an index's symbols.
    fn defs_of(indexed: &Indexed) -> Vec<clew_core::projectcalls::Def> {
        flatten(&indexed.by_file)
            .into_iter()
            .map(|s| clew_core::projectcalls::Def {
                name: s.name,
                kind: s.kind,
                file: s.abs,
                line: s.line,
            })
            .collect()
    }

    /// The project call graph links the call sites the index read, so a full
    /// build — index and graph together — parses each indexable file ONCE.
    /// The graph pass used to re-read and re-parse every file right after the
    /// index had; it is still the graph that doing so gave. A warm start
    /// parses nothing and restores the same call sites from the cache, and a
    /// damaged cache entry is parsed again rather than trusted.
    #[test]
    fn a_full_build_parses_each_file_once_and_the_call_graph_parses_none() {
        use clew_core::highlight::parses_on_this_thread as parses;
        use clew_core::projectcalls::ProjectCallGraph;
        let root = clew_core::testutil::TempDir::new("index-parses");
        let store = clew_core::testutil::TempDir::new("index-parses-store");
        let sources: &[(&str, &str)] = &[
            (
                "src/a.rs",
                "pub fn helper() {}\npub fn run() { helper(); crate::b::other(); }\n",
            ),
            (
                "src/b.rs",
                "pub fn other() { super::a::run(); }\n#[test]\nfn t() { other(); }\n",
            ),
            (
                "src/c.h",
                "int proto(int x);\nint use_it(void) { return proto(1); }\n",
            ),
            ("web/d.js", "function a() { b(); }\nconst b = () => a();\n"),
            // Nested same-name functions: which `f` a call is in comes from
            // the definition spans, which a warm start must restore too.
            (
                "web/n.js",
                "function f() {\n  function f() {\n    inner();\n  }\n  outer();\n}\n\
                 function inner() {}\nfunction outer() {}\n",
            ),
            ("Cargo.toml", "[package]\nname = \"p\"\n"),
            ("README.md", "# not indexed\n"),
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
        // What the index takes: the languages it extracts from (Cargo.toml
        // and README.md are left out — `fs_scan::index_language`).
        let indexable: Vec<&FileEntry> = files
            .iter()
            .filter(|f| clew_core::fs_scan::index_language(&f.rel, &f.abs).is_some())
            .collect();
        assert_eq!(indexable.len(), files.len() - 2);
        let per_file = indexable.len() as u64;
        let reread: Vec<(PathBuf, String)> = indexable
            .iter()
            .map(|f| (f.abs.clone(), std::fs::read_to_string(&f.abs).unwrap()))
            .collect();
        let files = Arc::new(files);
        let scope = HashMap::new();
        let wire = |g: &ProjectCallGraph| g.to_wire(|p| p.to_string_lossy().into_owned());
        let graph_of = |indexed: &Indexed, defs: Vec<clew_core::projectcalls::Def>| {
            ProjectCallGraph::build_from_calls(
                defs,
                indexed.calls_by_file.values().map(Arc::as_ref),
                &scope,
            )
        };

        let start = parses();
        let cold = build_indexed_warm(&root, Some(&store), files.clone());
        assert_eq!(parses() - start, per_file, "one parse per indexable file");
        let defs = defs_of(&cold);
        let graph = graph_of(&cold, defs.clone());
        assert_eq!(parses() - start, per_file, "the call graph parses nothing");
        let reparsed = ProjectCallGraph::build(defs.clone(), &reread, &scope);
        assert_eq!(
            wire(&graph),
            wire(&reparsed),
            "the graph re-parsing every file gave"
        );
        assert!(graph.edge_count() >= 5, "{:?}", wire(&graph));

        // Warm: nothing parsed, and the same call sites, from the cache.
        let start = parses();
        let warm = build_indexed_warm(&root, Some(&store), files.clone());
        assert_eq!(parses(), start, "a warm start parses nothing");
        assert_eq!(wire(&graph_of(&warm, defs.clone())), wire(&graph));
        let nested = root.join("web/n.js");
        let spans = &warm.calls_by_file[&nested].bodies;
        assert_eq!(spans, &cold.calls_by_file[&nested].bodies);
        assert_eq!(spans.len(), 2, "both `f`s keep their spans: {spans:?}");

        // A damaged entry is parsed again — only it, and it did not change.
        let mut cache = cache::Store::load(&store);
        let mut entry = cache.get("src/a.rs").unwrap().clone();
        entry.calls.as_mut().expect("a.rs makes calls").sites[1] = 9999;
        cache.insert("src/a.rs".into(), entry);
        cache.save(&store).unwrap();
        let start = parses();
        let healed = build_indexed_warm(&root, Some(&store), files);
        assert_eq!(parses() - start, 1, "only the damaged entry's file");
        assert!(healed.changed.is_empty(), "{:?}", healed.changed);
        assert_eq!(wire(&graph_of(&healed, defs)), wire(&graph));
    }

    fn names(indexed: &Indexed) -> Vec<String> {
        indexed
            .by_file
            .values()
            .flatten()
            .map(|s| s.name.clone())
            .collect()
    }

    #[test]
    fn warm_build_reuses_unchanged_reparses_changed_and_confirms_by_hash() {
        let root = clew_core::testutil::TempDir::new("warm-build");
        let a = root.join("a.rs");
        let b = root.join("b.rs");
        std::fs::write(&a, "pub fn alpha() {}\n").unwrap();
        std::fs::write(&b, "pub fn beta() {}\n").unwrap();
        let files = Arc::new(vec![
            FileEntry {
                abs: a.clone(),
                rel: "a.rs".into(),
            },
            FileEntry {
                abs: b.clone(),
                rel: "b.rs".into(),
            },
        ]);

        // First warm build: no cache yet, so nothing counts as "changed while
        // closed", and the cache file is written.
        // The derived store is clew's own directory, not the project's.
        let store = root.join("derived-store");
        std::fs::create_dir_all(&store).unwrap();
        let i1 = build_indexed_warm(&root, Some(&store), files.clone());
        assert!(names(&i1).contains(&"alpha".to_string()));
        assert!(names(&i1).contains(&"beta".to_string()));
        assert!(i1.changed.is_empty());
        assert!(store.join("index.json").exists());
        assert!(
            !root.join(".clew/cache/index.json").exists(),
            "the derived index must not be written into the project"
        );

        // Edit b: warm build reuses a (unchanged) and re-parses b (flagged).
        std::fs::write(&b, "pub fn beta_renamed() {}\n").unwrap();
        let i2 = build_indexed_warm(&root, Some(&store), files.clone());
        let n2 = names(&i2);
        assert!(n2.contains(&"alpha".to_string())); // reused, still correct
        assert!(n2.contains(&"beta_renamed".to_string())); // re-parsed
        assert!(!n2.contains(&"beta".to_string())); // stale symbol dropped
        assert!(i2.changed.contains(&b));
        assert!(!i2.changed.contains(&a));

        // Rewrite a with identical bytes: mtime changes but the hash confirms the
        // content is unchanged, so a is NOT reported as changed.
        std::fs::write(&a, "pub fn alpha() {}\n").unwrap();
        let i3 = build_indexed_warm(&root, Some(&store), files);
        assert!(!i3.changed.contains(&a));
        assert!(names(&i3).contains(&"alpha".to_string()));
    }

    #[test]
    fn detects_rust_tests_by_attribute_and_go_python_by_name() {
        let src = "\
#[test]
fn t_plain() {}

#[tokio::test]
async fn t_async() {}

/// doc
#[cfg(feature = \"x\")]
#[rstest]
fn t_with_other_attrs() {}

fn not_a_test() {}
";
        let lines: Vec<&str> = src.lines().collect();
        assert!(is_test_fn(&lines, 2, "t_plain", "rust"));
        assert!(is_test_fn(&lines, 5, "t_async", "rust"));
        // Scans up past a doc comment and an unrelated attribute to the #[rstest].
        assert!(is_test_fn(&lines, 10, "t_with_other_attrs", "rust"));
        assert!(!is_test_fn(&lines, 12, "not_a_test", "rust"));
        // Name conventions for Go / Python.
        assert!(is_test_fn(&[], 0, "TestThing", "go"));
        assert!(is_test_fn(&[], 0, "test_thing", "python"));
        assert!(!is_test_fn(&[], 0, "helper", "python"));
    }

    /// The cap counts indexable files only: non-source files sorting first
    /// must not use up the budget, and whatever the cap leaves out is counted
    /// so the UI can say so.
    #[test]
    fn the_index_cap_applies_after_the_language_filter_and_is_reported() {
        let dir = clew_core::testutil::TempDir::new("index-cap-test");
        let mut files = Vec::new();
        // Twenty non-source files first in scan order…
        for i in 0..20 {
            let name = format!("a{i:02}.bin");
            std::fs::write(dir.join(&name), "x").unwrap();
            files.push(FileEntry {
                abs: dir.join(&name),
                rel: name,
            });
        }
        // …then five sources.
        for i in 0..5 {
            let name = format!("s{i}.rs");
            std::fs::write(dir.join(&name), format!("pub fn f{i}() {{}}\n")).unwrap();
            files.push(FileEntry {
                abs: dir.join(&name),
                rel: name,
            });
        }
        let (indexed, _) = build_core_capped(&dir, &files, &cache::Store::default(), 3);
        assert_eq!(indexed.by_file.len(), 3, "three sources indexed, not zero");
        assert_eq!(indexed.over_cap, 2);
        assert!(
            indexed
                .cap_note()
                .unwrap()
                .contains("2 more files not indexed")
        );
        let (all, _) = build_core_capped(&dir, &files, &cache::Store::default(), 100);
        assert_eq!(all.by_file.len(), 5);
        assert_eq!(all.cap_note(), None);
    }

    /// Rust imports extracted for the index are rescoped to the file, so a
    /// test module's `use super::*` never reaches the graph as "depends on
    /// the parent".
    #[test]
    fn rust_file_imports_are_rescoped() {
        let src = "use crate::a::A;\n#[cfg(test)]\nmod tests {\n    use super::*;\n}\n";
        let modules: Vec<String> = file_imports(src, "rust")
            .into_iter()
            .map(|r| r.module)
            .collect();
        assert!(modules.contains(&"crate::a::A".to_string()), "{modules:?}");
        assert!(modules.contains(&"self::*".to_string()), "{modules:?}");
        assert!(!modules.contains(&"super::*".to_string()), "{modules:?}");
    }

    /// I7: one parse yields what the two separate extractions did — the same
    /// symbols (with their test classification) and the same imports — for
    /// every language shape the index sees, and nothing for an unknown one.
    #[test]
    fn one_parse_matches_the_separate_extractions() {
        let cases: [(&str, &'static str, &str); 3] = [
            (
                "src/lib.rs",
                "rust",
                "use crate::a::A;\nmod b;\n#[test]\nfn t() {}\npub fn f() {}\n\
                 #[cfg(test)]\nmod tests {\n    use super::*;\n}\n",
            ),
            (
                "pkg/mod.py",
                "python",
                "import os\nfrom . import views\n\ndef test_x():\n    pass\n\nclass C:\n    def m(self):\n        pass\n",
            ),
            (
                "web/app.ts",
                "typescript",
                "import { x } from './x.js';\nexport function main(): number { return 1; }\n",
            ),
        ];
        for (rel, lang, src) in cases {
            let abs = Path::new("/p").join(rel);
            let facts = analyze_file(&abs, rel, src, lang);
            let lines: Vec<&str> = src.lines().collect();
            let expected: Vec<(String, String, usize, bool)> = outline::extract(src, lang)
                .into_iter()
                .map(|s| {
                    let is_test = matches!(s.kind.as_str(), "function" | "method")
                        && is_test_fn(&lines, s.line, &s.name, lang);
                    (s.name, s.kind, s.line, is_test)
                })
                .collect();
            let got: Vec<(String, String, usize, bool)> = facts
                .symbols
                .iter()
                .map(|s| (s.name.clone(), s.kind.clone(), s.line, s.is_test))
                .collect();
            assert!(!got.is_empty(), "{rel}: no symbols");
            assert_eq!(got, expected, "{rel}");
            assert!(facts.symbols.iter().all(|s| s.rel == rel && s.abs == abs));
            let raw = clew_core::rustscope::scope_imports(
                src,
                lang,
                crate::imports::imports_of(src, lang),
            );
            let key = |v: &[RawImport]| -> Vec<(String, usize, bool)> {
                v.iter()
                    .map(|r| (r.module.clone(), r.line, r.is_mod_decl))
                    .collect()
            };
            assert!(!facts.imports.is_empty(), "{rel}: no imports");
            assert_eq!(key(&facts.imports), key(&raw), "{rel}");
        }
        let none = analyze_file(Path::new("/p/x.klingon"), "x.klingon", "qapla'", "klingon");
        assert!(none.symbols.is_empty() && none.imports.is_empty());
    }

    /// A19: the cap counts only files the index extracts from — the filter
    /// the server's snapshot applies too (`fs_scan::index_language`). Data
    /// files the editor merely colors (JSON, YAML, CSS) used to spend it, so
    /// a tree full of fixtures indexed fewer sources locally than remotely.
    #[test]
    fn data_files_cost_no_index_budget() {
        let dir = clew_core::testutil::TempDir::new("index-data-files");
        let mut files = Vec::new();
        for rel in [
            "fixtures/a.json",
            "fixtures/b.yaml",
            "fixtures/c.css",
            "src/a.rs",
            "src/b.rs",
        ] {
            let abs = dir.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, "pub fn f() {}\n").unwrap();
            files.push(FileEntry {
                abs,
                rel: rel.to_string(),
            });
        }
        let (indexed, _) = build_core_capped(&dir, &files, &cache::Store::default(), 2);
        let mut indexed_files: Vec<&PathBuf> = indexed.by_file.keys().collect();
        indexed_files.sort();
        assert_eq!(
            indexed_files,
            [&dir.join("src/a.rs"), &dir.join("src/b.rs")]
        );
        assert_eq!(indexed.over_cap, 0, "a data file counted against the cap");
        let counted: Vec<&str> = files
            .iter()
            .filter(|f| clew_core::fs_scan::index_language(&f.rel, &f.abs).is_some())
            .map(|f| f.rel.as_str())
            .collect();
        assert_eq!(counted, ["src/a.rs", "src/b.rs"]);
    }

    /// I7: files under a noise directory (a vendored `node_modules`, build
    /// output, clew's own `.clew/`) are never indexed and never count against
    /// the cap, even when a file list hands them over.
    #[test]
    fn noise_directories_are_not_indexed_and_cost_no_budget() {
        let dir = clew_core::testutil::TempDir::new("index-noise");
        let mut files = Vec::new();
        for rel in [
            "node_modules/dep/index.js",
            "target/debug/build.rs",
            ".clew/launch.json",
            "src/node_modules_helper.rs",
            "src/lib.rs",
        ] {
            let abs = dir.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, "pub fn f() {}\nfunction g() {}\n").unwrap();
            files.push(FileEntry {
                abs,
                rel: rel.to_string(),
            });
        }
        // A cap of two: only the two real sources may use it.
        let (indexed, _) = build_core_capped(&dir, &files, &cache::Store::default(), 2);
        let mut rels: Vec<String> = indexed
            .by_file
            .values()
            .flatten()
            .map(|s| s.rel.clone())
            .collect();
        rels.sort();
        rels.dedup();
        assert_eq!(rels, ["src/lib.rs", "src/node_modules_helper.rs"]);
        assert_eq!(indexed.over_cap, 0, "noise must not count against the cap");
        assert!(
            indexed
                .hashes
                .iter()
                .all(|(p, _)| !p.to_string_lossy().contains("node_modules/")),
            "a noise file was read"
        );
    }

    #[test]
    fn builds_symbols_for_supported_files_only() {
        let dir = clew_core::testutil::TempDir::new("index-test");
        std::fs::write(dir.join("lib.rs"), "pub fn origin() -> f64 {\n    0.0\n}\n").unwrap();
        std::fs::write(dir.join("data.json"), "{\"a\": 1}").unwrap();

        let files = Arc::new(vec![
            FileEntry {
                abs: dir.join("lib.rs"),
                rel: "lib.rs".into(),
            },
            FileEntry {
                abs: dir.join("data.json"),
                rel: "data.json".into(),
            },
        ]);
        let entries = build(&dir, files);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "origin");
        assert_eq!(entries[0].kind, "function");
        assert_eq!(entries[0].rel, "lib.rs");
        assert_eq!(entries[0].line, 1);
    }
}
