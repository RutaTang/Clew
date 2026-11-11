//! The History view (`app::history`): the project's commits as an IDE's
//! git log shows them — each beside the lane it runs in, the names that
//! point at it, who made it and when — opened to its message and the files
//! it changed, each read at that commit in Time Travel; the uncommitted
//! changes above them; filtered by message, `@author` or a commit id, by
//! this branch or every one, by a file or folder.

use super::*;
use crate::app::history::{GraphRow, HistoryView};
use clew_protocol::{CommitDetail, LogCommit, RefKind};
use iced::widget::{column, row};

/// A commit's row: its subject over who and when.
const ROW_H: f32 = 44.0;
/// The space a lane takes, and the most lanes drawn (more run on unseen).
const LANE_W: f32 = 12.0;
const MAX_LANES: usize = 8;
/// The lanes' space before the first.
const LANE_PAD: f32 = 6.0;

/// The History view.
pub(crate) fn history_tab(app: &App) -> Element<'_, Message> {
    let h = &app.proj.git_log;
    if app.proj.project.is_none() {
        return empty_state(
            Glyph::Branch,
            tr!("No project open"),
            tr!("Open a project to read its history."),
            None,
        );
    }
    // The rows a voice counts down: "open the second".
    super::heard::list();
    let header = header(app, h);
    let body: Element<'_, Message> = if h.commits.is_empty() {
        let (title, said): (&str, String) = match &h.error {
            Some(e) if e.contains("not a git repository") => (
                tr!("Not a git repository"),
                tr!("This project's history is not kept in git.").into(),
            ),
            Some(e) => (tr!("Could not read the history"), e.clone()),
            None if h.loading || !h.asked => (tr!("Reading the history…"), String::new()),
            None if !h.query.trim().is_empty() || h.path.is_some() => (
                tr!("No commit matches"),
                // In the switch's own words, and not when it is already on.
                if h.all {
                    tr!("Try other words.").into()
                } else {
                    tr!("Try other words, or All branches.").into()
                },
            ),
            None => (
                tr!("No commits yet"),
                tr!("Commits show here once there are some.").into(),
            ),
        };
        // A commit the filter names by its id, found: that is the match —
        // "No commit matches" under it contradicted it.
        let found_by_id = matches!(&h.by_id, Some((_, Ok(Some(_)))));
        let note: Element<'_, Message> = if found_by_id && h.error.is_none() {
            space().height(0).into()
        } else {
            container(
                column![
                    text(title).size(ts::BODY).color(theme::fg_muted()),
                    text(said)
                        .size(ts::SMALL)
                        .color(theme::dim())
                        .wrapping(Wrapping::WordOrGlyph),
                ]
                .spacing(4),
            )
            .padding([12, 12])
            .into()
        };
        let mut col = column![changes_rows(app, h), by_id_row(app, h), note];
        col = col.spacing(2);
        col.into()
    } else {
        commit_list(app, h)
    };
    column![header, body].spacing(8).height(Fill).into()
}

/// The filter, the branches it reads, and the path it is about.
fn header<'a>(app: &'a App, h: &'a HistoryView) -> Element<'a, Message> {
    // The filter every sidebar view has: its mark, its ✕, and Esc that
    // clears it — a plain field let Esc go on to close Time Travel.
    let field = super::sidebar::filter_field(
        tr!("Message, @author or commit id"),
        &h.query,
        None,
        |q| Message::History(HistoryMsg::Query(q)),
        None,
        Message::History(HistoryMsg::Query(String::new())),
        None,
    );
    let scope = super::form::segmented_across([
        (
            tr!("This branch").to_string(),
            !h.all,
            Message::History(HistoryMsg::AllBranches(false)),
        ),
        (
            tr!("All branches").to_string(),
            h.all,
            Message::History(HistoryMsg::AllBranches(true)),
        ),
    ]);
    // Read again, by hand: what changed elsewhere and moved no HEAD.
    let refresh = super::toolbar::busy_action(
        Glyph::Refresh,
        tr!("Refresh"),
        Some(Message::History(HistoryMsg::Refresh)),
        h.loading,
    );
    let field = row![container(field).width(Fill), refresh]
        .spacing(4)
        .align_y(iced::Center);
    let mut head = column![field, scope].spacing(6);
    // The path the log is narrowed to, under the switch: a chip that says
    // so and lets the whole project back.
    if let Some(path) = &h.path {
        head = head.push(
            container(
                row![
                    text(tr!("In {path}", path = path))
                        .size(ts::SMALL)
                        .color(theme::fg_muted())
                        .wrapping(Wrapping::WordOrGlyph)
                        .width(Fill),
                    button(glyph::icon(Glyph::Close, theme::dim(), 11.0))
                        .style(theme::icon_button(false))
                        .padding(2)
                        .on_press(super::heard::said(
                            tr!("Whole project"),
                            Message::History(HistoryMsg::WholeProject),
                        )),
                ]
                .spacing(4)
                .align_y(iced::Center),
            )
            .padding([2, 6])
            .style(theme::chip),
        );
    }
    let _ = app;
    head.padding(Padding {
        top: 0.0,
        right: 10.0,
        bottom: 0.0,
        left: 10.0,
    })
    .into()
}

/// The commits, a lane graph beside them, an opened one's detail under it,
/// and the next page as the list nears its end.
fn commit_list<'a>(app: &'a App, h: &'a HistoryView) -> Element<'a, Message> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let lanes = h
        .graph
        .iter()
        .map(GraphRow::width)
        .max()
        .unwrap_or(1)
        .clamp(1, MAX_LANES);
    let lanes_w = LANE_PAD * 2.0 + lanes as f32 * LANE_W;
    // What a row has for its words, right of its lanes: cut to it with an
    // ellipsis, not clipped mid-letter.
    let room = (app.sidebar_width - lanes_w - ROW_PAD_R - 2.0).max(0.0);
    let mut rows: Vec<Element<'a, Message>> = vec![changes_rows(app, h), by_id_row(app, h)];
    for (c, g) in h.commits.iter().zip(&h.graph) {
        let open = h.open.as_deref() == Some(c.sha.as_str());
        rows.push(commit_row(c, g, lanes_w, room, open, now));
        // Opened once it has been read: it opened on "Reading the commit…"
        // and then a moment later on the whole detail, everything below
        // moving twice. (A commit is read in well under a second.)
        if open && h.details.contains_key(c.sha.as_str()) {
            rows.push(
                row![
                    canvas_of(g, false, lanes_w, Length::Fill),
                    container(commit_detail(h, &c.sha, room, now))
                        .padding(Padding {
                            top: 2.0,
                            right: ROW_PAD_R,
                            bottom: 10.0,
                            left: 0.0,
                        })
                        .width(Fill),
                ]
                .into(),
            );
        }
    }
    let more = || {
        button(text(tr!("More commits")).size(ts::SMALL))
            .style(theme::toolbar_button)
            .padding([3, 10])
            .on_press(super::heard::said(
                tr!("More commits"),
                Message::History(HistoryMsg::More),
            ))
    };
    let foot: Element<'a, Message> = if h.loading {
        text(tr!("Reading more…"))
            .size(ts::SMALL)
            .color(theme::dim())
            .into()
    } else if let Some(e) = &h.error {
        // Why the next page did not come — it was under "More commits",
        // and scrolling read no further — and the button to ask again.
        let said = text(e.clone())
            .size(ts::SMALL)
            .color(theme::danger())
            .wrapping(Wrapping::WordOrGlyph);
        if h.more {
            column![said, more()].spacing(6).into()
        } else {
            said.into()
        }
    } else if h.more {
        more().into()
    } else {
        space().height(0).into()
    };
    rows.push(container(foot).padding([8, 12]).into());
    scrollable(Column::with_children(rows).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .on_scroll(|viewport| {
            Message::History(HistoryMsg::Scrolled {
                near_end: viewport.relative_offset().y > 0.9,
            })
        })
        .height(Fill)
        .into()
}

/// One commit's row: its lane, its names, its subject, who and when.
fn commit_row<'a>(
    c: &'a LogCommit,
    g: &'a GraphRow,
    lanes_w: f32,
    room: f32,
    open: bool,
    now: i64,
) -> Element<'a, Message> {
    use super::steady::{cut, width_of};
    const GAP: f32 = 4.0;
    let mut first = row![].spacing(GAP).align_y(iced::Center);
    // The names on it, while they leave the subject most of the row; the
    // rest counted in one more ("+2").
    let mut left = room;
    for (i, r) in c.refs.iter().enumerate() {
        let name = cut(&r.name, BADGE_NAME_MAX, ts::CAPTION, crate::fonts::UI);
        let w = badge_width(r.kind, &name) + GAP;
        if i > 0 && room - (left - w) > room * 0.5 {
            let more = format!("+{}", c.refs.len() - i);
            left -= width_of(&more, ts::CAPTION, crate::fonts::UI) + 10.0 + GAP;
            first = first.push(
                container(text(more).size(ts::CAPTION).color(theme::dim()))
                    .padding([1, 5])
                    .style(theme::chip),
            );
            break;
        }
        left -= w;
        first = first.push(ref_badge(r.kind, name));
    }
    first = first.push(
        text(cut(&c.subject, left.max(0.0), ts::BODY, crate::fonts::UI))
            .size(ts::BODY)
            .color(theme::fg())
            .wrapping(Wrapping::None),
    );
    let said = tr!(
        "{sha} by {author}, {ago}",
        sha = short_sha(&c.sha),
        author = c.author,
        ago = crate::i18n::ago(c.time, now)
    );
    let words = column![
        container(first).clip(true).width(Fill),
        text(cut(&said, room, ts::SMALL, crate::fonts::UI))
            .size(ts::SMALL)
            .color(theme::dim())
            .wrapping(Wrapping::None),
    ]
    .spacing(3);
    let msg = super::heard::row(
        || c.subject.clone(),
        Message::History(HistoryMsg::Toggle(c.sha.clone())),
    );
    button(
        row![
            canvas_of(g, true, lanes_w, Length::Fixed(ROW_H)),
            container(words)
                .padding(Padding {
                    top: 0.0,
                    right: ROW_PAD_R,
                    bottom: 0.0,
                    left: 0.0,
                })
                .center_y(ROW_H)
                .width(Fill),
        ]
        .height(ROW_H),
    )
    .style(theme::list_row(open))
    .padding(0)
    .width(Fill)
    .on_press(msg)
    .into()
}

/// A name pointing at a commit: the branch checked out in the accent, other
/// branches plain, remote ones and tags quieter.
fn ref_badge(kind: RefKind, name: String) -> Element<'static, Message> {
    let (color, mark) = match kind {
        RefKind::Head => (theme::accent(), None),
        RefKind::Branch => (theme::fg_muted(), None),
        RefKind::Remote => (theme::dim(), None),
        RefKind::Tag => (theme::warning(), Some(Glyph::Tag)),
    };
    let mut line = row![].spacing(3).align_y(iced::Center);
    if let Some(g) = mark {
        line = line.push(glyph::icon(g, color, 10.0));
    }
    line = line.push(
        text(name)
            .size(ts::CAPTION)
            .color(color)
            .wrapping(Wrapping::None),
    );
    container(line).padding([1, 5]).style(theme::chip).into()
}

/// How wide [`ref_badge`] draws `name`: its padding, a tag's mark.
fn badge_width(kind: RefKind, name: &str) -> f32 {
    let mark = if kind == RefKind::Tag {
        10.0 + 3.0
    } else {
        0.0
    };
    super::steady::width_of(name, ts::CAPTION, crate::fonts::UI) + 10.0 + mark
}

/// Widest a branch or tag's name reads on a commit's row before it is cut.
const BADGE_NAME_MAX: f32 = 110.0;
/// Room right of a commit's words, to the panel's edge.
const ROW_PAD_R: f32 = 10.0;

/// An opened commit: its message's body, its parents, the files it
/// changed — each read at this commit in Time Travel.
fn commit_detail<'a>(
    h: &'a HistoryView,
    sha: &'a str,
    room: f32,
    now: i64,
) -> Element<'a, Message> {
    let note = |s: String| -> Element<'a, Message> {
        text(s)
            .size(ts::SMALL)
            .color(theme::dim())
            .wrapping(Wrapping::WordOrGlyph)
            .into()
    };
    let d = match h.details.get(sha) {
        None => return note(tr!("Reading the commit…").into()),
        Some(Err(e)) => return note(e.clone()),
        Some(Ok(None)) => return note(tr!("This commit is not in the repository.").into()),
        Some(Ok(Some(d))) => d,
    };
    detail_of(d, room, now)
}

fn detail_of(d: &CommitDetail, room: f32, now: i64) -> Element<'_, Message> {
    let mut col = column![].spacing(6);
    // The whole message: its subject — the row cuts it to one line, and an
    // opened commit never said it whole — and its body, its paragraphs
    // joined again where git broke them at 72 columns.
    let (subject, body) = d
        .message
        .split_once('\n')
        .map_or((d.message.as_str(), ""), |(s, rest)| (s, rest.trim()));
    col = col.push(
        text(subject.trim().to_string())
            .size(ts::BODY)
            .color(theme::fg())
            .wrapping(Wrapping::WordOrGlyph),
    );
    if !body.is_empty() {
        col = col.push(
            text(reflow(body))
                .size(ts::SMALL)
                .color(theme::fg_muted())
                .wrapping(Wrapping::WordOrGlyph),
        );
    }
    // Its day, what it merged, and its whole id to copy: the row says the
    // short one, and the whole one wrapped over two lines read as noise.
    let mut when = super::panes::short_date(d.time, now);
    if d.parents.len() > 1 {
        let parents: Vec<String> = d.parents.iter().map(|p| short_sha(p)).collect();
        when = tr!(
            "{date}, a merge of {parents}",
            date = when,
            parents = parents.join(", ")
        );
    }
    let copy_said = tr!("Copy id");
    col = col.push(
        row![
            text(when)
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(Wrapping::WordOrGlyph)
                .width(Fill),
            button(text(copy_said).size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(super::heard::said(
                    copy_said,
                    Message::History(HistoryMsg::CopyId(d.sha.clone())),
                )),
        ]
        .spacing(6)
        .align_y(iced::Center),
    );
    let changed = if d.more_files > 0 {
        tr!(
            "{files}, and {more} more",
            files = trn!("1 file changed", "{n} files changed", d.files.len()),
            more = d.more_files
        )
    } else {
        trn!("1 file changed", "{n} files changed", d.files.len())
    };
    col = col.push(text(changed).size(ts::SMALL).color(theme::fg_muted()));
    let mut files = Column::new().spacing(0);
    for f in &d.files {
        files = files.push(file_row(&d.sha, f, room));
    }
    col.push(files).into()
}

/// A commit message's body as prose: the lines of each paragraph joined
/// (git wraps them at 72 columns, and the panel wraps them again), a list's
/// items and indented lines kept as written.
fn reflow(body: &str) -> String {
    body.split("\n\n")
        .map(|paragraph| {
            let lines: Vec<&str> = paragraph.lines().collect();
            let kept = lines.iter().any(|line| {
                let t = line.trim_start();
                line.starts_with([' ', '\t'])
                    || t.starts_with(['-', '*', '+', '>', '|'])
                    || t.split_once(['.', ')']).is_some_and(|(n, _)| {
                        !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit())
                    })
            });
            if kept {
                paragraph.to_string()
            } else {
                lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A file a commit changed: its status, its path (folder quieter), the
/// lines it gained and lost. Pressed, it is read as the commit left it.
fn file_row<'a>(sha: &str, f: &'a clew_protocol::CommitFile, room: f32) -> Element<'a, Message> {
    use super::steady::{cut, width_of};
    let (letter_color, deleted) = match f.status {
        'A' => (theme::success(), false),
        'D' => (theme::danger(), true),
        'R' | 'C' => (theme::info(), false),
        _ => (theme::accent(), false),
    };
    let stats: Vec<(String, iced::Color)> = match (f.added, f.removed) {
        (Some(a), Some(r)) => vec![
            (format!("+{a}"), theme::success()),
            (format!("\u{2212}{r}"), theme::danger()),
        ],
        _ => vec![(tr!("binary").to_string(), theme::dim())],
    };
    // The name, then its folder, in what the letter and the counts leave:
    // each cut with an ellipsis, the name first to keep.
    let stats_w: f32 = stats
        .iter()
        .map(|(s, _)| width_of(s, ts::CAPTION, crate::fonts::CODE) + 4.0)
        .sum();
    let names_room = (room - 8.0 - 12.0 - 4.0 - stats_w).max(0.0);
    let (dir, name) = match f.rel.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name.to_string()),
        None => (String::new(), f.rel.clone()),
    };
    let name = cut(&name, names_room, ts::SMALL, crate::fonts::UI);
    let dir_room = names_room - width_of(&name, ts::SMALL, crate::fonts::UI) - 6.0;
    let dir = if dir_room < 24.0 {
        String::new()
    } else {
        cut(&dir, dir_room, ts::CAPTION, crate::fonts::UI)
    };
    let mut line = row![
        text(f.status.to_string())
            .size(ts::SMALL)
            .font(crate::fonts::CODE)
            .color(letter_color)
            .width(12),
        container(
            row![
                text(name)
                    .size(ts::SMALL)
                    .color(if deleted { theme::dim() } else { theme::fg() })
                    .wrapping(Wrapping::None),
                text(dir)
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            ]
            .spacing(6)
            .align_y(iced::Center)
        )
        .clip(true)
        .width(Fill),
    ]
    .spacing(4)
    .align_y(iced::Center);
    for (s, color) in stats {
        line = line.push(
            text(s)
                .size(ts::CAPTION)
                .font(crate::fonts::CODE)
                .color(color)
                .wrapping(Wrapping::None),
        );
    }
    let press = (!deleted).then(|| {
        super::heard::said(
            &f.rel,
            Message::History(HistoryMsg::OpenAt {
                sha: sha.to_string(),
                rel: f.rel.clone(),
            }),
        )
    });
    button(line)
        .style(theme::menu_row)
        .padding([3, 4])
        .width(Fill)
        .on_press_maybe(press)
        .into()
}

/// The working tree's changes, above the commits: a row that opens to the
/// files changed, each opened in its Diff.
fn changes_rows<'a>(app: &'a App, h: &'a HistoryView) -> Element<'a, Message> {
    let mut changed: Vec<(&String, &char)> = app
        .proj
        .git_status
        .iter()
        .filter(|(rel, _)| {
            h.path
                .as_ref()
                .is_none_or(|p| rel.as_str() == p || rel.starts_with(&format!("{p}/")))
        })
        .collect();
    if changed.is_empty() || !h.query.trim().is_empty() {
        return space().height(0).into();
    }
    // What git tracks first, then the files it does not: one real change
    // came last, under nine untracked build files.
    changed.sort_by_key(|(rel, letter)| (**letter == 'U', rel.as_str()));
    let room = (app.sidebar_width - 34.0 - 12.0 - 12.0 - 4.0 - 2.0).max(0.0);
    let head = button(
        row![
            glyph::icon(Glyph::Clock, theme::accent(), 13.0),
            text(tr!("Uncommitted changes"))
                .size(ts::BODY)
                .color(theme::fg()),
            space().width(Fill),
            text(trn!("1 file", "{n} files", changed.len()))
                .size(ts::SMALL)
                .color(theme::dim()),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .style(theme::list_row(h.changes_open))
    .padding([8, 12])
    .width(Fill)
    .on_press(super::heard::said(
        tr!("Uncommitted changes"),
        Message::History(HistoryMsg::ToggleChanges),
    ));
    if !h.changes_open {
        return head.into();
    }
    let mut col = column![head].spacing(0);
    for (rel, letter) in changed {
        let color = match letter {
            'A' | 'U' => theme::success(),
            'D' => theme::danger(),
            _ => theme::accent(),
        };
        let gone = *letter == 'D';
        // A compiled or packed file (`__pycache__/*.pyc`, an image) has no
        // text to show: pressed, it closed what was open for "binary file"
        // and became a stop on the trail. Quiet and inert, it says why.
        let binary = binary_name(rel);
        // Its name, then its folder quieter — as a commit's files read —
        // each cut with an ellipsis.
        let (dir, name) = match rel.rsplit_once('/') {
            Some((dir, name)) => (format!("{dir}/"), name),
            None => (String::new(), rel.as_str()),
        };
        let name = super::steady::cut(name, room, ts::SMALL, crate::fonts::UI);
        let dir_room = room - super::steady::width_of(&name, ts::SMALL, crate::fonts::UI) - 6.0;
        let dir = if dir_room < 24.0 {
            String::new()
        } else {
            super::steady::cut(&dir, dir_room, ts::CAPTION, crate::fonts::UI)
        };
        let row = button(
            row![
                text(letter.to_string())
                    .size(ts::SMALL)
                    .font(crate::fonts::CODE)
                    .color(if binary { theme::dim() } else { color })
                    .width(12),
                text(name)
                    .size(ts::SMALL)
                    .color(if gone || binary {
                        theme::dim()
                    } else {
                        theme::fg()
                    })
                    .wrapping(Wrapping::None),
                text(dir)
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            ]
            .spacing(4)
            .align_y(iced::Center),
        )
        .style(theme::menu_row)
        .padding(Padding {
            top: 3.0,
            right: 12.0,
            bottom: 3.0,
            left: 34.0,
        })
        .width(Fill)
        .on_press_maybe((!gone && !binary).then(|| {
            super::heard::said(rel, Message::History(HistoryMsg::OpenChanged(rel.clone())))
        }));
        col = col.push(if binary {
            let bubble = container(
                text(tr!("A binary file: Clew shows text files only."))
                    .size(ts::SMALL)
                    .color(theme::fg()),
            )
            .padding([4, 8])
            .max_width(super::TIP_MAX_W)
            .style(theme::modal_panel);
            Element::from(super::tip::tip(
                row,
                bubble,
                iced::widget::tooltip::Position::Bottom,
            ))
        } else {
            row.into()
        });
    }
    col.into()
}

/// Whether `rel` names a file with no text to read — compiled, packed or
/// media — by its extension: the uncommitted list shows these without
/// reading them.
pub(crate) fn binary_name(rel: &str) -> bool {
    let Some((_, ext)) = rel.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "pyc"
            | "pyo"
            | "pyd"
            | "so"
            | "dylib"
            | "dll"
            | "exe"
            | "o"
            | "a"
            | "obj"
            | "lib"
            | "class"
            | "jar"
            | "war"
            | "wasm"
            | "zip"
            | "gz"
            | "tgz"
            | "bz2"
            | "xz"
            | "zst"
            | "7z"
            | "rar"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "ico"
            | "tiff"
            | "pdf"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "mp3"
            | "mp4"
            | "wav"
            | "ogg"
            | "mov"
            | "db"
            | "sqlite"
            | "bin"
            | "onnx"
            | "pt"
            | "npy"
            | "pkl"
    )
}

/// The commit the filter names by its id, found: above the list, to open.
fn by_id_row<'a>(app: &'a App, h: &'a HistoryView) -> Element<'a, Message> {
    let Some((_, Ok(Some(d)))) = &h.by_id else {
        return space().height(0).into();
    };
    // Already in the list: it is there.
    if h.commits.iter().any(|c| c.sha == d.sha) {
        return space().height(0).into();
    }
    use super::steady::cut;
    let subject = d.message.lines().next().unwrap_or("").to_string();
    let open = h.open.as_deref() == Some(d.sha.as_str());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| t.as_secs() as i64);
    // Its words in the row less its padding, cut with an ellipsis.
    let room = (app.sidebar_width - 24.0 - 2.0).max(0.0);
    let said = tr!(
        "{sha} by {author}, {ago}",
        sha = short_sha(&d.sha),
        author = d.author,
        ago = crate::i18n::ago(d.time, now)
    );
    let head = button(
        column![
            text(cut(&subject, room, ts::BODY, crate::fonts::UI))
                .size(ts::BODY)
                .color(theme::fg())
                .wrapping(Wrapping::None),
            text(cut(&said, room, ts::SMALL, crate::fonts::UI))
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(Wrapping::None),
        ]
        .spacing(3),
    )
    .style(theme::list_row(open))
    .padding([6, 12])
    .width(Fill)
    .on_press(super::heard::row(
        move || subject,
        Message::History(HistoryMsg::Toggle(d.sha.clone())),
    ));
    if !open {
        return head.into();
    }
    column![
        head,
        container(detail_of(d, room, now)).padding(Padding {
            top: 2.0,
            right: 12.0,
            bottom: 10.0,
            left: 12.0,
        })
    ]
    .into()
}

/// The lanes beside a commit (`commit`), or carried down beside its opened
/// detail.
fn canvas_of<'a>(g: &GraphRow, commit: bool, width: f32, height: Length) -> Element<'a, Message> {
    iced::widget::canvas(Lanes {
        row: g.clone(),
        commit,
    })
    .width(width)
    .height(height)
    .into()
}

/// A lane's colour.
fn lane_color(i: usize) -> iced::Color {
    const MORE: [iced::Color; 2] = [
        iced::Color::from_rgb(0.72, 0.48, 0.92),
        iced::Color::from_rgb(0.28, 0.74, 0.74),
    ];
    match i % 6 {
        0 => theme::accent(),
        1 => theme::success(),
        2 => theme::warning(),
        3 => theme::info(),
        n => MORE[n - 4],
    }
}

struct Lanes {
    row: GraphRow,
    commit: bool,
}

impl iced::widget::canvas::Program<Message> for Lanes {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: iced::Rectangle,
        _cursor: iced::advanced::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry> {
        use iced::widget::canvas::{Frame, LineCap, LineDash, Path, Stroke};
        let mut frame = Frame::new(renderer, bounds.size());
        let x = |c: usize| LANE_PAD + LANE_W / 2.0 + c as f32 * LANE_W;
        let shown = |c: usize| c < MAX_LANES;
        let h = bounds.height;
        let dashed = self.row.dashed;
        let pen = |color: iced::Color| {
            let pen = Stroke::default()
                .with_width(1.6)
                .with_color(color)
                .with_line_cap(LineCap::Round);
            // Commits a filter left out lie between: the line says so.
            if dashed {
                Stroke {
                    line_dash: LineDash {
                        segments: &[2.0, 4.0],
                        offset: 0,
                    },
                    ..pen.with_line_cap(LineCap::Butt)
                }
            } else {
                pen
            }
        };
        let p = iced::Point::new;
        if !self.commit {
            for &(c, color) in &self.row.below {
                if shown(c) {
                    frame.stroke(
                        &Path::line(p(x(c), 0.0), p(x(c), h)),
                        pen(lane_color(color)),
                    );
                }
            }
            return vec![frame.into_geometry()];
        }
        let mid = h / 2.0;
        let col = self.row.col.min(MAX_LANES - 1);
        // A line between two columns bends halfway: down, across, down.
        let bend = |from: (f32, f32), to: (f32, f32)| {
            Path::new(|b| {
                b.move_to(p(from.0, from.1));
                let half = (from.1 + to.1) / 2.0;
                b.bezier_curve_to(p(from.0, half), p(to.0, half), p(to.0, to.1));
            })
        };
        for &(c, color) in &self.row.through {
            if shown(c) {
                frame.stroke(
                    &Path::line(p(x(c), 0.0), p(x(c), h)),
                    pen(lane_color(color)),
                );
            }
        }
        for &(c, color) in &self.row.into {
            let c = c.min(MAX_LANES - 1);
            frame.stroke(&bend((x(c), 0.0), (x(col), mid)), pen(lane_color(color)));
        }
        for &(c, color) in &self.row.out {
            let c = c.min(MAX_LANES - 1);
            frame.stroke(&bend((x(col), mid), (x(c), h)), pen(lane_color(color)));
        }
        let dot = Path::circle(p(x(col), mid), 4.0);
        frame.fill(&dot, lane_color(self.row.color));
        frame.stroke(
            &dot,
            Stroke::default().with_width(1.5).with_color(theme::bg()),
        );
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::reflow;

    /// A body's paragraphs read as prose, git's 72-column breaks joined; a
    /// list keeps its lines.
    #[test]
    fn a_commit_body_is_reflowed_but_lists_are_kept() {
        let body = "Tax the discounted amount, not the\nsubtotal before it.\n\n- one\n- two";
        assert_eq!(
            reflow(body),
            "Tax the discounted amount, not the subtotal before it.\n\n- one\n- two"
        );
        assert_eq!(reflow("1. first\n2. second"), "1. first\n2. second");
    }
}
