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
/// Height of a heading over a group of the command bar's suggestions.
pub(crate) const FINDER_HEAD_H: f32 = 29.0;
/// Size of a command bar row's detail (a symbol's file and line).
const BAR_DETAIL_SIZE: f32 = 12.0;
/// Height of the finder's query row, over its results.
const FINDER_INPUT_H: f32 = 52.0;
/// The command bar's corners, as the design rounds them.
const FINDER_RADIUS: f32 = 12.0;
/// Height of the finder's foot, the line of key hints under its results.
const FINDER_FOOT_H: f32 = 32.0;

/// Modal to set a breakpoint's condition — the program only stops there when the
/// expression is true. Empty condition sets a plain breakpoint.
pub(crate) fn bp_condition_modal<'a>(
    app: &'a App,
    edit: &'a (std::path::PathBuf, usize, String),
) -> Element<'a, Message> {
    let (path, line, draft) = edit;
    prompt_modal(Prompt {
        title: format!("Break at {}:{} when…", rel_of(app, path), line),
        placeholder: "e.g. i == 3",
        value: draft,
        input_id: bp_condition_input_id(),
        on_input: |v| Message::Debug(DebugMsg::BpConditionInput(v)),
        submit: Message::Debug(DebugMsg::BpConditionSet),
        submit_label: "Set",
        cancel: Message::Debug(DebugMsg::BpConditionCancel),
    })
}

/// Modal to attach an optional plain-text note to a bookmark.
pub(crate) fn bookmark_note_modal(edit: &(String, usize, String)) -> Element<'_, Message> {
    let (rel, line, draft) = edit;
    prompt_modal(Prompt {
        title: format!("Note for {rel}:{line}"),
        placeholder: "a short note to your future self…",
        value: draft,
        input_id: note_input_id(),
        on_input: |v| Message::Reading(ReadingMsg::BookmarkNoteInput(v)),
        submit: Message::Reading(ReadingMsg::BookmarkNoteSave),
        submit_label: "Save",
        cancel: Message::Reading(ReadingMsg::BookmarkNoteCancel),
    })
}

/// Modal to attach an optional plain-text reading note to a symbol.
pub(crate) fn reading_note_modal(edit: &(String, String, String)) -> Element<'_, Message> {
    let (rel, symbol, draft) = edit;
    prompt_modal(Prompt {
        title: format!("Note on {symbol}  ·  {rel}"),
        placeholder: "what you worked out about this symbol…",
        value: draft,
        input_id: note_input_id(),
        on_input: |v| Message::Reading(ReadingMsg::NoteEditInput(v)),
        submit: Message::Reading(ReadingMsg::NoteEditSave),
        submit_label: "Save",
        cancel: Message::Reading(ReadingMsg::NoteEditCancel),
    })
}

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
        text("Thinking…").size(ts::BODY).color(theme::dim()).into()
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
        .push(row![
            space().width(Fill),
            button(text("Close").size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([4, 12])
                .on_press(Message::TimeTravel(TimeTravelMsg::BlameWhyClose)),
        ]);

    let panel = container(col)
        .width(WHY_W)
        .max_height(WHY_MAX_H)
        .padding(PROMPT_PAD)
        .style(theme::modal_panel);
    modal(
        panel,
        Placement::Top(WHY_TOP),
        Backdrop::Dim(Some(Message::TimeTravel(TimeTravelMsg::BlameWhyClose))),
    )
}

/// The popup of a language's answer about the code under the cursor (a
/// macro's expansion, a Lean goal, a Haskell type), with a way to where it
/// points. "Asking…" until the answer lands.
pub(crate) fn insight_modal<'a>(app: &'a App, insight: &'a crate::Insight) -> Element<'a, Message> {
    let body: Element<'_, Message> = if insight.loading {
        text("Asking…").size(ts::BODY).color(theme::dim()).into()
    } else {
        Column::with_children(render_prepared(app, &insight.prepared))
            .spacing(8)
            .width(Fill)
            .into()
    };
    let mut buttons = row![space().width(Fill)].spacing(8);
    if insight.defined.is_some() {
        buttons = buttons.push(
            button(text("Open Definition").size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([4, 12])
                .on_press(Message::Hover(HoverMsg::InsightOpenDefinition)),
        );
    }
    buttons = buttons.push(
        button(text("Close").size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([4, 12])
            .on_press(Message::Hover(HoverMsg::InsightClose)),
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
        Placement::Top(WHY_TOP),
        Backdrop::Dim(Some(Message::Hover(HoverMsg::InsightClose))),
    )
}

// ---------------------------------------------------------------- finder modal

pub(crate) fn finder_modal(app: &App) -> Element<'_, Message> {
    let placeholder = match app.proj.finder.mode {
        _ if app.proj.finder.bar => "Run a command or open a file",
        FinderMode::Files => "Open a file  ·  > for a command, :123 for a line",
        FinderMode::Symbols => "Go to a symbol",
    };
    let glyph = match app.proj.finder.mode {
        FinderMode::Symbols if !app.proj.finder.is_commands() => Glyph::Cube,
        _ => Glyph::Search,
    };
    // Or say it: what is said runs a command, or opens what it names.
    let talk = super::toolbar::tip_at(
        button(
            container(glyph::icon(Glyph::Mic, theme::fg(), 15.0))
                .width(Fill)
                .height(Fill)
                .align_x(iced::Center)
                .align_y(iced::Center),
        )
        .style(theme::icon_button(true))
        .padding(0)
        .width(Length::Fixed(30.0))
        .height(Length::Fixed(30.0))
        .on_press(Message::Voice(crate::VoiceMsg::Toggle)),
        "Talk",
        Some(app.keymap.chord(crate::keymap::Action::Voice).caps()),
        tooltip::Position::Bottom,
    );
    let input = container(
        row![
            glyph::icon(glyph, theme::dim(), 16.0),
            ts::field_in(
                text_input(placeholder, &app.proj.finder.query)
                    .id(finder_input_id())
                    .on_input(|v| Message::Nav(NavMsg::FinderQueryChanged(v)))
                    .on_submit(Message::Nav(NavMsg::FinderConfirm))
                    .style(theme::bare_input),
                FINDER_INPUT_H,
                ts::INTER,
                ts::SUBTITLE,
            ),
            talk,
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
    if app.proj.finder.is_commands() {
        finder_command_rows(app, &mut rows);
    } else if let Some(n) = app.proj.finder.goto_line() {
        rows.push(
            container(
                text(format!("↵  Go to line {n}"))
                    .size(ts::BASE)
                    .color(theme::accent()),
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
            "No matching command"
        } else if app.proj.finder.mode == FinderMode::Symbols && app.proj.indexing {
            "Indexing symbols…"
        } else {
            "No matches"
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
        Some(note) if app.proj.finder.mode == FinderMode::Symbols => {
            text(format!("Index incomplete: {note}"))
                .size(ts::CAPTION)
                .color(theme::dim())
                .into()
        }
        _ => slot(),
    };
    let panel = container(iced::widget::column![
        input,
        hairline(),
        // The design's gap between what the list holds, and no padding
        // inside it: row `i` starts at exactly `i * FINDER_ROW_H` (its
        // height and the gap after it) for `reveal_finder_selection`.
        // Capped so the foot under it always shows.
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
        .max_height(FINDER_MAX_H - FINDER_INPUT_H - FINDER_FOOT_H - 24.0)
        .padding(6),
        cap_note,
        hairline(),
        finder_footer(),
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

/// The palette's foot: how to move through it, open a result, and leave.
fn finder_footer<'a>() -> Element<'a, Message> {
    let enter = if crate::keymap::MAC_KEYS {
        "Return to open"
    } else {
        "Enter to open"
    };
    let hint = |what: &'a str| {
        ts::set_in(
            text(what).color(theme::dim()),
            FINDER_FOOT_H,
            ts::INTER,
            ts::SMALL,
        )
    };
    container(
        row![hint("↑ ↓ to move"), hint(enter), hint("Esc to close")]
            .spacing(16)
            .align_y(iced::Center),
    )
    .height(Length::Fixed(FINDER_FOOT_H))
    .align_y(iced::Center)
    .padding([0, 16])
    .into()
}

/// One finder result: a fixed-height, full-width button. `pick` opens what
/// the row shows — its target itself, not an index into a list a rescan or a
/// re-index replaces.
fn finder_row<'a>(
    content: impl Into<Element<'a, Message>>,
    selected: bool,
    pick: Message,
) -> Element<'a, Message> {
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
                Glyph::Message,
                theme::accent(),
                title.clone(),
                None,
                Some(enter.to_string()),
            ),
            Suggestion::Talk => (
                Glyph::Mic,
                theme::dim(),
                "Talk instead of typing".to_string(),
                None,
                Some(chord(crate::keymap::Action::Voice)),
            ),
            Suggestion::GoToFile => (
                Glyph::File,
                theme::dim(),
                "Go to File…".to_string(),
                None,
                Some(chord(crate::keymap::Action::OpenFile)),
            ),
            Suggestion::Symbol {
                name, file, line, ..
            } => (
                Glyph::Braces,
                theme::dim(),
                name.clone(),
                Some(format!("{file} · line {line}")),
                None,
            ),
            Suggestion::Command { title, recent } => (
                match title.as_str() {
                    _ if *recent => Glyph::TimeTravel,
                    "Open Remote…" => Glyph::Remote,
                    _ => Glyph::Folder,
                },
                theme::dim(),
                title.clone(),
                None,
                app.palette
                    .iter()
                    .find(|c| &c.title == title)
                    .and_then(|c| c.keys.clone()),
            ),
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
            rows.push(bar_row(
                Glyph::ChevronRight,
                theme::dim(),
                command.title.clone(),
                None,
                command.keys.clone(),
                selected,
                Message::Nav(NavMsg::PaletteRun(command.title.clone())),
            ));
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
        rows.push(bar_row(
            Glyph::File,
            theme::dim(),
            crate::highlight::in_order(name).into_owned(),
            Some(crate::highlight::in_order(dir).into_owned()),
            None,
            selected,
            Message::Nav(NavMsg::FinderPick {
                abs: entry.abs.clone(),
                line: None,
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
            text("— no longer in the project —")
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
        let Some(entry) = project.files.get(idx) else {
            rows.push(stale_finder_row(pos == app.proj.finder.selected));
            continue;
        };
        let (dir, name) = match entry.rel.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", entry.rel.as_str()),
        };
        rows.push(finder_row(
            row![
                glyph::icon(Glyph::File, theme::dim(), 15.0),
                // In the order they are spelled (`highlight::in_order`).
                text(crate::highlight::in_order(name)).size(ts::READING),
                space().width(Fill),
                text(crate::highlight::in_order(dir))
                    .size(ts::SMALL)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            ]
            .spacing(11)
            .align_y(iced::Center),
            pos == app.proj.finder.selected,
            Message::Nav(NavMsg::FinderPick {
                abs: entry.abs.clone(),
                line: None,
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
        rows.push(finder_row(
            row![
                text(command.title.clone()).size(ts::READING),
                space().width(Fill),
                keys,
            ]
            .spacing(10)
            .align_y(iced::Center),
            pos == app.proj.finder.selected,
            Message::Nav(NavMsg::PaletteRun(command.title.clone())),
        ));
    }
}

pub(crate) fn finder_symbol_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    for (pos, &idx) in app.proj.finder.results.iter().enumerate() {
        let Some(entry) = app.proj.symbol_index.get(idx) else {
            rows.push(stale_finder_row(pos == app.proj.finder.selected));
            continue;
        };
        rows.push(finder_row(
            row![
                text(short_kind(&entry.kind))
                    .size(ts::CAPTION)
                    .color(theme::kind_color(&entry.kind))
                    .width(40),
                text(crate::highlight::in_order(&entry.name)).size(ts::READING),
                text(format!(
                    "{}:{}",
                    crate::highlight::in_order(&entry.rel),
                    entry.line
                ))
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(Wrapping::None),
            ]
            .spacing(10)
            .align_y(iced::Center),
            pos == app.proj.finder.selected,
            Message::Nav(NavMsg::FinderPick {
                abs: entry.abs.clone(),
                line: Some(entry.line),
            }),
        ));
    }
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
