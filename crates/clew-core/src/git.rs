//! Read-only git information for the open file: per-line blame (who last
//! touched each line) and per-line change status versus `HEAD` for the gutter.
//!
//! Everything here shells out to the `git` binary with read-only commands and
//! degrades to `None` when the file is not in a repo or `git` is unavailable.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

// Blame, change status, and the per-file git view are protocol wire types (git
// produces them here, the server transmits them, the gutter renders them), so
// there is no conversion between produce and render.
pub use clew_protocol::{BlameLine, ChangeKind, GitInfo};

/// Collect blame + change status for `abs`, or `None` when it is not tracked in
/// a git work tree. Blocking; run off the UI thread.
pub fn info(root: &Path, abs: &Path) -> Option<GitInfo> {
    if !is_work_tree(root) {
        return None;
    }
    let blame = blame(root, abs).unwrap_or_default();
    let (status, deleted_at) = diff_status(root, abs).unwrap_or_default();
    if blame.is_empty() && status.is_empty() && deleted_at.is_empty() {
        return None;
    }
    Some(GitInfo {
        blame,
        status,
        deleted_at,
    })
}

fn is_work_tree(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success() && o.stdout.starts_with(b"true"))
        .unwrap_or(false)
}

/// Parse `git blame --porcelain`. The porcelain format prints a header line
/// `<sha> <orig> <final> [group-size]` per line, and the author/summary fields
/// only on a commit's first appearance, so we cache them by sha.
fn blame(root: &Path, abs: &Path) -> Option<Vec<BlameLine>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["blame", "--porcelain", "--"])
        .arg(abs)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);

    // sha -> (author, time, summary)
    let mut meta: std::collections::HashMap<String, (String, i64, String)> =
        std::collections::HashMap::new();
    let mut result: Vec<BlameLine> = Vec::new();
    let mut cur_sha = String::new();
    let mut cur_final = 0usize;

    for line in text.lines() {
        if let Some((sha, rest)) = line.split_once(' ')
            && is_hex40(sha)
        {
            // Header line: "<sha> <orig> <final> [group]".
            cur_sha = sha.to_string();
            cur_final = rest
                .split(' ')
                .nth(1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            meta.entry(cur_sha.clone())
                .or_insert_with(|| (String::from("Unknown"), 0, String::new()));
            continue;
        }
        if let Some(name) = line.strip_prefix("author ") {
            if let Some(e) = meta.get_mut(&cur_sha) {
                e.0 = name.to_string();
            }
        } else if let Some(t) = line.strip_prefix("author-time ") {
            if let Some(e) = meta.get_mut(&cur_sha) {
                e.1 = t.trim().parse().unwrap_or(0);
            }
        } else if let Some(s) = line.strip_prefix("summary ") {
            if let Some(e) = meta.get_mut(&cur_sha) {
                e.2 = s.to_string();
            }
        } else if let Some(_content) = line.strip_prefix('\t') {
            // The tab-prefixed content line closes one final line.
            let (author, time, summary) = meta
                .get(&cur_sha)
                .cloned()
                .unwrap_or_else(|| (String::from("Unknown"), 0, String::new()));
            let uncommitted = cur_sha.chars().all(|c| c == '0');
            if cur_final >= 1 {
                if result.len() < cur_final {
                    result.resize(
                        cur_final,
                        BlameLine {
                            commit: String::new(),
                            author: String::new(),
                            time: 0,
                            summary: String::new(),
                            uncommitted: false,
                        },
                    );
                }
                result[cur_final - 1] = BlameLine {
                    commit: cur_sha.chars().take(7).collect(),
                    author,
                    time,
                    summary,
                    uncommitted,
                };
            }
        }
    }
    Some(result)
}

/// Parse `git diff -U0 HEAD -- <file>` hunk headers into per-line status.
/// `@@ -oldStart,oldCount +newStart,newCount @@`.
fn diff_status(root: &Path, abs: &Path) -> Option<(Vec<Option<ChangeKind>>, HashSet<usize>)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--no-color", "-U0", "HEAD", "--"])
        .arg(abs)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);

    let mut status: Vec<Option<ChangeKind>> = Vec::new();
    let mut deleted_at: HashSet<usize> = HashSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("@@ ") else {
            continue;
        };
        let Some((old, new)) = parse_hunk(rest) else {
            continue;
        };
        let (_old_start, old_count) = old;
        let (new_start, new_count) = new;
        if new_count == 0 {
            // Pure deletion: mark the line above which content vanished.
            deleted_at.insert(new_start.saturating_sub(1));
            continue;
        }
        let kind = if old_count == 0 {
            ChangeKind::Added
        } else {
            ChangeKind::Modified
        };
        // new_start is 1-based; mark the new_count lines it covers.
        let start0 = new_start.saturating_sub(1);
        if status.len() < start0 + new_count {
            status.resize(start0 + new_count, None);
        }
        for s in status.iter_mut().skip(start0).take(new_count) {
            *s = Some(kind);
        }
    }
    Some((status, deleted_at))
}

/// Parse `-a,b +c,d` (counts optional, defaulting to 1) from a hunk header.
fn parse_hunk(s: &str) -> Option<((usize, usize), (usize, usize))> {
    let mut parts = s.split(' ');
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    Some((parse_range(old), parse_range(new)))
}

fn parse_range(s: &str) -> (usize, usize) {
    match s.split_once(',') {
        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(0)),
        None => (s.parse().unwrap_or(0), 1),
    }
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One line of a unified diff, tagged for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Header,
    Hunk,
    Context,
    Add,
    Remove,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// The unified diff of `abs` against `HEAD`, one tagged line per row, or `None`
/// when the file is not tracked. An empty vec means "no changes".
pub fn diff_lines(root: &Path, abs: &Path) -> Option<Vec<DiffLine>> {
    if !is_work_tree(root) {
        return None;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--no-color", "HEAD", "--"])
        .arg(abs)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().map(classify_diff_line).collect())
}

fn classify_diff_line(line: &str) -> DiffLine {
    // Order matters: `---`/`+++` file headers start with -/+ but are headers.
    let kind = if line.starts_with("@@") {
        DiffKind::Hunk
    } else if line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("similarity")
        || line.starts_with("rename ")
    {
        DiffKind::Header
    } else if line.starts_with('+') {
        DiffKind::Add
    } else if line.starts_with('-') {
        DiffKind::Remove
    } else {
        DiffKind::Context
    };
    DiffLine {
        kind,
        text: line.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Branch/PR review: the diff of the current work against a base, for the
// narrated "review changes" walkthrough.
// ---------------------------------------------------------------------------

/// Run a read-only git command in `root`, returning stdout on success.
fn git_out(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether a revision resolves in this repo.
fn rev_exists(root: &Path, rev: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", rev])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The base to review the current work against, with a human label: the branch's
/// merge-base with `main`/`master` (the "PR diff"), else the previous commit.
/// `None` when there's nothing to review or this isn't a repo. Diff the returned
/// base with `base...HEAD`; list its commits with `base..HEAD`.
pub fn review_base(root: &Path) -> Option<(String, String)> {
    if !is_work_tree(root) {
        return None;
    }
    for base in ["main", "master"] {
        if rev_exists(root, base)
            && git_out(root, &["rev-parse", "HEAD"]) != git_out(root, &["rev-parse", base])
        {
            let range = format!("{base}...HEAD");
            let has_diff = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["diff", "--quiet", &range])
                .output()
                .map(|o| !o.status.success()) // --quiet exits 1 when there are changes
                .unwrap_or(false);
            if has_diff {
                return Some((base.to_string(), format!("vs {base}")));
            }
        }
    }
    // No base branch (or HEAD is the base): review the last commit instead.
    rev_exists(root, "HEAD~1").then(|| ("HEAD~1".to_string(), "last commit".to_string()))
}

/// Files changed in `base...HEAD` as `(relative path, status letter)` (A/M/D/R…).
pub fn changed_files(root: &Path, base: &str) -> Vec<(String, char)> {
    let range = format!("{base}...HEAD");
    let Some(text) = git_out(root, &["diff", "--name-status", &range]) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let status = parts.next()?.chars().next()?;
            // Renames print `R100\told\tnew`; take the final (new) path.
            let path = parts.last()?.to_string();
            (!path.is_empty()).then_some((path, status))
        })
        .collect()
}

/// The unified patch of `base...HEAD`, truncated to `max_bytes` (with a marker)
/// so a huge diff can't blow the LLM context.
pub fn range_patch(root: &Path, base: &str, max_bytes: usize) -> String {
    let range = format!("{base}...HEAD");
    let mut text = git_out(root, &["diff", "--no-color", &range]).unwrap_or_default();
    if text.len() > max_bytes {
        // Truncate on a char boundary.
        let mut cut = max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… (diff truncated)\n");
    }
    text
}

/// Commit subjects in `base..HEAD`, oldest first (the change's intent).
pub fn commit_subjects(root: &Path, base: &str) -> Vec<String> {
    let range = format!("{base}..HEAD");
    git_out(root, &["log", "--reverse", "--format=%s", &range])
        .map(|t| {
            t.lines()
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Truncate `text` to at most `max_bytes` on a char boundary, with a marker.
fn truncate_marked(text: &mut String, max_bytes: usize) {
    if text.len() > max_bytes {
        let mut cut = max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… (truncated)\n");
    }
}

/// The full message (subject + body) of commit `sha`.
pub fn commit_message(root: &Path, sha: &str) -> Option<String> {
    git_out(root, &["show", "-s", "--format=%B", sha])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The diff commit `sha` made to `rel` (that file only), truncated to `max_bytes`.
/// The empty `--format=` suppresses the commit header, leaving just the patch.
pub fn commit_file_diff(root: &Path, sha: &str, rel: &str, max_bytes: usize) -> String {
    let mut text =
        git_out(root, &["show", "--no-color", "--format=", sha, "--", rel]).unwrap_or_default();
    truncate_marked(&mut text, max_bytes);
    text
}

/// A short "3 days ago" style label from a unix timestamp, relative to `now`.
pub fn relative_time(time: i64, now: i64) -> String {
    let d = now - time;
    if d < 0 {
        return "just now".to_string();
    }
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    let (n, unit) = if d < MIN {
        return "just now".to_string();
    } else if d < HOUR {
        (d / MIN, "minute")
    } else if d < DAY {
        (d / HOUR, "hour")
    } else if d < 30 * DAY {
        (d / DAY, "day")
    } else if d < 365 * DAY {
        (d / (30 * DAY), "month")
    } else {
        (d / (365 * DAY), "year")
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

// ------------------------------------------------------------- time travel

/// One commit in a file's (or a symbol's) history: short sha, author, authored
/// time (unix), subject, and the file's path *as of that commit* (so renames are
/// followed when fetching content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistCommit {
    pub sha: String,
    pub author: String,
    pub time: i64,
    pub subject: String,
    pub path: String,
}

// Record/field separators — control chars that never appear in git metadata,
// so `--format` output parses unambiguously even with odd commit messages.
const RS: char = '\x1e';
const FS: char = '\x1f';

/// Commits that touched `rel`, newest first, following renames. Capped at `limit`.
pub fn file_history(root: &Path, rel: &str, limit: usize) -> Vec<HistCommit> {
    if !is_work_tree(root) {
        return Vec::new();
    }
    let n = format!("-n{limit}");
    // A record marker (RS) before each commit's fields, plus `--name-only` so we
    // can read the file's path at that commit (which differs across renames).
    let fmt = format!("--format={RS}%H{FS}%an{FS}%at{FS}%s");
    let out = git_out(
        root,
        &["log", "--follow", &n, &fmt, "--name-only", "--", rel],
    )
    .unwrap_or_default();
    parse_hist(&out, rel)
}

/// Commits that changed lines `start..=end` (1-based) of `rel`, newest first.
/// Uses `git log -L`, which scopes history to that line range (a function).
pub fn symbol_history(
    root: &Path,
    rel: &str,
    start: usize,
    end: usize,
    limit: usize,
) -> Vec<HistCommit> {
    if !is_work_tree(root) || start == 0 || end < start {
        return Vec::new();
    }
    let range = format!("-L{start},{end}:{rel}");
    let n = format!("-n{limit}");
    // `-L` always prints the patch; a record marker lets us pluck the commit
    // headers and ignore the diff hunks (which never contain the marker).
    let fmt = format!("--format={RS}%H{FS}%an{FS}%at{FS}%s");
    let out = git_out(root, &["log", &n, &fmt, &range]).unwrap_or_default();
    out.split(RS)
        .skip(1)
        .filter_map(|rec| parse_hist_record(rec.lines().next().unwrap_or(""), rel))
        .collect()
}

fn parse_hist(out: &str, fallback_rel: &str) -> Vec<HistCommit> {
    out.split(RS)
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            if rec.is_empty() {
                return None;
            }
            let mut lines = rec.lines();
            let mut commit = parse_hist_record(lines.next()?, fallback_rel)?;
            // `--name-only` prints the file's path (post-rename) after the header.
            if let Some(p) = lines.find(|l| !l.trim().is_empty()) {
                commit.path = p.to_string();
            }
            Some(commit)
        })
        .collect()
}

fn parse_hist_record(head: &str, fallback_rel: &str) -> Option<HistCommit> {
    let mut f = head.splitn(4, FS);
    let sha = f.next()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    let author = f.next()?.to_string();
    let time = f.next()?.trim().parse::<i64>().ok()?;
    let subject = f.next().unwrap_or("").to_string();
    Some(HistCommit {
        sha,
        author,
        time,
        subject,
        path: fallback_rel.to_string(),
    })
}

/// The full text of `rel` as of commit `sha`, or `None` when it is absent at
/// that commit or is binary.
pub fn file_at(root: &Path, sha: &str, rel: &str) -> Option<String> {
    let spec = format!("{sha}:{rel}");
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &spec])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    if out.stdout.contains(&0) {
        return None; // binary
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The 1-based line numbers in `rel` @ `sha` that this commit added or changed
/// (the '+' side of its diff), for highlighting what a step introduced.
pub fn commit_added_lines(root: &Path, sha: &str, rel: &str) -> HashSet<usize> {
    let diff =
        git_out(root, &["show", "--no-color", "--format=", sha, "--", rel]).unwrap_or_default();
    added_lines_from_diff(&diff)
}

/// Parse a unified diff, returning the new-side line numbers of added lines.
fn added_lines_from_diff(diff: &str) -> HashSet<usize> {
    let mut set = HashSet::new();
    let mut new_line = 0usize;
    for line in diff.lines() {
        if line.starts_with("@@") {
            // `@@ -a,b +c,d @@` — the new file's hunk starts at line c.
            if let Some(plus) = line.split('+').nth(1) {
                let c: usize = plus
                    .split([',', ' '])
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                new_line = c;
            }
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                set.insert(new_line);
                new_line += 1;
            }
            Some(b'-') => {}    // removed line: doesn't exist on the new side
            _ => new_line += 1, // context line
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunk_ranges() {
        assert_eq!(parse_hunk("-1,0 +2,3 @@ ctx"), Some(((1, 0), (2, 3))));
        assert_eq!(parse_hunk("-5 +5 @@"), Some(((5, 1), (5, 1))));
        assert_eq!(parse_hunk("-10,2 +0,0 @@"), Some(((10, 2), (0, 0))));
    }

    #[test]
    fn parses_file_history_records() {
        // Two commits, `--name-only` path after each header; the second is a
        // rename (old path), which must be captured as that commit's path.
        let out = format!(
            "{RS}abc123{FS}Ada{FS}1700000000{FS}Add parser\nsrc/parser.rs\n\
             {RS}def456{FS}Bo{FS}1699990000{FS}Initial\nsrc/parse.rs\n"
        );
        let h = parse_hist(&out, "src/parser.rs");
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].sha, "abc123");
        assert_eq!(h[0].author, "Ada");
        assert_eq!(h[0].time, 1_700_000_000);
        assert_eq!(h[0].subject, "Add parser");
        assert_eq!(h[0].path, "src/parser.rs");
        assert_eq!(h[1].sha, "def456");
        assert_eq!(h[1].path, "src/parse.rs"); // rename followed
    }

    #[test]
    fn added_lines_tracks_new_side_numbering() {
        // New side: line 2 added; then in the second hunk line 10 added, line 11
        // is context, a removal (old side only) doesn't advance the new counter,
        // so the next '+' is new-side line 12.
        let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                    @@ -1,1 +1,2 @@\n ctx\n+new-2\n\
                    @@ -9,3 +10,3 @@\n+add-10\n ctx-11\n-gone\n+add-12\n";
        let added = added_lines_from_diff(diff);
        assert_eq!(added, HashSet::from([2, 10, 12]), "added new-side lines");
        assert!(!added.contains(&11), "context line 11 not added: {added:?}");
    }

    #[test]
    fn relative_time_labels() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now - 30, now), "just now");
        assert_eq!(relative_time(now - 120, now), "2 minutes ago");
        assert_eq!(relative_time(now - 3 * 3600, now), "3 hours ago");
        assert_eq!(relative_time(now - 24 * 3600, now), "1 day ago");
        assert_eq!(relative_time(now - 40 * 86400, now), "1 month ago");
    }

    #[test]
    fn classify_diff_lines_by_prefix() {
        assert_eq!(classify_diff_line("@@ -1,2 +1,3 @@").kind, DiffKind::Hunk);
        assert_eq!(classify_diff_line("--- a/x.rs").kind, DiffKind::Header);
        assert_eq!(classify_diff_line("+++ b/x.rs").kind, DiffKind::Header);
        assert_eq!(
            classify_diff_line("diff --git a/x b/x").kind,
            DiffKind::Header
        );
        assert_eq!(classify_diff_line("+added line").kind, DiffKind::Add);
        assert_eq!(classify_diff_line("-removed line").kind, DiffKind::Remove);
        assert_eq!(classify_diff_line(" context").kind, DiffKind::Context);
    }

    #[test]
    fn is_hex40_detects_sha() {
        assert!(is_hex40(&"a".repeat(40)));
        assert!(is_hex40("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_hex40("abc"));
        assert!(!is_hex40(&"g".repeat(40)));
    }
}
