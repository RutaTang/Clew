//! The setup sheet (see `crate::app::setup`): a first open's question —
//! where clew keeps its data, its language servers, explaining it — and
//! "Set up project…" and Explain All asking again.

use iced::widget::{button, checkbox, column, container, row, space, text};
use iced::{Element, Fill, Padding};

use super::chrome::{Backdrop, CONFIRM_PAD, DIALOG_W, Placement, modal};
use super::ts;
use crate::app::message::{Message, ProjectMsg};
use crate::app::setup::{ProjectSetup, SetupMode, explain_line, servers_line};
use crate::app::state::App;
use crate::theme;
use crate::tr;

/// Room under a tick for what it takes, lined up with its label.
const DETAIL_INDENT: f32 = 26.0;

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
    let explain_says = explain_line(app.llm_available, calls.flatten(), app.embed_available);

    let title = match mode {
        SetupMode::Open => tr!("Open {name}", name = name),
        SetupMode::Manage => tr!("Set up {name}", name = name),
        SetupMode::Explain => tr!("Explain {name}?", name = name),
    };
    let title = text(title).size(ts::TITLE).color(theme::fg());
    // A first open's ×, as Escape: the project is not opened.
    let header: Element<'a, Message> = if mode == SetupMode::Open {
        row![
            title,
            space().width(Fill),
            super::chrome::close_button(Message::Project(ProjectMsg::ConsentDenied(
                root.to_path_buf()
            ))),
        ]
        .align_y(iced::Center)
        .into()
    } else {
        title.into()
    };
    let mut body = column![header].spacing(14);

    if mode == SetupMode::Open {
        body = body.push(
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
        );
    }

    let choice = |label: &'a str,
                  on: bool,
                  toggle: Option<fn(bool) -> Message>,
                  detail: String|
     -> Element<'a, Message> {
        let tick = checkbox(on)
            .style(theme::check)
            .label(label)
            .size(16)
            .spacing(10)
            .text_size(ts::BASE);
        column![
            match toggle {
                Some(toggle) => tick.on_toggle(toggle),
                None => tick,
            },
            container(text(detail).size(ts::SMALL).color(theme::dim())).padding(Padding {
                left: DETAIL_INDENT,
                ..Padding::ZERO
            }),
        ]
        .spacing(3)
        .into()
    };

    match mode {
        SetupMode::Open | SetupMode::Manage => {
            // The servers: listed once the look is in; a first open with
            // none to set up says nothing of them.
            let servers = census.map(|c| c.servers.as_slice());
            match servers {
                None => {
                    body = body.push(choice(
                        tr!("Language servers"),
                        false,
                        None,
                        tr!("Checking the project…").into(),
                    ))
                }
                Some([]) if mode == SetupMode::Open => {}
                Some([]) => {
                    body = body.push(choice(
                        tr!("Language servers"),
                        false,
                        None,
                        tr!("Ready.").into(),
                    ))
                }
                Some(list) => {
                    body = body.push(choice(
                        tr!("Language servers"),
                        setup.is_some_and(|s| s.servers),
                        Some(|on| Message::Project(ProjectMsg::SetupServers(on))),
                        servers_line(list),
                    ));
                }
            }
            let can_explain =
                app.llm_available && !matches!(census.map(|c| c.calls), Some(Some(0)));
            body = body.push(choice(
                tr!("Explain the project"),
                setup.is_some_and(|s| s.explain),
                can_explain.then_some(|on| Message::Project(ProjectMsg::SetupExplain(on))),
                explain_says,
            ));
        }
        SetupMode::Explain => {
            body = body.push(text(explain_says).size(ts::BODY).color(theme::fg_muted()));
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
    // A first open's Not now opens the project, with nothing set up: an
    // answer to the question as Open is, so pressed only once it can have
    // been read. Not opening it at all is the ×, or Escape.
    let (decline_label, decline, decline_gated) = if mode == SetupMode::Open {
        (
            tr!("Not now"),
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
    let ready = (!gated || asked.ready) && something;
    let accept_button = button(text(accept).size(ts::BASE))
        .style(theme::primary_button)
        .padding([6, 16])
        .on_press_maybe(ready.then_some(accept_msg));
    let decline_ready = !decline_gated || asked.ready;
    let decline_button = button(text(decline_label).size(ts::BASE))
        .style(theme::toolbar_button)
        .padding([6, 16])
        .on_press_maybe(decline_ready.then_some(decline));
    let decline_button: Element<'a, Message> = if decline_gated {
        crate::ui::fresh::allow(decline_ready, decline_button)
    } else {
        decline_button.into()
    };
    body = body.push(
        row![
            space().width(Fill),
            decline_button,
            crate::ui::fresh::allow(ready, accept_button),
        ]
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
