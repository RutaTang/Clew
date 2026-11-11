//! The project's history, the way an IDE's git log shows it: its commits a
//! page at a time, with their parents (the graph is drawn from them) and the
//! names that point at them, filtered by author, message and path; and one
//! commit whole — its message and the files it changed, with their lines
//! gained and lost.
//!
//! Read-only, like everything here. The arguments a reader types (the author
//! and message filters) reach git as the value of one option each, as plain
//! text (`--fixed-strings`), never as an argument of their own.

use clew_protocol::{
    CommitDetail, CommitFile, LogCommit, LogPage, LogRef, MAX_COMMIT_FILES, RefKind,
};

use super::*;

/// The most commits a page of the log may pass over: `--skip` walks every
/// one of them again for each page.
pub const MAX_LOG_SKIP: usize = 10_000_000;

/// The longest author or message text the log is filtered by.
const MAX_LOG_TEXT: usize = 256;

/// The log's format: [`RECORD`] at the start of each, then its fields,
/// separated by NUL — id, parents, author, time, the names pointing at it
/// (`--decorate=full`), and last the subject, so that a NUL in a subject can
/// only lengthen the subject.
const LOG_FORMAT: &str = "--format=%x00%x1e%H%x00%P%x00%an%x00%at%x00%D%x00%s";

/// One commit's head and message: id, parents, author, time, then all of its
/// message (which may run over many lines).
const DETAIL_FORMAT: &str = "--format=%x00%x1e%H%x00%P%x00%an%x00%at%x00%B";

/// `Err(Refused)` for an author or message filter that is empty, too long,
/// or holds a control character (a newline would be a second pattern).
pub fn check_log_text(text: &str) -> Result<(), GitError> {
    if text.trim().is_empty() {
        return Err(GitError::Refused("an empty filter".into()));
    }
    if text.len() > MAX_LOG_TEXT {
        return Err(GitError::Refused(format!(
            "a filter longer than {MAX_LOG_TEXT} bytes"
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(GitError::Refused(
            "a filter with a control character".into(),
        ));
    }
    Ok(())
}

/// A page of the project's commits, newest first: those HEAD reaches, or
/// with `all` those of every branch, remote branch and tag too; that touched
/// `rel` (a file or a folder, root-relative) or anything under the project;
/// whose author holds `author` and whose message holds `grep` (plain text,
/// any case). The first `skip` are passed over and at most `limit` come
/// back; `more` says whether others follow. Empty in a repository with no
/// commit yet.
pub fn project_log(
    root: &Path,
    all: bool,
    rel: Option<&str>,
    author: Option<&str>,
    grep: Option<&str>,
    skip: usize,
    limit: usize,
) -> Result<LogPage, GitError> {
    if let Some(rel) = rel {
        check_rel(rel)?;
    }
    for text in [author, grep].into_iter().flatten() {
        check_log_text(text)?;
    }
    if skip > MAX_LOG_SKIP {
        return Err(GitError::Refused(format!(
            "an offset past {MAX_LOG_SKIP} commits"
        )));
    }
    let git = Git::open(root)?;
    // One more than asked: whether it comes says whether more follow.
    let mut args: Vec<String> = vec![
        "log".into(),
        "--no-color".into(),
        "--decorate=full".into(),
        LOG_FORMAT.into(),
        format!("-n{}", limit.saturating_add(1)),
        format!("--skip={skip}"),
        "--regexp-ignore-case".into(),
        "--fixed-strings".into(),
    ];
    if let Some(author) = author {
        args.push(format!("--author={author}"));
    }
    if let Some(grep) = grep {
        args.push(format!("--grep={grep}"));
    }
    if all {
        args.extend(["--branches", "--remotes", "--tags"].map(String::from));
    }
    args.extend([END_OF_OPTIONS, "HEAD", "--"].map(String::from));
    args.push(rel.map_or_else(|| ".".to_string(), |rel| git_name(root, rel)));
    let out = match git.text(&args, GIT_HISTORY_TIMEOUT) {
        Ok(out) => out,
        Err(e @ GitError::Failed { .. }) => {
            return if git.has_head()? {
                Err(e)
            } else {
                Ok(LogPage {
                    commits: Vec::new(),
                    more: false,
                })
            };
        }
        Err(e) => return Err(e),
    };
    let mut commits: Vec<LogCommit> = records(&out)
        .into_iter()
        .filter_map(|record| parse_log_header(record.split('\n').next().unwrap_or("")))
        .collect();
    let more = commits.len() > limit;
    commits.truncate(limit);
    Ok(LogPage { commits, more })
}

/// One record of [`LOG_FORMAT`]: `None` for one that is not a commit's.
fn parse_log_header(head: &str) -> Option<LogCommit> {
    let mut f = head.splitn(6, '\0');
    let sha = f.next()?;
    if !is_full_sha(sha) {
        return None;
    }
    let parents = f.next()?;
    let author = f.next()?;
    let time = f.next()?;
    let names = f.next()?;
    let subject = f.next().unwrap_or("");
    Some(LogCommit {
        sha: sha.to_string(),
        parents: parents
            .split(' ')
            .filter(|p| is_full_sha(p))
            .map(String::from)
            .collect(),
        author: author.to_string(),
        time: time.trim().parse().unwrap_or(0),
        subject: subject.to_string(),
        refs: parse_refs(names),
    })
}

/// The names `%D` lists with `--decorate=full`, short: `HEAD -> refs/heads/x`
/// (the branch checked out), `HEAD` (detached), `tag: refs/tags/v1`,
/// `refs/heads/x`, `refs/remotes/origin/x`. A remote's own `HEAD` and refs of
/// other kinds (the stash, notes) are left out. Names cannot hold a space
/// (git refuses one), so `", "` separates them.
fn parse_refs(names: &str) -> Vec<LogRef> {
    let mut refs = Vec::new();
    let mut push = |kind, name: &str| {
        if !name.is_empty() {
            refs.push(LogRef {
                kind,
                name: name.to_string(),
            });
        }
    };
    for part in names.split(", ").map(str::trim).filter(|p| !p.is_empty()) {
        if let Some(branch) = part.strip_prefix("HEAD -> ") {
            push(
                RefKind::Head,
                branch.strip_prefix("refs/heads/").unwrap_or(branch),
            );
        } else if part == "HEAD" {
            push(RefKind::Head, "HEAD");
        } else if let Some(tag) = part.strip_prefix("tag: ") {
            push(RefKind::Tag, tag.strip_prefix("refs/tags/").unwrap_or(tag));
        } else if let Some(branch) = part.strip_prefix("refs/heads/") {
            push(RefKind::Branch, branch);
        } else if let Some(remote) = part.strip_prefix("refs/remotes/") {
            if !remote.ends_with("/HEAD") {
                push(RefKind::Remote, remote);
            }
        }
    }
    refs
}

/// One commit whole: its message, its parents, and the files it changed
/// under the project against its first parent — against nothing for a root
/// commit — with the lines each gained and lost (none for a binary file).
/// `sha` may be any unambiguous prefix. `None` when it names no commit.
pub fn commit_detail(root: &Path, sha: &str) -> Result<Option<CommitDetail>, GitError> {
    check_sha(sha)?;
    let git = Git::open(root)?;
    let Some(full) = git.resolve(&format!("{sha}^{{commit}}"))? else {
        return Ok(None);
    };
    if !is_full_sha(&full) {
        return Ok(None);
    }
    let head = git.text(
        [
            "show",
            "-s",
            "--no-color",
            DETAIL_FORMAT,
            END_OF_OPTIONS,
            &full,
        ],
        GIT_TIMEOUT,
    )?;
    let Some(record) = records(&head).into_iter().next() else {
        return Ok(None);
    };
    let mut f = record.splitn(5, '\0');
    let (Some(id), Some(parents), Some(author), Some(time)) =
        (f.next(), f.next(), f.next(), f.next())
    else {
        return Ok(None);
    };
    let message = f.next().unwrap_or("").trim_end().to_string();
    let parents: Vec<String> = parents
        .split(' ')
        .filter(|p| is_full_sha(p))
        .map(String::from)
        .collect();
    let mut args: Vec<&str> = vec![
        "diff-tree",
        "-r",
        "-M",
        "-z",
        "--no-commit-id",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--raw",
        "--numstat",
        "--relative",
    ];
    match parents.first() {
        Some(first) => {
            args.extend([END_OF_OPTIONS, first.as_str(), full.as_str()]);
        }
        None => args.extend(["--root", END_OF_OPTIONS, full.as_str()]),
    }
    let changed = git.bytes(&args, GIT_TIMEOUT)?;
    let mut files = parse_raw_numstat(&changed);
    let more_files = files.len().saturating_sub(MAX_COMMIT_FILES);
    files.truncate(MAX_COMMIT_FILES);
    Ok(Some(CommitDetail {
        sha: id.to_string(),
        parents,
        author: author.to_string(),
        time: time.trim().parse().unwrap_or(0),
        message,
        files,
        more_files,
    }))
}

/// `diff-tree -z --raw --numstat`: the raw entries — `:<modes> <ids>
/// <status>` then its path, or for a rename or copy its old path and new —
/// and after them the numstat ones, in the same order — `<added>\t<removed>\t`
/// and its path, or an empty path and then the old and new; `-` for both
/// counts of a binary file. A path that is not UTF-8 leaves its file out.
fn parse_raw_numstat(out: &[u8]) -> Vec<CommitFile> {
    let mut fields = out.split(|&b| b == 0).peekable();
    let text = |f: &[u8]| std::str::from_utf8(f).ok().map(String::from);
    let mut files: Vec<Option<CommitFile>> = Vec::new();
    // The raw entries.
    while let Some(head) = fields.next_if(|f| f.first() == Some(&b':')) {
        let status = head
            .rsplit(|&b| b == b' ')
            .next()
            .and_then(|s| s.first())
            .map_or('M', |&b| b as char);
        let file = if matches!(status, 'R' | 'C') {
            let (old, new) = (fields.next(), fields.next());
            old.and_then(text)
                .zip(new.and_then(text))
                .map(|(old, rel)| CommitFile {
                    rel,
                    old: Some(old),
                    status,
                    added: None,
                    removed: None,
                })
        } else {
            fields.next().and_then(text).map(|rel| CommitFile {
                rel,
                old: None,
                status,
                added: None,
                removed: None,
            })
        };
        files.push(file);
    }
    // The numstat entries, one per raw one.
    for file in files.iter_mut() {
        let Some(stat) = fields.next() else {
            break;
        };
        let stat = String::from_utf8_lossy(stat);
        let mut parts = stat.splitn(3, '\t');
        let count = |s: Option<&str>| s.and_then(|s| s.parse::<u32>().ok());
        let (added, removed, path) = (count(parts.next()), count(parts.next()), parts.next());
        // A rename's paths come after it, each a field of its own.
        if path == Some("") {
            fields.next();
            fields.next();
        }
        if let Some(file) = file {
            file.added = added;
            file.removed = removed;
        }
    }
    files.into_iter().flatten().collect()
}
