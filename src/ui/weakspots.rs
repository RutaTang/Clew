//! The weak spots page (see `app::weakspots`): the functions that ask for
//! attention first, each with why, a way to its code and a question to Ask.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use clew_core::weakspots::{Reason, Risk};

use crate::WeakMsg;

/// A reason, in the reader's words: `212 lines`, `nested 5 deep`,
/// `3 unwraps`, `changed in 24 commits`.
pub(crate) fn reason_words(reason: &Reason) -> String {
    match *reason {
        Reason::Long(n) => trn!("1 line", "{n} lines", n),
        Reason::Deep(n) => tr!("nested {n} deep", n = n),
        Reason::Branchy(n) => trn!("1 branch", "{n} branches", n),
        Reason::Risky(risk, n) => match risk {
            Risk::Unwrap => trn!("1 unwrap", "{n} unwraps", n),
            Risk::Panic => trn!("1 panic", "{n} panics", n),
            Risk::Unsafe => trn!("1 unsafe block", "{n} unsafe blocks", n),
            Risk::Eval => trn!("runs text as code once", "runs text as code {n} times", n),
            Risk::Shell => trn!("1 shell command", "{n} shell commands", n),
            Risk::UnboundedCopy => trn!("1 unbounded copy", "{n} unbounded copies", n),
            Risk::Deserialize => trn!("1 deserialization", "{n} deserializations", n),
            Risk::RawHtml => trn!("writes raw HTML once", "writes raw HTML {n} times", n),
            Risk::CatchAll => trn!("1 catch-all", "{n} catch-alls", n),
        },
        Reason::Swallowed(n) => trn!("swallows 1 error", "swallows {n} errors", n),
        Reason::Marked(n) => trn!("1 TODO or FIXME", "{n} TODOs or FIXMEs", n),
        Reason::Churn(n) => trn!("changed in 1 commit", "changed in {n} commits", n as usize),
    }
}

/// The page: what it is, then each spot, highest first.
pub(crate) fn weak_home(app: &App) -> Element<'_, Message> {
    let weak = &app.proj.weak;
    let close = || Message::Weak(WeakMsg::Close);
    let refresh = super::toolbar::busy_action(
        Glyph::Refresh,
        tr!("Refresh"),
        Some(Message::Weak(WeakMsg::Refresh)),
        weak.building,
    );
    let header = row![
        text(tr!("Weak Spots")).size(ts::HEADING).color(theme::fg()),
        space().width(Fill),
        refresh,
        space().width(8),
        super::chrome::close_button(close()),
    ]
    .align_y(iced::Center);
    let Some(spots) = weak.spots.as_ref() else {
        let words = match (&weak.failed, weak.building) {
            (Some(why), _) => tr!("No weak spots: {why}", why = why.clone()),
            (None, true) => tr!("Looking for weak spots…").to_string(),
            (None, false) => String::new(),
        };
        return container(
            column![header, text(words).size(ts::BASE).color(theme::dim())].spacing(14),
        )
        .width(Fill)
        .height(Fill)
        .padding([20, 28])
        .into();
    };
    if spots.is_empty() {
        return container(
            column![
                header,
                text(tr!(
                    "Nothing stands out: no function is over {lines} lines, nests more than {depth} levels deep, has more than {branches} branches, or makes a risky call such as eval or a shell command.",
                    lines = clew_core::weakspots::LONG_LINES,
                    depth = clew_core::weakspots::DEEP_NESTING,
                    branches = clew_core::weakspots::MANY_BRANCHES
                ))
                .size(ts::BASE)
                .color(theme::dim()),
            ]
            .spacing(14),
        )
        .width(Fill)
        .height(Fill)
        .padding([20, 28])
        .into();
    }
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for (i, spot) in spots.iter().enumerate() {
        let name = if spot.name.is_empty() {
            tr!("notes in the file").to_string()
        } else {
            spot.name.clone()
        };
        let place = format!("{}:{}", spot.rel, spot.line);
        let chips = Row::with_children(spot.reasons.iter().map(|r| {
            container(
                text(reason_words(r))
                    .size(ts::CAPTION)
                    .color(theme::fg_muted())
                    .wrapping(Wrapping::None),
            )
            .padding([1, 6])
            .style(theme::chip)
            .into()
        }))
        .spacing(4)
        .wrap();
        let open = button(
            column![
                row![
                    text(name.clone())
                        .size(ts::BASE)
                        .font(crate::fonts::CODE)
                        .color(theme::fg())
                        .wrapping(Wrapping::None),
                    text(place)
                        .size(ts::SMALL)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                ]
                .spacing(10)
                .align_y(iced::Center),
                chips,
            ]
            .spacing(4),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([6, 8])
        .on_press(super::heard::row(
            || name.clone(),
            Message::Weak(WeakMsg::Open(i)),
        ));
        let ask = button(text(tr!("Ask why")).size(ts::SMALL))
            .style(theme::toolbar_button)
            .padding([3, 10])
            .on_press(super::heard::said(
                tr!("Ask why"),
                Message::Weak(WeakMsg::AskWhy(i)),
            ));
        rows.push(row![open, ask].spacing(8).align_y(iced::Center).into());
    }
    let count = text(trn!(
        "1 spot, from {files} files measured",
        "{n} spots, from {files} files measured",
        spots.len(),
        files = weak.files
    ))
    .size(ts::SMALL)
    .color(theme::dim());
    rows.push(super::askfloat::pill_foot());
    container(
        column![
            header,
            count,
            scrollable(
                Column::with_children(rows)
                    .spacing(2)
                    .width(Fill)
                    .max_width(960)
            )
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
        ]
        .spacing(12),
    )
    .width(Fill)
    .height(Fill)
    .padding([20, 28])
    .into()
}
