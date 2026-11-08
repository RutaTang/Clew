//! Code/markdown panes, time-travel & diff views, outline, find bar.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

pub(crate) fn editor_shell(inner: Element<'_, Message>) -> Element<'_, Message> {
    container(inner)
        .width(Fill)
        .height(Fill)
        .style(theme::on_sheet)
        .into()
}

/// One code pane.
///
/// Its skeleton is fixed — `column![header slot, stack![content, find slot]]`
/// — whatever is showing, so opening the find bar or splitting the view never
/// moves the content to another position in the widget tree. (Both used to:
/// the find bar wrapped the content in a new `Stack` and the split header
/// pushed it down a slot, and either rebuilt the code view's scrollable at the
/// top of the file.)
pub(crate) fn pane_view(app: &App, pane: usize) -> Element<'_, Message> {
    let time_travel = time_travel_on_screen(app).filter(|_| pane == app.proj.active);
    let content: Element<'_, Message> = match (time_travel, &app.proj.panes[pane]) {
        // Time Travel takes over the active pane (its own read-only view). The
        // live viewer is the fallback so the code stays visible while the first
        // historical revision loads (no blank "flash" on entry).
        (Some(tt), live) => time_travel_view(app, tt, live.as_ref()),
        (None, Some(v)) => {
            // The diff view replaces the code of the active pane's file.
            if pane == app.proj.active
                && let Some(d) = &app.proj.diff
                && d.abs == v.abs
            {
                diff_view(app, pane, d)
            } else if let Some(doc) = v.notebook.as_deref() {
                // A Jupyter notebook renders as its native cell view.
                notebook_pane(app, pane, v, doc)
            } else if let Some(md) = v.md.as_ref().filter(|_| !v.show_source) {
                // A markdown file renders as a document; a toggle in the
                // breadcrumb switches to the raw source.
                markdown_pane(app, pane, v, md)
            } else {
                code_pane(app, pane, v)
            }
        }
        (None, None) => mouse_area(empty_state(
            Glyph::Note,
            tr!("No file open"),
            if crate::keymap::MAC_KEYS {
                tr!("Pick a file from the tree, or press ⌘P.")
            } else {
                tr!("Pick a file from the tree, or press Ctrl+P.")
            },
            None,
        ))
        .on_press(Message::Editor(EditorMsg::PaneFocused(pane)))
        .into(),
    };

    // The find bar floats over the top-right of the active pane.
    let find: Element<'_, Message> =
        if app.proj.find.open && pane == app.proj.active && time_travel.is_none() {
            find_bar(app)
        } else {
            slot()
        };
    let header: Element<'_, Message> = if app.proj.split {
        pane_header(app, pane)
    } else {
        slot()
    };
    column![header, stack![editor_shell(content), find]]
        .width(Fill)
        .height(Fill)
        .into()
}

/// What `pane`, showing `abs` — as code, a rendered document, a notebook or
/// a diff — reports of the reader's own scrolling ([`reader_scroll()`]):
/// `ReaderScrolled`, while a walkthrough step waits to settle in `abs` and
/// the scroll would still supersede it. That is all it is for, so the rest
/// of the time nothing is watched, and a scroll costs no message.
pub(crate) fn reader_scroll_report(
    app: &App,
    pane: usize,
    abs: &std::path::Path,
) -> Option<Message> {
    app.proj
        .walk
        .waits_in(abs)
        .then_some(Message::Editor(EditorMsg::ReaderScrolled(pane)))
}

// ------------------------------------------------------------- time travel

/// The time-travel session, when it is on screen: it takes over the active
/// pane while that pane shows the session's file, and nothing covers the
/// panes ([`pane_cover`]). A session outlives both — the reader focuses the
/// other half of a split, or opens the overview — and shows again once its
/// file does; meanwhile what the reader sees is something else. The view
/// ([`pane_view`]) and whatever acts for the reader on the view — the keys
/// (`App::handle_key`), the copy, the status bar — go through this one
/// choice, so a session off screen never takes a key meant for what is on it.
pub(crate) fn time_travel_on_screen(app: &App) -> Option<&TimeTravel> {
    let tt = app.proj.time_travel.as_ref()?;
    let shown = pane_cover(app).is_none() && app.active_viewer().is_some_and(|v| v.abs == tt.abs);
    shown.then_some(tt)
}

/// The pane the time-travel session is drawn in — the active one, while the
/// session is on screen ([`time_travel_on_screen`]) — and `None` while it is
/// off screen, where no pane shows it.
pub(crate) fn time_travel_pane(app: &App) -> Option<usize> {
    time_travel_on_screen(app).map(|_| app.proj.active)
}

/// The git time-travel view: a commit banner on top, the historical (read-only)
/// code in the middle, and a timeline scrubber at the bottom. `live` is the
/// pane's current viewer, shown until the first historical revision loads.
pub(crate) fn time_travel_view<'a>(
    app: &'a App,
    tt: &'a TimeTravel,
    live: Option<&'a Viewer>,
) -> Element<'a, Message> {
    let commit = tt.commits.get(tt.idx);
    // The historical viewer once ready; otherwise the live one so the code area
    // never goes blank on entry.
    let code: Element<'a, Message> = match tt.viewer.as_ref().or(live) {
        Some(hv) => time_travel_code(app, tt, hv),
        None => center(
            text(tr!("Loading revision…"))
                .size(ts::BASE)
                .color(theme::dim()),
        )
        .into(),
    };
    // The story sits in a slot that is always there (zero-size until it is
    // asked for): pushed in ahead of the code, it moved the historical view's
    // scrollable down a position, iced rebuilt it at the top, and its scroll
    // report overwrote where the reader was.
    let story: Element<'a, Message> = match &tt.story {
        Some(story) => time_travel_story(app, tt, story),
        None => slot(),
    };
    column![
        time_travel_banner(tt, commit),
        story,
        container(code).width(Fill).height(Fill),
        time_travel_bar(tt),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

/// The commit banner: sha · author · when, the subject, and the AI "what & why".
pub(crate) fn time_travel_banner<'a>(
    tt: &'a TimeTravel,
    commit: Option<&'a crate::git::HistCommit>,
) -> Element<'a, Message> {
    // A tidy "Exit  esc" — the little keycap reads as a control and teaches the
    // shortcut, instead of a bare ✕ glyph.
    let keycap = container(text("esc").size(ts::CAPTION).color(theme::fg_muted()))
        .padding(Padding {
            top: 1.0,
            right: 5.0,
            bottom: 1.0,
            left: 5.0,
        })
        .style(|_: &iced::Theme| iced::widget::container::Style {
            background: Some(theme::bg_active().into()),
            border: iced::Border {
                radius: 3.0.into(),
                width: 1.0,
                color: theme::hairline(),
            },
            ..Default::default()
        });
    let exit = button(
        row![
            text(tr!("Exit")).size(ts::SMALL).color(theme::fg_muted()),
            keycap
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .style(theme::toolbar_button)
    .padding([2, 8])
    .on_press(Message::TimeTravel(TimeTravelMsg::Exit));

    let Some(c) = commit else {
        let head = row![
            glyph::icon(Glyph::TimeTravel, theme::accent(), 15.0),
            text(tr!("Time Travel")).size(ts::BODY).color(theme::fg()),
            space().width(Fill),
            exit,
        ]
        .spacing(8)
        .align_y(iced::Center);
        return container(head)
            .padding([7, 12])
            .width(Fill)
            .style(theme::pane_header)
            .into();
    };

    let short: String = c.sha.chars().take(8).collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let head = row![
        glyph::icon(Glyph::TimeTravel, theme::accent(), 15.0),
        text(short)
            .size(ts::BODY)
            .color(theme::accent())
            .font(crate::fonts::CODE),
        text(format!(
            "{}  ·  {}",
            c.author,
            crate::i18n::ago(c.time, now)
        ))
        .size(ts::SMALL)
        .color(theme::dim()),
        space().width(Fill),
        exit,
    ]
    .spacing(10)
    .align_y(iced::Center);

    let subject = text(c.subject.clone())
        .size(ts::BODY)
        .color(theme::fg())
        .wrapping(Wrapping::Word);

    let why: Element<'a, Message> = if tt.why_loading {
        text(tr!("Summarizing…"))
            .size(ts::SMALL)
            .color(theme::dim())
            .into()
    } else if let Some(w) = tt.why.get(&c.sha) {
        // Plain text: a model's code marks would show as they are.
        text(strip_backticks(w))
            .size(ts::SMALL)
            .color(theme::fg_muted())
            .wrapping(Wrapping::Word)
            .into()
    } else {
        button(
            text(tr!("What & why?"))
                .size(ts::SMALL)
                .color(theme::accent()),
        )
        .style(theme::toolbar_button)
        .padding([2, 8])
        .on_press(Message::TimeTravel(TimeTravelMsg::Why))
        .into()
    };

    container(column![head, subject, why].spacing(5))
        .padding([7, 12])
        .width(Fill)
        .style(theme::pane_header)
        .into()
}

/// The historical code — a read-only code view of the file at this revision,
/// with the commit's added/changed lines marked in the gutter.
pub(crate) fn time_travel_code<'a>(
    app: &'a App,
    tt: &'a TimeTravel,
    hv: &'a Viewer,
) -> Element<'a, Message> {
    let lh = app.line_height();
    let row0 = (tt.scroll_y / lh) as usize;
    let sticky = hv.sticky_at_row(row0);
    // Read-only, but clicking still places a caret and dragging selects (for
    // reading / copying). Right-click has no menu in a historical view.
    let code = CodeView::new(
        &hv.lines,
        hv.max_cols,
        app.font_size,
        lh,
        theme::code_fg(),
        |(line, col)| Message::TimeTravel(TimeTravelMsg::SelectStart { line, col }),
        |(line, col)| Message::TimeTravel(TimeTravelMsg::SelectDrag { line, col }),
        |_, _| Message::Noop,
    )
    .generation(hv.generation())
    .lines_generation(hv.lines_generation())
    .cursor(hv.caret)
    .selection(hv.selection_ordered())
    .sticky(sticky)
    .folds(hv.visible_rows(), &hv.fold_header_set, &hv.collapsed)
    .indent_guides(app.show_indent_guides)
    .git_gutter(hv.git.as_deref());
    // The live pane's scroll id, so the scroll operations aimed at the active
    // pane reach this view too. An id does not carry widget state across the
    // enter/exit swap — this scrollable sits elsewhere in the tree than the
    // live one, so iced builds it fresh — which is why entering and leaving
    // re-issue `scroll_to` from `TimeTravel::scroll_y` / `Viewer::scroll_y`.
    scrollable(code)
        .id(code_scroll_id(app.proj.active))
        .on_scroll(|v| Message::TimeTravel(TimeTravelMsg::Scrolled(v)))
        .direction(both_scroll())
        .style(theme::overlay_scrollbar)
        .width(Fill)
        .height(Fill)
        .into()
}

/// The timeline scrubber: older/newer steps, a slider, position, scope toggle,
/// and (for a function scope) the "story of this function" narrative button.
pub(crate) fn time_travel_bar(tt: &TimeTravel) -> Element<'_, Message> {
    let n = tt.commits.len();
    let last = n.saturating_sub(1);
    // `then` (lazy) — not `then_some` — so `idx - 1` isn't evaluated (underflowing
    // usize) when idx is 0.
    let older = (tt.idx < last).then(|| Message::TimeTravel(TimeTravelMsg::Goto(tt.idx + 1)));
    let newer = (tt.idx > 0).then(|| Message::TimeTravel(TimeTravelMsg::Goto(tt.idx - 1)));
    let step = |g: Glyph, msg: Option<Message>| {
        let on = msg.is_some();
        let mut b = button(glyph::icon(
            g,
            if on { theme::fg() } else { theme::dim() },
            15.0,
        ))
        .style(theme::toolbar_button)
        .padding([2, 8]);
        if let Some(m) = msg {
            b = b.on_press(m);
        }
        b
    };
    // Slider: left = oldest, right = newest; position = last - idx.
    let sl = slider(0.0..=last.max(1) as f32, (last - tt.idx) as f32, move |v| {
        let p = (v.round() as usize).min(last);
        Message::TimeTravel(TimeTravelMsg::Goto(last - p))
    })
    .step(1.0)
    .width(Fill);

    let scope_label = match &tt.scope {
        TimeScope::Symbol { name, kind, .. } => format!("{} {name}", short_kind(kind)),
        TimeScope::File => tr!("whole file").to_string(),
    };
    // Clicking toggles between the whole file and the block under the caret.
    let scope_btn = button(
        text(format!("{}  ⇄", tr!("Scope: {scope}", scope = scope_label)))
            .size(ts::SMALL)
            .color(theme::fg_muted()),
    )
    .style(theme::toolbar_button)
    .padding([2, 8])
    .on_press(Message::TimeTravel(TimeTravelMsg::ToggleScope));

    let story: Element<'_, Message> = if matches!(tt.scope, TimeScope::Symbol { .. }) {
        if tt.story_loading {
            text(tr!("Writing the story…"))
                .size(ts::SMALL)
                .color(theme::dim())
                .into()
        } else {
            let label = if tt.story.is_some() {
                tr!("Hide story")
            } else {
                tr!("Story")
            };
            let color = if tt.story.is_some() {
                theme::fg_muted()
            } else {
                theme::accent()
            };
            button(text(label).size(ts::SMALL).color(color))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::TimeTravel(TimeTravelMsg::Story))
                .into()
        }
    } else {
        space().into()
    };
    // Writing the story, Story, Hide story: in a place as wide as the widest
    // of them (and a button's padding), at the bar's end — turning from one
    // to the next moved the scope, the count and the arrows before it.
    let story: Element<'_, Message> = if matches!(tt.scope, TimeScope::Symbol { .. }) {
        let widest = super::steady::widest(
            &[tr!("Writing the story…"), tr!("Hide story"), tr!("Story")],
            ts::SMALL,
            crate::fonts::UI,
        );
        container(story).width(widest + 16.0).into()
    } else {
        story
    };

    container(
        row![
            chrome_tip(
                step(Glyph::ArrowLeft, older),
                tr!("Older commit"),
                Some(crate::keymap::local_keys("⌘←"))
            ),
            sl,
            chrome_tip(
                step(Glyph::ArrowRight, newer),
                tr!("Newer commit"),
                Some(crate::keymap::local_keys("⌘→"))
            ),
            // As wide as it reads at the last commit: scrubbing past the
            // tenth moved the arrow before it and the slider's end. Counted
            // as the slider runs, from the oldest: it counted from the
            // newest, down as the slider went up.
            {
                let said = format!("{} / {}", n - tt.idx, n);
                super::steady::slot(
                    text(said.clone()).size(ts::SMALL).color(theme::dim()),
                    &said,
                    &[&format!("{n} / {n}")],
                    ts::SMALL,
                )
            },
            space().width(16),
            scope_btn,
            story,
        ]
        .spacing(10)
        .align_y(iced::Center),
    )
    .padding([5, 12])
    .width(Fill)
    .style(theme::statusbar)
    .into()
}

/// The "story of this function" narrative panel (AI), shown above the code.
pub(crate) fn time_travel_story<'a>(
    app: &'a App,
    tt: &'a TimeTravel,
    story: &'a [crate::PreparedSeg],
) -> Element<'a, Message> {
    // A name is quoted; the block that has none is not.
    let title = match tt.scope.symbol_name() {
        Some(name) => tr!("Story of “{name}”", name = name),
        None => tr!("Story of this block").to_string(),
    };
    let header = row![
        column![
            text(title).size(ts::BODY).color(theme::accent()),
            // `git log -L` only follows the block's CURRENT lines, so earlier
            // rewrites may not be attributed — say so, so it's not read as a full
            // biography.
            text(tr!("From the commits that touched these lines"))
                .size(ts::CAPTION)
                .color(theme::dim()),
        ]
        .spacing(1),
        space().width(Fill),
        button(glyph::icon(Glyph::Close, theme::dim(), 10.0))
            .style(theme::toolbar_button)
            .padding([4, 6])
            .on_press(Message::TimeTravel(TimeTravelMsg::Story)),
    ]
    .align_y(iced::Center);
    let body = scrollable(
        Column::with_children(render_prepared(app, story))
            .spacing(8)
            .width(Fill),
    )
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Length::Fixed(200.0));
    container(column![header, body].spacing(6))
        .padding([8, 12])
        .width(Fill)
        .style(theme::modal_panel)
        .into()
}

/// The unified diff of the active file versus `HEAD`, colored by line kind,
/// in `pane` (the active one).
pub(crate) fn diff_view<'a>(
    app: &'a App,
    pane: usize,
    d: &'a crate::DiffState,
) -> Element<'a, Message> {
    use crate::git::DiffKind;

    let header = container(
        row![
            text(tr!("{file}  ·  vs HEAD", file = d.rel))
                .size(ts::BODY)
                .color(theme::accent()),
            space().width(Fill),
            super::toolbar::icon_action(
                crate::glyph::Glyph::Close,
                tr!("Close"),
                Some(Message::Editor(EditorMsg::ToggleDiff)),
            ),
        ]
        .align_y(iced::Center),
    )
    .padding(Padding {
        top: 5.0,
        right: 8.0,
        bottom: 5.0,
        left: 10.0,
    })
    .style(theme::pane_header)
    .width(Fill);

    if d.lines.is_empty() {
        return column![
            header,
            center(
                text(tr!("No uncommitted changes in {file}", file = d.rel))
                    .size(ts::BASE)
                    .color(theme::dim()),
            )
        ]
        .width(Fill)
        .height(Fill)
        .into();
    }

    // Size every run to the widest line so the color tints span the full
    // content width and long lines become reachable via horizontal scroll —
    // measured, not guessed: the real monospace advance, and the columns the
    // widest line is drawn in (counted once, when the diff arrived — see
    // `DiffState::new` — not per repaint). Never narrower than the pane: a
    // diff of short lines tinted its added lines and hunk headers only as
    // far as its longest line, a third of the way across.
    let advance = mono_advance(app.font_size);
    // The numbers' columns: the widest number, before and after, a space
    // between — then the mark (+, −) of what happened to the line.
    let gutter_cols = d
        .runs
        .iter()
        .find(|r| r.kind != DiffKind::Hunk)
        .and_then(|r| r.gutter.lines().next())
        .map_or(0, |g| g.chars().count());
    let gutter_w = (gutter_cols as f32 + 1.0) * advance + DIFF_PAD_LEFT;
    let mark_w = 2.0 * advance;
    let content_width =
        row_width(d.max_cols, advance, DIFF_PAD_LEFT + DIFF_PAD_RIGHT) + gutter_w + mark_w;
    let body = iced::widget::responsive(move |size| {
        let run_width = diff_run_width(content_width, size.width);
        // One text block per run of same-kind lines rather than a widget per
        // line: the same picture (runs stack with no gap) at a fraction of the
        // layout work on a long diff — prepared once, borrowed here.
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
        for run in &d.runs {
            let code_text = |t: &'a str, color: iced::Color| {
                text(t)
                    .font(crate::fonts::CODE)
                    .size(app.font_size)
                    .color(color)
                    .wrapping(Wrapping::None)
            };
            // Where a hunk is: a quiet line naming the place, not git's
            // `@@ -12,7 +12,9 @@`.
            if run.kind == DiffKind::Hunk {
                rows.push(
                    container(code_text(run.text.as_str(), theme::dim()))
                        .width(Length::Fixed(run_width))
                        .padding(Padding {
                            top: 6.0,
                            right: DIFF_PAD_RIGHT,
                            bottom: 2.0,
                            left: gutter_w + mark_w,
                        })
                        .style(|_: &iced::Theme| container::Style {
                            background: Some(theme::with_alpha(theme::accent(), 0.06).into()),
                            ..container::Style::default()
                        })
                        .into(),
                );
                continue;
            }
            let (bg, mark_color) = match run.kind {
                DiffKind::Add => (
                    Some(theme::with_alpha(theme::success(), 0.14)),
                    theme::success(),
                ),
                DiffKind::Remove => (
                    Some(theme::with_alpha(theme::danger(), 0.14)),
                    theme::danger(),
                ),
                _ => (None, theme::dim()),
            };
            // The code, coloured as the editor colours it.
            let spans: Vec<iced::widget::text::Span<'a, Message>> = run
                .spans
                .iter()
                .map(|(t, style)| {
                    iced::widget::span(t.as_str())
                        .color(
                            style
                                .and_then(crate::highlight::style_color)
                                .unwrap_or(theme::code_fg()),
                        )
                        .font(crate::fonts::CODE)
                        .size(app.font_size)
                })
                .collect();
            let line = row![
                container(code_text(run.gutter.as_str(), theme::dim()))
                    .width(Length::Fixed(gutter_w))
                    .padding(Padding {
                        left: DIFF_PAD_LEFT,
                        ..Padding::ZERO
                    }),
                container(code_text(run.marks.as_str(), mark_color)).width(Length::Fixed(mark_w)),
                iced::widget::rich_text(spans).wrapping(Wrapping::None),
            ];
            let mut cell = container(line)
                .width(Length::Fixed(run_width))
                .padding(Padding {
                    top: 0.0,
                    right: DIFF_PAD_RIGHT,
                    bottom: 0.0,
                    left: 0.0,
                });
            if let Some(bg) = bg {
                cell = cell.style(move |_: &iced::Theme| container::Style {
                    background: Some(bg.into()),
                    ..container::Style::default()
                });
            }
            rows.push(cell.into());
        }
        if d.lines.len() > MAX_DIFF_ROWS {
            rows.push(
                text(trn!(
                    "… 1 more line",
                    "… {n} more lines",
                    d.lines.len() - MAX_DIFF_ROWS
                ))
                .size(ts::SMALL)
                .color(theme::dim())
                .into(),
            );
        }

        // A walkthrough step can open its file here — the diff stays on for
        // the file it was asked for — so the reader's scroll counts here as
        // in the code this stands in for.
        reader_scroll(
            scrollable(Column::with_children(rows).padding([4, 0]))
                .direction(both_scroll())
                .style(theme::overlay_scrollbar)
                .width(Fill)
                .height(Fill),
            reader_scroll_report(app, pane, &d.abs),
        )
    });

    column![header, body].width(Fill).height(Fill).into()
}

/// The width of a diff row: the widest line's (`content`), or the pane's
/// (`visible`) when the lines are all shorter, so a row's tint spans it.
pub(crate) fn diff_run_width(content: f32, visible: f32) -> f32 {
    content.max(visible)
}

/// The most diff lines the view renders (the rest are summarized).
pub(crate) const MAX_DIFF_ROWS: usize = 8000;
/// Horizontal padding of a diff row.
const DIFF_PAD_LEFT: f32 = 10.0;
const DIFF_PAD_RIGHT: f32 = 8.0;
/// Columns a tab advances in the reader (the highlighter expands tabs to this
/// many spaces, so the diff draws them the same way).
pub(crate) const TAB_COLS: usize = 4;

/// A diff line as the view draws it: tabs expanded like the code view's, and a
/// blank line kept one space wide so it does not collapse.
pub(crate) fn diff_display_text(line: &str) -> String {
    // In order, as the code view draws it (`highlight::line_in_order`): a
    // direction control in a changed line reordered it here.
    let shown = crate::highlight::line_in_order(line);
    if shown.is_empty() {
        " ".to_string()
    } else {
        shown.replace('\t', &" ".repeat(TAB_COLS))
    }
}

/// Monospace columns `line` occupies once drawn: tabs as [`TAB_COLS`], wide
/// glyphs (CJK, emoji) as two, zero-width marks as none.
pub(crate) fn display_cols(line: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    line.chars()
        .map(|c| match c {
            '\t' => TAB_COLS,
            c => c.width().unwrap_or(0),
        })
        .sum()
}

/// Width of a diff row wide enough for every line: the widest line's `cols`
/// (plus one of slack) at `advance` px each, and the row's padding.
pub(crate) fn row_width(cols: usize, advance: f32, padding: f32) -> f32 {
    (cols as f32 + 1.0) * advance + padding
}

/// Advance of one monospace glyph at `size`, measured by the same text engine
/// the renderer shapes with (the diff used to assume 0.6em). Memoized per size,
/// as the view asks on every repaint.
pub(crate) fn mono_advance(size: f32) -> f32 {
    thread_local! {
        static MEMO: std::cell::Cell<Option<(u32, f32)>> = const { std::cell::Cell::new(None) };
    }
    if let Some((bits, w)) = MEMO.get()
        && bits == size.to_bits()
    {
        return w;
    }
    let w = measured_mono_advance(size).unwrap_or(size * 0.6);
    MEMO.set(Some((size.to_bits(), w)));
    w
}

/// One monospace glyph's advance at `size` as the text engine shapes it, or
/// `None` when it measured nothing (no monospace face at all) — the case
/// [`mono_advance`] falls back to 0.6 em for.
pub(crate) fn measured_mono_advance(size: f32) -> Option<f32> {
    use iced::advanced::text::{self, Paragraph as _};
    let sample = <iced::Renderer as text::Renderer>::Paragraph::with_text(text::Text {
        content: "0",
        bounds: iced::Size::INFINITE,
        size: size.into(),
        line_height: text::LineHeight::Absolute(size.into()),
        font: crate::fonts::CODE,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
    });
    Some(sample.min_bounds().width).filter(|w| *w > 0.0)
}

pub(crate) fn find_bar(app: &App) -> Element<'_, Message> {
    let count = if app.proj.find.query.is_empty() {
        String::new()
    } else if app.proj.find.matches.is_empty() {
        "0/0".to_string()
    } else {
        format!(
            "{}/{}",
            app.proj.find.current + 1,
            app.proj.find.matches.len()
        )
    };

    let input = text_input(tr!("Find in file…"), &app.proj.find.query)
        .style(theme::input)
        .id(find_input_id())
        .on_input(|v| Message::Editor(EditorMsg::FindQueryChanged(v)))
        .on_submit(Message::Editor(EditorMsg::FindStep(1)))
        .size(ts::BASE)
        .padding([4, 8])
        .width(190);

    let btn = |g: Glyph, msg: Message| {
        button(glyph::icon(g, theme::fg_muted(), 12.0))
            .style(theme::toolbar_button)
            .padding([6, 7])
            .on_press(msg)
    };

    let bar = container(
        row![
            input,
            text(count).size(ts::SMALL).color(theme::dim()).width(46),
            btn(Glyph::ChevronLeft, Message::Editor(EditorMsg::FindStep(-1))),
            btn(Glyph::ChevronRight, Message::Editor(EditorMsg::FindStep(1))),
            btn(Glyph::Close, Message::Editor(EditorMsg::FindClosed)),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .padding(6)
    .style(theme::modal_panel);

    // Pin to the top-right of the pane. Its panel takes every press and
    // hover over it: the count and the gaps between its controls took none,
    // and a press on one reached the code drawn under it (the caret moved
    // there, a ⌘-press went to a definition, a rest asked for a peek of the
    // token hidden there). The row it is pinned in does not, or the code to
    // its left could not be clicked.
    container(iced::widget::opaque(bar))
        .width(Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .padding(Padding {
            top: 6.0,
            right: 16.0,
            bottom: 0.0,
            left: 0.0,
        })
        .into()
}

pub(crate) fn pane_header(app: &App, pane: usize) -> Element<'_, Message> {
    let active = app.proj.active == pane;
    let title = app.proj.panes[pane]
        .as_ref()
        .map(|v| crate::highlight::in_order(&v.rel))
        .unwrap_or("—".into());
    mouse_area(
        container(
            text(title)
                .size(ts::SMALL)
                .color(if active {
                    theme::accent()
                } else {
                    theme::dim()
                })
                .wrapping(Wrapping::None),
        )
        .width(Fill)
        .padding([3, 8])
        .style(theme::pane_header),
    )
    .on_press(Message::Editor(EditorMsg::PaneFocused(pane)))
    .into()
}

pub(crate) fn welcome(app: &App) -> Element<'_, Message> {
    // On a live remote the hero invites browsing that host; otherwise it offers
    // opening local code or connecting out over SSH.
    let (subtitle, primary, secondary): (String, (&str, Message), (&str, Message)) =
        if app.connection.is_remote() {
            let host = app.connection.label();
            // Said as it is: a host that could not be reached read
            // "Connected to …", with Browse folders offering its folders.
            let (subtitle, primary) = if app.server.ready {
                (
                    tr!("Connected to {host}", host = host),
                    tr!("Browse folders…"),
                )
            } else if app.handshake_failure.is_some() {
                (
                    tr!("Could not connect to {host}", host = host),
                    tr!("Open Remote…"),
                )
            } else {
                (
                    tr!("Connecting to {host}…", host = host),
                    tr!("Open Remote…"),
                )
            };
            (
                subtitle,
                (primary, Message::Connect(ConnectMsg::Open)),
                (tr!("Disconnect"), Message::Connect(ConnectMsg::Disconnect)),
            )
        } else {
            (
                // "clew" = the thread that guides you out of the labyrinth.
                tr!("Find the thread through your codebase").to_string(),
                (
                    tr!("Open Folder…"),
                    Message::Project(ProjectMsg::OpenFolderPressed),
                ),
                (tr!("Open Remote…"), Message::Connect(ConnectMsg::Open)),
            )
        };

    let actions = row![
        button(text(primary.0.to_string()).size(ts::EMPHASIS))
            .style(theme::primary_button)
            .padding([8, 20])
            .on_press(primary.1),
        button(text(secondary.0.to_string()).size(ts::EMPHASIS))
            .style(theme::secondary_button)
            .padding([8, 20])
            .on_press(secondary.1),
    ]
    .spacing(10);

    // Brand lockup: the "C" mark on the left, the name on the right — same mark
    // as the app icon (minus the square), so the welcome reads as part of the app.
    // Tint the mark to a soft foreground tone so it reads on every theme — the
    // asset's own light-grey stroke washes out on light backgrounds.
    let mark = iced::widget::svg(iced::widget::svg::Handle::from_memory(MARK_SVG))
        .width(Length::Fixed(52.0))
        .height(Length::Fixed(52.0))
        .style(|_theme, _status| iced::widget::svg::Style {
            color: Some(theme::fg_muted()),
        });
    let brand = row![mark, text("Clew").size(ts::HERO).color(theme::fg_bright())]
        .spacing(14)
        .align_y(iced::Center);

    center(
        column![
            brand,
            space().height(4),
            text(subtitle).size(ts::BASE).color(theme::fg_muted()),
            space().height(22),
            actions,
        ]
        .spacing(6)
        .align_x(iced::Center),
    )
    .into()
}

/// The Clew "C" mark (monochrome arcs, transparent background) for the welcome
/// screen. Rendered with a theme-adaptive tint (see `welcome`), so the asset's
/// own stroke colour doesn't matter.
const MARK_SVG: &[u8] = include_bytes!("../../assets/icon/mark.svg");

/// Render a markdown file (`v`'s `items`) as a document — readmes,
/// changelogs — instead of raw source. Links open via the normal `OpenLink`
/// path; the `PaneFocused` mouse area keeps click-to-focus working like the
/// code view, and the reader's scroll counts as it does there
/// (`reader_scroll_report`).
pub(crate) fn markdown_pane<'a>(
    app: &App,
    pane: usize,
    v: &Viewer,
    items: &'a [iced::widget::markdown::Item],
) -> Element<'a, Message> {
    let doc =
        iced::widget::markdown::view(items, theme::markdown_settings_at(theme::DOCUMENT_SIZE))
            .map(|url| Message::Content(ContentMsg::OpenLink(url.to_string())));
    let body = container(doc).padding([16, 28]).max_width(920);
    mouse_area(reader_scroll(
        scrollable(body)
            .width(Fill)
            .height(Fill)
            .style(theme::overlay_scrollbar),
        reader_scroll_report(app, pane, &v.abs),
    ))
    .on_press(Message::Editor(EditorMsg::PaneFocused(pane)))
    .into()
}

pub(crate) fn code_pane<'a>(app: &'a App, pane: usize, v: &'a Viewer) -> Element<'a, Message> {
    // Bookmarked lines of this file, for the gutter marker.
    let marked: std::collections::HashSet<usize> = app
        .proj
        .bookmarks
        .iter()
        .filter(|b| b.rel == v.rel)
        .map(|b| b.line)
        .collect();

    // Debug: this file's breakpoints (and which are conditional), plus the
    // current stopped line (if here).
    let file_bps = app.debug.breakpoints.get(&v.abs);
    let breakpoints: std::collections::HashSet<usize> = file_bps
        .map(|m| m.keys().copied().collect())
        .unwrap_or_default();
    let cond_breakpoints: std::collections::HashSet<usize> = file_bps
        .map(|m| {
            m.iter()
                .filter(|(_, bp)| bp.condition.is_some())
                .map(|(l, _)| *l)
                .collect()
        })
        .unwrap_or_default();
    // Only an explicit refusal, never `None`: a breakpoint the adapter has not
    // answered about (no session yet, reply in flight) is not known to be dead.
    let unverified_breakpoints: std::collections::HashSet<usize> = file_bps
        .map(|m| {
            m.iter()
                .filter(|(_, bp)| bp.verified == Some(false))
                .map(|(l, _)| *l)
                .collect()
        })
        .unwrap_or_default();
    let debug_current = app
        .debug
        .session
        .as_ref()
        .and_then(|d| d.current.as_ref())
        .filter(|(p, _)| *p == v.abs)
        .map(|(_, line)| *line);

    let frame = app.window_frame;
    // The block cursor shows only on the active pane while the code view has
    // keyboard focus.
    let cursor = if pane == app.proj.active && app.code_focused {
        v.caret
    } else {
        None
    };

    let mut code = CodeView::new(
        &v.lines,
        v.max_cols,
        app.font_size,
        app.line_height(),
        theme::code_fg(),
        move |(line, col)| Message::Editor(EditorMsg::SelectStart { pane, line, col }),
        move |(line, col)| Message::Editor(EditorMsg::SelectDrag { pane, line, col }),
        // The pointer is in the window's coordinates; what is placed at it
        // is inside the window's frame.
        move |(line, col), at| {
            Message::Hover(HoverMsg::ContextMenuOpened {
                pane,
                line,
                col,
                x: at.x - frame,
                y: at.y - frame,
            })
        },
    )
    .generation(v.generation())
    .lines_generation(v.lines_generation())
    .selection(v.selection_ordered())
    .cursor(cursor)
    .reading_line(
        (pane == app.proj.active)
            .then_some(v.caret)
            .flatten()
            .map(|(line, _)| line),
    )
    .highlights(app.code_highlights(pane, v))
    .sticky(app.sticky_headers(v))
    .bookmarks(marked)
    .breakpoints(breakpoints)
    .cond_breakpoints(cond_breakpoints)
    .unverified_breakpoints(unverified_breakpoints)
    .debug_current(debug_current)
    .inlay_hints(v.inlay_hints(), theme::dim())
    .inactive(v.inactive_lines())
    // The line that says why the lines under it are left out, with the
    // reason beside it.
    .inactive_heads(v.inactive_heads(), move |pred| {
        use crate::inactive::CfgReason;
        Some(
            match crate::inactive::rust_cfg_reason(
                pred,
                &app.proj.reading_target,
                &app.proj.build_facts,
                Some(&v.rel),
            )? {
                CfgReason::Target(target) => {
                    tr!(
                        "not built for {target}",
                        target = super::statusbar::target_name(&target)
                    )
                }
                CfgReason::Features => tr!("not built with these features").to_string(),
            },
        )
    })
    // The summary of those two and the folds, kept current by the viewer's
    // setters — else the view re-derives it, hashing every chip, per build.
    .annotations(v.annotations())
    .folds(v.visible_rows(), &v.fold_header_set, &v.collapsed)
    .on_fold(move |line| Message::Editor(EditorMsg::FoldToggle { pane, line }))
    .on_breakpoint(move |line| {
        Message::Debug(DebugMsg::BreakpointToggle {
            path: v.abs.clone(),
            line,
        })
    })
    .indent_guides(app.show_indent_guides)
    .git_gutter(v.git.as_deref())
    .blame(
        if pane == app.proj.active && app.code_focused && app.show_inline_blame {
            app.blame_annotation(v)
        } else {
            None
        },
    )
    .on_hover(move |(line, col), at| {
        Message::Hover(HoverMsg::Requested {
            pane,
            line,
            col,
            x: at.x - frame,
            y: at.y - frame,
        })
    })
    .on_hover_end(|| Message::Hover(HoverMsg::Cleared));
    // The minimap is opt-in (toggle in the "More" menu); without the callback
    // the widget draws no minimap band at all.
    if app.show_minimap {
        code = code.on_minimap(move |fraction| {
            Message::Editor(EditorMsg::MinimapScrolled { pane, fraction })
        });
    }
    // Find matches are painted from the find state itself (sorted, borrowed),
    // not copied into the highlight list per view — see `code_highlights`.
    if pane == app.proj.active && app.proj.find.open {
        code = code.find_matches(&app.proj.find.matches, app.proj.find.current);
    }

    // Scrolling runs on past the last line until it is the first in view, so
    // a file's end reads as any other part of it: under the line read, with
    // room after it, as the design shows the end of `lib.rs`.
    let overscroll = (v.viewport_h - app.line_height()).max(0.0);
    let scroller = reader_scroll(
        scrollable(column![code, space().height(overscroll)])
            .id(code_scroll_id(pane))
            .on_scroll(move |viewport| Message::Editor(EditorMsg::Scrolled(pane, viewport)))
            .direction(both_scroll())
            .style(theme::overlay_scrollbar)
            .width(Fill)
            .height(Fill),
        reader_scroll_report(app, pane, &v.abs),
    );

    // File TL;DR banner: a one-line "what is this file" from the explain cache,
    // pinned above the code. Dismissable (toggle back via the More menu). Its
    // slot is always there, so the summary arriving mid-read (Explain All
    // fills the cache as you scroll) does not rebuild the scroller below it.
    //
    // Not while the margin shows the file's own note — the caret between
    // functions, the Notes open beside the code: it said the same twice.
    let file_node = crate::explain::Node::File(v.abs.clone());
    let margin_says_it = right_panel_shown(app)
        && app.margin_tab == crate::app::model::MarginTab::Notes
        && app.proj.explain.view.as_ref() == Some(&file_node);
    let banner = if app.show_file_banner && !margin_says_it {
        app.proj
            .explain
            .cache
            .get(&file_node)
            .filter(|c| !crate::explain::is_error_summary(&c.summary))
            .map(|c| file_banner(first_sentence(&crate::app::shown_summary(c)), file_node))
    } else {
        None
    };
    // The design's breath between the sheet's header and the first line.
    column![
        banner.unwrap_or_else(slot),
        space().height(CODE_TOP_PAD),
        scroller
    ]
    .into()
}

/// Room above the code's first line.
const CODE_TOP_PAD: f32 = 6.0;

/// A one-line file summary pinned at the top of the code view: a click on
/// it shows the file's whole note in the margin. (A "›" before it read as
/// something to open, and opened nothing.)
pub(crate) fn file_banner<'a>(summary: String, file: crate::explain::Node) -> Element<'a, Message> {
    container(
        row![
            super::toolbar::tip_at(
                button(
                    text(summary)
                        .size(ts::BODY)
                        .color(theme::fg_muted())
                        .wrapping(Wrapping::None),
                )
                .style(theme::toolbar_button)
                .padding([2, 6])
                .width(Fill)
                .on_press(Message::Explain(ExplainMsg::Show(file))),
                tr!("Show the file's note"),
                None,
                tooltip::Position::Bottom,
            ),
            button(glyph::icon(Glyph::Close, theme::dim(), 10.0))
                .style(theme::toolbar_button)
                .padding([4, 6])
                .on_press(Message::Window(WindowMsg::ToggleFileBanner)),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .width(Fill)
    .padding([4, 10])
    .style(theme::panel)
    .into()
}

// ---------------------------------------------------------------- outline

/// Narrowest window the right panel is drawn in: below it, the reader gets
/// the width.
pub(crate) const RIGHT_PANEL_MIN_WINDOW_W: f32 = 950.0;

/// Whether the right panel is drawn: asked for, and room and a file for it.
/// The one test the view and the tutorial's spotlight both use — the
/// spotlight used to take `show_right_panel` alone and framed a panel the
/// view had hidden (a split, a narrow window, no file).
///
/// Not beside a page that covers the panes (the Overview, Stats, Glossary,
/// a Docs page): its notes are of the file under it, and the sheet's
/// header, whose View menu shows and hides it, is not drawn there either.
pub(crate) fn right_panel_shown(app: &App) -> bool {
    app.show_right_panel
        && !app.proj.split
        && app.window_width >= RIGHT_PANEL_MIN_WINDOW_W
        && app.active_viewer().is_some()
        && super::docs_stats::pane_cover(app).is_none()
}

/// The reading panel — the margin beside the code: notes on what is read
/// (the note on the function the caret is in, raised as a card, over this
/// file's symbols with theirs), the calls around that function, and its
/// history. Hidden on narrow/split windows or with no file open.
pub(crate) fn right_panel(app: &App) -> Option<Element<'_, Message>> {
    if !right_panel_shown(app) {
        return None;
    }
    let body: Element<'_, Message> = match app.margin_tab {
        crate::MarginTab::Notes => margin_notes(app),
        crate::MarginTab::Calls => margin_calls(app),
        crate::MarginTab::History => margin_history(app),
    };
    Some(
        container(body)
            .width(Length::Fixed(app.right_width))
            .height(Fill)
            .style(theme::on_sheet)
            .into(),
    )
}

/// Height given to a note beside the code: its name and up to four lines
/// of what it says. Every note is as tall, so each can be placed level
/// with its function before it is drawn.
const NOTE_H: f32 = 104.0;
/// Height of the note raised as a card: the one on what the caret is in.
const ACTIVE_NOTE_H: f32 = 193.0;
/// Room kept between notes that would otherwise overlap.
const NOTE_GAP: f32 = 8.0;
/// How far the raised card reaches above its function's first line: the
/// design's 9, and the pixel of the edge it draws round itself (outside its
/// box, where iced draws a border inside), so its name is where the
/// design's is.
const ACTIVE_NOTE_LIFT: f32 = 10.0;
/// The raised card's edge: the design draws it as a ring round the card.
const NOTE_EDGE: f32 = 1.0;

/// The margin's Notes: clew's note on each function of the open file,
/// level with the function in the code; the note on the one the caret is
/// in raised as a card, with its calls, its history, and a way to ask
/// about it. Between functions, the file's own note heads the margin.
fn margin_notes(app: &App) -> Element<'_, Message> {
    use crate::explain::Node;
    let Some(v) = app.active_viewer() else {
        return slot();
    };
    let active = app.proj.explain.view.as_ref();
    let ordinals = crate::outline::fn_ordinals(&v.symbols);
    let function_node = |sym: &crate::outline::Symbol, ordinal: u32| Node::Function {
        file: v.abs.clone(),
        name: sym.name.clone(),
        ordinal,
    };
    let Some(active) = active else {
        // Nothing under the caret, and nothing of this file explained: say
        // how a note comes to be.
        let any_note = v
            .symbols
            .iter()
            .zip(&ordinals)
            .any(|(sym, &o)| app.proj.explain.cache.contains_key(&function_node(sym, o)));
        if !any_note {
            return compact_empty_state(Glyph::Lightbulb, tr!("Move the cursor into a function."));
        }
        return notes_beside_code(app, v, None, ordinals);
    };
    // A folder, or another file, ⌘-clicked: its note, whole, at the top.
    let elsewhere = match active {
        Node::Function { file, .. } | Node::File(file) => *file != v.abs,
        Node::Folder(_) => true,
    };
    if elsewhere {
        return container(
            container(explain_content(app))
                .width(Fill)
                .max_height((app.window_height * 0.6).max(160.0))
                .style(theme::note_card),
        )
        .padding(Padding {
            top: 0.0,
            right: 12.0,
            bottom: 0.0,
            left: 4.0,
        })
        .into();
    }
    notes_beside_code(app, v, Some(active), ordinals)
}

/// The notes of [`margin_notes`], each level with its function: `active`
/// is what the caret is in, raised as a card.
fn notes_beside_code<'a>(
    app: &'a App,
    v: &'a Viewer,
    active: Option<&'a crate::explain::Node>,
    ordinals: Vec<u32>,
) -> Element<'a, Message> {
    use crate::explain::Node;
    iced::widget::responsive(move |size| {
        let lh = app.line_height();
        // The code runs down to the margin's foot; what is above it in the
        // pane (a banner, the find bar) is the difference in height.
        let offset = (size.height - v.viewport_h).max(0.0);
        let y_of =
            |line1: usize| offset + v.row_of(line1.saturating_sub(1)) as f32 * lh - v.scroll_y;
        // The Ask pill floats over the margin's foot while Ask is hidden: a
        // raised note low in the view had its references, date and Ask
        // under it. The raised note stops above it.
        let foot = if app.ask_view == crate::AskView::Hidden {
            super::askfloat::PILL_H + super::askfloat::ASK_MARGIN + NOTE_GAP
        } else {
            0.0
        };
        let mut notes: Vec<(f32, f32, Element<'_, Message>)> = Vec::new();
        if matches!(active, Some(Node::File(_))) {
            let name = v.rel.rsplit('/').next().unwrap_or(&v.rel).to_string();
            notes.push((
                0.0,
                ACTIVE_NOTE_H,
                active_note(app, name, tr!("file").into(), NoteOf::File),
            ));
        }
        for (sym, &ordinal) in v.symbols.iter().zip(&ordinals) {
            if !matches!(sym.kind.as_str(), "function" | "method") {
                continue;
            }
            // What the caret is in (in this file, the margin's own).
            let is_active = matches!(
                active,
                Some(Node::Function { name, ordinal: o, .. })
                    if *name == sym.name && *o == ordinal
            );
            let y = y_of(sym.line);
            let (h, lift) = if is_active {
                (ACTIVE_NOTE_H, ACTIVE_NOTE_LIFT)
            } else {
                (NOTE_H, 0.0)
            };
            // Only what is in view is looked up.
            if y + h < 0.0 || y - lift > size.height {
                continue;
            }
            let node = Node::Function {
                file: v.abs.clone(),
                name: sym.name.clone(),
                ordinal,
            };
            let cached = app.proj.explain.cache.get(&node);
            if !is_active && cached.is_none() {
                continue;
            }
            let meta = tr!(
                "{kind} · line {line}",
                kind = short_kind(&sym.kind),
                line = sym.line
            );
            let note = if is_active {
                active_note(
                    app,
                    sym.name.clone(),
                    meta,
                    NoteOf::Function { sym, ordinal },
                )
            } else {
                let summary = cached
                    .map(|c| crate::app::shown_summary(c).into_owned())
                    .unwrap_or_default();
                plain_note(sym.name.clone(), meta, summary, sym.line)
            };
            let top = if is_active {
                (y - lift).min(size.height - h - foot)
            } else {
                y - lift
            };
            notes.push((top.max(0.0), h, note));
        }
        notes.sort_by(|a, b| a.0.total_cmp(&b.0));
        // Each note level with its function, or just under the one above
        // when they would overlap.
        // `end` is how far down the column has laid out; `floor` the highest
        // the next note may start, a gap under the one before.
        let mut col = Column::new();
        let (mut end, mut floor) = (0.0_f32, 0.0_f32);
        for (y, h, note) in notes {
            let top = y.max(floor);
            col = col.push(space().height(Length::Fixed(top - end)));
            col = col.push(
                container(note)
                    .height(Length::Fixed(h))
                    .width(Fill)
                    .clip(true),
            );
            end = top + h;
            floor = end + NOTE_GAP;
        }
        // The design's 4 and 12 to a note's box, less the edge the raised
        // card draws outside its own: a plain note pads that pixel back.
        container(col)
            .width(Fill)
            .height(Fill)
            .clip(true)
            .padding(Padding {
                top: 0.0,
                right: 12.0 - NOTE_EDGE,
                bottom: 0.0,
                left: 4.0 - NOTE_EDGE,
            })
            .into()
    })
    .into()
}

/// A note's head: the name it is about (as code, for a function) and what
/// kind of thing that is, where.
fn note_head<'a>(name: String, meta: String, code: bool, raised: bool) -> Element<'a, Message> {
    // Each in its face's own line height, as the design sets the head: the
    // row as tall as the taller, the two on one baseline.
    row![
        text(name)
            .size(NOTE_NAME_SIZE)
            .line_height(if code {
                ts::normal_line_code(NOTE_NAME_SIZE)
            } else {
                ts::normal_line(NOTE_NAME_SIZE)
            })
            .font(if code {
                crate::fonts::CODE_SEMIBOLD
            } else {
                crate::fonts::UI_SEMIBOLD
            })
            .color(if raised {
                theme::fg()
            } else {
                theme::fg_muted()
            })
            .wrapping(Wrapping::None),
        space().width(Fill),
        text(meta)
            .size(ts::SMALL)
            .line_height(ts::normal_line(ts::SMALL))
            .color(theme::dim())
            .wrapping(Wrapping::None),
    ]
    .spacing(8)
    .align_y(iced::Center)
    .into()
}

/// A note's name, over what is said of it.
const NOTE_NAME_SIZE: f32 = 12.0;

/// A note on a function the caret is not in: what clew says of it, a
/// click away from the function itself.
fn plain_note<'a>(
    name: String,
    meta: String,
    summary: String,
    line: usize,
) -> Element<'a, Message> {
    button(
        column![
            note_head(name, meta, true, false),
            text(summary)
                .size(super::explain_view::NOTE_SIZE)
                .line_height(super::explain_view::NOTE_LINE_HEIGHT)
                .color(theme::fg_muted()),
        ]
        .spacing(6),
    )
    .style(theme::menu_row)
    .width(Fill)
    .height(Fill)
    .padding([3.0, 14.0 + NOTE_EDGE])
    .on_press(Message::Editor(EditorMsg::OutlineJump(line)))
    .into()
}

/// The note on what the caret is in, raised as a card: what clew says of
/// it (as it streams in), and what to do next — its calls, its history,
/// a question about it, and, for a function, the explanation block by
/// block, or written afresh.
/// What a raised note is about: a function of the open file, or the file.
#[derive(Clone, Copy)]
enum NoteOf<'a> {
    Function {
        sym: &'a crate::outline::Symbol,
        ordinal: u32,
    },
    File,
}

/// How the note on a function counts what leans on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leaning {
    /// The places in the project that call it: each call site naming it in
    /// a function the graph has calling it.
    References(usize),
    /// The functions that call it, when their call sites are not known
    /// here (a remote project's graph comes without them).
    Callers(usize),
}

impl Leaning {
    /// What the note says: "21 references", "1 caller".
    pub(crate) fn label(self) -> String {
        match self {
            Leaning::References(n) => trn!("1 reference", "{n} references", n),
            Leaning::Callers(n) => trn!("1 caller", "{n} callers", n),
        }
    }
}

/// How much of the project leans on the function `name` (the `ordinal`th
/// of that name in `file`): the call sites naming it in each function the
/// graph has calling it, or, where those functions' call sites are not
/// known, how many such functions there are.
pub(crate) fn leaning_on(
    app: &App,
    file: &std::path::Path,
    name: &str,
    ordinal: u32,
) -> Option<Leaning> {
    let g = &app.proj.project_calls.graph;
    let id = g.id_of_key(file, name, ordinal)?;
    let callers = g.callers_of(id);
    // A caller by its file and name: call sites say which function they
    // are in by its name alone, so two callers of one name in one file
    // would count each other's calls.
    let callers: std::collections::BTreeSet<(&std::path::Path, &str)> = callers
        .iter()
        .map(|&c| {
            let node = g.node(c);
            (node.file.as_path(), node.name.as_str())
        })
        .collect();
    let mut sites = 0;
    for (caller_file, caller) in &callers {
        let Some(calls) = app.proj.calls_by_file.get(*caller_file) else {
            return Some(Leaning::Callers(g.callers_of(id).len()));
        };
        sites += calls
            .calls
            .iter()
            .filter(|s| s.callee == name && s.caller.as_deref() == Some(*caller))
            .count();
    }
    // The calls at the top of its own file, in no function: Python's
    // `if __name__ == "__main__": main()`, a script's own steps. They are
    // not in the graph, which links functions, and a `main` so called read
    // "0 references". Only a plain function's: a bare call is no method's.
    if g.node(id).kind == "function"
        && let Some(calls) = app.proj.calls_by_file.get(file)
    {
        sites += calls
            .calls
            .iter()
            .filter(|s| s.callee == name && s.caller.is_none() && !s.method)
            .count();
    }
    Some(Leaning::References(sites))
}

/// When lines `first..=last` (1-based) of the open file last changed, as
/// its blame has it: the newest of their commits — or `None` with no blame,
/// and "Uncommitted" (`Some(None)`) when one of them is not committed yet.
fn last_changed(v: &Viewer, first: usize, last: usize) -> Option<Option<i64>> {
    let git = v.git.as_ref()?;
    let lines = git
        .blame
        .get(first.saturating_sub(1)..last.min(git.blame.len()))?;
    if lines.is_empty() {
        return None;
    }
    if lines.iter().any(|b| b.uncommitted) {
        return Some(None);
    }
    Some(lines.iter().map(|b| b.time).max())
}

/// A commit's day, as a reader says it: "Nov 4", with its year when that
/// is not this one ("Nov 4, 2024"). In UTC.
pub(crate) fn short_date(time: i64, now: i64) -> String {
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    fn civil(t: i64) -> (i64, u32, u32) {
        let z = t.div_euclid(86_400) + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = yoe + era * 400 + i64::from(m <= 2);
        (y, m, d)
    }
    // The months' short names, in the reader's language where shown.
    const MONTHS: [&str; 12] = [
        tr_noop!("Jan"),
        tr_noop!("Feb"),
        tr_noop!("Mar"),
        tr_noop!("Apr"),
        tr_noop!("May"),
        tr_noop!("Jun"),
        tr_noop!("Jul"),
        tr_noop!("Aug"),
        tr_noop!("Sep"),
        tr_noop!("Oct"),
        tr_noop!("Nov"),
        tr_noop!("Dec"),
    ];
    let (y, m, d) = civil(time);
    let month = crate::i18n::lookup(MONTHS[(m - 1) as usize]);
    if y == civil(now).0 {
        tr!("{month} {day}", month = month, day = d)
    } else {
        tr!("{month} {day}, {year}", month = month, day = d, year = y)
    }
}

fn active_note<'a>(
    app: &'a App,
    name: String,
    meta: String,
    of: NoteOf<'a>,
) -> Element<'a, Message> {
    let code = matches!(of, NoteOf::Function { .. });
    let action = |glyph: Glyph, label: String, accent: bool, msg: Message| {
        let tint = if accent {
            theme::accent()
        } else {
            theme::fg_muted()
        };
        // A button lays its content out at its top: the row is given the
        // button's height to sit in the middle of.
        button(
            row![
                glyph::icon(glyph, tint, 13.0),
                text(label)
                    .size(NOTE_ACTION_SIZE)
                    .font(crate::fonts::in_control(if accent {
                        crate::fonts::UI_SEMIBOLD
                    } else {
                        crate::fonts::UI_MEDIUM
                    }))
                    .color(tint),
            ]
            .spacing(6)
            .height(Fill)
            .align_y(iced::Center),
        )
        .style(theme::icon_button(false))
        .height(NOTE_ACTION_H)
        .padding([0.0, NOTE_ACTION_PAD_X])
        .on_press(msg)
    };
    // How much of the project leans on it, and when it last changed — each
    // a way into the margin's tab that says more.
    let v = app.active_viewer();
    let (leaning, changed) = match (of, v) {
        (NoteOf::Function { sym, ordinal }, Some(v)) => (
            leaning_on(app, &v.abs, &sym.name, ordinal),
            last_changed(v, sym.line, sym.end_line),
        ),
        (NoteOf::File, Some(v)) => (None, last_changed(v, 1, usize::MAX)),
        _ => (None, None),
    };
    let calls_label = leaning.map_or_else(|| tr!("Calls").to_string(), Leaning::label);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let history_label = match changed {
        Some(Some(time)) => short_date(time, now),
        Some(None) => tr!("Uncommitted").to_string(),
        None => tr!("History").to_string(),
    };
    // Not explained yet: what explains it, first, where the note says so.
    let node = match (of, v) {
        (NoteOf::Function { sym, ordinal }, Some(v)) => Some(crate::explain::Node::Function {
            file: v.abs.clone(),
            name: sym.name.clone(),
            ordinal,
        }),
        (NoteOf::File, Some(v)) => Some(crate::explain::Node::File(v.abs.clone())),
        _ => None,
    };
    let unexplained =
        !app.proj.explain.running && node.is_some_and(|n| !app.proj.explain.cache.contains_key(&n));
    let mut actions = row![].spacing(2).align_y(iced::Center);
    if unexplained {
        actions = actions.push(action(
            Glyph::TextLines,
            tr!("Explain All").into(),
            true,
            super::toolbar::explain_all_message(app),
        ));
    }
    let actions = actions
        .push(action(
            Glyph::Network,
            calls_label,
            false,
            Message::Window(WindowMsg::MarginTabPicked(crate::MarginTab::Calls)),
        ))
        .push(action(
            Glyph::Clock,
            history_label,
            false,
            Message::Window(WindowMsg::MarginTabPicked(crate::MarginTab::History)),
        ))
        .push(action(
            Glyph::Message,
            tr!("Ask").into(),
            true,
            Message::Ask(AskMsg::Suggested(tr!(
                "What does `{name}` in {file} do, and why is it written this way?",
                name = name,
                file = app.active_viewer().map_or("", |v| v.rel.as_str())
            ))),
        ));
    let said = scrollable(
        Column::with_children(super::explain_view::render_note(
            app,
            &app.proj.explain.prepared,
        ))
        .spacing(6)
        .width(Fill),
    )
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Fill);
    // The actions run on to a second line when the card is too narrow for
    // them on one. Their icons line up with the text above: the buttons
    // reach out into the card's padding by their own.
    let actions = actions.wrap().vertical_spacing(2);
    let inset = iced::padding::left(NOTE_ACTION_PAD_X);
    container(
        column![
            container(column![note_head(name, meta, code, true), said].spacing(6))
                .padding(inset)
                .height(Fill),
            actions,
        ]
        .spacing(10),
    )
    .width(Fill)
    .height(Fill)
    // The design's 12 and 14, inside the edge iced draws in the card's box.
    .padding(Padding {
        top: 12.0 + NOTE_EDGE,
        right: 14.0 + NOTE_EDGE,
        bottom: 12.0 + NOTE_EDGE,
        left: 14.0 + NOTE_EDGE - NOTE_ACTION_PAD_X,
    })
    .style(theme::note_card)
    .into()
}

/// A raised note's actions: their text size, height, and the padding either
/// side of what they say.
const NOTE_ACTION_SIZE: f32 = 12.0;
const NOTE_ACTION_H: f32 = 26.0;
const NOTE_ACTION_PAD_X: f32 = 7.0;

/// The margin's Calls: who calls the function the caret is in, and what it
/// calls — each a jump — or what to do to see them.
fn margin_calls(app: &App) -> Element<'_, Message> {
    let node = app
        .proj
        .explain
        .view
        .as_ref()
        .filter(|n| matches!(n, crate::explain::Node::Function { .. }));
    let Some(node) = node else {
        return compact_empty_state(Glyph::CallGraph, tr!("Move the cursor into a function."));
    };
    let rows = call_flow_rows(app, node);
    let open = button(
        row![
            glyph::icon(Glyph::CallGraph, theme::fg_muted(), 14.0),
            text(tr!("Open the call hierarchy")).size(ts::BODY),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .style(theme::menu_row)
    .width(Fill)
    .padding([6, 8])
    .on_press(Message::Calls(CallsMsg::OfFunction));
    scrollable(
        column![Column::with_children(rows).spacing(8).width(Fill), open,]
            .spacing(10)
            .padding(Padding {
                top: 2.0,
                right: 12.0,
                bottom: 10.0,
                left: 12.0,
            }),
    )
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Fill)
    .into()
}

/// The margin's History: the ways into how this code came to be.
fn margin_history(app: &App) -> Element<'_, Message> {
    let in_function = matches!(
        app.proj.explain.view,
        Some(crate::explain::Node::Function { .. })
    );
    let row_of = |glyph: Glyph, title: &'static str, detail: &'static str, msg: Message| {
        button(
            row![
                container(glyph::icon(glyph, theme::fg_muted(), 15.0)).padding([2, 0]),
                column![
                    text(title).size(ts::BASE).color(theme::fg()),
                    text(detail).size(ts::SMALL).color(theme::dim()),
                ]
                .spacing(2),
            ]
            .spacing(10)
            .align_y(iced::alignment::Vertical::Top),
        )
        .style(theme::menu_row)
        .width(Fill)
        .padding([8, 10])
        .on_press(msg)
    };
    let mut rows = Column::new().spacing(2);
    if in_function {
        rows = rows.push(row_of(
            Glyph::TimeTravel,
            tr!("This function through time"),
            tr!("Step through its history, commit by commit."),
            Message::TimeTravel(TimeTravelMsg::Start { symbol: true }),
        ));
    }
    rows = rows
        .push(row_of(
            Glyph::TimeTravel,
            tr!("This file through time"),
            tr!("Step back through the whole file's commits."),
            Message::TimeTravel(TimeTravelMsg::Start { symbol: false }),
        ))
        .push(row_of(
            Glyph::Info,
            tr!("Why is this line here?"),
            tr!("The commits that shaped the current line, explained."),
            Message::TimeTravel(TimeTravelMsg::WhyIsThisHere),
        ))
        .push(row_of(
            Glyph::Diff,
            tr!("Changes since the last commit"),
            tr!("See what's new in this file."),
            Message::Editor(EditorMsg::ToggleDiff),
        ));
    container(rows)
        .padding(Padding {
            top: 2.0,
            right: 8.0,
            bottom: 8.0,
            left: 8.0,
        })
        .into()
}

/// A 1px horizontal divider spanning the panel width.
pub(crate) fn hairline() -> Element<'static, Message> {
    container(space().width(Fill).height(1))
        .width(Fill)
        .height(1)
        .style(|_: &iced::Theme| iced::widget::container::Style {
            background: Some(theme::hairline().into()),
            ..Default::default()
        })
        .into()
}

/// What an empty outline says: why there is nothing to list. A language
/// without an outline query, and one whose query does not compile (a grammar
/// bump broke it), are not files without symbols — the last used to read as
/// one, silently.
pub(crate) fn empty_outline_note(lang_key: Option<&str>) -> String {
    use crate::highlight::{Lang, tags_query};
    outline_note_for(
        lang_key
            .and_then(Lang::from_key)
            .map(|lang| tags_query(lang).map(|_| ())),
    )
}

/// [`empty_outline_note`] given how the language's outline query came out
/// (`None`: no language at all).
pub(crate) fn outline_note_for(query: Option<Result<(), crate::highlight::QueryError>>) -> String {
    use crate::highlight::QueryError;
    match query {
        Some(Err(QueryError::Invalid(e))) => {
            tr!(
                "Outline unavailable: the outline query does not compile ({error}).",
                error = e
            )
        }
        Some(Err(QueryError::Unsupported)) | None => tr!("No outline for this language.").into(),
        Some(Ok(())) => tr!("No symbols in this file.").into(),
    }
}

/// The Outline tab's content: the active file's symbols, click to jump.
pub(crate) fn outline_content(app: &App) -> Element<'_, Message> {
    let Some(v) = app.active_viewer() else {
        return space().into();
    };
    if v.symbols.is_empty() {
        return compact_empty_state(Glyph::List, empty_outline_note(v.lang_key));
    }
    // The symbol the reading cursor is currently inside, to highlight its row.
    let current = match &app.proj.explain.view {
        Some(crate::explain::Node::Function {
            file,
            name,
            ordinal,
        }) if *file == v.abs => Some((name.as_str(), *ordinal)),
        _ => None,
    };
    // Rebuilt on every repaint, so nothing below may scan a whole list per
    // symbol (that was quadratic: ~100 ms a frame on a file with thousands of
    // symbols). The same-name ordinals come from one pass, and this file's
    // notes are indexed by symbol once — the first note for a name wins, as
    // with `notes::find`.
    let ordinals = crate::outline::fn_ordinals(&v.symbols);
    let mut notes: std::collections::HashMap<&str, &crate::notes::Note> =
        std::collections::HashMap::new();
    for n in app.proj.notes.iter().filter(|n| n.rel == v.rel) {
        notes.entry(n.symbol.as_str()).or_insert(n);
    }
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for (symbol, &ordinal) in v.symbols.iter().zip(&ordinals) {
        let is_fn = matches!(symbol.kind.as_str(), "function" | "method");
        let is_current = is_fn && current == Some((symbol.name.as_str(), ordinal));
        // The reader's note/progress on this symbol (anchored by name, so it
        // follows the symbol across edits/re-scans).
        let note = notes.get(symbol.name.as_str()).copied();
        let understood = note.is_some_and(|n| n.understood);
        let has_text = note.is_some_and(|n| !n.text.is_empty());

        let label = row![
            text(short_kind(&symbol.kind))
                .size(ts::CAPTION)
                .color(theme::kind_color(&symbol.kind))
                .width(34),
            // Understood symbols dim, so the outline shows at a glance what's left.
            text(&symbol.name)
                .size(ts::BODY)
                .color(if understood {
                    theme::dim()
                } else {
                    theme::fg()
                })
                .wrapping(Wrapping::None),
        ]
        .spacing(4)
        .align_y(iced::Center);

        // Annotate each function/method with its one-line explanation, turning
        // the outline into a table of contents that says what each symbol does.
        // Same toggle and error-filter as the inline code summaries.
        let summary = if app.show_inline_summaries && is_fn {
            let node = crate::explain::Node::Function {
                file: v.abs.clone(),
                name: symbol.name.clone(),
                ordinal,
            };
            app.proj
                .explain
                .cache
                .get(&node)
                .filter(|c| !crate::explain::is_error_summary(&c.summary))
                .map(|c| crate::app::shown_summary(c).into_owned())
        } else {
            None
        };

        let mut col = Column::new().spacing(1).push(label);
        if let Some(full) = summary {
            // A one-line table-of-contents entry: the first sentence, truncated
            // with an ellipsis and clipped so it never wraps or overflows the
            // panel. The complete explanation shows in a bubble on hover.
            let clean = strip_backticks(&full);
            let one_line = truncate_ellipsis(&first_sentence(&clean), 52);
            let line = container(
                text(one_line)
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            )
            .clip(true)
            .width(Fill)
            .padding(Padding {
                top: 0.0,
                right: 6.0,
                bottom: 0.0,
                left: 44.0,
            });
            let bubble = container(text(clean).size(ts::SMALL).color(theme::fg()))
                .padding(Padding {
                    top: 6.0,
                    right: 9.0,
                    bottom: 6.0,
                    left: 9.0,
                })
                .max_width(TIP_MAX_W)
                .style(theme::modal_panel);
            col = col.push(super::tip::tip(line, bubble, tooltip::Position::Bottom).gap(4.0));
        }
        if let Some(n) = note.filter(|n| !n.text.is_empty()) {
            // The reader's own note, in accent so it's distinct from the
            // summary — on one line, as the summary is, the whole of it in a
            // bubble on hover: the outline's follow counts its rows' lines
            // (`App::outline_offset`), and a note wrapped over many threw it
            // off by as many.
            let first = n.text.lines().next().unwrap_or_default();
            let mut one_line = truncate_ellipsis(first, 52);
            if one_line == first && n.text.trim_end() != first {
                one_line.push('…');
            }
            let line = container(
                text(format!("\u{270e} {one_line}"))
                    .size(ts::CAPTION)
                    .color(theme::accent())
                    .wrapping(Wrapping::None),
            )
            .clip(true)
            .width(Fill)
            .padding(Padding {
                top: 0.0,
                right: 4.0,
                bottom: 0.0,
                left: 44.0,
            });
            let bubble = container(text(n.text.as_str()).size(ts::SMALL).color(theme::fg()))
                .padding(Padding {
                    top: 6.0,
                    right: 9.0,
                    bottom: 6.0,
                    left: 9.0,
                })
                .max_width(TIP_MAX_W)
                .style(theme::modal_panel);
            col = col.push(super::tip::tip(line, bubble, tooltip::Position::Bottom).gap(4.0));
        }

        // Leading "understood" toggle and trailing note pencil sit inside
        // the row — each a button of its own, which takes its own press
        // before the row's — so the row's band, and its hover, run across
        // the whole of it rather than starting after them.
        let (cg, gcolor) = if understood {
            (Glyph::CheckCircle, theme::accent())
        } else {
            (Glyph::Circle, theme::dim())
        };
        let toggle = button(glyph::icon(cg, gcolor, 13.0))
            .style(theme::icon_button(false))
            .padding([4, 4])
            .on_press(Message::Reading(ReadingMsg::NoteToggleUnderstood {
                rel: v.rel.clone(),
                symbol: symbol.name.clone(),
            }));
        let pencil = button(glyph::icon(
            Glyph::Edit,
            if has_text {
                theme::accent()
            } else {
                theme::dim()
            },
            12.0,
        ))
        .style(theme::icon_button(false))
        .padding([4, 4])
        .on_press(Message::Reading(ReadingMsg::NoteEditStart {
            rel: v.rel.clone(),
            symbol: symbol.name.clone(),
        }));
        // Top-align so the toggle circle and pencil sit on the kind-badge/name
        // line rather than floating in the middle of the multi-line row.
        let jump = button(
            row![toggle, col.width(Fill), pencil]
                .spacing(2)
                .align_y(iced::alignment::Vertical::Top),
        )
        .style(theme::list_row(is_current))
        .width(Fill)
        .padding(Padding {
            top: 2.0,
            right: 2.0,
            bottom: 2.0,
            left: 2.0,
        })
        .on_press(Message::Editor(EditorMsg::OutlineJump(symbol.line)));
        rows.push(jump.into());
    }

    // Sub-label under "OUTLINE": the reader's manual "understood" coverage for
    // this file. Plain text, left-aligned with the section header — no decorative
    // leading circle (that read as a stray, non-clickable control). Explain-All
    // progress lives only in the status bar, so it isn't duplicated here.
    let names: Vec<String> = v.symbols.iter().map(|s| s.name.clone()).collect();
    let (done, total) = crate::notes::coverage(&app.proj.notes, &v.rel, &names);
    let header_content: Element<'_, Message> =
        text(tr!("{done}/{total} understood", done = done, total = total))
            .size(ts::SMALL)
            .color(theme::fg_muted())
            .into();
    let header = container(header_content).padding(Padding {
        top: 2.0,
        right: 10.0,
        bottom: 4.0,
        left: 10.0,
    });

    // The wrapping column must be Fill so the scrollable has a bounded height to
    // scroll within — otherwise it grows to its content and never scrolls (which
    // made long outlines like main.rs's 224 symbols un-navigable).
    column![
        header,
        scrollable(Column::with_children(rows).width(Fill))
            .id(outline_scroll_id())
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .height(Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E2-10: a diff line's width counts what is drawn — tabs at the reader's
    /// tab width, CJK and emoji as two columns, combining marks as none.
    #[test]
    fn display_columns_follow_the_drawn_text() {
        assert_eq!(display_cols("abc"), 3);
        assert_eq!(display_cols("\tx"), TAB_COLS + 1);
        assert_eq!(display_cols("数据"), 4);
        assert_eq!(display_cols("😀"), 2);
        assert_eq!(display_cols("e\u{301}"), 1);
        assert_eq!(display_cols(""), 0);
    }

    /// E2-5: a diff is prepared for the view once, when it arrives: its
    /// runs of one kind joined (tabs expanded, blank lines kept a space
    /// wide), its widest drawn line counted — the view used to redo both for
    /// up to 8000 lines on every repaint — and at most `MAX_DIFF_ROWS` lines.
    #[test]
    fn diff_rows_are_as_wide_as_the_widest_drawn_line() {
        use crate::git::{DiffKind, DiffLine};
        let line = |kind, text: &str| DiffLine {
            kind,
            text: text.into(),
        };
        let d = crate::DiffState::new(
            std::path::PathBuf::from("/p/a.rs"),
            "a.rs".into(),
            vec![
                line(DiffKind::Context, "ab"),
                line(DiffKind::Context, ""),
                line(DiffKind::Add, "数据库"),
                line(DiffKind::Add, "\tx"),
                line(DiffKind::Remove, "gone"),
            ],
        );
        // Widest: 3 CJK = 6 columns.
        assert_eq!(d.max_cols, 6);
        let runs: Vec<(DiffKind, &str)> =
            d.runs.iter().map(|r| (r.kind, r.text.as_str())).collect();
        assert_eq!(
            runs,
            [
                (DiffKind::Context, "ab\n "),
                (DiffKind::Add, "数据库\n    x"),
                (DiffKind::Remove, "gone"),
            ]
        );
        // + 1 of slack, at 7 px, + 18 px padding.
        assert_eq!(row_width(d.max_cols, 7.0, 18.0), 7.0 * 7.0 + 18.0);
        assert_eq!(row_width(0, 7.0, 18.0), 7.0 + 18.0);
        let long = crate::DiffState::new(
            std::path::PathBuf::from("/p/a.rs"),
            "a.rs".into(),
            (0..MAX_DIFF_ROWS + 5)
                .map(|_| line(DiffKind::Add, "+"))
                .collect(),
        );
        assert_eq!(long.runs[0].text.lines().count(), MAX_DIFF_ROWS);
    }

    #[test]
    fn diff_text_expands_tabs_and_keeps_blank_lines_tall() {
        assert_eq!(diff_display_text(""), " ");
        assert_eq!(
            diff_display_text("\tx"),
            format!("{}x", " ".repeat(TAB_COLS))
        );
        assert_eq!(diff_display_text("+ok"), "+ok");
        // A line of nothing but a `\r` (from a `\r\r\n` end) is a blank line.
        assert_eq!(diff_display_text("\r"), " ");
        // A direction control is drawn as U+FFFD, a column it took none of:
        // the row is as wide as what is drawn, so its tail can be scrolled
        // to.
        let line = "+a\u{2066}\u{2066}\u{2066}b".to_string();
        let diff = crate::DiffState::new(
            "/p/a.rs".into(),
            "a.rs".into(),
            vec![crate::git::DiffLine {
                kind: crate::git::DiffKind::Add,
                text: line,
            }],
        );
        // Its `+` is drawn in a column of its own, not in the line.
        assert_eq!(diff.max_cols, 5);
    }

    /// A diff reads as code does: git's headers are not drawn, a hunk is a
    /// quiet line naming where it is, each line has its numbers before and
    /// after and its mark apart, and the code is coloured as the editor
    /// colours it. It was git's raw text — `diff --git`, `index`,
    /// `@@ -10,2 +10,3 @@` — in one colour per kind.
    #[test]
    fn a_diff_reads_as_code_does() {
        use crate::git::{DiffKind, DiffLine};
        let line = |kind, text: &str| DiffLine {
            kind,
            text: text.into(),
        };
        let d = crate::DiffState::new(
            std::path::PathBuf::from("/p/src/main.rs"),
            "src/main.rs".into(),
            vec![
                line(DiffKind::Header, "diff --git a/src/main.rs b/src/main.rs"),
                line(DiffKind::Header, "index 1a2b3c..4d5e6f 100644"),
                line(DiffKind::Header, "--- a/src/main.rs"),
                line(DiffKind::Header, "+++ b/src/main.rs"),
                line(DiffKind::Hunk, "@@ -10,2 +10,2 @@ fn main() {"),
                line(DiffKind::Context, "    let x = 1;"),
                line(DiffKind::Remove, "-    println!(\"old\");"),
                line(DiffKind::Add, "+    println!(\"new\");"),
            ],
        );
        let kinds: Vec<DiffKind> = d.runs.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                DiffKind::Hunk,
                DiffKind::Context,
                DiffKind::Remove,
                DiffKind::Add
            ]
        );
        assert!(d.runs.iter().all(|r| !r.text.contains("diff --git")));
        assert_eq!(d.runs[0].text, "⋯  fn main() {");
        assert_eq!(d.runs[1].gutter, "10 10");
        assert_eq!(d.runs[2].gutter, "11   ");
        assert_eq!(d.runs[3].gutter, "   11");
        assert_eq!(d.runs[2].marks, "−");
        assert_eq!(d.runs[3].marks, "+");
        assert_eq!(d.runs[3].text, "    println!(\"new\");");
        // Coloured: the string is a string.
        assert!(
            d.runs[3]
                .spans
                .iter()
                .any(|(t, s)| t.contains("new") && s.is_some()),
            "{:?}",
            d.runs[3].spans
        );
        // A hunk git names no place for.
        let bare = crate::DiffState::new(
            std::path::PathBuf::from("/p/a.txt"),
            "a.txt".into(),
            vec![
                line(DiffKind::Hunk, "@@ -1 +1 @@"),
                line(DiffKind::Add, "+x"),
            ],
        );
        assert_eq!(bare.runs[0].text, "⋯");
        assert_eq!(bare.runs[1].gutter, "  1");
    }

    /// The advance is measured by the text engine, not assumed at 0.6 em —
    /// the engine answers, and what the diff uses is that answer (a range
    /// check alone admitted the 0.6 em fallback) — and repeated asks for one
    /// size agree.
    #[test]
    fn the_monospace_advance_is_measured() {
        let measured =
            measured_mono_advance(13.0).expect("the text engine measures the monospace face");
        assert_eq!(mono_advance(13.0), measured);
        assert!(
            measured > 13.0 * 0.3 && measured < 13.0,
            "implausible advance {measured}"
        );
        assert_eq!(mono_advance(13.0), measured);
        assert!(mono_advance(26.0) > measured);
    }
}
