//! Project scanning: builds the directory tree and the flat file list,
//! honoring `.gitignore` (via the `ignore` crate) — except where honoring it
//! would hide what the repository really contains (see [`scan_with_report`]).

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ignore::WalkBuilder;

use crate::statefile::ReadError;

/// Hard cap on scanned entries to keep giant repos responsive.
pub const MAX_ENTRIES: usize = 100_000;

/// How deeply the emitted [`DirNode`] tree may nest before the rest of a path
/// is folded into the row's own name.
///
/// Nesting is repository-controlled and was unbounded: `MAX_ENTRIES` bounds how
/// many entries are kept, not how deep they sit, since 45 nested directories
/// are only 45 entries. Every frame crosses the transport as NDJSON — including
/// a LOCAL project, whose server is a child process over stdio — and
/// `serde_json`'s deserializer refuses more than 128 nested containers. One
/// directory level costs three of them (`dirs` array, tuple array, child
/// object), so past ~41 levels the client cannot parse the `Tree` frame AT ALL,
/// however small it is: it drops the link as unreadable and respawns into the
/// identical frame, so the project can never be opened. No node is deeper than
/// this, which is well inside that ceiling and inside the stack that `convert`
/// (and `DirNode`'s recursive drop) can afford.
pub const MAX_TREE_DEPTH: usize = 32;

/// How many files a pass that PARSES project files takes on (the symbol
/// index, the type-structure pass, the server's symbol snapshot). One value
/// for all of them — they used to carry four copies — and applied AFTER the
/// pass has filtered the list down to the languages it parses: capping first
/// dropped whatever sorted late (every `src/` Rust file of a big polyglot
/// tree) while spending the budget on files the pass then skipped.
pub const MAX_INDEX_FILES: usize = 20_000;

/// The per-file byte cap that goes with [`MAX_INDEX_FILES`].
pub const MAX_INDEX_FILE_BYTES: u64 = 512 * 1024;

/// The language the symbol indexers read `rel` (at `abs`) as, or `None` when
/// they skip it: a language with an outline query (what an index extracts),
/// outside the directories no reader indexes ([`NOISE_DIRS`]).
///
/// The ONE filter the client's index, the server's symbol snapshot and its
/// call graph apply — before [`MAX_INDEX_FILES`], so a local and a remote
/// project count the same files against the cap. The client used to count
/// every language it can color (JSON, YAML, CSS…), so a tree heavy in data
/// files spent the cap on files with nothing to index and left sources out
/// that the server indexed; the server did not skip the noise directories.
pub fn index_language(rel: &str, abs: &Path) -> Option<&'static str> {
    let in_noise_dir = Path::new(rel)
        .parent()
        .is_some_and(|dir| dir.components().any(|c| is_noise_dir(c.as_os_str())));
    if in_noise_dir {
        return None;
    }
    let lang = crate::highlight::detect(abs)?;
    crate::highlight::tags_for(lang).map(|_| lang)
}

/// Directories a code reader should never scan: clew/git internals, and the
/// build-output / vendored-dependency dirs of the supported languages. Skipped
/// unconditionally (even without a `.gitignore`, which many projects lack) so
/// the file tree and symbol index stay about the reader's own source — e.g. a
/// TS project's `node_modules` `.d.ts` files would otherwise flood the index.
///
/// The ONE list: statistics, which used to keep its own (and had drifted —
/// no `venv`, `__pycache__` or `.dart_tool`), now counts this scan's files.
pub const NOISE_DIRS: &[&str] = &[
    ".git",
    ".clew",
    "node_modules",
    "target",
    ".dart_tool",
    ".venv",
    "venv",
    "__pycache__",
];

/// Whether a directory with this name is one of [`NOISE_DIRS`].
pub fn is_noise_dir(name: &OsStr) -> bool {
    name.to_str().is_some_and(|n| NOISE_DIRS.contains(&n))
}

/// Files inside the (otherwise pruned) `.clew/` that are shown anyway: the
/// repository's own configuration of what clew RUNS — language-server
/// commands and their options, debugger launch configurations. They ship with
/// the repository and decide what executes, so a reader must be able to see
/// and open them like any other file.
pub const VISIBLE_CLEW_FILES: &[&str] = &["lsp.toml", "launch.json"];

// The directory tree is a protocol wire type: the scanner builds it here, the
// server transmits it, the client renders it verbatim.
pub use clew_protocol::DirNode;

/// A file known to the project, with absolute path and root-relative display path.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub abs: PathBuf,
    pub rel: String,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub root: PathBuf,
    pub tree: DirNode,
    pub files: Vec<FileEntry>,
    pub truncated: bool,
}

/// What a scan left out or added beyond the plain `.gitignore` walk, so the
/// caller can say so instead of silently showing less.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Entries skipped because their name is not valid UTF-8. A rel is a
    /// `String` everywhere downstream (the wire, the stores, the finder), and
    /// a lossily converted name points at no file at all — so such entries
    /// are left out, and counted.
    pub non_utf8: usize,
    /// Directory entries the walk could not read (permission denied, a
    /// directory that vanished mid-walk, …).
    pub walk_errors: usize,
    /// The first of those errors, for the log line.
    pub first_error: Option<String>,
    /// Files git TRACKS that the walk left out — hidden by the ignore rules,
    /// or inside a nested directory with a build-output name — and which are
    /// therefore in the list anyway (root-relative).
    pub tracked_ignored: Vec<String>,
    /// Why the tracked files could not be listed, when git was asked and
    /// failed (it timed out, the repository's configuration was refused, …).
    /// The scan then holds only what the walk found — which may be missing
    /// files the repository contains, and the reader must be told so.
    pub tracked_error: Option<String>,
}

impl ScanReport {
    /// One line describing what the scan skipped or added, or `None` when it
    /// was a plain walk.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.non_utf8 > 0 {
            parts.push(format!(
                "{} entr{} skipped (name is not UTF-8)",
                self.non_utf8,
                if self.non_utf8 == 1 { "y" } else { "ies" }
            ));
        }
        if self.walk_errors > 0 {
            parts.push(format!(
                "{} unreadable entr{}{}",
                self.walk_errors,
                if self.walk_errors == 1 { "y" } else { "ies" },
                self.first_error
                    .as_deref()
                    .map(|e| format!(" (first: {e})"))
                    .unwrap_or_default()
            ));
        }
        if !self.tracked_ignored.is_empty() {
            parts.push(format!(
                "{} tracked file{} shown despite .gitignore or a build-directory name",
                self.tracked_ignored.len(),
                if self.tracked_ignored.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if let Some(e) = &self.tracked_error {
            parts.push(format!(
                "git could not list the tracked files, so ignored ones may be missing ({e})"
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

#[derive(Default)]
struct TmpDir {
    dirs: BTreeMap<String, TmpDir>,
    files: Vec<String>,
}

impl TmpDir {
    /// Insert an entry by its components. A file below [`MAX_TREE_DEPTH`]
    /// becomes a row OF the deepest node, named by its whole remaining path:
    /// the client builds each row's rel by joining names with `/`, so the row
    /// still opens the same file, and the slashes in the name are what tell
    /// the reader the nesting below this point was folded away.
    ///
    /// A folded directory gets no row of its own. Giving each one a row would
    /// repeat every ancestor's name inside every descendant's, so a single
    /// deep chain would cost bytes quadratic in its length — the frame is what
    /// we are trying to keep sendable. The cost is that a directory nested
    /// past the cap and holding no file anywhere below it has no row at all;
    /// it would have rendered as an empty folder. Files are never lost: the
    /// flat file list carries every rel in full, so search, docs and the agent
    /// are unaffected.
    fn insert(&mut self, comps: &[&str], is_dir: bool) {
        let mut node = self;
        for (i, name) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            if last && !is_dir {
                node.files.push((*name).to_string());
                return;
            }
            if i == MAX_TREE_DEPTH {
                if !is_dir {
                    node.files.push(comps[i..].join("/"));
                }
                return;
            }
            node = node.dirs.entry((*name).to_string()).or_default();
        }
    }
}

/// Walk `root` and build the tree + flat file list. Blocking; run off the UI
/// thread.
///
/// What the walk skipped or added (see [`ScanReport`]) only reaches the log
/// from here, where no reader sees it: a caller that shows a project to a
/// person uses [`scan_with_report`] and shows [`ScanReport::summary`].
pub fn scan(root: PathBuf) -> ScanResult {
    let (result, report) = scan_with_report(root);
    if let Some(summary) = report.summary() {
        eprintln!("[clew] scan of {}: {summary}", result.root.display());
    }
    result
}

/// [`scan`], plus the [`ScanReport`] of what it skipped or added.
///
/// `.gitignore` decides which UNTRACKED files are noise, never which tracked
/// ones exist. git itself shows a tracked file whatever the ignore rules say,
/// but the walker applied them to everything, so a repository could commit
/// `src/payload.rs`, list it in its own `.gitignore`, and have it compiled
/// while invisible to the tree, search, the index and the agent. When `root`
/// is in a git work tree, every tracked file the walk did not produce is added
/// (flagged in [`ScanReport::tracked_ignored`]), subject to the same rules as
/// any other entry: a regular file, reached through real directories, not
/// under a TOP-LEVEL [`NOISE_DIRS`] directory (see [`extra_file`]), and not
/// already listed under another spelling (see [`already_listed`]). The
/// [`VISIBLE_CLEW_FILES`] are added the same way.
pub fn scan_with_report(root: PathBuf) -> (ScanResult, ScanReport) {
    let mut tmp = TmpDir::default();
    let mut files: Vec<FileEntry> = Vec::new();
    let mut report = ScanReport::default();
    let mut truncated = false;
    let mut seen = 0usize;

    let non_utf8 = Arc::new(AtomicUsize::new(0));
    let skipped = Arc::clone(&non_utf8);
    let walker = WalkBuilder::new(&root)
        .hidden(false) // show dotfiles; tool-internal dirs are filtered below
        .follow_links(false)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_name().to_str().is_none() {
                skipped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            !(entry.file_type().is_some_and(|t| t.is_dir()) && is_noise_dir(entry.file_name()))
        })
        .build();

    // The walked files by inode, for telling a tracked path that is the same
    // file under another spelling from a file the walk did not find.
    #[cfg(unix)]
    let mut inodes: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();

    for item in walker {
        let entry = match item {
            Ok(entry) => entry,
            Err(e) => {
                report.walk_errors += 1;
                report.first_error.get_or_insert_with(|| e.to_string());
                continue;
            }
        };
        if seen >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(rel) = path.strip_prefix(&root) else {
            continue;
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        // Only real directories and regular files are part of a project. A
        // symlink would read whatever it points at — including outside the
        // project — and a FIFO or device node would block or stream forever.
        if !is_dir && !file_type.is_file() {
            continue;
        }
        // Every name below the root passed the UTF-8 filter, so this only
        // fails for a path the walker built some other way; count it anyway.
        let Some(comps) = rel
            .iter()
            .map(|c| c.to_str())
            .collect::<Option<Vec<&str>>>()
        else {
            non_utf8.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        seen += 1;
        tmp.insert(&comps, is_dir);
        if !is_dir {
            #[cfg(unix)]
            if let Some(ino) = entry.ino() {
                inodes.entry(ino).or_insert(files.len());
            }
            files.push(FileEntry {
                abs: path.to_path_buf(),
                rel: comps.join("/"),
            });
        }
    }
    report.non_utf8 = non_utf8.load(Ordering::Relaxed);

    // What the ignore rules and the noise pruning hid that must be visible.
    let mut known: HashSet<String> = files.iter().map(|f| f.rel.clone()).collect();
    let visible_clew = VISIBLE_CLEW_FILES
        .iter()
        .map(|name| format!(".clew/{name}"));
    let tracked = match crate::git::tracked_files(&root) {
        Ok(tracked) => tracked.unwrap_or_default(),
        Err(e) => {
            report.tracked_error = Some(e.to_string());
            Vec::new()
        }
    };
    for (rel, is_tracked) in visible_clew
        .map(|rel| (rel, false))
        .chain(tracked.into_iter().map(|rel| (rel, true)))
    {
        if known.contains(&rel) {
            continue;
        }
        if seen >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let Some((abs, meta)) = extra_file(&root, &rel) else {
            continue;
        };
        #[cfg(unix)]
        if already_listed(&meta, &inodes, &files) {
            continue;
        }
        #[cfg(not(unix))]
        let _ = meta;
        let comps: Vec<&str> = rel.split('/').collect();
        seen += 1;
        tmp.insert(&comps, false);
        files.push(FileEntry {
            abs,
            rel: rel.clone(),
        });
        if is_tracked {
            report.tracked_ignored.push(rel.clone());
        }
        known.insert(rel);
    }

    // Case-insensitive, with the exact name as the tie-break so the order is
    // the same on every filesystem (the walk's own order is not). Keys are
    // computed once per entry, not twice per comparison.
    files.sort_by_cached_key(|f| (f.rel.to_lowercase(), f.rel.clone()));
    report.tracked_ignored.sort();

    (
        ScanResult {
            root,
            tree: convert(tmp),
            files,
            truncated,
        },
        report,
    )
}

/// The absolute path (and metadata) of `rel` when it may be added to the
/// scan outside the walk (a tracked file the walk left out, a visible `.clew`
/// config): a safe rel, not under a top-level noise directory (except the
/// visible `.clew` configs), every ancestor a REAL directory, and the leaf a
/// regular file.
///
/// Only a TOP-LEVEL noise directory keeps a tracked file out. At the top of
/// the project those names are the project's own build output, dependencies
/// and clew/git state — tracked or not, a vendored `node_modules` would flood
/// the index. Deeper down, a tracked directory with such a name is as likely
/// to be source (`src/target/mod.rs`, a `venv` test fixture), and git says it
/// is part of the repository; hiding it hid code that is compiled.
fn extra_file(root: &Path, rel: &str) -> Option<(PathBuf, std::fs::Metadata)> {
    if !crate::statefile::safe_rel(rel) {
        return None;
    }
    let comps: Vec<&str> = rel.split('/').collect();
    let (leaf, dirs) = comps.split_last()?;
    let visible_clew = dirs == [".clew"] && VISIBLE_CLEW_FILES.contains(leaf);
    if !visible_clew && dirs.first().is_some_and(|d| is_noise_dir(OsStr::new(d))) {
        return None;
    }
    let mut at = root.to_path_buf();
    for dir in dirs {
        at.push(dir);
        if !std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) {
            return None;
        }
    }
    at.push(leaf);
    let meta = std::fs::symlink_metadata(&at).ok()?;
    meta.is_file().then_some((at, meta))
}

/// Whether the file `meta` describes is already in `files` under another
/// spelling: git's index holds `Src/Main.rs` while a case-insensitive
/// filesystem (or a normalization-insensitive one: NFC in the index, NFD on
/// disk) walked it as `src/main.rs`. Same device and inode is the same file,
/// whatever its name — listing it twice showed two rows for one file.
#[cfg(unix)]
fn already_listed(
    meta: &std::fs::Metadata,
    inodes: &std::collections::HashMap<u64, usize>,
    files: &[FileEntry],
) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(&i) = inodes.get(&meta.ino()) else {
        return false;
    };
    // The walk's inode comes from the directory entry; confirm it (and the
    // device) against the walked file itself.
    std::fs::symlink_metadata(&files[i].abs)
        .is_ok_and(|walked| walked.dev() == meta.dev() && walked.ino() == meta.ino())
}

/// Whether `path` is a regular file that really lives inside `root`.
///
/// The scan already excludes symlinks, but a path can be swapped for one
/// afterwards (or arrive from elsewhere), so every read re-checks: canonicalize
/// both sides and require containment. Cheap next to the read that follows.
pub fn is_inside(root: &Path, path: &Path) -> bool {
    matches!(inside_checked(root, path), Ok(true))
}

/// [`is_inside`] for a reader that must tell "nothing is there" (`Ok(false)`)
/// from "something is there that may not be read" (`Err`): not a regular
/// file, not inside `root`, or not examinable at all.
fn inside_checked(root: &Path, path: &Path) -> Result<bool, ReadError> {
    // Only an answer that the path names nothing is an absence. Any other
    // failure to look — a directory on the way this user may not search, an
    // I/O error — says nothing about whether the file is there.
    let absent = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound => Ok(false),
        _ => Err(ReadError::Io(e)),
    };
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) => return absent(e),
    };
    if !meta.is_file() {
        return Err(ReadError::NotPlainFile); // symlink, FIFO, device, directory
    }
    let real = match path.canonicalize() {
        Ok(real) => real,
        Err(e) => return absent(e),
    };
    let real_root = root.canonicalize().map_err(ReadError::Io)?;
    if real.starts_with(&real_root) {
        Ok(true)
    } else {
        Err(ReadError::Outside)
    }
}

/// Read a project file as text, refusing anything that isn't a regular file
/// inside `root` or larger than `max_bytes`.
///
/// Opened ONCE, through [`crate::statefile::open_plain`], and read through the
/// cap. The obvious spelling — check the path, stat the path, then read the
/// path — resolves the name three times and enforces the size on a stat the
/// read never sees. That let a file growing between the two past the cap, and
/// a path swapped for a FIFO in the same window blocked `read_to_string`
/// forever, wedging the indexer thread (and the publication lock it holds)
/// for the life of the process.
pub fn read_confined_capped(root: &Path, path: &Path, max_bytes: u64) -> Option<String> {
    read_confined_capped_checked(root, path, max_bytes)
        .ok()
        .flatten()
}

/// [`read_confined_capped`] for a caller that must tell a file that is gone
/// from one it could not read, as [`crate::statefile::read_checked`] does for
/// state files: `Ok(None)` when nothing is at `path`, `Err` when something is
/// and it was refused — not a regular file inside `root`, over the cap (with
/// its size), not UTF-8 — or could not be read. An explain pass drops what it
/// recorded for a file that is gone, and for one refused, which it cannot
/// explain; it keeps it for one it could not read, since nothing says that
/// file changed.
pub fn read_confined_capped_checked(
    root: &Path,
    path: &Path,
    max_bytes: u64,
) -> Result<Option<String>, ReadError> {
    match read_confined_bytes_capped_checked(root, path, max_bytes)? {
        Some(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| ReadError::NotUtf8),
        None => Ok(None),
    }
}

/// [`read_confined_capped`] for raw bytes (content that need not be UTF-8,
/// e.g. line counting).
pub fn read_confined_bytes_capped(root: &Path, path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    read_confined_bytes_capped_checked(root, path, max_bytes)
        .ok()
        .flatten()
}

/// [`read_confined_bytes_capped`], telling gone from unreadable as
/// [`read_confined_capped_checked`] does.
fn read_confined_bytes_capped_checked(
    root: &Path,
    path: &Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, ReadError> {
    use std::io::Read;
    // Containment is still decided from the path, so a swapped PARENT
    // directory remains the accepted residual documented for `.clew`. The
    // leaf, which is what actually gets read, is now safe on its own:
    // `O_NOFOLLOW` refuses a symlink and `O_NONBLOCK` refuses to block.
    if !inside_checked(root, path)? {
        return Ok(None);
    }
    let Some(f) = crate::statefile::open_plain_checked(path)? else {
        return Ok(None);
    };
    let len = f.metadata().map_err(ReadError::Io)?.len();
    if len > max_bytes {
        // A cheap early reject, before reading a byte.
        return Err(ReadError::TooLarge {
            cap: max_bytes,
            size: len,
        });
    }
    let mut bytes = Vec::new();
    // `max_bytes + 1`: reading one byte past the cap is what distinguishes
    // "exactly at the limit" from "grew past it while we were reading".
    (&f).take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() as u64 > max_bytes {
        return Err(crate::statefile::grew_past(&f, max_bytes, bytes.len()));
    }
    Ok(Some(bytes))
}

fn convert(tmp: TmpDir) -> DirNode {
    let mut dirs: Vec<(String, DirNode)> = tmp
        .dirs
        .into_iter()
        .map(|(name, child)| (name, convert(child)))
        .collect();
    // Stable, and the BTreeMap above already ordered exact names, so equal
    // lowercase keys keep a deterministic order.
    dirs.sort_by_cached_key(|(name, _)| name.to_lowercase());
    let mut node_files = tmp.files;
    node_files.sort_by_cached_key(|name| (name.to_lowercase(), name.clone()));
    DirNode {
        dirs,
        files: node_files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn scan_respects_gitignore_and_builds_tree() {
        let scratch = TempDir::new("scan-gitignore");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap(); // make gitignore apply
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("target/debug/junk.o"), "x").unwrap();
        std::fs::write(dir.join("README.md"), "# hi\n").unwrap();

        let result = scan(dir.clone());
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(rels.contains(&"src/main.rs"), "files: {rels:?}");
        assert!(rels.contains(&"README.md"), "files: {rels:?}");
        assert!(
            !rels.iter().any(|r| r.starts_with("target")),
            "gitignored files leaked: {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.starts_with(".git/")),
            ".git leaked: {rels:?}"
        );

        // Tree: dirs sorted first, then files.
        let dir_names: Vec<&str> = result.tree.dirs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(dir_names.contains(&"src"));
        assert!(!dir_names.contains(&"target"));
        assert!(result.tree.files.contains(&"README.md".to_string()));
    }

    #[test]
    fn skips_vendor_dirs_without_gitignore() {
        // No .git / .gitignore here — vendor/build dirs must still be pruned.
        let scratch = TempDir::new("scan-vendor");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/lodash")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join(".dart_tool")).unwrap();
        std::fs::write(dir.join("src/index.ts"), "export const x = 1;\n").unwrap();
        std::fs::write(
            dir.join("node_modules/lodash/index.d.ts"),
            "export declare const y: number;\n",
        )
        .unwrap();
        std::fs::write(dir.join("target/debug/build.json"), "{}").unwrap();
        std::fs::write(dir.join(".dart_tool/pkg.json"), "{}").unwrap();

        let result = scan(dir);
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(
            rels.contains(&"src/index.ts"),
            "own source missing: {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.contains("node_modules")),
            "node_modules leaked: {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.starts_with("target")),
            "target leaked: {rels:?}"
        );
        assert!(
            !rels.iter().any(|r| r.contains(".dart_tool")),
            ".dart_tool leaked: {rels:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_project_files() {
        // A repository can ship symlinks pointing anywhere on the host
        // (`outside.txt -> /etc/hosts`). They must not become project files:
        // search, docs, and the agent's `read` would follow them out.
        let scratch = TempDir::new("scan-symlink");
        let dir = scratch.join("proj");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

        let secret_dir = scratch.join("secret");
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::write(secret_dir.join("secret.txt"), "s3cret\n").unwrap();

        std::os::unix::fs::symlink(secret_dir.join("secret.txt"), dir.join("outside.txt")).unwrap();
        std::os::unix::fs::symlink(&secret_dir, dir.join("outside-dir")).unwrap();
        // A link pointing *inside* the project is excluded too: reads resolve
        // paths, and only real files should ever be read.
        std::os::unix::fs::symlink(dir.join("src/main.rs"), dir.join("alias.rs")).unwrap();

        let result = scan(dir.clone());
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(rels.contains(&"src/main.rs"), "files: {rels:?}");
        assert!(
            !rels.iter().any(|r| r.contains("outside")),
            "symlink leaked into the scan: {rels:?}"
        );
        assert!(
            !rels.contains(&"alias.rs"),
            "in-project symlink leaked: {rels:?}"
        );

        // The read-time re-check agrees: a symlink is refused even when named
        // directly (the scan can be stale, or the path may come from elsewhere).
        assert!(is_inside(&dir, &dir.join("src/main.rs")));
        assert!(!is_inside(&dir, &dir.join("outside.txt")));
        assert!(!is_inside(&dir, &secret_dir.join("secret.txt")));
        const CAP: u64 = 1024 * 1024;
        assert!(read_confined_capped(&dir, &dir.join("outside.txt"), CAP).is_none());
        assert_eq!(
            read_confined_capped(&dir, &dir.join("src/main.rs"), CAP).as_deref(),
            Some("fn main() {}\n")
        );
    }

    /// A deep directory must still produce a tree the CLIENT can parse. The
    /// frame here is a few KB — the failure this guards is `serde_json`'s
    /// 128-container recursion limit, not the frame cap — and it is fatal:
    /// an unparseable frame drops the connection, and the respawn rebuilds the
    /// same frame forever.
    #[test]
    fn deep_nesting_stays_within_the_wire_recursion_limit() {
        const DEPTH: usize = 60; // comfortably past MAX_TREE_DEPTH
        let scratch = TempDir::new("scan-depth");
        let dir = scratch.to_path_buf();
        let mut deep = dir.clone();
        let mut rel = String::new();
        for i in 0..DEPTH {
            deep = deep.join(format!("d{i}"));
            if i > 0 {
                rel.push('/');
            }
            rel.push_str(&format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("leaf.rs"), "fn leaf() {}\n").unwrap();

        let result = scan(dir.clone());

        // The flat file list is untouched by the fold: every file keeps its
        // full rel, so search, docs and the agent still see it.
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        let leaf_rel = format!("{rel}/leaf.rs");
        assert!(
            rels.contains(&leaf_rel.as_str()),
            "deep file lost: {rels:?}"
        );

        // The assertion that was missing: the real wire envelope round-trips.
        let msg = clew_protocol::ServerMessage::Notification {
            event: clew_protocol::Event::Tree {
                root: result.root.to_string_lossy().into_owned(),
                seq: 1,
                tree: result.tree.clone(),
                files: result.files.iter().map(|f| f.rel.clone()).collect(),
                truncated: result.truncated,
                tracked_ignored: Vec::new(),
            },
        };
        let line = serde_json::to_string(&msg).unwrap();
        let back = serde_json::from_str::<clew_protocol::ServerMessage>(&line);
        assert!(
            back.is_ok(),
            "the client cannot parse its own Tree frame: {:?}",
            back.err()
        );

        // The tree is bounded, and the fold is visible in the tree itself: the
        // deep file appears as one row whose name is its whole remaining path.
        fn walk(node: &DirNode, depth: usize, deepest: &mut usize, folded: &mut Vec<String>) {
            *deepest = (*deepest).max(depth);
            folded.extend(node.files.iter().filter(|f| f.contains('/')).cloned());
            for (_, child) in &node.dirs {
                walk(child, depth + 1, deepest, folded);
            }
        }
        let (mut deepest, mut folded) = (0usize, Vec::new());
        walk(&result.tree, 0, &mut deepest, &mut folded);
        assert!(
            deepest <= MAX_TREE_DEPTH,
            "tree nests {deepest} levels, past the cap"
        );
        assert_eq!(folded.len(), 1, "expected one folded row, got {folded:?}");
        // Joining row names the way the sidebar does still yields the real rel,
        // so the folded row opens exactly the file it names.
        assert_eq!(
            format!(
                "{}/{}",
                (0..MAX_TREE_DEPTH)
                    .map(|i| format!("d{i}"))
                    .collect::<Vec<_>>()
                    .join("/"),
                folded[0]
            ),
            leaf_rel
        );
    }

    /// The cap belongs on the READ, not on a separate stat: a file can grow
    /// between the two, and the type check has to hold for the handle that is
    /// actually read — a FIFO swapped in would otherwise block forever.
    #[test]
    fn read_confined_capped_bounds_the_read_and_refuses_a_fifo() {
        let scratch = TempDir::new("scan-read-capped");
        let dir = scratch.to_path_buf();
        let file = dir.join("big.rs");
        std::fs::write(&file, "x".repeat(4096)).unwrap();

        assert!(
            read_confined_capped(&dir, &file, 1024).is_none(),
            "over cap"
        );
        assert_eq!(
            read_confined_capped(&dir, &file, 8192).map(|s| s.len()),
            Some(4096)
        );
        // Exactly at the limit is allowed; one byte more is not.
        assert!(read_confined_capped(&dir, &file, 4096).is_some());
        assert!(read_confined_capped(&dir, &file, 4095).is_none());

        #[cfg(unix)]
        {
            let pipe = dir.join("pipe.rs");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&pipe)
                    .status()
                    .is_ok_and(|s| s.success())
            );
            // On a thread, so a regression is a failed assert rather than a
            // test run that never finishes.
            let (tx, rx) = std::sync::mpsc::channel();
            let (d, p) = (dir.clone(), pipe.clone());
            std::thread::spawn(move || {
                let _ = tx.send(read_confined_capped(&d, &p, 8192).is_none());
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_secs(5))
                    .expect("the read blocked on the FIFO"),
                "a FIFO must not be read as a project file"
            );
        }
    }

    /// Only a path that names nothing reads as gone (`Ok(None)`). A file this
    /// user may not read, one behind a directory they may not search, one
    /// over the cap, one that is not text, a symlink, and one reached through
    /// a directory that leads out of the root are all there: each is an
    /// `Err`. An explain pass dropped every summary of such a file as if it
    /// had been deleted. The unchecked read still answers `None` for all.
    #[cfg(unix)]
    #[test]
    fn the_checked_read_tells_a_missing_file_from_an_unreadable_one() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads everything; nothing to observe
        }
        let scratch = TempDir::new("scan-read-checked");
        let dir = scratch.join("proj");
        let outside = scratch.join("outside");
        std::fs::create_dir_all(dir.join("sealed")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("leak.rs"), "fn leak() {}\n").unwrap();
        std::fs::write(dir.join("ok.rs"), "fn ok() {}\n").unwrap();
        std::fs::write(dir.join("big.rs"), "x".repeat(2048)).unwrap();
        std::fs::write(dir.join("binary.rs"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(dir.join("locked.rs"), "fn locked() {}\n").unwrap();
        std::fs::write(dir.join("sealed/inner.rs"), "fn inner() {}\n").unwrap();
        std::os::unix::fs::symlink(outside.join("leak.rs"), dir.join("link.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("out")).unwrap();
        const CAP: u64 = 1024;
        let read = |rel: &str| read_confined_capped_checked(&dir, &dir.join(rel), CAP);

        let set_mode = |path: PathBuf, mode: u32| {
            std::fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
        };
        set_mode(dir.join("locked.rs"), 0o000);
        set_mode(dir.join("sealed"), 0o000);
        let (locked, sealed) = (read("locked.rs"), read("sealed/inner.rs"));
        let unchecked = ["locked.rs", "sealed/inner.rs"]
            .map(|rel| read_confined_capped(&dir, &dir.join(rel), CAP).is_none());
        set_mode(dir.join("sealed"), 0o755);
        set_mode(dir.join("locked.rs"), 0o644);
        assert!(matches!(locked, Err(ReadError::Io(_))), "{locked:?}");
        assert!(matches!(sealed, Err(ReadError::Io(_))), "{sealed:?}");
        assert_eq!(unchecked, [true, true]);

        assert_eq!(read("ok.rs").unwrap().as_deref(), Some("fn ok() {}\n"));
        assert!(matches!(read("gone.rs"), Ok(None)), "{:?}", read("gone.rs"));
        assert!(matches!(read("gone/deeper.rs"), Ok(None)));
        assert!(matches!(
            read("big.rs"),
            Err(ReadError::TooLarge {
                cap: CAP,
                size: 2048
            })
        ));
        assert!(matches!(read("binary.rs"), Err(ReadError::NotUtf8)));
        assert!(matches!(read("link.rs"), Err(ReadError::NotPlainFile)));
        assert!(matches!(read("out/leak.rs"), Err(ReadError::Outside)));
        for rel in ["gone.rs", "big.rs", "binary.rs", "link.rs", "out/leak.rs"] {
            assert!(
                read_confined_capped(&dir, &dir.join(rel), CAP).is_none(),
                "{rel}"
            );
        }
    }

    /// Plain git for the fixture, isolated from the developer's global and
    /// system config (a hook or `commit.gpgsign` there must not decide it).
    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// A fresh repository with an identity, canonical path.
    fn repo(scratch: &TempDir) -> PathBuf {
        let dir = scratch.canonicalize().unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        dir
    }

    /// A tracked file cannot be hidden by the repository's own `.gitignore`:
    /// git shows it, compiles it and diffs it, so clew's tree, search, index
    /// and agent must see it too. Untracked ignored files stay hidden, and so
    /// does noise even when it is tracked — at the TOP of the project; a
    /// tracked file under a nested directory that merely has a build-output
    /// name (`src/target/mod.rs`) is source, and is shown.
    #[test]
    fn a_tracked_file_cannot_hide_behind_the_gitignore() {
        let scratch = TempDir::new("scan-tracked-ignored");
        let dir = repo(&scratch);
        std::fs::create_dir_all(dir.join("src/target")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "mod target;\nfn main() {}\n").unwrap();
        std::fs::write(dir.join("src/payload.rs"), "fn payload() {}\n").unwrap();
        std::fs::write(dir.join("src/target/mod.rs"), "fn compiled() {}\n").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "x\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        // Committed, then hidden by the repository's own ignore rules.
        std::fs::write(
            dir.join(".gitignore"),
            "src/payload.rs\nnode_modules/\n*.log\n",
        )
        .unwrap();
        std::fs::write(dir.join("debug.log"), "noise\n").unwrap();
        // Untracked content of a nested build-named directory stays pruned.
        std::fs::write(dir.join("src/target/scratch.rs"), "x\n").unwrap();

        let (result, report) = scan_with_report(dir.clone());
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(
            rels.contains(&"src/payload.rs"),
            "hidden tracked file: {rels:?}"
        );
        assert!(
            rels.contains(&"src/target/mod.rs"),
            "tracked source under a nested `target` dir: {rels:?}"
        );
        assert!(!rels.contains(&"src/target/scratch.rs"), "{rels:?}");
        assert_eq!(
            report.tracked_ignored,
            vec![
                "src/payload.rs".to_string(),
                "src/target/mod.rs".to_string()
            ]
        );
        assert!(
            !rels.contains(&"debug.log"),
            "untracked ignored stays hidden"
        );
        assert!(
            !rels.iter().any(|r| r.starts_with("node_modules")),
            "tracked top-level noise stays pruned: {rels:?}"
        );
        // It is in the tree too, where the reader browses.
        let src = result
            .tree
            .dirs
            .iter()
            .find(|(n, _)| n == "src")
            .map(|(_, d)| d)
            .unwrap();
        assert!(src.files.contains(&"payload.rs".to_string()));
        assert!(report.summary().unwrap().contains("despite .gitignore"));
        assert!(report.tracked_error.is_none());
    }

    /// One file, two spellings: git's index says `Src/Main.rs` while a
    /// case-insensitive filesystem walks `src/main.rs`. It is listed once.
    #[test]
    fn a_tracked_path_spelled_differently_on_disk_is_listed_once() {
        let scratch = TempDir::new("scan-case");
        let dir = repo(&scratch);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        // Rename the directory on disk only; the index keeps `src/`.
        std::fs::rename(dir.join("src"), dir.join("tmp")).unwrap();
        std::fs::rename(dir.join("tmp"), dir.join("Src")).unwrap();
        let case_insensitive = dir.join("src/main.rs").exists();

        let (result, report) = scan_with_report(dir.clone());
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        if case_insensitive {
            assert_eq!(rels, ["Src/main.rs"], "one file, one row");
            assert!(report.tracked_ignored.is_empty(), "{report:?}");
        } else {
            // A case-sensitive filesystem really has no `src/main.rs`.
            assert_eq!(rels, ["Src/main.rs"]);
        }
    }

    /// When git cannot list the tracked files, the scan holds only what the
    /// walk found — and says so, instead of quietly showing less.
    #[test]
    fn a_git_failure_listing_tracked_files_is_reported() {
        let scratch = TempDir::new("scan-git-error");
        let dir = repo(&scratch);
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        // A filter driver clew cannot switch off makes it refuse to run git.
        git(&dir, &["config", "filter.x=y.clean", "cat"]);
        let (result, report) = scan_with_report(dir.clone());
        assert!(result.files.iter().any(|f| f.rel == "a.rs"));
        assert!(report.tracked_error.is_some(), "{report:?}");
        assert!(
            report
                .summary()
                .unwrap()
                .contains("could not list the tracked files"),
            "{report:?}"
        );
    }

    /// `.clew/` is pruned, but the two files in it that decide what clew RUNS
    /// are the repository's own configuration and are shown like any file;
    /// the reader's private state next to them is not.
    #[test]
    fn repository_run_configuration_in_clew_is_visible() {
        let scratch = TempDir::new("scan-clew-config");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join(".clew/cache")).unwrap();
        std::fs::write(dir.join(".clew/lsp.toml"), "[rust]\n").unwrap();
        std::fs::write(dir.join(".clew/launch.json"), "{}").unwrap();
        std::fs::write(dir.join(".clew/bookmarks.json"), "[]").unwrap();
        std::fs::write(dir.join(".clew/cache/walkthroughs.json"), "[]").unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();

        let (result, report) = scan_with_report(dir.clone());
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(
            rels,
            [".clew/launch.json", ".clew/lsp.toml", "main.rs"],
            "{rels:?}"
        );
        assert!(report.tracked_ignored.is_empty());

        // A symlinked config is still refused.
        #[cfg(unix)]
        {
            std::fs::remove_file(dir.join(".clew/lsp.toml")).unwrap();
            std::os::unix::fs::symlink("/etc/hosts", dir.join(".clew/lsp.toml")).unwrap();
            let rels: Vec<String> = scan(dir.clone()).files.into_iter().map(|f| f.rel).collect();
            assert!(!rels.contains(&".clew/lsp.toml".to_string()), "{rels:?}");
        }
    }

    /// A name that is not UTF-8 cannot be a rel (a lossy one points at no
    /// file), so it is skipped — and counted, instead of vanishing silently.
    #[test]
    #[cfg(target_os = "linux")]
    fn non_utf8_names_are_skipped_and_counted() {
        use std::os::unix::ffi::OsStrExt;
        let scratch = TempDir::new("scan-non-utf8");
        let dir = scratch.to_path_buf();
        std::fs::write(dir.join("ok.rs"), "fn ok() {}\n").unwrap();
        std::fs::write(dir.join(OsStr::from_bytes(b"caf\xe9.rs")), "x").unwrap();
        let (result, report) = scan_with_report(dir);
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["ok.rs"]);
        assert_eq!(report.non_utf8, 1);
    }

    /// Unreadable directories are reported, not silently dropped.
    #[test]
    #[cfg(unix)]
    fn walk_errors_are_counted() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads everything; nothing to observe
        }
        let scratch = TempDir::new("scan-walk-errors");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        std::fs::write(dir.join("locked/a.rs"), "x").unwrap();
        std::fs::write(dir.join("b.rs"), "x").unwrap();
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let (result, report) = scan_with_report(dir.clone());
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(result.files.iter().any(|f| f.rel == "b.rs"));
        assert_eq!(report.walk_errors, 1, "{report:?}");
        assert!(report.first_error.is_some());
        assert!(report.summary().unwrap().contains("unreadable"));
    }

    /// Case-insensitive order with a deterministic tie-break, whatever order
    /// the filesystem returned entries in.
    #[test]
    fn file_order_is_case_insensitive_and_deterministic() {
        let scratch = TempDir::new("scan-order");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("B")).unwrap();
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("B/x.rs"), "x").unwrap();
        std::fs::write(dir.join("a/y.rs"), "x").unwrap();
        std::fs::write(dir.join("c.rs"), "x").unwrap();
        let result = scan(dir);
        let rels: Vec<&str> = result.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["a/y.rs", "B/x.rs", "c.rs"]);
        let dirs: Vec<&str> = result.tree.dirs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(dirs, ["a", "B"]);
    }
}
