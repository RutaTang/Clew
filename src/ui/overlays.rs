//! Hover tooltip, context menu, server-panel modal; shared text/path helpers.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

pub(crate) fn hover_tooltip(h: &crate::HoverState) -> Element<'_, Message> {
    let mut parts: Vec<Element<'_, Message>> = Vec::new();
    // The LSP diagnostic first, in warn colour — if the symbol is underlined, the
    // reason is the most useful thing to surface (VS Code shows it on hover too).
    if let Some(d) = &h.diagnostic {
        parts.push(text(d.clone()).size(12).color(theme::WARN).into());
    }
    // clew's cached one-liner next, in accent so it reads as a summary, not code.
    if let Some(s) = &h.summary {
        parts.push(text(s.clone()).size(12).color(theme::ACCENT).into());
    }
    // The LSP / local-peek text below (monospace), trimmed if very long.
    if let Some(t) = &h.text {
        let shown: String = if t.chars().count() > 1200 {
            t.chars().take(1200).collect::<String>() + "…"
        } else {
            t.clone()
        };
        parts.push(
            text(shown)
                .size(12)
                .font(Font::MONOSPACE)
                .color(theme::FG)
                .into(),
        );
    }

    let panel = container(
        scrollable(Column::with_children(parts).spacing(8))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(iced::Length::Shrink),
    )
    .max_width(560)
    .max_height(320)
    .padding(8)
    .style(theme::modal_panel);

    // Pin the peek while the cursor is inside it, so it can be moved into,
    // read, and scrolled rather than vanishing the instant the cursor leaves
    // the symbol.
    let interactive = mouse_area(panel)
        .on_enter(Message::HoverPin(true))
        .on_exit(Message::HoverPin(false))
        // Swallow wheel events the inner scrollable released at its top/bottom
        // edge so overscroll doesn't chain through to the editor behind. The
        // scrollable captures the event whenever it actually moves; mouse_area
        // only reaches this handler (and calls capture_event) when it didn't.
        .on_scroll(|_| Message::Noop);

    // Position just below the hovered point (close, but clear of the line).
    container(interactive)
        .width(Fill)
        .height(Fill)
        .padding(Padding {
            top: h.y + 10.0,
            left: h.x,
            right: 0.0,
            bottom: 0.0,
        })
        .into()
}

// ---------------------------------------------------------------- context menu

pub(crate) fn context_menu<'a>(app: &'a App, menu: &'a crate::ContextMenu) -> Element<'a, Message> {
    use crate::GotoKind;

    let item = |kind: GotoKind| {
        button(text(kind.label()).size(13))
            .style(theme::list_row(false))
            .width(Fill)
            .padding([5, 12])
            .on_press(Message::ContextGoto(kind))
    };

    let plain_item = |label: &'static str, msg: Message| {
        button(text(label).size(13))
            .style(theme::list_row(false))
            .width(Fill)
            .padding([5, 12])
            .on_press(msg)
    };

    let panel = container(
        column![
            item(GotoKind::Definition),
            item(GotoKind::References),
            item(GotoKind::Implementation),
            item(GotoKind::TypeDefinition),
            plain_item("View docs", Message::ViewDocsFromMenu),
            plain_item("Call Hierarchy", Message::CallHierarchyFromMenu),
            plain_item("Explain", Message::ExplainFromMenu),
            plain_item("Add to Ask", Message::AskAboutSelection),
            plain_item("Why is this here?", Message::WhyIsThisHere),
            plain_item("Toggle Breakpoint", Message::ToggleBreakpointFromMenu),
            plain_item(
                "Conditional Breakpoint…",
                Message::ConditionalBreakpointFromMenu
            ),
        ]
        .spacing(1),
    )
    .width(MENU_W)
    .padding(4)
    .style(theme::modal_panel);

    // Place the menu at the click point, but flip it up/left when it would spill
    // past the bottom or right edge so it always shows in full.
    const MENU_W: f32 = 210.0;
    const ITEM_H: f32 = 28.0;
    let menu_h = 11.0 * ITEM_H + 16.0; // eleven items + spacing/padding
    let top = if menu.y + menu_h > app.window_height {
        (menu.y - menu_h).max(8.0)
    } else {
        menu.y
    };
    let left = if menu.x + MENU_W > app.window_width {
        (menu.x - MENU_W).max(8.0)
    } else {
        menu.x
    };
    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .padding(Padding {
            top,
            left,
            right: 0.0,
            bottom: 0.0,
        });

    // A full-size backdrop closes the menu on any outside click.
    opaque(mouse_area(positioned).on_press(Message::ContextMenuClosed))
}

// ---------------------------------------------------------------- server panel

pub(crate) fn server_panel_modal(app: &App) -> Element<'_, Message> {
    use crate::LspSlot;

    // Languages relevant to this project (present in it, or installed/running).
    let languages = app.managed_languages();

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(section_header("SERVERS FOR THIS PROJECT"));
    if languages.is_empty() {
        rows.push(
            container(
                text("No supported languages detected in this project.")
                    .size(11)
                    .color(theme::DIM),
            )
            .padding([2, 8])
            .into(),
        );
    }
    for lang in &languages {
        let (status, action) = app.lsp_row(lang);
        let server_name = crate::lsp::registry::default_for_language(lang)
            .map(|s| s.name.to_string())
            .unwrap_or_else(|| "custom".into());

        let action_el: Element<'_, Message> = match action {
            Some((label, msg)) => button(text(label).size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(msg)
                .into(),
            None => space().width(0).into(),
        };

        rows.push(
            row![
                text(lang.clone()).size(12).width(70),
                text(server_name).size(12).color(theme::ACCENT).width(140),
                text(status).size(11).color(theme::DIM).width(Fill),
                action_el,
            ]
            .spacing(8)
            .align_y(iced::Center)
            .padding([3, 8])
            .into(),
        );
    }

    rows.push(section_header("INSTALLED (global, shared across projects)"));
    if app.installed_servers.is_empty() {
        rows.push(
            container(text("Nothing downloaded yet.").size(11).color(theme::DIM))
                .padding([2, 8])
                .into(),
        );
    }
    for srv in &app.installed_servers {
        rows.push(
            row![
                text(&srv.name).size(12).width(150),
                text(&srv.version).size(11).color(theme::DIM).width(120),
                text(human_size(srv.bytes))
                    .size(11)
                    .color(theme::DIM)
                    .width(Fill),
                button(text("Remove").size(11))
                    .style(theme::toolbar_button)
                    .padding([2, 8])
                    .on_press(Message::LspRemove {
                        name: srv.name.clone(),
                        version: srv.version.clone(),
                    }),
            ]
            .spacing(8)
            .align_y(iced::Center)
            .padding([3, 8])
            .into(),
        );
    }

    // Log of the active file's language server, if it is running.
    let logs = app
        .active_viewer()
        .and_then(|v| v.lang_key)
        .and_then(|l| app.lsp.get(l))
        .and_then(|s| match s {
            LspSlot::Ready(c) => Some(c.logs()),
            _ => None,
        })
        .unwrap_or_default();
    rows.push(section_header("SERVER LOG"));
    let log_lines: Vec<Element<'_, Message>> = logs
        .iter()
        .rev()
        .take(200)
        .map(|line| {
            text(line.clone())
                .size(11)
                .font(Font::MONOSPACE)
                .color(theme::DIM)
                .wrapping(Wrapping::None)
                .into()
        })
        .collect();
    let log_view = if log_lines.is_empty() {
        container(text("No output.").size(11).color(theme::DIM)).padding([2, 8])
    } else {
        container(
            scrollable(Column::with_children(log_lines).spacing(1))
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(160),
        )
        .padding([2, 8])
    };
    rows.push(log_view.into());

    let panel = container(
        column![
            row![
                text("Language Servers").size(17).color(theme::FG),
                space().width(Fill),
                button(text("Close").size(12))
                    .style(theme::toolbar_button)
                    .padding([3, 12])
                    .on_press(Message::ToggleServerPanel),
            ]
            .align_y(iced::Center),
            scrollable(Column::with_children(rows).spacing(2).width(Fill))
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(iced::Length::Fill),
        ]
        .spacing(12),
    )
    .width(720)
    .max_height(600)
    .padding(20)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);

    opaque(mouse_area(positioned).on_press(Message::ToggleServerPanel))
}

/// A thin vertical scrollbar geometry, paired with [`theme::overlay_scrollbar`]
/// so panels get a slim, auto-hiding bar instead of the chunky default.
pub(crate) fn thin_scroll() -> Direction {
    Direction::Vertical(Scrollbar::new().width(6.0).scroller_width(6.0))
}

// -------------------------------------------------- project graph overlays

/// Path relative to the project root, for compact display in the overlays.
/// The first sentence of a summary, capped, for a compact inline annotation.
pub fn first_sentence(s: &str) -> String {
    let s = s.trim();
    let sentence = match s.split_once(". ") {
        Some((first, _)) => first,
        None => s.strip_suffix('.').unwrap_or(s),
    };
    let capped: String = sentence.chars().take(96).collect();
    if capped.chars().count() < sentence.chars().count() {
        format!("{}…", capped.trim_end())
    } else {
        capped
    }
}

/// Truncate to at most `max` characters, appending an ellipsis when cut. Used
/// for single-line list entries whose full text is available on hover.
pub(crate) fn truncate_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let capped: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", capped.trim_end())
    } else {
        s.to_string()
    }
}

/// Strip inline-code backticks for compact one-line descriptions. These dense
/// rows don't render markdown chips, so raw backticks would otherwise leak in as
/// literal characters (unlike the TL;DR banner / Overview, which do render them).
pub(crate) fn strip_backticks(s: &str) -> String {
    s.replace('`', "")
}

/// A dim, single-line secondary description: first sentence, backticks stripped,
/// truncated with an ellipsis and clipped so it never wraps or overflows the
/// panel. Shared by the right-panel call-flow / contains rows so they match the
/// outline rows' treatment instead of hard-cutting mid-word.
pub(crate) fn one_line_desc<'a>(full: &str, max: usize) -> Element<'a, Message> {
    let one = truncate_ellipsis(&first_sentence(&strip_backticks(full)), max);
    container(
        text(one)
            .size(10)
            .color(theme::DIM)
            .wrapping(Wrapping::None),
    )
    .clip(true)
    .width(Fill)
    .into()
}

pub(crate) fn rel_of(app: &App, path: &std::path::Path) -> String {
    match &app.project {
        Some(p) => path
            .strip_prefix(&p.root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string(),
        None => path.to_string_lossy().to_string(),
    }
}
