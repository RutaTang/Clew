//! The History view: the project's commits, the way an IDE's git log shows
//! them — newest first, a page at a time, each with the lane it runs in and
//! the names that point at it; a commit opened to its message and the files
//! it changed, each file read at that commit in Time Travel; filtered by its
//! message, its author or its id, by one file or folder, by this branch or
//! every one. Read-only, as Clew is: it reads history, it writes none.

use crate::app::prelude::*;
use crate::*;
use clew_protocol::{CommitDetail, GitOp, LogCommit, LogPage};

/// How many commits a page brings.
pub(crate) const PAGE: usize = 100;

/// How long the filter rests before it is read.
const QUERY_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// The History view's state, the open project's.
#[derive(Debug, Default)]
pub struct HistoryView {
    /// Every branch's commits (and remote branches' and tags'), not only
    /// those HEAD reaches.
    pub all: bool,
    /// The file or folder (root-relative) the commits touched; `None` for
    /// the whole project.
    pub path: Option<String>,
    /// The filter as typed: words of the message, `@author`, a commit id.
    pub query: String,
    /// The commits shown, newest first, and the lanes they run in.
    pub commits: Vec<LogCommit>,
    pub graph: Vec<GraphRow>,
    /// Whether more commits come after them.
    pub more: bool,
    /// A page on its way.
    pub loading: bool,
    /// Why the last page could not be read.
    pub error: Option<String>,
    /// Numbers the reads: an answer to an earlier filter is not this one's.
    pub generation: u64,
    /// A first page was asked for, for what the view shows now.
    pub asked: bool,
    /// The commit opened to its message and files, by its id.
    pub open: Option<String>,
    /// Opened commits read whole, by id.
    pub details: HashMap<String, Result<Option<CommitDetail>, String>>,
    /// The commit the filter names by its id, when it is shaped like one.
    pub by_id: Option<(String, Result<Option<CommitDetail>, String>)>,
    /// The uncommitted changes' row opened to its files.
    pub changes_open: bool,
    /// Numbers the filter's keys: it is read once one has settled.
    pub typed: u64,
}

/// What the filter asks for: whose commits (`@ada`), what their message
/// says (the other words), and — a word shaped like a commit id — which
/// commit.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LogFilter {
    pub author: Option<String>,
    pub grep: Option<String>,
    pub id: Option<String>,
}

/// The filter `query` asks for. Words after `@` name the author; the
/// others are looked for in the message. A query of one word of 7 to 40
/// hex digits names a commit too.
pub(crate) fn log_filter(query: &str) -> LogFilter {
    let mut authors = Vec::new();
    let mut words = Vec::new();
    for word in query.split_whitespace() {
        match word.strip_prefix('@') {
            Some(author) if !author.is_empty() => authors.push(author),
            Some(_) => {}
            None => words.push(word),
        }
    }
    let join = |w: Vec<&str>| (!w.is_empty()).then(|| w.join(" "));
    let id = match words.as_slice() {
        // As short as git takes one (4 digits), as long as a SHA-256 id.
        [one] if (4..=64).contains(&one.len()) && one.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Some(one.to_ascii_lowercase())
        }
        _ => None,
    };
    LogFilter {
        author: join(authors),
        grep: join(words),
        id,
    }
}

/// One commit's row of the graph: the column it is drawn in, and the lines
/// through the row — each a column and the lane's colour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphRow {
    /// The commit's column, and its lane's colour.
    pub col: usize,
    pub color: usize,
    /// Lanes passing by, top to bottom.
    pub through: Vec<(usize, usize)>,
    /// Lanes coming down into the commit, from their column at the top:
    /// its own lane, and the branches that started from it.
    pub into: Vec<(usize, usize)>,
    /// Lines from the commit down to its parents' lanes, at the bottom.
    pub out: Vec<(usize, usize)>,
    /// The lanes running on below the row (what an opened commit's detail
    /// carries down beside it).
    pub below: Vec<(usize, usize)>,
    /// Drawn dashed: commits a filter left out lie between this one and the
    /// next.
    pub dashed: bool,
}

impl GraphRow {
    /// How many columns the row draws in.
    pub fn width(&self) -> usize {
        std::iter::once(self.col)
            .chain(self.through.iter().map(|&(c, _)| c))
            .chain(self.into.iter().map(|&(c, _)| c))
            .chain(self.out.iter().map(|&(c, _)| c))
            .max()
            .map_or(0, |c| c + 1)
    }
}

/// The rows of a filtered list (words, an author): the commits that match
/// are not each other's parents, so no branches are drawn between them —
/// one lane, dashed, each dot a match with others between.
pub(crate) fn sparse_rows(n: usize, more: bool) -> Vec<GraphRow> {
    (0..n)
        .map(|i| {
            let on = i + 1 < n || more;
            GraphRow {
                col: 0,
                color: 0,
                through: Vec::new(),
                into: if i > 0 { vec![(0, 0)] } else { Vec::new() },
                out: if on { vec![(0, 0)] } else { Vec::new() },
                below: if on { vec![(0, 0)] } else { Vec::new() },
                dashed: true,
            }
        })
        .collect()
}

/// The lanes of `commits` (newest first, each after its children): each
/// commit in the leftmost lane that expects it — its child's — or a new
/// one; its first parent carried on in its lane, its others in lanes of
/// their own, or into the lane already expecting them. Lanes keep their
/// columns: one that ends leaves a gap the next new lane takes.
pub(crate) fn graph_rows(commits: &[LogCommit]) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<(String, usize)>> = Vec::new();
    let mut colors = 0usize;
    let mut new_color = || {
        colors += 1;
        colors - 1
    };
    let mut rows = Vec::with_capacity(commits.len());
    for c in commits {
        let hits: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_ref().is_some_and(|(sha, _)| *sha == c.sha))
            .map(|(i, _)| i)
            .collect();
        let (col, color) = match hits.first() {
            Some(&h) => (h, lanes[h].as_ref().map_or(0, |l| l.1)),
            None => {
                let col = lanes
                    .iter()
                    .position(Option::is_none)
                    .unwrap_or(lanes.len());
                (col, new_color())
            }
        };
        if col == lanes.len() {
            lanes.push(None);
        }
        let into: Vec<(usize, usize)> = hits
            .iter()
            .map(|&h| (h, lanes[h].as_ref().map_or(color, |l| l.1)))
            .collect();
        for &h in &hits {
            lanes[h] = None;
        }
        let through: Vec<(usize, usize)> = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.as_ref().map(|(_, color)| (i, *color)))
            .collect();
        let mut out = Vec::new();
        let expecting = |lanes: &[Option<(String, usize)>], sha: &str| {
            lanes
                .iter()
                .position(|l| l.as_ref().is_some_and(|(s, _)| s == sha))
        };
        for (n, parent) in c.parents.iter().enumerate() {
            // The first parent carries on down the commit's own lane — even
            // one another lane expects too: both come into it where it is
            // drawn, in the leftmost of them, as `git log --graph` keeps a
            // branch's line to its left.
            if n == 0 {
                lanes[col] = Some((parent.clone(), color));
                out.push((col, color));
            } else if let Some(at) = expecting(&lanes, parent) {
                let color = lanes[at].as_ref().map_or(color, |l| l.1);
                out.push((at, color));
            } else {
                let at = lanes
                    .iter()
                    .position(Option::is_none)
                    .unwrap_or(lanes.len());
                if at == lanes.len() {
                    lanes.push(None);
                }
                let color = new_color();
                lanes[at] = Some((parent.clone(), color));
                out.push((at, color));
            }
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        let below = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.as_ref().map(|(_, color)| (i, *color)))
            .collect();
        rows.push(GraphRow {
            col,
            color,
            through,
            into,
            out,
            below,
            dashed: false,
        });
    }
    rows
}

impl App {
    /// Handle a [`HistoryMsg`].
    pub(crate) fn update_history(&mut self, message: HistoryMsg) -> Task<Message> {
        match message {
            HistoryMsg::Show { path } => {
                let h = &mut self.proj.git_log;
                if h.path != path {
                    h.path = path;
                    h.asked = false;
                }
                self.sidebar = SidebarTab::History;
                self.show_left_sidebar = true;
                self.proj.git_status_stale = true;
                Task::batch([
                    self.sync_history(),
                    self.sync_git_status().unwrap_or_else(Task::none),
                ])
            }
            HistoryMsg::Query(query) => {
                let h = &mut self.proj.git_log;
                if h.query == query {
                    return Task::none();
                }
                h.query = query;
                h.typed += 1;
                let typed = h.typed;
                // Read once the typing settles, not for each key.
                Task::perform(
                    async { tokio::time::sleep(QUERY_SETTLE).await },
                    move |()| Message::History(HistoryMsg::QuerySettled(typed)),
                )
            }
            HistoryMsg::QuerySettled(typed) => {
                let h = &mut self.proj.git_log;
                if typed != h.typed {
                    return Task::none();
                }
                h.asked = false;
                self.sync_history()
            }
            HistoryMsg::AllBranches(all) => {
                let h = &mut self.proj.git_log;
                if h.all == all {
                    return Task::none();
                }
                h.all = all;
                h.asked = false;
                self.sync_history()
            }
            HistoryMsg::WholeProject => {
                let h = &mut self.proj.git_log;
                if h.path.take().is_some() {
                    h.asked = false;
                }
                self.sync_history()
            }
            HistoryMsg::More => self.load_history_page(false),
            HistoryMsg::Scrolled { near_end } => {
                let h = &self.proj.git_log;
                if near_end && h.more && !h.loading && h.error.is_none() {
                    return self.load_history_page(false);
                }
                Task::none()
            }
            HistoryMsg::Loaded {
                generation,
                skip,
                result,
                ..
            } => {
                // One file's commits are followed across its renames, and
                // so are not each other's parents: drawn apart.
                let followed = self.history_follows_a_file();
                let h = &mut self.proj.git_log;
                if generation != h.generation {
                    return Task::none();
                }
                h.loading = false;
                match result {
                    Ok(page) => {
                        // A page read again from the top (the history moved
                        // on) replaces what was shown; a later page adds on.
                        if skip == 0 {
                            h.commits = page.commits;
                        } else if skip == h.commits.len() {
                            h.commits.extend(page.commits);
                        }
                        h.more = page.more;
                        h.error = None;
                        let filter = log_filter(&h.query);
                        h.graph = if followed || filter.author.is_some() || filter.grep.is_some() {
                            sparse_rows(h.commits.len(), h.more)
                        } else {
                            graph_rows(&h.commits)
                        };
                    }
                    Err(e) => {
                        // The first page of what is asked now failed: what
                        // the last filter showed is not this one's — it
                        // stayed, under the new chip, as if it matched.
                        if skip == 0 {
                            h.commits.clear();
                            h.graph.clear();
                            h.more = false;
                            h.open = None;
                        }
                        h.error = Some(e);
                    }
                }
                Task::none()
            }
            HistoryMsg::Toggle(sha) => {
                let h = &mut self.proj.git_log;
                if h.open.as_deref() == Some(sha.as_str()) {
                    h.open = None;
                    return Task::none();
                }
                h.open = Some(sha.clone());
                // Read once, and again after a read that failed.
                if matches!(h.details.get(&sha), Some(Ok(_))) {
                    return Task::none();
                }
                h.details.remove(&sha);
                self.read_commit(sha)
            }
            HistoryMsg::DetailRead { sha, result, .. } => {
                let h = &mut self.proj.git_log;
                if let Some((id, found)) = h.by_id.as_mut()
                    && *id == sha
                {
                    *found = result.clone();
                }
                h.details.insert(sha, result);
                Task::none()
            }
            HistoryMsg::ToggleChanges => {
                let h = &mut self.proj.git_log;
                h.changes_open = !h.changes_open;
                if h.changes_open {
                    self.proj.git_status_stale = true;
                    return self.sync_git_status().unwrap_or_else(Task::none);
                }
                Task::none()
            }
            HistoryMsg::OpenAt { sha, rel } => self.open_file_at_commit(sha, rel),
            HistoryMsg::TipRead { result, .. } => {
                self.proj.repo_tip_loading = false;
                let Ok(tip) = result else {
                    return Task::none();
                };
                // Empty with no commit yet: the first commit moves it too.
                let tip = tip.unwrap_or_default();
                let moved = self
                    .proj
                    .repo_tip
                    .as_ref()
                    .is_some_and(|before| *before != tip);
                self.proj.repo_tip = Some(tip);
                if moved {
                    self.repo_moved()
                } else {
                    Task::none()
                }
            }
            HistoryMsg::OpenChanged(rel) => {
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                let abs = root.join(&rel);
                // Already the file being read: its Diff now — no load is
                // coming to show it after.
                if self.active_viewer().is_some_and(|v| v.abs == abs) {
                    return self.show_diff_of(&abs);
                }
                let open = self.open_file(abs.clone(), None, true);
                self.proj.diff_after_open = Some(abs);
                open
            }
            HistoryMsg::Refresh => {
                self.proj.git_status_stale = true;
                // A commit's detail never changes: only what failed to read
                // is read again. All of it cleared, the commit open closed
                // (it is drawn once read) and took two clicks to see again.
                self.proj.git_log.details.retain(|_, d| d.is_ok());
                Task::batch([
                    self.refresh_history(),
                    self.sync_git_status().unwrap_or_else(Task::none),
                ])
            }
            HistoryMsg::CopyId(sha) => {
                self.status = tr!("Copied the commit id").into();
                iced::clipboard::write(sha)
            }
        }
    }

    /// The project's newest commit read anew when it may have moved
    /// (`repo_tip_stale`): one read at a time. `None` when there is nothing
    /// to read.
    pub(crate) fn sync_repo_tip(&mut self) -> Option<Task<Message>> {
        if !self.proj.repo_tip_stale || self.proj.repo_tip_loading {
            return None;
        }
        let git = self.git_source()?;
        self.proj.repo_tip_stale = false;
        self.proj.repo_tip_loading = true;
        let stamp = self.stamp();
        Some(Task::perform(
            async move {
                git.run::<LogPage>(GitOp::Log {
                    all: false,
                    rel: None,
                    author: None,
                    grep: None,
                    skip: 0,
                    limit: 1,
                })
                .await
                .map(|page| page.commits.into_iter().next().map(|c| c.sha))
            },
            move |result| Message::History(HistoryMsg::TipRead { stamp, result }),
        ))
    }

    /// The project's newest commit moved — a commit, a checkout, a pull
    /// made outside Clew: what was read of its history is read again. The
    /// gutter's change bars and blame, the margin's History and the History
    /// view stayed as they were until the file was opened again.
    fn repo_moved(&mut self) -> Task<Message> {
        self.proj.git_status_stale = true;
        self.proj.head_stale = true;
        self.proj.margin_history = None;
        let open = self.refresh_git_of_open_files();
        let margin = self.sync_margin_history().unwrap_or_else(Task::none);
        Task::batch([open, margin, self.refresh_history()])
    }

    /// The first page read, when the view shows and has not asked for what
    /// it shows now (a new filter, another branch or path, a new project).
    pub(crate) fn sync_history(&mut self) -> Task<Message> {
        if !self.history_unread() {
            return Task::none();
        }
        self.proj.git_log.asked = true;
        self.load_history_page(true)
    }

    /// Whether the History view shows and has not asked for what it shows.
    pub(crate) fn history_unread(&self) -> bool {
        self.sidebar == SidebarTab::History && self.show_left_sidebar && !self.proj.git_log.asked
    }

    /// The History view read again from its top — the history moved on (a
    /// commit, a checkout made elsewhere) — keeping what is opened.
    pub(crate) fn refresh_history(&mut self) -> Task<Message> {
        self.proj.git_log.asked = false;
        self.sync_history()
    }

    /// A page of commits read: from the top (`fresh`), or the next after
    /// those shown.
    fn load_history_page(&mut self, fresh: bool) -> Task<Message> {
        let Some(git) = self.git_source() else {
            return Task::none();
        };
        let h = &mut self.proj.git_log;
        if fresh {
            h.generation += 1;
            h.more = false;
            h.error = None;
        } else if h.loading || !h.more {
            return Task::none();
        }
        h.loading = true;
        let skip = if fresh { 0 } else { h.commits.len() };
        let filter = log_filter(&h.query);
        let op = GitOp::Log {
            all: h.all,
            rel: h.path.clone(),
            author: filter.author,
            grep: filter.grep,
            skip,
            limit: PAGE,
        };
        let generation = h.generation;
        // A query shaped like a commit id: that commit read too, to show
        // above what the words find.
        let by_id = match (fresh, filter.id) {
            (true, Some(id)) => {
                h.by_id = None;
                let stamp = self.stamp();
                let git = git.clone();
                self.proj.git_log.by_id = Some((id.clone(), Ok(None)));
                Task::perform(
                    async move {
                        git.run::<Option<CommitDetail>>(GitOp::CommitDetail { sha: id.clone() })
                            .await
                            .map(|d| (id, d))
                    },
                    move |result| match result {
                        Ok((sha, d)) => Message::History(HistoryMsg::DetailRead {
                            stamp: stamp.clone(),
                            sha,
                            result: Ok(d),
                        }),
                        Err(_) => Message::Noop,
                    },
                )
            }
            (true, None) => {
                self.proj.git_log.by_id = None;
                Task::none()
            }
            _ => Task::none(),
        };
        let stamp = self.stamp();
        let page = Task::perform(async move { git.run::<LogPage>(op).await }, move |result| {
            Message::History(HistoryMsg::Loaded {
                stamp: stamp.clone(),
                generation,
                skip,
                result,
            })
        });
        Task::batch([page, by_id])
    }

    /// Commit `sha` read whole, for its opened row.
    fn read_commit(&mut self, sha: String) -> Task<Message> {
        let Some(git) = self.git_source() else {
            return Task::none();
        };
        let stamp = self.stamp();
        Task::perform(
            async move {
                let result = git
                    .run::<Option<CommitDetail>>(GitOp::CommitDetail { sha: sha.clone() })
                    .await;
                (sha, result)
            },
            move |(sha, result)| {
                Message::History(HistoryMsg::DetailRead {
                    stamp: stamp.clone(),
                    sha,
                    result,
                })
            },
        )
    }

    /// The file `rel` read as commit `sha` left it: opened, and Time Travel
    /// started at that commit once it is (`time_travel_after_open`).
    fn open_file_at_commit(&mut self, sha: String, rel: String) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let abs = root.join(&rel);
        if self.active_viewer().is_some_and(|v| v.abs == abs) {
            return Task::done(Message::TimeTravel(TimeTravelMsg::StartAt { sha }));
        }
        let open = self.open_file(abs.clone(), None, true);
        self.proj.time_travel_after_open = Some((abs, sha));
        open
    }

    /// What waits for the file `abs` to open (`open_file_at_commit`, a
    /// changed file's Diff): carried out now it has. Only the next file to
    /// open answers it: another opening first (the reader went elsewhere)
    /// drops it — left waiting, it fired at the file's next opening by any
    /// way, long after.
    pub(crate) fn after_history_open(&mut self, abs: &std::path::Path) -> Task<Message> {
        let mut tasks = Vec::new();
        if let Some((file, sha)) = self.proj.time_travel_after_open.take()
            && file == abs
        {
            tasks.push(Task::done(Message::TimeTravel(TimeTravelMsg::StartAt {
                sha,
            })));
        }
        if let Some(file) = self.proj.diff_after_open.take()
            && file == abs
        {
            tasks.push(self.show_diff_of(abs));
        }
        Task::batch(tasks)
    }

    /// A file asked for from the History view could not be opened: what
    /// waited for it does not wait on.
    pub(crate) fn forget_history_open(&mut self, abs: &std::path::Path) {
        if self
            .proj
            .time_travel_after_open
            .as_ref()
            .is_some_and(|(file, _)| file == abs)
        {
            self.proj.time_travel_after_open = None;
        }
        if self.proj.diff_after_open.as_deref() == Some(abs) {
            self.proj.diff_after_open = None;
        }
    }

    /// The Diff of `abs`, the file being read: shown, not toggled — a Diff
    /// already on (or on its way) stays.
    fn show_diff_of(&mut self, abs: &std::path::Path) -> Task<Message> {
        let shown = self.proj.diff.as_ref().is_some_and(|d| d.abs == abs)
            || self
                .proj
                .diff_pending
                .as_ref()
                .is_some_and(|(_, file)| file == abs);
        if shown {
            Task::none()
        } else {
            Task::done(Message::Editor(EditorMsg::ToggleDiff))
        }
    }

    /// Whether the History view lists one file's commits (`HistoryView::
    /// path` names a file of the project): followed across renames, so
    /// drawn as a list, not a graph.
    fn history_follows_a_file(&self) -> bool {
        let (Some(path), Some(project)) = (&self.proj.git_log.path, &self.proj.project) else {
            return false;
        };
        project.files.iter().any(|f| f.rel == *path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> LogCommit {
        LogCommit {
            sha: sha.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author: "Ada".into(),
            time: 0,
            subject: sha.into(),
            refs: Vec::new(),
        }
    }

    /// The filter reads `@` words as the author, the rest as the message,
    /// and a lone word of hex digits as a commit id too.
    #[test]
    fn the_filter_reads_author_message_and_id() {
        assert_eq!(log_filter("  "), LogFilter::default());
        let f = log_filter("fix parser @ada");
        assert_eq!(f.author.as_deref(), Some("ada"));
        assert_eq!(f.grep.as_deref(), Some("fix parser"));
        assert_eq!(f.id, None);
        let f = log_filter("A1B2C3D");
        assert_eq!(f.id.as_deref(), Some("a1b2c3d"));
        assert_eq!(f.grep.as_deref(), Some("A1B2C3D"));
        // As short as git takes one: four hex digits.
        assert_eq!(log_filter("cafe").id.as_deref(), Some("cafe"));
        assert_eq!(log_filter("caf").id, None, "too short to be an id");
        assert_eq!(log_filter("@").author, None);
    }

    /// A straight history runs in one lane; a branch merged back takes a
    /// second lane from the merge down to where it started, and gives it
    /// back.
    /// A list a filter of words or author cut: its commits are not each
    /// other's parents, so no lane is opened between them — one, dashed,
    /// running on past the last only when more follow.
    #[test]
    fn a_filtered_list_is_one_dashed_lane() {
        let rows = sparse_rows(3, false);
        assert!(
            rows.iter()
                .all(|r| r.dashed && r.col == 0 && r.width() == 1)
        );
        assert!(rows[0].into.is_empty() && !rows[0].out.is_empty());
        assert!(!rows[2].into.is_empty() && rows[2].out.is_empty() && rows[2].below.is_empty());
        let more = sparse_rows(2, true);
        assert!(!more[1].out.is_empty(), "the lane runs on to the next page");
        assert!(sparse_rows(0, true).is_empty());
        // The whole graph draws solid.
        assert!(
            graph_rows(&[commit("b", &["a"]), commit("a", &[])])
                .iter()
                .all(|r| !r.dashed)
        );
    }

    #[test]
    fn a_merged_branch_takes_a_lane_and_gives_it_back() {
        // m merges b into a; b and a both come from r.
        let commits = [
            commit("m", &["a", "b"]),
            commit("b", &["r"]),
            commit("a", &["r"]),
            commit("r", &[]),
        ];
        let rows = graph_rows(&commits);
        assert_eq!(rows[0].col, 0);
        assert_eq!(
            rows[0].out,
            vec![(0, 0), (1, 1)],
            "first parent below, the other beside"
        );
        assert_eq!(rows[1].col, 1, "b in the merge's second lane");
        assert_eq!(rows[1].through, vec![(0, 0)]);
        assert_eq!(rows[1].out, vec![(1, 1)]);
        assert_eq!(rows[2].col, 0);
        // r ends both lanes: its children's.
        assert_eq!(rows[3].col, 0);
        assert_eq!(rows[3].into, vec![(0, 0), (1, 1)]);
        assert!(rows[3].below.is_empty());
        assert!(rows.iter().all(|r| r.width() <= 2));

        let straight = graph_rows(&[commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])]);
        assert!(straight.iter().all(|r| r.col == 0 && r.width() == 1));
    }
}
