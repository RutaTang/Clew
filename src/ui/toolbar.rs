//! Top toolbar, tools menu, and target menu.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// Wrap a bare-icon toolbar control with a hover tooltip showing its name and,
/// when it has one, its current keyboard shortcut. Icon buttons carry no visible
/// label, so the tooltip is where a reader learns what each one does.
pub(crate) fn chrome_tip<'a>(
    control: impl Into<Element<'a, Message>>,
    name: &'a str,
    shortcut: Option<String>,
) -> Element<'a, Message> {
    let mut body = row![text(name).size(12).color(theme::FG)].spacing(10).align_y(iced::Center);
    if let Some(sc) = shortcut {
        body = body.push(text(sc).size(12).color(theme::DIM));
    }
    let bubble = container(body)
        .padding(Padding { top: 3.0, right: 8.0, bottom: 3.0, left: 8.0 })
        .style(theme::modal_panel);
    tooltip(control, bubble, tooltip::Position::Bottom).gap(6).into()
}

pub(crate) fn toolbar(app: &App) -> Element<'_, Message> {
    // Nav arrows use the embedded Nerd Font (not a raw Unicode arrow) so they
    // share a baseline with the panel-toggle icons; mixing glyphs pulled from
    // different fallback fonts left the toolbar icons visibly misaligned.
    let nav = |glyph: Glyph, enabled: bool, msg: Message| {
        let color = if enabled { theme::FG } else { theme::DIM };
        let mut b = button(glyph::icon(glyph, color, 18.0))
            .style(theme::toolbar_button)
            .padding([2, 8]);
        if enabled {
            b = b.on_press(msg);
        }
        b
    };
    // A bare-icon toolbar action. The label lives in a hover tooltip (via
    // `chrome_tip`), so the bar reads as a clean row of glyphs that name
    // themselves on hover — matching the nav/sidebar icons beside it.
    let tool_icon = |glyph: Glyph, label: &'static str, msg: Message| {
        chrome_tip(
            button(glyph::icon(glyph, theme::FG, 18.0))
                .style(theme::toolbar_button)
                .padding([3, 9])
                .on_press(msg),
            label,
            None,
        )
    };
    // A layout-toggle icon (bright = panel shown, dim = hidden), hand-drawn to
    // match the nav arrows beside it.
    let panel_toggle = |glyph: Glyph, shown: bool, msg: Message| {
        button(glyph::icon(glyph, if shown { theme::FG } else { theme::DIM }, 18.0))
            .style(theme::toolbar_button)
            .padding([2, 6])
            .on_press(msg)
    };

    // Breadcrumb: dim folders › bright filename, for orientation while reading.
    let breadcrumb: Element<'_, Message> = match app.active_viewer() {
        Some(v) => {
            let parts: Vec<&str> = v.rel.split('/').collect();
            let mut r = Row::new().spacing(5).align_y(iced::Center);
            for (i, seg) in parts.iter().enumerate() {
                if i > 0 {
                    r = r.push(text("›").size(12).color(theme::DIM));
                }
                let last = i + 1 == parts.len();
                if last {
                    // The filename gets its file-type icon, kept tight to the name.
                    // Clickable: refocuses the code view, so opening a Stats /
                    // Overview / Docs page still leaves a one-click way back to the
                    // file (the page otherwise hides the code with no return path).
                    let (glyph, color) = crate::icons::file_icon(seg);
                    r = r.push(
                        button(
                            row![
                                icon_text(glyph, color, 13.0),
                                text(seg.to_string()).size(13).color(theme::FG),
                            ]
                            .spacing(4)
                            .align_y(iced::Center),
                        )
                        .style(theme::toolbar_button)
                        .padding([2, 4])
                        .on_press(Message::OpenAbs {
                            abs: v.abs.clone(),
                            line: None,
                            push: false,
                        }),
                    );
                } else {
                    r = r.push(text(seg.to_string()).size(13).color(theme::DIM));
                }
            }
            r.into()
        }
        None => text("").into(),
    };

    // Custom window controls (the frameless window has no OS buttons): a row of
    // macOS-style red/amber/green circles. Being real buttons, they capture
    // their own clicks, so dragging from them never moves the window. Like
    // native traffic lights, they show glyphs while the pointer is over the
    // cluster, and grey out when the window has no focus (unless hovered).
    let show_icon = app.controls_hovered;
    let colored = app.window_focused || app.controls_hovered;
    let glyph_color = theme::with_alpha(theme::rgb(0x000000), 0.6);
    let light = move |color: iced::Color, icon: TrafficIcon, msg: Message| {
        let content: Element<'_, Message> = if show_icon {
            iced::widget::canvas::Canvas::new(TrafficGlyph { icon, color: glyph_color })
                .width(12)
                .height(12)
                .into()
        } else {
            space().width(12).height(12).into()
        };
        button(content)
            .style(move |_theme, status: button::Status| {
                let bg = if !colored {
                    theme::rgb(0x8b8b8b) // grey while the window is unfocused
                } else {
                    match status {
                        // Native traffic lights keep full colour on hover (only the
                        // glyph appears); they darken slightly only on an actual press.
                        button::Status::Pressed => theme::with_alpha(color, 0.82),
                        _ => color,
                    }
                };
                button::Style {
                    background: Some(bg.into()),
                    border: iced::Border { radius: 6.0.into(), ..Default::default() },
                    ..button::Style::default()
                }
            })
            .padding(0)
            .on_press(msg)
    };
    // No text tooltips on the traffic lights — native ones show only the glyph
    // on hover, and a "Fullscreen" bubble popping up looks out of place.
    let controls = mouse_area(
        row![
            light(theme::rgb(0xff5f57), TrafficIcon::Close, Message::CloseWindow),
            light(theme::rgb(0xfebc2e), TrafficIcon::Minimize, Message::MinimizeWindow),
            light(
                theme::rgb(0x28c840),
                TrafficIcon::Fullscreen(app.fullscreen),
                Message::ToggleFullscreen
            ),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .on_enter(Message::ControlsHover(true))
    .on_exit(Message::ControlsHover(false));

    // Left cluster: window controls · layout toggle · back/forward · breadcrumb.
    let mut left = row![
        controls,
        space().width(6),
        // Codicons (VS Code's icon set): sidebar toggle + arrows all come from
        // the same family, so they share one baseline and sit on a line.
        chrome_tip(
            panel_toggle(Glyph::PanelLeft, app.show_left_sidebar, Message::ToggleLeftSidebar),
            "Toggle sidebar",
            None,
        ),
        chrome_tip(
            nav(Glyph::ArrowLeft, app.history.can_back(), Message::GoBack),
            "Back",
            Some(app.keymap.chord(crate::keymap::Action::GoBack).caps()),
        ),
        chrome_tip(
            nav(Glyph::ArrowRight, app.history.can_forward(), Message::GoForward),
            "Forward",
            Some(app.keymap.chord(crate::keymap::Action::GoForward).caps()),
        ),
        breadcrumb,
    ]
    .spacing(6)
    .align_y(iced::Center);
    // A markdown file gets a rendered/source toggle beside the breadcrumb.
    if let Some(v) = app.active_viewer().filter(|v| v.md.is_some()) {
        let (glyph, tip) = if v.show_source {
            (Glyph::Overview, "Show rendered")
        } else {
            (Glyph::Note, "Show source")
        };
        left = left.push(tool_icon(glyph, tip, Message::ToggleMarkdownSource(app.active)));
    }

    // Primary reading actions stay on the bar; everything else moves to "More".
    // Hand-drawn line icons (see `glyph`), one family with the traffic lights.
    // Each names itself on hover.
    let core = row![
        tool_icon(Glyph::Overview, "Overview", Message::ShowOverview),
        tool_icon(Glyph::Stats, "Stats", Message::ShowStats),
        tool_icon(Glyph::Ask, "Ask", Message::ToggleAsk),
        tool_icon(Glyph::Debug, "Debug", Message::StartDebug),
        tool_icon(Glyph::CallGraph, "Call Graph", Message::OpenOverlay(crate::Overlay::ProjectCalls)),
        tool_icon(Glyph::ImportGraph, "Import Graph", Message::OpenOverlay(crate::Overlay::ProjectImports)),
        tool_icon(Glyph::Settings, "Settings", Message::OpenSettings),
    ]
    .spacing(4)
    .align_y(iced::Center);

    let divider = text("│").size(15).color(theme::HAIRLINE);
    let more = button(text("⋯").size(17).color(if app.show_tools_menu { theme::FG } else { theme::DIM }))
        .style(theme::toolbar_button)
        .padding([0, 9])
        .on_press(Message::ToggleToolsMenu);

    let right = row![
        core,
        divider,
        chrome_tip(more, "More", None),
        chrome_tip(
            panel_toggle(Glyph::PanelRight, app.show_right_panel, Message::ToggleRightPanel),
            "Toggle panel",
            None,
        ),
    ]
    .spacing(8)
    .align_y(iced::Center);

    // clew draws its own window controls, so just a small margin from the edge.
    let bar = row![left, space().width(Fill), right].align_y(iced::Center).padding(Padding {
        top: 0.0,
        right: 12.0,
        bottom: 0.0,
        left: 12.0,
    });
    // A fixed title-bar height with vertically-centered content. The whole
    // toolbar is the window's drag region; its buttons (including the window
    // controls) capture their own clicks, so only empty areas start a drag.
    mouse_area(
        container(bar)
            .width(Fill)
            .height(Length::Fixed(38.0))
            .align_y(iced::Center)
            .style(theme::panel),
    )
    .on_press(Message::TitleBarDragged)
    .into()
}

/// The toolbar's "More" overflow menu: the secondary actions that don't need to
/// crowd the bar. Positioned under the "⋯" button (top-right).
pub(crate) fn tools_menu(app: &App) -> Element<'_, Message> {
    // Each row is icon + label (like a native macOS menu). Toggles show a
    // trailing accent check when active; the icon sits in a fixed gutter so
    // every label lines up.
    let menu_icon = |glyph: Glyph| {
        container(glyph::icon(glyph, theme::FG_MUTED, 17.0))
            .width(26)
            .align_x(iced::alignment::Horizontal::Center)
    };
    let toggle_item = |glyph: Glyph, label: &str, checked: bool, msg: Message| {
        let trailing: Element<'_, Message> = if checked {
            text("✓").size(12).color(theme::ACCENT).into()
        } else {
            space().into()
        };
        button(
            row![
                menu_icon(glyph),
                text(label.to_string()).size(13),
                space().width(Fill),
                trailing,
            ]
            .spacing(8)
            .align_y(iced::Center),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([5, 10])
        .on_press(msg)
    };
    let action_item = |glyph: Glyph, label: &str, msg: Message| {
        button(
            row![menu_icon(glyph), text(label.to_string()).size(13)]
                .spacing(8)
                .align_y(iced::Center),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([5, 10])
        .on_press(msg)
    };
    // Explain All lives here now; it carries its own progress and, with no LLM
    // key configured, routes to Settings instead. While a pass runs this entry
    // turns into a Cancel control — the bottom-up pass is thousands of LLM calls
    // on a big repo, so it must be stoppable.
    let explain: Element<'_, Message> = if app.explain.running {
        let label = match app.explain.progress {
            Some((done, total)) if total > 0 => format!("Cancel explaining ({done}/{total})"),
            _ => "Cancel explaining".to_string(),
        };
        button(
            row![menu_icon(Glyph::Close), text(label).size(13)]
                .spacing(8)
                .align_y(iced::Center),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([5, 10])
        .on_press(Message::CancelExplain)
        .into()
    } else {
        let mut btn = button(
            row![menu_icon(Glyph::Sparkle), text("Explain All").size(13)]
                .spacing(8)
                .align_y(iced::Center),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([5, 10]);
        if app.llm_available {
            btn = btn.on_press(Message::ExplainProject);
        } else {
            btn = btn.on_press(Message::OpenSettings);
        }
        btn.into()
    };
    // Three groups, hairline-separated: view toggles, then open-project actions,
    // then content/analysis actions.
    let separator = container(hairline()).padding(Padding {
        top: 4.0,
        right: 6.0,
        bottom: 4.0,
        left: 6.0,
    });
    let panel = container(
        column![
            toggle_item(Glyph::Note, "Summaries", app.show_inline_summaries, Message::ToggleInlineSummaries),
            toggle_item(Glyph::Info, "File summary", app.show_file_banner, Message::ToggleFileBanner),
            toggle_item(Glyph::Lightbulb, "Inlay hints", app.show_inlay_hints, Message::ToggleInlayHints),
            toggle_item(Glyph::Minimap, "Minimap", app.show_minimap, Message::ToggleMinimap),
            separator,
            action_item(Glyph::Folder, "Open Folder…", Message::OpenFolderPressed),
            action_item(Glyph::Remote, "Open Remote…", Message::OpenConnect),
            explain,
            action_item(Glyph::Compass, "Walkthrough", Message::SidebarTabPicked(SidebarTab::Walk)),
            action_item(Glyph::Skim, "Skim (fold bodies)", Message::SkimFile),
            action_item(Glyph::Diff, "Diff", Message::ToggleDiff),
            action_item(Glyph::TimeTravel, "Time travel", Message::TimeTravelStart { symbol: false }),
            action_item(Glyph::Servers, "LSP Servers", Message::ToggleServerPanel),
            action_item(Glyph::Shortcuts, "Keyboard Shortcuts", Message::OpenShortcuts),
        ]
        .spacing(1),
    )
    .width(224)
    .padding(4)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .padding(Padding { top: 44.0, right: 56.0, bottom: 0.0, left: 0.0 });
    opaque(mouse_area(positioned).on_press(Message::ToggleToolsMenu))
}

/// The status-bar `#[cfg]` target dropdown: which platform's cfg branches read
/// as live. A hand-rolled popup (not a `pick_list`, which pads to its widest
/// option and leaves a gap before the chevron) anchored to the bottom-right so
/// it hugs its trigger button.
pub(crate) fn target_menu(app: &App) -> Element<'_, Message> {
    let current = app.reading_target.clone();
    let items: Vec<Element<'_, Message>> = crate::inactive::Target::presets()
        .into_iter()
        .map(|t| {
            let selected = t == current;
            let mark: Element<'_, Message> = if selected {
                text("✓").size(11).color(theme::ACCENT).into()
            } else {
                space().into()
            };
            button(row![container(mark).width(15), text(t.to_string()).size(12)].align_y(iced::Center))
                .style(theme::list_row(selected))
                .width(Fill)
                .padding([5, 10])
                .on_press(Message::TargetSelected(t))
                .into()
        })
        .collect();
    let panel = container(Column::with_children(items).spacing(1))
        .width(172)
        .padding(4)
        .style(theme::modal_panel);
    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Bottom)
        .padding(Padding { top: 0.0, right: 12.0, bottom: 30.0, left: 0.0 });
    opaque(mouse_area(positioned).on_press(Message::ToggleTargetMenu))
}

// ---------------------------------------------------------------- sidebar

