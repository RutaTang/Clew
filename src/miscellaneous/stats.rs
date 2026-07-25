//! Code statistics — lines of code by language, computed with the `tokei`
//! library (embedded, so no external binary is required).
//!
//! Like `overview`, this is a derived artifact cached under `.clew/cache`. The
//! cache is keyed by the change-detection registry's revision, so it is served
//! instantly on reopen and recomputed only after files were created, deleted,
//! or edited. The computation is CPU-bound (it walks and parses the tree) and
//! must run off the UI thread via `spawn_blocking`.

use std::path::{Path, PathBuf};

/// tokei-style ignore patterns (`.gitignore` syntax) layered on top of the
/// project's own `.gitignore`, which tokei already honours. `.clew` is clew's
/// own data dir in the target project and is not necessarily git-ignored there.
const EXCLUDED: &[&str] = &[".git", "target", "node_modules", ".clew"];

/// How many of the largest files to surface.
const TOP_FILES: usize = 12;

/// Project-wide totals across every counted language.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Totals {
    pub files: usize,
    pub code: usize,
    pub comments: usize,
    pub blanks: usize,
}

impl Totals {
    pub fn lines(&self) -> usize {
        self.code + self.comments + self.blanks
    }
}

/// One language's aggregate counts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LangStat {
    /// Display name, e.g. "Rust", "TypeScript".
    pub name: String,
    pub files: usize,
    pub code: usize,
    pub comments: usize,
    pub blanks: usize,
}

impl LangStat {
    pub fn lines(&self) -> usize {
        self.code + self.comments + self.blanks
    }
}

/// One file in the "largest files" list. The path is stored relative to the
/// project root so the cache stays portable; the absolute path is rebuilt as
/// `root.join(rel)` when the row is opened.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileStat {
    pub rel: PathBuf,
    pub lang: String,
    pub code: usize,
    pub lines: usize,
}

/// The full statistics report rendered by the Stats view.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StatsReport {
    pub totals: Totals,
    /// Languages sorted by code lines, descending.
    pub langs: Vec<LangStat>,
    /// Largest files by total lines, descending, capped at `TOP_FILES`.
    pub top_files: Vec<FileStat>,
}

impl StatsReport {
    pub fn is_empty(&self) -> bool {
        self.langs.is_empty()
    }
}

/// A persisted report plus the registry revision it was computed at, so a
/// changed file set (or edited file) misses the cache.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cached {
    pub report: StatsReport,
    pub rev: u64,
}

fn cache_path(root: &Path) -> PathBuf {
    root.join(".clew").join("cache").join("stats.json")
}

/// Load the persisted stats (None on any error / not yet computed).
pub fn load(root: &Path) -> Option<Cached> {
    std::fs::read_to_string(cache_path(root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// Persist the stats (atomic temp+rename).
pub fn save(root: &Path, cached: &Cached) -> std::io::Result<()> {
    let path = cache_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(cached).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// Walk `root` and count lines by language. Blocking (CPU-bound); run via
/// `spawn_blocking`. Per-language and total counts are summed from the same
/// per-file reports we surface, so the totals, the language bar, and the file
/// list are always internally consistent.
pub fn compute(root: &Path) -> StatsReport {
    let config = tokei::Config::default();
    let mut languages = tokei::Languages::new();
    languages.get_statistics(&[root], EXCLUDED, &config);

    let mut totals = Totals::default();
    let mut langs: Vec<LangStat> = Vec::new();
    let mut files: Vec<FileStat> = Vec::new();

    for (lang_type, lang) in languages {
        if lang.reports.is_empty() {
            continue;
        }
        let name = lang_type.name().to_string();
        let mut lang_stat = LangStat {
            name: name.clone(),
            files: lang.reports.len(),
            code: 0,
            comments: 0,
            blanks: 0,
        };
        for report in &lang.reports {
            let s = &report.stats;
            lang_stat.code += s.code;
            lang_stat.comments += s.comments;
            lang_stat.blanks += s.blanks;
            let lines = s.code + s.comments + s.blanks;
            let rel = report
                .name
                .strip_prefix(root)
                .unwrap_or(&report.name)
                .to_path_buf();
            files.push(FileStat {
                rel,
                lang: name.clone(),
                code: s.code,
                lines,
            });
        }
        totals.files += lang_stat.files;
        totals.code += lang_stat.code;
        totals.comments += lang_stat.comments;
        totals.blanks += lang_stat.blanks;
        langs.push(lang_stat);
    }

    // Languages by code (desc), then name for a stable tie-break.
    langs.sort_by(|a, b| b.code.cmp(&a.code).then_with(|| a.name.cmp(&b.name)));
    // Largest files by total lines (desc), then path for a stable tie-break.
    files.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.rel.cmp(&b.rel)));
    files.truncate(TOP_FILES);

    StatsReport {
        totals,
        langs,
        top_files: files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_counts_by_language_and_ranks_files() {
        let dir = std::env::temp_dir().join("clew-stats-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        // A big Rust file, a small Rust file, and a Python file.
        std::fs::write(
            dir.join("src/big.rs"),
            "// a comment\nfn a() {}\nfn b() {}\n\nfn c() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/small.rs"), "fn tiny() {}\n").unwrap();
        std::fs::write(dir.join("app.py"), "# c\nprint('hi')\n").unwrap();

        let report = compute(&dir);

        // Two languages, ranked with Rust (more code) first.
        assert_eq!(report.langs.len(), 2);
        assert_eq!(report.langs[0].name, "Rust");
        assert_eq!(report.langs[0].files, 2);
        assert!(report.langs[0].comments >= 1, "the comment line is counted");

        // Totals are the sum across languages.
        let summed: usize = report.langs.iter().map(|l| l.code).sum();
        assert_eq!(report.totals.code, summed);
        assert_eq!(report.totals.files, 3);

        // The largest file is ranked first and its path is project-relative.
        assert_eq!(report.top_files[0].rel, PathBuf::from("src/big.rs"));
        assert!(report.top_files[0].lines >= report.top_files[1].lines);
        assert!(!report.top_files[0].rel.is_absolute());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
