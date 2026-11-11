//! The setup sheet (see `crate::app::setup`): a first open's question —
//! where clew keeps its data, its language servers, explaining it — and
//! "Set up project…" and Explain All asking again.

use iced::widget::{checkbox, column, container, row, space, text};

use super::button;
use iced::{Element, Fill, Padding};

use super::chrome::{Backdrop, CONFIRM_PAD, DIALOG_W, Placement, modal};
use super::ts;
use crate::app::message::{Message, ProjectMsg};
use crate::app::setup::{ProjectSetup, SetupMode, explain_line, servers_line};
use crate::app::state::App;
use crate::theme;
use crate::{tr, trn};

/// Room under a tick for what it takes, lined up with its label.
const DETAIL_INDENT: f32 = 26.0;

/// The sheet's language-servers row, as `(ticked, changeable, line)`, or
/// none. Listed once the look is in; the row stands from the first frame,
/// ticked as it will be (they are set up unless the reader says not), and
/// its line keeps two lines' room: the look landing neither ticks it nor
/// grows the sheet under Open. Nothing to set up is done, not declined. A
/// project on another machine has nothing to say of them on a first open,
/// from the first frame on: its servers ask on its host.
pub(crate) fn servers_row(
    setup: Option<&ProjectSetup>,
    mode: SetupMode,
    remote: bool,
) -> Option<(bool, bool, String)> {
    match setup
        .and_then(|s| s.census.as_ref())
        .map(|c| c.servers.as_slice())
    {
        None => Some((true, false, tr!("Checking the project…").to_string())),
        Some([]) if remote && mode == SetupMode::Open => None,
        Some([]) => Some((true, false, tr!("Ready.").to_string())),
        Some(list) => Some((setup.is_some_and(|s| s.servers), true, servers_line(list))),
    }
}

/// The sheet for `root`: `setup` as it stands (a first open's before its
/// look at the project is in may not have one yet). `asked` holds back a
/// first open's Open until it can have been read, as every question's
/// Allow is.
pub(crate) fn setup_sheet<'a>(
    app: &'a App,
    root: &'a std::path::Path,
    setup: Option<&'a ProjectSetup>,
    asked: impl Into<crate::ui::placed::Asked>,
) -> Element<'a, Message> {
    let asked = asked.into();
    let mode = setup.map_or(SetupMode::Open, |s| s.mode);
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    let census = setup.and_then(|s| s.census.as_ref());
    let calls = setup.map(|s| s.census.as_ref().map(|c| c.calls));
    // What those calls are likely to cost, with the count of them: said
    // before anything is spent, not after.
    let counted = calls.flatten().flatten();
    let estimate = counted.and_then(|n| Some((n, app.explain_estimate(n)?)));
    // The count, its tokens and its money in one sentence of one weight —
    // the count stood bold over a dim line of the rest — and what else
    // explaining does on a line of its own.
    let (explain_says, explain_also) = match &estimate {
        Some((n, e)) => (
            e.said(*n),
            if app.embed_available {
                tr!("Also builds the semantic index.").to_string()
            } else {
                String::new()
            },
        ),
        None => (
            explain_line(app.llm_available, calls.flatten(), app.embed_available),
            String::new(),
        ),
    };

    let title = match mode {
        SetupMode::Open => tr!("Open {name}", name = name),
        SetupMode::Manage => tr!("Set up {name}", name = name),
        SetupMode::Explain => tr!("Explain {name}?", name = name),
    };
    let title = text(title).size(ts::TITLE).color(theme::fg());
    // A first open's ×, as Escape: the project is not opened — said in its
    // tip, beside Open without setup, which opens it.
    let header: Element<'a, Message> = if mode == SetupMode::Open {
        row![
            title,
            space().width(Fill),
            super::chrome::close_button_saying(
                // "No" to opening it.
                super::heard::answer(
                    tr!("Close without opening"),
                    super::heard::Answer::No,
                    Message::Project(ProjectMsg::ConsentDenied(root.to_path_buf())),
                ),
                tr!("Close without opening"),
            ),
        ]
        .align_y(iced::Center)
        .into()
    } else {
        // The other sheets' × too, as their Cancel: the first open's had
        // one, "Set up" none, and the same dialog looked two ways.
        row![
            title,
            space().width(Fill),
            super::chrome::close_button_saying(
                Message::Project(ProjectMsg::SetupClose),
                tr!("Close"),
            ),
        ]
        .align_y(iced::Center)
        .into()
    };
    let mut body = column![header].spacing(14);

    if mode == SetupMode::Open {
        body = body.push(
            column![
                row![
                    text(tr!("Project data folder:"))
                        .size(ts::BODY)
                        .color(theme::dim()),
                    text(format!("{}/.clew", root.display()))
                        .size(ts::BODY)
                        .font(crate::fonts::CODE)
                        .color(theme::fg_muted())
                        .wrapping(iced::widget::text::Wrapping::Glyph),
                ]
                .spacing(6)
                .align_y(iced::Center),
                // What of it git sees: its bookmarks and notes showed up
                // among the reader's uncommitted changes with no word why.
                text(tr!(
                    "Its bookmarks, notes and project settings can be committed with the project; your own reading state stays out of git."
                ))
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(iced::widget::text::Wrapping::Word),
            ]
            .spacing(4),
        );
    }

    let choice = |label: &'a str,
                  on: bool,
                  toggle: Option<fn(bool) -> Message>,
                  detail: String,
                  lines: f32,
                  more: Option<String>,
                  ai: bool|
     -> Element<'a, Message> {
        let tick = checkbox(on)
            .style(theme::check)
            .label(label)
            .size(16)
            .spacing(10)
            .text_size(ts::BASE);
        if let Some(toggle) = toggle {
            super::heard::switch(label, on, toggle);
        }
        let col = column![
            // A tick is pressed by its jump code and the keyboard's ring as
            // by a click.
            {
                let tick: Element<'a, Message> = match toggle {
                    Some(toggle) => super::jump::mark(tick.on_toggle(toggle), toggle(!on)),
                    None => tick.into(),
                };
                // What spends AI calls is marked so, as everywhere else.
                if ai {
                    row![
                        tick,
                        crate::glyph::icon(crate::glyph::Glyph::Sparkle, theme::accent(), 13.0),
                    ]
                    .spacing(6)
                    .align_y(iced::Center)
                    .into()
                } else {
                    tick
                }
            },
            container(row![
                // The room the line keeps, whatever it says for now: a pixel
                // wide (a row drops what has no width), taken off the indent.
                space()
                    .width(1)
                    .height(lines * text::LineHeight::default().to_absolute(ts::SMALL.into()).0),
                text(detail).size(ts::SMALL).color(theme::dim()),
            ])
            .padding(Padding {
                left: DETAIL_INDENT - 1.0,
                ..Padding::ZERO
            }),
        ];
        // A line of its own under the detail, kept while it is empty.
        let col = match more {
            Some(more) => col.push(
                container(row![
                    space()
                        .width(1)
                        .height(text::LineHeight::default().to_absolute(ts::SMALL.into()).0),
                    text(more).size(ts::SMALL).color(theme::dim()),
                ])
                .padding(Padding {
                    left: DETAIL_INDENT - 1.0,
                    ..Padding::ZERO
                }),
            ),
            None => col,
        };
        col.spacing(3).into()
    };

    match mode {
        SetupMode::Open | SetupMode::Manage => {
            if let Some((on, changeable, detail)) =
                servers_row(setup, mode, app.connection.is_remote())
            {
                let toggle = changeable.then_some(
                    (|on| Message::Project(ProjectMsg::SetupServers(on))) as fn(bool) -> Message,
                );
                body = body.push(choice(
                    tr!("Language servers"),
                    on,
                    toggle,
                    detail,
                    2.0,
                    None,
                    false,
                ));
            }
            let can_explain =
                app.llm_available && !matches!(census.map(|c| c.calls), Some(Some(0)));
            // Its line, and the estimate's under it: room for both kept
            // while the count is still coming.
            body = body.push(choice(
                tr!("Explain the project"),
                setup.is_some_and(|s| s.explain),
                can_explain.then_some(|on| Message::Project(ProjectMsg::SetupExplain(on))),
                explain_says,
                1.0,
                Some(explain_also),
                true,
            ));
            // No key: where one is added, a click away — the line said
            // "in Settings" and left the reader to find it.
            if !app.llm_available {
                body = body.push(
                    container(
                        button(
                            text(tr!("Add an API Key…"))
                                .size(ts::SMALL)
                                .color(theme::accent()),
                        )
                        .style(theme::icon_button(false))
                        .padding([2, 6])
                        .on_press(super::heard::said(
                            tr!("Add an API Key…"),
                            Message::Settings(crate::SettingsMsg::OpenAt(
                                crate::SettingsPage::LanguageModel,
                            )),
                        )),
                    )
                    .padding(Padding {
                        left: DETAIL_INDENT - 6.0,
                        ..Padding::ZERO
                    }),
                );
            }
        }
        SetupMode::Explain => {
            body = body.push(match estimate {
                // What it will likely take, as three figures of one weight:
                // the calls, their tokens, their money.
                Some((n, e)) => {
                    let tile = |value: String, label: String| -> Element<'a, Message> {
                        container(
                            column![
                                text(value)
                                    .size(ts::TITLE)
                                    .font(crate::fonts::UI_MEDIUM)
                                    .color(theme::fg()),
                                text(label).size(ts::SMALL).color(theme::dim()),
                            ]
                            .spacing(2),
                        )
                        .padding([10, 12])
                        .width(Fill)
                        .style(theme::form_card)
                        .into()
                    };
                    let cost = e.cost.map_or_else(|| "—".to_string(), crate::app::usage::money);
                    let mut notes = column![
                        text(e.basis())
                            .size(ts::SMALL)
                            .color(theme::dim())
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    ]
                    .spacing(2);
                    if e.cost.is_none() {
                        notes = notes.push(
                            text(tr!(
                                "Clew doesn't know this model's prices: enter them under Usage and cost to see what it costs."
                            ))
                            .size(ts::SMALL)
                            .color(theme::dim())
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                        );
                    }
                    if !explain_also.is_empty() {
                        notes = notes.push(
                            text(explain_also).size(ts::SMALL).color(theme::dim()),
                        );
                    }
                    column![
                        row![
                            tile(
                                crate::app::setup::about(n),
                                trn!("AI call", "AI calls", n).to_string()
                            ),
                            tile(
                                crate::app::usage::tokens(e.tokens),
                                tr!("Tokens").to_string()
                            ),
                            tile(cost, tr!("Estimated cost").to_string()),
                        ]
                        .spacing(8),
                        notes,
                    ]
                    .spacing(10)
                    .into()
                }
                None => Element::from(text(explain_says).size(ts::BODY).color(theme::fg_muted())),
            });
        }
    }

    let (decline, accept, accept_msg, gated) = match mode {
        SetupMode::Open => (
            Message::Project(ProjectMsg::ConsentDenied(root.to_path_buf())),
            tr!("Open"),
            Message::Project(ProjectMsg::ConsentAllowed(root.to_path_buf())),
            true,
        ),
        SetupMode::Manage => (
            Message::Project(ProjectMsg::SetupClose),
            tr!("Apply"),
            Message::Project(ProjectMsg::SetupApply),
            false,
        ),
        SetupMode::Explain => (
            Message::Project(ProjectMsg::SetupClose),
            tr!("Explain"),
            Message::Project(ProjectMsg::SetupApply),
            false,
        ),
    };
    // A first open's Open without setup opens the project, with nothing set up: an
    // answer to the question as Open is, so pressed only once it can have
    // been read. Not opening it at all is the ×, or Escape.
    let (decline_label, decline, decline_gated) = if mode == SetupMode::Open {
        (
            tr!("Open without setup"),
            Message::Project(ProjectMsg::OpenWithoutSetup(root.to_path_buf())),
            true,
        )
    } else {
        (tr!("Cancel"), decline, false)
    };
    // Something to do: a first open always opens; Apply needs a tick that
    // does something; Explain, something left to explain.
    let something = match (mode, setup) {
        (SetupMode::Open, _) => true,
        (SetupMode::Manage, Some(s)) => {
            (s.servers && census.is_some_and(|c| !c.servers.is_empty())) || s.explain
        }
        (SetupMode::Explain, Some(s)) => {
            s.explain && !matches!(census.map(|c| c.calls), Some(Some(0)))
        }
        _ => false,
    };
    // Open sets up what the look found: held until it is in, so it is not
    // pressed for less than the sheet is about to say — nor where the line
    // above it is still settling.
    let looking = setup.is_some() && census.is_none();
    let ready = (!gated || asked.ready) && something && !looking;
    let accept_button = button(text(accept).size(ts::BASE))
        .style(theme::primary_button)
        .padding([6, 16])
        .on_press_maybe(
            ready.then(|| super::heard::answer(accept, super::heard::Answer::Yes, accept_msg)),
        );
    let decline_ready = !decline_gated || asked.ready;
    let decline_button = button(text(decline_label).size(ts::BASE))
        .style(theme::toolbar_button)
        .padding([6, 16])
        .on_press_maybe(decline_ready.then(|| {
            // Open without setup opens the project: no "no" to opening it, which
            // the × is; Cancel is.
            if decline_gated {
                super::heard::said(decline_label, decline)
            } else {
                super::heard::answer(decline_label, super::heard::Answer::No, decline)
            }
        }));
    let decline_button: Element<'a, Message> = if decline_gated {
        // What it leaves out, said where the pointer rests: beside Open, it
        // opened the project too, and said nothing of the ticks it ignored.
        super::toolbar::tip_at(
            crate::ui::fresh::allow(decline_ready, decline_button),
            tr!("Open the project and set nothing up. The choices above are ignored."),
            None,
            iced::widget::tooltip::Position::Top,
        )
    } else {
        decline_button.into()
    };
    // Held while the look is on its way, it says why where the pointer
    // rests: inert with nothing said, it read as broken (L24).
    let accept_button = crate::ui::fresh::allow(ready, accept_button);
    let accept_button = if looking {
        super::toolbar::tip_at(
            accept_button,
            tr!("Ready once the project is checked"),
            None,
            iced::widget::tooltip::Position::Top,
        )
    } else if gated && !asked.ready {
        // Held a moment, so it is not pressed before the sheet is read —
        // or while the window is behind another: a press then did nothing,
        // with no word why.
        super::toolbar::tip_at(
            accept_button,
            tr!("Ready in a moment, once the sheet is in front"),
            None,
            iced::widget::tooltip::Position::Top,
        )
    } else {
        accept_button
    };
    // Why its main button waits, said beside it: greyed with nothing said,
    // Apply read as broken.
    let why_not: Option<&str> = (!something && !looking).then(|| match (mode, setup) {
        (SetupMode::Manage, Some(s))
            if !s.explain && census.is_some_and(|c| c.servers.is_empty()) =>
        {
            tr!("Nothing to apply: the language servers are already set up.")
        }
        (SetupMode::Explain, Some(s)) if s.explain => tr!("Nothing is left to explain."),
        _ => tr!("Nothing to apply: tick what to set up."),
    });
    let left: Element<'a, Message> = match why_not {
        Some(why) => text(why)
            .size(ts::SMALL)
            .color(theme::dim())
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Fill)
            .into(),
        None => space().width(Fill).into(),
    };
    body = body.push(
        row![left, decline_button, accept_button,]
            .spacing(10)
            .align_y(iced::Center),
    );

    let panel = container(body)
        .width(DIALOG_W)
        .padding(CONFIRM_PAD)
        .style(theme::modal_panel);
    // Fresh for each project asked about: a press held on another's Open is
    // not this one's.
    let panel = crate::ui::placed::placed(&asked, crate::ui::fresh::fresh_for(&root, panel));
    modal(panel, Placement::Center, Backdrop::Dim(None))
}
