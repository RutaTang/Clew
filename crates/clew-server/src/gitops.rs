//! `Request::Git`: argument validation. The operations themselves run through
//! `clew_core::git::run_op`, the one dispatch the client's local path uses too,
//! and a git that could not answer comes back from it as a `GitError`, which
//! the request loop replies as `Event::Error` (`failed`) — never as an empty
//! `GitResult`.

/// Validate a `GitOp`'s arguments before anything reaches a git subprocess.
/// They arrive from the client, but the client got them from data — a path in
/// a model's answer, a sha in a commit list, a ref a user typed — so they are
/// checked as data (see the crate docs, "Trust"): rels must stay confined,
/// shas must be plain hex, and refs must never look like options (`-...`).
pub(crate) fn validate_git_op(op: &clew_protocol::GitOp) -> Result<(), String> {
    use clew_protocol::GitOp;
    const MAX_LIMIT: usize = 1000;
    const MAX_DIFF_BYTES: usize = 1024 * 1024;
    // The one lexical confinement predicate every client path goes through
    // (`clew_core::confine`), not a second one: two copies of a security
    // predicate are two chances for one to drift open. Lexical only — git
    // reads these from its object store, where a file may no longer exist to
    // be resolved (the history of a deleted or renamed file).
    let rel_ok = |rel: &str| {
        clew_core::confine::check_lexical(rel)
            .map_err(|e| format!("refused: bad path: {rel} ({e})"))
    };
    // One definition of "a sha" for both paths: the local GUI reaches these
    // same git helpers directly, and a second copy of the predicate here is
    // exactly how the remote gate and the local one drifted apart before.
    let sha_ok = |sha: &str| {
        clew_core::git::is_hex_sha(sha)
            .then_some(())
            .ok_or_else(|| format!("refused: bad commit id: {sha}"))
    };
    let ref_ok = |base: &str| {
        (!base.is_empty()
            && !base.starts_with('-')
            && base.len() <= 256
            && base
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._/@~^-".contains(c)))
        .then_some(())
        .ok_or_else(|| format!("refused: bad ref: {base}"))
    };
    let limit_ok = |n: usize| {
        (n <= MAX_LIMIT)
            .then_some(())
            .ok_or_else(|| "refused: history limit too large".to_string())
    };
    let bytes_ok = |n: usize| {
        (n <= MAX_DIFF_BYTES)
            .then_some(())
            .ok_or_else(|| "refused: diff cap too large".to_string())
    };
    match op {
        GitOp::FileHistory { rel, limit } => rel_ok(rel).and(limit_ok(*limit)),
        GitOp::Churn { commits } => limit_ok(*commits),
        GitOp::SymbolHistory { rel, limit, .. } => rel_ok(rel).and(limit_ok(*limit)),
        GitOp::FileAt { sha, rel } | GitOp::AddedLines { sha, rel } => sha_ok(sha).and(rel_ok(rel)),
        GitOp::CommitMessage { sha } => sha_ok(sha),
        GitOp::CommitFileDiff {
            sha,
            rel,
            max_bytes,
        } => sha_ok(sha).and(rel_ok(rel)).and(bytes_ok(*max_bytes)),
        GitOp::DiffLines { rel } => rel_ok(rel),
        GitOp::ReviewBase => Ok(()),
        GitOp::CommitSubjects { base } | GitOp::ChangedFiles { base } => ref_ok(base),
        GitOp::RangePatch { base, max_bytes } => ref_ok(base).and(bytes_ok(*max_bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_git_op;
    use clew_protocol::GitOp;

    /// A rel is judged by the shared confinement predicate, exactly: the git
    /// bridge refuses what every other client path refuses, and nothing more.
    #[test]
    fn git_paths_are_confined_by_the_shared_predicate() {
        for rel in [
            "src/lib.rs",
            "./src/lib.rs",
            "a b.rs",
            "",
            "  ",
            "/etc/passwd",
            "../outside.rs",
            "src/../../outside.rs",
            "src/../src/lib.rs",
        ] {
            let op = GitOp::FileHistory {
                rel: rel.into(),
                limit: 10,
            };
            assert_eq!(
                validate_git_op(&op).is_ok(),
                clew_core::confine::check_lexical(rel).is_ok(),
                "{rel:?}"
            );
        }
        assert!(
            validate_git_op(&GitOp::DiffLines { rel: "../x".into() })
                .is_err_and(|e| e.starts_with("refused: bad path"))
        );
    }

    /// Shas are hex, refs never look like options, and caps are capped.
    #[test]
    fn non_path_arguments_are_checked_as_data() {
        let file_at = |sha: &str| GitOp::FileAt {
            sha: sha.into(),
            rel: "a.rs".into(),
        };
        assert!(validate_git_op(&file_at("0123abcd")).is_ok());
        assert!(validate_git_op(&file_at("--output=x")).is_err());
        let subjects = |base: &str| GitOp::CommitSubjects { base: base.into() };
        assert!(validate_git_op(&subjects("origin/main")).is_ok());
        assert!(validate_git_op(&subjects("-p")).is_err());
        assert!(validate_git_op(&subjects("")).is_err());
        assert!(
            validate_git_op(&GitOp::RangePatch {
                base: "main".into(),
                max_bytes: usize::MAX,
            })
            .is_err()
        );
        assert!(validate_git_op(&GitOp::Churn { commits: 300 }).is_ok());
        assert!(validate_git_op(&GitOp::Churn { commits: 100_000 }).is_err());
    }
}
