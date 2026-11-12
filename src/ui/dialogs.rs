//! Input modals (breakpoint/bookmark/note), why panel, and the finder.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::row;

/// Pitch of the finder's result rows: a row and the gap under it. Fixed so
/// the key handler can compute where the selected row sits in the list and
/// scroll it into view.
pub(crate) const FINDER_ROW_H: f32 = 39.0;
/// The gap between the list's rows (and headings), inside their pitch.
const FINDER_ROW_GAP: f32 = 1.0;
/// What a finder row has inside it to lay its parts in: the panel less its
/// border, the list's padding and the row's own (the scrollbar floats in
/// the list's padding, clear of the rows' text).
const FINDER_ROW_INNER_W: f32 = super::chrome::FINDER_W - 2.0 - 2.0 * 6.0 - 2.0 * 10.0;
/// A file row's icon, and the gap between its parts.
const FILE_ICON_W: f32 = 15.0;
const FILE_ROW_GAP: f32 = 11.0;
/// A symbol row's kind column, and the gap between its parts.
const SYMBOL_KIND_W: f32 = 40.0;
const SYMBOL_ROW_GAP: f32 = 10.0;
/// Height of a heading over a group of the command bar's suggestions.
pub(crate) const FINDER_HEAD_H: f32 = 29.0;
/// Size of a command bar row's detail (a symbol's file and line).
const BAR_DETAIL_SIZE: f32 = 12.0;
/// Height of the finder's query row, over its results.
const FINDER_INPUT_H: f32 = 52.0;
/// The command bar's corners, as the design rounds them.
const FINDER_RADIUS: f32 = 12.0;

/// Modal to set a breakpoint's condition — the program only stops there when the
/// expression is true. Empty condition sets a plain breakpoint.
pub(crate) fn bp_condition_modal<'a>(
    app: &'a App,
    edit: &'a (std::path::PathBuf, usize, String),
) -> Element<'a, Message> {
    let (path, line, draft) = edit;
    prompt_modal(Prompt {
        title: tr!(
            "Break at {location} when…",
            location = format!("{}:{}", rel_of(app, path), line)
        ),
        placeholder: tr!("e.g. i == 3"),
        value: draft,
        input_id: bp_condition_input_id(),
        on_input: |v| Message::Debug(DebugMsg::BpConditionInput(v)),
        submit: Message::Debug(DebugMsg::BpConditionSet),
        // With no condition, what it does: "Set" silently made a plain
        // breakpoint of an empty condition.
        submit_label: if draft.trim().is_empty() {
            tr!("Set without condition")
        } else {
            tr!("Set")
        },
        cancel: Message::Debug(DebugMsg::BpConditionCancel),
        // A breakpoint already there can be taken away where its condition
        // is: the dialog offered only to set one.
        remove: app
            .debug
            .breakpoints
            .get(path)
            .is_some_and(|m| m.contains_key(line))
            .then(|| {
                (
                    tr!("Remove breakpoint"),
                    Message::Debug(DebugMsg::BpConditionRemove),
                )
            }),
    })
}

/// Modal to attach an optional plain-text note to a bookmark.
pub(crate) fn bookmark_note_modal(edit: &(String, usize, String)) -> Element<'_, Message> {
    let (rel, line, draft) = edit;
    prompt_modal(Prompt {
        title: tr!("Note for {location}", location = format!("{rel}:{line}")),
        placeholder: tr!("a short note to your future self…"),
        value: draft,
        input_id: note_input_id(),
        on_input: |v| Message::Reading(ReadingMsg::BookmarkNoteInput(v)),
        submit: Message::Reading(ReadingMsg::BookmarkNoteSave),
        submit_label: tr!("Save"),
        cancel: Message::Reading(ReadingMsg::BookmarkNoteCancel),
        remove: None,
    })
}

/// Modal to attach an optional plain-text reading note to a symbol.
pub(crate) fn reading_note_modal(edit: &(String, String, String)) -> Element<'_, Message> {
    let (rel, symbol, draft) = edit;
    prompt_modal(Prompt {
        title: tr!("Note on {symbol} in {file}", symbol = symbol, file = rel),
        placeholder: tr!("what you worked out about this symbol…"),
        value: draft,
        input_id: note_input_id(),
        on_input: |v| Message::Reading(ReadingMsg::NoteEditInput(v)),
        submit: Message::Reading(ReadingMsg::NoteEditSave),
        submit_label: tr!("Save"),
        cancel: Message::Reading(ReadingMsg::NoteEditCancel),
        remove: None,
    })
}

/// Where the "why is this here?" popup opens: under the point it was
/// asked from — the right-click on the line, a line's height clear of it —
/// or, with too little room under it for the popup, over it; asked from no
/// point, near the top of the window.
pub(crate) fn why_placement(app: &App, anchor: Option<f32>) -> Placement {
    let Some(y) = anchor else {
        return Placement::Top(WHY_TOP);
    };
    let clear = app.line_height();
    let floor = app.window_height - STATUSBAR_H;
    if y + clear + WHY_ROOM <= floor {
        Placement::Top(y + clear)
    } else {
        Placement::Bottom((app.window_height - y + clear).max(STATUSBAR_H))
    }
}

/// The least room the popup is opened into under the line: its title, its
/// commits and a few lines of answer.
const WHY_ROOM: f32 = 220.0;

/// The "Why is this here?" popup: the git-grounded explanation of why a line or
/// selection exists, with the commit(s) it cites. Async — "Thinking…" until the
/// answer lands.
pub(crate) fn why_modal<'a>(app: &'a App, bw: &'a crate::BlameWhy) -> Element<'a, Message> {
    let mut col = Column::new()
        .spacing(8)
        .push(text(bw.title.clone()).size(ts::EMPHASIS).color(theme::fg()));
    // The commits it's grounded in.
    for (sha, subject) in &bw.commits {
        col = col.push(
            row![
                text(sha.clone())
                    .size(ts::SMALL)
                    .font(crate::fonts::CODE)
                    .color(theme::accent()),
                text(truncate_ellipsis(subject, 52))
                    .size(ts::SMALL)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            ]
            .spacing(8),
        );
    }
    col = col.push(hairline());
    let body: Element<'_, Message> = if bw.loading {
        text(tr!("Thinking…"))
            .size(ts::BODY)
            .color(theme::dim())
            .into()
    } else {
        Column::with_children(render_prepared(app, &bw.prepared))
            .spacing(8)
            .width(Fill)
            .into()
    };
    col = col
        .push(
            scrollable(container(body).width(Fill))
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(Length::Shrink),
        )
        .push(
            row![space().width(Fill)]
                // Failed: asked again from here — it offered only Settings
                // and Close — and, for what Settings puts right, the way
                // there, as every such failure has.
                .extend(bw.failed.then(|| {
                    Element::from(
                        button(text(tr!("Try again")).size(ts::BODY))
                            .style(theme::toolbar_button)
                            .padding([4, 12])
                            .on_press(super::heard::said(
                                tr!("Try again"),
                                Message::TimeTravel(TimeTravelMsg::BlameWhyAgain),
                            )),
                    )
                }))
                .extend(bw.fix.map(super::panes::settings_fix_button))
                .push(
                    button(text(tr!("Close")).size(ts::BODY))
                        .style(theme::toolbar_button)
                        .padding([4, 12])
                        .on_press(super::heard::said(
                            tr!("Close"),
                            Message::TimeTravel(TimeTravelMsg::BlameWhyClose),
                        )),
                )
                .spacing(6)
                .align_y(iced::Center),
        );

    let panel = container(col)
        .width(WHY_W)
        .max_height(WHY_MAX_H)
        .padding(PROMPT_PAD)
        .style(theme::modal_panel);
    modal(
        panel,
        why_placement(app, bw.anchor),
        Backdrop::Dim(Some(Message::TimeTravel(TimeTravelMsg::BlameWhyClose))),
    )
}

/// The popup of a language's answer about the code under the cursor (a
/// macro's expansion, a Lean goal, a Haskell type), with a way to where it
/// points. "Asking…" until the answer lands.
pub(crate) fn insight_modal<'a>(app: &'a App, insight: &'a crate::Insight) -> Element<'a, Message> {
    let body: Element<'_, Message> = if insight.loading {
        text(tr!("Asking…"))
            .size(ts::BODY)
            .color(theme::dim())
            .into()
    } else {
        Column::with_children(render_prepared(app, &insight.prepared))
            .spacing(8)
            .width(Fill)
            .into()
    };
    let mut buttons = row![space().width(Fill)].spacing(8);
    if insight.defined.is_some() {
        buttons = buttons.push(
            button(text(tr!("Open definition")).size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([4, 12])
                .on_press(super::heard::said(
                    tr!("Open definition"),
                    Message::Hover(HoverMsg::InsightOpenDefinition),
                )),
        );
    }
    buttons = buttons.push(
        button(text(tr!("Close")).size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([4, 12])
            .on_press(super::heard::said(
                tr!("Close"),
                Message::Hover(HoverMsg::InsightClose),
            )),
    );
    let col = Column::new()
        .spacing(8)
        .push(
            text(insight.title.clone())
                .size(ts::EMPHASIS)
                .color(theme::fg()),
        )
        .push(hairline())
        .push(
            scrollable(container(body).width(Fill))
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(Length::Shrink),
        )
        .push(buttons);
    let panel = container(col)
        .width(WHY_W)
        .max_height(WHY_MAX_H)
        .padding(PROMPT_PAD)
        .style(theme::modal_panel);
    modal(
        panel,
        why_placement(app, insight.anchor),
        Backdrop::Dim(Some(Message::Hover(HoverMsg::InsightClose))),
    )
}

// ---------------------------------------------------------------- finder modal

pub(crate) fn finder_modal(app: &App) -> Element<'_, Message> {
    let placeholder = match app.proj.finder.mode {
        _ if app.proj.finder.bar => tr!("Run a command or open a file"),
        // A question typed here is offered to Ask: said, as the rest is.
        FinderMode::Files => {
            tr!("Open a file, ask a question, or type > for a command, :123 for a line")
        }
        FinderMode::Symbols => tr!("Go to a symbol"),
    };
    let glyph = match app.proj.finder.mode {
        FinderMode::Symbols if !app.proj.finder.is_commands() => Glyph::Cube,
        _ => Glyph::Search,
    };
    let input = container(
        row![
            glyph::icon(glyph, theme::dim(), 16.0),
            ts::field_in(
                text_input(placeholder, &app.proj.finder.query)
                    .id(finder_input_id())
                    .on_input({
                        // What is said that names no row is typed in it.
                        super::heard::field(&app.proj.finder.query, |v| {
                            Message::Nav(NavMsg::FinderQueryChanged(v))
                        });
                        |v| Message::Nav(NavMsg::FinderQueryChanged(v))
                    })
                    .on_submit(Message::Nav(NavMsg::FinderConfirm))
                    .style(theme::bare_input),
                FINDER_INPUT_H,
                ts::INTER,
                ts::SUBTITLE,
            ),
        ]
        .spacing(11)
        .align_y(iced::Center),
    )
    .height(Length::Fixed(FINDER_INPUT_H))
    .align_y(iced::Center)
    .padding(Padding {
        top: 0.0,
        right: 12.0,
        bottom: 0.0,
        left: 16.0,
    });

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    // Its rows a list voice counts down (`heard::row`).
    super::heard::list();
    if app.proj.finder.is_commands() {
        finder_command_rows(app, &mut rows);
    } else if let Some(n) = app.proj.finder.goto_line() {
        // Past the end: where Enter goes, the last line, said as such.
        let last = app.active_viewer().map(|v| v.lines.len().max(1));
        let said = match last {
            Some(last) if n > last => tr!("Go to line {n}, the last line", n = last),
            _ => tr!("Go to line {n}", n = n),
        };
        rows.push(
            container(
                text(format!("↵  {said}"))
                    .size(ts::BASE)
                    .color(theme::accent()),
            )
            .padding(8)
            .into(),
        );
    } else if app.proj.finder.is_line_query() {
        // Go to Line before its number: what to type, not the project's
        // files (Enter took the first of them).
        rows.push(
            container(
                text(tr!("Type a line number"))
                    .size(ts::BODY)
                    .color(theme::dim()),
            )
            .padding(8)
            .into(),
        );
    } else if app.proj.finder.is_home() {
        suggestion_rows(app, &mut rows);
    } else if app.proj.finder.is_bar_search() {
        bar_rows(app, &mut rows);
    } else {
        match app.proj.finder.mode {
            FinderMode::Files => finder_file_rows(app, &mut rows),
            FinderMode::Symbols => finder_symbol_rows(app, &mut rows),
        }
    }
    if rows.is_empty() {
        let hint = if app.proj.finder.is_commands() {
            tr!("No matching command")
        } else if app.proj.finder.mode == FinderMode::Symbols && app.proj.indexing {
            tr!("Indexing symbols…")
        } else {
            tr!("No matches")
        };
        rows.push(
            container(text(hint).size(ts::BODY).color(theme::dim()))
                .padding(8)
                .into(),
        );
    }

    // Symbols come from the project index: when its caps left files out, the
    // symbol a reader is looking for may simply not be in it — say so under
    // the list (outside it, so row `i` still starts at `i * FINDER_ROW_H`).
    let cap_note: Element<'_, Message> = match index_cap_note(app) {
        Some(note) if app.proj.finder.mode == FinderMode::Symbols => container(
            row![
                glyph::icon(Glyph::Info, theme::fg_muted(), 12.0),
                text(tr!("Index incomplete: {note}", note = note))
                    .size(ts::CAPTION)
                    .color(theme::dim()),
            ]
            .spacing(6)
            .align_y(iced::Center),
        )
        .padding(Padding {
            top: 0.0,
            right: 16.0,
            bottom: 10.0,
            left: 16.0,
        })
        .width(Fill)
        .into(),
        _ => slot(),
    };
    let panel = container(iced::widget::column![
        input,
        hairline(),
        // The design's gap between what the list holds, and no padding
        // inside it: row `i` starts at exactly `i * FINDER_ROW_H` (its
        // height and the gap after it) for `reveal_finder_selection`.
        // Capped so the panel never runs past the window.
        container(
            scrollable(
                Column::with_children(rows)
                    .spacing(FINDER_ROW_GAP)
                    .width(Fill),
            )
            .id(finder_list_id())
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(iced::Length::Shrink),
        )
        .max_height(FINDER_MAX_H - FINDER_INPUT_H - 24.0)
        .padding(6),
        cap_note,
    ])
    .width(FINDER_W)
    .max_height(FINDER_MAX_H)
    // The border is drawn over the panel's edge; what is in it starts inside.
    .padding(1)
    .clip(true)
    .style(theme::modal_panel_rounded(FINDER_RADIUS));

    modal(
        panel,
        Placement::Top(FINDER_TOP),
        Backdrop::Dim(Some(Message::Nav(NavMsg::FinderClosed))),
    )
}

/// One finder result: a fixed-height, full-width button. `pick` opens what
/// the row shows — its target itself, not an index into a list a rescan or a
/// re-index replaces.
fn finder_row<'a>(
    content: impl Into<Element<'a, Message>>,
    selected: bool,
    label: String,
    pick: Message,
) -> Element<'a, Message> {
    // Said by what it shows, or by its place in the list.
    let pick = super::heard::row(move || label, pick);
    button(
        container(content)
            .height(Fill)
            .align_y(iced::alignment::Vertical::Center),
    )
    .style(theme::finder_row(selected))
    .width(Fill)
    .height(Length::Fixed(FINDER_ROW_H - FINDER_ROW_GAP))
    .padding([0, 10])
    .on_press(pick)
    .into()
}

/// A heading over a group of the command bar's suggestions.
fn finder_heading<'a>(title: &'static str) -> Element<'a, Message> {
    // The design's 10 over it and 4 under, in its pitch less the list's gap.
    container(
        text(title)
            .size(ts::SMALL)
            .line_height(ts::normal_line(ts::SMALL))
            .font(crate::fonts::UI_MEDIUM)
            .color(theme::dim()),
    )
    .height(Length::Fixed(FINDER_HEAD_H - FINDER_ROW_GAP))
    .padding(Padding {
        top: 10.0,
        right: 10.0,
        bottom: 4.0,
        left: 10.0,
    })
    .into()
}

/// A row of the command bar: its icon, what it does, a detail, its keys.
fn bar_row<'a>(
    glyph: Glyph,
    tint: iced::Color,
    title: String,
    detail: Option<String>,
    keys: Option<String>,
    selected: bool,
    pick: Message,
) -> Element<'a, Message> {
    // Each line on the design's baseline in the row.
    let row_h = FINDER_ROW_H - FINDER_ROW_GAP;
    let detail: Element<'a, Message> = match detail {
        Some(detail) => ts::set_in(
            text(detail).color(theme::dim()).wrapping(Wrapping::None),
            row_h,
            ts::INTER,
            BAR_DETAIL_SIZE,
        ),
        None => slot(),
    };
    let keys: Element<'a, Message> = match keys {
        Some(keys) => super::toolbar::row_keycap(keys),
        None => slot(),
    };
    let label = title.clone();
    finder_row(
        row![
            glyph::icon(glyph, tint, 15.0),
            container(ts::set_in(
                text(title).wrapping(Wrapping::None),
                row_h,
                ts::INTER,
                ts::READING,
            ))
            .width(Fill),
            detail,
            keys,
        ]
        .spacing(11)
        .align_y(iced::Center),
        selected,
        label,
        pick,
    )
}

/// The command bar with nothing typed: its suggestions, under headings.
fn suggestion_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    use crate::app::palette::Suggestion;
    let enter = if crate::keymap::MAC_KEYS {
        "↩"
    } else {
        "Enter"
    };
    let chord = |action| app.keymap.chord(action).caps();
    let mut group = "";
    for (pos, suggestion) in app.palette_home.iter().enumerate() {
        if suggestion.group() != group {
            group = suggestion.group();
            rows.push(finder_heading(group));
        }
        let (glyph, tint, title, detail, keys) = match suggestion {
            Suggestion::Ask(title) => (
                Glyph::AskAi,
                theme::accent(),
                title.clone(),
                None,
                Some(enter.to_string()),
            ),
            Suggestion::GoToFile => (
                Glyph::File,
                theme::dim(),
                tr!("Go to File…").to_string(),
                None,
                Some(chord(crate::keymap::Action::OpenFile)),
            ),
            Suggestion::Symbol {
                name, file, line, ..
            } => (
                Glyph::Braces,
                theme::dim(),
                name.clone(),
                Some(tr!("{file}, line {line}", file = file, line = line)),
                None,
            ),
            Suggestion::Command { title, recent } => {
                let command = app.palette.iter().find(|c| &c.title == title);
                // What uses the AI model is marked so here too, as when
                // typed for: a recent Explain looked like any command.
                let (glyph, tint) = if command.is_some_and(|c| c.spends_ai()) {
                    (Glyph::Sparkle, theme::accent())
                } else {
                    let glyph = match title.as_str() {
                        _ if *recent => Glyph::TimeTravel,
                        "Open Remote…" => Glyph::Remote,
                        "Open Folder…" => Glyph::FolderOpen,
                        _ => Glyph::Prompt,
                    };
                    (glyph, theme::dim())
                };
                (
                    glyph,
                    tint,
                    crate::i18n::lookup(title).to_string(),
                    None,
                    command.and_then(|c| c.keys.clone()),
                )
            }
        };
        rows.push(bar_row(
            glyph,
            tint,
            title,
            detail,
            keys,
            pos == app.proj.finder.selected,
            Message::Nav(NavMsg::PaletteSuggestion(suggestion.clone())),
        ));
    }
}

/// The command bar with something typed: the commands it matches, then the
/// files.
fn bar_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    let files = app
        .proj
        .project
        .as_ref()
        .map(|p| &p.files[..])
        .unwrap_or(&[]);
    for (pos, &idx) in app.proj.finder.results.iter().enumerate() {
        let selected = pos == app.proj.finder.selected;
        if let Some(command) = app.palette.get(idx) {
            // What uses the AI model is marked so: a typed "expl" put
            // Explain first, looking like any other command.
            let (glyph, tint) = if command.spends_ai() {
                (Glyph::Sparkle, theme::accent())
            } else {
                (Glyph::Prompt, theme::dim())
            };
            rows.push(bar_row(
                glyph,
                tint,
                command.shown().to_string(),
                None,
                command.keys.clone(),
                selected,
                Message::Nav(NavMsg::PaletteRun(command.title.clone())),
            ));
            continue;
        }
        // Past the files, the symbols the file picker found (Ctrl+P, a
        // question typed: what it found, then the way to ask it).
        if let Some(symbol) = (idx - app.palette.len()).checked_sub(files.len()) {
            rows.push(match app.proj.symbol_index.get(symbol) {
                Some(entry) => symbol_row(entry, selected),
                None => stale_finder_row(selected),
            });
            continue;
        }
        let Some(entry) = files.get(idx - app.palette.len()) else {
            rows.push(stale_finder_row(selected));
            continue;
        };
        let (dir, name) = match entry.rel.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", entry.rel.as_str()),
        };
        // Its folder in what the row leaves beside its icon, name and the
        // keys' place, cut at the start.
        let dir_room = FINDER_ROW_INNER_W
            - FILE_ICON_W
            - 3.0 * FILE_ROW_GAP
            - super::steady::width_of(name, ts::READING, crate::fonts::UI);
        let dir = super::steady::elide_start(dir, BAR_DETAIL_SIZE, crate::fonts::UI, dir_room);
        rows.push(bar_row(
            Glyph::File,
            theme::dim(),
            crate::highlight::in_order(name).into_owned(),
            Some(crate::highlight::in_order(&dir).into_owned()),
            None,
            selected,
            Message::Nav(NavMsg::FinderPick {
                abs: entry.abs.clone(),
                line: None,
                name: None,
            }),
        ));
    }
}

/// The row of a result whose index no longer names an entry (its list was
/// replaced after the finder ranked it): drawn, inert, so row `i` stays the
/// `i`th result — skipping it put the highlight and the scroll-into-view
/// (`reveal_finder_selection`, which places row `i` at `i * FINDER_ROW_H`)
/// one row off for everything after it.
fn stale_finder_row<'a>(selected: bool) -> Element<'a, Message> {
    button(
        container(
            text(tr!("— no longer in the project —"))
                .size(ts::SMALL)
                .color(theme::dim()),
        )
        .height(Fill)
        .align_y(iced::alignment::Vertical::Center),
    )
    .style(theme::finder_row(selected))
    .width(Fill)
    .height(Length::Fixed(FINDER_ROW_H - FINDER_ROW_GAP))
    .padding([0, 10])
    .into()
}

pub(crate) fn finder_file_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    let Some(project) = &app.proj.project else {
        return;
    };
    for (pos, &idx) in app.proj.finder.results.iter().enumerate() {
        let selected = pos == app.proj.finder.selected;
        // Past the files, the symbols the query named.
        if let Some(symbol) = idx.checked_sub(project.files.len()) {
            rows.push(match app.proj.symbol_index.get(symbol) {
                Some(entry) => symbol_row(entry, selected),
                None => stale_finder_row(selected),
            });
            continue;
        }
        let Some(entry) = project.files.get(idx) else {
            rows.push(stale_finder_row(selected));
            continue;
        };
        let (dir, name) = match entry.rel.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", entry.rel.as_str()),
        };
        // Its folder in what the row has left beside its name, cut at the
        // start: the end of a path says where it is.
        let dir_room = FINDER_ROW_INNER_W
            - FILE_ICON_W
            - 3.0 * FILE_ROW_GAP
            - super::steady::width_of(name, ts::READING, crate::fonts::UI);
        let dir = super::steady::elide_start(dir, ts::SMALL, crate::fonts::UI, dir_room);
        rows.push(finder_row(
            row![
                glyph::icon(Glyph::File, theme::dim(), FILE_ICON_W),
                // In the order they are spelled (`highlight::in_order`).
                text(crate::highlight::in_order(name)).size(ts::READING),
                space().width(Fill),
                text(crate::highlight::in_order(&dir).into_owned())
                    .size(ts::SMALL)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            ]
            .spacing(FILE_ROW_GAP)
            .align_y(iced::Center),
            pos == app.proj.finder.selected,
            entry.rel.clone(),
            Message::Nav(NavMsg::FinderPick {
                abs: entry.abs.clone(),
                line: None,
                name: None,
            }),
        ));
    }
}

/// The palette's commands, each with its key on the right.
pub(crate) fn finder_command_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    for (pos, &idx) in app.proj.finder.results.iter().enumerate() {
        let Some(command) = app.palette.get(idx) else {
            rows.push(stale_finder_row(pos == app.proj.finder.selected));
            continue;
        };
        let keys: Element<'_, Message> = match &command.keys {
            Some(keys) => super::toolbar::row_keycap(keys.clone()),
            None => slot(),
        };
        // What spends an AI call carries the sparkle here as in the
        // command bar: ">" listed Explain and Explain All as plain commands.
        let title: Element<'_, Message> = if command.spends_ai() {
            super::toolbar::ai_words(command.shown(), ts::READING, theme::fg())
        } else {
            text(command.shown().to_string()).size(ts::READING).into()
        };
        rows.push(finder_row(
            row![title, space().width(Fill), keys,]
                .spacing(10)
                .align_y(iced::Center),
            pos == app.proj.finder.selected,
            command.shown().to_string(),
            Message::Nav(NavMsg::PaletteRun(command.title.clone())),
        ));
    }
}

pub(crate) fn finder_symbol_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    for (pos, &idx) in app.proj.finder.results.iter().enumerate() {
        let selected = pos == app.proj.finder.selected;
        rows.push(match app.proj.symbol_index.get(idx) {
            Some(entry) => symbol_row(entry, selected),
            None => stale_finder_row(selected),
        });
    }
}

/// A symbol in the finder: its kind, its name, where it is.
fn symbol_row<'a>(entry: &'a crate::index::SymbolEntry, selected: bool) -> Element<'a, Message> {
    // Where it is, in what the row has left beside its kind and name, cut
    // at the start.
    let place_room = FINDER_ROW_INNER_W
        - SYMBOL_KIND_W
        - 2.0 * SYMBOL_ROW_GAP
        - super::steady::width_of(&entry.name, ts::READING, crate::fonts::UI);
    let place = super::steady::elide_start(
        &format!("{}:{}", entry.rel, entry.line),
        ts::SMALL,
        crate::fonts::UI,
        place_room,
    );
    finder_row(
        row![
            text(short_kind(&entry.kind))
                .size(ts::CAPTION)
                .color(theme::kind_color(&entry.kind))
                .width(SYMBOL_KIND_W),
            text(crate::highlight::in_order(&entry.name)).size(ts::READING),
            text(crate::highlight::in_order(&place).into_owned())
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(Wrapping::None),
        ]
        .spacing(SYMBOL_ROW_GAP)
        .align_y(iced::Center),
        selected,
        entry.name.clone(),
        Message::Nav(NavMsg::FinderPick {
            abs: entry.abs.clone(),
            line: Some(entry.line),
            name: Some(entry.name.clone()),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::LangStat;

    fn lang(name: &str, code: usize) -> LangStat {
        LangStat {
            name: name.into(),
            files: 1,
            code,
            comments: 0,
            blanks: 0,
        }
    }

    // Regression: the language proportion bar must never let its FillPortion
    // factors sum past u16::MAX — a `Row` sums them into a u16, which panicked the
    // flex layout in debug (the riverpod Stats crash) when several languages were
    // large. Scaling by the total keeps the sum ~BUDGET regardless.
    #[test]
    fn bar_portions_sum_fits_u16_with_multiple_huge_languages() {
        let langs = vec![
            lang("Dart", 500_000),
            lang("TypeScript", 400_000),
            lang("YAML", 5_000),
            lang("Markdown", 0), // zero-code language still gets a sliver, not dropped
        ];
        let portions = bar_portions(&langs);
        assert_eq!(portions.len(), langs.len());
        let sum: u32 = portions.iter().map(|&p| p as u32).sum();
        assert!(
            sum <= u16::MAX as u32,
            "FillPortion sum must fit u16, got {sum}"
        );
        assert!(
            portions.iter().all(|&p| p >= 1),
            "every segment gets at least a 1-wide sliver"
        );
    }

    #[test]
    fn bar_portions_empty_when_no_code() {
        assert!(bar_portions(&[lang("Text", 0)]).is_empty());
    }
}
