//! Project-wide symbol index, built in the background after a scan and kept
//! incrementally fresh per file as the codebase changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::fs_scan::FileEntry;
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
}

/// Result of the initial background pass: symbols grouped by file (so a single
/// file can be re-indexed in place) plus each file's content hash (so the
/// incremental registry is seeded from the same single read of the tree).
#[derive(Debug, Default, Clone)]
pub struct Indexed {
    pub by_file: HashMap<PathBuf, Vec<SymbolEntry>>,
    pub hashes: Vec<(PathBuf, Version)>,
}

/// Definition symbols for one already-read file's `content`.
pub fn file_symbols(abs: &Path, rel: &str, content: &str, lang: &'static str) -> Vec<SymbolEntry> {
    if highlight::tags_for(lang).is_none() {
        return Vec::new();
    }
    outline::extract(content, lang)
        .into_iter()
        .map(|symbol| SymbolEntry {
            name: symbol.name,
            kind: symbol.kind,
            rel: rel.to_string(),
            abs: abs.to_path_buf(),
            line: symbol.line,
        })
        .collect()
}

/// Index every supported file once, returning per-file symbols and hashes.
/// Blocking; run off the UI thread. Reading each file a single time seeds both
/// the symbol index and the change-detection registry.
pub fn build_indexed(files: Arc<Vec<FileEntry>>) -> Indexed {
    let mut out = Indexed::default();
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
        let Ok(bytes) = std::fs::read(&file.abs) else {
            continue;
        };
        out.hashes.push((file.abs.clone(), content_hash(&bytes)));
        // Skips non-UTF-8 (and thus binary) files for symbol extraction.
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        let syms = file_symbols(&file.abs, &file.rel, &content, lang);
        if !syms.is_empty() {
            out.by_file.insert(file.abs.clone(), syms);
        }
    }
    out
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
