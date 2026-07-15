//! Project-wide symbol index, built in the background after a scan.

use std::path::PathBuf;
use std::sync::Arc;

use crate::fs_scan::FileEntry;
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

/// Extract definition symbols from every supported file.
/// Blocking; run off the UI thread.
pub fn build(files: Arc<Vec<FileEntry>>) -> Vec<SymbolEntry> {
    let mut entries = Vec::new();
    for file in files.iter().take(MAX_INDEX_FILES) {
        let Some(lang) = highlight::detect(&file.abs) else {
            continue;
        };
        if highlight::tags_for(lang).is_none() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&file.abs) else {
            continue;
        };
        if meta.len() > MAX_INDEX_FILE_BYTES {
            continue;
        }
        // Skips non-UTF-8 (and thus binary) files.
        let Ok(content) = std::fs::read_to_string(&file.abs) else {
            continue;
        };
        for symbol in outline::extract(&content, lang) {
            entries.push(SymbolEntry {
                name: symbol.name,
                kind: symbol.kind,
                rel: file.rel.clone(),
                abs: file.abs.clone(),
                line: symbol.line,
            });
        }
    }
    entries
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
