//! Input modals (breakpoint/bookmark/note), why panel, and the finder.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// Modal to set a breakpoint's condition — the program only stops there when the
/// expression is true. Empty condition sets a plain breakpoint.
pub(crate) fn bp_condition_modal<'a>(
    app: &'a App,
    edit: &'a (std::path::PathBuf, usize, String),
) -> Element<'a, Message> {
    let (path, line, draft) = edit;
    let panel = container(
        column![
            text(format!("Break at {}:{} when…", rel_of(app, path), line))
                .size(14)
                .color(theme::FG),
            text("Expression evaluated in scope. Empty means always break.")
                .size(11)
                .color(theme::DIM),
            text_input("e.g. i == 3", draft)
                .id(bp_condition_input_id())
                .on_input(Message::BpConditionInput)
                .on_submit(Message::BpConditionSet)
                .size(13)
                .padding(8),
            row![
                space().width(Fill),
                button(text("Cancel").size(12))
                    .style(theme::toolbar_button)
                    .padding([4, 12])
                    .on_press(Message::BpConditionCancel),
                button(text("Set").size(12))
                    .style(theme::toolbar_button)
                    .padding([4, 12])
                    .on_press(Message::BpConditionSet),
            ]
            .spacing(8),
        ]
        .spacing(10),
    )
    .width(460)
    .padding(16)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .padding(Padding {
            top: 120.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::BpConditionCancel))
}

/// Modal to attach an optional plain-text note to a bookmark.
pub(crate) fn bookmark_note_modal<'a>(
    _app: &'a App,
    edit: &'a (String, usize, String),
) -> Element<'a, Message> {
    let (rel, line, draft) = edit;
    let panel = container(
        column![
            text(format!("Note for {rel}:{line}"))
                .size(14)
                .color(theme::FG),
            text("Plain-text note. Leave empty to remove it.")
                .size(11)
                .color(theme::DIM),
            text_input("a short note to your future self…", draft)
                .id(note_input_id())
                .on_input(Message::BookmarkNoteInput)
                .on_submit(Message::BookmarkNoteSave)
                .size(13)
                .padding(8),
            row![
                space().width(Fill),
                button(text("Cancel").size(12))
                    .style(theme::toolbar_button)
                    .padding([4, 12])
                    .on_press(Message::BookmarkNoteCancel),
                button(text("Save").size(12))
                    .style(theme::primary_button)
                    .padding([4, 12])
                    .on_press(Message::BookmarkNoteSave),
            ]
            .spacing(8),
        ]
        .spacing(10),
    )
    .width(460)
    .padding(16)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .padding(Padding {
            top: 120.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::BookmarkNoteCancel))
}

/// Modal to attach an optional plain-text reading note to a symbol.
pub(crate) fn reading_note_modal(edit: &(String, String, String)) -> Element<'_, Message> {
    let (rel, symbol, draft) = edit;
    let panel = container(
        column![
            text(format!("Note on {symbol}  ·  {rel}"))
                .size(14)
                .color(theme::FG),
            text("Plain-text note anchored to this symbol. Leave empty to remove it.")
                .size(11)
                .color(theme::DIM),
            text_input("what you worked out about this symbol…", draft)
                .id(note_input_id())
                .on_input(Message::NoteEditInput)
                .on_submit(Message::NoteEditSave)
                .size(13)
                .padding(8),
            row![
                space().width(Fill),
                button(text("Cancel").size(12))
                    .style(theme::toolbar_button)
                    .padding([4, 12])
                    .on_press(Message::NoteEditCancel),
                button(text("Save").size(12))
                    .style(theme::primary_button)
                    .padding([4, 12])
                    .on_press(Message::NoteEditSave),
            ]
            .spacing(8),
        ]
        .spacing(10),
    )
    .width(460)
    .padding(16)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .padding(Padding {
            top: 120.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::NoteEditCancel))
}

/// The "Why is this here?" popup: the git-grounded explanation of why a line or
/// selection exists, with the commit(s) it cites. Async — "Thinking…" until the
/// answer lands.
pub(crate) fn why_modal<'a>(app: &'a App, bw: &'a crate::BlameWhy) -> Element<'a, Message> {
    let mut col = Column::new()
        .spacing(8)
        .push(text(bw.title.clone()).size(14).color(theme::FG));
    // The commits it's grounded in.
    for (sha, subject) in &bw.commits {
        let subject: String = if subject.chars().count() > 52 {
            subject.chars().take(51).collect::<String>() + "…"
        } else {
            subject.clone()
        };
        col = col.push(
            row![
                text(sha.clone())
                    .size(11)
                    .font(Font::MONOSPACE)
                    .color(theme::ACCENT),
                text(subject)
                    .size(11)
                    .color(theme::DIM)
                    .wrapping(Wrapping::None),
            ]
            .spacing(8),
        );
    }
    col = col.push(hairline());
    let body: Element<'_, Message> = if bw.loading {
        text("Thinking…").size(12).color(theme::DIM).into()
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
            button(text("Close").size(12))
                .style(theme::toolbar_button)
                .padding([4, 12])
                .on_press(Message::BlameWhyClose),
        ]);

    let panel = container(col)
        .width(480)
        .max_height(440)
        .padding(16)
        .style(theme::modal_panel);
    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .padding(Padding {
            top: 110.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::BlameWhyClose))
}

// ---------------------------------------------------------------- finder modal

pub(crate) fn finder_modal(app: &App) -> Element<'_, Message> {
    let placeholder = match app.finder.mode {
        FinderMode::Files => "File name…  (:123 jumps to a line)",
        FinderMode::Symbols => "Symbol name…",
    };
    let input = text_input(placeholder, &app.finder.query)
        .id(finder_input_id())
        .on_input(Message::FinderQueryChanged)
        .on_submit(Message::FinderConfirm)
        .size(14)
        .padding(10);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if let Some(n) = app.finder.goto_line() {
        rows.push(
            container(
                text(format!("↵  Go to line {n}"))
                    .size(13)
                    .color(theme::ACCENT),
            )
            .padding(8)
            .into(),
        );
    } else {
        match app.finder.mode {
            FinderMode::Files => finder_file_rows(app, &mut rows),
            FinderMode::Symbols => finder_symbol_rows(app, &mut rows),
        }
    }
    if rows.is_empty() {
        let hint = if app.finder.mode == FinderMode::Symbols && app.indexing {
            "Indexing symbols…"
        } else {
            "No matches"
        };
        rows.push(
            container(text(hint).size(12).color(theme::DIM))
                .padding(8)
                .into(),
        );
    }

    let panel = container(
        column![
            input,
            scrollable(Column::with_children(rows).width(Fill))
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(iced::Length::Shrink),
        ]
        .spacing(8),
    )
    .width(640)
    .max_height(520)
    .padding(10)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .padding(Padding {
            top: 80.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .style(theme::backdrop);

    opaque(mouse_area(positioned).on_press(Message::FinderClosed))
}

pub(crate) fn finder_file_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    let Some(project) = &app.project else {
        return;
    };
    for (pos, &idx) in app.finder.results.iter().enumerate() {
        let Some(entry) = project.files.get(idx) else {
            continue;
        };
        let (dir, name) = match entry.rel.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", entry.rel.as_str()),
        };
        let (glyph, color) = crate::icons::file_icon(name);
        rows.push(
            button(
                row![
                    tree_icon(glyph, color),
                    text(name).size(13),
                    text(dir)
                        .size(11)
                        .color(theme::DIM)
                        .wrapping(Wrapping::None),
                ]
                .spacing(8)
                .align_y(iced::Center),
            )
            .style(theme::list_row(pos == app.finder.selected))
            .width(Fill)
            .padding([3, 8])
            .on_press(Message::FinderPick(idx))
            .into(),
        );
    }
}

pub(crate) fn finder_symbol_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    for (pos, &idx) in app.finder.results.iter().enumerate() {
        let Some(entry) = app.symbol_index.get(idx) else {
            continue;
        };
        rows.push(
            button(
                row![
                    text(short_kind(&entry.kind))
                        .size(10)
                        .color(kind_color(&entry.kind))
                        .width(40),
                    text(&entry.name).size(13),
                    text(format!("{}:{}", entry.rel, entry.line))
                        .size(11)
                        .color(theme::DIM)
                        .wrapping(Wrapping::None),
                ]
                .spacing(10)
                .align_y(iced::Center),
            )
            .style(theme::list_row(pos == app.finder.selected))
            .width(Fill)
            .padding([3, 8])
            .on_press(Message::FinderPick(idx))
            .into(),
        );
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
