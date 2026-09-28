//! Hover tooltip, context menu, server-panel modal; shared text/path helpers.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// Gap between the pointer and the hover peek below (or above) it.
const PEEK_GAP: f32 = 10.0;
/// Width of the right-click menu.
const MENU_W: f32 = 210.0;

pub(crate) fn hover_tooltip(h: &crate::HoverState) -> Element<'_, Message> {
    // Nothing to say (no summary, no LSP text, no diagnostic): show nothing
    // rather than an empty box — an empty peek reads as "hover is broken".
    if h.diagnostic.is_none() && h.summary.is_none() && h.text.is_none() {
        return slot();
    }
    let mut parts: Vec<Element<'_, Message>> = Vec::new();
    // The LSP diagnostic first, in warn colour — if the symbol is underlined, the
    // reason is the most useful thing to surface (VS Code shows it on hover too).
    if let Some(d) = &h.diagnostic {
        parts.push(text(d.clone()).size(ts::BODY).color(theme::warn()).into());
    }
    // clew's cached one-liner next, in accent so it reads as a summary, not code.
    if let Some(s) = &h.summary {
        parts.push(text(s.clone()).size(ts::BODY).color(theme::accent()).into());
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
                .size(ts::BODY)
                .font(Font::MONOSPACE)
                .color(theme::fg())
                .into(),
        );
    }

    let panel = container(
        scrollable(Column::with_children(parts).spacing(8))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(iced::Length::Shrink),
    )
    .max_width(PEEK_MAX_W)
    .max_height(PEEK_MAX_H)
    .padding(8)
    .style(theme::modal_panel);

    // Pin the peek while the cursor is inside it, so it can be moved into,
    // read, and scrolled rather than vanishing the instant the cursor leaves
    // the symbol.
    let interactive = mouse_area(panel)
        .on_enter(Message::Hover(HoverMsg::Pin(true)))
        .on_exit(Message::Hover(HoverMsg::Pin(false)))
        // Swallow wheel events the inner scrollable released at its top/bottom
        // edge so overscroll doesn't chain through to the editor behind. The
        // scrollable captures the event whenever it actually moves; mouse_area
        // only reaches this handler (and calls capture_event) when it didn't.
        .on_scroll(|_| Message::Noop);

    // Just below the hovered point (close, but clear of the line), flipped
    // above it near the bottom edge and slid left near the right edge — placed
    // from the peek's measured size, so it always shows in full.
    Anchored::new(
        interactive,
        iced::Point::new(h.x, h.y),
        Anchoring::Below { gap: PEEK_GAP },
    )
    .into()
}

// ---------------------------------------------------------------- context menu

pub(crate) fn context_menu(menu: &crate::ContextMenu) -> Element<'_, Message> {
    use crate::GotoKind;

    let item = |label: &'static str, msg: Message| {
        button(text(label).size(ts::BASE))
            .style(theme::list_row(false))
            .width(Fill)
            .padding([5, 12])
            .on_press(msg)
    };
    let goto = |kind: GotoKind| item(kind.label(), Message::Hover(HoverMsg::ContextGoto(kind)));

    let panel = container(
        column![
            goto(GotoKind::Definition),
            goto(GotoKind::References),
            goto(GotoKind::Implementation),
            goto(GotoKind::TypeDefinition),
            item("View docs", Message::Docs(DocsMsg::ViewFromMenu)),
            item("Call Hierarchy", Message::Calls(CallsMsg::FromMenu)),
            item("Explain", Message::Explain(ExplainMsg::FromMenu)),
            item("Add to Ask", Message::Ask(AskMsg::AboutSelection)),
            item(
                "Why is this here?",
                Message::TimeTravel(TimeTravelMsg::WhyIsThisHere)
            ),
            item(
                "Toggle Breakpoint",
                Message::Debug(DebugMsg::ToggleBreakpointFromMenu)
            ),
            item(
                "Conditional Breakpoint…",
                Message::Debug(DebugMsg::ConditionalBreakpointFromMenu)
            ),
        ]
        .spacing(1),
    )
    .width(MENU_W)
    .padding(4)
    .style(theme::modal_panel);

    // At the click point, opening down-right and flipping up / left at the
    // window edges — from the menu's measured size, not a counted estimate.
    let placed = Anchored::new(
        opaque(panel),
        iced::Point::new(menu.x, menu.y),
        Anchoring::Corner,
    );
    // A full-size backdrop closes the menu on any outside click.
    opaque(mouse_area(placed).on_press(Message::Hover(HoverMsg::ContextMenuClosed)))
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
                    .size(ts::SMALL)
                    .color(theme::dim()),
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
            Some((label, msg)) => button(text(label).size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(msg)
                .into(),
            None => slot(),
        };

        rows.push(
            row![
                text(lang.clone()).size(ts::BODY).width(70),
                text(server_name)
                    .size(ts::BODY)
                    .color(theme::accent())
                    .width(140),
                text(status).size(ts::SMALL).color(theme::dim()).width(Fill),
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
            container(
                text("Nothing downloaded yet.")
                    .size(ts::SMALL)
                    .color(theme::dim()),
            )
            .padding([2, 8])
            .into(),
        );
    }
    for srv in &app.installed_servers {
        rows.push(
            row![
                text(&srv.name).size(ts::BODY).width(150),
                text(&srv.version)
                    .size(ts::SMALL)
                    .color(theme::dim())
                    .width(120),
                text(human_size(srv.bytes))
                    .size(ts::SMALL)
                    .color(theme::dim())
                    .width(Fill),
                button(text("Remove").size(ts::SMALL))
                    .style(theme::toolbar_button)
                    .padding([2, 8])
                    .on_press(Message::Lsp(LspMsg::Remove {
                        name: srv.name.clone(),
                        version: srv.version.clone(),
                    })),
            ]
            .spacing(8)
            .align_y(iced::Center)
            .padding([3, 8])
            .into(),
        );
    }

    // Log of the active file's language server, if it is running: its newest
    // lines, which the app copies from the server only when the log moved
    // (`App::sync_lsp_log`) — never per repaint.
    let slot_state = app
        .active_viewer()
        .and_then(|v| v.lang_key)
        .and_then(|l| app.proj.link.lsp.get(l));
    let (logs, unreadable): (&[String], _) = match &app.proj.link.lsp_log {
        Some(tail) => match &tail.lines {
            Ok(lines) => (lines, None),
            // A poisoned state is not an empty log: say what happened.
            Err(poisoned) => (&[], Some(*poisoned)),
        },
        None => (&[], None),
    };
    rows.push(section_header("SERVER LOG"));
    let log_lines: Vec<Element<'_, Message>> = logs
        .iter()
        .rev()
        .map(|line| {
            text(line.as_str())
                .size(ts::SMALL)
                .font(Font::MONOSPACE)
                .color(theme::dim())
                .wrapping(Wrapping::None)
                .into()
        })
        .collect();
    let log_view = if log_lines.is_empty() {
        // A server that is not running has no log to show — say which, so an
        // empty box is never mistaken for a quiet but healthy server.
        let (why, color) = match (slot_state, unreadable) {
            (_, Some(poisoned)) => (poisoned.to_string(), theme::warning()),
            (Some(LspSlot::Ready(c)), None) if !c.alive() => (
                "The server has stopped; restart it to see new output.".to_string(),
                theme::dim(),
            ),
            (Some(LspSlot::Ready(_)), None) => ("No output.".to_string(), theme::dim()),
            _ => ("No server running for this file.".to_string(), theme::dim()),
        };
        container(text(why).size(ts::SMALL).color(color)).padding([2, 8])
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
                text("Language Servers").size(ts::TITLE).color(theme::fg()),
                space().width(Fill),
                button(text("Close").size(ts::BODY))
                    .style(theme::toolbar_button)
                    .padding([3, 12])
                    .on_press(Message::Lsp(LspMsg::TogglePanel)),
            ]
            .align_y(iced::Center),
            scrollable(Column::with_children(rows).spacing(2).width(Fill))
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(iced::Length::Fill),
        ]
        .spacing(12),
    )
    .width(SERVER_PANEL_W)
    .max_height(SERVER_PANEL_MAX_H)
    .padding(MODAL_PAD)
    .style(theme::modal_panel);

    modal(
        panel,
        Placement::Center,
        Backdrop::Dim(Some(Message::Lsp(LspMsg::TogglePanel))),
    )
}

// ------------------------------------------------------------ text helpers

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
            .size(ts::CAPTION)
            .color(theme::dim())
            .wrapping(Wrapping::None),
    )
    .clip(true)
    .width(Fill)
    .into()
}

/// Path relative to the project root, for compact display in the overlays.
pub(crate) fn rel_of(app: &App, path: &std::path::Path) -> String {
    match &app.proj.project {
        Some(p) => path
            .strip_prefix(&p.root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string(),
        None => path.to_string_lossy().to_string(),
    }
}
