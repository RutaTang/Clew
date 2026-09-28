//! Read-only git information for the open file: per-line blame (who last
//! touched each line) and per-line change status versus `HEAD` for the gutter,
//! plus the history, review and time-travel queries built on the same runner.
//!
//! Everything here shells out to the `git` binary with read-only commands.
//!
//! # Failures are errors, never empty answers
//!
//! Every query returns `Result<_, GitError>`. An empty history, an empty diff
//! or a file absent at a commit is a real answer; a git that timed out, is not
//! installed, refused the repository or could not read an object is not, and
//! must never be mistaken for one — it showed as "no history" in Time Travel
//! and reached the (paid) review prompt as an empty diff. What IS still an
//! empty answer is spelled out per function: a path `HEAD` does not have, or a
//! repository with no commit yet, has no history and no blame. The gutter's
//! query, [`try_info`], is no exception: a git that could not answer is an
//! `Err` its caller reports, since a blank gutter reads as "nothing changed".
//!
//! # One hardened runner
//!
//! Every invocation goes through [`Git`], because a repository (and the
//! user's own git configuration) gets a say in what a "read-only" git command
//! does:
//!
//! - **Programs git would run as a side effect** are switched off:
//!   `core.fsmonitor` (a hook run on index refresh), `core.hooksPath`,
//!   external diff drivers (`--no-ext-diff`, and `GIT_EXTERNAL_DIFF` removed
//!   from the environment), textconv filters (`--no-textconv`), and the
//!   clean/smudge/process **filter drivers** the repository's own
//!   configuration defines. A filter is a program git runs on a working-tree
//!   file whenever it compares one — `diff HEAD -- <file>` and `blame` both
//!   do — and `.gitattributes` decides which files get it, so a repository
//!   shipping both halves ran a program of its choosing on every file open.
//!   Each driver defined by the repository (its `.git/config`, worktree
//!   config, or anything they include) is discovered when a query starts
//!   ([`filter_overrides`]) and overridden on the command line (`-c
//!   filter.<name>.clean=` and friends, `required=false`). Drivers from the
//!   user's global or system configuration — git-lfs — are the user's own
//!   choice and keep working. A driver whose name cannot be spelled as a `-c`
//!   key refuses the query ([`GitError::UnsafeConfig`]).
//! - **Nothing is ever fetched.** Reading an old blob in a partial (blobless)
//!   clone makes git fetch it from the promisor remote, through whatever
//!   transport, `core.sshCommand` or credential helper the repository's
//!   configuration names — an `ext::` remote simply runs a command. Every
//!   invocation sets `GIT_NO_LAZY_FETCH=1` (git 2.44+) and, for every git
//!   that ignores it, `GIT_ALLOW_PROTOCOL` to a name no transport has: with
//!   that variable set, git allows exactly the protocols it lists — none —
//!   whatever any configuration says. `-c` pins alone were not enough: they
//!   outrank a repository's `protocol.<name>.allow` only for the names they
//!   spell out, and a repository could enable one they did not (`ftp`, or
//!   any remote helper) and have the lazy fetch dial its host. The pins stay
//!   as a second layer. Both travel to the `git fetch` a lazy fetch spawns
//!   (git hands its environment and `-c` settings to its children). A missing
//!   object is then an error the reader sees, with a hint to fetch it
//!   themselves.
//! - **Submodules are never entered.** Showing a submodule change as a patch
//!   (`diff.submodule=diff`, which a repository may set) makes git run a
//!   second `git diff` INSIDE the submodule, and a working-tree comparison of
//!   a submodule entry runs `git status` there. Such a child reads the
//!   submodule's own configuration, and neither `--no-ext-diff`,
//!   `--no-textconv` nor the filter overrides (which cover the
//!   superproject's drivers) reach it — so the submodule's external diff,
//!   textconv or clean filter ran. `diff.submodule=short` is pinned (a
//!   submodule change is its two commit ids), and the working-tree diffs pass
//!   `--ignore-submodules=dirty`, which, unlike the configuration of the same
//!   name, a committed `.gitmodules` cannot override.
//! - **Output shape** is pinned where the parsers depend on it:
//!   `core.quotePath=false`, `diff.noprefix`/`diff.mnemonicPrefix`/
//!   `diff.relative` off, `diff.suppressBlankEmpty` off (a blank context line
//!   keeps its leading space), UTF-8 log output, colour off, no signature
//!   display — and blame runs with `--no-ignore-revs-file`, because a
//!   `blame.ignoreRevsFile` in the user's global config makes blame fail
//!   outright in every repository that lacks that file. (A `-c
//!   blame.ignoreRevsFile=` reset does not work: git keeps that list sorted,
//!   so the empty "reset" entry is applied before the file it was meant to
//!   clear.)
//! - **Paths are literal** (`--literal-pathspecs`): `:(top)`, `:(exclude)`
//!   and glob magic in a path are just characters.
//! - **Nothing interactive, nothing that waits forever**: stdin is
//!   `/dev/null` (on clew-server fd 0 IS the protocol stream, which a child
//!   must never read), prompts are disabled, optional index locks are skipped,
//!   output is capped at [`MAX_GIT_OUTPUT`], and a deadline kills the child —
//!   together with everything it started: git runs in a process group of its
//!   own and the whole group is killed, so a helper it spawned (a fetch, an
//!   ssh, a filter process) cannot outlive it holding the pipes.
//! - **Failures say why**: git runs in the C locale, the end of its stderr is
//!   kept, and every way a run can fail is a [`GitError`] variant.
//!
//! Residual, deliberately accepted: the filter overrides are read from the
//! configuration as it is when a query starts. A configuration EDITED in the
//! milliseconds between that read and the command is not seen. What this
//! defends against is a repository's committed or downloaded configuration,
//! present before clew opens it; winning that window needs something already
//! writing into the repository's `.git` while clew runs — which can run its
//! programs through the user's own next `git commit` anyway.
//!
//! # Paths: project root vs. repository top
//!
//! The project root may be a SUBDIRECTORY of its repository. Every `rel` this
//! module takes or returns — including [`HistCommit::path`] — is relative to
//! the PROJECT ROOT, the same space as the file tree. Pathspecs (`-- rel`) are
//! resolved against the root (git runs with `-C root`), and paths git prints
//! (always top-relative) are mapped back ([`Repo::root_rel`]). History of a
//! file from before it entered the project directory is outside the project
//! and is left out.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// Blame, change status, the per-file git view, history commits and diff lines
// are protocol wire types (git produces them here, the server transmits them,
// the client renders them), so there is no conversion between produce and
// render — and the build fingerprint covers their shape.
pub use clew_protocol::{BlameLine, ChangeKind, DiffKind, DiffLine, GitInfo, HistCommit};

// ------------------------------------------------------------------ errors

/// Why a git query has no answer (see the module docs: never an empty one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// `git` could not be started (not installed, not on `PATH`).
    Spawn(String),
    /// The installed git is older than the oldest one clew works with.
    Unsupported(String),
    /// The project root is not inside a git work tree.
    NotARepository,
    /// Not run: an argument is not what it must be — a commit id that is not
    /// hex, a path that would leave the project, a range that is empty.
    Refused(String),
    /// Not run: the repository's configuration names a program clew cannot
    /// switch off (see the module docs).
    UnsafeConfig(String),
    /// Reading git's output failed.
    Io(String),
    /// git ran and exited unsuccessfully. `stderr` is the end of what it said
    /// (its last lines, which is where git puts the `fatal:` one).
    Failed { code: Option<i32>, stderr: String },
    /// Its output overran the cap, and it was stopped.
    TooLarge { limit: u64 },
    /// It did not finish before its deadline, and it was killed together with
    /// everything it had started.
    TimedOut { after: Duration },
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Spawn(e) => write!(f, "git could not be started: {e}"),
            GitError::Unsupported(version) => write!(
                f,
                "git {version} is too old for clew (it needs {}.{} or newer)",
                MIN_GIT.0, MIN_GIT.1
            ),
            GitError::NotARepository => f.write_str("not a git repository"),
            GitError::Refused(why) => write!(f, "refused: {why}"),
            GitError::UnsafeConfig(why) => write!(f, "not running git in this repository: {why}"),
            GitError::Io(e) => write!(f, "reading git's output failed: {e}"),
            GitError::Failed { code, stderr } => {
                match (stderr.is_empty(), code) {
                    (false, _) => write!(f, "git failed: {stderr}")?,
                    (true, Some(code)) => write!(f, "git exited with status {code}")?,
                    (true, None) => f.write_str("git was stopped by a signal")?,
                }
                if stderr.contains("promisor remote") {
                    f.write_str(
                        " (this is a partial clone and clew never fetches: run `git fetch` in \
                         the repository to get the missing objects)",
                    )?;
                }
                Ok(())
            }
            GitError::TooLarge { limit } if *limit >= 1024 * 1024 => write!(
                f,
                "git's output exceeded {} MiB and it was stopped",
                limit / (1024 * 1024)
            ),
            GitError::TooLarge { limit } => {
                write!(f, "git's output exceeded {limit} bytes and it was stopped")
            }
            GitError::TimedOut { after } => write!(
                f,
                "git did not finish within {}s and was stopped",
                after.as_secs()
            ),
        }
    }
}

impl std::error::Error for GitError {}

/// Collect blame + change status for `abs` for the gutter. `Ok(None)` when
/// there is nothing to show: the root is not in a work tree, or `abs` has
/// neither blame nor changes (an untracked file, a repository with no commit
/// yet). `Err` when git could not answer — the caller reports it, since a
/// blank gutter would read as "nothing changed". Blocking; run off the UI
/// thread.
pub fn try_info(root: &Path, abs: &Path) -> Result<Option<GitInfo>, GitError> {
    let git = match Git::open(root) {
        Ok(git) => git,
        Err(GitError::NotARepository) => return Ok(None),
        Err(e) => return Err(e),
    };
    let blame = git.blame(abs)?;
    let (status, deleted_at) = git.diff_status(abs)?;
    if blame.is_empty() && status.is_empty() && deleted_at.is_empty() {
        return Ok(None);
    }
    Ok(Some(GitInfo {
        blame,
        status,
        deleted_at,
    }))
}

// ------------------------------------------------------------------ runner

/// Cap on what any one git invocation may hand back. A repository chooses its
/// own history and file sizes, and reading a whole stdout before deciding it
/// is too big is a memory spike already paid for — so the cap is applied WHILE
/// reading, and the child is killed the moment it overruns. Real outputs here
/// are a file, a diff, or a few hundred log lines.
const MAX_GIT_OUTPUT: u64 = 64 * 1024 * 1024;

/// Deadline for an ordinary invocation (a diff, a show, a rev-parse).
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline for the walks whose cost grows with the repository's history:
/// blame, `log --follow`, `log -L`.
const GIT_HISTORY_TIMEOUT: Duration = Duration::from_secs(90);

/// How much of git's stderr is kept: the END of it, where the `fatal:` line
/// is. Everything before is read and dropped, so a chatty child can never
/// block on a full pipe.
const MAX_STDERR_TAIL: usize = 16 * 1024;

/// How long to wait for stderr's last bytes once git has exited. Only a
/// grandchild still holding the pipe makes this wait at all.
const STDERR_GRACE: Duration = Duration::from_millis(200);

/// The oldest git clew runs: `--end-of-options`, which every sha- or
/// ref-taking call relies on, is 2.24.
const MIN_GIT: (u32, u32) = (2, 24);

/// Configuration pinned on every invocation (see the module docs).
const PINNED_CONFIG: &[&str] = &[
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "core.quotePath=false",
    "diff.noprefix=false",
    "diff.mnemonicPrefix=false",
    "diff.relative=false",
    "diff.suppressBlankEmpty=false",
    "color.ui=false",
    "log.showSignature=false",
    "log.showRoot=true",
    "i18n.logOutputEncoding=UTF-8",
    // A submodule change is shown as its commit ids, never as a patch that a
    // git run inside the submodule would produce (see "Submodules are never
    // entered").
    "diff.submodule=short",
    // No transport of any kind (see "Nothing is ever fetched";
    // `GIT_ALLOW_PROTOCOL` is the switch that holds). Each built-in protocol
    // is pinned by name as well: a repository's own
    // `protocol.<name>.allow=always` outranks the `protocol.allow` default,
    // but not the same key given on the command line.
    "protocol.allow=never",
    "protocol.file.allow=never",
    "protocol.git.allow=never",
    "protocol.ssh.allow=never",
    "protocol.http.allow=never",
    "protocol.https.allow=never",
    "protocol.ext.allow=never",
];

/// `GIT_ALLOW_PROTOCOL` for every invocation: a list naming one protocol that
/// does not exist, so no transport is allowed (see "Nothing is ever fetched").
/// A plain word on purpose. A remote helper is looked up as
/// `git-remote-<name>` on `PATH` only, and a name holding a `/` would be
/// looked up relative to the working directory — the repository — instead.
/// Not empty either: an empty list does mean "nothing", but a non-empty one
/// cannot be mistaken for "unset" by any reading of the variable.
const NO_PROTOCOL: &str = "clew-none";

/// Environment a parent process (a git hook, a shell alias, a CI job) may have
/// set that would redirect git to another repository, inject configuration,
/// run an external diff, or change how paths are matched —
/// `GIT_GLOB_PATHSPECS` alone makes `--literal-pathspecs` a fatal conflict.
/// `GIT_CONFIG` redirects `git config` (and only it), so the filter discovery
/// would read another file than every other invocation.
const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_EXTERNAL_DIFF",
    "GIT_DIFF_OPTS",
    "GIT_PAGER",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "LANGUAGE",
];

/// What to do with output past the cap.
#[derive(Debug, Clone, Copy)]
enum Cap {
    /// Past `n` bytes the whole result is refused.
    Fail(u64),
    /// Keep the first `n` bytes, stop git, and report the result truncated —
    /// for output whose consumer truncates anyway (a patch for a prompt).
    Truncate(u64),
}

impl Cap {
    fn limit(self) -> u64 {
        match self {
            Cap::Fail(n) | Cap::Truncate(n) => n,
        }
    }
}

/// How one invocation ended.
#[derive(Debug)]
struct Finished {
    stdout: Vec<u8>,
    ended: Ended,
    /// The end of what it wrote to stderr (see [`MAX_STDERR_TAIL`]).
    stderr: String,
}

#[derive(Debug)]
enum Ended {
    Exited(ExitStatus),
    /// Stopped by us at a [`Cap::Truncate`] limit — a deliberate, complete
    /// result.
    Truncated,
}

/// A successful run's stdout.
#[derive(Debug)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

impl Finished {
    /// The output of a run that succeeded (or was deliberately truncated),
    /// else the failure it was.
    fn success(self) -> Result<Captured, GitError> {
        match self.ended {
            Ended::Truncated => Ok(Captured {
                bytes: self.stdout,
                truncated: true,
            }),
            Ended::Exited(status) if status.success() => Ok(Captured {
                bytes: self.stdout,
                truncated: false,
            }),
            Ended::Exited(status) => Err(failure(status, &self.stderr)),
        }
    }

    /// For the commands whose exit status IS the answer (`--quiet`,
    /// `--error-unmatch`, `--verify`): 0 is yes, 1 is no, and anything else —
    /// git's 128 for a fatal error — is a failure.
    fn answer(self) -> Result<bool, GitError> {
        match self.ended {
            Ended::Exited(status) if status.success() => Ok(true),
            Ended::Exited(status) if status.code() == Some(1) => Ok(false),
            Ended::Exited(status) => Err(failure(status, &self.stderr)),
            Ended::Truncated => Err(GitError::Io("unexpected truncation".into())),
        }
    }
}

/// The error for an unsuccessful exit. "Not a git repository" is recognised
/// by its message (git runs in the C locale, so it is stable); every other
/// failure keeps what git said.
fn failure(status: ExitStatus, stderr: &str) -> GitError {
    if stderr.contains("not a git repository") {
        return GitError::NotARepository;
    }
    GitError::Failed {
        code: status.code(),
        stderr: summarize_stderr(stderr),
    }
}

/// The last few non-empty lines of git's stderr, joined, at most a few
/// hundred characters — what a status line can show.
fn summarize_stderr(stderr: &str) -> String {
    const LINES: usize = 3;
    const CHARS: usize = 600;
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let text = lines[lines.len().saturating_sub(LINES)..].join("; ");
    let count = text.chars().count();
    if count <= CHARS {
        return text;
    }
    let tail: String = text.chars().skip(count - CHARS).collect();
    format!("…{tail}")
}

/// The command every invocation starts from — `git -C root`, hardened as the
/// module docs describe. `-C` makes the root git's working directory, which
/// is what relative pathspecs resolve against.
fn base_command(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .arg("--no-pager")
        .arg("--literal-pathspecs");
    for kv in PINNED_CONFIG {
        cmd.arg("-c").arg(kv);
    }
    for var in SCRUBBED_ENV {
        cmd.env_remove(var);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        // Set (never inherited): a parent's own list would re-enable what it
        // names, since this variable outranks every `protocol.*` setting.
        .env("GIT_ALLOW_PROTOCOL", NO_PROTOCOL)
        // Messages in one language, so the few that are recognised
        // (`failure`) stay recognisable. Paths and log text are unaffected:
        // quoting follows `core.quotePath`, the output encoding is pinned.
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    // This crate's tests run git on fixtures only: the developer's
    // `~/.gitconfig` and the system config must not decide what they see.
    // (The test binaries of the crates above set the same variables for the
    // whole process: `testutil::isolate_git_config`.)
    #[cfg(test)]
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

/// Spawn `cmd`, read its stdout through `cap` and the tail of its stderr on
/// helper threads, and wait for it under `timeout` — killing it, with its
/// whole process group, if it overruns either. `Err` only for what stopped the
/// run itself (spawn, read, cap, deadline); an unsuccessful exit is a
/// [`Finished`] for the caller to judge.
///
/// The readers are threads because a blocking read is the one thing a
/// deadline cannot interrupt. A grandchild that inherited the pipes keeps
/// them open after git exits; the group kill is what ends it, and with it the
/// read.
fn execute(mut cmd: Command, timeout: Duration, cap: Cap) -> Result<Finished, GitError> {
    let deadline = Instant::now() + timeout;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so a kill reaches everything it started.
        cmd.process_group(0);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GitError::Spawn(e.to_string()))?;
    let stderr = child.stderr.take().map(read_tail);
    let Some(stdout) = child.stdout.take() else {
        kill(&mut child);
        return Err(GitError::Io("git's stdout was not captured".into()));
    };
    let limit = cap.limit();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("clew-git-stdout".into())
        .spawn(move || {
            let mut buf = Vec::new();
            // One byte past the cap is what tells "exactly the cap" from
            // "more than the cap".
            let read = stdout.take(limit + 1).read_to_end(&mut buf).map(|_| buf);
            let _ = tx.send(read);
        });
    if let Err(e) = reader {
        kill(&mut child);
        return Err(GitError::Io(e.to_string()));
    }
    let mut bytes = match rx.recv_timeout(timeout) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            kill(&mut child);
            return Err(GitError::Io(e.to_string()));
        }
        Err(_) => {
            kill(&mut child);
            return Err(GitError::TimedOut { after: timeout });
        }
    };
    if bytes.len() as u64 > limit {
        kill(&mut child);
        return match cap {
            Cap::Fail(n) => Err(GitError::TooLarge { limit: n }),
            Cap::Truncate(n) => {
                bytes.truncate(n as usize);
                Ok(Finished {
                    stdout: bytes,
                    ended: Ended::Truncated,
                    stderr: String::new(),
                })
            }
        };
    }
    let status = match wait_until(&mut child, deadline) {
        Waited::Exited(status) => status,
        Waited::Running => {
            kill(&mut child);
            return Err(GitError::TimedOut { after: timeout });
        }
        // Nothing of it is ours to signal any more (see `kill`).
        Waited::Lost(e) => {
            return Err(GitError::Io(format!("git's exit could not be read: {e}")));
        }
    };
    let stderr = stderr
        .and_then(|rx| rx.recv_timeout(STDERR_GRACE).ok())
        .unwrap_or_default();
    Ok(Finished {
        stdout: bytes,
        ended: Ended::Exited(status),
        stderr,
    })
}

/// Drain `pipe` on a thread, keeping only its last [`MAX_STDERR_TAIL`] bytes,
/// and hand them over when it closes.
fn read_tail(mut pipe: std::process::ChildStderr) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("clew-git-stderr".into())
        .spawn(move || {
            let mut tail: Vec<u8> = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > 2 * MAX_STDERR_TAIL {
                            tail.drain(..tail.len() - MAX_STDERR_TAIL);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let start = tail.len().saturating_sub(MAX_STDERR_TAIL);
            let _ = tx.send(String::from_utf8_lossy(&tail[start..]).into_owned());
        });
    rx
}

/// How [`wait_until`] left the child.
#[derive(Debug)]
enum Waited {
    /// It exited, and is reaped.
    Exited(ExitStatus),
    /// It is still running at the deadline.
    Running,
    /// Its exit cannot be read: it was reaped behind our back (`ECHILD`).
    /// Its pid, and the id of the group it led, may name someone else's
    /// processes by now, so it is not signalled ([`kill`]). This used to
    /// read as still running, and the child was killed — its pid and group
    /// signalled — as a timeout.
    Lost(std::io::Error),
}

/// Wait for `child` until `deadline`.
fn wait_until(child: &mut std::process::Child, deadline: Instant) -> Waited {
    let mut pause = Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Waited::Exited(status),
            Ok(None) => {}
            Err(e) => return Waited::Lost(e),
        }
        let now = Instant::now();
        if now >= deadline {
            return Waited::Running;
        }
        std::thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(Duration::from_millis(25));
    }
}

/// Kill `child` and everything in its process group, then reap it.
///
/// The group, because git's own children — a lazy fetch and the ssh under
/// it, a long-running filter — would otherwise run on, holding the pipes and
/// whatever they were doing; all of it, a child one of them is forking at
/// that moment included ([`crate::procgroup::kill`]). The group id is the
/// child's pid, and the child is never reaped before that is done (reaping
/// is what could free that id for reuse), so no signal can reach an
/// unrelated group.
///
/// Unless it was reaped behind our back: nothing in clew waits on another's
/// child, but a waiter on any child could. Then the pid and the group id are
/// free for reuse, and neither is signalled — nor the pid by
/// `Child::kill`, which would signal it too — so a child that is no longer
/// ours to wait for ([`crate::framing::leader_state`]) is left alone.
fn kill(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        use crate::framing::{LeaderState, leader_state};
        if leader_state(child.id()) == LeaderState::Unknown {
            return;
        }
        crate::procgroup::kill(child.id());
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The installed git's version, `(major, minor, patch)`: asked once per
/// process (a failure is not remembered, so installing git later works).
fn installed_version() -> Result<(u32, u32, u32), GitError> {
    static VERSION: std::sync::OnceLock<(u32, u32, u32)> = std::sync::OnceLock::new();
    if let Some(v) = VERSION.get() {
        return Ok(*v);
    }
    let mut cmd = Command::new("git");
    cmd.arg("--version")
        .env("LC_ALL", "C")
        .env_remove("LANGUAGE")
        .stdin(Stdio::null());
    // Even `--version` reads the global config (and fails when it cannot):
    // kept away from the developer's in this crate's tests, as in
    // `base_command`.
    #[cfg(test)]
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let out = execute(cmd, GIT_TIMEOUT, Cap::Fail(4096))?.success()?;
    let text = String::from_utf8_lossy(&out.bytes);
    let version = parse_version(&text).ok_or_else(|| {
        GitError::Io(format!(
            "cannot read the git version from {:?}",
            text.trim()
        ))
    })?;
    if (version.0, version.1) < MIN_GIT {
        return Err(GitError::Unsupported(format!(
            "{}.{}.{}",
            version.0, version.1, version.2
        )));
    }
    Ok(*VERSION.get_or_init(|| version))
}

/// `git version 2.39.5 (Apple Git-154)` → `(2, 39, 5)`; a missing or
/// non-numeric patch (`2.45.rc0`, `2.46`) reads as 0.
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let number = text.split_whitespace().nth(2)?;
    let mut parts = number.split('.').map(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// `-c` settings that switch off every filter driver the REPOSITORY defines
/// (see the module docs). Drivers the user's global or system configuration
/// defines are left alone, unless the repository redefines them — then any
/// key of theirs at repository scope puts the whole driver on the list.
///
/// git older than 2.26 cannot say where a key came from (`--show-scope`), and
/// there every driver is switched off: safe, at the price of the user's own
/// filters (git-lfs files then compare as their pointers).
fn filter_overrides(root: &Path, version: (u32, u32, u32)) -> Result<Vec<OsString>, GitError> {
    let scoped = version >= (2, 26, 0);
    let mut cmd = base_command(root);
    cmd.args(["config", "-z", "--name-only"]);
    if scoped {
        cmd.arg("--show-scope");
    }
    cmd.args(["--get-regexp", r"^filter\."]);
    let finished = execute(cmd, GIT_TIMEOUT, Cap::Fail(1024 * 1024))?;
    // Exit 1 is "no key matched": no filter driver anywhere.
    if let Ended::Exited(status) = &finished.ended
        && status.code() == Some(1)
    {
        return Ok(Vec::new());
    }
    let listing = finished.success()?.bytes;
    overrides_for(&drivers_to_switch_off(&listing, scoped))
}

/// The filter driver names in `git config -z --name-only [--show-scope]
/// --get-regexp` output that must be switched off: every one with a key at
/// any scope but the user's own (`global`, `system`) — or every one at all,
/// when the listing carries no scopes. In order of first appearance.
fn drivers_to_switch_off(listing: &[u8], scoped: bool) -> Vec<&[u8]> {
    let mut fields = listing.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut names: Vec<&[u8]> = Vec::new();
    loop {
        let scope = if scoped {
            match fields.next() {
                Some(scope) => Some(scope),
                None => break,
            }
        } else {
            None
        };
        let Some(key) = fields.next() else { break };
        // The user's own configuration is the user's choice.
        if matches!(scope, Some(b"global" | b"system")) {
            continue;
        }
        // `filter.<name>.<var>`: the name is everything between the first
        // and the last dot, and may itself contain dots.
        let Some(rest) = key.strip_prefix(b"filter.") else {
            continue;
        };
        let Some(dot) = rest.iter().rposition(|&b| b == b'.') else {
            continue; // `filter.<var>`: no driver name, not a driver
        };
        let name = &rest[..dot];
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The `-c` settings that disable each driver in `names`: no clean, smudge or
/// process command, and not required (a required filter that does not run
/// makes git fail).
fn overrides_for(names: &[&[u8]]) -> Result<Vec<OsString>, GitError> {
    let mut overrides = Vec::with_capacity(names.len() * 4);
    for name in names {
        // `-c` splits at the FIRST `=`, so a name holding one would turn the
        // override into some other key, and the filter would still run.
        if name.contains(&b'=') {
            return Err(GitError::UnsafeConfig(format!(
                "its configuration defines a filter driver named {:?}, which cannot be \
                 switched off from the command line",
                String::from_utf8_lossy(name)
            )));
        }
        for (var, value) in [
            ("clean", ""),
            ("smudge", ""),
            ("process", ""),
            ("required", "false"),
        ] {
            let mut kv = b"filter.".to_vec();
            kv.extend_from_slice(name);
            kv.extend_from_slice(format!(".{var}={value}").as_bytes());
            overrides.push(os_string(kv));
        }
    }
    Ok(overrides)
}

#[cfg(unix)]
fn os_string(bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn os_string(bytes: Vec<u8>) -> OsString {
    OsString::from(String::from_utf8_lossy(&bytes).into_owned())
}

/// One query's view of its repository: where the project root sits in it,
/// and the command line every invocation of the query runs with. Opened once
/// per query ([`Git::open`]), so the checks it makes are paid once.
struct Git<'a> {
    root: &'a Path,
    repo: Repo,
    /// `-c` settings switching off the repository's own filter drivers
    /// ([`filter_overrides`]).
    overrides: Vec<OsString>,
}

impl<'a> Git<'a> {
    /// The repository `root` is in: [`GitError::NotARepository`] when it is
    /// not inside a work tree.
    fn open(root: &'a Path) -> Result<Git<'a>, GitError> {
        let version = installed_version()?;
        let mut cmd = base_command(root);
        cmd.args(["rev-parse", "--is-inside-work-tree", "--show-prefix"]);
        let out = execute(cmd, GIT_TIMEOUT, Cap::Fail(64 * 1024))?.success()?;
        let text = String::from_utf8_lossy(&out.bytes);
        let mut lines = text.split('\n');
        // `false` inside `.git` itself or a bare repository: no work tree.
        if lines.next() != Some("true") {
            return Err(GitError::NotARepository);
        }
        let prefix = lines.next().unwrap_or("").to_string();
        Ok(Git {
            root,
            repo: Repo { prefix },
            overrides: filter_overrides(root, version)?,
        })
    }

    fn command(&self) -> Command {
        let mut cmd = base_command(self.root);
        for kv in &self.overrides {
            cmd.arg("-c").arg(kv);
        }
        cmd
    }

    fn run<I, S>(&self, args: I, timeout: Duration, cap: Cap) -> Result<Finished, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.command();
        cmd.args(args);
        execute(cmd, timeout, cap)
    }

    /// stdout of a successful run.
    fn bytes<I, S>(&self, args: I, timeout: Duration) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(self
            .run(args, timeout, Cap::Fail(MAX_GIT_OUTPUT))?
            .success()?
            .bytes)
    }

    /// [`Git::bytes`] as (lossy) text.
    fn text<I, S>(&self, args: I, timeout: Duration) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.bytes(args, timeout)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Text of a run whose consumer truncates to `max_bytes` anyway: git is
    /// stopped once it has produced more than that, and a cut result ends in
    /// the marker [`truncate_marked`] adds.
    fn text_truncated<I, S>(&self, args: I, max_bytes: usize) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let cap = (max_bytes as u64).min(MAX_GIT_OUTPUT);
        let captured = self.run(args, GIT_TIMEOUT, Cap::Truncate(cap))?.success()?;
        let mut text = String::from_utf8_lossy(&captured.bytes).into_owned();
        if captured.truncated {
            // Cut mid-output: the tail may be half a character, and the reader
            // must be told there was more.
            cut_at_char_boundary(&mut text, max_bytes);
            text.push_str(TRUNCATED_MARKER);
        } else {
            truncate_marked(&mut text, max_bytes);
        }
        Ok(text)
    }

    /// Run a command whose exit status is the answer (see
    /// [`Finished::answer`]).
    fn check<I, S>(&self, args: I) -> Result<bool, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run(args, GIT_TIMEOUT, Cap::Fail(MAX_GIT_OUTPUT))?
            .answer()
    }

    /// The object id `rev` names, or `None` when it names nothing. Only ever
    /// called with clew's own constants (`HEAD`, `HEAD~1`, `main`, `master`),
    /// never with data; the leading-dash refusal is the guard
    /// `--end-of-options` would otherwise be, which `rev-parse` only
    /// understands from git 2.30 on (before that, `--verify` rejected it and
    /// every revision looked missing).
    fn resolve(&self, rev: &str) -> Result<Option<String>, GitError> {
        if rev.starts_with('-') {
            return Err(GitError::Refused(format!("{rev:?} is not a revision")));
        }
        let finished = self.run(
            ["rev-parse", "--verify", "--quiet", rev],
            GIT_TIMEOUT,
            Cap::Fail(4096),
        )?;
        match finished.ended {
            Ended::Exited(status) if status.code() == Some(1) => Ok(None),
            _ => {
                let out = finished.success()?;
                Ok(Some(String::from_utf8_lossy(&out.bytes).trim().to_string()))
            }
        }
    }

    /// Whether `HEAD` has an entry at `path` (a pathspec: root-relative or
    /// absolute) — `false` too when there is no `HEAD` yet.
    fn in_head(&self, path: &OsStr) -> Result<bool, GitError> {
        if self.resolve("HEAD")?.is_none() {
            return Ok(false);
        }
        let out = self.bytes(
            [
                OsStr::new("ls-tree"),
                OsStr::new("-z"),
                OsStr::new("HEAD"),
                OsStr::new("--"),
                path,
            ],
            GIT_TIMEOUT,
        )?;
        Ok(!out.is_empty())
    }
}

// ------------------------------------------------------------- repo paths

/// Where the project root sits inside its repository.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Repo {
    /// The root relative to the repository top: empty at the top, otherwise
    /// ending in `/` (`git rev-parse --show-prefix`).
    prefix: String,
}

impl Repo {
    /// A repository-top-relative path (as git prints them) in the project
    /// root's terms, or `None` when it lies outside the project directory.
    fn root_rel(&self, top_rel: &str) -> Option<String> {
        let rel = top_rel.strip_prefix(self.prefix.as_str())?;
        (!rel.is_empty()).then(|| rel.to_string())
    }
}

/// Undo git's C-style path quoting: a path holding a control character, a
/// double quote or a backslash (with `core.quotePath=false`, only those) is
/// printed as `"…"` with `\t`, `\n`, `\"`, `\\` and `\ooo` octal escapes.
/// Unquoted text passes through. `None` for malformed quoting or a path that
/// is not UTF-8 (which no project file can be — see `fs_scan`).
fn unquote_path(s: &str) -> Option<String> {
    let Some(inner) = s.strip_prefix('"') else {
        return Some(s.to_string());
    };
    let inner = inner.strip_suffix('"')?;
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut bytes = inner.bytes();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        let esc = bytes.next()?;
        out.push(match esc {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'"' => b'"',
            b'\\' => b'\\',
            b'0'..=b'3' => {
                let d1 = bytes.next()?;
                let d2 = bytes.next()?;
                if !(b'0'..=b'7').contains(&d1) || !(b'0'..=b'7').contains(&d2) {
                    return None;
                }
                ((esc - b'0') << 6) | ((d1 - b'0') << 3) | (d2 - b'0')
            }
            _ => return None,
        });
    }
    String::from_utf8(out).ok()
}

// -------------------------------------------------------------- the gutter

impl Git<'_> {
    /// `git blame --porcelain` of the working-tree file. Empty — not an error
    /// — for a path `HEAD` does not have (untracked, newly added, a repository
    /// with no commit yet): blame refuses those, and there is simply nothing
    /// to attribute.
    fn blame(&self, abs: &Path) -> Result<Vec<BlameLine>, GitError> {
        let args = [
            OsStr::new("blame"),
            OsStr::new("--porcelain"),
            // blame applies textconv filters by default, unlike diff/log.
            OsStr::new("--no-textconv"),
            // Clears a configured ignore-revs list (see the module docs).
            OsStr::new("--no-ignore-revs-file"),
            OsStr::new("--"),
            abs.as_os_str(),
        ];
        match self.text(args, GIT_HISTORY_TIMEOUT) {
            Ok(text) => Ok(parse_blame(&text)),
            Err(e @ GitError::Failed { .. }) => {
                if self.in_head(abs.as_os_str())? {
                    Err(e)
                } else {
                    Ok(Vec::new())
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Per-line change status of the working-tree file against `HEAD` (see
    /// [`parse_diff_status`]). Empty when there is no `HEAD` to compare with.
    fn diff_status(
        &self,
        abs: &Path,
    ) -> Result<(Vec<Option<ChangeKind>>, HashSet<usize>), GitError> {
        let args = [
            OsStr::new("diff"),
            OsStr::new("--no-color"),
            OsStr::new("--no-ext-diff"),
            OsStr::new("--no-textconv"),
            // A submodule's working tree is never examined (module docs).
            OsStr::new("--ignore-submodules=dirty"),
            OsStr::new("-U0"),
            OsStr::new("HEAD"),
            OsStr::new("--"),
            abs.as_os_str(),
        ];
        match self.text(args, GIT_TIMEOUT) {
            Ok(text) => Ok(parse_diff_status(&text)),
            Err(e @ GitError::Failed { .. }) => {
                if self.resolve("HEAD")?.is_none() {
                    Ok(Default::default())
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// Parse `git blame --porcelain`. The porcelain format prints a header line
/// `<sha> <orig> <final> [group-size]` per line, and the author/summary fields
/// only on a commit's first appearance, so we cache them by sha.
fn parse_blame(text: &str) -> Vec<BlameLine> {
    // sha -> (author, time, summary)
    let mut meta: std::collections::HashMap<String, (String, i64, String)> =
        std::collections::HashMap::new();
    let mut result: Vec<BlameLine> = Vec::new();
    let mut cur_sha = String::new();
    let mut cur_final = 0usize;

    for line in text.lines() {
        if let Some((sha, rest)) = line.split_once(' ')
            && is_full_sha(sha)
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
        } else if line.starts_with('\t') {
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
    result
}

/// Parse `git diff -U0 HEAD -- <file>` hunk headers into per-line status.
/// `@@ -oldStart,oldCount +newStart,newCount @@`.
fn parse_diff_status(text: &str) -> (Vec<Option<ChangeKind>>, HashSet<usize>) {
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
    (status, deleted_at)
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

/// Whether `s` is a FULL object id: 40 hex digits (SHA-1 repositories) or 64
/// (SHA-256 repositories) — what `%H` and blame's porcelain headers print.
fn is_full_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `s` has the shape of a commit id — abbreviated or full, never
/// anything else. This is not an existence check, it is an argv check: a
/// commit id is the one piece of git metadata clew takes from repository
/// content and hands straight back to `git` in a position where a leading `-`
/// makes it an OPTION, and `git show --output=<path>` truncates and rewrites
/// that path. Everything that parses or forwards a sha goes through here —
/// the server's `validate_git_op` calls this very function, so the local and
/// the remote path share one definition of what a sha may look like.
pub fn is_hex_sha(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Placed immediately before a revision argument so git stops parsing options
/// there. The shape checks above are the real gate; this is the second lock,
/// so a future caller that forwards an unvalidated rev still cannot turn it
/// into `--output=<path>`. Needs git >= 2.24 ([`MIN_GIT`]).
const END_OF_OPTIONS: &str = "--end-of-options";

/// `Err(Refused)` unless `sha` is shaped like a commit id ([`is_hex_sha`]).
fn check_sha(sha: &str) -> Result<(), GitError> {
    if is_hex_sha(sha) {
        Ok(())
    } else {
        Err(GitError::Refused(format!("{sha:?} is not a commit id")))
    }
}

/// `Err(Refused)` unless `rel` stays inside the project
/// ([`crate::statefile::safe_rel`]).
fn check_rel(rel: &str) -> Result<(), GitError> {
    if crate::statefile::safe_rel(rel) {
        Ok(())
    } else {
        Err(GitError::Refused(format!(
            "{rel:?} is not a path inside the project"
        )))
    }
}

/// `Err(Refused)` for a base that git would read as an option.
fn check_base(base: &str) -> Result<(), GitError> {
    if base.is_empty() || base.starts_with('-') {
        Err(GitError::Refused(format!("{base:?} is not a revision")))
    } else {
        Ok(())
    }
}

/// The unified diff of `abs` against `HEAD`, one tagged line per row. `None`
/// when the file is not tracked, the root is not in a repository, or there is
/// no commit to compare with yet; an empty vec means "no changes".
///
/// "Not tracked" is asked of git directly: `git diff HEAD -- <untracked>`
/// succeeds with no output, which is indistinguishable from an unchanged file.
pub fn diff_lines(root: &Path, abs: &Path) -> Result<Option<Vec<DiffLine>>, GitError> {
    let git = match Git::open(root) {
        Ok(git) => git,
        Err(GitError::NotARepository) => return Ok(None),
        Err(e) => return Err(e),
    };
    let tracked = git.check([
        OsStr::new("ls-files"),
        OsStr::new("--error-unmatch"),
        OsStr::new("--"),
        abs.as_os_str(),
    ])?;
    if !tracked {
        return Ok(None);
    }
    let args = [
        OsStr::new("diff"),
        OsStr::new("--no-color"),
        OsStr::new("--no-ext-diff"),
        OsStr::new("--no-textconv"),
        // A submodule's working tree is never examined (module docs).
        OsStr::new("--ignore-submodules=dirty"),
        OsStr::new("HEAD"),
        OsStr::new("--"),
        abs.as_os_str(),
    ];
    match git.text(args, GIT_TIMEOUT) {
        Ok(text) => Ok(Some(classify_diff(&text))),
        Err(e @ GitError::Failed { .. }) => {
            if git.resolve("HEAD")?.is_none() {
                Ok(None)
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Tag each line of a unified diff. Stateful on purpose: `---`/`+++` are file
/// headers only BETWEEN a `diff ` line and the first hunk. Inside a hunk the
/// first character is the line's marker, so an added line whose text starts
/// with `++` (`+++x`) is an addition, not a header — classifying by prefix
/// alone got exactly that wrong.
fn classify_diff(text: &str) -> Vec<DiffLine> {
    let mut in_hunk = false;
    text.lines()
        .map(|line| {
            let kind = if line.starts_with("@@") {
                in_hunk = true;
                DiffKind::Hunk
            } else if line.starts_with("diff ") {
                in_hunk = false;
                DiffKind::Header
            } else if in_hunk {
                match line.as_bytes().first() {
                    Some(b'+') => DiffKind::Add,
                    Some(b'-') => DiffKind::Remove,
                    Some(b' ') | Some(b'\\') | None => DiffKind::Context,
                    Some(_) => {
                        in_hunk = false;
                        DiffKind::Header
                    }
                }
            } else {
                DiffKind::Header
            };
            DiffLine {
                kind,
                text: line.to_string(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Branch/PR review: the diff of the current work against a base, for the
// narrated "review changes" walkthrough. Scoped to the project directory: a
// subdirectory project reviews its own changes, with paths relative to it.
// ---------------------------------------------------------------------------

/// The base to review the current work against, with a human label: the
/// branch's merge-base with `main`/`master` (the "PR diff"), else the previous
/// commit. `None` when there is nothing to review (no commit, or a single one
/// with no base branch to compare with). Diff the returned base with
/// `base...HEAD`; list its commits with `base..HEAD`.
pub fn review_base(root: &Path) -> Result<Option<(String, String)>, GitError> {
    let git = Git::open(root)?;
    let Some(head) = git.resolve("HEAD")? else {
        return Ok(None);
    };
    // `base` comes from this fixed list, never from outside.
    for base in ["main", "master"] {
        let Some(base_id) = git.resolve(base)? else {
            continue;
        };
        if base_id == head {
            continue;
        }
        let range = format!("{base}...HEAD");
        // `--quiet` exits 1 when there ARE changes; `-- .` scopes the
        // question to the project directory.
        let unchanged = git.check([
            "diff",
            "--quiet",
            "--no-ext-diff",
            END_OF_OPTIONS,
            &range,
            "--",
            ".",
        ])?;
        if !unchanged {
            return Ok(Some((base.to_string(), format!("vs {base}"))));
        }
    }
    // No base branch (or HEAD is the base): review the last commit instead.
    Ok(git
        .resolve("HEAD~1")?
        .map(|_| ("HEAD~1".to_string(), "last commit".to_string())))
}

/// Files changed in `base...HEAD` under the project directory, as
/// `(root-relative path, status letter)` (A/M/D/R…).
pub fn changed_files(root: &Path, base: &str) -> Result<Vec<(String, char)>, GitError> {
    check_base(base)?;
    let git = Git::open(root)?;
    // `base` is a branch name, so the range it builds is another string that
    // must not be read as an option (see [`END_OF_OPTIONS`]).
    let range = format!("{base}...HEAD");
    let out = git.bytes(
        [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--relative",
            "--name-status",
            "-z",
            END_OF_OPTIONS,
            &range,
        ],
        GIT_TIMEOUT,
    )?;
    Ok(parse_name_status_z(&out))
}

/// `--name-status -z` output: `STATUS\0path\0`, or `R100\0old\0new\0` /
/// `C75\0src\0dst\0` for renames and copies (whose final path is taken). Paths
/// are raw bytes under `-z` — no quoting to undo.
fn parse_name_status_z(out: &[u8]) -> Vec<(String, char)> {
    let mut fields = out.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let Some(&letter) = status.first() else {
            break;
        };
        let letter = letter as char;
        let path = if matches!(letter, 'R' | 'C') {
            fields.next();
            fields.next()
        } else {
            fields.next()
        };
        let Some(path) = path else {
            break;
        };
        if let Ok(path) = std::str::from_utf8(path) {
            files.push((path.to_string(), letter));
        }
    }
    files
}

/// The unified patch of `base...HEAD` under the project directory (paths
/// relative to it), truncated to `max_bytes` (with a marker) so a huge diff
/// can't blow the LLM context — and git is stopped once it has produced that
/// much, rather than after it has produced all of it.
pub fn range_patch(root: &Path, base: &str, max_bytes: usize) -> Result<String, GitError> {
    check_base(base)?;
    let git = Git::open(root)?;
    let range = format!("{base}...HEAD");
    git.text_truncated(
        [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--relative",
            END_OF_OPTIONS,
            &range,
        ],
        max_bytes,
    )
}

/// Subjects of the commits in `base..HEAD` that touched the project
/// directory, oldest first (the change's intent).
pub fn commit_subjects(root: &Path, base: &str) -> Result<Vec<String>, GitError> {
    check_base(base)?;
    let git = Git::open(root)?;
    let range = format!("{base}..HEAD");
    let text = git.text(
        [
            "log",
            "--reverse",
            "--no-color",
            "--format=%s",
            END_OF_OPTIONS,
            &range,
            "--",
            ".",
        ],
        GIT_HISTORY_TIMEOUT,
    )?;
    Ok(text
        .lines()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .collect())
}

/// Appended to output that was cut short.
const TRUNCATED_MARKER: &str = "\n… (truncated)\n";

/// Truncate `text` to at most `max_bytes` on a char boundary, with a marker.
fn truncate_marked(text: &mut String, max_bytes: usize) {
    if text.len() > max_bytes {
        cut_at_char_boundary(text, max_bytes);
        text.push_str(TRUNCATED_MARKER);
    }
}

/// Shorten `text` to at most `max_bytes`, backing off to a char boundary.
fn cut_at_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() > max_bytes {
        let mut cut = max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
}

/// The full message (subject + body) of commit `sha`; `None` when it has
/// none. Refused for a sha that is not shaped like one: it would land in an
/// option slot (see [`is_hex_sha`]).
pub fn commit_message(root: &Path, sha: &str) -> Result<Option<String>, GitError> {
    check_sha(sha)?;
    let git = Git::open(root)?;
    let text = git.text(
        [
            "show",
            "-s",
            "--no-color",
            "--format=%B",
            END_OF_OPTIONS,
            sha,
        ],
        GIT_TIMEOUT,
    )?;
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

/// The diff commit `sha` made to `rel` (that file only; root-relative),
/// truncated to `max_bytes` — empty when the commit did not touch it. The
/// empty `--format=` suppresses the commit header, leaving just the patch.
/// Refused for a sha that is not shaped like one (see [`is_hex_sha`]) or a rel
/// that would leave the project.
pub fn commit_file_diff(
    root: &Path,
    sha: &str,
    rel: &str,
    max_bytes: usize,
) -> Result<String, GitError> {
    check_sha(sha)?;
    check_rel(rel)?;
    let git = Git::open(root)?;
    git.text_truncated(
        [
            "show",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--format=",
            END_OF_OPTIONS,
            sha,
            "--",
            rel,
        ],
        max_bytes,
    )
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

/// The history format: every record starts with [`RECORD`] — a NUL, then RS —
/// at the START OF A LINE, and its fields are separated by NUL.
///
/// That is what makes the stream unforgeable. Nothing a repository controls
/// can put a NUL at the start of a line: author and subject are header fields,
/// which hold no newline (and in which a NUL can only shift the fields of the
/// record it belongs to, never start another); a PATH cannot hold a NUL at
/// all; and every line of a `-L` patch begins with its own marker (` `, `+`,
/// `-`, `@`, `\`, `diff`, …). Paths DO get to start lines, though: `-L`
/// prints them raw — a newline in a historical file name breaks its diff
/// header over several lines (only `--name-only` quotes them) — which is why
/// a line-initial RS alone was not enough, and why `-L` paths are decoded by
/// [`patch_path`] rather than read off a `+++` line.
const HIST_FORMAT: &str = "--format=%x00%x1e%H%x00%an%x00%at%x00%s";
const RECORD: &str = "\0\x1e";

impl Git<'_> {
    /// Whether `HEAD` exists at all: a history query in a repository without
    /// a commit has an empty answer, not an error.
    fn has_head(&self) -> Result<bool, GitError> {
        Ok(self.resolve("HEAD")?.is_some())
    }
}

/// Commits that touched `rel` (root-relative), newest first, following
/// renames. Capped at `limit`. Empty for a path with no history (untracked,
/// or a repository with no commit yet).
pub fn file_history(root: &Path, rel: &str, limit: usize) -> Result<Vec<HistCommit>, GitError> {
    check_rel(rel)?;
    let git = Git::open(root)?;
    let n = format!("-n{limit}");
    let args = [
        "log",
        "--follow",
        "--no-color",
        &n,
        HIST_FORMAT,
        "--name-only",
        "--",
        rel,
    ];
    let out = match git.text(args, GIT_HISTORY_TIMEOUT) {
        Ok(out) => out,
        Err(e @ GitError::Failed { .. }) => {
            return if git.has_head()? {
                Err(e)
            } else {
                Ok(Vec::new())
            };
        }
        Err(e) => return Err(e),
    };
    Ok(parse_hist(&out, rel, &git.repo, HistPaths::NameOnly))
}

/// Commits that changed lines `start..=end` (1-based) of `rel` (root-relative),
/// newest first. Uses `git log -L`, which scopes history to that line range (a
/// function) and follows it across renames. Empty for a path `HEAD` does not
/// have; an error for a range `HEAD`'s version of the file does not have.
pub fn symbol_history(
    root: &Path,
    rel: &str,
    start: usize,
    end: usize,
    limit: usize,
) -> Result<Vec<HistCommit>, GitError> {
    if start == 0 || end < start {
        return Err(GitError::Refused(format!(
            "{start}..={end} is not a line range"
        )));
    }
    check_rel(rel)?;
    let git = Git::open(root)?;
    // `rel` rides inside the `-L` argument (resolved against the working
    // directory, i.e. the root), so it is never an argv slot of its own.
    let range = format!("-L{start},{end}:{rel}");
    let n = format!("-n{limit}");
    let args = [
        "log",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        &n,
        HIST_FORMAT,
        &range,
    ];
    let out = match git.text(args, GIT_HISTORY_TIMEOUT) {
        Ok(out) => out,
        Err(e @ GitError::Failed { .. }) => {
            return if git.in_head(OsStr::new(rel))? {
                Err(e)
            } else {
                Ok(Vec::new())
            };
        }
        Err(e) => return Err(e),
    };
    Ok(parse_hist(&out, rel, &git.repo, HistPaths::Patch))
}

/// Where a history record says which path the file had at that commit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HistPaths {
    /// `--name-only`: the first non-empty line after the header (C-quoted
    /// when it holds anything unusual, so always one line).
    NameOnly,
    /// `-L`'s patch: the `b/` side of its diff header ([`patch_path`]).
    Patch,
}

/// The records of [`HIST_FORMAT`] output, each starting just after its
/// [`RECORD`] marker: the header line, then the record's body.
fn records(out: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = match out.strip_prefix(RECORD) {
        Some(rest) => rest,
        // git prints nothing before the first record; tolerate it anyway.
        None => match out.find("\n\0\x1e") {
            Some(i) => &out[i + 1 + RECORD.len()..],
            None => return found,
        },
    };
    loop {
        match rest.find("\n\0\x1e") {
            Some(i) => {
                found.push(&rest[..i]);
                rest = &rest[i + 1 + RECORD.len()..];
            }
            None => {
                found.push(rest);
                return found;
            }
        }
    }
}

/// Parse [`HIST_FORMAT`] output (see there for why it cannot be forged).
///
/// A record whose header is malformed — above all one whose sha is not a full
/// object id — is dropped. A record that names no path (a merge commit prints
/// no file list or patch) inherits the path of the next-newer record, which is
/// the file's name at that point unless the merge itself renamed it. Records
/// whose path lies outside the project directory are dropped (the time-travel
/// view cannot — and, remotely, may not — read it), and so are `-L` records
/// whose diff header does not decode to exactly one path: no record is ever
/// shown under a path it did not have. After either, a path-less record has
/// nothing trustworthy to inherit and is dropped too.
fn parse_hist(out: &str, current_rel: &str, repo: &Repo, paths: HistPaths) -> Vec<HistCommit> {
    let mut commits = Vec::new();
    // The path of the newest record seen so far, for records that name none.
    let mut carried: Option<String> = Some(current_rel.to_string());
    for record in records(out) {
        let (head, body) = record.split_once('\n').unwrap_or((record, ""));
        let Some(mut commit) = parse_hist_header(head) else {
            continue;
        };
        let path = match paths {
            HistPaths::NameOnly => match body.split('\n').find(|l| !l.is_empty()) {
                None => carried.clone(),
                Some(line) => unquote_path(line).and_then(|top| repo.root_rel(&top)),
            },
            HistPaths::Patch => match patch_path(body) {
                PatchPath::NoDiff => carried.clone(),
                PatchPath::Named(top) => repo.root_rel(&top),
                PatchPath::Undecodable => None,
            },
        };
        carried = path.clone();
        if let Some(path) = path {
            commit.path = path;
            commits.push(commit);
        }
    }
    commits
}

/// One header line (after its [`RECORD`]): `sha NUL author NUL time NUL
/// subject`. The sha must be a full object id — `%H` never prints anything
/// else, so anything else is not a record git wrote.
fn parse_hist_header(head: &str) -> Option<HistCommit> {
    let mut f = head.splitn(4, '\0');
    let sha = f.next()?;
    if !is_full_sha(sha) {
        return None;
    }
    let author = f.next()?.to_string();
    let time = f.next()?.trim().parse::<i64>().ok()?;
    let subject = f.next()?.to_string();
    Some(HistCommit {
        sha: sha.to_string(),
        author,
        time,
        subject,
        path: String::new(),
    })
}

/// What a `-L` record's patch says the file was called at that commit.
#[derive(Debug, PartialEq, Eq)]
enum PatchPath {
    /// No diff at all (a merge commit may print none).
    NoDiff,
    /// The `b/` side of its diff header, repository-top-relative.
    Named(String),
    /// A diff header that does not decode to exactly one path.
    Undecodable,
}

/// The path at this commit, from a `-L` record's patch `body`.
///
/// git prints `-L` diff headers RAW (its own minimal printer, not the diff
/// machinery; 2.39 does, and nothing here assumes a later one changed) —
/// `diff --git a/X b/Y`, `--- a/X` (or `--- /dev/null`), `+++ b/Y` — so a
/// newline in a historical name breaks the header over extra lines that the
/// NAME chooses, including a `+++ b/<anything>` line or an early `@@` that
/// cuts the header short. Reading the first `+++` line let a repository show
/// a commit under an in-project path of its choosing.
///
/// Instead the header is decoded as a whole: every split of the body into
/// `header` + `\n@@ …` and every `\n+++ b/` inside it is tried as a solution
/// of that exact grammar ([`raw_header_path`]). The true split always solves
/// it — a name holding newlines only makes the header longer — so a forged
/// decoding can only ever appear NEXT TO the true one, never instead of it,
/// and two different answers are [`PatchPath::Undecodable`]: the record is
/// dropped, never mislabelled. The search is bounded ([`patch_path_budget`]);
/// running out of budget is undecodable too.
///
/// A header with no raw decoding at all is not from that printer (a git that
/// prints `-L` through the standard diff machinery, with quoted paths and
/// extended header lines) and is read the standard way
/// ([`standard_header_path`]).
fn patch_path(body: &str) -> PatchPath {
    let body = body.trim_start_matches('\n');
    if body.trim().is_empty() {
        return PatchPath::NoDiff;
    }
    if !body.starts_with("diff --git ") {
        return PatchPath::Undecodable;
    }
    // Where the header can end: before a hunk header, or at the end of the
    // record when there is none.
    let mut ends: Vec<usize> = body.match_indices("\n@@ ").map(|(i, _)| i).collect();
    if ends.is_empty() {
        ends.push(body.trim_end_matches('\n').len());
    }
    let pluses: Vec<usize> = body.match_indices("\n+++ b/").map(|(i, _)| i).collect();
    let mut budget = patch_path_budget(body);
    let mut found: Option<&str> = None;
    for &end in &ends {
        let header = &body[..end];
        // Both paths appear twice in a header, so its closing `+++ b/` sits in
        // the second half: 2·i ≥ end + 16 (see `raw_header_path`).
        let first = pluses.partition_point(|&i| 2 * i < end + 16);
        for &i in pluses[first..].iter().take_while(|&&i| i + 7 < end) {
            // Every candidate costs something, so the number visited is
            // bounded too — not only the bytes compared.
            budget = match budget.checked_sub(1) {
                Some(left) => left,
                None => return PatchPath::Undecodable,
            };
            match raw_header_path(header, i, &mut budget) {
                Err(()) => return PatchPath::Undecodable,
                Ok(None) => {}
                Ok(Some(y)) => match found {
                    None => found = Some(y),
                    Some(previous) if previous == y => {}
                    Some(_) => return PatchPath::Undecodable,
                },
            }
        }
    }
    match found {
        Some(y) => PatchPath::Named(y.to_string()),
        None => {
            standard_header_path(&body[..ends[0]]).map_or(PatchPath::Undecodable, PatchPath::Named)
        }
    }
}

/// How many bytes [`patch_path`] may compare for one record: a few passes
/// over it, plus room for a header of long names. A benign record decodes
/// in one pass over its header.
fn patch_path_budget(body: &str) -> usize {
    4 * body.len() + 64 * 1024
}

/// Whether `header` is exactly `diff --git a/X b/Y` NL `--- a/X` (or `---
/// /dev/null`) NL `+++ b/Y` with its `\n+++ b/` at byte `i`; `Ok(Some(Y))`
/// when it is. X is determined by the lengths — with `L = header.len()`:
/// `2·i = L + 16 + 2·|X|` for `--- a/X`, `2·i = L + 23 + |X|` for
/// `/dev/null` — so each candidate costs one comparison pass, charged to
/// `budget` (`Err` once it is spent).
fn raw_header_path<'h>(
    header: &'h str,
    i: usize,
    budget: &mut usize,
) -> Result<Option<&'h str>, ()> {
    const DIFF: &[u8] = b"diff --git a/";
    let h = header.as_bytes();
    let y = &header[i + 7..];
    let yb = y.as_bytes();
    // Whether `parts`, in order, spell exactly `h`; each part compared is
    // charged to the budget, and the first mismatch ends the comparison.
    let matches = |parts: &[&[u8]], budget: &mut usize| -> Result<bool, ()> {
        let mut at = 0;
        for part in parts {
            let Some(slice) = h.get(at..at + part.len()) else {
                return Ok(false);
            };
            *budget = budget.checked_sub(part.len()).ok_or(())?;
            if slice != *part {
                return Ok(false);
            }
            at += part.len();
        }
        Ok(at == h.len())
    };
    let len = h.len();
    // `--- a/X`
    if 2 * i >= len + 16 && (2 * i - len - 16).is_multiple_of(2) {
        let xl = (2 * i - len - 16) / 2;
        if let Some(x) = h.get(DIFF.len()..DIFF.len() + xl)
            && matches(
                &[DIFF, x, b" b/", yb, b"\n--- a/", x, b"\n+++ b/", yb],
                budget,
            )?
        {
            return Ok(Some(y));
        }
    }
    // `--- /dev/null` (the file was added by this commit)
    if 2 * i >= len + 23 {
        let xl = 2 * i - len - 23;
        if let Some(x) = h.get(DIFF.len()..DIFF.len() + xl)
            && matches(
                &[DIFF, x, b" b/", yb, b"\n--- /dev/null", b"\n+++ b/", yb],
                budget,
            )?
        {
            return Ok(Some(y));
        }
    }
    Ok(None)
}

/// The `b/` path of a diff header printed the standard way: one line per
/// field (paths C-quoted when unusual, so a name can never break a line),
/// the `diff --git` line, known extended header lines, one `---` line, and
/// the closing `+++` line.
fn standard_header_path(header: &str) -> Option<String> {
    const EXTENDED: &[&str] = &[
        "index ",
        "old mode ",
        "new mode ",
        "deleted file mode ",
        "new file mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
    ];
    let mut lines = header.split('\n');
    if !lines.next()?.starts_with("diff --git ") {
        return None;
    }
    let rest: Vec<&str> = lines.collect();
    let (last, middle) = rest.split_last()?;
    let target = last.strip_prefix("+++ ")?;
    let (minus, extended): (Vec<&str>, Vec<&str>) =
        middle.iter().partition(|l| l.starts_with("--- "));
    if minus.len() != 1
        || !extended
            .iter()
            .all(|l| EXTENDED.iter().any(|p| l.starts_with(p)))
    {
        return None;
    }
    unquote_path(target)?.strip_prefix("b/").map(str::to_string)
}

/// The full text of `rel` (root-relative) as of commit `sha`: `None` when it
/// is absent at that commit, is not a file there (a directory, a submodule),
/// or is binary. Refused when `rel` would leave the project or `sha` is not
/// shaped like one (see [`is_hex_sha`]).
///
/// The entry is looked up in the commit's TREE first (`ls-tree`, a pathspec
/// resolved against the root, like every other), and only then is its blob
/// read: "not at this commit" is a tree answer, so a blob that cannot be read
/// — a partial clone that does not have it — is an error rather than a file
/// that "did not exist".
pub fn file_at(root: &Path, sha: &str, rel: &str) -> Result<Option<String>, GitError> {
    check_sha(sha)?;
    check_rel(rel)?;
    let git = Git::open(root)?;
    let listing = git.bytes(
        ["ls-tree", "-z", END_OF_OPTIONS, sha, "--", rel],
        GIT_TIMEOUT,
    )?;
    // `<mode> SP <type> SP <oid> TAB <path>`
    let Some(entry) = listing.split(|&b| b == 0).find(|e| !e.is_empty()) else {
        return Ok(None);
    };
    let Some(meta) = entry.split(|&b| b == b'\t').next() else {
        return Ok(None);
    };
    let mut fields = meta.split(|&b| b == b' ');
    let (_mode, kind, oid) = (fields.next(), fields.next(), fields.next());
    let (Some(b"blob"), Some(oid)) = (kind, oid) else {
        return Ok(None);
    };
    let oid = String::from_utf8_lossy(oid).into_owned();
    if !is_full_sha(&oid) {
        return Err(GitError::Io(format!("unexpected ls-tree entry for {rel}")));
    }
    let out = git.bytes(["cat-file", "blob", END_OF_OPTIONS, &oid], GIT_TIMEOUT)?;
    if out.contains(&0) {
        return Ok(None); // binary
    }
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}

/// The 1-based line numbers in `rel` (root-relative) @ `sha` that this commit
/// added or changed (the '+' side of its diff), for highlighting what a step
/// introduced; empty when the commit did not touch it. Refused for a sha that
/// is not shaped like one (see [`is_hex_sha`]) or a rel that would leave the
/// project.
pub fn commit_added_lines(root: &Path, sha: &str, rel: &str) -> Result<HashSet<usize>, GitError> {
    check_sha(sha)?;
    check_rel(rel)?;
    let git = Git::open(root)?;
    let diff = git.text(
        [
            "show",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--format=",
            END_OF_OPTIONS,
            sha,
            "--",
            rel,
        ],
        GIT_TIMEOUT,
    )?;
    Ok(added_lines_from_diff(&diff))
}

/// Parse a unified — or combined (`diff --cc`, a merge commit) — diff,
/// returning the result-side line numbers of added lines.
///
/// A combined diff has one marker COLUMN per parent (`@@@` headers for two
/// parents). A line with `-` in any column is not in the result and takes no
/// line number; any other line is, and it counts as added when some column is
/// `+` (it is new relative to at least one parent — what git itself colours
/// as an addition). Reading only the first character misnumbered every line
/// after a ` -` (present in the first parent, dropped by the merge).
///
/// Headers are recognised by position, not by prefix: `---`/`+++` are file
/// headers only between a `diff ` line and the first hunk, so an added line
/// whose text starts with `++` is still counted. An EMPTY line inside a hunk
/// is a blank context line — what git prints for one under
/// `diff.suppressBlankEmpty` (pinned off, so this only guards a git that
/// ignores the pin); ending the hunk there dropped every later addition.
fn added_lines_from_diff(diff: &str) -> HashSet<usize> {
    let mut set = HashSet::new();
    // (marker columns, next result-side line number) while inside a hunk.
    let mut hunk: Option<(usize, usize)> = None;
    for line in diff.lines() {
        if line.starts_with("diff ") {
            hunk = None;
            continue;
        }
        if line.starts_with('@') {
            hunk = parse_any_hunk_header(line);
            continue;
        }
        let Some((cols, next)) = hunk.as_mut() else {
            continue; // a file header line (`index`, `---`, `+++`, modes, …)
        };
        // `\ No newline at end of file` describes the line before it.
        if line.starts_with('\\') {
            continue;
        }
        if line.is_empty() {
            *next += 1;
            continue;
        }
        let Some(markers) = line.as_bytes().get(..*cols) else {
            hunk = None;
            continue;
        };
        if !markers.iter().all(|m| matches!(m, b' ' | b'+' | b'-')) {
            hunk = None;
            continue;
        }
        if markers.contains(&b'-') {
            continue; // not in the result
        }
        if markers.contains(&b'+') {
            set.insert(*next);
        }
        *next += 1;
    }
    set
}

/// `(marker columns, result start line)` from a unified (`@@ -a,b +c,d @@`)
/// or combined (`@@@ -a,b -c,d +e,f @@@`) hunk header: one column per parent,
/// i.e. one fewer than the `@` run, and the result range is the `+` one.
fn parse_any_hunk_header(line: &str) -> Option<(usize, usize)> {
    let ats = line.bytes().take_while(|&b| b == b'@').count();
    if ats < 2 {
        return None;
    }
    let plus = line[ats..].split(' ').find(|t| t.starts_with('+'))?;
    let start = plus[1..].split(',').next()?.parse().ok()?;
    Some((ats - 1, start))
}

/// Every tracked file under `root`, root-relative — the files git considers
/// part of the repository whatever `.gitignore` says. `Ok(None)` when `root`
/// is not in a git work tree. Paths git cannot hand over as UTF-8 are skipped.
pub fn tracked_files(root: &Path) -> Result<Option<Vec<String>>, GitError> {
    let git = match Git::open(root) {
        Ok(git) => git,
        Err(GitError::NotARepository) => return Ok(None),
        Err(e) => return Err(e),
    };
    let out = git.bytes(["ls-files", "-z", "--cached"], GIT_TIMEOUT)?;
    Ok(Some(
        out.split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .filter_map(|p| std::str::from_utf8(p).ok())
            .map(str::to_string)
            .collect(),
    ))
}

// ----------------------------------------------------------- one operation

/// Run one [`clew_protocol::GitOp`] against the repository at `root` and
/// answer it with the [`clew_protocol::GitResult`] variant of the same name —
/// or with the reason it could not be answered, which the server sends as an
/// `Event::Error` and the client shows (see the module docs). Blocking (git
/// subprocesses).
///
/// The one dispatch for both places a git operation runs: clew-server, for a
/// remote project (after validating the op's arguments — it takes them from
/// the wire), and the client, for a local one. The client used to carry its
/// own copy of this match, answering in JSON it then decoded; two copies are
/// how the local and remote answers drift apart.
pub fn run_op(root: &Path, op: clew_protocol::GitOp) -> Result<clew_protocol::GitResult, GitError> {
    use clew_protocol::{GitOp, GitResult};
    Ok(match op {
        GitOp::FileHistory { rel, limit } => {
            GitResult::FileHistory(file_history(root, &rel, limit)?)
        }
        GitOp::SymbolHistory {
            rel,
            start,
            end,
            limit,
        } => GitResult::SymbolHistory(symbol_history(root, &rel, start, end, limit)?),
        GitOp::FileAt { sha, rel } => GitResult::FileAt(file_at(root, &sha, &rel)?),
        GitOp::AddedLines { sha, rel } => {
            GitResult::AddedLines(commit_added_lines(root, &sha, &rel)?)
        }
        GitOp::CommitMessage { sha } => GitResult::CommitMessage(commit_message(root, &sha)?),
        GitOp::CommitFileDiff {
            sha,
            rel,
            max_bytes,
        } => GitResult::CommitFileDiff(commit_file_diff(root, &sha, &rel, max_bytes)?),
        GitOp::DiffLines { rel } => {
            check_rel(&rel)?;
            GitResult::DiffLines(diff_lines(root, &root.join(&rel))?)
        }
        GitOp::ReviewBase => GitResult::ReviewBase(review_base(root)?),
        GitOp::CommitSubjects { base } => GitResult::CommitSubjects(commit_subjects(root, &base)?),
        GitOp::ChangedFiles { base } => GitResult::ChangedFiles(changed_files(root, &base)?),
        GitOp::RangePatch { base, max_bytes } => {
            GitResult::RangePatch(range_patch(root, &base, max_bytes)?)
        }
    })
}

#[cfg(test)]
mod tests;
