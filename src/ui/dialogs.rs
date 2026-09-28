//! Input modals (breakpoint/bookmark/note), why panel, and the finder.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::row;

/// Height of one finder result row. Fixed so the key handler can compute where
/// the selected row sits in the list and scroll it into view.
pub(crate) const FINDER_ROW_H: f32 = 28.0;

/// Modal to set a breakpoint's condition — the program only stops there when the
/// expression is true. Empty condition sets a plain breakpoint.
pub(crate) fn bp_condition_modal<'a>(
    app: &'a App,
    edit: &'a (std::path::PathBuf, usize, String),
) -> Element<'a, Message> {
    let (path, line, draft) = edit;
    prompt_modal(Prompt {
        title: format!("Break at {}:{} when…", rel_of(app, path), line),
        hint: "Expression evaluated in scope. Empty means always break.",
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
        hint: "Plain-text note. Leave empty to remove it.",
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
        hint: "Plain-text note anchored to this symbol. Leave empty to remove it.",
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
                    .font(Font::MONOSPACE)
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

// ---------------------------------------------------------------- finder modal

pub(crate) fn finder_modal(app: &App) -> Element<'_, Message> {
    let placeholder = match app.proj.finder.mode {
        FinderMode::Files => "File name…  (:123 jumps to a line)",
        FinderMode::Symbols => "Symbol name…",
    };
    let input = text_input(placeholder, &app.proj.finder.query)
        .id(finder_input_id())
        .on_input(|v| Message::Nav(NavMsg::FinderQueryChanged(v)))
        .on_submit(Message::Nav(NavMsg::FinderConfirm))
        .size(ts::EMPHASIS)
        .padding(10);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if let Some(n) = app.proj.finder.goto_line() {
        rows.push(
            container(
                text(format!("↵  Go to line {n}"))
                    .size(ts::BASE)
                    .color(theme::accent()),
            )
            .padding(8)
            .into(),
        );
    } else {
        match app.proj.finder.mode {
            FinderMode::Files => finder_file_rows(app, &mut rows),
            FinderMode::Symbols => finder_symbol_rows(app, &mut rows),
        }
    }
    if rows.is_empty() {
        let hint = if app.proj.finder.mode == FinderMode::Symbols && app.proj.indexing {
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
    let panel = container(
        iced::widget::column![
            input,
            // No spacing or padding inside the list: row `i` must start at
            // exactly `i * FINDER_ROW_H` for `reveal_finder_selection`.
            scrollable(Column::with_children(rows).width(Fill))
                .id(finder_list_id())
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(iced::Length::Shrink),
            cap_note,
        ]
        .spacing(8),
    )
    .width(FINDER_W)
    .max_height(FINDER_MAX_H)
    .padding(10)
    .style(theme::modal_panel);

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
    pick: Message,
) -> Element<'a, Message> {
    button(
        container(content)
            .height(Fill)
            .align_y(iced::alignment::Vertical::Center),
    )
    .style(theme::list_row(selected))
    .width(Fill)
    .height(Length::Fixed(FINDER_ROW_H))
    .padding([0, 8])
    .on_press(pick)
    .into()
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
    .style(theme::list_row(selected))
    .width(Fill)
    .height(Length::Fixed(FINDER_ROW_H))
    .padding([0, 8])
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
        let (glyph, color) = crate::icons::file_icon(name);
        rows.push(finder_row(
            row![
                tree_icon(glyph, color),
                text(name).size(ts::BASE),
                text(dir)
                    .size(ts::SMALL)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            ]
            .spacing(8)
            .align_y(iced::Center),
            pos == app.proj.finder.selected,
            Message::Nav(NavMsg::FinderPick {
                abs: entry.abs.clone(),
                line: None,
            }),
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
                text(&entry.name).size(ts::BASE),
                text(format!("{}:{}", entry.rel, entry.line))
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
