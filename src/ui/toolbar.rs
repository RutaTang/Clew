//! Top toolbar, tools menu, and target menu.
//!
//! The bar's geometry is fixed and published here (`TOOLBAR_H`, the icon and
//! ⋯-menu positions, [`TOOLS_MENU_ROWS`]) so the tutorial's spotlight is
//! computed from the same numbers the bar is drawn with.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::row;

use crate::keymap::Action;

/// Height of the title bar / toolbar.
pub(crate) const TOOLBAR_H: f32 = 38.0;
/// Horizontal inset of the bar's two clusters from the window edges.
const BAR_PAD_X: f32 = 12.0;
/// Size of a toolbar icon glyph.
const TOOL_ICON: f32 = 18.0;
/// Horizontal padding inside a tool icon button.
const TOOL_PAD_X: f32 = 9.0;
/// Width of one tool icon button.
const TOOL_W: f32 = TOOL_ICON + 2.0 * TOOL_PAD_X;
/// Gap between neighbouring tool icons.
const TOOL_SPACING: f32 = 4.0;
/// Gap between the groups of the right cluster.
const CLUSTER_SPACING: f32 = 8.0;
/// Width of the ⋯ (More) button.
const MORE_W: f32 = 34.0;
/// Width of the right-panel toggle.
const PANEL_TOGGLE_W: f32 = TOOL_ICON + 12.0;
/// Width of the rule between the tool icons and the ⋯ button.
const DIVIDER_W: f32 = 1.0;
/// The primary tool icons on the bar, left → right.
pub(crate) const CORE_TOOLS: usize = 7;

/// Center x of core tool icon `i` (0-based, left → right) in a window
/// `window_w` wide. The right cluster is right-aligned with fixed-width parts,
/// so each icon sits a fixed distance in from the right edge.
pub(crate) fn toolbar_icon_center_x(window_w: f32, i: usize) -> f32 {
    let core_right =
        window_w - BAR_PAD_X - PANEL_TOGGLE_W - MORE_W - DIVIDER_W - 3.0 * CLUSTER_SPACING;
    let after = CORE_TOOLS.saturating_sub(i + 1) as f32;
    core_right - after * (TOOL_W + TOOL_SPACING) - TOOL_W / 2.0
}

/// Center x of the ⋯ (More) button in a window `window_w` wide.
pub(crate) fn toolbar_more_center_x(window_w: f32) -> f32 {
    window_w - BAR_PAD_X - PANEL_TOGGLE_W - CLUSTER_SPACING - MORE_W / 2.0
}

/// A row of the toolbar's ⋯ menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolsRow {
    OutlineSummaries,
    FileSummary,
    InlayHints,
    Minimap,
    Tutorial,
    OpenFolder,
    OpenRemote,
    ExplainAll,
    Walkthrough,
    Skim,
    Diff,
    TimeTravel,
    LspServers,
    Shortcuts,
}

/// The ⋯ menu's rows, top to bottom. The menu is built from this list, and the
/// tutorial points at rows by their position in it — so the two cannot drift.
pub(crate) const TOOLS_MENU_ROWS: [ToolsRow; 14] = [
    ToolsRow::OutlineSummaries,
    ToolsRow::FileSummary,
    ToolsRow::InlayHints,
    ToolsRow::Minimap,
    ToolsRow::Tutorial,
    ToolsRow::OpenFolder,
    ToolsRow::OpenRemote,
    ToolsRow::ExplainAll,
    ToolsRow::Walkthrough,
    ToolsRow::Skim,
    ToolsRow::Diff,
    ToolsRow::TimeTravel,
    ToolsRow::LspServers,
    ToolsRow::Shortcuts,
];

/// How many rows at the top of the menu are view toggles; a separator follows.
pub(crate) const TOOLS_MENU_TOGGLES: usize = 4;

impl ToolsRow {
    /// This row's position in [`TOOLS_MENU_ROWS`] (0 = top).
    pub(crate) const fn index(self) -> usize {
        let mut i = 0;
        while i < TOOLS_MENU_ROWS.len() {
            if TOOLS_MENU_ROWS[i] as u8 == self as u8 {
                return i;
            }
            i += 1;
        }
        // Every variant is listed (a test enforces it); unreachable otherwise.
        TOOLS_MENU_ROWS.len()
    }

    /// The rebindable command this row also has, whose chord the row shows.
    fn action(self) -> Option<Action> {
        Some(match self {
            ToolsRow::ExplainAll => Action::ExplainAll,
            ToolsRow::Walkthrough => Action::Walkthrough,
            ToolsRow::Diff => Action::ToggleDiff,
            ToolsRow::TimeTravel => Action::TimeTravel,
            ToolsRow::LspServers => Action::LspServers,
            ToolsRow::Shortcuts => Action::Shortcuts,
            _ => return None,
        })
    }
}

/// Distance of the ⋯ menu panel from the window's top edge.
pub(crate) const TOOLS_MENU_TOP: f32 = TOOLBAR_H + 6.0;
/// Distance of the ⋯ menu panel from the window's right edge.
pub(crate) const TOOLS_MENU_RIGHT: f32 = 56.0;
/// Width of the ⋯ menu panel.
pub(crate) const TOOLS_MENU_W: f32 = 224.0;
/// Inner padding of the ⋯ menu panel.
const TOOLS_MENU_PAD: f32 = 4.0;
/// Height of one ⋯ menu row.
const TOOLS_ROW_H: f32 = 27.0;
/// Gap between ⋯ menu rows.
const TOOLS_ROW_SPACING: f32 = 1.0;
/// Height of the separator between the toggles and the actions.
const TOOLS_SEP_H: f32 = 9.0;

/// Window-space rectangle covering `count` consecutive ⋯ menu rows starting at
/// row `first`, spanning the item column (inside the panel's padding).
pub(crate) fn tools_menu_rows_rect(window_w: f32, first: usize, count: usize) -> iced::Rectangle {
    let top_of = |i: usize| {
        let sep = if i >= TOOLS_MENU_TOGGLES {
            TOOLS_SEP_H + TOOLS_ROW_SPACING
        } else {
            0.0
        };
        TOOLS_MENU_TOP + TOOLS_MENU_PAD + i as f32 * (TOOLS_ROW_H + TOOLS_ROW_SPACING) + sep
    };
    let last = first + count.max(1) - 1;
    let y = top_of(first);
    iced::Rectangle {
        x: window_w - TOOLS_MENU_RIGHT - TOOLS_MENU_W + TOOLS_MENU_PAD,
        y,
        width: TOOLS_MENU_W - 2.0 * TOOLS_MENU_PAD,
        height: top_of(last) + TOOLS_ROW_H - y,
    }
}

/// Wrap a bare-icon toolbar control with a hover tooltip showing its name and,
/// when it has one, its current keyboard shortcut. Icon buttons carry no visible
/// label, so the tooltip is where a reader learns what each one does.
pub(crate) fn chrome_tip<'a>(
    control: impl Into<Element<'a, Message>>,
    name: &'a str,
    shortcut: Option<String>,
) -> Element<'a, Message> {
    let mut body = row![text(name).size(ts::BODY).color(theme::fg())]
        .spacing(10)
        .align_y(iced::Center);
    if let Some(sc) = shortcut {
        body = body.push(text(sc).size(ts::BODY).color(theme::dim()));
    }
    let bubble = container(body)
        .padding(Padding {
            top: 3.0,
            right: 8.0,
            bottom: 3.0,
            left: 8.0,
        })
        .style(theme::modal_panel);
    tooltip(control, bubble, tooltip::Position::Bottom)
        .gap(6)
        .into()
}

pub(crate) fn toolbar(app: &App) -> Element<'_, Message> {
    let caps = |action: Action| Some(app.keymap.chord(action).caps());
    // Nav arrows use the embedded Nerd Font (not a raw Unicode arrow) so they
    // share a baseline with the panel-toggle icons; mixing glyphs pulled from
    // different fallback fonts left the toolbar icons visibly misaligned.
    let nav = |glyph: Glyph, enabled: bool, msg: Message| {
        let color = if enabled { theme::fg() } else { theme::dim() };
        let mut b = button(glyph::icon(glyph, color, TOOL_ICON))
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
    let tool_icon = |glyph: Glyph, label: &'static str, shortcut: Option<String>, msg: Message| {
        chrome_tip(
            button(glyph::icon(glyph, theme::fg(), TOOL_ICON))
                .style(theme::toolbar_button)
                .padding([3.0, TOOL_PAD_X])
                .width(Length::Fixed(TOOL_W))
                .on_press(msg),
            label,
            shortcut,
        )
    };
    // A layout-toggle icon (bright = panel shown, dim = hidden), hand-drawn to
    // match the nav arrows beside it.
    let panel_toggle = |glyph: Glyph, shown: bool, msg: Message| {
        button(glyph::icon(
            glyph,
            if shown { theme::fg() } else { theme::dim() },
            TOOL_ICON,
        ))
        .style(theme::toolbar_button)
        .padding([2, 6])
        .width(Length::Fixed(PANEL_TOGGLE_W))
        .on_press(msg)
    };

    // Breadcrumb: dim folders › bright filename, for orientation while reading.
    let breadcrumb: Element<'_, Message> = match app.active_viewer() {
        Some(v) => {
            let parts: Vec<&str> = v.rel.split('/').collect();
            let mut r = Row::new().spacing(5).align_y(iced::Center);
            for (i, seg) in parts.iter().enumerate() {
                if i > 0 {
                    r = r.push(text("›").size(ts::BODY).color(theme::dim()));
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
                                text(seg.to_string()).size(ts::BASE).color(theme::fg()),
                            ]
                            .spacing(4)
                            .align_y(iced::Center),
                        )
                        .style(theme::toolbar_button)
                        .padding([2, 4])
                        .on_press(Message::Editor(EditorMsg::OpenAbs {
                            abs: v.abs.clone(),
                            line: None,
                            push: false,
                        })),
                    );
                } else {
                    r = r.push(text(seg.to_string()).size(ts::BASE).color(theme::dim()));
                }
            }
            r.into()
        }
        None => slot(),
    };

    // Custom window controls (the frameless window has no OS buttons): a row of
    // macOS-style red/amber/green circles. Being real buttons, they capture
    // their own clicks, so dragging from them never moves the window. Like
    // native traffic lights, they show glyphs while the pointer is over the
    // cluster, and grey out when the window has no focus (unless hovered).
    let show_icon = app.controls_hovered;
    let colored = app.window_focused || app.controls_hovered;
    let light = move |color: iced::Color, icon: TrafficIcon, msg: Message| {
        let content: Element<'_, Message> = if show_icon {
            iced::widget::canvas::Canvas::new(TrafficGlyph {
                icon,
                color: theme::TRAFFIC_GLYPH,
            })
            .width(12)
            .height(12)
            .into()
        } else {
            space().width(12).height(12).into()
        };
        button(content)
            .style(move |_theme, status: button::Status| {
                let bg = if !colored {
                    theme::TRAFFIC_INACTIVE // grey while the window is unfocused
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
                    border: iced::Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
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
            light(
                theme::TRAFFIC_CLOSE,
                TrafficIcon::Close,
                Message::Window(WindowMsg::Close)
            ),
            light(
                theme::TRAFFIC_MINIMIZE,
                TrafficIcon::Minimize,
                Message::Window(WindowMsg::Minimize)
            ),
            light(
                theme::TRAFFIC_ZOOM,
                TrafficIcon::Fullscreen(app.fullscreen),
                Message::Window(WindowMsg::ToggleFullscreen)
            ),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .on_enter(Message::Window(WindowMsg::ControlsHover(true)))
    .on_exit(Message::Window(WindowMsg::ControlsHover(false)));

    // Left cluster: window controls · layout toggle · back/forward · breadcrumb.
    let mut left = row![
        controls,
        space().width(6),
        // Codicons (VS Code's icon set): sidebar toggle + arrows all come from
        // the same family, so they share one baseline and sit on a line.
        chrome_tip(
            panel_toggle(
                Glyph::PanelLeft,
                app.show_left_sidebar,
                Message::Window(WindowMsg::ToggleLeftSidebar)
            ),
            "Toggle sidebar",
            None,
        ),
        chrome_tip(
            nav(
                Glyph::ArrowLeft,
                app.proj.history.can_back(),
                Message::Reading(ReadingMsg::GoBack)
            ),
            "Back",
            caps(Action::GoBack),
        ),
        chrome_tip(
            nav(
                Glyph::ArrowRight,
                app.proj.history.can_forward(),
                Message::Reading(ReadingMsg::GoForward)
            ),
            "Forward",
            caps(Action::GoForward),
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
        left = left.push(tool_icon(
            glyph,
            tip,
            None,
            Message::Editor(EditorMsg::ToggleMarkdownSource(app.proj.active)),
        ));
    }

    // Primary reading actions stay on the bar; everything else moves to "More".
    // Hand-drawn line icons (see `glyph`), one family with the traffic lights.
    // Each names itself (and its shortcut) on hover. `CORE_TOOLS` counts them.
    let core = row![
        tool_icon(
            Glyph::Overview,
            "Overview",
            None,
            Message::Overview(OverviewMsg::Show)
        ),
        tool_icon(
            Glyph::Stats,
            "Stats",
            None,
            Message::Overview(OverviewMsg::ShowStats)
        ),
        tool_icon(
            Glyph::Ask,
            "Ask",
            caps(Action::ToggleAsk),
            Message::Ask(AskMsg::Toggle)
        ),
        tool_icon(
            Glyph::Debug,
            "Debug",
            caps(Action::StartDebug),
            Message::Debug(DebugMsg::Start)
        ),
        tool_icon(
            Glyph::CallGraph,
            "Call Graph",
            caps(Action::CallGraph),
            Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectCalls))
        ),
        tool_icon(
            Glyph::ImportGraph,
            "Import Graph",
            caps(Action::ImportGraph),
            Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectImports))
        ),
        tool_icon(
            Glyph::Settings,
            "Settings",
            Some("⌘,".to_string()),
            Message::Settings(SettingsMsg::Open)
        ),
    ]
    .spacing(TOOL_SPACING)
    .align_y(iced::Center);

    let divider = container(space())
        .width(Length::Fixed(DIVIDER_W))
        .height(Length::Fixed(TOOL_ICON))
        .style(|_: &iced::Theme| container::Style {
            background: Some(theme::hairline().into()),
            ..container::Style::default()
        });
    let more = button(
        text("⋯")
            .size(ts::TITLE)
            .color(if app.show_tools_menu {
                theme::fg()
            } else {
                theme::dim()
            })
            .width(Fill)
            .align_x(iced::Center),
    )
    .style(theme::toolbar_button)
    .padding(0)
    .width(Length::Fixed(MORE_W))
    .on_press(Message::Window(WindowMsg::ToggleToolsMenu));

    let right = row![
        core,
        divider,
        chrome_tip(more, "More", None),
        chrome_tip(
            panel_toggle(
                Glyph::PanelRight,
                app.show_right_panel,
                Message::Window(WindowMsg::ToggleRightPanel)
            ),
            "Toggle panel",
            None,
        ),
    ]
    .spacing(CLUSTER_SPACING)
    .align_y(iced::Center);

    // clew draws its own window controls, so just a small margin from the edge.
    let bar = row![left, space().width(Fill), right]
        .align_y(iced::Center)
        .padding(Padding {
            top: 0.0,
            right: BAR_PAD_X,
            bottom: 0.0,
            left: BAR_PAD_X,
        });
    // A fixed title-bar height with vertically-centered content. The whole
    // toolbar is the window's drag region; its buttons (including the window
    // controls) capture their own clicks, so only empty areas start a drag.
    mouse_area(
        container(bar)
            .width(Fill)
            .height(Length::Fixed(TOOLBAR_H))
            .align_y(iced::Center)
            .style(theme::panel),
    )
    .on_press(Message::Window(WindowMsg::TitleBarDragged))
    .into()
}

/// The toolbar's "More" overflow menu: the secondary actions that don't need to
/// crowd the bar, in [`TOOLS_MENU_ROWS`] order. Positioned under the "⋯" button
/// (top-right).
pub(crate) fn tools_menu(app: &App) -> Element<'_, Message> {
    // Each row is icon + label (like a native macOS menu). Toggles show a
    // trailing accent check when active, actions their shortcut; the icon sits
    // in a fixed gutter so every label lines up.
    let menu_row =
        |glyph: Glyph, label: String, trailing: Element<'static, Message>, msg: Option<Message>| {
            let mut b = button(
                row![
                    container(glyph::icon(glyph, theme::fg_muted(), 17.0))
                        .width(26)
                        .align_x(iced::alignment::Horizontal::Center),
                    text(label).size(ts::BASE),
                    space().width(Fill),
                    trailing,
                ]
                .spacing(8)
                .height(Fill)
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .height(Length::Fixed(TOOLS_ROW_H))
            .padding([0, 10]);
            if let Some(msg) = msg {
                b = b.on_press(msg);
            }
            Element::from(b)
        };
    let check = |on: bool| -> Element<'static, Message> {
        if on {
            text("✓").size(ts::BODY).color(theme::accent()).into()
        } else {
            slot()
        }
    };
    let chord = |row: ToolsRow| -> Element<'static, Message> {
        match row.action() {
            Some(action) => text(app.keymap.chord(action).caps())
                .size(ts::SMALL)
                .color(theme::dim())
                .into(),
            None => slot(),
        }
    };

    let mut items: Vec<Element<'_, Message>> = Vec::new();
    for (i, &tools_row) in TOOLS_MENU_ROWS.iter().enumerate() {
        if i == TOOLS_MENU_TOGGLES {
            // Two groups, hairline-separated: view toggles above, actions below.
            items.push(
                container(hairline())
                    .height(Length::Fixed(TOOLS_SEP_H))
                    .align_y(iced::Center)
                    .padding([0, 6])
                    .into(),
            );
        }
        let item = match tools_row {
            ToolsRow::OutlineSummaries => menu_row(
                Glyph::Note,
                "Outline summaries".into(),
                check(app.show_inline_summaries),
                Some(Message::Window(WindowMsg::ToggleInlineSummaries)),
            ),
            ToolsRow::FileSummary => menu_row(
                Glyph::Info,
                "File summary".into(),
                check(app.show_file_banner),
                Some(Message::Window(WindowMsg::ToggleFileBanner)),
            ),
            ToolsRow::InlayHints => menu_row(
                Glyph::Lightbulb,
                "Inlay hints".into(),
                check(app.show_inlay_hints),
                Some(Message::Lsp(LspMsg::ToggleInlayHints)),
            ),
            ToolsRow::Minimap => menu_row(
                Glyph::Minimap,
                "Minimap".into(),
                check(app.show_minimap),
                Some(Message::Window(WindowMsg::ToggleMinimap)),
            ),
            // The interactive tutorial needs a project to tour: without one the
            // row stays (so the menu keeps its shape) but is inert.
            ToolsRow::Tutorial => menu_row(
                Glyph::Lightbulb,
                "Tutorial".into(),
                slot(),
                app.proj
                    .project
                    .is_some()
                    .then_some(Message::Tutorial(TutorialMsg::Start)),
            ),
            ToolsRow::OpenFolder => menu_row(
                Glyph::Folder,
                "Open Folder…".into(),
                slot(),
                Some(Message::Project(ProjectMsg::OpenFolderPressed)),
            ),
            ToolsRow::OpenRemote => menu_row(
                Glyph::Remote,
                "Open Remote…".into(),
                slot(),
                Some(Message::Connect(ConnectMsg::Open)),
            ),
            // Explain All carries its own progress and, with no LLM key
            // configured, routes to Settings instead. While a pass runs it turns
            // into a Cancel control — the bottom-up pass is thousands of LLM
            // calls on a big repo, so it must be stoppable.
            ToolsRow::ExplainAll if app.proj.explain.running => {
                let label = match app.proj.explain.progress {
                    Some((done, total)) if total > 0 => {
                        format!("Cancel explaining ({done}/{total})")
                    }
                    _ => "Cancel explaining".to_string(),
                };
                menu_row(
                    Glyph::Close,
                    label,
                    chord(tools_row),
                    Some(Message::Explain(ExplainMsg::Cancel)),
                )
            }
            ToolsRow::ExplainAll => menu_row(
                Glyph::Sparkle,
                "Explain All".into(),
                chord(tools_row),
                Some(if app.llm_available {
                    Message::Explain(ExplainMsg::Project)
                } else {
                    Message::Settings(SettingsMsg::Open)
                }),
            ),
            ToolsRow::Walkthrough => menu_row(
                Glyph::Compass,
                "Walkthrough".into(),
                chord(tools_row),
                Some(Message::Window(WindowMsg::SidebarTabPicked(
                    SidebarTab::Walk,
                ))),
            ),
            ToolsRow::Skim => menu_row(
                Glyph::Skim,
                "Skim (fold bodies)".into(),
                slot(),
                Some(Message::Editor(EditorMsg::SkimFile)),
            ),
            ToolsRow::Diff => menu_row(
                Glyph::Diff,
                "Diff".into(),
                chord(tools_row),
                Some(Message::Editor(EditorMsg::ToggleDiff)),
            ),
            ToolsRow::TimeTravel => menu_row(
                Glyph::TimeTravel,
                "Time Travel".into(),
                chord(tools_row),
                Some(Message::TimeTravel(TimeTravelMsg::Start { symbol: false })),
            ),
            ToolsRow::LspServers => menu_row(
                Glyph::Servers,
                "LSP Servers".into(),
                chord(tools_row),
                Some(Message::Lsp(LspMsg::TogglePanel)),
            ),
            ToolsRow::Shortcuts => menu_row(
                Glyph::Shortcuts,
                "Keyboard Shortcuts".into(),
                chord(tools_row),
                Some(Message::Window(WindowMsg::OpenShortcuts)),
            ),
        };
        items.push(item);
    }

    let panel = container(Column::with_children(items).spacing(TOOLS_ROW_SPACING))
        .width(TOOLS_MENU_W)
        .padding(TOOLS_MENU_PAD)
        .style(theme::modal_panel);
    modal(
        panel,
        Placement::TopRight {
            top: TOOLS_MENU_TOP,
            right: TOOLS_MENU_RIGHT,
        },
        Backdrop::Clear(Message::Window(WindowMsg::ToggleToolsMenu)),
    )
}

/// The status-bar `#[cfg]` target dropdown: which platform's cfg branches read
/// as live. A hand-rolled popup (not a `pick_list`, which pads to its widest
/// option and leaves a gap before the chevron) anchored to the bottom-right so
/// it hugs its trigger button.
pub(crate) fn target_menu(app: &App) -> Element<'_, Message> {
    let current = app.proj.reading_target.clone();
    let items: Vec<Element<'_, Message>> = crate::inactive::Target::presets()
        .into_iter()
        .map(|t| {
            let selected = t == current;
            let mark: Element<'_, Message> = if selected {
                text("✓").size(ts::SMALL).color(theme::accent()).into()
            } else {
                slot()
            };
            button(
                row![
                    container(mark).width(15),
                    text(t.to_string()).size(ts::BODY)
                ]
                .align_y(iced::Center),
            )
            .style(theme::list_row(selected))
            .width(Fill)
            .padding([5, 10])
            .on_press(Message::Reading(ReadingMsg::TargetSelected(t)))
            .into()
        })
        .collect();
    let panel = container(Column::with_children(items).spacing(1))
        .width(TARGET_MENU_W)
        .padding(4)
        .style(theme::modal_panel);
    modal(
        panel,
        Placement::BottomRight {
            bottom: STATUSBAR_H + 3.0,
            right: 12.0,
        },
        Backdrop::Clear(Message::Window(WindowMsg::ToggleTargetMenu)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tools_row_is_listed_once_and_knows_its_index() {
        for (i, row) in TOOLS_MENU_ROWS.iter().enumerate() {
            assert_eq!(row.index(), i, "{row:?}");
        }
        let mut seen = TOOLS_MENU_ROWS.to_vec();
        seen.dedup();
        assert_eq!(seen.len(), TOOLS_MENU_ROWS.len());
    }

    /// Everything is placed from the window's right edge: the icons move
    /// with it, one pitch apart.
    #[test]
    fn toolbar_geometry_follows_the_right_edge() {
        let w = 1280.0;
        for i in 0..CORE_TOOLS {
            assert_eq!(
                toolbar_icon_center_x(w + 100.0, i) - toolbar_icon_center_x(w, i),
                100.0
            );
        }
        assert_eq!(
            toolbar_more_center_x(w + 100.0) - toolbar_more_center_x(w),
            100.0
        );
        let menu = tools_menu_rows_rect(w + 100.0, 0, 1);
        assert_eq!(menu.x - tools_menu_rows_rect(w, 0, 1).x, 100.0);
    }
}
