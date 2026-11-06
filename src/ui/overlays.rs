//! Hover tooltip and context menu; shared text/path helpers.

use super::*;

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
                .font(crate::fonts::CODE)
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
    //
    // Opaque, as the context menu is: the peek's text reports no mouse
    // interaction of its own, so the stack left the cursor to the code view
    // under it, and a click on the peek moved the caret (or, with ⌘, jumped
    // to a definition) at the hidden code beneath.
    Anchored::new(
        opaque(interactive),
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

    let mut items = vec![
        goto(GotoKind::Definition),
        goto(GotoKind::References),
        goto(GotoKind::Implementation),
        goto(GotoKind::TypeDefinition),
    ];
    // The language's own action, for a file whose language has one.
    if let Some(kind) = menu.insight {
        items.push(item(kind.label(), Message::Hover(HoverMsg::Insight)));
    }
    items.extend([
        item("View docs", Message::Docs(DocsMsg::ViewFromMenu)),
        item("Call Hierarchy", Message::Calls(CallsMsg::FromMenu)),
        item("Trace Value", Message::Flow(crate::FlowMsg::FromMenu)),
        item("Explain", Message::Explain(ExplainMsg::FromMenu)),
        item("Add to Ask", Message::Ask(AskMsg::AboutSelection)),
        item(
            "Why is this here?",
            Message::TimeTravel(TimeTravelMsg::WhyIsThisHere),
        ),
        item(
            "Toggle Breakpoint",
            Message::Debug(DebugMsg::ToggleBreakpointFromMenu),
        ),
        item(
            "Conditional Breakpoint…",
            Message::Debug(DebugMsg::ConditionalBreakpointFromMenu),
        ),
    ]);
    let panel = container(Column::with_children(items.into_iter().map(Element::from)).spacing(1))
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
        Some(p) => clew_core::rel_string(path.strip_prefix(&p.root).unwrap_or(path)),
        None => path.to_string_lossy().to_string(),
    }
}
