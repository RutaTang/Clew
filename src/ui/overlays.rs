//! Hover tooltip and context menu; shared text/path helpers.

use super::*;

/// Gap between the pointer and the hover peek below (or above) it.
const PEEK_GAP: f32 = 10.0;
/// Width of the right-click menu as designed: wider where a row needs it.
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

pub(crate) fn context_menu<'a>(app: &App, menu: &'a crate::ContextMenu) -> Element<'a, Message> {
    use crate::GotoKind;

    // A row: its words, what it does, the keys that do it in the code (the
    // `g` pairs, a shortcut), and whether it can do it now — what only the
    // language server answers waits for it.
    struct Row {
        label: &'static str,
        msg: Message,
        keys: Option<String>,
        able: bool,
        /// Spends an AI call: marked so, as every such control is.
        ai: bool,
    }
    let keys = |k: &str| Some(k.to_string());
    let goto = |kind: GotoKind, k: &str| Row {
        label: kind.label(),
        msg: Message::Hover(HoverMsg::ContextGoto(kind)),
        keys: keys(k),
        able: menu.served,
        ai: false,
    };
    let mut navigate = vec![
        goto(GotoKind::Definition, "gd"),
        goto(GotoKind::References, "gr"),
        goto(GotoKind::Implementation, "gi"),
        goto(GotoKind::TypeDefinition, "gy"),
        Row {
            label: tr!("Call Hierarchy"),
            msg: Message::Calls(CallsMsg::FromMenu),
            keys: keys("gc"),
            able: menu.served,
            ai: false,
        },
    ];
    let lang = app
        .proj
        .panes
        .get(menu.pane)
        .and_then(Option::as_ref)
        .and_then(|v| v.lang_key);
    // The language's own action, for a file whose language has one: its
    // language server's to answer — but C and C++'s macros, which Clew
    // reads itself. It was offered with no server, and pressed said only
    // that none was running.
    if let Some(kind) = menu.insight {
        navigate.push(Row {
            label: kind.label(),
            msg: Message::Hover(HoverMsg::Insight),
            keys: keys("ge"),
            able: menu.served || matches!(lang, Some("c" | "cpp")),
            ai: false,
        });
    }
    // And its others (Rust: a type's memory layout, an item's tests),
    // which only its language server can answer.
    for &kind in lang.map_or(&[][..], crate::InsightKind::more_for_lang) {
        navigate.push(Row {
            label: kind.label(),
            msg: Message::Hover(HoverMsg::InsightOf(kind)),
            keys: None,
            able: menu.served,
            ai: false,
        });
    }
    // A Lean proof, walked one tactic at a time.
    if lang == Some("lean") {
        navigate.push(Row {
            label: tr!("Step Through Proof"),
            msg: Message::Proof(crate::ProofMsg::FromMenu),
            keys: None,
            able: menu.served,
            ai: false,
        });
    }
    let plain = |label: &'static str, msg: Message| Row {
        label,
        msg,
        keys: None,
        able: true,
        ai: false,
    };
    // In groups: going somewhere, understanding what is here, marking the
    // line, debugging.
    let groups = [
        navigate,
        vec![
            plain(tr!("View Docs"), Message::Docs(DocsMsg::ViewFromMenu)),
            Row {
                keys: Some(app.keymap.chord(crate::keymap::Action::TraceValue).caps()),
                ..plain(tr!("Trace Value"), Message::Flow(crate::FlowMsg::FromMenu))
            },
            Row {
                keys: Some(app.keymap.chord(crate::keymap::Action::FakeRun).caps()),
                ai: true,
                ..plain(
                    tr!("Fake Run…"),
                    Message::Fake(crate::FakeMsg::FromMenu { debug: false }),
                )
            },
            Row {
                keys: Some(app.keymap.chord(crate::keymap::Action::FakeDebug).caps()),
                ai: true,
                ..plain(
                    tr!("Fake Debug…"),
                    Message::Fake(crate::FakeMsg::FromMenu { debug: true }),
                )
            },
            Row {
                ai: true,
                ..plain(tr!("Explain"), Message::Explain(ExplainMsg::FromMenu))
            },
            plain(tr!("Add to Ask"), Message::Ask(AskMsg::AboutSelection)),
            Row {
                ai: true,
                ..plain(
                    tr!("Why Is This Here?"),
                    Message::TimeTravel(TimeTravelMsg::WhyIsThisHere),
                )
            },
        ],
        // A bookmark on this line, and a note to come back to it by.
        {
            let marked = app
                .proj
                .panes
                .get(menu.pane)
                .and_then(Option::as_ref)
                .is_some_and(|v| {
                    app.proj
                        .bookmarks
                        .iter()
                        .any(|b| b.rel == v.rel && b.line == menu.line + 1)
                });
            vec![
                Row {
                    keys: Some(
                        app.keymap
                            .chord(crate::keymap::Action::ToggleBookmark)
                            .caps(),
                    ),
                    ..plain(
                        tr!("Toggle Bookmark"),
                        Message::Reading(ReadingMsg::BookmarkFromMenu { note: false }),
                    )
                },
                plain(
                    if marked {
                        tr!("Edit Bookmark Note…")
                    } else {
                        tr!("Bookmark with Note…")
                    },
                    Message::Reading(ReadingMsg::BookmarkFromMenu { note: true }),
                ),
            ]
        },
        vec![
            plain(
                tr!("Toggle Breakpoint"),
                Message::Debug(DebugMsg::ToggleBreakpointFromMenu),
            ),
            plain(
                tr!("Conditional Breakpoint…"),
                Message::Debug(DebugMsg::ConditionalBreakpointFromMenu),
            ),
        ],
    ];
    // As designed, or as wide as its longest row (its keys too) in the
    // reader's language, so none wraps or is clipped. A row is its words,
    // the room between and its keys, two gaps of 16 apart (the keys' slot
    // is there, empty, on a row with none): counted one gap short, a long
    // French or Japanese row's keys were cut at the menu's edge.
    let key_w = |k: &Option<String>| {
        2.0 * ROW_GAP
            + k.as_deref().map_or(0.0, |k| {
                super::sheet::text_width(k, ts::SMALL, crate::fonts::UI)
            })
    };
    const ROW_GAP: f32 = 16.0;
    let width = groups
        .iter()
        .flatten()
        .map(|r| {
            super::sheet::text_width(r.label, ts::BASE, crate::fonts::UI)
                + key_w(&r.keys)
                + if r.ai { 12.0 + 6.0 } else { 0.0 }
                + 2.0 * 12.0
                + 2.0 * 4.0
                + 2.0
        })
        .fold(MENU_W, f32::max)
        .ceil();
    // Why what only the language server answers is not on offer: as it
    // is — its download declined, its start failed — and where that is
    // changed; "isn't ready" said of a server that would never be.
    let not_served = match lang.and_then(|l| app.proj.link.lsp.get(l)) {
        Some(crate::LspSlot::Declined(why) | crate::LspSlot::Unsupported(why)) => format!(
            "{}\n{}",
            crate::i18n::message(why),
            tr!("Open Settings → Language servers")
        ),
        Some(crate::LspSlot::Failed(error)) => {
            format!("{error}\n{}", tr!("Open Settings → Language servers"))
        }
        _ => tr!("The language server isn't ready").to_string(),
    };
    let row = |r: Row| -> Element<'a, Message> {
        let words = text(r.label).size(ts::BASE).wrapping(Wrapping::None);
        let words = if r.able {
            words
        } else {
            words.color(theme::dim())
        };
        let keys: Element<'a, Message> = match r.keys {
            Some(k) => text(k)
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(Wrapping::None)
                .into(),
            None => space().into(),
        };
        // What spends an AI call says so, at the row's end: these sat
        // among the free rows, alike.
        let trailing: Element<'a, Message> = if r.ai {
            row![keys, glyph::icon(Glyph::Sparkle, theme::accent(), 12.0)]
                .spacing(6)
                .align_y(iced::Center)
                .into()
        } else {
            keys
        };
        let b = button(
            row![words, space().width(Fill), trailing]
                .spacing(ROW_GAP)
                .align_y(iced::Center),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([5, 12])
        .on_press_maybe(r.able.then(|| super::heard::said(r.label, r.msg)));
        if r.able && r.ai {
            super::toolbar::tip_at(
                b,
                tr!("Uses one AI call"),
                None,
                iced::widget::tooltip::Position::Right,
            )
        } else if r.able {
            b.into()
        } else {
            super::toolbar::tip_at(
                b,
                not_served.clone(),
                None,
                iced::widget::tooltip::Position::Right,
            )
        }
    };
    let rule = || -> Element<'a, Message> {
        container(
            container(space())
                .width(Fill)
                .height(Length::Fixed(1.0))
                .style(|_: &iced::Theme| container::Style {
                    background: Some(theme::hairline().into()),
                    ..container::Style::default()
                }),
        )
        .padding([4, 8])
        .into()
    };
    let mut items: Vec<Element<'a, Message>> = Vec::new();
    for (i, group) in groups.into_iter().enumerate() {
        if i > 0 {
            items.push(rule());
        }
        items.extend(group.into_iter().map(row));
    }
    let panel = container(Column::with_children(items).spacing(1))
        .width(width)
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

/// A dim, short secondary description: first sentence, backticks stripped,
/// cut at `max` characters with an ellipsis, wrapped in a narrow panel.
/// Shared by the right-panel call-flow / contains rows so they match the
/// outline rows' treatment instead of hard-cutting mid-word.
pub(crate) fn one_line_desc<'a>(full: &str, max: usize) -> Element<'a, Message> {
    let one = truncate_ellipsis(&first_sentence(&strip_backticks(full)), max);
    // Wrapped, not clipped: cut at a narrow column's edge it ended mid-word
    // ("the final amount to charg"), with no sign more was there.
    container(
        text(one)
            .size(ts::CAPTION)
            .color(theme::dim())
            .wrapping(Wrapping::WordOrGlyph),
    )
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
