//! Project-wide symbol index, built in the background after a scan and kept
//! incrementally fresh per file as the codebase changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::cache::{self, CachedImport, CachedSymbol, FileCache};
use crate::fs_scan::FileEntry;
use crate::imports::{self, RawImport};
use crate::incremental::{Version, content_hash};
use crate::{highlight, outline};

/// Caps keeping index build time bounded on huge repos.
pub const MAX_INDEX_FILES: usize = 20_000;
pub const MAX_INDEX_FILE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub name: String,
    pub kind: String,
    pub rel: String,
    pub abs: PathBuf,
    pub line: usize, // 1-based
    /// True when this function/method is a test (see [`is_test_fn`]).
    pub is_test: bool,
}

/// Whether the function/method named `name` at 1-based `line1` in `lines` (the
/// file split into lines) is a test. Rust: a `#[…test…]` attribute above the
/// definition (`#[test]`, `#[tokio::test]`, `#[rstest]`, `#[test_case(…)]`, …).
/// Go/Python: the standard test-name convention. Pure text, no tree-sitter.
pub fn is_test_fn(lines: &[&str], line1: usize, name: &str, lang: &str) -> bool {
    match lang {
        "rust" => {
            if line1 == 0 || line1 > lines.len() {
                return false;
            }
            // Scan upward over attributes, doc-comments and blank lines; a test
            // attribute anywhere in that run marks it. Stop at the first real line.
            let mut i = line1 - 1; // 0-based index of the definition line
            while i > 0 {
                i -= 1;
                let t = lines[i].trim();
                if t.is_empty() || t.starts_with("//") || t.starts_with("#!") {
                    continue;
                }
                if let Some(rest) = t.strip_prefix("#[") {
                    if rest.contains("test") {
                        return true;
                    }
                    continue; // another attribute (e.g. #[cfg(...)]) — keep scanning
                }
                break; // a code line — the attribute run has ended
            }
            false
        }
        "go" => {
            name.starts_with("Test") || name.starts_with("Benchmark") || name.starts_with("Fuzz")
        }
        "python" => name.starts_with("test") || name.starts_with("Test"),
        _ => false,
    }
}

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
    pub hashes: Vec<(PathBuf, Version)>,
    pub changed: Vec<PathBuf>,
}

/// Definition symbols for one already-read file's `content`.
pub fn file_symbols(abs: &Path, rel: &str, content: &str, lang: &'static str) -> Vec<SymbolEntry> {
    if highlight::tags_for(lang).is_none() {
        return Vec::new();
    }
    let lines: Vec<&str> = content.lines().collect();
    outline::extract(content, lang)
        .into_iter()
        .map(|symbol| {
            let is_test = matches!(symbol.kind.as_str(), "function" | "method")
                && is_test_fn(&lines, symbol.line, &symbol.name, lang);
            SymbolEntry {
                name: symbol.name,
                kind: symbol.kind,
                rel: rel.to_string(),
                abs: abs.to_path_buf(),
                line: symbol.line,
                is_test,
            }
        })
        .collect()
}

/// Raw imports for one already-read file's `content` (unresolved).
pub fn file_imports(content: &str, lang: &'static str) -> Vec<RawImport> {
    imports::imports_of(content, lang)
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

/// Cold index build (no cache): read + parse every supported file. Blocking.
/// Retained for tests; the runtime always warm-starts via [`build_indexed_warm`].
#[cfg(test)]
pub fn build_indexed(files: Arc<Vec<FileEntry>>) -> Indexed {
    build_core(&files, &cache::Store::default()).0
}

/// Warm index build: reuse the persistent cache for files confirmed unchanged
/// (mtime+size fast path, content-hash fallback), re-parsing only what changed
/// while clew was closed, then persist the refreshed cache. Blocking.
pub fn build_indexed_warm(root: &Path, files: Arc<Vec<FileEntry>>) -> Indexed {
    let old = cache::Store::load(root);
    let (indexed, fresh) = build_core(&files, &old);
    // Best-effort persist; a failure only means a colder start next time.
    let _ = fresh.save(root);
    indexed
}

/// Shared index build. Reuses a file's cached hash + symbols only after
/// confirming its content is unchanged; otherwise reads, hashes, and (on a real
/// change) re-parses. Returns the index plus the freshly rebuilt cache.
fn build_core(files: &[FileEntry], old: &cache::Store) -> (Indexed, cache::Store) {
    let mut indexed = Indexed::default();
    let mut fresh = cache::Store::default();
    for file in files.iter().take(MAX_INDEX_FILES) {
        let Some(lang) = highlight::detect(&file.abs) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&file.abs) else {
            continue;
        };
        if meta.len() > MAX_INDEX_FILE_BYTES {
            continue;
        }
        let size = meta.len();
        let mtime = mtime_ns(&meta);
        let cached = old.get(&file.rel);

        let entry: FileCache = if let Some(c) = cached
            && mtime != 0
            && c.mtime_ns == mtime
            && c.size == size
        {
            // Fast path: stat says unchanged — reuse cached hash + symbols.
            c.clone()
        } else {
            let Ok(bytes) = std::fs::read(&file.abs) else {
                continue;
            };
            let hash = content_hash(&bytes);
            if let Some(c) = cached
                && c.hash == hash
            {
                // Content is identical; only the mtime changed. Reuse the derived
                // artifacts, refresh the stat metadata so the fast path hits next.
                FileCache {
                    mtime_ns: mtime,
                    size,
                    hash,
                    symbols: c.symbols.clone(),
                    imports: c.imports.clone(),
                }
            } else {
                // Genuinely changed (or new): re-parse. A prior cache entry with
                // a different hash means it changed while clew was closed.
                if cached.is_some() {
                    indexed.changed.push(file.abs.clone());
                }
                let (symbols, cached_imports) = match String::from_utf8(bytes) {
                    Ok(content) => {
                        let syms = file_symbols(&file.abs, &file.rel, &content, lang)
                            .into_iter()
                            .map(|s| CachedSymbol {
                                name: s.name,
                                kind: s.kind,
                                line: s.line,
                                is_test: s.is_test,
                            })
                            .collect();
                        let imps = to_cached_imports(&file_imports(&content, lang));
                        (syms, imps)
                    }
                    Err(_) => (Vec::new(), Vec::new()), // binary — nothing to extract
                };
                FileCache {
                    mtime_ns: mtime,
                    size,
                    hash,
                    symbols,
                    imports: cached_imports,
                }
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
                })
                .collect();
            indexed.by_file.insert(file.abs.clone(), syms);
        }
        if !entry.imports.is_empty() {
            indexed
                .imports_by_file
                .insert(file.abs.clone(), from_cached_imports(&entry.imports));
        }
        fresh.insert(file.rel.clone(), entry);
    }
    (indexed, fresh)
}

/// Flatten per-file symbols into one list (stable order by path then line) for
/// the fuzzy symbol finder.
pub fn flatten(by_file: &HashMap<PathBuf, Vec<SymbolEntry>>) -> Vec<SymbolEntry> {
    let mut files: Vec<&PathBuf> = by_file.keys().collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        out.extend(by_file[f].iter().cloned());
    }
    out
}

/// Extract definition symbols from every supported file (flat list). Retained
/// for tests and callers that don't need the per-file grouping.
#[cfg(test)]
pub fn build(files: Arc<Vec<FileEntry>>) -> Vec<SymbolEntry> {
    flatten(&build_indexed(files).by_file)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let root = std::env::temp_dir().join("clew-warm-build");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let a = root.join("a.rs");
        let b = root.join("b.rs");
        std::fs::write(&a, "pub fn alpha() {}\n").unwrap();
        std::fs::write(&b, "pub fn beta() {}\n").unwrap();
        let files = Arc::new(vec![
            FileEntry { abs: a.clone(), rel: "a.rs".into() },
            FileEntry { abs: b.clone(), rel: "b.rs".into() },
        ]);

        // First warm build: no cache yet, so nothing counts as "changed while
        // closed", and the cache file is written.
        let i1 = build_indexed_warm(&root, files.clone());
        assert!(names(&i1).contains(&"alpha".to_string()));
        assert!(names(&i1).contains(&"beta".to_string()));
        assert!(i1.changed.is_empty());
        assert!(root.join(".clew/cache/index.json").exists());

        // Edit b: warm build reuses a (unchanged) and re-parses b (flagged).
        std::fs::write(&b, "pub fn beta_renamed() {}\n").unwrap();
        let i2 = build_indexed_warm(&root, files.clone());
        let n2 = names(&i2);
        assert!(n2.contains(&"alpha".to_string())); // reused, still correct
        assert!(n2.contains(&"beta_renamed".to_string())); // re-parsed
        assert!(!n2.contains(&"beta".to_string())); // stale symbol dropped
        assert!(i2.changed.contains(&b));
        assert!(!i2.changed.contains(&a));

        // Rewrite a with identical bytes: mtime changes but the hash confirms the
        // content is unchanged, so a is NOT reported as changed.
        std::fs::write(&a, "pub fn alpha() {}\n").unwrap();
        let i3 = build_indexed_warm(&root, files);
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

    #[test]
    fn builds_symbols_for_supported_files_only() {
        let dir = std::env::temp_dir().join("clew-index-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("lib.rs"),
            "pub fn origin() -> f64 {\n    0.0\n}\n",
        )
        .unwrap();
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
        let entries = build(files);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "origin");
        assert_eq!(entries[0].kind, "function");
        assert_eq!(entries[0].rel, "lib.rs");
        assert_eq!(entries[0].line, 1);
    }
}
