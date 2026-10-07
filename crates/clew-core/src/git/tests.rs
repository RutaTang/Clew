use super::*;
use crate::testutil::TempDir;

/// A scratch repository: `git init` + identity, in a fresh temp dir that is
/// removed when the guard drops. `path()` is canonical (macOS's `/var` is a
/// link), which is how the server hands roots to this module.
struct TempRepo {
    _dir: TempDir,
    root: std::path::PathBuf,
}

impl std::ops::Deref for TempRepo {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.root
    }
}

fn repo_dir(tag: &str) -> TempRepo {
    let dir = TempDir::new(tag);
    let root = std::fs::canonicalize(dir.path()).unwrap();
    sh_git(&root, &["init", "-q"]);
    sh_git(&root, &["config", "user.email", "t@example.com"]);
    sh_git(&root, &["config", "user.name", "t"]);
    sh_git(&root, &["config", "commit.gpgsign", "false"]);
    TempRepo { _dir: dir, root }
}

/// Plain git for fixtures (not the hardened runner under test), isolated
/// from the developer's global and system config: a `commit.gpgsign`, a hook
/// directory or a template there must not decide what these tests see.
fn plain_git() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

/// Plain git for SETTING UP fixtures (not the hardened runner under test).
fn sh_git(dir: &Path, args: &[&str]) {
    let ok = plain_git()
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?} failed");
}

/// Plain git's stdout, for reading fixture facts (a sha).
fn sh_git_out(dir: &Path, args: &[&str]) -> String {
    let out = plain_git()
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn commit_all(dir: &Path, msg: &str) {
    sh_git(dir, &["add", "-A"]);
    sh_git(dir, &["commit", "-qm", msg]);
}

#[test]
fn parses_hunk_ranges() {
    assert_eq!(parse_hunk("-1,0 +2,3 @@ ctx"), Some(((1, 0), (2, 3))));
    assert_eq!(parse_hunk("-5 +5 @@"), Some(((5, 1), (5, 1))));
    assert_eq!(parse_hunk("-10,2 +0,0 @@"), Some(((10, 2), (0, 0))));
}

fn top() -> Repo {
    Repo {
        prefix: String::new(),
    }
}

#[test]
fn parses_file_history_records() {
    // Two commits, `--name-only` path after each header; the second is a
    // rename (old path), which must be captured as that commit's path.
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    let out = format!(
        "{RECORD}{a}\0Ada\x001700000000\0Add parser\n\nsrc/parser.rs\n\
         {RECORD}{b}\0Bo\x001699990000\0Initial\n\nsrc/parse.rs\n"
    );
    let h = parse_hist(&out, "src/parser.rs", &top(), HistPaths::NameOnly);
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].sha, a);
    assert_eq!(h[0].author, "Ada");
    assert_eq!(h[0].time, 1_700_000_000);
    assert_eq!(h[0].subject, "Add parser");
    assert_eq!(h[0].path, "src/parser.rs");
    assert_eq!(h[1].sha, b);
    assert_eq!(h[1].path, "src/parse.rs"); // rename followed
}

/// Every repository-controlled byte in the stream — subjects, author names,
/// `-L` patch bodies — tried as a record forger. A record can only start with
/// a NUL at the beginning of a line, fields are NUL-separated, and a sha must
/// be a full object id; none of those can be supplied from inside a subject,
/// a name, a patch line or a path.
#[test]
fn history_records_cannot_be_forged_from_repository_content() {
    let real = "0123456789abcdef0123456789abcdef01234567";
    let forged_sha = "f".repeat(40);
    // A subject and an author carrying the old separators (RS/FS) and a
    // hex-shaped "sha" — exactly what the old RS/FS split turned into an
    // extra record.
    let subject = format!("fix\x1e{forged_sha}\x1fMallory\x1f1\x1fforged");
    let author = format!("Eve\x1e{forged_sha}");
    let out = format!("{RECORD}{real}\0{author}\x001700000000\0{subject}\n\nsrc/f.rs\n");
    let h = parse_hist(&out, "src/f.rs", &top(), HistPaths::NameOnly);
    assert_eq!(h.len(), 1, "only the real commit: {h:?}");
    assert_eq!(h[0].sha, real);
    assert_eq!(h[0].subject, subject, "the subject survives intact");
    assert_eq!(h[0].author, author);

    // A `-L` patch whose added line carries a whole fake header: patch lines
    // begin with their marker, never with a NUL.
    let out = format!(
        "{RECORD}{real}\0Ada\x001700000000\0real\n\n\
         diff --git a/src/f.rs b/src/f.rs\n--- a/src/f.rs\n+++ b/src/f.rs\n\
         @@ -1,1 +1,2 @@\n fn a() {{}}\n+{RECORD}{forged_sha}\0Mallory\x001\0forged\n\
         ++++ b/elsewhere.rs\n"
    );
    let h = parse_hist(&out, "src/f.rs", &top(), HistPaths::Patch);
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!(h[0].sha, real);
    assert_eq!(
        h[0].path, "src/f.rs",
        "a patch body cannot re-point the path"
    );

    // A line-initial RS without the NUL — what a raw `-L` path with a newline
    // can put at the start of a line — starts nothing.
    let out = format!(
        "{RECORD}{real}\0Ada\x001700000000\0real\n\n\x1e{forged_sha}\0M\x001\0forged\n\nsrc/f.rs\n"
    );
    let h = parse_hist(&out, "src/f.rs", &top(), HistPaths::NameOnly);
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!(h[0].sha, real);

    // Shapes that are not a full object id never make a record.
    for sha in ["-n", "--output=/tmp/x", "..", "HEAD~1", "", "abc123"] {
        let rec = format!("{sha}\0A\x001700000000\0subj");
        assert!(
            parse_hist_header(&rec).is_none(),
            "sha {sha:?} must not parse"
        );
    }
    // SHA-256 repositories print 64 hex digits.
    assert!(parse_hist_header(&format!("{}\0A\x001\0s", "a".repeat(64))).is_some());
}

/// The same, end to end: a real commit whose subject carries the old
/// separators and a hex-shaped sha yields exactly its own record.
#[test]
fn a_hostile_subject_in_a_real_repository_forges_nothing() {
    let dir = repo_dir("git-forged-subject");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/f.rs"), "fn a() {}\n").unwrap();
    commit_all(&dir, "initial");
    std::fs::write(dir.join("src/f.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    let forged = format!("tweak\x1e{}\x1fMallory\x1f1\x1fforged", "e".repeat(40));
    commit_all(&dir, &forged);

    let h = file_history(&dir, "src/f.rs", 10).unwrap();
    assert_eq!(h.len(), 2, "{h:?}");
    assert!(h.iter().all(|c| is_full_sha(&c.sha) && c.author == "t"));
    assert_eq!(h[0].subject, forged);
    assert!(h.iter().all(|c| c.path == "src/f.rs"));
    let sym = symbol_history(&dir, "src/f.rs", 1, 2, 10).unwrap();
    assert!(!sym.is_empty());
    assert!(sym.iter().all(|c| c.author == "t"), "{sym:?}");
}

/// A project that is a SUBDIRECTORY of its repository: every path this
/// module returns is relative to the project root, and hands straight back to
/// `file_at`/`commit_added_lines` — across a rename, for a non-ASCII name, for
/// both history kinds.
#[test]
fn a_subdirectory_root_and_non_ascii_names_agree_on_one_path_space() {
    let dir = repo_dir("git-subdir-root");
    let sub = dir.join("sub");
    std::fs::create_dir_all(sub.join("src")).unwrap();
    // Big enough that one edited line keeps the rename above git's 50%
    // similarity threshold.
    let body: String = (0..12).map(|i| format!("fn f{i}() {{}}\n")).collect();
    std::fs::write(sub.join("src/f.rs"), format!("fn a() {{}}\n{body}")).unwrap();
    std::fs::write(sub.join("src/caf\u{e9}.rs"), "fn c() {}\n").unwrap();
    std::fs::write(dir.join("outside.rs"), "fn secret() {}\n").unwrap();
    commit_all(&dir, "first");
    sh_git(&dir, &["mv", "sub/src/f.rs", "sub/src/g.rs"]);
    std::fs::write(sub.join("src/g.rs"), format!("fn a() {{ 1 }}\n{body}")).unwrap();
    std::fs::write(sub.join("src/caf\u{e9}.rs"), "fn c() { 2 }\n").unwrap();
    commit_all(&dir, "second");

    let h = file_history(&sub, "src/g.rs", 10).unwrap();
    assert_eq!(
        h.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
        ["src/g.rs", "src/f.rs"],
        "root-relative, rename followed"
    );
    for c in &h {
        let content = file_at(&sub, &c.sha, &c.path)
            .unwrap()
            .expect("content at each step");
        assert!(content.contains("fn a()"), "{content}");
        assert!(
            !commit_added_lines(&sub, &c.sha, &c.path)
                .unwrap()
                .is_empty(),
            "each step highlights what it introduced ({})",
            c.path
        );
    }

    let sym = symbol_history(&sub, "src/g.rs", 1, 2, 10).unwrap();
    assert!(!sym.is_empty());
    assert_eq!(sym[0].path, "src/g.rs");
    for c in &sym {
        assert!(
            file_at(&sub, &c.sha, &c.path).unwrap().is_some(),
            "symbol history path {} resolves at {}",
            c.path,
            c.sha
        );
    }

    let cafe = file_history(&sub, "src/caf\u{e9}.rs", 10).unwrap();
    assert_eq!(cafe.len(), 2);
    assert!(
        cafe.iter().all(|c| c.path == "src/caf\u{e9}.rs"),
        "{cafe:?}"
    );
    assert_eq!(
        file_at(&sub, &cafe[0].sha, "src/caf\u{e9}.rs")
            .unwrap()
            .as_deref(),
        Some("fn c() { 2 }\n")
    );

    // Confinement: a rel can only ever name something under the project —
    // an escaping one is refused, an in-repository one outside the project
    // directory is simply not there.
    let head = &h[0].sha;
    assert!(matches!(
        file_at(&sub, head, "../outside.rs"),
        Err(GitError::Refused(_))
    ));
    assert_eq!(file_at(&sub, head, "outside.rs"), Ok(None));
    assert!(matches!(
        commit_file_diff(&sub, head, "../outside.rs", 1000),
        Err(GitError::Refused(_))
    ));
    // Pathspec magic is just characters.
    assert_eq!(file_history(&sub, ":(top)outside.rs", 10), Ok(Vec::new()));
    // A directory at that commit is not a file's text.
    assert_eq!(file_at(&sub, head, "src"), Ok(None));
}

/// A commit id, abbreviated or full — and nothing that git would read as an
/// option or a revision expression.
#[test]
fn is_hex_sha_accepts_only_commit_ids() {
    assert!(is_hex_sha("abc123"));
    assert!(is_hex_sha(&"a".repeat(40)));
    assert!(is_hex_sha(&"0".repeat(64)));
    assert!(
        !is_hex_sha("abc"),
        "too short to be an abbreviation git takes"
    );
    assert!(!is_hex_sha(&"a".repeat(65)));
    assert!(!is_hex_sha(""));
    assert!(!is_hex_sha("--output=/tmp/x"));
    assert!(!is_hex_sha("-n"));
    assert!(!is_hex_sha("HEAD"));
    assert!(!is_hex_sha("abc123:../etc/passwd"));
}

/// The sinks are the place the damage happens: with the sha in an argv slot
/// git parses as an option, `git show --output=<path>` truncates and rewrites
/// that path. They must refuse the sha themselves — as a refusal, not as an
/// empty answer — so a caller that skipped the parser still cannot write a
/// file.
#[test]
fn show_sinks_refuse_an_option_shaped_sha() {
    let dir = repo_dir("git-option-sha");
    std::fs::write(dir.join("f.rs"), "fn main() {}\n").unwrap();
    commit_all(&dir, "initial");
    let target = dir.join("PRECIOUS");
    std::fs::write(&target, "keep-me").unwrap();
    let sha = format!("--output={}", target.display());

    let refused = |r: Result<(), GitError>| matches!(r, Err(GitError::Refused(_)));
    assert!(refused(file_at(&dir, &sha, "f.rs").map(drop)));
    assert!(refused(commit_added_lines(&dir, &sha, "f.rs").map(drop)));
    assert!(refused(commit_message(&dir, &sha).map(drop)));
    assert!(refused(
        commit_file_diff(&dir, &sha, "f.rs", 1000).map(drop)
    ));
    assert!(refused(
        range_patch(&dir, "--output=/tmp/x", 1000).map(drop)
    ));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "keep-me",
        "git must not have been allowed to rewrite the file"
    );
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

/// `\ No newline at end of file` describes the line before it; counting it as
/// a line of its own shifted everything after it down by one.
#[test]
fn added_lines_ignore_the_no_newline_marker() {
    // A one-line file, rewritten, with no trailing newline on either side.
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                @@ -1 +1 @@\n-old\n\\ No newline at end of file\n\
                +new\n\\ No newline at end of file\n";
    assert_eq!(added_lines_from_diff(diff), HashSet::from([1]));

    // And it must not shift the lines that follow it in a later hunk.
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                @@ -1,2 +1,2 @@\n ctx\n-old\n\\ No newline at end of file\n\
                +new\n\\ No newline at end of file\n";
    assert_eq!(added_lines_from_diff(diff), HashSet::from([2]));
}

/// Content that looks like a file header is content inside a hunk: an added
/// `++x` prints as `+++x`, a removed `--x` as `---x`. Treating them as headers
/// dropped the addition and shifted every later line number.
#[test]
fn header_shaped_content_lines_are_counted_as_content() {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                @@ -1,2 +1,3 @@\n ctx\n+++x\n----y\n+z\n ctx2\n";
    assert_eq!(added_lines_from_diff(diff), HashSet::from([2, 3]));

    let lines = classify_diff(diff);
    let kinds: Vec<DiffKind> = lines.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        [
            DiffKind::Header,
            DiffKind::Header,
            DiffKind::Header,
            DiffKind::Hunk,
            DiffKind::Context,
            DiffKind::Add,
            DiffKind::Remove,
            DiffKind::Add,
            DiffKind::Context,
        ]
    );
}

/// With `diff.suppressBlankEmpty` a blank context line is printed as an EMPTY
/// line. Reading that as the end of the hunk dropped every addition after the
/// first blank line of context. The runner pins the setting off, and the
/// parser treats an empty in-hunk line as the context line it is.
#[test]
fn a_blank_context_line_does_not_end_the_hunk() {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                @@ -1,3 +1,4 @@\n x\n\n+new\n y\n";
    assert_eq!(added_lines_from_diff(diff), HashSet::from([3]));

    // Through git, in a repository that turns the setting on.
    let dir = repo_dir("git-blank-context");
    sh_git(&dir, &["config", "diff.suppressBlankEmpty", "true"]);
    std::fs::write(dir.join("f.txt"), "x\n\ny\n").unwrap();
    commit_all(&dir, "base");
    std::fs::write(dir.join("f.txt"), "x\n\nnew\ny\n").unwrap();
    commit_all(&dir, "insert after a blank line");
    let head = sh_git_out(&dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        commit_added_lines(&dir, &head, "f.txt").unwrap(),
        HashSet::from([3])
    );
    let diff = commit_file_diff(&dir, &head, "f.txt", 10_000).unwrap();
    assert!(
        diff.lines().all(|l| !l.is_empty()),
        "the blank context line keeps its marker: {diff:?}"
    );
}

/// A merge commit's `git show` is a COMBINED diff: one marker column per
/// parent. ` -` is a line the merge dropped from the second parent — it has no
/// result-side number — and ` +`/`+ `/`++` are result lines new to at least
/// one parent.
#[test]
fn merge_commit_combined_diffs_are_numbered_on_the_result_side() {
    let diff = "diff --cc f.txt\nindex 1111111,2222222..3333333\n--- a/f.txt\n+++ b/f.txt\n\
                @@@ -1,3 -1,3 +1,4 @@@\n\
                \x20 common\n\
                \x20+from-second\n\
                + from-first\n\
                - gone-in-first\n\
                \x20-gone-in-second\n\
                ++both\n\
                \x20 tail\n";
    assert_eq!(added_lines_from_diff(diff), HashSet::from([2, 3, 4]));
    assert_eq!(
        parse_any_hunk_header("@@@ -1,3 -1,3 +7,4 @@@ fn x()"),
        Some((2, 7))
    );
    assert_eq!(parse_any_hunk_header("@@ -1 +1 @@"), Some((1, 1)));
    assert_eq!(parse_any_hunk_header("@ nope"), None);
}

/// The same through git: a real merge whose resolution adds a line.
#[test]
fn a_real_merge_highlights_the_resolution() {
    let dir = repo_dir("git-merge");
    std::fs::write(dir.join("f.txt"), "a\nb\nc\n").unwrap();
    commit_all(&dir, "base");
    sh_git(&dir, &["checkout", "-qb", "side"]);
    std::fs::write(dir.join("f.txt"), "a\nb-side\nc\n").unwrap();
    commit_all(&dir, "side");
    sh_git(&dir, &["checkout", "-q", "-"]);
    std::fs::write(dir.join("f.txt"), "a\nb-main\nc\n").unwrap();
    commit_all(&dir, "main");
    let merged = plain_git()
        .args(["merge", "-q", "side"])
        .current_dir(&*dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!merged.success(), "the fixture needs a conflict");
    std::fs::write(dir.join("f.txt"), "a\nb-resolved\nextra\nc\n").unwrap();
    commit_all(&dir, "merge");
    let head = sh_git_out(&dir, &["rev-parse", "HEAD"]);
    let added = commit_added_lines(&dir, &head, "f.txt").unwrap();
    assert_eq!(added, HashSet::from([2, 3]), "the resolution's lines");
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
fn classify_diff_lines_by_position() {
    let text = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x.rs\n+++ b/x.rs\n\
                @@ -1,2 +1,3 @@\n context\n+added line\n-removed line\n";
    let kinds: Vec<DiffKind> = classify_diff(text).iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        [
            DiffKind::Header,
            DiffKind::Header,
            DiffKind::Header,
            DiffKind::Header,
            DiffKind::Hunk,
            DiffKind::Context,
            DiffKind::Add,
            DiffKind::Remove,
        ]
    );
}

#[test]
fn full_shas_are_40_or_64_hex_digits() {
    assert!(is_full_sha(&"a".repeat(40)));
    assert!(is_full_sha("0123456789abcdef0123456789abcdef01234567"));
    assert!(is_full_sha(&"b".repeat(64)));
    assert!(!is_full_sha("abc"));
    assert!(!is_full_sha(&"g".repeat(40)));
    assert!(!is_full_sha(&"a".repeat(41)));
}

/// SHA-256 repositories print 64-digit ids in blame's porcelain headers;
/// accepting only 40 left every line unattributed.
#[test]
fn blame_parses_sha256_headers() {
    let sha = "c".repeat(64);
    let porcelain = format!(
        "{sha} 1 1 1\nauthor Ada\nauthor-time 1700000000\nsummary init\nfilename f\n\tline\n"
    );
    let lines = parse_blame(&porcelain);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].author, "Ada");
    assert_eq!(lines[0].commit, "ccccccc");

    // End to end, where this git supports it.
    let scratch = TempDir::new("git-sha256");
    let init = plain_git()
        .args(["init", "-q", "--object-format=sha256"])
        .current_dir(scratch.path())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if init {
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        sh_git(&dir, &["config", "user.email", "t@example.com"]);
        sh_git(&dir, &["config", "user.name", "t"]);
        sh_git(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("f.rs"), "fn a() {}\n").unwrap();
        commit_all(&dir, "init");
        let info = try_info(&dir, &dir.join("f.rs"))
            .unwrap()
            .expect("blame in a SHA-256 repo");
        assert_eq!(info.blame.len(), 1);
        assert_eq!(info.blame[0].author, "t");
    }
}

/// `git diff HEAD -- <untracked>` succeeds with no output, which is what "no
/// changes" looks like; the documented `None` needs git asked directly.
/// A file opened through a folder link has its history, and its changes,
/// as the file the link leads to: git does not follow the link, and asked by
/// the name opened, answered none — Time Travel said "No git history" of a
/// file the gutter's blame, and the Ask agent's `history`, had commits for.
#[cfg(unix)]
#[test]
fn a_file_through_a_folder_link_has_its_own_history() {
    let dir = repo_dir("git-through-link");
    std::fs::create_dir_all(dir.join("src/real")).unwrap();
    std::fs::write(dir.join("src/real/x.rs"), "fn x() {}\n").unwrap();
    commit_all(&dir, "Add x");
    std::os::unix::fs::symlink(dir.join("src/real"), dir.join("src/linked")).unwrap();
    let history = |rel: &str| match run_op(
        &dir,
        clew_protocol::GitOp::FileHistory {
            rel: rel.into(),
            limit: 10,
        },
    ) {
        Ok(clew_protocol::GitResult::FileHistory(commits)) => commits,
        other => panic!("{rel}: {other:?}"),
    };
    for rel in ["src/real/x.rs", "src/linked/x.rs"] {
        let commits = history(rel);
        assert_eq!(commits.len(), 1, "{rel}");
        assert_eq!(commits[0].subject, "Add x");
    }
    std::fs::write(dir.join("src/real/x.rs"), "fn x() { 1 }\n").unwrap();
    match run_op(
        &dir,
        clew_protocol::GitOp::DiffLines {
            rel: "src/linked/x.rs".into(),
        },
    ) {
        Ok(clew_protocol::GitResult::DiffLines(Some(lines))) => {
            assert!(lines.iter().any(|l| l.kind == DiffKind::Add), "{lines:?}");
        }
        other => panic!("no changes through the link: {other:?}"),
    }
}

#[test]
fn diff_lines_tells_untracked_from_unchanged() {
    let dir = repo_dir("git-untracked");
    std::fs::write(dir.join("tracked.rs"), "fn a() {}\n").unwrap();
    commit_all(&dir, "init");
    std::fs::write(dir.join("untracked.rs"), "fn b() {}\n").unwrap();

    assert_eq!(diff_lines(&dir, &dir.join("untracked.rs")), Ok(None));
    assert_eq!(
        diff_lines(&dir, &dir.join("tracked.rs")).map(|d| d.map(|d| d.len())),
        Ok(Some(0)),
        "tracked and unchanged: an empty diff"
    );
    std::fs::write(dir.join("tracked.rs"), "fn a() { 1 }\n").unwrap();
    let d = diff_lines(&dir, &dir.join("tracked.rs")).unwrap().unwrap();
    assert!(d.iter().any(|l| l.kind == DiffKind::Add));
}

/// What is an EMPTY answer and what is an ERROR: a path `HEAD` does not have
/// and a repository without a commit have no history and no blame; a range
/// `HEAD`'s file does not have, or a malformed argument, is an error that
/// says so.
#[test]
fn empty_answers_are_real_answers_and_nothing_else_is() {
    let dir = repo_dir("git-empty-answers");
    std::fs::write(dir.join("staged.rs"), "fn s() {}\n").unwrap();
    sh_git(&dir, &["add", "staged.rs"]);

    // No commit yet.
    assert!(matches!(try_info(&dir, &dir.join("staged.rs")), Ok(None)));
    assert_eq!(file_history(&dir, "staged.rs", 5), Ok(Vec::new()));
    assert_eq!(symbol_history(&dir, "staged.rs", 1, 1, 5), Ok(Vec::new()));
    assert_eq!(review_base(&dir), Ok(None));
    assert_eq!(diff_lines(&dir, &dir.join("staged.rs")), Ok(None));

    commit_all(&dir, "first");
    std::fs::write(dir.join("untracked.rs"), "fn u() {}\n").unwrap();
    assert!(matches!(
        try_info(&dir, &dir.join("untracked.rs")),
        Ok(None)
    ));
    assert_eq!(file_history(&dir, "untracked.rs", 5), Ok(Vec::new()));
    assert_eq!(
        symbol_history(&dir, "untracked.rs", 1, 1, 5),
        Ok(Vec::new())
    );
    // One commit, no base branch to compare with: nothing to review.
    assert_eq!(review_base(&dir), Ok(None));

    let past_the_end = symbol_history(&dir, "staged.rs", 5, 9, 5).unwrap_err();
    assert!(
        matches!(past_the_end, GitError::Failed { ref stderr, .. } if stderr.contains("staged.rs")),
        "{past_the_end}"
    );
    assert!(matches!(
        symbol_history(&dir, "staged.rs", 0, 1, 5),
        Err(GitError::Refused(_))
    ));
    assert!(matches!(
        run_op(
            &dir,
            clew_protocol::GitOp::DiffLines {
                rel: "../x.rs".into()
            }
        ),
        Err(GitError::Refused(_))
    ));
}

/// Outside a repository every query that has a question to answer says so —
/// "no history" there was a lie — while the gutter and the diff view, which
/// document "nothing to show" for it, answer that.
#[test]
fn queries_outside_a_repository_say_so() {
    let scratch = TempDir::new("git-not-a-repo");
    let dir = std::fs::canonicalize(scratch.path()).unwrap();
    assert_eq!(file_history(&dir, "a.rs", 5), Err(GitError::NotARepository));
    assert_eq!(review_base(&dir), Err(GitError::NotARepository));
    assert_eq!(
        run_op(
            &dir,
            clew_protocol::GitOp::FileHistory {
                rel: "a.rs".into(),
                limit: 5
            }
        ),
        Err(GitError::NotARepository)
    );
    assert_eq!(GitError::NotARepository.to_string(), "not a git repository");
    assert!(matches!(try_info(&dir, &dir.join("a.rs")), Ok(None)));
    assert_eq!(diff_lines(&dir, &dir.join("a.rs")), Ok(None));
    assert_eq!(tracked_files(&dir), Ok(None));
}

/// A repository's own configuration must not get to run programs from a
/// read. Configured here: an external diff driver, a textconv filter, an
/// fsmonitor hook — and the clean/smudge/process FILTER drivers
/// `.gitattributes` applies to every file (the reviewer's reproduction: `diff
/// -U0 HEAD` and `blame` of a modified file ran `filter.evil.clean`), one of
/// them defined in a file the config includes and one with a dotted name.
/// None may execute, while the results still come back.
#[test]
#[cfg(unix)]
fn repository_configured_programs_never_run() {
    use std::os::unix::fs::PermissionsExt;
    let dir = repo_dir("git-hostile-config");
    let marker = dir.join("RAN");
    let script = dir.join("payload.sh");
    // Records that it ran, then behaves like a working filter (`cat`), so
    // nothing about the run would look like a failure.
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        dir.join(".gitattributes"),
        "*.rs diff=evil filter=evil\n*.txt filter=dotted.name\n*.md filter=included\n",
    )
    .unwrap();
    std::fs::write(dir.join("f.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "one\n").unwrap();
    std::fs::write(dir.join("readme.md"), "# one\n").unwrap();
    commit_all(&dir, "init");

    // Configured after the commit, the way a downloaded repository's
    // `.git/config` simply is.
    let script = script.display().to_string();
    sh_git(&dir, &["config", "diff.external", &script]);
    sh_git(&dir, &["config", "diff.evil.textconv", &script]);
    sh_git(&dir, &["config", "diff.evil.command", &script]);
    sh_git(&dir, &["config", "core.fsmonitor", &script]);
    sh_git(
        &dir,
        &["config", "filter.evil.clean", &format!("{script} clean")],
    );
    sh_git(
        &dir,
        &["config", "filter.evil.smudge", &format!("{script} smudge")],
    );
    sh_git(&dir, &["config", "filter.evil.required", "true"]);
    sh_git(&dir, &["config", "filter.dotted.name.process", &script]);
    let extra = dir.join("extra.cfg");
    std::fs::write(
        &extra,
        format!("[filter \"included\"]\n\tclean = {script} included\n"),
    )
    .unwrap();
    sh_git(
        &dir,
        &["config", "include.path", &extra.display().to_string()],
    );
    // A global-style ignore-revs file this repository does not have.
    sh_git(
        &dir,
        &["config", "blame.ignoreRevsFile", ".git-blame-ignore-revs"],
    );
    // Modified in the working tree, so every comparison must read the file —
    // through the filter, if one were allowed to run.
    std::fs::write(dir.join("f.rs"), "fn a() { 1 }\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "two\n").unwrap();
    std::fs::write(dir.join("readme.md"), "# two\n").unwrap();

    // The fixture reproduces the attack: plain git with every earlier
    // defence (no fsmonitor, no external diff, no textconv) still runs the
    // clean filter.
    let plain = plain_git()
        .args([
            "-c",
            "core.fsmonitor=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "-U0",
            "HEAD",
            "--",
            "f.rs",
        ])
        .current_dir(&*dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(plain.success());
    assert!(
        marker.exists(),
        "precondition: plain git runs the repository's filter"
    );
    std::fs::remove_file(&marker).unwrap();

    let head = sh_git_out(&dir, &["rev-parse", "HEAD"]);
    for file in ["f.rs", "notes.txt", "readme.md"] {
        let info = try_info(&dir, &dir.join(file))
            .unwrap()
            .expect("gutter info");
        assert_eq!(
            info.blame.len(),
            1,
            "blame survives a missing ignore-revs file"
        );
        assert!(!info.status.is_empty(), "{file} shows as changed");
        let diff = diff_lines(&dir, &dir.join(file)).unwrap().expect("a diff");
        assert!(diff.iter().any(|l| l.kind == DiffKind::Add));
    }
    assert!(
        !commit_file_diff(&dir, &head, "f.rs", 10_000)
            .unwrap()
            .is_empty()
    );
    assert!(!file_history(&dir, "f.rs", 5).unwrap().is_empty());
    assert!(!symbol_history(&dir, "f.rs", 1, 1, 5).unwrap().is_empty());
    assert_eq!(
        file_at(&dir, &head, "f.rs").unwrap().as_deref(),
        Some("fn a() {}\n")
    );
    assert!(!commit_added_lines(&dir, &head, "f.rs").unwrap().is_empty());
    assert!(
        !marker.exists(),
        "a repository-configured program ran: {:?}",
        std::fs::read_to_string(&marker)
    );
}

/// A driver name `-c` cannot spell (it splits at the first `=`) would leave
/// that filter running; the query is refused instead, and nothing runs.
#[test]
#[cfg(unix)]
fn a_filter_that_cannot_be_switched_off_refuses_the_query() {
    use std::os::unix::fs::PermissionsExt;
    let dir = repo_dir("git-unsafe-filter");
    let marker = dir.join("RAN");
    let script = dir.join("payload.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join(".gitattributes"), "*.rs filter=a=b\n").unwrap();
    std::fs::write(dir.join("f.rs"), "fn a() {}\n").unwrap();
    commit_all(&dir, "init");
    sh_git(
        &dir,
        &["config", "filter.a=b.clean", &script.display().to_string()],
    );
    std::fs::write(dir.join("f.rs"), "fn a() { 1 }\n").unwrap();

    let err = try_info(&dir, &dir.join("f.rs")).unwrap_err();
    assert!(matches!(err, GitError::UnsafeConfig(_)), "{err}");
    assert!(file_history(&dir, "f.rs", 5).is_err());
    assert!(!marker.exists(), "the filter ran");
}

/// Which drivers are switched off: every one the repository defines (at any
/// scope that is not the user's own), including a user driver the repository
/// redefines — and all of them when git cannot say where a key came from.
#[test]
fn only_the_repositorys_own_filter_drivers_are_switched_off() {
    let listing = b"global\0filter.lfs.clean\0global\0filter.lfs.required\0\
                    local\0filter.evil.clean\0worktree\0filter.wt.smudge\0\
                    system\0filter.sys.clean\0local\0filter.a.b.process\0\
                    local\0filter.lfs.process\0local\0filter.nodriver\0\
                    local\0filter.evil.required\0";
    let names: Vec<String> = drivers_to_switch_off(listing, true)
        .into_iter()
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .collect();
    assert_eq!(names, ["evil", "wt", "a.b", "lfs"]);

    let unscoped = b"filter.lfs.clean\0filter.evil.clean\0filter.lfs.smudge\0";
    let names: Vec<String> = drivers_to_switch_off(unscoped, false)
        .into_iter()
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .collect();
    assert_eq!(names, ["lfs", "evil"]);

    let overrides: Vec<String> = overrides_for(&[b"a.b".as_slice()])
        .unwrap()
        .into_iter()
        .map(|o| o.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        overrides,
        [
            "filter.a.b.clean=",
            "filter.a.b.smudge=",
            "filter.a.b.process=",
            "filter.a.b.required=false"
        ]
    );
    assert!(matches!(
        overrides_for(&[b"x=y".as_slice()]),
        Err(GitError::UnsafeConfig(_))
    ));
}

/// The partial-clone hint follows the runner's finding, not git's wording,
/// which 2.55 changed (see `names_a_missing_object`); the wording that names
/// the promisor remote itself still earns it, and a corrupt repository's
/// "bad file" alone does not.
#[test]
fn the_partial_clone_hint_needs_the_repository_to_be_one() {
    let failed = |stderr: &str, partial_clone: bool| {
        GitError::Failed {
            code: Some(128),
            stderr: stderr.into(),
            partial_clone,
        }
        .to_string()
    };
    let hint = "this is a partial clone";
    assert!(failed("fatal: git cat-file 6267 bad file", true).contains(hint));
    assert!(!failed("fatal: git cat-file 6267: bad file", false).contains(hint));
    assert!(failed("fatal: could not fetch 6267 from promisor remote", false).contains(hint));
    for wording in [
        "fatal: git cat-file 6267: bad file",
        "fatal: bad object 6267:f",
        "fatal: unable to read 6267",
        "fatal: Not a valid object name 6267",
        "fatal: could not fetch 6267 from promisor remote",
    ] {
        assert!(names_a_missing_object(wording), "{wording}");
    }
    assert!(!names_a_missing_object("fatal: ambiguous argument 'nope'"));
}

/// Change frequency: each file's commits among the last N (newest first,
/// so a file's latest time is the newest commit touching it), most changed
/// first then by path, merges left out, quoted names read back, the window
/// honoured, and nothing for a repository without commits.
#[test]
fn churn_counts_each_files_commits_over_the_recent_history() {
    let dir = repo_dir("git-churn");
    assert!(churn(&dir, 300).unwrap().is_empty(), "no commits yet");
    std::fs::write(dir.join("a.rs"), "1\n").unwrap();
    std::fs::write(dir.join("b.rs"), "1\n").unwrap();
    std::fs::write(dir.join("ünï.rs"), "1\n").unwrap();
    commit_all(&dir, "one");
    std::fs::write(dir.join("a.rs"), "2\n").unwrap();
    commit_all(&dir, "two");
    // A merge commit is not a change of the files it brings together.
    sh_git(&dir, &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.join("b.rs"), "2\n").unwrap();
    commit_all(&dir, "side");
    sh_git(&dir, &["checkout", "-q", "-"]);
    std::fs::write(dir.join("a.rs"), "3\n").unwrap();
    commit_all(&dir, "three");
    sh_git(
        &dir,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let latest: i64 = sh_git_out(&dir, &["log", "-1", "--format=%at", "--no-merges"])
        .parse()
        .unwrap();

    let files = churn(&dir, 300).unwrap();
    let counted: Vec<(&str, u32)> = files.iter().map(|f| (f.rel.as_str(), f.commits)).collect();
    assert_eq!(
        counted,
        [("a.rs", 3), ("b.rs", 2), ("ünï.rs", 1)],
        "{files:?}"
    );
    assert_eq!(files[0].last, latest, "a.rs's latest is the newest commit");
    assert!(files[2].last <= files[0].last);
    // The window: the newest commit alone (the merge does not count).
    let recent = churn(&dir, 1).unwrap();
    assert_eq!(
        recent.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(),
        ["a.rs"],
        "{recent:?}"
    );

    // A file deleted (or renamed away) since is history, not a file the
    // reader can open: it leaves the list, however often it changed.
    sh_git(&dir, &["rm", "-q", "b.rs"]);
    sh_git(&dir, &["mv", "ünï.rs", "moved.rs"]);
    commit_all(&dir, "drop b, rename ünï");
    let files = churn(&dir, 300).unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
    assert_eq!(names, ["a.rs", "moved.rs"], "{files:?}");
}

/// A blobless clone fetches old blobs on demand from its promisor remote, and
/// the remote's URL is repository configuration: `ext::` runs a command. That
/// fetch must never happen — the object is simply missing, which is an error
/// that says so — and nothing it names may run.
#[test]
#[cfg(unix)]
fn a_partial_clone_never_fetches_through_its_remote() {
    let scratch = TempDir::new("git-lazy-fetch");
    let base = std::fs::canonicalize(scratch.path()).unwrap();
    let src = base.join("src");
    std::fs::create_dir_all(&src).unwrap();
    sh_git(&src, &["init", "-q"]);
    sh_git(&src, &["config", "user.email", "t@example.com"]);
    sh_git(&src, &["config", "user.name", "t"]);
    sh_git(&src, &["config", "commit.gpgsign", "false"]);
    sh_git(&src, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(src.join("f.txt"), "v1\n").unwrap();
    commit_all(&src, "one");
    let first = sh_git_out(&src, &["rev-parse", "HEAD"]);
    std::fs::write(src.join("f.txt"), "v2\n").unwrap();
    commit_all(&src, "two");
    sh_git(
        &base,
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            &format!("file://{}", src.display()),
            "part",
        ],
    );
    let part = base.join("part");
    let marker = base.join("FETCHED");
    sh_git(
        &part,
        &[
            "config",
            "remote.origin.url",
            &format!("ext::sh -c touch% {}% ;exit% 1", marker.display()),
        ],
    );
    sh_git(&part, &["config", "protocol.ext.allow", "always"]);

    // The fixture reproduces the attack with plain git.
    let _ = plain_git()
        .args(["show", &format!("{first}:f.txt")])
        .current_dir(&part)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    assert!(
        marker.exists(),
        "precondition: plain git lazily fetches through the ext:: remote"
    );
    std::fs::remove_file(&marker).unwrap();

    let err = file_at(&part, &first, "f.txt").unwrap_err();
    assert!(matches!(err, GitError::Failed { .. }), "{err}");
    assert!(
        err.to_string().contains("partial clone"),
        "the reader is told why: {err}"
    );
    let _ = commit_added_lines(&part, &first, "f.txt");
    let _ = symbol_history(&part, "f.txt", 1, 1, 10);
    let _ = file_history(&part, "f.txt", 10);
    // What IS local still answers.
    assert_eq!(
        file_at(&part, &sh_git_out(&part, &["rev-parse", "HEAD"]), "f.txt"),
        Ok(Some("v2\n".to_string()))
    );
    assert!(!marker.exists(), "a lazy fetch ran the remote's command");
}

/// A git that ignores `GIT_NO_LAZY_FETCH` (older than 2.44, and not patched)
/// still fetches lazily — and the `-c protocol.*` pins only outrank a
/// repository's `protocol.<name>.allow` for the names they spell out. Here the
/// repository enables a protocol they do not (a remote helper), exactly as it
/// could enable `ftp` and have clew dial its host. `GIT_ALLOW_PROTOCOL` is what
/// closes that: with it set, no configuration can allow a transport.
#[test]
#[cfg(unix)]
fn a_git_that_ignores_the_lazy_fetch_switch_still_fetches_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = TempDir::new("git-lazy-fetch-old");
    let base = std::fs::canonicalize(scratch.path()).unwrap();
    let src = base.join("src");
    std::fs::create_dir_all(&src).unwrap();
    sh_git(&src, &["init", "-q"]);
    sh_git(&src, &["config", "user.email", "t@example.com"]);
    sh_git(&src, &["config", "user.name", "t"]);
    sh_git(&src, &["config", "commit.gpgsign", "false"]);
    sh_git(&src, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(src.join("f.txt"), "v1\n").unwrap();
    commit_all(&src, "one");
    let first = sh_git_out(&src, &["rev-parse", "HEAD"]);
    std::fs::write(src.join("f.txt"), "v2\n").unwrap();
    commit_all(&src, "two");
    sh_git(
        &base,
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            &format!("file://{}", src.display()),
            "part",
        ],
    );
    let part = base.join("part");
    // The remote helper the repository names: it only records that it ran.
    let marker = base.join("FETCHED");
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let helper = bin.join("git-remote-clewtest");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    sh_git(&part, &["config", "remote.origin.url", "clewtest::x"]);
    sh_git(&part, &["config", "protocol.clewtest.allow", "always"]);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // The hardened command as a git without the lazy-fetch switch runs it.
    let show = |allow_list: bool| {
        let mut cmd = base_command(&part);
        cmd.env_remove("GIT_NO_LAZY_FETCH").env("PATH", &path);
        if !allow_list {
            cmd.env_remove("GIT_ALLOW_PROTOCOL");
        }
        cmd.args(["show", END_OF_OPTIONS, &format!("{first}:f.txt")]);
        execute(cmd, GIT_TIMEOUT, Cap::Fail(1024 * 1024)).unwrap()
    };

    // The fixture reproduces the residual: the `-c` pins alone let the
    // repository's own protocol through.
    let _ = show(false);
    assert!(
        marker.exists(),
        "precondition: without GIT_ALLOW_PROTOCOL the repository's protocol is allowed"
    );
    std::fs::remove_file(&marker).unwrap();

    let err = show(true).success().unwrap_err();
    assert!(matches!(err, GitError::Failed { .. }), "{err}");
    assert!(!marker.exists(), "a lazy fetch ran the repository's helper");
}

/// A submodule is a repository with a configuration of its own, and git runs
/// a second git INSIDE it for two things a superproject can ask for: a
/// submodule change shown as a patch (`diff.submodule=diff` — the reviewer's
/// reproduction, where the Review prompt's `range_patch` ran the submodule's
/// `diff.external`), and a working-tree comparison of the submodule entry
/// (`git status` in it, which runs its clean filter on a file whose stat
/// changed). None of clew's other defences reach that child, so neither may
/// happen — while every query still answers.
#[test]
#[cfg(unix)]
fn a_submodule_never_runs_its_own_configured_programs() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = TempDir::new("git-submodule-programs");
    let base = std::fs::canonicalize(scratch.path()).unwrap();
    let marker = base.join("RAN");
    let script = base.join("payload.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let identity = ["-c", "user.email=t@example.com", "-c", "user.name=t"];
    let git_as = |dir: &Path, args: &[&str]| {
        let all: Vec<&str> = identity.iter().chain(args).copied().collect();
        sh_git(dir, &all);
    };

    // The submodule's upstream: its files go through a filter and a diff
    // driver its own configuration will define.
    let sub = base.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    sh_git(&sub, &["init", "-q"]);
    std::fs::write(
        sub.join(".gitattributes"),
        "*.txt filter=subevil diff=subevil\n",
    )
    .unwrap();
    std::fs::write(sub.join("f.txt"), "v1\n").unwrap();
    git_as(&sub, &["add", "-A"]);
    git_as(&sub, &["commit", "-qm", "one"]);

    // The superproject: the submodule checked out, and a commit that moves it.
    let sup = base.join("super");
    std::fs::create_dir_all(&sup).unwrap();
    sh_git(&sup, &["init", "-q"]);
    let sub_url = sub.display().to_string();
    git_as(
        &sup,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &sub_url,
            "sub",
        ],
    );
    git_as(&sup, &["commit", "-qm", "base"]);
    sh_git(&sup, &["branch", "-f", "main"]);
    let checkout = sup.join("sub");
    std::fs::write(checkout.join("f.txt"), "v2\n").unwrap();
    git_as(&checkout, &["commit", "-qam", "two"]);
    git_as(&sup, &["add", "sub"]);
    git_as(&sup, &["commit", "-qm", "bump"]);
    let head = sh_git_out(&sup, &["rev-parse", "HEAD"]);

    // Configured after the fact, the way a downloaded repository simply is:
    // the superproject asks for submodule patches, the submodule's own
    // configuration (in the superproject's `.git/modules`) names programs.
    let script = script.display().to_string();
    sh_git(&sup, &["config", "diff.submodule", "diff"]);
    sh_git(&checkout, &["config", "diff.external", &script]);
    sh_git(&checkout, &["config", "diff.subevil.textconv", &script]);
    sh_git(
        &checkout,
        &["config", "filter.subevil.clean", &format!("{script} clean")],
    );
    // Same size, new mtime: only reading the file (through the filter) can
    // tell whether it changed.
    std::fs::write(checkout.join("f.txt"), "v3\n").unwrap();

    // The fixture reproduces both attacks with plain git and clew's other
    // defences in place: the patch child runs the external diff ...
    let plain = |args: &[&str]| {
        let _ = plain_git()
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(&sup)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let ran = marker.exists();
        let _ = std::fs::remove_file(&marker);
        ran
    };
    assert!(
        plain(&["diff", "--no-ext-diff", "--no-textconv", "main...HEAD"]),
        "precondition: a submodule patch runs the submodule's own diff program"
    );
    // ... and the status child runs the clean filter.
    assert!(
        plain(&[
            "-c",
            "diff.submodule=short",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
            "--",
            "sub",
        ]),
        "precondition: comparing the submodule's working tree runs its clean filter"
    );

    assert_eq!(
        review_base(&sup).unwrap().map(|(base, _)| base).as_deref(),
        Some("main")
    );
    let patch = range_patch(&sup, "main", 100_000).unwrap();
    assert!(patch.contains("Subproject commit"), "{patch}");
    assert!(!changed_files(&sup, "main").unwrap().is_empty());
    assert!(
        commit_file_diff(&sup, &head, "sub", 10_000)
            .unwrap()
            .contains("Subproject commit")
    );
    let _ = diff_lines(&sup, &checkout).unwrap();
    let _ = try_info(&sup, &checkout);
    let _ = file_history(&sup, "sub", 5).unwrap();
    assert!(
        !marker.exists(),
        "a submodule's configured program ran: {:?}",
        std::fs::read_to_string(&marker)
    );

    // A submodule added and not committed yet: HEAD has no gitlink there, so
    // blame answers nothing and the gutter's working-tree diff runs — the
    // one path into `diff_status` for a submodule (the committed one above
    // stops at blame).
    git_as(
        &sup,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &sub_url,
            "added",
        ],
    );
    let added = sup.join("added");
    sh_git(
        &added,
        &["config", "filter.subevil.clean", &format!("{script} clean")],
    );
    std::fs::write(added.join("f.txt"), "v9\n").unwrap();
    assert!(
        plain(&[
            "-c",
            "diff.submodule=short",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
            "--",
            "added",
        ]),
        "precondition: comparing the added submodule's working tree runs its clean filter"
    );
    let _ = try_info(&sup, &added);
    assert!(
        !marker.exists(),
        "the gutter's diff ran an added submodule's filter: {:?}",
        std::fs::read_to_string(&marker)
    );
}

/// `-L` prints historical paths RAW. A name that, split at its newlines,
/// reads as the end of one record and a header naming another file must
/// neither cut the record short (the old RS marker) nor relabel it (the old
/// first-`+++` read) — every step keeps the path it really had, and resolves.
#[test]
#[cfg(unix)]
fn a_newline_in_a_historical_path_neither_splits_nor_relabels_records() {
    let dir = repo_dir("git-newline-path");
    let forged_dir = format!("x\n\x1e{}\n+++ b", "e".repeat(40));
    let hostile = format!("{forged_dir}/src/other.rs\n@@ -0,0 +1 @@\ny.rs");
    std::fs::create_dir_all(dir.join(&forged_dir).join("src")).unwrap();
    let body: String = (0..12).map(|i| format!("fn f{i}() {{}}\n")).collect();
    std::fs::write(dir.join(&hostile), format!("fn a() {{}}\n{body}")).unwrap();
    // The path the forgery points at exists, so a mislabelled record would
    // even "resolve" — to the wrong file.
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/other.rs"), "fn decoy() {}\n").unwrap();
    commit_all(&dir, "hostile name");
    sh_git(&dir, &["mv", &hostile, "f.rs"]);
    std::fs::write(dir.join("f.rs"), format!("fn a() {{ 1 }}\n{body}")).unwrap();
    commit_all(&dir, "renamed");

    let sym = symbol_history(&dir, "f.rs", 1, 2, 10).unwrap();
    assert_eq!(
        sym.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
        ["f.rs", hostile.as_str()],
        "{sym:?}"
    );
    for c in &sym {
        assert!(is_full_sha(&c.sha));
        let content = file_at(&dir, &c.sha, &c.path)
            .unwrap()
            .expect("each step resolves");
        assert!(content.contains("fn a()"), "{content}");
    }
    let hist = file_history(&dir, "f.rs", 10).unwrap();
    assert_eq!(
        hist.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
        ["f.rs", hostile.as_str()]
    );
}

/// The `-L` header decoder: plain headers, an added file, a rename; a header
/// forged inside a raw name is never believed (it coexists with the true
/// decoding, so the record is undecodable, never relabelled); the standard
/// quoted form of newer gits; and bounded work on a hostile body.
#[test]
fn patch_headers_decode_to_exactly_one_path_or_none() {
    let plain = "diff --git a/src/f.rs b/src/f.rs\n--- a/src/f.rs\n+++ b/src/f.rs\n\
                 @@ -1 +1 @@\n-a\n+b\n";
    assert_eq!(patch_path(plain), PatchPath::Named("src/f.rs".into()));
    let added = "\ndiff --git a/n.rs b/n.rs\n--- /dev/null\n+++ b/n.rs\n@@ -0,0 +1 @@\n+a\n";
    assert_eq!(patch_path(added), PatchPath::Named("n.rs".into()));
    let renamed = "diff --git a/old.rs b/new.rs\n--- a/old.rs\n+++ b/new.rs\n@@ -1 +1 @@\n-a\n+b\n";
    assert_eq!(patch_path(renamed), PatchPath::Named("new.rs".into()));
    // Names with spaces, and " b/" inside a name.
    let spaced = "diff --git a/a b/c.rs b/a b/c.rs\n--- a/a b/c.rs\n+++ b/a b/c.rs\n@@ -1 +1 @@\n";
    assert_eq!(patch_path(spaced), PatchPath::Named("a b/c.rs".into()));

    // X is a raw historical name holding a complete fake header and a fake
    // hunk line: read line by line, the record says `src/target.rs`.
    let x = "t b/src/target.rs\n--- a/t\n+++ b/src/target.rs\n@@ -0,0 +1 @@\n+zz";
    let forged = format!("diff --git a/{x} b/f.rs\n--- a/{x}\n+++ b/f.rs\n@@ -1 +1 @@\n-a\n+b\n");
    assert_eq!(patch_path(&forged), PatchPath::Undecodable);
    // A newline in a name without a forgery still decodes.
    let y = "we\nird.rs";
    let odd = format!("diff --git a/{y} b/f.rs\n--- a/{y}\n+++ b/f.rs\n@@ -1 +1 @@\n-a\n+b\n");
    assert_eq!(patch_path(&odd), PatchPath::Named("f.rs".into()));

    // The standard form: quoted names and extended header lines.
    let standard = "diff --git \"a/we\\nird.rs\" b/f.rs\nsimilarity index 91%\n\
                    rename from \"we\\nird.rs\"\nrename to f.rs\nindex 1111111..2222222 100644\n\
                    --- \"a/we\\nird.rs\"\n+++ b/f.rs\n@@ -1 +1 @@\n";
    assert_eq!(patch_path(standard), PatchPath::Named("f.rs".into()));
    let quoted_b = "diff --git \"a/we\\nird.rs\" \"b/we\\nird.rs\"\n--- \"a/we\\nird.rs\"\n\
                    +++ \"b/we\\nird.rs\"\n@@ -1 +1 @@\n";
    assert_eq!(patch_path(quoted_b), PatchPath::Named("we\nird.rs".into()));
    // A name with a space ends its `---`/`+++` label with a tab (git's
    // diff.c), quoted — git 2.55 quotes the names `-L` prints — or not.
    let tabbed = "diff --git \"a/we\\nird one.rs\" \"b/we\\nird one.rs\"\n\
                  index 1111111..2222222 100644\n--- \"a/we\\nird one.rs\"\t\n\
                  +++ \"b/we\\nird one.rs\"\t\n@@ -1 +1 @@\n";
    assert_eq!(
        patch_path(tabbed),
        PatchPath::Named("we\nird one.rs".into())
    );
    let spaced_tabbed =
        "diff --git a/a b/c.rs b/a b/c.rs\n--- a/a b/c.rs\t\n+++ b/a b/c.rs\t\n@@ -1 +1 @@\n";
    assert_eq!(
        patch_path(spaced_tabbed),
        PatchPath::Named("a b/c.rs".into())
    );

    assert_eq!(patch_path(""), PatchPath::NoDiff);
    assert_eq!(patch_path("\n\n"), PatchPath::NoDiff);
    assert_eq!(patch_path("hello\n"), PatchPath::Undecodable);

    // A body built to make the search quadratic stays cheap.
    let mut hostile = String::from("diff --git a/f b/f\n--- a/f\n+++ b/f\n");
    for _ in 0..20_000 {
        // An added line `++ b/x` prints as `+++ b/x`.
        hostile.push_str("@@ -1 +1 @@\n+++ b/x\n");
    }
    let started = Instant::now();
    let _ = patch_path(&hostile);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

/// The deadline and the cap belong to the runner, and hold for any child:
/// one that never finishes is killed on time, one that floods stdout is cut
/// off (or truncated, when the caller asked for that).
#[test]
#[cfg(unix)]
fn the_runner_kills_a_child_at_its_deadline_or_its_cap() {
    let started = Instant::now();
    let mut slow = Command::new("sh");
    slow.args(["-c", "sleep 30"]).stdin(Stdio::null());
    let r = execute(slow, Duration::from_millis(300), Cap::Fail(1024));
    assert!(matches!(r, Err(GitError::TimedOut { .. })), "{r:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the deadline held"
    );

    let mut flood = Command::new("sh");
    flood.args(["-c", "yes clew"]).stdin(Stdio::null());
    let r = execute(flood, Duration::from_secs(20), Cap::Fail(4096));
    assert!(
        matches!(r, Err(GitError::TooLarge { limit: 4096 })),
        "{r:?}"
    );

    let mut flood = Command::new("sh");
    flood.args(["-c", "yes clew"]).stdin(Stdio::null());
    let r = execute(flood, Duration::from_secs(20), Cap::Truncate(4096))
        .unwrap()
        .success()
        .unwrap();
    assert!(r.truncated);
    assert_eq!(r.bytes.len(), 4096);

    let mut fine = Command::new("sh");
    fine.args(["-c", "printf ok"]).stdin(Stdio::null());
    let r = execute(fine, Duration::from_secs(20), Cap::Fail(4096))
        .unwrap()
        .success()
        .unwrap();
    assert_eq!(r.bytes, b"ok");
    assert!(!r.truncated);
}

/// A kill reaches what git started: the child runs in its own process group
/// and the whole group goes. Killing only the direct child left a lazy fetch
/// (or its ssh) running — and holding the pipes — after the query had
/// "timed out".
#[test]
#[cfg(unix)]
fn a_timeout_kills_everything_the_child_started() {
    let dir = TempDir::new("git-process-group");
    let pidfile = dir.join("pid");
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        &format!("sleep 60 & echo $! > '{}'; wait", pidfile.display()),
    ])
    .stdin(Stdio::null());
    let r = execute(cmd, Duration::from_millis(500), Cap::Fail(1024));
    assert!(matches!(r, Err(GitError::TimedOut { .. })), "{r:?}");
    let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
        .expect("the grandchild's pid")
        .trim()
        .parse()
        .unwrap();
    // Signalled with its group, then reaped by init once its parent died.
    let alive = || unsafe { libc::kill(pid, 0) } == 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(), "the grandchild outlived the timeout");
}

/// A kill that lands while git's children are starting theirs stops those
/// too. One SIGKILL to the group did not: a child forked as it was sent could
/// join the group after the kernel went through it, and ran on, holding the
/// pipes. Here the output cap is what kills, part-way through the forking —
/// at a different point each time.
#[test]
#[cfg(unix)]
fn a_kill_while_the_child_forks_stops_everything_it_started() {
    let dir = TempDir::new("git-process-group-forking");
    for attempt in 0..8u32 {
        let group = dir.join(format!("group-{attempt}"));
        // Records its group, then starts helpers back to back (a bounded
        // number of them, whatever becomes of the kill), and after the first
        // few overflows the cap in the background: the forking goes on while
        // the kill comes.
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!(
                "echo $$ > '{}'; for i in $(seq 300); do sleep 30 & \
                 if [ $i = {} ]; then head -c 5000 /dev/zero & fi; done; wait",
                group.display(),
                10 + 20 * attempt
            ),
        ])
        .stdin(Stdio::null());
        // The deadline only bounds the script's start on a busy machine: the
        // cap is what ends the run.
        let r = execute(cmd, Duration::from_secs(60), Cap::Fail(4096));
        assert!(
            matches!(r, Err(GitError::TooLarge { limit: 4096 })),
            "{r:?}"
        );
        let pgid: libc::pid_t = std::fs::read_to_string(&group)
            .expect("the group's id")
            .trim()
            .parse()
            .unwrap();
        // The leader is reaped, and its helpers' zombies are init's to reap:
        // soon nothing in the group is left, where a helper the kill missed
        // would live on for 30 s.
        // SAFETY: signal 0 only probes; the group is this test's own.
        let alive = || unsafe { libc::killpg(pgid, 0) } == 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let survived = alive();
        if survived {
            // SAFETY: as above; the survivors are this test's helpers.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
        assert!(
            !survived,
            "a helper forked as git was killed outlived it (attempt {attempt})"
        );
    }
}

/// A child reaped behind git's back — another waiter took its exit — is
/// neither taken for still running nor signalled. Its lost exit read as
/// "still running", and it was killed as a timeout: its pid, and the group
/// it led, signalled — ids that were free for someone else's processes by
/// then — with rounds of signals for as long as the group had a member.
#[test]
#[cfg(unix)]
fn a_child_reaped_elsewhere_is_not_signalled() {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    // The leader starts a helper in its group, says its pid, and exits.
    let mut child = Command::new("sh")
        .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut said)
        .unwrap();
    let helper: libc::pid_t = said.trim().parse().expect("the helper's pid");
    // Someone else reaps the leader.
    let leader = child.id() as libc::pid_t;
    let mut status = 0;
    // SAFETY: waits for this test's own child.
    assert_eq!(unsafe { libc::waitpid(leader, &mut status, 0) }, leader);

    let began = Instant::now();
    let waited = wait_until(&mut child, Instant::now() + Duration::from_secs(5));
    assert!(matches!(waited, Waited::Lost(_)), "{waited:?}");
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "the lost exit was waited out"
    );
    // Nothing is signalled. The helper is still in the group the leader led
    // — here, still this test's — which a signal to it would have ended.
    kill(&mut child);
    // SAFETY: signal 0 only probes this test's own helper.
    let alive = || unsafe { libc::kill(helper, 0) } == 0;
    let watched = Instant::now() + Duration::from_millis(500);
    while alive() && Instant::now() < watched {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = alive();
    // SAFETY: the helper is this test's own.
    unsafe { libc::kill(helper, libc::SIGKILL) };
    assert!(
        survived,
        "the group of a child reaped elsewhere was signalled"
    );
}

/// What git said is kept (the end of it, where the `fatal:` line is) and
/// becomes the reason; a flood of stderr is drained rather than left to block
/// the child.
#[test]
#[cfg(unix)]
fn a_failure_keeps_what_git_said() {
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        "echo 'warning: noise' >&2; echo 'fatal: the real reason' >&2; exit 3",
    ])
    .stdin(Stdio::null());
    let err = execute(cmd, Duration::from_secs(20), Cap::Fail(1024))
        .unwrap()
        .success()
        .unwrap_err();
    assert_eq!(
        err,
        GitError::Failed {
            code: Some(3),
            stderr: "warning: noise; fatal: the real reason".into(),
            partial_clone: false,
        }
    );
    assert_eq!(
        err.to_string(),
        "git failed: warning: noise; fatal: the real reason"
    );

    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        "yes noise | head -c 1000000 >&2; echo 'fatal: last words' >&2; exit 1",
    ])
    .stdin(Stdio::null());
    let err = execute(cmd, Duration::from_secs(20), Cap::Fail(1024))
        .unwrap()
        .success()
        .unwrap_err();
    assert!(err.to_string().ends_with("fatal: last words"), "{err}");

    // Exit codes that ARE the answer.
    let mut yes = Command::new("sh");
    yes.args(["-c", "exit 0"]).stdin(Stdio::null());
    assert_eq!(
        execute(yes, Duration::from_secs(20), Cap::Fail(64))
            .unwrap()
            .answer(),
        Ok(true)
    );
    let mut no = Command::new("sh");
    no.args(["-c", "exit 1"]).stdin(Stdio::null());
    assert_eq!(
        execute(no, Duration::from_secs(20), Cap::Fail(64))
            .unwrap()
            .answer(),
        Ok(false)
    );
    let mut broken = Command::new("sh");
    broken
        .args(["-c", "echo 'fatal: bad' >&2; exit 128"])
        .stdin(Stdio::null());
    assert!(matches!(
        execute(broken, Duration::from_secs(20), Cap::Fail(64))
            .unwrap()
            .answer(),
        Err(GitError::Failed {
            code: Some(128),
            ..
        })
    ));
}

#[test]
fn git_versions_parse() {
    assert_eq!(
        parse_version("git version 2.39.5 (Apple Git-154)\n"),
        Some((2, 39, 5))
    );
    assert_eq!(
        parse_version("git version 2.45.1.windows.1"),
        Some((2, 45, 1))
    );
    assert_eq!(parse_version("git version 2.46.0-rc0"), Some((2, 46, 0)));
    assert_eq!(parse_version("git version 2.47"), Some((2, 47, 0)));
    assert_eq!(parse_version("nonsense"), None);
    assert!(
        installed_version().is_ok(),
        "the test host has a usable git"
    );
}

#[test]
fn unquotes_git_paths() {
    assert_eq!(unquote_path("src/a.rs").as_deref(), Some("src/a.rs"));
    assert_eq!(
        unquote_path(r#""tab\there.rs""#).as_deref(),
        Some("tab\there.rs")
    );
    assert_eq!(
        unquote_path(r#""q\"uote\\d.rs""#).as_deref(),
        Some("q\"uote\\d.rs")
    );
    assert_eq!(
        unquote_path(r#""caf\303\251.rs""#).as_deref(),
        Some("caf\u{e9}.rs")
    );
    assert_eq!(unquote_path(r#""\036rs""#).as_deref(), Some("\x1ers"));
    assert!(unquote_path(r#""broken\q""#).is_none());
    assert!(unquote_path(r#""\377""#).is_none(), "not UTF-8");
}

#[test]
fn name_status_z_takes_the_final_path_of_renames() {
    let out = b"M\0src/a.rs\0R100\0old.rs\0new name.rs\0A\0caf\xc3\xa9.rs\0";
    assert_eq!(
        parse_name_status_z(out),
        vec![
            ("src/a.rs".to_string(), 'M'),
            ("new name.rs".to_string(), 'R'),
            ("caf\u{e9}.rs".to_string(), 'A'),
        ]
    );
}

/// Review queries are scoped to the project directory and speak its paths:
/// a subdirectory project reviews its own changes only.
#[test]
fn review_queries_are_scoped_to_a_subdirectory_root() {
    let dir = repo_dir("git-review-scope");
    let sub = dir.join("app");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("other.rs"), "fn o() {}\n").unwrap();
    commit_all(&dir, "base");
    std::fs::write(sub.join("a.rs"), "fn a() { 1 }\n").unwrap();
    std::fs::write(dir.join("other.rs"), "fn o() { 1 }\n").unwrap();
    commit_all(&dir, "change both");

    let (base, _) = review_base(&sub).unwrap().expect("something to review");
    let files = changed_files(&sub, &base).unwrap();
    assert_eq!(files, vec![("a.rs".to_string(), 'M')]);
    let patch = range_patch(&sub, &base, 100_000).unwrap();
    assert!(
        patch.contains("a.rs") && !patch.contains("other.rs"),
        "{patch}"
    );
    assert_eq!(
        commit_subjects(&sub, &base).unwrap(),
        vec!["change both".to_string()]
    );
    let small = range_patch(&sub, &base, 16).unwrap();
    assert!(small.ends_with("… (truncated)\n"), "{small:?}");
}

/// The current work includes what is not committed yet: from where the
/// branch left its base (or from HEAD on the base itself) to the working
/// tree — the branch's commits and the edits on top, together.
#[test]
fn uncommitted_work_is_part_of_the_current_work() {
    let dir = repo_dir("git-work");
    assert!(!has_uncommitted(&dir).unwrap(), "no commit yet");
    std::fs::write(dir.join("a.rs"), "1\n").unwrap();
    std::fs::write(dir.join("b.rs"), "1\n").unwrap();
    commit_all(&dir, "start");
    assert!(!has_uncommitted(&dir).unwrap());
    std::fs::write(dir.join("a.rs"), "1\nedited\n").unwrap();
    assert!(has_uncommitted(&dir).unwrap());
    // On the base itself, the work is the edit alone.
    assert_eq!(
        work_changed_files(&dir, "HEAD").unwrap(),
        [("a.rs".to_string(), 'M')]
    );
    let patch = work_patch(&dir, "HEAD", 64 * 1024).unwrap();
    assert!(patch.contains("+edited"), "{patch}");
    // Staged counts too.
    sh_git(&dir, &["add", "a.rs"]);
    assert!(has_uncommitted(&dir).unwrap());
    sh_git(&dir, &["commit", "-qm", "edit a"]);
    assert!(!has_uncommitted(&dir).unwrap());

    // On a branch: its commit, and an uncommitted edit of another file.
    let base = sh_git_out(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
    sh_git(&dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("c.rs"), "new\n").unwrap();
    commit_all(&dir, "add c");
    std::fs::write(dir.join("b.rs"), "1\nwip\n").unwrap();
    let files = work_changed_files(&dir, &base).unwrap();
    assert_eq!(
        files,
        [("b.rs".to_string(), 'M'), ("c.rs".to_string(), 'A')],
        "the branch's commit and the edit on top"
    );
    let patch = work_patch_of(&dir, &base, "b.rs", 64 * 1024).unwrap();
    assert!(patch.contains("+wip") && !patch.contains("c.rs"), "{patch}");
    // The commits-only range still leaves the edit out.
    assert_eq!(
        changed_files(&dir, &base).unwrap(),
        [("c.rs".to_string(), 'A')]
    );
    assert!(
        work_changed_files(&dir, "-x").is_err(),
        "an option is no base"
    );

    // A new file not added to git yet is part of the work, shown whole; an
    // ignored one is not.
    std::fs::write(dir.join(".git/info/exclude"), "*.log\n").unwrap();
    std::fs::write(dir.join("build.log"), "noise\n").unwrap();
    std::fs::write(dir.join("new.rs"), "fresh\n").unwrap();
    let files = work_changed_files(&dir, &base).unwrap();
    assert_eq!(
        files,
        [
            ("b.rs".to_string(), 'M'),
            ("c.rs".to_string(), 'A'),
            ("new.rs".to_string(), '?')
        ]
    );
    let patch = work_patch(&dir, &base, 64 * 1024).unwrap();
    assert!(
        patch.contains("+++ b/new.rs") && patch.contains("+fresh") && !patch.contains("noise"),
        "{patch}"
    );
    let one = work_patch_of(&dir, &base, "new.rs", 64 * 1024).unwrap();
    assert!(one.contains("@@ -0,0 +1,1 @@\n+fresh"), "{one}");
    let small = work_patch(&dir, &base, 64).unwrap();
    assert!(small.ends_with("… (truncated)\n"), "{small:?}");
}

/// With only a new, untracked file, there is work to review.
#[test]
fn a_new_untracked_file_alone_is_uncommitted_work() {
    let dir = repo_dir("git-untracked");
    std::fs::write(dir.join("a.rs"), "1\n").unwrap();
    commit_all(&dir, "start");
    assert!(!has_uncommitted(&dir).unwrap());
    std::fs::write(dir.join("new.rs"), "x\n").unwrap();
    assert!(has_uncommitted(&dir).unwrap());
    assert_eq!(
        work_changed_files(&dir, "HEAD").unwrap(),
        [("new.rs".to_string(), '?')]
    );
}

/// The base a branch is reviewed against is the base branch it left: the
/// nearest of the remote's default, `main`, `master` and `develop`; a
/// remote default with no local branch is compared as `origin/<name>`; and
/// HEAD on a base branch has no branch to review.
#[test]
fn the_review_base_is_the_branch_the_work_left() {
    let dir = repo_dir("git-review-base");
    std::fs::write(dir.join("a.rs"), "1\n").unwrap();
    commit_all(&dir, "start");
    sh_git(&dir, &["branch", "-M", "main"]);
    sh_git(&dir, &["checkout", "-q", "-b", "develop"]);
    std::fs::write(dir.join("b.rs"), "1\n").unwrap();
    commit_all(&dir, "develop work");
    sh_git(&dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("c.rs"), "1\n").unwrap();
    commit_all(&dir, "feature work");
    assert_eq!(
        review_base(&dir).unwrap(),
        Some(("develop".to_string(), "vs develop".to_string())),
        "develop is nearer than main"
    );

    // The remote's default, known only as a remote-tracking branch.
    let develop = sh_git_out(&dir, &["rev-parse", "develop"]);
    sh_git(&dir, &["update-ref", "refs/remotes/origin/trunk", &develop]);
    sh_git(
        &dir,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    sh_git(&dir, &["branch", "-D", "develop"]);
    assert_eq!(
        review_base(&dir).unwrap().map(|(base, _)| base).as_deref(),
        Some("origin/trunk")
    );
    let files = changed_files(&dir, "origin/trunk").unwrap();
    assert_eq!(files, [("c.rs".to_string(), 'A')]);

    // On the remote's default branch, there is no branch: its last commit,
    // or (with a single commit) nothing.
    sh_git(&dir, &["checkout", "-q", "--detach", "origin/trunk"]);
    assert_eq!(
        review_base(&dir).unwrap().map(|(base, _)| base).as_deref(),
        Some("HEAD~1")
    );
    sh_git(&dir, &["checkout", "-q", "main"]);
    assert_eq!(review_base(&dir).unwrap(), None);

    assert!(is_plain_branch_name("develop") && is_plain_branch_name("release/1.2"));
    for bad in [
        "", "-x", "a..b", "../x", "/x", "x/", "a//b", "a b", "a~1", "a^",
    ] {
        assert!(!is_plain_branch_name(bad), "{bad:?}");
    }
}
