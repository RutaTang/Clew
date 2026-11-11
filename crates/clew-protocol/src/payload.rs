//! The typed results that ride inside replies: git answers, code statistics,
//! the project call graph and the type-structure index — computed by clew-core
//! where the files live, rendered by the client.
//!
//! They are defined HERE, not in clew-core, for two reasons. The dependency
//! points this way (core depends on the protocol, never the reverse), and the
//! build fingerprint (`build.rs`) covers only this crate. When these shapes
//! lived in core and crossed the wire as JSON strings, a change to one moved
//! neither `PROTOCOL_VERSION` nor the fingerprint: the handshake passed, the
//! string failed to decode on the other side, and the client quietly showed a
//! default — an empty Time Travel, a project with zero lines of code. As wire
//! types they are covered by the fingerprint, and a mismatched build is refused
//! at the handshake instead.
//!
//! clew-core keeps every piece of logic that PRODUCES these (and re-exports
//! them from its own modules, so its callers are unchanged). The only methods
//! here are read-only accessors, which Rust only allows next to the type.
//! Where core has a richer type of its own — the call graph, whose adjacency
//! indices its accessors trust — this is the wire form, and core converts.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{GitOp, Rel};

// -- git ----------------------------------------------------------------------

/// One commit in a file's (or a symbol's) history: short sha, author,
/// authored time (unix seconds), subject, and the file's path AS OF THAT
/// COMMIT, relative to the project root (renames are followed, and the path
/// goes straight back into `GitOp::FileAt` / `GitOp::AddedLines`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistCommit {
    pub sha: String,
    pub author: String,
    pub time: i64,
    pub subject: String,
    pub path: Rel,
}

/// How often a file changed over the recent history a `GitOp::Churn` looked
/// at: its commits among those, and the time of the latest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChurn {
    pub rel: Rel,
    pub commits: u32,
    /// Unix seconds of the most recent commit touching the file.
    pub last: i64,
}

/// What one line of a unified diff is, for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffKind {
    /// `diff --git`, `index`, `---`/`+++` between a `diff` line and a hunk.
    Header,
    /// An `@@ … @@` hunk header.
    Hunk,
    Context,
    Add,
    Remove,
}

/// One line of a unified diff, tagged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// The answer to one [`GitOp`], in the variant of the same name — so a reply
/// can be checked against the request it answers ([`GitResult::answers`])
/// before anything reads it.
///
/// Every variant is the complete answer: an empty history, an empty diff or a
/// missing commit message is a real result. A git operation that could not be
/// answered — git missing or timed out, the project not a repository, an
/// object that could not be read, a refused argument — is not one of these:
/// `clew_core::git::run_op` returns it as a `GitError`, the server replies it
/// as an `Event::Error` naming the reason, and the client's local path turns
/// it into the same `Err`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitResult {
    /// Newest first, following renames; at most the op's `limit`.
    FileHistory(Vec<HistCommit>),
    /// Commits that touched the symbol's line range, newest first.
    SymbolHistory(Vec<HistCommit>),
    /// The file's full text at the commit; `None` when it did not exist
    /// there or is binary.
    FileAt(Option<String>),
    /// The 1-based lines of the file at the commit that the commit added or
    /// changed (the `+` side of its diff).
    AddedLines(HashSet<usize>),
    /// Subject and body; `None` when the commit has no message.
    CommitMessage(Option<String>),
    /// The commit's patch for that one file, truncated to the op's
    /// `max_bytes`.
    CommitFileDiff(String),
    /// The working file against `HEAD`, one tagged line per row: `None` when
    /// the file is untracked (or not in a repository), empty when unchanged.
    DiffLines(Option<Vec<DiffLine>>),
    /// `(base, label)`: the revision to review the current work against and
    /// a human label for it — the merge-base with `main`/`master`, else the
    /// previous commit. `None` when there is nothing to review.
    ReviewBase(Option<(String, String)>),
    /// Subjects of the commits in `base..HEAD`, oldest first.
    CommitSubjects(Vec<String>),
    /// `(rel, status letter)` for each file changed in `base...HEAD`
    /// (`A`/`M`/`D`/`R`…).
    ChangedFiles(Vec<(Rel, char)>),
    /// The unified patch of `base...HEAD`, truncated to the op's `max_bytes`.
    RangePatch(String),
    /// The files most changed over the recent history, most first (see
    /// `GitOp::Churn`).
    Churn(Vec<FileChurn>),
    /// `(rel, letter)` for each file the working tree changes against HEAD
    /// (`GitOp::WorkingStatus`): `M`odified, `A`dded, `D`eleted, `R`enamed,
    /// `U`ntracked, `!` in conflict.
    WorkingStatus(Vec<(Rel, char)>),
    /// What HEAD is (`GitOp::Head`); `None` when it names nothing.
    Head(Option<HeadName>),
    /// A page of the project's commits (`GitOp::Log`).
    Log(LogPage),
    /// The commit whole (`GitOp::CommitDetail`); `None` when the id names
    /// no commit.
    CommitDetail(Option<CommitDetail>),
}

/// A page of the project's log (`GitOp::Log`): its commits, newest first,
/// and whether more come after them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogPage {
    pub commits: Vec<LogCommit>,
    pub more: bool,
}

/// One commit of the project's log: its full id, its parents' (none for a
/// root commit, two or more for a merge), author, authored time (unix
/// seconds), subject, and the names that point at it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogCommit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub time: i64,
    pub subject: String,
    pub refs: Vec<LogRef>,
}

/// A name pointing at a commit, short (`main`, `origin/main`, `v1.0`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRef {
    pub kind: RefKind,
    pub name: String,
}

/// What a [`LogRef`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    /// HEAD: the branch checked out (by its name), or `HEAD` itself when
    /// detached.
    Head,
    /// Another local branch.
    Branch,
    /// A remote-tracking branch (`origin/main`).
    Remote,
    Tag,
}

/// The most files a [`CommitDetail`] lists: a commit that vendors a
/// library or reformats a tree changes thousands, and a list that long is
/// read by no one.
pub const MAX_COMMIT_FILES: usize = 1000;

/// One commit whole (`GitOp::CommitDetail`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDetail {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub time: i64,
    /// Subject and body, as written.
    pub message: String,
    /// The files it changed under the project, against its first parent
    /// (against nothing for a root commit), in git's order — at most
    /// [`MAX_COMMIT_FILES`] of them.
    pub files: Vec<CommitFile>,
    /// How many more it changed than are listed.
    pub more_files: usize,
}

/// A file a commit changed: its path after the commit (before it, for one
/// deleted), the path it had before when renamed or copied, git's status
/// letter (`A`dded, `M`odified, `D`eleted, `R`enamed, `C`opied, `T`ype
/// changed), and its lines added and removed — `None` for a binary file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFile {
    pub rel: Rel,
    pub old: Option<Rel>,
    pub status: char,
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

/// What HEAD is: a branch, or, detached, a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeadName {
    /// The branch checked out, by its short name (`main`, `feature/x`) —
    /// one with no commit yet too.
    Branch(String),
    /// Detached: the full id of the commit HEAD is at.
    Detached(String),
}

impl GitResult {
    /// Whether this is the answer to `op`: the same operation. (Only the
    /// operation — the arguments are the server's to have honoured.)
    pub fn answers(&self, op: &GitOp) -> bool {
        matches!(
            (self, op),
            (GitResult::FileHistory(_), GitOp::FileHistory { .. })
                | (GitResult::SymbolHistory(_), GitOp::SymbolHistory { .. })
                | (GitResult::FileAt(_), GitOp::FileAt { .. })
                | (GitResult::AddedLines(_), GitOp::AddedLines { .. })
                | (GitResult::CommitMessage(_), GitOp::CommitMessage { .. })
                | (GitResult::CommitFileDiff(_), GitOp::CommitFileDiff { .. })
                | (GitResult::DiffLines(_), GitOp::DiffLines { .. })
                | (GitResult::ReviewBase(_), GitOp::ReviewBase)
                | (GitResult::CommitSubjects(_), GitOp::CommitSubjects { .. })
                | (GitResult::ChangedFiles(_), GitOp::ChangedFiles { .. })
                | (GitResult::RangePatch(_), GitOp::RangePatch { .. })
                | (GitResult::Churn(_), GitOp::Churn { .. })
                | (GitResult::WorkingStatus(_), GitOp::WorkingStatus)
                | (GitResult::Head(_), GitOp::Head)
                | (GitResult::Log(_), GitOp::Log { .. })
                | (GitResult::CommitDetail(_), GitOp::CommitDetail { .. })
        )
    }

    /// The variant's name, for error messages ("expected FileAt, got …").
    pub fn name(&self) -> &'static str {
        match self {
            GitResult::FileHistory(_) => "FileHistory",
            GitResult::SymbolHistory(_) => "SymbolHistory",
            GitResult::FileAt(_) => "FileAt",
            GitResult::AddedLines(_) => "AddedLines",
            GitResult::CommitMessage(_) => "CommitMessage",
            GitResult::CommitFileDiff(_) => "CommitFileDiff",
            GitResult::DiffLines(_) => "DiffLines",
            GitResult::ReviewBase(_) => "ReviewBase",
            GitResult::CommitSubjects(_) => "CommitSubjects",
            GitResult::ChangedFiles(_) => "ChangedFiles",
            GitResult::RangePatch(_) => "RangePatch",
            GitResult::Churn(_) => "Churn",
            GitResult::WorkingStatus(_) => "WorkingStatus",
            GitResult::Head(_) => "Head",
            GitResult::Log(_) => "Log",
            GitResult::CommitDetail(_) => "CommitDetail",
        }
    }
}

// -- code statistics ----------------------------------------------------------

/// Project-wide totals across every counted language.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// One file in the "largest files" list. The path is relative to the project
/// root so the report is host-agnostic; the client rebuilds the absolute path
/// as `root.join(rel)` when the row is opened.
///
/// A `String`, like every rel: it comes from the scanner, which only lists
/// UTF-8 names, and it always serializes (a `PathBuf` holding a non-UTF-8 name
/// failed to, which emptied the whole report on the server).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub rel: Rel,
    pub lang: String,
    pub code: usize,
    pub lines: usize,
}

/// The code-statistics report the Stats view renders (reply to `Stats`;
/// computed by `clew_core::stats`).
///
/// Also the shape of the client's on-disk stats cache, which is why `skipped`
/// is defaulted: a cache written before the field existed must still load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsReport {
    pub totals: Totals,
    /// Languages sorted by code lines, descending.
    pub langs: Vec<LangStat>,
    /// Largest files by total lines, descending.
    pub top_files: Vec<FileStat>,
    /// Files in a counted language that were NOT counted: over the per-file
    /// size cap, or unreadable.
    #[serde(default)]
    pub skipped: usize,
}

impl StatsReport {
    pub fn is_empty(&self) -> bool {
        self.langs.is_empty()
    }
}

// -- project maps -------------------------------------------------------------

/// What a C or C++ macro (or a whole line) expands to at a point, read
/// from the `#define`s in effect there — the file's and its headers'.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroExpansion {
    /// The macro expanded; empty when a whole line was.
    pub name: String,
    /// What was expanded, as written.
    pub invocation: String,
    /// What the preprocessor makes of it.
    pub expanded: String,
    /// Where the macro is defined, relative to the project, with its 1-based
    /// line; empty (and 0) for one the command line defines, or one in a
    /// header outside the project.
    pub file: String,
    pub line: usize,
    /// Its `#define`, as written.
    pub definition: String,
    /// Whether the expansion was cut short.
    pub truncated: bool,
}

/// One of a project's maps (reply to `ProjectMaps`): what a language or a
/// framework says about how its code fits together — a database's tables and
/// their foreign keys, a web app's routes and the handlers they reach, an OTP
/// application's supervision tree — read from its files.
///
/// `edges` hold indices into `nodes`, and they arrive from another process:
/// the receiver checks them ([`ProjectMap::is_well_formed`]) before trusting
/// them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMap {
    /// What the map is (`sql.schema`, `routes`, `erlang.supervision`, …):
    /// stable, for the client to remember which one was shown.
    pub id: String,
    pub title: String,
    /// One line on how to read it.
    pub description: String,
    pub nodes: Vec<MapNode>,
    pub edges: Vec<MapEdge>,
}

impl ProjectMap {
    /// Whether every edge names a node there is.
    pub fn is_well_formed(&self) -> bool {
        let n = self.nodes.len();
        self.edges.iter().all(|e| e.from < n && e.to < n)
    }
}

/// A thing on a map — a table, a route, a handler, a process — where it is
/// defined, and what more there is to say of it (`detail`: a table's
/// columns, a class's method resolution order, …).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapNode {
    pub label: String,
    /// What it is (`table`, `route`, `handler`, `supervisor`, …).
    pub kind: String,
    /// Its file, relative to the project root; empty for a node no file
    /// defines (a route's path, a table only ever referenced).
    pub file: Rel,
    /// 1-based line in `file`; 0 when there is none.
    pub line: usize,
    pub detail: String,
}

/// How one node relates to another: an arrow `from` → `to`, saying what it
/// is (`label`: a foreign key's column, `handles`, `supervises`, …).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapEdge {
    pub from: usize,
    pub to: usize,
    pub label: String,
}

// -- the project call graph ---------------------------------------------------

/// The name-based project call graph (reply to `ProjectCalls`), as an
/// adjacency list over project-relative paths.
///
/// This is the WIRE form of `clew_core::projectcalls::ProjectCallGraph`.
/// `callers` / `callees` hold indices into `nodes`, and they arrive from
/// another process: the receiver must validate them before trusting them,
/// which `ProjectCallGraph::from_wire` does (every accessor of the rich type
/// indexes with them — an out-of-range one used to panic the UI on the first
/// frame that drew the graph).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallGraph {
    pub nodes: Vec<CallGraphNode>,
}

/// One function or method definition in a [`CallGraph`], with the nodes that
/// call it and the nodes it calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallGraphNode {
    pub name: String,
    pub kind: String,
    /// The defining file, relative to the project root.
    pub file: Rel,
    /// 1-based definition line.
    pub line: usize,
    /// Indices into [`CallGraph::nodes`].
    pub callers: Vec<usize>,
    /// Indices into [`CallGraph::nodes`].
    pub callees: Vec<usize>,
}

// -- the type-structure index -------------------------------------------------

/// What one type implements, aggregated across the project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeStructure {
    /// Traits implemented for the type (`impl Trait for Type`), sorted.
    pub traits: Vec<String>,
    /// Inherent method names (`impl Type { fn … }`), sorted.
    pub methods: Vec<String>,
}

/// The project's Rust type/trait relations — the hover peek's "implements /
/// implementors" line. Built by `clew_core::structure::build`, where the
/// files live; a remote project's arrives in `ProjectSymbols`.
///
/// Ordered maps: the index serializes the same way every time, so identical
/// indexes are identical frames.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructureIndex {
    /// Type name → what it implements.
    pub by_type: BTreeMap<String, TypeStructure>,
    /// Trait name → the types that implement it, sorted.
    pub implementors: BTreeMap<String, Vec<String>>,
}

impl StructureIndex {
    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty() && self.implementors.is_empty()
    }

    /// A one-line structure summary for the type or trait named `name`, or
    /// `None` when it is neither. A trait wins the tie (a name is one or the
    /// other).
    pub fn summary_line(&self, name: &str) -> Option<String> {
        if let Some(impls) = self.implementors.get(name) {
            return Some(list_line("Implementors", impls));
        }
        let ts = self.by_type.get(name)?;
        let mut bits = Vec::new();
        if !ts.traits.is_empty() {
            bits.push(list_line("impl", &ts.traits));
        }
        if !ts.methods.is_empty() {
            let n = ts.methods.len();
            bits.push(format!("{n} method{}", if n == 1 { "" } else { "s" }));
        }
        (!bits.is_empty()).then(|| bits.join("; "))
    }
}

/// `"impl A, B, C (+2)"` — at most 8 names, then a `(+n)` overflow.
fn list_line(label: &str, names: &[String]) -> String {
    const MAX: usize = 8;
    let shown: Vec<&str> = names.iter().take(MAX).map(String::as_str).collect();
    let more = names.len().saturating_sub(shown.len());
    let mut s = format!("{label} {}", shown.join(", "));
    if more > 0 {
        s.push_str(&format!(" (+{more})"));
    }
    s
}

// -- search -------------------------------------------------------------------

/// A file a search could not (fully) read, and why: over the size cap, a line
/// too long to buffer, not a plain file, an I/O error. Reported rather than
/// left out silently — a search that covers less than it claims is how a real
/// hit goes missing unnoticed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    pub rel: Rel,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_git_result_answers_only_its_own_operation() {
        let history = GitResult::FileHistory(Vec::new());
        assert!(history.answers(&GitOp::FileHistory {
            rel: "a.rs".into(),
            limit: 1,
        }));
        assert!(!history.answers(&GitOp::SymbolHistory {
            rel: "a.rs".into(),
            start: 1,
            end: 2,
            limit: 1,
        }));
        assert!(GitResult::ReviewBase(None).answers(&GitOp::ReviewBase));
        assert!(!GitResult::FileAt(None).answers(&GitOp::CommitMessage { sha: "ab".into() }));
    }

    #[test]
    fn structure_summaries_name_traits_and_types() {
        let mut idx = StructureIndex::default();
        idx.by_type.insert(
            "Point".into(),
            TypeStructure {
                traits: vec!["Clone".into(), "Debug".into()],
                methods: vec!["norm".into()],
            },
        );
        idx.implementors
            .insert("Shape".into(), (0..10).map(|i| format!("T{i}")).collect());
        assert_eq!(
            idx.summary_line("Point").as_deref(),
            Some("impl Clone, Debug; 1 method")
        );
        assert_eq!(
            idx.summary_line("Shape").as_deref(),
            Some("Implementors T0, T1, T2, T3, T4, T5, T6, T7 (+2)")
        );
        assert_eq!(idx.summary_line("Nothing"), None);
        assert!(!idx.is_empty());
        assert!(StructureIndex::default().is_empty());
    }
}
