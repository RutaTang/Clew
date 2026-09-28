//! Code statistics — lines of code by language, counted with the `tokei`
//! library's per-language parsers (embedded, so no external binary is
//! required).
//!
//! Lives in clew-core so the computation runs where the files live: the
//! client calls [`compute`] directly for a local project, and clew-server
//! answers the `Stats` request with it for a remote one (the report types are
//! clew-protocol's, re-exported here).
//!
//! The files counted are the SCANNER's ([`crate::fs_scan`]): the same
//! `.gitignore` handling, the same pruned noise directories, the same
//! symlink refusal as the tree the reader sees. Tokei's own directory walker
//! is not used — it kept a separate (and drifted) exclusion list, followed no
//! confinement rules, and read every file whole, so one multi-gigabyte `.sql`
//! dump in a remote tree was read into memory on every Stats request. Each
//! file here is read through the scanner's confined, size-capped read, and
//! the ones that are too large or unreadable are counted in
//! [`StatsReport::skipped`] rather than silently left out.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokei::LanguageType;

use crate::fs_scan::FileEntry;

/// How many of the largest files to surface.
const TOP_FILES: usize = 12;

/// Largest file counted. Far above any hand-written source file; what lies
/// beyond it is generated data (dumps, bundles, fixtures) whose line count
/// says nothing about the code base.
pub const MAX_STATS_FILE_BYTES: u64 = 8 * 1024 * 1024;

// The report types are protocol wire types (clew-server answers `Stats` with
// one), defined in clew-protocol so the build fingerprint covers their shape;
// the counting that fills them in stays here.
pub use clew_protocol::{FileStat, LangStat, StatsReport, Totals};

/// Scan `root` and count lines by language. Blocking (I/O + CPU-bound); run
/// via `spawn_blocking`.
pub fn compute(root: &Path) -> StatsReport {
    let scan = crate::fs_scan::scan(root.to_path_buf());
    compute_files(root, &scan.files)
}

/// Count lines by language over an existing scan's `files` (every read
/// confined to `root` and capped at [`MAX_STATS_FILE_BYTES`]). Per-language
/// and total counts are summed from the same per-file counts the file list is
/// built from, so the totals, the language bar, and the file list are always
/// internally consistent.
pub fn compute_files(root: &Path, files: &[FileEntry]) -> StatsReport {
    compute_files_capped(root, files, MAX_STATS_FILE_BYTES)
}

/// One counted file.
struct Counted {
    lang: LanguageType,
    rel: String,
    code: usize,
    comments: usize,
    blanks: usize,
}

fn compute_files_capped(root: &Path, files: &[FileEntry], max_bytes: u64) -> StatsReport {
    let config = tokei::Config::default();
    let next = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let counted: Mutex<Vec<Counted>> = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let mut mine = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(i) else { break };
                    let Some(lang) = language_of(&file.abs) else {
                        continue;
                    };
                    let Some(bytes) =
                        crate::fs_scan::read_confined_bytes_capped(root, &file.abs, max_bytes)
                    else {
                        skipped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let Some(lang) = lang.or_else(|| language_from_shebang(&bytes)) else {
                        continue;
                    };
                    let stats = lang.parse_from_slice(&bytes, &config);
                    mine.push(Counted {
                        lang,
                        rel: file.rel.clone(),
                        code: stats.code,
                        comments: stats.comments,
                        blanks: stats.blanks,
                    });
                }
                counted
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(mine);
            });
        }
    });
    let counted = counted.into_inner().unwrap_or_else(|e| e.into_inner());

    let mut by_lang: HashMap<LanguageType, LangStat> = HashMap::new();
    let mut top: Vec<FileStat> = Vec::with_capacity(counted.len());
    for c in counted {
        let name = c.lang.name().to_string();
        let lang = by_lang.entry(c.lang).or_insert_with(|| LangStat {
            name: name.clone(),
            files: 0,
            code: 0,
            comments: 0,
            blanks: 0,
        });
        lang.files += 1;
        lang.code += c.code;
        lang.comments += c.comments;
        lang.blanks += c.blanks;
        top.push(FileStat {
            rel: c.rel,
            lang: name,
            code: c.code,
            lines: c.code + c.comments + c.blanks,
        });
    }

    let mut langs: Vec<LangStat> = by_lang.into_values().collect();
    let mut totals = Totals::default();
    for l in &langs {
        totals.files += l.files;
        totals.code += l.code;
        totals.comments += l.comments;
        totals.blanks += l.blanks;
    }
    // Languages by code (desc), then name for a stable tie-break.
    langs.sort_by(|a, b| b.code.cmp(&a.code).then_with(|| a.name.cmp(&b.name)));
    // Largest files by total lines (desc), then path for a stable tie-break.
    top.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.rel.cmp(&b.rel)));
    top.truncate(TOP_FILES);

    StatsReport {
        totals,
        langs,
        top_files: top,
        skipped: skipped.into_inner(),
    }
}

/// The tokei language of `path` from its NAME alone: `Some(Some(lang))` for a
/// known filename or extension, `Some(None)` for a file without an extension
/// (decided by its shebang once read), `None` for a file tokei does not count.
///
/// Tokei's own `from_path` falls back to opening the file to read a shebang —
/// by path, following symlinks, blocking on a FIFO, reading an unbounded first
/// line — so it is handed a path that cannot be opened (same name, nonexistent
/// directory) and the shebang is read from the capped bytes instead.
fn language_of(path: &Path) -> Option<Option<LanguageType>> {
    let name = path.file_name()?;
    let probe = Path::new("/nonexistent-clew-stats-probe").join(name);
    if let Some(lang) = LanguageType::from_path(&probe, &tokei::Config::default()) {
        return Some(Some(lang));
    }
    path.extension().is_none().then_some(None)
}

/// The language a `#!` line names, for extension-less scripts: the common
/// interpreters, mapped onto the extension tokei keys each language by.
fn language_from_shebang(bytes: &[u8]) -> Option<LanguageType> {
    let first = bytes.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(first.get(..first.len().min(256))?).ok()?;
    let mut words = line.strip_prefix("#!")?.split_whitespace();
    let mut interpreter = words.next()?.rsplit('/').next()?;
    if interpreter == "env" {
        interpreter = words.find(|w| !w.starts_with('-'))?;
    }
    let ext = match interpreter.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.') {
        "python" => "py",
        "sh" | "dash" | "ksh" => "sh",
        "bash" => "bash",
        "zsh" => "zsh",
        "fish" => "fish",
        "node" | "nodejs" => "js",
        "deno" | "ts-node" => "ts",
        "ruby" => "rb",
        "perl" => "pl",
        "php" => "php",
        "lua" => "lua",
        "tclsh" => "tcl",
        _ => return None,
    };
    LanguageType::from_file_extension(ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn compute_counts_by_language_and_ranks_files() {
        let dir = TempDir::new("stats-count");
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
        assert_eq!(report.top_files[0].rel, "src/big.rs");
        assert!(report.top_files[0].lines >= report.top_files[1].lines);
        assert!(!Path::new(&report.top_files[0].rel).is_absolute());
        assert_eq!(report.skipped, 0);
    }

    /// A file past the cap is never read whole — it is skipped and counted,
    /// and the rest of the report is unaffected.
    #[test]
    fn an_over_cap_file_is_skipped_not_read() {
        let dir = TempDir::new("stats-cap");
        std::fs::write(
            dir.join("dump.sql"),
            "INSERT INTO t VALUES (1);\n".repeat(100),
        )
        .unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        let files = crate::fs_scan::scan(dir.to_path_buf()).files;
        let report = compute_files_capped(&dir, &files, 1024);
        assert_eq!(report.skipped, 1);
        assert_eq!(
            report
                .langs
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>(),
            ["Rust"]
        );
        // Under the real cap the dump is counted.
        assert!(compute(&dir).langs.iter().any(|l| l.name == "SQL"));
    }

    /// The same noise directories as the file tree, and no symlinks: the
    /// counts describe the project the reader sees.
    #[test]
    fn counts_follow_the_scanners_rules() {
        let dir = TempDir::new("stats-noise");
        for noise in [
            "venv/lib",
            "__pycache__",
            ".dart_tool",
            "node_modules/x",
            "target",
        ] {
            std::fs::create_dir_all(dir.join(noise)).unwrap();
            std::fs::write(dir.join(noise).join("junk.py"), "x = 1\n").unwrap();
        }
        std::fs::write(dir.join("real.py"), "x = 1\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", dir.join("hosts.py")).unwrap();
        let report = compute(&dir);
        assert_eq!(report.totals.files, 1, "{:?}", report.top_files);
        assert_eq!(report.top_files[0].rel, "real.py");
    }

    #[test]
    fn extensionless_scripts_are_classified_by_their_shebang() {
        assert_eq!(
            language_from_shebang(b"#!/usr/bin/env python3\nprint(1)\n"),
            Some(LanguageType::Python)
        );
        assert_eq!(
            language_from_shebang(b"#!/bin/bash\necho\n"),
            Some(LanguageType::Bash)
        );
        assert_eq!(language_from_shebang(b"no shebang\n"), None);
        assert!(language_of(Path::new("/p/Makefile")).is_some_and(|l| l.is_some()));
        assert_eq!(language_of(Path::new("/p/script")), Some(None));
        assert_eq!(language_of(Path::new("/p/data.unknownext")), None);
    }
}
