//! All view code: toolbar, sidebar (files / search / marks), split code
//! panes, outline, status bar and the finder modal (files / symbols / :N).

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::text::Wrapping;
use iced::widget::{
    Column, Row, button, center, column, container, mouse_area, opaque, pick_list, progress_bar,
    row, scrollable, slider, space, stack, text, text_input, tooltip,
};
use iced::{Element, Fill, Font, Length, Padding};

use crate::codeview::CodeView;
use crate::finder::FinderMode;
use crate::fs_scan::DirNode;
use crate::viewer::Viewer;
use crate::glyph::{self, Glyph};
use crate::{App, Message, SidebarTab, TimeScope, TimeTravel, theme};

pub fn code_scroll_id(pane: usize) -> iced::widget::Id {
    iced::widget::Id::new(if pane == 0 { "code-view-0" } else { "code-view-1" })
}

pub fn finder_input_id() -> iced::widget::Id {
    iced::widget::Id::new("finder-input")
}

pub fn search_input_id() -> iced::widget::Id {
    iced::widget::Id::new("search-input")
}

pub fn find_input_id() -> iced::widget::Id {
    iced::widget::Id::new("find-input")
}

pub fn ask_input_id() -> iced::widget::Id {
    iced::widget::Id::new("ask-input")
}

/// The Ask conversation scrollable, so a new answer can snap it to the bottom.
pub fn ask_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("ask-conversation")
}

/// The outline scrollable, so it can follow the caret's current symbol.
pub fn outline_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("outline-list")
}


pub fn bp_condition_input_id() -> iced::widget::Id {
    iced::widget::Id::new("bp-condition-input")
}

pub fn note_input_id() -> iced::widget::Id {
    iced::widget::Id::new("bookmark-note-input")
}

pub fn view(app: &App) -> Element<'_, Message> {
    let mut main = Row::new();
    if app.show_left_sidebar {
        main = main.push(sidebar(app));
        main = main.push(crate::resize::Divider::vertical(Message::ResizeSidebar));
    }
    main = main.push(pane_area(app));
    // Right sidebar: the cursor-following reading-context panel.
    if let Some(rp) = right_panel(app) {
        main = main.push(crate::resize::Divider::vertical(Message::ResizeRight));
        main = main.push(rp);
    }
    // A bottom panel docks under the code, keeping it visible above. "Ask clew"
    // surfaces over the debugger when opened, so you can ask about the live state
    // while paused (the answer is grounded in the current stack + variables). Its
    // height is user-draggable via the divider between it and the code.
    let body: Element<'_, Message> = if app.show_bottom {
        column![
            main.height(Fill),
            crate::resize::Divider::horizontal(Message::ResizeBottom),
            container(bottom_panel(app)).height(Length::Fixed(app.bottom_height)),
        ]
        .height(Fill)
        .into()
    } else {
        main.height(Fill).into()
    };
    let base: Element<'_, Message> = column![toolbar(app), body, statusbar(app)].into();

    // Pick the single active overlay (if any).
    let overlay: Option<Element<'_, Message>> = if let Some(root) = &app.pending_consent {
        Some(consent_modal(root))
    } else if let Some(consent) = &app.pending_lsp_consent {
        Some(lsp_consent_modal(consent))
    } else if app.settings_open {
        Some(settings_modal(app))
    } else if app.connect.is_some() {
        Some(connect_modal(app))
    } else if app.show_shortcuts {
        Some(shortcuts_modal(app))
    } else if let Some(edit) = &app.bp_cond_edit {
        Some(bp_condition_modal(app, edit))
    } else if let Some(edit) = &app.note_edit {
        Some(bookmark_note_modal(app, edit))
    } else if let Some(edit) = &app.reading_note_edit {
        Some(reading_note_modal(edit))
    } else if let Some(bw) = &app.blame_why {
        Some(why_modal(app, bw))
    } else if app.show_tools_menu {
        Some(tools_menu(app))
    } else if app.show_target_menu {
        Some(target_menu(app))
    } else if let Some(overlay) = app.overlay {
        Some(project_graph_modal(app, overlay))
    } else if app.server_panel {
        Some(server_panel_modal(app))
    } else if app.finder.open {
        Some(finder_modal(app))
    } else if let Some(menu) = &app.context_menu {
        Some(context_menu(app, menu))
    } else {
        app.hover.as_ref().filter(|h| h.text.is_some() || h.summary.is_some()).map(hover_tooltip)
    };

    // `base` is ALWAYS child 0 of a Stack — even with no overlay — so opening or
    // closing one never changes the base subtree's position in the widget tree.
    // Otherwise iced rebuilds it and resets the code view's scroll offset (a
    // right-click would snap the view back to the top).
    match overlay {
        Some(o) => stack![base, o].into(),
        None => stack![base].into(),
    }
}

// ---------------------------------------------------------------- hover tooltip

fn hover_tooltip(h: &crate::HoverState) -> Element<'_, Message> {
    let mut parts: Vec<Element<'_, Message>> = Vec::new();
    // clew's cached one-liner first, in accent so it reads as a summary, not code.
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
        parts.push(text(shown).size(12).font(Font::MONOSPACE).color(theme::FG).into());
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

fn context_menu<'a>(app: &'a App, menu: &'a crate::ContextMenu) -> Element<'a, Message> {
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
            plain_item("Conditional Breakpoint…", Message::ConditionalBreakpointFromMenu),
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
        .padding(Padding { top, left, right: 0.0, bottom: 0.0 });

    // A full-size backdrop closes the menu on any outside click.
    opaque(mouse_area(positioned).on_press(Message::ContextMenuClosed))
}

// ---------------------------------------------------------------- server panel

fn server_panel_modal(app: &App) -> Element<'_, Message> {
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
                text(human_size(srv.bytes)).size(11).color(theme::DIM).width(Fill),
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
fn thin_scroll() -> Direction {
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
fn truncate_ellipsis(s: &str, max: usize) -> String {
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
fn strip_backticks(s: &str) -> String {
    s.replace('`', "")
}

/// A dim, single-line secondary description: first sentence, backticks stripped,
/// truncated with an ellipsis and clipped so it never wraps or overflows the
/// panel. Shared by the right-panel call-flow / contains rows so they match the
/// outline rows' treatment instead of hard-cutting mid-word.
fn one_line_desc<'a>(full: &str, max: usize) -> Element<'a, Message> {
    let one = truncate_ellipsis(&first_sentence(&strip_backticks(full)), max);
    container(text(one).size(10).color(theme::DIM).wrapping(Wrapping::None))
        .clip(true)
        .width(Fill)
        .into()
}

fn rel_of(app: &App, path: &std::path::Path) -> String {
    match &app.project {
        Some(p) => path
            .strip_prefix(&p.root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string(),
        None => path.to_string_lossy().to_string(),
    }
}

/// Modal frame shared by the project-graph overlays: a titled panel with a
/// List/Map toggle, over a dismissable backdrop.
fn graph_modal_frame<'a>(
    title: &'a str,
    graph_mode: bool,
    extra: Option<Element<'a, Message>>,
    body: Element<'a, Message>,
) -> Element<'a, Message> {
    let toggle_label = if graph_mode { "List" } else { "Map" };
    let mut header = row![
        text(title).size(17).color(theme::FG),
        space().width(Fill),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if let Some(extra) = extra {
        header = header.push(extra);
    }
    header = header
        .push(
            button(text(toggle_label).size(12))
                .style(theme::toolbar_button)
                .padding([3, 12])
                .on_press(Message::OverlayViewToggle),
        )
        .push(
            button(text("Close").size(12))
                .style(theme::toolbar_button)
                .padding([3, 12])
                .on_press(Message::CloseOverlay),
        );
    let panel = container(
        column![header, body].spacing(12),
    )
    .width(760)
    .max_height(640)
    .padding(20)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);

    opaque(mouse_area(positioned).on_press(Message::CloseOverlay))
}

fn project_graph_modal(app: &App, overlay: crate::Overlay) -> Element<'_, Message> {
    let title = match overlay {
        crate::Overlay::ProjectImports => "Project Import Graph",
        crate::Overlay::ProjectCalls => "Project Call Graph",
    };
    // The call graph can be refined to exact LSP edges; show its control/status.
    let extra: Option<Element<'_, Message>> = match overlay {
        crate::Overlay::ProjectCalls => Some(if let Some((done, total)) = app.refine_progress {
            text(format!("Refining {done}/{total}…"))
                .size(11)
                .color(theme::rgb(0xe5c07b))
                .into()
        } else if app.project_calls_precise {
            text("● LSP-precise").size(11).color(theme::ACCENT).into()
        } else {
            button(text("Refine with LSP").size(11))
                .style(theme::toolbar_button)
                .padding([3, 10])
                .on_press(Message::RefineProjectCalls)
                .into()
        }),
        crate::Overlay::ProjectImports => None,
    };
    let body = if app.graph_mode {
        graph_map_view(app)
    } else {
        match overlay {
            crate::Overlay::ProjectImports => project_imports_body(app),
            crate::Overlay::ProjectCalls => project_calls_body(app),
        }
    };
    graph_modal_frame(title, app.graph_mode, extra, body)
}

/// The node-link map: a force-directed canvas plus a legend.
fn graph_map_view(app: &App) -> Element<'_, Message> {
    let overlay = app.overlay;
    let hint = |msg: &str| {
        container(text(msg.to_string()).size(12).color(theme::DIM))
            .padding(8)
            .width(Fill)
            .height(iced::Length::Fill)
            .into()
    };
    let Some(layout) = &app.graph_layout else {
        return hint(if app.building_calls {
            "Building call graph…"
        } else {
            "Nothing to show."
        });
    };
    if layout.nodes.is_empty() {
        return hint("Nothing to show.");
    }
    let Some(kind) = overlay else {
        return hint("Nothing to show.");
    };
    let map = iced::widget::canvas::Canvas::new(GraphCanvas { layout, kind })
        .width(Fill)
        .height(Fill);
    let legend = if layout.total > layout.nodes.len() {
        format!(
            "Showing the {} most-connected of {} files · drag to pan · scroll to zoom · click a node to open it",
            layout.nodes.len(),
            layout.total,
        )
    } else {
        "Drag to pan · scroll to zoom · click a node to open it · size = degree · orange = in a cycle"
            .to_string()
    };
    column![map, text(legend).size(10).color(theme::DIM)]
        .spacing(6)
        .height(iced::Length::Fill)
        .into()
}

/// Force-directed node-link renderer for a project graph.
struct GraphCanvas<'a> {
    layout: &'a crate::graphlayout::Layout,
    kind: crate::Overlay,
}

/// Padding inside the canvas so node labels aren't clipped at the edges.
const GRAPH_PAD: f32 = 48.0;

/// Pan/zoom view state for a map, persisted by the canvas widget across frames.
#[derive(Clone, Copy)]
struct MapView {
    /// Multiplier applied to the auto-fit layout (spreads nodes apart on zoom-in).
    scale: f32,
    /// Pan translation, in screen pixels.
    offset: iced::Vector,
    /// A left-drag pan is in progress.
    dragging: bool,
    /// Last cursor position (absolute) while dragging, for the pan delta.
    last: iced::Point,
    /// Distance dragged since press — a small total means "click", not "pan".
    moved: f32,
}

impl Default for MapView {
    fn default() -> Self {
        MapView {
            scale: 1.0,
            offset: iced::Vector::new(0.0, 0.0),
            dragging: false,
            last: iced::Point::new(0.0, 0.0),
            moved: 0.0,
        }
    }
}

impl GraphCanvas<'_> {
    /// Untransformed auto-fit pixel position of node `i`.
    fn node_fit(&self, i: usize, bounds: iced::Rectangle) -> iced::Point {
        let n = &self.layout.nodes[i];
        let w = (bounds.width - 2.0 * GRAPH_PAD).max(1.0);
        let h = (bounds.height - 2.0 * GRAPH_PAD).max(1.0);
        iced::Point::new(GRAPH_PAD + n.x * w, GRAPH_PAD + n.y * h)
    }

    /// Screen position after applying the view's pan + zoom.
    fn node_screen(&self, i: usize, bounds: iced::Rectangle, v: &MapView) -> iced::Point {
        let f = self.node_fit(i, bounds);
        iced::Point::new(f.x * v.scale + v.offset.x, f.y * v.scale + v.offset.y)
    }

    /// The node nearest to `cursor` (in screen space) within a click radius.
    fn hit(&self, cursor: iced::Point, bounds: iced::Rectangle, v: &MapView) -> Option<usize> {
        let mut best = None;
        let mut best_d = 22.0f32;
        for i in 0..self.layout.nodes.len() {
            let p = self.node_screen(i, bounds, v);
            let d = ((p.x - cursor.x).powi(2) + (p.y - cursor.y).powi(2)).sqrt();
            if d < best_d {
                best_d = d;
                best = Some(i);
            }
        }
        best
    }

    fn open_message(&self, i: usize) -> Message {
        let file = self.layout.nodes[i].file.clone();
        match self.kind {
            crate::Overlay::ProjectImports => Message::OverlayOpenImports(file),
            crate::Overlay::ProjectCalls => Message::OverlayOpenAt { abs: file, line: 1 },
        }
    }
}

/// Axis-aligned overlap test for label decluttering.
fn rects_overlap(a: iced::Rectangle, b: iced::Rectangle) -> bool {
    a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y
}

impl iced::widget::canvas::Program<Message> for GraphCanvas<'_> {
    type State = MapView;

    fn draw(
        &self,
        state: &MapView,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: iced::Rectangle,
        cursor: iced::advanced::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry> {
        use iced::widget::canvas::{Frame, Path, Stroke, Text};
        let mut frame = Frame::new(renderer, bounds.size());

        // Edges first, so nodes draw on top.
        let edge_color = theme::rgb(0x3a3f4b);
        for &(a, b) in &self.layout.edges {
            let pa = self.node_screen(a, bounds, state);
            let pb = self.node_screen(b, bounds, state);
            frame.stroke(
                &Path::line(pa, pb),
                Stroke::default().with_width(1.0).with_color(edge_color),
            );
        }

        let hovered = cursor
            .position_in(bounds)
            .and_then(|c| self.hit(c, bounds, state));

        // Circles for every node.
        for (i, n) in self.layout.nodes.iter().enumerate() {
            let p = self.node_screen(i, bounds, state);
            let r = 3.5 + n.weight.sqrt() * 1.8;
            let base = if n.cyclic {
                theme::rgb(0xe5c07b)
            } else {
                theme::ACCENT
            };
            let color = if hovered == Some(i) { theme::FG } else { base };
            frame.fill(&Path::circle(p, r), color);
        }

        // Labels, decluttered: place by priority (hovered, then degree) and skip
        // any that would overlap an already-placed label. Zooming in spreads the
        // nodes, so more labels fit — the map stays readable at any density.
        let mut order: Vec<usize> = (0..self.layout.nodes.len()).collect();
        order.sort_by(|&a, &b| {
            let ha = (hovered == Some(a)) as u8;
            let hb = (hovered == Some(b)) as u8;
            hb.cmp(&ha).then_with(|| {
                self.layout.nodes[b]
                    .weight
                    .partial_cmp(&self.layout.nodes[a].weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        let mut placed: Vec<iced::Rectangle> = Vec::new();
        for i in order {
            let n = &self.layout.nodes[i];
            let p = self.node_screen(i, bounds, state);
            let r = 3.5 + n.weight.sqrt() * 1.8;
            let width = n.label.chars().count() as f32 * 6.0 + 2.0;
            // Place the label to the right of the node, but flip it to the left
            // when that would clip the canvas's right edge — so an edge node (e.g.
            // a disconnected file pushed into a corner) stays fully readable.
            let flip = p.x + r + 3.0 + width > bounds.width - 2.0;
            let (rect_x, text_x, align_x) = if flip {
                (p.x - r - 3.0 - width, p.x - r - 3.0, iced::alignment::Horizontal::Right)
            } else {
                (p.x + r + 3.0, p.x + r + 3.0, iced::alignment::Horizontal::Left)
            };
            let rect = iced::Rectangle { x: rect_x, y: p.y - 6.5, width, height: 13.0 };
            let is_hover = hovered == Some(i);
            if is_hover || !placed.iter().any(|pr| rects_overlap(*pr, rect)) {
                frame.fill_text(Text {
                    content: n.label.clone(),
                    position: iced::Point::new(text_x, p.y),
                    color: if is_hover { theme::FG } else { theme::DIM },
                    size: 11.0.into(),
                    align_x: align_x.into(),
                    align_y: iced::alignment::Vertical::Center,
                    ..Text::default()
                });
                placed.push(rect);
            }
        }
        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        state: &mut MapView,
        event: &iced::Event,
        bounds: iced::Rectangle,
        cursor: iced::advanced::mouse::Cursor,
    ) -> Option<iced::widget::canvas::Action<Message>> {
        use iced::mouse;
        use iced::widget::canvas::Action;
        match event {
            // Zoom around the cursor.
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let c = cursor.position_in(bounds)?;
                let dy = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / 40.0,
                };
                let old = state.scale.max(0.05);
                let new = (old * (1.0 + dy * 0.15)).clamp(0.3, 6.0);
                let ratio = new / old;
                // Keep the point under the cursor fixed.
                state.offset = iced::Vector::new(
                    c.x - (c.x - state.offset.x) * ratio,
                    c.y - (c.y - state.offset.y) * ratio,
                );
                state.scale = new;
                Some(Action::request_redraw().and_capture())
            }
            // Begin a potential drag (resolved as click vs pan on release).
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.position_in(bounds).is_some() =>
            {
                if let Some(abs) = cursor.position() {
                    state.dragging = true;
                    state.last = abs;
                    state.moved = 0.0;
                }
                Some(Action::capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                if let Some(abs) = cursor.position() {
                    let dx = abs.x - state.last.x;
                    let dy = abs.y - state.last.y;
                    state.offset = iced::Vector::new(state.offset.x + dx, state.offset.y + dy);
                    state.moved += (dx * dx + dy * dy).sqrt();
                    state.last = abs;
                }
                Some(Action::request_redraw().and_capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.dragging =>
            {
                state.dragging = false;
                // A press with negligible movement is a click → open the node.
                if state.moved < 5.0
                    && let Some(c) = cursor.position_in(bounds)
                    && let Some(i) = self.hit(c, bounds, state)
                {
                    return Some(Action::publish(self.open_message(i)).and_capture());
                }
                Some(Action::capture())
            }
            _ => None,
        }
    }

    fn mouse_interaction(
        &self,
        state: &MapView,
        bounds: iced::Rectangle,
        cursor: iced::advanced::mouse::Cursor,
    ) -> iced::advanced::mouse::Interaction {
        use iced::advanced::mouse::Interaction;
        if state.dragging {
            return Interaction::Grabbing;
        }
        match cursor.position_in(bounds).and_then(|c| self.hit(c, bounds, state)) {
            Some(_) => Interaction::Pointer,
            None if cursor.is_over(bounds) => Interaction::Grab,
            None => Interaction::default(),
        }
    }
}

/// Which macOS-style window control an icon draws.
#[derive(Clone, Copy)]
enum TrafficIcon {
    Close,
    Minimize,
    /// `true` while the window is fullscreen (draws the collapse variant).
    Fullscreen(bool),
}

/// Draws the traffic-light glyphs by hand so they match the native macOS
/// weight: thin round-capped strokes for the ✕ and −, and two solid triangles
/// with a diagonal gap for the fullscreen control. Font glyphs (Nerd Font)
/// render far too bold/large at this size, so we stroke/fill directly.
struct TrafficGlyph {
    icon: TrafficIcon,
    color: iced::Color,
}

impl iced::widget::canvas::Program<Message> for TrafficGlyph {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: iced::Rectangle,
        _cursor: iced::advanced::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry> {
        use iced::widget::canvas::{Frame, LineCap, Path, Stroke};
        let mut frame = Frame::new(renderer, bounds.size());
        let m = bounds.width.min(bounds.height);
        let p = |x: f32, y: f32| iced::Point::new(x, y);
        // A fresh thin, round-capped stroke (Stroke isn't cheaply reusable).
        let pen = || {
            Stroke::default()
                .with_width(1.15)
                .with_color(self.color)
                .with_line_cap(LineCap::Round)
        };
        match self.icon {
            TrafficIcon::Close => {
                let a = m * 0.30;
                let b = m - a;
                frame.stroke(&Path::line(p(a, a), p(b, b)), pen());
                frame.stroke(&Path::line(p(b, a), p(a, b)), pen());
            }
            TrafficIcon::Minimize => {
                let a = m * 0.27;
                frame.stroke(&Path::line(p(a, m / 2.0), p(m - a, m / 2.0)), pen());
            }
            TrafficIcon::Fullscreen(fs) => {
                let tri = |v: [(f32, f32); 3]| {
                    Path::new(|b| {
                        b.move_to(p(v[0].0, v[0].1));
                        b.line_to(p(v[1].0, v[1].1));
                        b.line_to(p(v[2].0, v[2].1));
                        b.close();
                    })
                };
                // Small, delicate triangles with a clear diagonal gap, so the
                // fullscreen control carries the same light weight as the thin
                // ✕ and − rather than reading as a solid green disc.
                let pad = m * 0.27;
                let leg = m * 0.33;
                if fs {
                    // Collapse: two triangles meeting near the center.
                    let c = m / 2.0;
                    frame.fill(&tri([(pad, c), (c, pad), (c, c)]), self.color);
                    frame.fill(&tri([(m - pad, c), (c, m - pad), (c, c)]), self.color);
                } else {
                    // Expand: solid triangles in the top-left / bottom-right corners.
                    frame.fill(&tri([(pad, pad), (pad + leg, pad), (pad, pad + leg)]), self.color);
                    frame.fill(
                        &tri([(m - pad, m - pad), (m - pad - leg, m - pad), (m - pad, m - pad - leg)]),
                        self.color,
                    );
                }
            }
        }
        vec![frame.into_geometry()]
    }
}

/// A file row in the import overlay: name + directory + fan-in/out counts.
fn import_file_row<'a>(app: &'a App, path: &std::path::Path) -> Element<'a, Message> {
    let g = &app.import_graph;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let dir = std::path::Path::new(&rel_of(app, path))
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".into());
    button(
        row![
            text(name.to_string()).size(12).wrapping(Wrapping::None),
            space().width(6),
            text(dir).size(10).color(theme::DIM).wrapping(Wrapping::None),
            space().width(Fill),
            text(format!("←{} →{}", g.fan_in(path), g.fan_out(path)))
                .size(10)
                .color(theme::DIM)
                .wrapping(Wrapping::None),
        ]
        .align_y(iced::Center),
    )
    .style(theme::list_row(false))
    .width(Fill)
    .padding([2, 6])
    .on_press(Message::OverlayOpenImports(path.to_path_buf()))
    .into()
}

fn project_imports_body(app: &App) -> Element<'_, Message> {
    let g = &app.import_graph;
    if g.is_empty() {
        return container(text("No imports found in this project.").size(12).color(theme::DIM))
            .padding(8)
            .into();
    }
    let files = g.files();
    let externals = g.external_packages();

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(
        text(format!(
            "{} files · {} internal edges · {} external packages · {} cycles",
            files.len(),
            g.internal_edge_count(),
            externals.len(),
            app.import_cycles.len(),
        ))
        .size(12)
        .color(theme::ACCENT)
        .into(),
    );

    // Cycles — a real structural smell worth surfacing first.
    if !app.import_cycles.is_empty() {
        rows.push(section_header("IMPORT CYCLES"));
        for cycle in &app.import_cycles {
            let names: Vec<String> = cycle
                .iter()
                .map(|p| p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string())
                .collect();
            rows.push(
                container(
                    text(format!("↺ {}", names.join(" → ")))
                        .size(11)
                        .color(theme::rgb(0xe5c07b))
                        .wrapping(Wrapping::None),
                )
                .padding([2, 8])
                .into(),
            );
        }
    }

    // Most depended-on (highest fan-in) — the architectural hubs.
    let mut by_in = files.clone();
    by_in.sort_by(|a, b| g.fan_in(b).cmp(&g.fan_in(a)).then_with(|| a.cmp(b)));
    by_in.retain(|p| g.fan_in(p) > 0);
    rows.push(section_header("MOST DEPENDED-ON (fan-in)"));
    for p in by_in.iter().take(12) {
        rows.push(import_file_row(app, p));
    }

    // Most dependencies (highest fan-out).
    let mut by_out = files.clone();
    by_out.sort_by(|a, b| g.fan_out(b).cmp(&g.fan_out(a)).then_with(|| a.cmp(b)));
    by_out.retain(|p| g.fan_out(p) > 0);
    rows.push(section_header("MOST DEPENDENCIES (fan-out)"));
    for p in by_out.iter().take(12) {
        rows.push(import_file_row(app, p));
    }

    // External packages the project pulls in.
    if !externals.is_empty() {
        rows.push(section_header("EXTERNAL PACKAGES"));
        rows.push(
            container(
                text(externals.join("  ·  "))
                    .size(11)
                    .color(theme::DIM),
            )
            .padding([2, 8])
            .into(),
        );
    }

    scrollable(Column::with_children(rows).spacing(3).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(iced::Length::Fill)
        .into()
}

/// A symbol row in the call overlay: name + file:line + a trailing count.
fn call_symbol_row<'a>(
    app: &'a App,
    id: usize,
    trailing: String,
) -> Element<'a, Message> {
    let n = app.project_calls.node(id);
    button(
        row![
            text(n.name.clone()).size(12).wrapping(Wrapping::None),
            space().width(6),
            text(format!("{}:{}", rel_of(app, &n.file), n.line))
                .size(10)
                .color(theme::DIM)
                .wrapping(Wrapping::None),
            space().width(Fill),
            text(trailing).size(10).color(theme::DIM).wrapping(Wrapping::None),
        ]
        .align_y(iced::Center),
    )
    .style(theme::list_row(false))
    .width(Fill)
    .padding([2, 6])
    .on_press(Message::OverlayOpenAt {
        abs: n.file.clone(),
        line: n.line,
    })
    .into()
}

fn project_calls_body(app: &App) -> Element<'_, Message> {
    let g = &app.project_calls;
    if g.is_empty() {
        let msg = if app.building_calls {
            "Building call graph…"
        } else {
            "No functions found in this project."
        };
        return container(text(msg).size(12).color(theme::DIM)).padding(8).into();
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(
        text(format!(
            "{} functions · {} call edges",
            g.node_count(),
            g.edge_count(),
        ))
        .size(12)
        .color(theme::ACCENT)
        .into(),
    );
    rows.push(
        text(if app.project_calls_precise {
            "LSP-precise — exact caller/callee edges."
        } else {
            "Name-based & approximate — Refine with LSP for exact edges."
        })
        .size(10)
        .color(theme::DIM)
        .into(),
    );

    // Most-called functions (hubs), unique names only so the counts mean something.
    let hubs = g.most_called(15);
    if !hubs.is_empty() {
        rows.push(section_header("MOST CALLED (unique names)"));
        for &id in &hubs {
            let c = g.node(id).caller_count();
            rows.push(call_symbol_row(app, id, format!("{c} callers")));
        }
    }

    // Uncalled functions — entry points, public API, or dead code. Test
    // functions are always "uncalled" (the test harness invokes them, not
    // project code), so they'd swamp the list as false positives — drop them.
    let is_test_node = |id: usize| {
        let n = g.node(id);
        app.symbol_index_by_file
            .get(&n.file)
            .and_then(|syms| syms.iter().find(|s| s.name == n.name && s.line == n.line))
            .map(|s| s.is_test)
            .unwrap_or(false)
    };
    let uncalled: Vec<usize> = g.uncalled().into_iter().filter(|&id| !is_test_node(id)).collect();
    rows.push(section_header("UNCALLED (entry points / possibly dead)"));
    for &id in uncalled.iter().take(60) {
        let out = g.node(id).callee_count();
        rows.push(call_symbol_row(app, id, format!("→{out}")));
    }
    if uncalled.len() > 60 {
        rows.push(
            container(
                text(format!("… and {} more", uncalled.len() - 60))
                    .size(10)
                    .color(theme::DIM),
            )
            .padding([2, 8])
            .into(),
        );
    }

    scrollable(Column::with_children(rows).spacing(3).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(iced::Length::Fill)
        .into()
}

// -------------------------------------------------- explanation overlay

fn explain_child_label(node: &crate::explain::Node) -> String {
    use crate::explain::Node;
    let name = |p: &std::path::Path| p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
    match node {
        Node::Folder(p) => format!("📁 {}", name(p)),
        Node::File(p) => name(p),
        Node::Function { name, .. } => format!("fn {name}"),
    }
}

fn explain_is_child(parent: &crate::explain::Node, node: &crate::explain::Node) -> bool {
    use crate::explain::Node;
    match (parent, node) {
        (Node::Folder(p), Node::Folder(c) | Node::File(c)) => c.parent() == Some(p.as_path()),
        (Node::File(p), Node::Function { file, .. }) => file == p,
        _ => false,
    }
}

/// The LLM settings modal: pick a provider, enter the API key, and optionally
/// override the model / base URL. Saved to the global `config.toml`.
fn settings_modal(app: &App) -> Element<'_, Message> {
    use crate::llm::Provider;
    let label = |s: &str| text(s.to_string()).size(11).color(theme::DIM);
    let field = |title: &str, input: Element<'static, Message>| {
        column![label(title), input].spacing(3)
    };

    let provider: Element<'_, Message> = pick_list(
        &Provider::ALL[..],
        Some(app.settings_provider),
        Message::SettingsProviderPicked,
    )
    .text_size(13)
    .padding([4, 8])
    // Match the full width of the text fields below it, so the form column
    // doesn't look ragged with a half-width dropdown.
    .width(Fill)
    .into();

    let key = text_input("paste your API key", &app.settings_key)
        .on_input(Message::SettingsKeyChanged)
        .secure(true)
        .size(13)
        .padding(6);
    let model = text_input(app.settings_provider.default_model(), &app.settings_model)
        .on_input(Message::SettingsModelChanged)
        .size(13)
        .padding(6);
    let base = text_input(app.settings_provider.default_base_url(), &app.settings_base_url)
        .on_input(Message::SettingsBaseUrlChanged)
        .size(13)
        .padding(6);

    // Embeddings (semantic search) — an OpenAI-compatible endpoint.
    let embed_key = text_input("embedding API key", &app.settings_embed_key)
        .on_input(Message::SettingsEmbedKeyChanged)
        .secure(true)
        .size(13)
        .padding(6);
    let embed_model = text_input("text-embedding-3-small", &app.settings_embed_model)
        .on_input(Message::SettingsEmbedModelChanged)
        .size(13)
        .padding(6);
    let embed_base = text_input("https://api.openai.com/v1", &app.settings_embed_base_url)
        .on_input(Message::SettingsEmbedBaseUrlChanged)
        .size(13)
        .padding(6);

    let section = |s: &str| text(s.to_string()).size(12).color(theme::ACCENT);
    let panel = container(
        column![
            row![
                text("Settings").size(16).color(theme::FG),
                space().width(Fill),
                button(text("Save").size(12))
                    .style(theme::primary_button)
                    .padding([3, 14])
                    .on_press(Message::SettingsSaved),
                button(text("Close").size(12))
                    .style(theme::toolbar_button)
                    .padding([3, 12])
                    .on_press(Message::CloseSettings),
            ]
            .spacing(6)
            .align_y(iced::Center),
            section("Language model"),
            field("Provider", provider),
            field("API key", key.into()),
            field("Model", model.into()),
            field("Base URL", base.into()),
            section("Embeddings (semantic search)"),
            field("API key", embed_key.into()),
            field("Model", embed_model.into()),
            field("Base URL", embed_base.into()),
            text(format!("Stored in {}", crate::llm::config_hint()))
                .size(10)
                .color(theme::DIM),
        ]
        .spacing(12),
    )
    .width(480)
    .padding(20)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::CloseSettings))
}

/// Join a browsed directory with a child name, tolerating a trailing slash (so
/// the filesystem root `/` yields `/child`, not `//child`).
fn remote_join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The Connect modal: pick or define an SSH host, then browse its folders for
/// the one to open. Walks `ConnectStage` — picking → connecting → browsing —
/// but always in one panel so the flow reads as a single place.
fn connect_modal(app: &App) -> Element<'_, Message> {
    use crate::ConnectStage;
    let Some(ui) = &app.connect else {
        return space().into();
    };

    let title = row![
        glyph::icon(Glyph::Remote, theme::ACCENT, 18.0),
        text("Connect to Remote").size(16).color(theme::FG),
        space().width(Fill),
        button(text("Close").size(12))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::CloseConnect),
    ]
    .spacing(8)
    .align_y(iced::Center);

    let body: Element<'_, Message> = match &ui.stage {
        ConnectStage::Picking => connect_picker(app, ui, None),
        ConnectStage::Error(msg) => connect_picker(app, ui, Some(msg)),
        ConnectStage::Connecting { label } => center(
            column![
                glyph::icon(Glyph::Remote, theme::ACCENT, 34.0),
                text(format!("Connecting to {label}…")).size(13).color(theme::FG),
                text("Preparing the server on the remote host.")
                    .size(11)
                    .color(theme::DIM),
                space().height(6),
                button(text("Cancel").size(12))
                    .style(theme::toolbar_button)
                    .padding([4, 14])
                    .on_press(Message::CloseConnect),
            ]
            .spacing(6)
            .align_x(iced::Center),
        )
        .height(Length::Fixed(260.0))
        .into(),
        ConnectStage::Browsing(browser) => remote_browser_view(browser),
    };

    let panel = container(column![title, body].spacing(14))
        .width(560)
        .max_height(620)
        .padding(20)
        .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::CloseConnect))
}

/// The picking stage: a list of saved hosts (if any) above a new-connection form.
fn connect_picker<'a>(
    app: &'a App,
    ui: &'a crate::ConnectUi,
    error: Option<&'a str>,
) -> Element<'a, Message> {
    use crate::ConnectField;
    let label = |s: &str| text(s.to_string()).size(11).color(theme::DIM);

    let mut col = Column::new().spacing(12);

    if let Some(msg) = error {
        col = col.push(
            container(text(msg.to_string()).size(12).color(theme::rgb(0xe06c75)))
                .padding([6, 10])
                .width(Fill)
                .style(theme::modal_panel),
        );
    }

    // Saved hosts: click a row to connect, × to forget.
    if !app.saved_connections.is_empty() {
        col = col.push(section_header("SAVED HOSTS"));
        let mut list = Column::new().spacing(2);
        for (idx, conn) in app.saved_connections.iter().enumerate() {
            let open = button(
                row![
                    glyph::icon(Glyph::Remote, theme::FG_MUTED, 14.0),
                    column![
                        text(conn.label()).size(13).color(theme::FG),
                        text(conn.user_host()).size(11).color(theme::DIM),
                    ]
                    .spacing(1),
                ]
                .spacing(8)
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([5, 10])
            .on_press(Message::ConnectToSaved(idx));
            let remove = button(glyph::icon(Glyph::Close, theme::DIM, 13.0))
                .style(theme::toolbar_button)
                .padding([5, 8])
                .on_press(Message::ConnectRemoveSaved(idx));
            list = list.push(row![open, remove].spacing(4).align_y(iced::Center));
        }
        col = col.push(list);
    }

    // New-connection form.
    let field = |title: &str, input: Element<'a, Message>| column![label(title), input].spacing(3);
    let input = |placeholder: &str, value: &str, f: ConnectField| {
        text_input(placeholder, value)
            .on_input(move |s| Message::ConnectField(f, s))
            .size(13)
            .padding(6)
    };

    let identity = row![
        input("(optional) ~/.ssh/id_ed25519", &ui.identity, ConnectField::Identity).width(Fill),
        button(text("Browse…").size(12))
            .style(theme::toolbar_button)
            .padding([6, 12])
            .on_press(Message::ConnectPickIdentity),
    ]
    .spacing(6);

    col = col.push(section_header("NEW CONNECTION"));
    col = col.push(field("Name (optional)", input("prod box", &ui.name, ConnectField::Name).into()));
    col = col.push(
        row![
            field("Host", input("192.168.1.10 or example.com", &ui.host, ConnectField::Host).into())
                .width(Fill),
            field("Port", input("22", &ui.port, ConnectField::Port).into()).width(80),
        ]
        .spacing(8),
    );
    col = col.push(field("User", input("root", &ui.user, ConnectField::User).into()));
    col = col.push(field("Identity file", identity.into()));
    col = col.push(
        row![
            space().width(Fill),
            button(text("Connect").size(13))
                .style(theme::primary_button)
                .padding([6, 18])
                .on_press(Message::ConnectSubmit),
        ]
        .align_y(iced::Center),
    );

    // While connected to a remote, offer a way back to local reading.
    if app.connection.is_remote() {
        col = col.push(
            row![
                space().width(Fill),
                button(text("Disconnect (read local code)").size(11))
                    .style(theme::toolbar_button)
                    .padding([4, 12])
                    .on_press(Message::ConnectDisconnect),
            ],
        );
    }

    scrollable(col.width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Length::Shrink)
        .into()
}

/// The browsing stage: a path bar with an "up" control, the directory's contents
/// (folders navigable, files dimmed for context), and "Open this folder".
fn remote_browser_view(browser: &crate::RemoteBrowser) -> Element<'_, Message> {
    let mut up = button(glyph::icon(Glyph::ArrowLeft, theme::FG_MUTED, 14.0))
        .style(theme::toolbar_button)
        .padding([4, 10]);
    if let Some(parent) = &browser.parent {
        up = up.on_press(Message::RemoteBrowseTo(parent.clone()));
    }
    let path_bar = row![
        up,
        container(
            text(browser.cwd.clone())
                .size(12)
                .font(Font::MONOSPACE)
                .color(theme::FG)
                .wrapping(Wrapping::None)
        )
        .width(Fill)
        .clip(true),
    ]
    .spacing(8)
    .align_y(iced::Center);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if browser.entries.is_empty() {
        let msg = if browser.loading { "Loading…" } else { "Empty folder." };
        rows.push(container(text(msg).size(12).color(theme::DIM)).padding([4, 8]).into());
    }
    for entry in &browser.entries {
        if entry.is_dir {
            let (glyph, color) = crate::icons::folder_icon(false);
            rows.push(
                button(
                    row![
                        tree_icon(glyph, color),
                        text(entry.name.clone()).size(13).wrapping(Wrapping::None),
                    ]
                    .spacing(4)
                    .align_y(iced::Center),
                )
                .style(theme::list_row(false))
                .width(Fill)
                .padding([4, 8])
                .on_press(Message::RemoteBrowseTo(remote_join(&browser.cwd, &entry.name)))
                .into(),
            );
        } else {
            let (glyph, color) = crate::icons::file_icon(&entry.name);
            rows.push(
                row![
                    tree_icon(glyph, color),
                    text(entry.name.clone()).size(13).color(theme::DIM).wrapping(Wrapping::None),
                ]
                .spacing(4)
                .align_y(iced::Center)
                .padding([4, 8])
                .into(),
            );
        }
    }

    let entries = scrollable(Column::with_children(rows).spacing(1).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Length::Fixed(300.0));

    let footer = row![
        column![
            text("Open this folder as the project").size(11).color(theme::DIM),
            text(browser.cwd.clone())
                .size(12)
                .font(Font::MONOSPACE)
                .color(theme::FG)
                .wrapping(Wrapping::None),
        ]
        .spacing(1)
        .width(Fill),
        button(text("Open").size(13))
            .style(theme::primary_button)
            .padding([6, 18])
            .on_press(Message::RemoteOpenHere),
    ]
    .spacing(8)
    .align_y(iced::Center);

    column![
        path_bar,
        container(entries).style(theme::modal_panel).padding(4),
        footer,
    ]
    .spacing(10)
    .into()
}

/// The "Keyboard Shortcuts" modal: rebindable command chords on top, the fixed
/// Vim-style reading motions below as a read-only reference.
fn shortcuts_modal(app: &App) -> Element<'_, Message> {
    use crate::keymap::Action;
    let section = |s: &str| text(s.to_string()).size(12).color(theme::ACCENT);

    // Header: title, optional "Reset all", Close.
    let mut header = row![text("Keyboard Shortcuts").size(16).color(theme::FG), space().width(Fill)]
        .spacing(6)
        .align_y(iced::Center);
    if app.keymap.any_overridden() {
        header = header.push(
            button(text("Reset all").size(12))
                .style(theme::toolbar_button)
                .padding([3, 12])
                .on_press(Message::RebindResetAll),
        );
    }
    header = header.push(
        button(text("Close").size(12))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::CloseShortcuts),
    );

    // A one-line hint, replaced by a warning when a rebind is rejected.
    let notice: Element<'_, Message> = match &app.keymap_notice {
        Some(msg) => text(msg.clone()).size(11).color(theme::rgb(0xff9558)).into(),
        None => text("Click a shortcut, then press the new keys. Esc cancels.")
            .size(11)
            .color(theme::DIM)
            .into(),
    };

    // Rebindable command rows.
    let mut cmds = Column::new().spacing(2);
    for action in Action::ALL {
        let binding: Element<'_, Message> = if app.rebinding == Some(action) {
            container(text("Press a shortcut… esc to cancel").size(12).color(theme::ACCENT))
                .padding([3, 8])
                .into()
        } else {
            let pill = button(text(app.keymap.chord(action).caps()).size(13).color(theme::FG))
                .style(theme::toolbar_button)
                .padding([3, 10])
                .on_press(Message::RebindStart(action));
            if app.keymap.is_overridden(action) {
                row![
                    pill,
                    button(text("↺").size(13).color(theme::DIM))
                        .style(theme::toolbar_button)
                        .padding([3, 7])
                        .on_press(Message::RebindReset(action)),
                ]
                .spacing(4)
                .align_y(iced::Center)
                .into()
            } else {
                pill.into()
            }
        };
        cmds = cmds.push(
            row![
                text(action.label()).size(13).color(theme::FG),
                space().width(Fill),
                binding,
            ]
            .align_y(iced::Center)
            .spacing(10)
            .padding([1, 2]),
        );
    }

    // Read-only reading motions (not part of the customizable keymap).
    let motions: [(&str, &str); 13] = [
        ("Move left / down / up / right", "h j k l   ← ↓ ↑ →"),
        ("Word forward / back", "w   b"),
        ("Line start / end", "0   $"),
        ("File start / end", "gg   G"),
        ("Go to definition", "gd"),
        ("Find references", "gr"),
        ("Go to implementation", "gi"),
        ("Go to type definition", "gy"),
        ("Call hierarchy", "gc"),
        ("Toggle fold", "za"),
        ("Open all folds", "zR"),
        ("Close all folds", "zM"),
        ("Clear selection / close", "esc"),
    ];
    let mut vim = Column::new().spacing(2);
    for (label, keys) in motions {
        vim = vim.push(
            row![
                text(label).size(13).color(theme::FG),
                space().width(Fill),
                text(keys).size(12).color(theme::DIM),
            ]
            .align_y(iced::Center)
            .spacing(10)
            .padding([1, 2]),
        );
    }

    let scroll_body = scrollable(
        column![
            section("Commands"),
            cmds,
            space().height(8),
            section("Reading motions (Vim, fixed)"),
            vim,
        ]
        .spacing(8)
        .width(Fill)
        .padding(Padding { top: 0.0, right: 8.0, bottom: 0.0, left: 0.0 }),
    )
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Length::Fixed(440.0));

    let panel = container(
        column![
            header,
            notice,
            scroll_body,
            text(format!("Saved to {}", crate::llm::config_hint())).size(10).color(theme::DIM),
        ]
        .spacing(12),
    )
    .width(540)
    .padding(20)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::CloseShortcuts))
}

/// Render prepared segments in order: markdown prose through the markdown widget,
/// math and mermaid as inline SVGs (with a placeholder until a background render
/// lands). Shared by the explanation panel and the architecture overview.
fn render_prepared<'a>(
    app: &'a App,
    segments: &'a [crate::PreparedSeg],
) -> Vec<Element<'a, Message>> {
    use crate::{PreparedInline, PreparedSeg};
    let mut out: Vec<Element<'_, Message>> = Vec::new();
    for seg in segments {
        match seg {
            PreparedSeg::Markdown(items) => out.push(
                iced::widget::markdown::view(items, iced::Theme::Dark)
                    .map(|url| Message::OpenLink(url.to_string())),
            ),
            PreparedSeg::DisplayMath(key) => out.push(match app.explain_svgs.get(key) {
                Some(sv) => container(svg_widget(sv))
                    .width(Fill)
                    .align_x(iced::Center)
                    .padding([6, 0])
                    .into(),
                None => svg_placeholder("equation"),
            }),
            PreparedSeg::Mermaid(key, src) => out.push(match app.explain_svgs.get(key) {
                Some(sv) => container(svg_widget(sv)).padding([8, 0]).into(),
                // No SVG yet (still rendering, or the diagram failed to render):
                // show the raw mermaid source rather than a perpetual spinner.
                None => container(
                    text(src.clone()).font(Font::MONOSPACE).size(11).color(theme::DIM),
                )
                .padding(8)
                .width(Fill)
                .style(theme::editor)
                .into(),
            }),
            PreparedSeg::InlineLine(parts) => {
                let mut line: Vec<Element<'_, Message>> = Vec::new();
                for p in parts {
                    match p {
                        PreparedInline::Text(t) => {
                            line.push(text(t.clone()).color(theme::FG).into());
                        }
                        PreparedInline::Math(key) => line.push(match app.explain_svgs.get(key) {
                            Some(sv) => svg_widget(sv),
                            None => text("…").color(theme::DIM).into(),
                        }),
                    }
                }
                out.push(Row::with_children(line).align_y(iced::Center).spacing(1).into());
            }
        }
    }
    out
}

/// A fixed-size `svg` widget for a rendered math/mermaid block.
fn svg_widget<'a>(sv: &crate::ExplainSvg) -> Element<'a, Message> {
    iced::widget::svg(sv.handle.clone())
        .width(Length::Fixed(sv.width))
        .height(Length::Fixed(sv.height))
        .into()
}

/// Shown in place of a math/mermaid block until its background render arrives.
fn svg_placeholder<'a>(what: &str) -> Element<'a, Message> {
    text(format!("rendering {what}…")).size(11).color(theme::DIM).into()
}

/// The Explain tab's content: the explanation of the node under the caret (or
/// the Cmd+clicked file/folder) — its summary or block detail, the action
/// buttons, and a drill-down into the summaries it contains.
/// The "CALLED BY" / "CALLS" navigation strip for a focused function: its
/// one-hop callers and callees from the project call graph, each annotated with
/// its explanation summary. Clicking a link jumps there — the code view and this
/// panel both follow (via `OpenNode` → `open_file` → `follow_caret`), so you can
/// walk the call flow one hop at a time.
fn call_flow_rows<'a>(app: &'a App, node: &crate::explain::Node) -> Vec<Element<'a, Message>> {
    use crate::explain::Node;
    let mut out: Vec<Element<'a, Message>> = Vec::new();
    let Node::Function { file, name } = node else {
        return out;
    };
    let g = &app.project_calls;
    let Some(id) = g.id_of(file, name) else {
        // No node for this function yet — show a hint only while the graph builds.
        if app.building_calls {
            out.push(section_header("CALL FLOW"));
            out.push(
                container(text("Building call graph…").size(10).color(theme::DIM))
                    .padding([1, 8])
                    .into(),
            );
        }
        return out;
    };
    // Debug overlay: the actual live caller (the frame below the current one on
    // the paused stack), so the static "CALLED BY" list marks who *really* called
    // this function in the running program.
    let live_parent: Option<String> = app
        .debug
        .as_ref()
        .filter(|s| s.status == crate::DebugStatus::Stopped)
        .and_then(|s| s.frames.get(1))
        .map(|f| crate::short_frame_name(&f.name));
    // Split callers into tests and the rest, so the tests that exercise this
    // function read as its executable spec. Callees stay their own group.
    let sorted = |ids: &[usize]| {
        let mut v: Vec<&crate::projectcalls::SymNode> = ids.iter().map(|&i| g.node(i)).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    };
    let (tests, callers): (Vec<_>, Vec<_>) =
        sorted(g.callers_of(id)).into_iter().partition(|n| app.is_test_symbol(&n.file, &n.name));
    let callees = sorted(g.callees_of(id));
    let green = theme::rgb(0x98c379);

    // (header, arrow, nodes, jump-to-call-site, name colour). Tests and callees
    // open the symbol's definition; a plain caller jumps to the actual call line.
    for (label, arrow, items, to_call, name_color) in [
        ("TESTS", "←", tests, false, green),
        ("CALLED BY", "←", callers, true, theme::ACCENT),
        ("CALLS", "→", callees, false, theme::ACCENT),
    ] {
        if items.is_empty() {
            continue;
        }
        out.push(section_header(label));
        for n in items {
            let target = Node::Function { file: n.file.clone(), name: n.name.clone() };
            let summary = app.explanations.get(&target).map(|c| c.summary.as_str()).unwrap_or("");
            let is_live = label == "CALLED BY" && live_parent.as_deref() == Some(n.name.as_str());
            let live = theme::rgb(0x98c379);
            let mut r = row![
                text(arrow).size(11).color(if is_live { live } else { theme::DIM }),
                text(n.name.clone()).size(12).color(name_color),
                text(rel_of(app, &n.file)).size(10).color(theme::DIM),
            ]
            .spacing(6);
            if is_live {
                r = r.push(text("● live").size(9).color(live));
            }
            let mut col = column![r].spacing(1);
            if !summary.is_empty() {
                col = col.push(one_line_desc(summary, 64));
            }
            let msg = if to_call {
                Message::JumpToCall {
                    caller_file: n.file.clone(),
                    caller: n.name.clone(),
                    callee: name.clone(),
                }
            } else {
                Message::OpenNode(target)
            };
            out.push(
                button(col)
                    .style(theme::list_row(false))
                    .width(Fill)
                    .padding([3, 6])
                    .on_press(msg)
                    .into(),
            );
        }
    }
    out
}

fn explain_content(app: &App) -> Element<'_, Message> {
    use crate::explain::Node;
    let Some(node) = app.explain_view.as_ref() else {
        return container(
            text("Move the cursor into a function, or Cmd+click a file/folder.")
                .size(11)
                .color(theme::DIM),
        )
        .padding(10)
        .into();
    };
    let title = match node {
        Node::Folder(p) => format!("📁 {}", rel_of(app, p)),
        Node::File(p) => rel_of(app, p),
        Node::Function { file, name } => format!("{name} · {}", rel_of(app, file)),
    };

    // Call-flow navigation first (callers/callees), then the explanation prose.
    let mut rows: Vec<Element<'_, Message>> = call_flow_rows(app, node);
    rows.extend(render_prepared(app, &app.explain_prepared));

    let mut children: Vec<(&Node, &str)> = app
        .explanations
        .iter()
        .filter(|(n, _)| explain_is_child(node, n))
        .map(|(n, c)| (n, c.summary.as_str()))
        .collect();
    children.sort_by_key(|(n, _)| explain_child_label(n));
    if !children.is_empty() {
        rows.push(section_header("CONTAINS"));
        for (n, sum) in children {
            rows.push(
                button(
                    column![
                        text(explain_child_label(n)).size(12).color(theme::ACCENT),
                        one_line_desc(sum, 64),
                    ]
                    .spacing(1),
                )
                .style(theme::list_row(false))
                .width(Fill)
                .padding([3, 6])
                .on_press(Message::ShowExplanation(n.clone()))
                .into(),
            );
        }
    }

    let act = |label: &str, msg: Message| {
        button(text(label.to_string()).size(11))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(msg)
    };
    let mut actions: Vec<Element<'_, Message>> = Vec::new();
    // Functions get a summary ⇄ per-block-detail toggle.
    if matches!(node, Node::Function { .. }) {
        actions.push(if app.explain_showing_detail {
            act("Summary", Message::ShowExplanation(node.clone())).into()
        } else {
            act("Explain blocks", Message::ExplainBlocks(node.clone())).into()
        });
    }
    if app.llm_available {
        actions.push(act("Re-explain", Message::ReexplainNode).into());
    }

    // The header is padded, but the scrollable itself reaches the panel's right
    // edge so its scrollbar lines up with the outline's below (content is padded
    // inside instead). A thin bar keeps both looking tidy.
    let pad = |l, r| Padding { top: 0.0, right: r as f32, bottom: 0.0, left: l as f32 };
    container(
        column![
            container(text(title).size(13).color(theme::FG)).padding(Padding {
                top: 10.0,
                right: 12.0,
                bottom: 0.0,
                left: 12.0,
            }),
            // left 4 so the button's own inner padding lands its text at ~12,
            // aligned with the title above (not indented past it).
            container(Row::with_children(actions).spacing(4)).padding(pad(4, 12)),
            scrollable(
                Column::with_children(rows)
                    .spacing(8)
                    .width(Fill)
                    .padding(Padding { top: 2.0, right: 10.0, bottom: 8.0, left: 12.0 }),
            )
            .direction(Direction::Vertical(Scrollbar::new().width(6.0).scroller_width(6.0)))
            .style(theme::overlay_scrollbar)
            .height(iced::Length::Fill),
        ]
        .spacing(8),
    )
    .height(Fill)
    .into()
}

/// A small uppercase section label (OUTLINE / CONTAINS / CALLED BY …) — a
/// single consistent style for every panel sub-heading.
fn section_header(label: &str) -> Element<'_, Message> {
    container(text(label.to_uppercase()).size(10).color(theme::FG_MUTED))
        .padding(Padding { top: 12.0, right: 10.0, bottom: 4.0, left: 10.0 })
        .into()
}

fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

// ---------------------------------------------------------------- LSP consent

fn lsp_consent_modal(consent: &crate::LspConsent) -> Element<'_, Message> {
    use crate::LspProvision;
    let is_install = matches!(consent.provision, LspProvision::Install(_));
    let (title, action) = if is_install {
        ("Install a language server?", "Install")
    } else {
        ("Download a language server?", "Download")
    };

    let panel = container(
        column![
            text(title).size(17).color(theme::FG),
            text(
                "clew manages its own “go to definition” server for this \
                 language, separate from anything on your system:",
            )
            .size(13)
            .color(theme::FG),
            container(
                text(format!("{} {}", consent.server_name, consent.version))
                    .size(12)
                    .color(theme::ACCENT)
                    .font(Font::MONOSPACE)
                    .wrapping(Wrapping::None),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor),
            text(consent.describe()).size(12).color(theme::DIM).wrapping(Wrapping::None),
            row![
                space().width(Fill),
                button(text("Not now").size(13))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(Message::LspConsentDismissed),
                button(text(action).size(13))
                    .style(theme::primary_button)
                    .padding([6, 16])
                    .on_press(Message::LspConsentAllowed),
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(14),
    )
    .width(560)
    .padding(22)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .style(theme::backdrop);

    opaque(positioned)
}

// ---------------------------------------------------------------- consent modal

fn consent_modal(root: &std::path::Path) -> Element<'_, Message> {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());

    let panel = container(
        column![
            text("Allow clew to use this project?").size(17).color(theme::FG),
            text(
                "clew stores bookmarks and reading data in a “.clew” folder \
                 inside the project:",
            )
            .size(13)
            .color(theme::FG),
            container(
                // Paths have no spaces to break on, so glyph-wrap to keep a long
                // path inside the box instead of overflowing off the panel.
                text(format!("{}/.clew", root.display()))
                    .size(12)
                    .color(theme::ACCENT)
                    .font(Font::MONOSPACE)
                    .wrapping(Wrapping::Glyph),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor),
            text("Without it the project can't be opened. You can delete .clew any time.")
                .size(12)
                .color(theme::DIM),
            row![
                space().width(Fill),
                button(text("Not now").size(13))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(Message::ConsentDenied),
                button(text(format!("Allow in {name}")).size(13))
                    .style(theme::primary_button)
                    .padding([6, 16])
                    .on_press(Message::ConsentAllowed),
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(14),
    )
    .width(560)
    .padding(22)
    .style(theme::modal_panel);

    // A plain backdrop; clicking it does nothing (the choice is required).
    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .style(theme::backdrop);

    opaque(positioned)
}

// ---------------------------------------------------------------- toolbar

/// Wrap a bare-icon toolbar control with a hover tooltip showing its name and,
/// when it has one, its current keyboard shortcut. Icon buttons carry no visible
/// label, so the tooltip is where a reader learns what each one does.
fn chrome_tip<'a>(
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

fn toolbar(app: &App) -> Element<'_, Message> {
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
    let left = row![
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
fn tools_menu(app: &App) -> Element<'_, Message> {
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
    // key configured, routes to Settings instead. Disabled while a pass runs.
    let explain: Element<'_, Message> = {
        let label = match app.explain_progress {
            Some((done, total)) if total > 0 => format!("Explaining {done}/{total}…"),
            Some(_) => "Explaining…".to_string(),
            None => "Explain All".to_string(),
        };
        let mut btn = button(
            row![menu_icon(Glyph::Sparkle), text(label).size(13)]
                .spacing(8)
                .align_y(iced::Center),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([5, 10]);
        if app.explaining {
            // disabled while a pass runs
        } else if app.llm_available {
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
fn target_menu(app: &App) -> Element<'_, Message> {
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

fn sidebar(app: &App) -> Element<'_, Message> {
    let tab = |label: &'static str, this: SidebarTab| {
        button(text(label).size(11))
            .style(theme::tab_button(app.sidebar == this))
            .padding([5, 7])
            .on_press(Message::SidebarTabPicked(this))
    };
    // Seven tabs rarely all fit a narrow sidebar, so they keep their natural
    // width and scroll horizontally (a thin bar appears only when they overflow)
    // — widening the sidebar reveals them all. CALLS/IMPORTS are always present.
    let tabs = scrollable(
        row![
            tab("FILES", SidebarTab::Files),
            tab("SEARCH", SidebarTab::Search),
            tab("FIND", SidebarTab::Semantic),
            tab("MARKS", SidebarTab::Marks),
            tab("TRAIL", SidebarTab::Trail),
            tab("CALLS", SidebarTab::Calls),
            tab("IMPORTS", SidebarTab::Imports),
            tab("WALK", SidebarTab::Walk),
            tab("NOTES", SidebarTab::Notes),
            tab("DOCS", SidebarTab::Docs),
        ]
        .spacing(1),
    )
    // Scrollable but with no visible bar — it scrolls by trackpad/wheel and the
    // sidebar can be widened to reveal all tabs; a bar here just looks noisy.
    .direction(Direction::Horizontal(
        Scrollbar::new().width(0.0).scroller_width(0.0),
    ))
    .width(Fill);

    let content: Element<'_, Message> = match app.sidebar {
        SidebarTab::Files => files_tab(app),
        SidebarTab::Search => search_tab(app),
        SidebarTab::Semantic => semantic_tab(app),
        SidebarTab::Marks => marks_tab(app),
        SidebarTab::Trail => trail_tab(app),
        SidebarTab::Calls => calls_tab(app),
        SidebarTab::Imports => imports_tab(app),
        SidebarTab::Walk => walk_tab(app),
        SidebarTab::Notes => notes_tab(app),
        SidebarTab::Docs => docs_tab(app),
    };

    container(column![tabs, content])
        .width(Length::Fixed(app.sidebar_width))
        .height(Fill)
        .style(theme::panel)
        .into()
}

/// The guided-walkthrough tab. The top bar toggles between *searching* the saved
/// library of tours and *walking* (generating) a new one. Opening a tour steps
/// through it, each step driving the editor to its anchor; regenerating a tour
/// lives next to its title.
fn walk_tab(app: &App) -> Element<'_, Message> {
    let header = walk_header(app);
    // A quick action to review the current branch/PR changes as a narrated tour.
    let review = container(
        button(text("\u{2387} Review branch changes").size(11))
            .style(theme::toolbar_button)
            .padding([4, 10])
            .width(Fill)
            .on_press(Message::GenerateDiffWalkthrough),
    )
    .padding(Padding { top: 0.0, right: 8.0, bottom: 6.0, left: 8.0 });

    // The library list is always shown under the input; selecting a tour expands
    // its steps inline (accordion) and its narration into the bottom pane — no
    // separate "back to library" navigation. Generation is shown per-row, so the
    // rest of the library stays usable while a tour is being built.
    let list = walk_library(app);
    let open = app.walkthrough_open.and_then(|o| app.walkthroughs.get(o).map(|w| (o, w)));

    let Some((idx, wt)) = open else {
        return column![header, review, hairline(), list].height(Fill).into();
    };

    let narration_block = walk_narration(app, idx, wt);
    column![
        header,
        review,
        hairline(),
        list,
        crate::resize::Divider::horizontal(Message::ResizeWalkNarration),
        narration_block,
    ]
    .height(Fill)
    .into()
}

/// A human label for a walkthrough's scope: the whole codebase, a change review,
/// or the user's feature prompt.
fn scope_label(scope: &str) -> String {
    if scope.is_empty() {
        "Whole codebase".to_string()
    } else if let Some(rest) = scope.strip_prefix("@diff") {
        format!("Change review{}", if rest.trim().is_empty() { String::new() } else { format!(" ({})", rest.trim()) })
    } else {
        scope.to_string()
    }
}

/// The top bar: a Search/Walk segmented toggle, the shared input, and (in Walk
/// mode) a Generate button.
fn walk_header(app: &App) -> Element<'_, Message> {
    let is_search = app.walkthrough_mode == crate::WalkMode::Search;
    // Two-segment control; only the inactive segment is pressable (it flips mode).
    let seg = |label: &str, active: bool| {
        let mut b =
            button(text(label.to_string()).size(11)).style(theme::tab_button(active)).padding([3, 10]);
        if !active {
            b = b.on_press(Message::WalkthroughToggleMode);
        }
        b
    };
    let toggle = row![seg("Search", is_search), seg("Walk", !is_search)].spacing(2);

    let placeholder = if is_search {
        "Search walkthroughs…"
    } else {
        "Walk a feature, or leave empty for the whole codebase…"
    };
    let mut input = text_input(placeholder, &app.walkthrough_input)
        .on_input(Message::WalkthroughInputChanged)
        .size(12)
        .padding(6);
    if !is_search {
        // Enter submits the same way the Generate button does ("" = whole codebase).
        input = input.on_submit(Message::GenerateWalkthrough(app.walkthrough_input.clone()));
    }
    let mut bar = row![toggle, input].spacing(6).align_y(iced::Center);
    if !is_search {
        bar = bar.push(
            button(text("Generate").size(11))
                .style(theme::toolbar_button)
                .padding([4, 12])
                .on_press(Message::GenerateWalkthrough(app.walkthrough_input.clone())),
        );
    }
    container(bar).padding(8).into()
}

/// The library list: every saved tour, filtered by the search query, each with a
/// per-tour Regenerate button on the right.
fn walk_library(app: &App) -> Element<'_, Message> {
    let query = if app.walkthrough_mode == crate::WalkMode::Search {
        app.walkthrough_input.trim().to_lowercase()
    } else {
        String::new()
    };
    let matches = |wt: &crate::walkthrough::Walkthrough| {
        query.is_empty()
            || wt.title.to_lowercase().contains(&query)
            || wt.scope.to_lowercase().contains(&query)
    };

    let visible: Vec<(usize, &crate::walkthrough::Walkthrough)> =
        app.walkthroughs.iter().enumerate().filter(|(_, wt)| matches(wt)).collect();

    // The scope currently generating, and — if it's a brand-new scope not yet in
    // the library — the label for a temporary "pending" row at the top.
    let gen_scope = app.generating_walkthrough.as_deref();
    let pending_new: Option<&str> =
        gen_scope.filter(|s| !app.walkthroughs.iter().any(|w| w.scope.as_str() == *s));

    if app.walkthroughs.is_empty() && pending_new.is_none() {
        return empty_state(
            Glyph::Compass,
            "No walkthroughs yet",
            "Switch to Walk mode and generate a guided tour of the codebase or a feature.",
            None,
        );
    }
    if visible.is_empty() && pending_new.is_none() {
        return empty_state(Glyph::Search, "No matches", "No saved walkthrough matches your search.", None);
    }

    // The current step of the open tour (for highlighting the expanded steps).
    let cur = app
        .walkthrough_open
        .and_then(|o| app.walkthroughs.get(o))
        .map(|w| app.walkthrough_step.min(w.steps.len().saturating_sub(1)));

    let mut list = Column::new().spacing(2).padding(8);

    // A new tour being generated shows a pending row until it lands in the library.
    if let Some(scope) = pending_new {
        let label = scope_label(scope);
        list = list.push(
            container(
                column![
                    text(label).size(13).color(theme::FG),
                    text("Generating…").size(10).color(theme::ACCENT),
                ]
                .spacing(1),
            )
            .width(Fill)
            .padding([6, 8]),
        );
    }

    for (i, wt) in visible {
        let is_open = app.walkthrough_open == Some(i);
        let busy = gen_scope == Some(wt.scope.as_str());
        let (subtitle, sub_color) = if busy {
            ("Generating…".to_string(), theme::ACCENT)
        } else {
            (scope_label(&wt.scope), theme::DIM)
        };
        // The tour row: a full-width clickable title (so its selected highlight
        // spans the whole row) with the regenerate/delete controls layered on top
        // at the right via a stack. Leave right padding for them so the title
        // text never runs under the controls.
        let title = button(
            column![
                text(wt.title.clone()).size(13).color(if is_open { theme::FG_BRIGHT } else { theme::FG }),
                text(subtitle).size(10).color(sub_color),
            ]
            .spacing(1),
        )
        .style(theme::list_row(is_open))
        .width(Fill)
        .padding(Padding { top: 6.0, right: if busy { 8.0 } else { 62.0 }, bottom: 6.0, left: 8.0 })
        .on_press(if is_open { Message::WalkthroughBack } else { Message::WalkthroughOpen(i) });
        let tour_row: Element<'_, Message> = if busy {
            title.into()
        } else {
            let controls = container(
                row![
                    button(text("↻").size(13))
                        .style(theme::toolbar_button)
                        .padding([6, 9])
                        .on_press(Message::WalkthroughRegenerate(i)),
                    button(text("✕").size(12))
                        .style(theme::toolbar_button)
                        .padding([6, 9])
                        .on_press(Message::WalkthroughDelete(i)),
                ]
                .spacing(2),
            )
            .width(Fill)
            .height(Fill)
            .align_x(iced::Right)
            .align_y(iced::Center)
            .padding(Padding { top: 0.0, right: 6.0, bottom: 0.0, left: 0.0 });
            stack![title, controls].into()
        };
        list = list.push(tour_row);

        // Expanded: the tour's steps, indented, current one highlighted.
        if is_open {
            for (si, step) in wt.steps.iter().enumerate() {
                let is_cur = cur == Some(si);
                list = list.push(
                    button(
                        row![
                            text(format!("{}", si + 1)).size(10).color(theme::DIM).width(18),
                            text(step.title.clone())
                                .size(12)
                                .color(if is_cur { theme::FG } else { theme::DIM }),
                        ]
                        .spacing(6)
                        .align_y(iced::Center),
                    )
                    .style(theme::list_row(is_cur))
                    .width(Fill)
                    .padding(Padding { top: 4.0, right: 8.0, bottom: 4.0, left: 22.0 })
                    .on_press(Message::WalkthroughGoto(si)),
                );
            }
        }
    }

    scrollable(list.width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill).into()
}

/// The bottom pane for the open tour: a compact nav row (file + step counter +
/// prev/next) over the current step's rendered narration.
fn walk_narration<'a>(
    app: &'a App,
    _idx: usize,
    wt: &'a crate::walkthrough::Walkthrough,
) -> Element<'a, Message> {
    let n = wt.steps.len();
    let cur = app.walkthrough_step.min(n.saturating_sub(1));
    let Some(step) = wt.steps.get(cur) else {
        return space().into();
    };

    let nav = row![
        text(step.file.clone()).size(10).color(theme::ACCENT),
        space().width(Fill),
        button(text("‹").size(14))
            .style(theme::toolbar_button)
            .padding([1, 8])
            .on_press(Message::WalkthroughStep(-1)),
        text(format!("{}/{}", cur + 1, n)).size(11).color(theme::DIM),
        button(text("›").size(14))
            .style(theme::toolbar_button)
            .padding([1, 8])
            .on_press(Message::WalkthroughStep(1)),
    ]
    .spacing(6)
    .align_y(iced::Center)
    .padding([4, 8]);

    let body: Element<'_, Message> = if app.walkthrough_prepared.is_empty() {
        text(step.narration.clone()).size(12).color(theme::FG).width(Fill).into()
    } else {
        Column::with_children(render_prepared(app, &app.walkthrough_prepared)).spacing(8).width(Fill).into()
    };
    let narration = scrollable(container(body).padding(Padding {
        top: 0.0,
        right: 8.0,
        bottom: 8.0,
        left: 8.0,
    }))
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Fill);

    container(column![nav, narration]).height(Length::Fixed(app.walkthrough_narration_height)).into()
}

fn files_tab(app: &App) -> Element<'_, Message> {
    let Some(project) = &app.project else {
        // Same centered empty-state pattern as the other tabs (Trail, Marks, …).
        // No action button here: the open/connect actions live in the centered
        // welcome hero, so the sidebar just states what's going on.
        return if app.scanning {
            empty_state(Glyph::Search, "Scanning…", "Reading the project's files.", None)
        } else if app.connection.is_remote() {
            empty_state(
                Glyph::Remote,
                "No folder open",
                "Browse the host to open a folder.",
                None,
            )
        } else {
            empty_state(
                Glyph::Folder,
                "No folder open",
                "Open a folder to start reading.",
                None,
            )
        };
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    append_tree_rows(&mut rows, &project.tree, "", 0, app);
    scrollable(Column::with_children(rows).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Fill)
        .into()
}

fn append_tree_rows<'a>(
    rows: &mut Vec<Element<'a, Message>>,
    node: &'a DirNode,
    prefix: &str,
    depth: u16,
    app: &'a App,
) {
    let indent = 10.0 + depth as f32 * 14.0;
    let pad = Padding {
        top: 2.0,
        right: 6.0,
        bottom: 2.0,
        left: indent,
    };

    for (name, child) in &node.dirs {
        let rel = join_rel(prefix, name);
        let expanded = app.expanded.contains(&rel);
        let arrow = if expanded { "▾" } else { "▸" };
        let (glyph, color) = crate::icons::folder_icon(expanded);
        let content = row![
            text(arrow).size(10).color(theme::DIM).width(10),
            tree_icon(glyph, color),
            text(name.as_str()).size(13).wrapping(Wrapping::None),
        ]
        .spacing(3)
        .align_y(iced::Center);
        rows.push(
            button(content)
                .style(theme::list_row(false))
                .width(Fill)
                .padding(pad)
                .on_press(Message::ToggleDir(rel.clone()))
                .into(),
        );
        if expanded {
            append_tree_rows(rows, child, &rel, depth + 1, app);
        }
    }

    for name in &node.files {
        let rel = join_rel(prefix, name);
        let is_current = app.active_viewer().is_some_and(|v| v.rel == rel);
        let (glyph, color) = crate::icons::file_icon(name);
        let content = row![
            space().width(10), // align names under the folders' arrow column
            tree_icon(glyph, color),
            text(name.as_str()).size(13).wrapping(Wrapping::None),
        ]
        .spacing(3)
        .align_y(iced::Center);
        rows.push(
            button(content)
                .style(theme::list_row(is_current))
                .width(Fill)
                .padding(pad)
                .on_press(Message::OpenRel { rel, line: None })
                .into(),
        );
    }
}

/// A file-type glyph in the embedded icon font, for inline use (breadcrumb,
/// finder rows, …).
fn icon_text(glyph: char, color: iced::Color, size: f32) -> iced::widget::Text<'static> {
    text(glyph.to_string()).font(crate::icons::ICON_FONT).size(size).color(color)
}

/// A fixed-width, centered file-type icon for the tree's icon column.
fn tree_icon(glyph: char, color: iced::Color) -> Element<'static, Message> {
    container(icon_text(glyph, color, 14.0))
        .width(18)
        .align_x(iced::alignment::Horizontal::Center)
        .into()
}

/// A centered empty / loading state: a large muted icon, a title, a subtitle,
/// and an optional action button — so every "nothing here yet" screen matches.
fn empty_state<'a>(
    g: Glyph,
    title: &'a str,
    subtitle: &'a str,
    action: Option<(&'a str, Message)>,
) -> Element<'a, Message> {
    let mut col = column![
        glyph::icon(g, theme::rgb(0x434b57), 42.0),
        space().height(6),
        text(title.to_string()).size(14).color(theme::FG),
        container(
            text(subtitle.to_string())
                .size(12)
                .color(theme::DIM)
                .align_x(iced::Center)
        )
        .max_width(260),
    ]
    .spacing(4)
    .align_x(iced::Center);
    if let Some((label, msg)) = action {
        col = col.push(space().height(10));
        col = col.push(
            button(text(label.to_string()).size(13))
                .style(theme::toolbar_button)
                .padding([7, 16])
                .on_press(msg),
        );
    }
    center(col).padding(20).into()
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn search_tab(app: &App) -> Element<'_, Message> {
    use crate::SearchOpt;

    let input = text_input("Search in project…", &app.search.query)
        .id(search_input_id())
        .on_input(Message::SearchQueryChanged)
        .on_submit(Message::SearchSubmitted)
        .size(13)
        .padding(7);

    // Match-option toggles: case-sensitive, whole-word, regex. Each carries a
    // border so it reads as a clickable control even when off, and fills with the
    // accent (like VS Code's search toggles) when on.
    let chip = |label: &'static str, active: bool, opt: SearchOpt| -> Element<'_, Message> {
        let style = move |_t: &iced::Theme, status: button::Status| {
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            let bg = if active {
                theme::ACCENT
            } else if hovered {
                theme::BG_HOVER
            } else {
                theme::BG
            };
            button::Style {
                background: Some(bg.into()),
                text_color: if active { theme::rgb(0x1b1d23) } else { theme::FG_MUTED },
                border: iced::Border {
                    radius: 4.0.into(),
                    width: 1.0,
                    color: if active { theme::ACCENT } else { theme::HAIRLINE },
                },
                ..button::Style::default()
            }
        };
        button(text(label).size(12).font(Font::MONOSPACE))
            .style(style)
            .padding([2, 7])
            .on_press(Message::SearchToggle(opt))
            .into()
    };
    let options = row![
        chip("Aa", app.search.case_sensitive, SearchOpt::Case),
        chip("W", app.search.whole_word, SearchOpt::WholeWord),
        chip(".*", app.search.regex, SearchOpt::Regex),
    ]
    .spacing(4);

    // Include / exclude glob filters.
    let include = text_input("files to include (e.g. *.rs)", &app.search.include)
        .on_input(Message::SearchIncludeChanged)
        .on_submit(Message::SearchSubmitted)
        .size(12)
        .padding(5);
    let exclude = text_input("files to exclude", &app.search.exclude)
        .on_input(Message::SearchExcludeChanged)
        .on_submit(Message::SearchSubmitted)
        .size(12)
        .padding(5);

    let status_line = if let Some(err) = &app.search.error {
        Some((err.clone(), theme::rgb(0xe06c75)))
    } else if app.search.running {
        Some(("Searching…".to_string(), theme::DIM))
    } else if app.search.ran {
        let n = app.search.hits.len();
        let msg = if n >= crate::search::MAX_HITS {
            format!("{n}+ matches (capped)")
        } else {
            format!("{n} matches")
        };
        Some((msg, theme::DIM))
    } else {
        None
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for hit in &app.search.hits {
        if last_rel != Some(hit.rel.as_str()) {
            last_rel = Some(hit.rel.as_str());
            rows.push(group_header(&hit.rel));
        }
        rows.push(
            button(
                row![
                    text(hit.line.to_string()).size(11).color(theme::DIM).width(36),
                    text(&hit.preview).size(12).wrapping(Wrapping::None),
                ]
                .spacing(4),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding {
                top: 1.0,
                right: 6.0,
                bottom: 1.0,
                left: 8.0,
            })
            .on_press(Message::OpenAbs {
                abs: hit.abs.clone(),
                line: Some(hit.line),
                push: true,
            })
            .into(),
        );
    }

    let mut col = column![input, options, include, exclude]
        .spacing(6)
        .padding(8);
    if let Some((status, color)) = status_line {
        col = col.push(text(status).size(11).color(color));
    }
    col.push(scrollable(Column::with_children(rows).width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill))
        .into()
}

/// Label a history entry: the symbol name recorded at nav time (stable as lines
/// shift), else `file:line`. Jumps land on symbol lines, so most read as names.
fn loc_label(loc: &crate::history::Loc, label: Option<&str>) -> String {
    if let Some(name) = label {
        return name.to_string();
    }
    let base = loc.path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    match loc.line {
        Some(l) => format!("{base}:{l}"),
        None => base.to_string(),
    }
}

/// The TRAIL tab: the navigation history as a tree. Backtracking then exploring
/// elsewhere branches (the old path is kept), so this is the full reading trail.
/// Indentation follows the tree depth; nodes with children can be collapsed;
/// click a node to jump. Scrolls both ways for deep/wide trees.
fn trail_tab(app: &App) -> Element<'_, Message> {
    let visits = app.history.flatten_with(&app.trail_collapsed);
    if visits.is_empty() {
        return empty_state(
            Glyph::Minimap,
            "No reading trail yet",
            "Jump around the code and your trail builds here.",
            None,
        );
    }

    let header = row![
        text("Reading trail").size(11).color(theme::DIM),
        space().width(Fill),
        button(text("Clear").size(10).color(theme::DIM))
            .style(theme::list_row(false))
            .padding([1, 6])
            .on_press(Message::HistoryClear),
    ]
    .align_y(iced::Center)
    .padding(Padding { top: 4.0, right: 8.0, bottom: 2.0, left: 8.0 });

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for v in &visits {
        // Indent by tree depth, but cap it: past ~8 levels a deep branch would
        // otherwise push the node (including the current one) off the panel's
        // right edge. Beyond the cap, depth stops adding indent.
        let indent = 4.0 + (v.depth.min(8) as f32) * 10.0;
        let name_color = if v.is_current { theme::ACCENT } else { theme::FG };
        let fname = v.loc.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let (glyph, gcolor) = crate::icons::file_icon(fname);

        // A collapse chevron for nodes with children (forks stand out in accent);
        // leaves get a fixed-width spacer so names still line up.
        let toggle: Element<'_, Message> = if v.has_children {
            let ar = if v.collapsed { "▸" } else { "▾" };
            button(text(ar).size(10).color(theme::DIM))
                .style(theme::list_row(false))
                .padding([2, 3])
                .on_press(Message::TrailToggleCollapse(v.id))
                .into()
        } else {
            space().width(12).into()
        };
        // Status marker: current ● (accent), fork ⋔ (accent), other visited
        // nodes a grey ● so the trail reads as a string of nodes. The dots are
        // small; the fork glyph stays readable.
        let (marker, mcolor, msize) = if v.is_current {
            ("●", theme::ACCENT, 7.0)
        } else if v.forks {
            ("⋔", theme::ACCENT, 11.0)
        } else {
            ("●", theme::DIM, 7.0)
        };
        let jump = button(
            row![
                text(marker).size(msize).color(mcolor).width(10),
                icon_text(glyph, gcolor, 12.0),
                column![
                    text(loc_label(&v.loc, v.label.as_deref())).size(12).color(name_color),
                    text(rel_of(app, &v.loc.path)).size(9).color(theme::DIM).wrapping(Wrapping::None),
                ],
            ]
            .spacing(5)
            .align_y(iced::Center),
        )
        .style(theme::list_row(v.is_current))
        .padding(Padding { top: 2.0, right: 8.0, bottom: 2.0, left: 4.0 })
        .on_press(Message::HistoryJump(v.id));

        rows.push(
            row![space().width(indent), toggle, jump]
                .spacing(1)
                .align_y(iced::Center)
                .into(),
        );
    }
    column![
        header,
        scrollable(Column::with_children(rows).spacing(1))
            .direction(Direction::Both {
                vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
                horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
            })
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .into()
}

fn marks_tab(app: &App) -> Element<'_, Message> {
    if app.bookmarks.is_empty() {
        return empty_state(
            Glyph::Bookmark,
            "No bookmarks yet",
            "Press ⌘D to mark the current line.",
            None,
        );
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for (idx, bm) in app.bookmarks.iter().enumerate() {
        if last_rel != Some(bm.rel.as_str()) {
            last_rel = Some(bm.rel.as_str());
            rows.push(group_header(&bm.rel));
        }
        // Clip the preview to its own column so a long line never draws over the
        // trailing pencil/✕ icons; truncate with an ellipsis for the cut affordance.
        let top = row![
            text(bm.line.to_string()).size(11).color(theme::DIM).width(36),
            container(text(truncate_ellipsis(&bm.preview, 48)).size(12).wrapping(Wrapping::None))
                .clip(true)
                .width(Fill),
        ]
        .spacing(4)
        .width(Fill);
        // A saved note shows as a wrapped dim line under the preview.
        let main: Element<'_, Message> = match &bm.note {
            Some(note) => column![
                top,
                container(text(note).size(10).color(theme::FG_MUTED).wrapping(Wrapping::Word))
                    .padding(Padding { top: 0.0, right: 4.0, bottom: 0.0, left: 40.0 }),
            ]
            .spacing(1)
            .width(Fill)
            .into(),
            None => top.into(),
        };
        let note_color = if bm.note.is_some() { theme::ACCENT } else { theme::DIM };
        // The whole row is one full-width button (jump); the pencil/✕ are inner
        // buttons that capture their own clicks, so the highlight spans the row.
        let pencil = button(glyph::icon(Glyph::Edit, note_color, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::BookmarkNoteEdit(bm.rel.clone(), bm.line));
        let close = button(glyph::icon(Glyph::Close, theme::DIM, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::BookmarkRemoved(idx));
        rows.push(
            button(row![main, pencil, close].spacing(2).align_y(iced::Center))
                .style(theme::list_row(false))
                .width(Fill)
                .padding(Padding { top: 2.0, right: 4.0, bottom: 2.0, left: 8.0 })
                .on_press(Message::OpenRel {
                    rel: bm.rel.clone(),
                    line: Some(bm.line),
                })
                .into(),
        );
    }

    column![scrollable(Column::with_children(rows).width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill)]
        .padding(Padding {
            top: 6.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .into()
}

/// The NOTES tab: every reading note grouped by file, with progress. Each note
/// jumps to its symbol's live line; a note whose symbol has vanished is flagged
/// "detached" (it opens the file top) rather than pointing at the wrong code.
fn notes_tab(app: &App) -> Element<'_, Message> {
    if app.notes.is_empty() {
        return empty_state(
            Glyph::Note,
            "No reading notes yet",
            "In the OUTLINE, click ○ to mark a symbol understood, or ✎ to add a note.",
            None,
        );
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for n in &app.notes {
        if last_rel != Some(n.rel.as_str()) {
            last_rel = Some(n.rel.as_str());
            rows.push(group_header(&n.rel));
        }
        let line = app.note_symbol_line(&n.rel, &n.symbol);
        // Leading understood toggle.
        let (cg, gcolor) =
            if n.understood { (Glyph::CheckCircle, theme::ACCENT) } else { (Glyph::Circle, theme::DIM) };
        let toggle = button(glyph::icon(cg, gcolor, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::NoteToggleUnderstood { rel: n.rel.clone(), symbol: n.symbol.clone() });

        // Symbol name + its live location (or a "detached" flag when orphaned).
        let loc: Element<'_, Message> = match line {
            Some(l) => text(format!("L{l}")).size(10).color(theme::DIM).into(),
            None => text("detached").size(10).color(theme::rgb(0xe5c07b)).into(),
        };
        let head = row![
            text(&n.symbol)
                .size(12)
                .color(if n.understood { theme::DIM } else { theme::FG })
                .wrapping(Wrapping::None),
            loc,
        ]
        .spacing(6)
        .width(Fill)
        .align_y(iced::Center);
        let main: Element<'_, Message> = if n.text.is_empty() {
            head.into()
        } else {
            column![
                head,
                container(text(&n.text).size(10).color(theme::FG_MUTED).wrapping(Wrapping::Word))
                    .padding(Padding { top: 0.0, right: 4.0, bottom: 0.0, left: 0.0 }),
            ]
            .spacing(1)
            .width(Fill)
            .into()
        };

        let note_color = if n.text.is_empty() { theme::DIM } else { theme::ACCENT };
        let pencil = button(glyph::icon(Glyph::Edit, note_color, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::NoteEditStart { rel: n.rel.clone(), symbol: n.symbol.clone() });
        let close = button(glyph::icon(Glyph::Close, theme::DIM, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::NoteRemove { rel: n.rel.clone(), symbol: n.symbol.clone() });
        // The name area jumps; the toggle/pencil/✕ capture their own clicks.
        let jump = button(main)
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding { top: 2.0, right: 4.0, bottom: 2.0, left: 4.0 })
            .on_press(Message::NoteJump { rel: n.rel.clone(), symbol: n.symbol.clone() });
        rows.push(row![toggle, jump, pencil, close].spacing(1).align_y(iced::Center).into());
    }

    column![
        scrollable(Column::with_children(rows).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill)
    ]
    .padding(Padding { top: 6.0, right: 0.0, bottom: 0.0, left: 0.0 })
    .into()
}

/// Short badge for an LSP SymbolKind number.
fn kind_short(kind: u8) -> &'static str {
    match kind {
        12 | 6 | 9 => "fn",  // Function / Method / Constructor
        5 | 23 => "type",    // Class / Struct
        11 | 10 => "trait",  // Interface / Enum
        2 => "mod",          // Module
        _ => "",
    }
}

/// The "Ask" (semantic search) tab: a natural-language query over the embedding
/// index, with a build/refresh control and ranked results that jump to the code.
fn semantic_tab(app: &App) -> Element<'_, Message> {
    use crate::explain::Node;
    let n = app.embed_index.entries.len();

    let input = text_input("Ask by meaning…", &app.semantic_query)
        .on_input(Message::SemanticQueryChanged)
        .on_submit(Message::SemanticSearch)
        .size(13)
        .padding(7);

    let build_label = if app.building_embeddings {
        "Building…"
    } else if n == 0 {
        "Build index"
    } else {
        "Rebuild"
    };
    let mut build = button(text(build_label).size(11)).style(theme::toolbar_button).padding([2, 8]);
    if !app.building_embeddings {
        build = build.on_press(Message::BuildEmbeddings);
    }
    // The index builds itself from explanation summaries (automatically, after
    // Explain All) — so the hint only reports state, never asks for a manual step.
    let info = text(if app.building_embeddings {
        "Building the index…".to_string()
    } else if n > 0 {
        format!("{n} indexed")
    } else if app.explanations.is_empty() {
        "Run Explain All to enable semantic search.".to_string()
    } else if !app.embed_available {
        "Set an embedding provider in Settings.".to_string()
    } else {
        "Preparing the index…".to_string()
    })
    .size(10)
    .color(theme::DIM);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(row![build, space().width(Fill), info].align_y(iced::Center).into());
    if app.searching_semantic {
        rows.push(text("Searching…").size(11).color(theme::DIM).into());
    }
    for (node, score) in &app.semantic_results {
        let label = match node {
            Node::Function { file, name } => format!("{name} · {}", rel_of(app, file)),
            Node::File(p) => rel_of(app, p),
            Node::Folder(p) => rel_of(app, p),
        };
        let sum = app.explanations.get(node).map(|c| c.summary.as_str()).unwrap_or("");
        let short: String = sum.chars().take(96).collect();
        rows.push(
            button(
                column![
                    row![
                        text(label).size(12).color(theme::ACCENT).wrapping(Wrapping::None),
                        space().width(Fill),
                        text(format!("{:.0}%", score * 100.0)).size(9).color(theme::DIM),
                    ]
                    .align_y(iced::Center),
                    text(short).size(10).color(theme::DIM),
                ]
                .spacing(1),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([3, 6])
            .on_press(Message::OpenNode(node.clone()))
            .into(),
        );
    }

    container(
        column![
            input,
            scrollable(Column::with_children(rows).spacing(4).width(Fill))
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(Fill),
        ]
        .spacing(8)
        .padding([8, 8]),
    )
    .height(Fill)
    .into()
}

/// A clickable chip for a retrieved source node — jumps to the code on press,
/// showing the similarity score when it came from the ranked retrieval.
fn source_chip<'a>(node: &crate::explain::Node, score: f32) -> Element<'a, Message> {
    use crate::explain::Node;
    let label = match node {
        Node::Function { name, .. } => name.clone(),
        Node::File(p) | Node::Folder(p) => {
            p.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string()
        }
    };
    let pct = if score > 0.0 {
        format!("  {}%", (score * 100.0).round() as i32)
    } else {
        String::new()
    };
    button(text(format!("{label}{pct}")).size(10).color(theme::DIM))
        .style(theme::toolbar_button)
        .padding([1, 6])
        .on_press(Message::OpenNode(node.clone()))
        .into()
}

/// The "Ask clew" bottom panel: a scrollable multi-turn Q&A over a question box.
/// Answers are grounded in retrieved code, cite it with jump links, and list
/// their retrieved sources as clickable chips.
/// One scrollable column of the debug panel (call stack / variables / output).
fn debug_col(rows: Vec<Element<'_, Message>>) -> Element<'_, Message> {
    container(
        scrollable(Column::with_children(rows).spacing(1).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    )
        .width(Fill)
        .height(Fill)
        .padding([4, 6])
        .into()
}

/// The bottom debugger panel: status + step controls, and three columns —
/// call stack (click a frame to jump), variables, and program output.
/// The collapsible bottom panel: a tab bar (Ask / Debug, like the left sidebar)
/// with a collapse control, above the selected tab's content.
fn bottom_panel(app: &App) -> Element<'_, Message> {
    use crate::BottomTab;
    let tab = |g: Glyph, label: &'static str, this: BottomTab| {
        let active = app.bottom_tab == this;
        let tint = if active { theme::FG_BRIGHT } else { theme::FG_MUTED };
        button(
            row![glyph::icon(g, tint, 16.0), text(label).size(11)]
                .spacing(6)
                .align_y(iced::Center),
        )
        .style(theme::tab_button(active))
        .padding([5, 12])
        .on_press(Message::BottomTabPicked(this))
    };
    // A borderless "hide" affordance — no button box, just a chevron that
    // brightens on hover.
    let collapse = button(text("⌄").size(13))
        .style(theme::list_row(false))
        .padding([3, 10])
        .on_press(Message::CollapseBottom);
    let tabs = row![
        tab(Glyph::Ask, "Ask", BottomTab::Ask),
        tab(Glyph::Debug, "Debug", BottomTab::Debug),
        space().width(Fill),
        collapse,
    ]
    .spacing(2)
    .align_y(iced::Center)
    .padding(Padding { top: 2.0, right: 6.0, bottom: 2.0, left: 6.0 });

    let content: Element<'_, Message> = match app.bottom_tab {
        BottomTab::Ask => ask_panel(app),
        BottomTab::Debug if app.debug.is_some() => debug_panel(app),
        BottomTab::Debug => empty_state(
            Glyph::Debug,
            "No debug session",
            "Press Debug in the toolbar to start one (needs .clew/launch.json).",
            None,
        ),
    };

    container(column![tabs, hairline(), content].height(Fill))
        .height(Fill)
        .style(theme::panel)
        .into()
}

fn debug_panel(app: &App) -> Element<'_, Message> {
    use crate::{DebugCmd, DebugStatus};
    let Some(session) = app.debug.as_ref() else {
        return space().into();
    };
    let (status_txt, status_color) = match session.status {
        DebugStatus::Launching => ("launching…", theme::DIM),
        DebugStatus::Running => ("running", theme::rgb(0x98c379)),
        DebugStatus::Stopped => ("stopped", theme::rgb(0xe5c07b)),
        DebugStatus::Terminated => ("terminated", theme::DIM),
    };
    let stopped = session.status == DebugStatus::Stopped;

    let ctrl = |label: &'static str, msg: Message, enabled: bool| {
        let mut b = button(text(label).size(12)).style(theme::toolbar_button).padding([2, 8]);
        if enabled {
            b = b.on_press(msg);
        }
        b
    };
    let controls = row![
        ctrl("▶ Continue", Message::DebugControl(DebugCmd::Continue), stopped),
        ctrl("⤼ Over", Message::DebugControl(DebugCmd::StepOver), stopped),
        ctrl("⤓ In", Message::DebugControl(DebugCmd::StepIn), stopped),
        ctrl("⤒ Out", Message::DebugControl(DebugCmd::StepOut), stopped),
        ctrl("■ Stop", Message::DebugStop, true),
    ]
    .spacing(4);
    let header = row![
        text("Debug").size(13).color(theme::FG),
        text(status_txt).size(11).color(status_color),
        space().width(Fill),
        controls,
    ]
    .spacing(8)
    .align_y(iced::Center);

    // Call stack — click a frame to jump to its source.
    let mut stack_rows: Vec<Element<'_, Message>> =
        vec![text("CALL STACK").size(10).color(theme::DIM).into()];
    for f in &session.frames {
        let loc = f
            .path
            .as_ref()
            .map(|p| format!("{}:{}", rel_of(app, p), f.line))
            .unwrap_or_default();
        let mut b = button(
            column![
                text(f.name.clone()).size(11).color(theme::ACCENT).wrapping(Wrapping::None),
                text(loc).size(9).color(theme::DIM).wrapping(Wrapping::None),
            ]
            .spacing(0),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([1, 6]);
        if let Some(p) = f.path.clone() {
            b = b.on_press(Message::OverlayOpenAt { abs: p, line: f.line });
        }
        stack_rows.push(b.into());
    }

    // Variables — each scope with its name = value rows.
    let mut var_rows: Vec<Element<'_, Message>> =
        vec![text("VARIABLES").size(10).color(theme::DIM).into()];
    for sc in &session.scopes {
        var_rows.push(text(sc.name.clone()).size(10).color(theme::DIM).into());
        for v in &sc.vars {
            var_rows.push(
                row![
                    text(v.name.clone()).size(11).color(theme::rgb(0xe5c07b)),
                    text(" = ").size(11).color(theme::DIM),
                    text(v.value.clone()).size(11).color(theme::FG).wrapping(Wrapping::None),
                ]
                .into(),
            );
        }
    }

    // Program output.
    let mut out_rows: Vec<Element<'_, Message>> =
        vec![text("OUTPUT").size(10).color(theme::DIM).into()];
    for (cat, txt) in &session.output {
        let color = if cat == "stderr" { theme::rgb(0xe06c75) } else { theme::FG };
        out_rows.push(
            text(txt.trim_end_matches('\n').to_string())
                .size(11)
                .color(color)
                .wrapping(Wrapping::None)
                .into(),
        );
    }

    // Watch — expressions re-evaluated each stop, with an add box + remove.
    let mut watch_rows: Vec<Element<'_, Message>> =
        vec![text("WATCH").size(10).color(theme::DIM).into()];
    watch_rows.push(
        text_input("Add watch…", &app.debug_watch_input)
            .on_input(Message::DebugWatchInput)
            .on_submit(Message::DebugWatchAdd)
            .size(11)
            .padding(3)
            .into(),
    );
    for (i, expr) in app.debug_watches.iter().enumerate() {
        let val = session
            .watches
            .iter()
            .find(|(e, _)| e == expr)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "…".into());
        watch_rows.push(
            row![
                button(text("✕").size(9).color(theme::DIM))
                    .style(theme::list_row(false))
                    .padding([0, 4])
                    .on_press(Message::DebugWatchRemove(i)),
                text(expr.clone()).size(11).color(theme::ACCENT),
                text(" = ").size(11).color(theme::DIM),
                text(val).size(11).color(theme::FG).wrapping(Wrapping::None),
            ]
            .spacing(2)
            .align_y(iced::Center)
            .into(),
        );
    }

    let panels = row![
        debug_col(stack_rows),
        debug_col(var_rows),
        debug_col(watch_rows),
        debug_col(out_rows)
    ]
    .spacing(6)
    .height(Fill);

    container(column![header, panels].spacing(6).padding([8, 12]))
        .width(Fill)
        .height(Fill)
        .style(theme::panel)
        .into()
}

fn ask_panel(app: &App) -> Element<'_, Message> {
    // The tab bar already names the panel and offers collapse, so the only header
    // control left is "Clear", and only once there's a conversation to clear.
    let header: Element<'_, Message> = if app.ask_turns.is_empty() {
        space().height(0).into()
    } else {
        row![
            space().width(Fill),
            button(text("Clear").size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::AskClear),
        ]
        .align_y(iced::Center)
        .into()
    };

    let mut convo: Vec<Element<'_, Message>> = Vec::new();
    if app.ask_turns.is_empty() && !app.asking {
        convo.push(
            text("Ask a question about this codebase. Answers cite the code and jump to it. \
                  Follow-ups keep the conversation. Select code and right-click → “Add to Ask” \
                  to attach snippets as context.")
                .size(12)
                .color(theme::DIM)
                .into(),
        );
        // Answers are grounded in the semantic index (or pinned snippets). Say so
        // upfront when neither exists, rather than only rejecting on submit — so
        // this matches how Overview/FIND show their "run Explain All first" state.
        if app.embed_index.entries.is_empty() && app.ask_pins.is_empty() {
            convo.push(
                text("Run “Explain All” to ground answers in the code — or right-click code → \
                      “Add to Ask” to ground a single question now.")
                    .size(11)
                    .color(theme::WARN)
                    .into(),
            );
        }
        // Context-aware starter questions — click one to ask it.
        let suggestions = app.suggested_questions();
        if !suggestions.is_empty() {
            convo.push(text("Try asking").size(10).color(theme::DIM).into());
            let chips: Vec<Element<'_, Message>> = suggestions
                .into_iter()
                .map(|q| {
                    button(text(strip_backticks(&q)).size(11).color(theme::ACCENT))
                        .style(theme::list_row(false))
                        .padding([3, 8])
                        .on_press(Message::AskSuggested(q))
                        .into()
                })
                .collect();
            convo.push(Row::with_children(chips).spacing(4).wrap().into());
        }
    }
    for turn in &app.ask_turns {
        convo.push(text(format!("❯ {}", turn.question)).size(13).color(theme::ACCENT).into());
        if turn.streaming {
            // Live answer: "Thinking…" until the first token, then the raw text
            // (with a cursor) as it streams; it's re-rendered richly when done.
            if turn.answer_md.trim().is_empty() {
                convo.push(text("Thinking…").size(12).color(theme::DIM).into());
            } else {
                convo.push(
                    text(format!("{}▍", turn.answer_md))
                        .size(13)
                        .color(theme::FG)
                        .into(),
                );
            }
        } else {
            convo.extend(render_prepared(app, &turn.answer));
        }
        if !turn.sources.is_empty() {
            convo.push(text("Sources").size(10).color(theme::DIM).into());
            let chips: Vec<Element<'_, Message>> =
                turn.sources.iter().map(|(n, s)| source_chip(n, *s)).collect();
            convo.push(Row::with_children(chips).spacing(4).wrap().into());
        }
    }
    // Retrieval phase (before the answer turn exists) shows a spinner line.
    if app.asking {
        convo.push(text("Thinking…").size(12).color(theme::DIM).into());
    }
    let conversation =
        scrollable(Column::with_children(convo).spacing(8).width(Fill))
            .id(ask_scroll_id())
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill);

    // Compose area: the pinned-selection chips (each a clickable jump + remove)
    // above the input row. Chips persist across turns and wrap when there are
    // several.
    let mut compose: Vec<Element<'_, Message>> = Vec::new();
    if !app.ask_pins.is_empty() {
        let chips: Vec<Element<'_, Message>> = app
            .ask_pins
            .iter()
            .enumerate()
            .map(|(i, pin)| {
                container(
                    row![
                        button(text(format!("📎 {} · L{}", pin.rel, pin.line)).size(11).color(theme::ACCENT))
                            .style(theme::toolbar_button)
                            .padding([0, 4])
                            .on_press(Message::AskPinGoto(i)),
                        button(text("✕").size(11).color(theme::DIM))
                            .style(theme::toolbar_button)
                            .padding([0, 6])
                            .on_press(Message::AskUnpin(i)),
                    ]
                    .spacing(2)
                    .align_y(iced::Center),
                )
                .padding([1, 2])
                .style(theme::panel)
                .into()
            })
            .collect();
        compose.push(Row::with_children(chips).spacing(4).wrap().into());
    }
    let input = text_input("Ask about this codebase…", &app.ask_input)
        .id(ask_input_id())
        .on_input(Message::AskInputChanged)
        .on_submit(Message::AskSubmit)
        .size(13)
        .padding(7);
    // Match the input's height (size 13 + 7 padding) so the row lines up. The
    // send button is the panel's primary action, so it gets accent emphasis
    // (dimmed to a plain style while a request is in flight / disabled).
    let idle = !app.asking;
    let mut ask_btn = button(text("Ask").size(13))
        .style(if idle { theme::primary_button } else { theme::toolbar_button })
        .padding([7, 16]);
    if idle {
        ask_btn = ask_btn.on_press(Message::AskSubmit);
    }
    compose.push(row![input, ask_btn].spacing(6).align_y(iced::Center).into());

    container(
        column![header, conversation, Column::with_children(compose).spacing(4)]
            .spacing(8)
            .padding([8, 12]),
    )
    .width(Fill)
    .height(Fill)
    .style(theme::panel)
    .into()
}

/// The call-hierarchy tree: a header with the root symbol + a callers/callees
/// toggle, then the lazily-expanded tree.
fn calls_tab(app: &App) -> Element<'_, Message> {
    let Some(tree) = &app.call_graph else {
        return empty_state(
            Glyph::CallGraph,
            "No call hierarchy yet",
            "Put the cursor on a function and press gc, or right-click it → Call Hierarchy.",
            None,
        );
    };

    let header = container(
        row![
            text(&tree.root_name)
                .size(12)
                .color(theme::ACCENT)
                .wrapping(Wrapping::None),
            space().width(Fill),
            button(text("⇊ all").size(11))
                .style(theme::toolbar_button)
                .padding([2, 7])
                .on_press(Message::CallHierarchyExpandAll),
            button(text(tree.direction.label()).size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::CallHierarchyDirection),
        ]
        .spacing(4)
        .align_y(iced::Center),
    )
    .padding(Padding {
        top: 6.0,
        right: 8.0,
        bottom: 6.0,
        left: 10.0,
    })
    .style(theme::pane_header)
    .width(Fill);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for id in tree.visible() {
        let node = tree.node(id);
        // Expansion affordance: an arrow for fetchable nodes, a loop glyph for
        // recursion, blank for a leaf with no further calls.
        let arrow: Element<'_, Message> = if node.loading {
            text("…").size(11).color(theme::ACCENT).width(16).into()
        } else if node.cyclic {
            text("↺").size(11).color(theme::DIM).width(16).into()
        } else if node.children.as_ref().is_some_and(|c| c.is_empty()) {
            space().width(16).into()
        } else {
            button(text(if node.expanded { "▾" } else { "▸" }).size(11).color(theme::DIM))
                .style(theme::list_row(false))
                .padding([0, 3])
                .on_press(Message::CallHierarchyExpand(id))
                .into()
        };

        let kind = kind_short(node.item.kind);
        let fname = node
            .item
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let name_btn = button(
            row![
                text(&node.item.name).size(12).wrapping(Wrapping::None),
                space().width(6),
                text(format!("{fname}:{}", node.item.line + 1))
                    .size(10)
                    .color(theme::DIM)
                    .wrapping(Wrapping::None),
            ]
            .align_y(iced::Center),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([1, 4])
        .on_press(Message::OpenAbs {
            abs: node.item.path.clone(),
            line: Some(node.item.line + 1),
            push: true,
        });

        let badge = text(kind).size(9).color(theme::DIM).width(if kind.is_empty() {
            0.0
        } else {
            22.0
        });

        rows.push(
            row![
                space().width(node.depth as f32 * 12.0),
                arrow,
                badge,
                name_btn,
            ]
            .spacing(2)
            .align_y(iced::Center)
            .into(),
        );
    }

    let mut col = column![header];
    if tree.stale {
        col = col.push(
            container(
                text("⟳ code changed — press gc to refresh")
                    .size(10)
                    .color(theme::rgb(0xe5c07b)),
            )
            .padding(Padding {
                top: 3.0,
                right: 8.0,
                bottom: 3.0,
                left: 10.0,
            })
            .width(Fill),
        );
    }
    col.push(scrollable(Column::with_children(rows).width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill))
        .into()
}

/// The import tree: a header with the focus file + an Imports/Importers toggle,
/// a cycles banner, then the lazily-expanded (but synchronous) tree.
fn imports_tab(app: &App) -> Element<'_, Message> {
    use crate::imports::Target;

    let Some(tree) = &app.import_tree else {
        return container(
            column![
                text("No file focused.").size(12).color(theme::DIM),
                space().height(6),
                text("Open a source file to see what it")
                    .size(11)
                    .color(theme::DIM),
                text("imports and what imports it.")
                    .size(11)
                    .color(theme::DIM),
            ]
            .spacing(2),
        )
        .padding(12)
        .into();
    };

    let header = container(
        row![
            text(&tree.root_name)
                .size(12)
                .color(theme::ACCENT)
                .wrapping(Wrapping::None),
            space().width(Fill),
            button(text("⇊ all").size(11))
                .style(theme::toolbar_button)
                .padding([2, 7])
                .on_press(Message::ImportExpandAll),
            button(text(tree.direction.label()).size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::ImportDirection),
        ]
        .spacing(4)
        .align_y(iced::Center),
    )
    .padding(Padding {
        top: 6.0,
        right: 8.0,
        bottom: 6.0,
        left: 10.0,
    })
    .style(theme::pane_header)
    .width(Fill);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for id in tree.visible() {
        let node = tree.node(id);
        // Expansion affordance: a loop glyph for a cycle, blank for a leaf
        // (external/unresolved, or an already-expanded internal with no edges),
        // an arrow otherwise.
        let arrow: Element<'_, Message> = if node.cyclic {
            text("↺").size(11).color(theme::DIM).width(16).into()
        } else if node.children.as_ref().is_some_and(|c| c.is_empty()) {
            space().width(16).into()
        } else {
            button(text(if node.expanded { "▾" } else { "▸" }).size(11).color(theme::DIM))
                .style(theme::list_row(false))
                .padding([0, 3])
                .on_press(Message::ImportExpand(id))
                .into()
        };

        // Internal files open on click; external/unresolved are dim leaves.
        let name: Element<'_, Message> = match &node.target {
            Target::Internal(path) => button(
                row![
                    text(&node.label).size(12).wrapping(Wrapping::None),
                    space().width(6),
                    text(&node.detail).size(10).color(theme::DIM).wrapping(Wrapping::None),
                ]
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([1, 4])
            .on_press(Message::OpenAbs {
                abs: path.clone(),
                line: None,
                push: true,
            })
            .into(),
            Target::External(_) => container(
                row![
                    text(&node.label).size(12).color(theme::DIM).wrapping(Wrapping::None),
                    space().width(6),
                    text("ext").size(9).color(theme::DIM),
                ]
                .align_y(iced::Center),
            )
            .padding([1, 4])
            .width(Fill)
            .into(),
            Target::Unresolved(_) => container(
                row![
                    text(&node.label).size(12).color(theme::DIM).wrapping(Wrapping::None),
                    space().width(6),
                    text("?").size(10).color(theme::DIM),
                ]
                .align_y(iced::Center),
            )
            .padding([1, 4])
            .width(Fill)
            .into(),
        };

        rows.push(
            row![space().width(node.depth as f32 * 12.0), arrow, name]
                .spacing(2)
                .align_y(iced::Center)
                .into(),
        );
    }

    let mut col = column![header];
    if !app.import_cycles.is_empty() {
        let n = app.import_cycles.len();
        col = col.push(
            container(
                text(format!(
                    "⚠ {n} import cycle{}",
                    if n == 1 { "" } else { "s" }
                ))
                .size(10)
                .color(theme::rgb(0xe5c07b)),
            )
            .padding(Padding {
                top: 3.0,
                right: 8.0,
                bottom: 3.0,
                left: 10.0,
            })
            .width(Fill),
        );
    }
    col.push(scrollable(Column::with_children(rows).width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill))
        .into()
}

fn group_header(rel: &str) -> Element<'_, Message> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let (glyph, color) = crate::icons::file_icon(name);
    container(
        row![
            icon_text(glyph, color, 12.0),
            text(rel).size(11).color(theme::FG_MUTED).wrapping(Wrapping::None),
        ]
        .spacing(5)
        .align_y(iced::Center),
    )
    .padding(Padding {
        top: 8.0,
        right: 6.0,
        bottom: 2.0,
        left: 8.0,
    })
    .into()
}

// ---------------------------------------------------------------- code panes

fn pane_area(app: &App) -> Element<'_, Message> {
    if app.scanning {
        return editor_shell(empty_state(
            Glyph::Search,
            "Scanning project…",
            "Indexing files so you can read and search them.",
            None,
        ));
    }
    if app.project.is_none() {
        return editor_shell(welcome(app));
    }
    if let Some(page) = &app.docs_page {
        return editor_shell(docs_page(app, page));
    }
    if app.show_overview {
        return editor_shell(overview_home(app));
    }
    if app.show_stats {
        return editor_shell(stats_home(app));
    }
    if !app.split {
        return pane_view(app, 0);
    }
    row![pane_view(app, 0), pane_view(app, 1)]
        .spacing(1)
        .into()
}

/// Map a file rel to a module/package label for the "Modules" grouping — a
/// display heuristic per language (Rust `src/lsp/client.rs` -> `lsp::client`,
/// Python `foo/bar.py` -> `foo.bar`, Go by directory/package, etc.). Files that
/// map to the same label are merged into one module group.
fn module_label(rel: &str) -> String {
    let lang = match rel.rsplit('.').next() {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("go") => "go",
        Some("ts") | Some("tsx") => "ts",
        Some("js") | Some("jsx") => "js",
        Some("dart") => "dart",
        _ => "",
    };
    let no_ext = rel.rsplit_once('.').map(|(a, _)| a).unwrap_or(rel);
    let mut segs: Vec<&str> = no_ext.split('/').filter(|s| !s.is_empty()).collect();
    // Drop a conventional source root.
    if segs.len() > 1 && matches!(segs.first().copied(), Some("src") | Some("lib")) {
        segs.remove(0);
    }
    // A file that names its parent module collapses to the directory.
    let is_dir_file = (lang == "rust" && matches!(segs.last().copied(), Some("mod") | Some("lib") | Some("main")))
        || (lang == "python" && segs.last().copied() == Some("__init__"))
        || (matches!(lang, "ts" | "js") && segs.last().copied() == Some("index"));
    if is_dir_file {
        segs.pop();
    }
    // Go's unit is the package = the directory.
    if lang == "go" && !segs.is_empty() {
        segs.pop();
    }
    if segs.is_empty() {
        return "(root)".to_string();
    }
    let sep = match lang {
        "rust" => "::",
        "python" => ".",
        _ => "/",
    };
    segs.join(sep)
}

/// Short badge for a symbol kind, shown before the name in the Docs tree/page.
fn kind_badge(kind: &str) -> &str {
    match kind {
        "function" | "fn" | "func" => "fn",
        "method" => "fn",
        "struct" => "struct",
        "enum" => "enum",
        "trait" | "interface" => "trait",
        "class" => "class",
        "constant" | "const" => "const",
        "module" | "mod" | "namespace" => "mod",
        "type" | "typealias" | "type_alias" => "type",
        "impl" => "impl",
        _ => kind,
    }
}

/// The DOCS sidebar tab: a filterable tree of files → public API items. Clicking
/// an item opens its doc page in the main pane.
fn docs_tab(app: &App) -> Element<'_, Message> {
    if app.docs.is_empty() {
        return if app.docs_loading {
            empty_state(Glyph::Sparkle, "Building docs…", "Reading the project's public API.", None)
        } else {
            empty_state(
                Glyph::Note,
                "No documentation",
                "No documented symbols found in this project.",
                Some(("Rebuild", Message::DocsRefresh)),
            )
        };
    }

    // Toolbar: a filter on top, then the grouping / visibility / rebuild
    // controls (two rows so they fit a narrow sidebar).
    let filter = text_input("Filter docs…", &app.docs_filter)
        .on_input(Message::DocsFilterChanged)
        .size(12)
        .padding(6)
        .width(Fill);
    let chip = |label: String, msg: Message| {
        button(text(label).size(11))
            .style(theme::toolbar_button)
            .padding([4, 8])
            .on_press(msg)
    };
    let group_btn = chip(
        if app.docs_by_module { "Modules".into() } else { "Files".into() },
        Message::DocsToggleGrouping,
    );
    let vis_btn = chip(
        if app.docs_show_all { "All".into() } else { "Public".into() },
        Message::DocsToggleShowAll,
    );
    let refresh = chip("↻".into(), Message::DocsRefresh);
    let controls = row![group_btn, vis_btn, space().width(Fill), refresh]
        .spacing(4)
        .align_y(iced::Center);
    let toolbar = column![filter, controls].spacing(4);

    let query = app.docs_filter.trim().to_lowercase();
    let selected_line = app
        .docs_page
        .as_ref()
        .and_then(|p| p.entries.first().map(|e| (p.rel.as_str(), e.line)));

    // Group the visible items by file or by module. Each group carries its items
    // as (source rel, item) so selection keeps working across merged files.
    let mut groups: std::collections::BTreeMap<String, Vec<(&str, &clew_protocol::DocItem)>> =
        std::collections::BTreeMap::new();
    for file in &app.docs {
        let label = if app.docs_by_module {
            module_label(&file.rel)
        } else {
            file.rel.clone()
        };
        // Match the filter against the symbol name OR the file path / module
        // label, so a path fragment like "http.dart" finds that file's symbols
        // (previously only the symbol name was matched, so paths found nothing).
        let path_matches = query.is_empty()
            || file.rel.to_lowercase().contains(&query)
            || label.to_lowercase().contains(&query);
        for item in &file.items {
            let matches = path_matches || item.name.to_lowercase().contains(&query);
            if (app.docs_show_all || item.public) && matches {
                groups.entry(label.clone()).or_default().push((&file.rel, item));
            }
        }
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for (label, mut items) in groups {
        if items.is_empty() {
            continue;
        }
        // Merged module groups read better alphabetically.
        if app.docs_by_module {
            items.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        }
        let expanded = !query.is_empty() || app.docs_expanded.contains(&label);
        let arrow = if expanded { "▾" } else { "▸" };
        rows.push(
            button(
                row![
                    text(arrow).size(10).color(theme::DIM).width(10),
                    text(label.clone()).size(12).color(theme::FG_MUTED).wrapping(Wrapping::None),
                ]
                .spacing(4)
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([3, 8])
            .on_press(Message::DocsToggleFile(label.clone()))
            .into(),
        );
        if expanded {
            for (rel, item) in items {
                let is_sel = selected_line == Some((rel, item.line));
                rows.push(
                    button(
                        row![
                            space().width(14),
                            text(kind_badge(&item.kind))
                                .size(10)
                                .color(theme::ACCENT)
                                .font(Font::MONOSPACE)
                                .width(42),
                            text(item.name.clone()).size(13).wrapping(Wrapping::None),
                        ]
                        .spacing(4)
                        .align_y(iced::Center),
                    )
                    .style(theme::list_row(is_sel))
                    .width(Fill)
                    .padding([3, 8])
                    .on_press(Message::DocsSelect {
                        rel: rel.to_string(),
                        line: item.line,
                    })
                    .into(),
                );
            }
        }
    }

    column![
        container(toolbar).padding([6, 6]),
        scrollable(Column::with_children(rows).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .into()
}

/// The main-pane doc page: the selected item followed by its members, each with
/// signature and rendered doc comment (like a rustdoc type page).
fn docs_page<'a>(app: &'a App, page: &'a crate::DocPage) -> Element<'a, Message> {
    let _ = app;
    let top_line = page.entries.first().map(|e| e.line);
    let header = row![
        text(page.rel.clone()).size(12).color(theme::DIM).wrapping(Wrapping::None),
        space().width(Fill),
        button(text("Open source").size(12))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::OpenRel {
                rel: page.rel.clone(),
                line: top_line,
            }),
    ]
    .align_y(iced::Center);

    let mut blocks: Vec<Element<'a, Message>> = Vec::new();
    for (idx, e) in page.entries.iter().enumerate() {
        let title_size = if idx == 0 { 22 } else { 15 };
        let title = row![
            text(kind_badge(&e.kind)).size(11).color(theme::ACCENT).font(Font::MONOSPACE),
            text(e.name.clone()).size(title_size).color(theme::FG),
        ]
        .spacing(8)
        .align_y(iced::Center);

        let signature = container(
            text(e.signature.clone())
                .size(12)
                .font(Font::MONOSPACE)
                .color(theme::FG_MUTED),
        )
        .padding([6, 10])
        .width(Fill)
        .style(theme::editor);

        let doc: Element<'a, Message> = if e.doc_items.is_empty() {
            text("No documentation.").size(12).color(theme::DIM).into()
        } else {
            iced::widget::markdown::view(&e.doc_items, iced::Theme::Dark)
                .map(|url| Message::OpenLink(url.to_string()))
        };

        let block = column![title, signature, doc].spacing(8);
        // Indent members under their type.
        let indent = e.depth as f32 * 18.0;
        blocks.push(
            container(block)
                .padding(Padding {
                    top: if idx == 0 { 0.0 } else { 14.0 },
                    right: 0.0,
                    bottom: 0.0,
                    left: indent,
                })
                .width(Fill)
                .into(),
        );
    }

    let body = scrollable(Column::with_children(blocks).spacing(4).width(Fill).padding(Padding {
        top: 6.0,
        right: 20.0,
        bottom: 24.0,
        left: 8.0,
    }))
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Fill);

    container(column![container(header).padding([10, 16]), body])
        .width(Fill)
        .height(Fill)
        .into()
}

/// The architecture-overview "home": the generated overview, a prompt to
/// generate it, or a generation-in-progress note.
fn overview_home(app: &App) -> Element<'_, Message> {
    let regen = |label: &'static str| {
        button(text(label).size(12))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::GenerateOverview)
    };

    if app.generating_overview {
        return center(text("Generating architecture overview…").size(14).color(theme::DIM)).into();
    }

    if app.overview.is_some() {
        let header = row![
            text("Architecture Overview").size(18).color(theme::FG),
            space().width(Fill),
            regen("Regenerate"),
        ]
        .align_y(iced::Center);
        // The module map, drawn natively (same engine as the Import Graph
        // overlay), sits at the top; the LLM prose follows.
        let mut items: Vec<Element<'_, Message>> = Vec::new();
        if let Some(layout) = app.overview_map.as_ref().filter(|l| !l.nodes.is_empty()) {
            items.push(
                column![
                    text("Module map").size(15).color(theme::FG_MUTED),
                    container(
                        iced::widget::canvas::Canvas::new(GraphCanvas {
                            layout,
                            kind: crate::Overlay::ProjectImports,
                        })
                        .width(Fill)
                        .height(iced::Length::Fixed(320.0)),
                    )
                    .width(Fill),
                    text("size = how connected · drag to pan · scroll to zoom · click a node to open it")
                        .size(10)
                        .color(theme::DIM),
                ]
                .spacing(6)
                .into(),
            );
        }
        items.extend(render_prepared(app, &app.overview_prepared));
        return container(
            column![
                header,
                scrollable(Column::with_children(items).spacing(10).width(Fill).max_width(860))
                    .direction(thin_scroll())
                    .style(theme::overlay_scrollbar)
                    .height(Fill),
            ]
            .spacing(14),
        )
        .width(Fill)
        .height(Fill)
        .padding([20, 28])
        .into();
    }

    // Not generated yet.
    let action: Element<'_, Message> = if !app.llm_available {
        text("Configure an LLM key in Settings to generate the overview.")
            .size(12)
            .color(theme::DIM)
            .into()
    } else if app.explanations.is_empty() {
        text("Run “Explain All” first — the overview is built from the explanations.")
            .size(12)
            .color(theme::DIM)
            .into()
    } else {
        regen("Generate overview").into()
    };
    center(
        column![
            text("Architecture Overview").size(18).color(theme::FG),
            text("A generated tour of this codebase: what it does, core modules, entry points, and where to start.")
                .size(13)
                .color(theme::DIM),
            action,
        ]
        .spacing(12)
        .align_x(iced::Center)
        .max_width(560),
    )
    .into()
}

/// Group a large integer with thousands separators, e.g. `12345` → `12,345`.
fn fmt_thousands(n: usize) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A stable, readable color for the language at rank `i` in the bar/table.
fn lang_color(i: usize) -> iced::Color {
    const PALETTE: [u32; 8] = [
        0x61afef, // blue
        0x98c379, // green
        0xe5c07b, // yellow
        0xe06c75, // red
        0xc678dd, // purple
        0x56b6c2, // cyan
        0xd19a66, // orange
        0x828b9c, // grey
    ];
    theme::rgb(PALETTE[i % PALETTE.len()])
}

/// A small filled square used as a color key next to a language row.
fn color_swatch(color: iced::Color) -> Element<'static, Message> {
    container(space())
        .width(10)
        .height(10)
        .style(move |_t| container::Style {
            background: Some(color.into()),
            border: iced::Border { radius: 2.0.into(), ..Default::default() },
            ..container::Style::default()
        })
        .into()
}

/// A GitHub-style proportion bar: one colored segment per language, its width
/// proportional to that language's code lines.
fn language_bar(report: &crate::stats::StatsReport) -> Element<'_, Message> {
    let max = report.langs.iter().map(|l| l.code).max().unwrap_or(0);
    if max == 0 {
        return space().height(12).into();
    }
    // Scale into `FillPortion`'s u16 range while preserving ratios exactly.
    let scale = 60000.0 / max as f64;
    let mut bar = Row::new();
    for (i, l) in report.langs.iter().enumerate() {
        let portion = ((l.code as f64) * scale).round().max(1.0) as u16;
        let color = lang_color(i);
        bar = bar.push(
            container(space())
                .width(Length::FillPortion(portion))
                .height(Fill)
                .style(move |_t| container::Style {
                    background: Some(color.into()),
                    ..container::Style::default()
                }),
        );
    }
    container(bar)
        .width(Fill)
        .height(12)
        .style(|_t| container::Style {
            background: Some(theme::BG_PANEL.into()),
            border: iced::Border { radius: 3.0.into(), ..Default::default() },
            ..container::Style::default()
        })
        .into()
}

/// One headline number in the summary strip (a big value over a muted label).
fn stat_cell(label: &str, value: usize) -> Element<'_, Message> {
    column![
        text(fmt_thousands(value)).size(22).color(theme::FG_BRIGHT),
        text(label.to_string()).size(11).color(theme::FG_MUTED),
    ]
    .spacing(2)
    .into()
}

/// The code-statistics "home": totals, a language-proportion bar, a per-language
/// breakdown, and the largest files (each row opens the file).
fn stats_home(app: &App) -> Element<'_, Message> {
    let refresh = button(text("Refresh").size(12))
        .style(theme::toolbar_button)
        .padding([3, 12])
        .on_press(Message::RefreshStats);

    // Nothing to show yet: computing, or a project with no counted code.
    let Some(report) = app.stats.as_ref().filter(|r| !r.is_empty()) else {
        let msg = if app.building_stats {
            "Computing code statistics…"
        } else {
            "No code files to count in this project."
        };
        return center(
            column![
                text("Code Statistics").size(18).color(theme::FG),
                text(msg).size(13).color(theme::DIM),
            ]
            .spacing(12)
            .align_x(iced::Center)
            .max_width(560),
        )
        .into();
    };

    // A recompute running over already-shown (stale) numbers.
    let updating: Element<'_, Message> = if app.building_stats {
        text("updating…").size(12).color(theme::DIM).into()
    } else {
        space().width(0).into()
    };
    let header = row![
        text("Code Statistics").size(18).color(theme::FG),
        space().width(Fill),
        updating,
        space().width(10),
        refresh,
    ]
    .align_y(iced::Center);

    let t = &report.totals;
    // "Code files" (tokei-counted source files), not the tree's total file count —
    // labelled explicitly so the two numbers don't read as a contradiction.
    let summary = row![
        stat_cell("Code files", t.files),
        stat_cell("Lines", t.lines()),
        stat_cell("Code", t.code),
        stat_cell("Comments", t.comments),
        stat_cell("Blanks", t.blanks),
    ]
    .spacing(36);

    // Per-language table: a color key, name, and counts, ranked by code lines.
    let total_code = report.totals.code.max(1);
    let cell = |s: String, w: f32, color: iced::Color| text(s).size(12).color(color).width(Length::Fixed(w));
    let head = |s: &'static str, w: f32| text(s).size(11).color(theme::FG_MUTED).width(Length::Fixed(w));
    let table_header = row![
        space().width(16),
        head("Language", 150.0),
        head("Files", 70.0),
        head("Code", 90.0),
        head("Comments", 90.0),
        head("Blanks", 80.0),
        head("Share", 70.0),
    ]
    .spacing(8)
    .align_y(iced::Center);
    let mut table = Column::new().spacing(6).push(table_header);
    for (i, l) in report.langs.iter().enumerate() {
        let share = l.code as f64 / total_code as f64 * 100.0;
        table = table.push(
            row![
                color_swatch(lang_color(i)),
                cell(l.name.clone(), 150.0, theme::FG),
                cell(fmt_thousands(l.files), 70.0, theme::FG_MUTED),
                cell(fmt_thousands(l.code), 90.0, theme::FG),
                cell(fmt_thousands(l.comments), 90.0, theme::FG_MUTED),
                cell(fmt_thousands(l.blanks), 80.0, theme::FG_MUTED),
                cell(format!("{share:.2}%"), 70.0, theme::DIM),
            ]
            .spacing(8)
            .align_y(iced::Center),
        );
    }

    // Largest files: click a row to open it.
    let root = app.project.as_ref().map(|p| p.root.clone());
    let mut files = Column::new().spacing(2);
    for f in &report.top_files {
        let inner = row![
            text(f.rel.to_string_lossy().into_owned())
                .size(12)
                .color(theme::FG)
                .width(Fill)
                .wrapping(Wrapping::None),
            text(fmt_thousands(f.lines)).size(12).color(theme::FG_MUTED).width(Length::Fixed(80.0)),
            text(f.lang.clone()).size(11).color(theme::DIM).width(Length::Fixed(90.0)),
        ]
        .spacing(8)
        .align_y(iced::Center);
        let mut b = button(inner)
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding { top: 2.0, right: 8.0, bottom: 2.0, left: 8.0 });
        if let Some(root) = &root {
            b = b.on_press(Message::OpenAbs { abs: root.join(&f.rel), line: None, push: true });
        }
        files = files.push(b);
    }

    let section = |title: &'static str| text(title).size(13).color(theme::FG_MUTED);
    let body = column![
        summary,
        space().height(4),
        language_bar(report),
        space().height(10),
        section("By language"),
        table,
        space().height(14),
        section("Largest files"),
        files,
    ]
    .spacing(8)
    .width(Fill)
    .max_width(860);

    container(
        column![
            header,
            scrollable(body)
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(Fill),
        ]
        .spacing(14),
    )
    .width(Fill)
    .height(Fill)
    .padding([20, 28])
    .into()
}

fn editor_shell(inner: Element<'_, Message>) -> Element<'_, Message> {
    container(inner)
        .width(Fill)
        .height(Fill)
        .style(theme::editor)
        .into()
}

fn pane_view(app: &App, pane: usize) -> Element<'_, Message> {
    // Time travel takes over the active pane entirely (its own read-only view).
    if pane == app.active
        && let Some(tt) = &app.time_travel
        && app.panes[pane].as_ref().is_some_and(|v| v.abs == tt.abs)
    {
        // Pass the live viewer as a fallback so the code stays visible while the
        // first historical revision loads (no blank "flash" on entry).
        return editor_shell(time_travel_view(app, tt, app.panes[pane].as_ref()));
    }
    let inner: Element<'_, Message> = match &app.panes[pane] {
        Some(v) => {
            // The diff view replaces the code of the active pane's file.
            if pane == app.active
                && let Some(d) = &app.diff
                && d.abs == v.abs
            {
                diff_view(app, d)
            } else {
                code_pane(app, pane, v)
            }
        }
        None => mouse_area(empty_state(
            Glyph::Note,
            "No file open",
            "Pick a file from the tree, or press ⌘P.",
            None,
        ))
        .on_press(Message::PaneFocused(pane))
        .into(),
    };

    // Find bar floats over the top-right of the active pane.
    let body: Element<'_, Message> = if app.find.open && pane == app.active {
        stack![editor_shell(inner), find_bar(app)].into()
    } else {
        editor_shell(inner)
    };

    let mut col = column![];
    if app.split {
        col = col.push(pane_header(app, pane));
    }
    col.push(body).width(Fill).height(Fill).into()
}

// ------------------------------------------------------------- time travel

/// The git time-travel view: a commit banner on top, the historical (read-only)
/// code in the middle, and a timeline scrubber at the bottom. `live` is the
/// pane's current viewer, shown until the first historical revision loads.
fn time_travel_view<'a>(
    app: &'a App,
    tt: &'a TimeTravel,
    live: Option<&'a Viewer>,
) -> Element<'a, Message> {
    let commit = tt.commits.get(tt.idx);
    // The historical viewer once ready; otherwise the live one so the code area
    // never goes blank on entry.
    let code: Element<'a, Message> = match tt.viewer.as_ref().or(live) {
        Some(hv) => time_travel_code(app, tt, hv),
        None => center(text("Loading revision…").size(13).color(theme::DIM)).into(),
    };
    let mut col = Column::new().push(time_travel_banner(tt, commit));
    if let Some(story) = &tt.story {
        col = col.push(time_travel_story(app, tt, story));
    }
    col.push(container(code).width(Fill).height(Fill))
        .push(time_travel_bar(tt))
        .width(Fill)
        .height(Fill)
        .into()
}

/// The commit banner: sha · author · when, the subject, and the AI "what & why".
fn time_travel_banner<'a>(
    tt: &'a TimeTravel,
    commit: Option<&'a crate::git::HistCommit>,
) -> Element<'a, Message> {
    // A tidy "Exit  esc" — the little keycap reads as a control and teaches the
    // shortcut, instead of a bare ✕ glyph.
    let keycap = container(text("esc").size(9).color(theme::FG_MUTED))
        .padding(Padding { top: 1.0, right: 5.0, bottom: 1.0, left: 5.0 })
        .style(|_: &iced::Theme| iced::widget::container::Style {
            background: Some(theme::BG_ACTIVE.into()),
            border: iced::Border { radius: 3.0.into(), width: 1.0, color: theme::HAIRLINE },
            ..Default::default()
        });
    let exit = button(
        row![text("Exit").size(11).color(theme::FG_MUTED), keycap]
            .spacing(6)
            .align_y(iced::Center),
    )
    .style(theme::toolbar_button)
    .padding([2, 8])
    .on_press(Message::TimeTravelExit);

    let Some(c) = commit else {
        let head = row![
            glyph::icon(Glyph::TimeTravel, theme::ACCENT, 15.0),
            text("Time travel").size(12).color(theme::FG),
            space().width(Fill),
            exit,
        ]
        .spacing(8)
        .align_y(iced::Center);
        return container(head).padding([7, 12]).width(Fill).style(theme::pane_header).into();
    };

    let short: String = c.sha.chars().take(8).collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let head = row![
        glyph::icon(Glyph::TimeTravel, theme::ACCENT, 15.0),
        text(short).size(12).color(theme::ACCENT).font(Font::MONOSPACE),
        text(format!("{}  ·  {}", c.author, crate::git::relative_time(c.time, now)))
            .size(11)
            .color(theme::DIM),
        space().width(Fill),
        exit,
    ]
    .spacing(10)
    .align_y(iced::Center);

    let subject = text(c.subject.clone()).size(12).color(theme::FG).wrapping(Wrapping::Word);

    let why: Element<'a, Message> = if tt.why_loading {
        text("Summarizing…").size(11).color(theme::DIM).into()
    } else if let Some(w) = tt.why.get(&c.sha) {
        text(w.clone()).size(11).color(theme::FG_MUTED).wrapping(Wrapping::Word).into()
    } else {
        button(text("What & why?").size(11).color(theme::ACCENT))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(Message::TimeTravelWhy)
            .into()
    };

    container(column![head, subject, why].spacing(5))
        .padding([7, 12])
        .width(Fill)
        .style(theme::pane_header)
        .into()
}

/// The historical code — a read-only code view of the file at this revision,
/// with the commit's added/changed lines marked in the gutter.
fn time_travel_code<'a>(app: &'a App, tt: &'a TimeTravel, hv: &'a Viewer) -> Element<'a, Message> {
    let lh = app.line_height();
    let row0 = (tt.scroll_y / lh) as usize;
    let sticky = crate::analyze::sticky_headers(&hv.folds, hv.line_at_row(row0), 5);
    // Read-only, but clicking still places a caret and dragging selects (for
    // reading / copying). Right-click has no menu in a historical view.
    let code = CodeView::new(
        &hv.lines,
        hv.max_cols,
        app.font_size,
        lh,
        theme::FG,
        |(line, col)| Message::TimeTravelSelectStart { line, col },
        |(line, col)| Message::TimeTravelSelectDrag { line, col },
        |_, _| Message::Noop,
    )
    .cursor(hv.caret)
    .selection(hv.selection_ordered())
    .sticky(sticky)
    .folds(hv.visible_rows(), &hv.fold_header_set, &hv.collapsed)
    .indent_guides(true)
    .git_gutter(hv.git.as_deref());
    // Reuse the live pane's scroll id so iced keeps the scroll position across
    // the enter/exit swap (the historical file is ~the same length), instead of
    // remounting a fresh scrollable at the top.
    scrollable(code)
        .id(code_scroll_id(app.active))
        .on_scroll(Message::TimeTravelScrolled)
        .direction(Direction::Both {
            vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
            horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
        })
        .style(theme::overlay_scrollbar)
        .width(Fill)
        .height(Fill)
        .into()
}

/// The timeline scrubber: older/newer steps, a slider, position, scope toggle,
/// and (for a function scope) the "story of this function" narrative button.
fn time_travel_bar(tt: &TimeTravel) -> Element<'_, Message> {
    let n = tt.commits.len();
    let last = n.saturating_sub(1);
    // `then` (lazy) — not `then_some` — so `idx - 1` isn't evaluated (underflowing
    // usize) when idx is 0.
    let older = (tt.idx < last).then(|| Message::TimeTravelGoto(tt.idx + 1));
    let newer = (tt.idx > 0).then(|| Message::TimeTravelGoto(tt.idx - 1));
    let step = |g: Glyph, msg: Option<Message>| {
        let on = msg.is_some();
        let mut b = button(glyph::icon(g, if on { theme::FG } else { theme::DIM }, 15.0))
            .style(theme::toolbar_button)
            .padding([2, 8]);
        if let Some(m) = msg {
            b = b.on_press(m);
        }
        b
    };
    // Slider: left = oldest, right = newest; position = last - idx.
    let sl = slider(0.0..=last.max(1) as f32, (last - tt.idx) as f32, move |v| {
        let p = (v.round() as usize).min(last);
        Message::TimeTravelGoto(last - p)
    })
    .step(1.0)
    .width(Fill);

    let scope_label = match &tt.scope {
        TimeScope::Symbol { name, kind, .. } => format!("{} {name}", short_kind(kind)),
        TimeScope::File => "whole file".to_string(),
    };
    // Clicking toggles between the whole file and the block under the caret.
    let scope_btn = button(text(format!("scope: {scope_label}  ⇄")).size(11).color(theme::FG_MUTED))
        .style(theme::toolbar_button)
        .padding([2, 8])
        .on_press(Message::TimeTravelToggleScope);

    let story: Element<'_, Message> = if matches!(tt.scope, TimeScope::Symbol { .. }) {
        if tt.story_loading {
            text("Story…").size(11).color(theme::DIM).into()
        } else {
            let label = if tt.story.is_some() { "Hide story" } else { "Story" };
            let color = if tt.story.is_some() { theme::FG_MUTED } else { theme::ACCENT };
            button(text(label).size(11).color(color))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::TimeTravelStory)
                .into()
        }
    } else {
        space().into()
    };

    container(
        row![
            chrome_tip(step(Glyph::ArrowLeft, older), "Older commit", Some("⌘←".to_string())),
            sl,
            chrome_tip(step(Glyph::ArrowRight, newer), "Newer commit", Some("⌘→".to_string())),
            text(format!("{} / {}", tt.idx + 1, n)).size(11).color(theme::DIM),
            space().width(16),
            scope_btn,
            story,
        ]
        .spacing(10)
        .align_y(iced::Center),
    )
    .padding([5, 12])
    .width(Fill)
    .style(theme::statusbar)
    .into()
}

/// The "story of this function" narrative panel (AI), shown above the code.
fn time_travel_story<'a>(
    app: &'a App,
    tt: &'a TimeTravel,
    story: &'a [crate::PreparedSeg],
) -> Element<'a, Message> {
    let name = tt.scope.symbol_name().unwrap_or("this block");
    let header = row![
        column![
            text(format!("Story of {name}")).size(12).color(theme::ACCENT),
            // `git log -L` only follows the block's CURRENT lines, so earlier
            // rewrites may not be attributed — say so, so it's not read as a full
            // biography.
            text("from the commits that touched these lines")
                .size(9)
                .color(theme::DIM),
        ]
        .spacing(1),
        space().width(Fill),
        button(text("✕").size(11).color(theme::DIM))
            .style(theme::toolbar_button)
            .padding([1, 6])
            .on_press(Message::TimeTravelStory),
    ]
    .align_y(iced::Center);
    let body = scrollable(Column::with_children(render_prepared(app, story)).spacing(8).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Length::Fixed(200.0));
    container(column![header, body].spacing(6))
        .padding([8, 12])
        .width(Fill)
        .style(theme::modal_panel)
        .into()
}

/// The unified diff of the active file versus `HEAD`, colored by line kind.
fn diff_view<'a>(app: &'a App, d: &'a crate::DiffState) -> Element<'a, Message> {
    use crate::git::DiffKind;

    let header = container(
        row![
            text(format!("{}  ·  vs HEAD", d.rel))
                .size(12)
                .color(theme::ACCENT),
            space().width(Fill),
            button(text("✕ close").size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::ToggleDiff),
        ]
        .align_y(iced::Center),
    )
    .padding(Padding {
        top: 5.0,
        right: 8.0,
        bottom: 5.0,
        left: 10.0,
    })
    .style(theme::pane_header)
    .width(Fill);

    if d.lines.is_empty() {
        return column![
            header,
            center(
                text(format!("No uncommitted changes in {}", d.rel))
                    .size(13)
                    .color(theme::DIM),
            )
        ]
        .width(Fill)
        .height(Fill)
        .into();
    }

    const MAX_DIFF_ROWS: usize = 8000;
    // Size every row to the longest line so the color tints span the full
    // content width and long lines become reachable via horizontal scroll.
    let max_cols = d
        .lines
        .iter()
        .take(MAX_DIFF_ROWS)
        .map(|l| l.text.chars().count())
        .max()
        .unwrap_or(0);
    let row_width = (max_cols as f32 + 1.0) * app.font_size * 0.6 + 18.0;
    let mut rows: Vec<Element<'a, Message>> = Vec::new();
    for dl in d.lines.iter().take(MAX_DIFF_ROWS) {
        let (bg, fg) = match dl.kind {
            DiffKind::Add => (Some(theme::with_alpha(theme::rgb(0x98c379), 0.14)), theme::rgb(0x98c379)),
            DiffKind::Remove => (Some(theme::with_alpha(theme::rgb(0xe06c75), 0.14)), theme::rgb(0xe06c75)),
            DiffKind::Hunk => (Some(theme::with_alpha(theme::ACCENT, 0.12)), theme::ACCENT),
            DiffKind::Header => (None, theme::DIM),
            DiffKind::Context => (None, theme::FG),
        };
        // A space keeps empty lines from collapsing to zero height.
        let content = if dl.text.is_empty() { " " } else { dl.text.as_str() };
        let mut cell = container(
            text(content)
                .font(Font::MONOSPACE)
                .size(app.font_size)
                .color(fg)
                .wrapping(Wrapping::None),
        )
        .width(Length::Fixed(row_width))
        .padding(Padding {
            top: 0.0,
            right: 8.0,
            bottom: 0.0,
            left: 10.0,
        });
        if let Some(bg) = bg {
            cell = cell.style(move |_: &iced::Theme| container::Style {
                background: Some(bg.into()),
                ..container::Style::default()
            });
        }
        rows.push(cell.into());
    }
    if d.lines.len() > MAX_DIFF_ROWS {
        rows.push(
            text(format!("… {} more lines", d.lines.len() - MAX_DIFF_ROWS))
                .size(11)
                .color(theme::DIM)
                .into(),
        );
    }

    let body = scrollable(Column::with_children(rows).padding([4, 0]))
        .direction(Direction::Both {
            vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
            horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
        })
        .style(theme::overlay_scrollbar)
        .width(Fill)
        .height(Fill);

    column![header, body].width(Fill).height(Fill).into()
}

fn find_bar(app: &App) -> Element<'_, Message> {
    let count = if app.find.query.is_empty() {
        String::new()
    } else if app.find.matches.is_empty() {
        "0/0".to_string()
    } else {
        format!("{}/{}", app.find.current + 1, app.find.matches.len())
    };

    let input = text_input("Find in file…", &app.find.query)
        .id(find_input_id())
        .on_input(Message::FindQueryChanged)
        .on_submit(Message::FindStep(1))
        .size(13)
        .padding([4, 8])
        .width(190);

    let btn = |label: &'static str, msg: Message| {
        button(text(label).size(13))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(msg)
    };

    let bar = container(
        row![
            input,
            text(count).size(11).color(theme::DIM).width(46),
            btn("‹", Message::FindStep(-1)),
            btn("›", Message::FindStep(1)),
            btn("✕", Message::FindClosed),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .padding(6)
    .style(theme::modal_panel);

    // Pin to the top-right of the pane.
    container(bar)
        .width(Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .padding(Padding {
            top: 6.0,
            right: 16.0,
            bottom: 0.0,
            left: 0.0,
        })
        .into()
}

fn pane_header(app: &App, pane: usize) -> Element<'_, Message> {
    let active = app.active == pane;
    let title = app.panes[pane]
        .as_ref()
        .map(|v| v.rel.as_str())
        .unwrap_or("—");
    mouse_area(
        container(
            text(title)
                .size(11)
                .color(if active { theme::ACCENT } else { theme::DIM })
                .wrapping(Wrapping::None),
        )
        .width(Fill)
        .padding([3, 8])
        .style(theme::pane_header),
    )
    .on_press(Message::PaneFocused(pane))
    .into()
}

fn welcome(app: &App) -> Element<'_, Message> {
    // On a live remote the hero invites browsing that host; otherwise it offers
    // opening local code or connecting out over SSH.
    let (subtitle, primary, secondary): (String, (&str, Message), (&str, Message)) =
        if app.connection.is_remote() {
            (
                format!("connected to {}", app.connection.label()),
                ("Browse folders…", Message::OpenConnect),
                ("Disconnect", Message::ConnectDisconnect),
            )
        } else {
            (
                // "clew" = the thread that guides you out of the labyrinth.
                "Find the thread through your codebase".to_string(),
                ("Open Folder…", Message::OpenFolderPressed),
                ("Open Remote…", Message::OpenConnect),
            )
        };

    let actions = row![
        button(text(primary.0.to_string()).size(14))
            .style(theme::primary_button)
            .padding([8, 20])
            .on_press(primary.1),
        button(text(secondary.0.to_string()).size(14))
            .style(theme::secondary_button)
            .padding([8, 20])
            .on_press(secondary.1),
    ]
    .spacing(10);

    // Brand lockup: the "C" mark on the left, the name on the right — same mark
    // as the app icon (minus the square), so the welcome reads as part of the app.
    let mark = iced::widget::svg(iced::widget::svg::Handle::from_memory(MARK_SVG))
        .width(Length::Fixed(52.0))
        .height(Length::Fixed(52.0));
    let brand = row![mark, text("Clew").size(34).color(theme::FG_BRIGHT)]
        .spacing(14)
        .align_y(iced::Center);

    center(
        column![
            brand,
            space().height(4),
            text(subtitle).size(13).color(theme::FG_MUTED),
            space().height(22),
            actions,
        ]
        .spacing(6)
        .align_x(iced::Center),
    )
    .into()
}

/// The Clew "C" mark (white arcs, transparent background) for the welcome screen.
const MARK_SVG: &[u8] = include_bytes!("../assets/icon/mark.svg");

fn code_pane<'a>(app: &'a App, pane: usize, v: &'a Viewer) -> Element<'a, Message> {
    // Bookmarked lines of this file, for the gutter marker.
    let marked: std::collections::HashSet<usize> = app
        .bookmarks
        .iter()
        .filter(|b| b.rel == v.rel)
        .map(|b| b.line)
        .collect();

    // Inline summaries: each function's one-line explanation, shown past its
    // signature so you read the code together with what it does.
    let summaries: std::collections::HashMap<usize, String> = if app.show_inline_summaries {
        app.symbol_index_by_file
            .get(&v.abs)
            .map(|syms| {
                syms.iter()
                    .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                    .filter_map(|s| {
                        let node =
                            crate::explain::Node::Function { file: v.abs.clone(), name: s.name.clone() };
                        app.explanations
                            .get(&node)
                            .filter(|c| !crate::explain::is_error_summary(&c.summary))
                            .map(|c| (s.line, first_sentence(&c.summary)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };

    // Debug: this file's breakpoints (and which are conditional), plus the
    // current stopped line (if here).
    let file_bps = app.breakpoints.get(&v.abs);
    let breakpoints: std::collections::HashSet<usize> =
        file_bps.map(|m| m.keys().copied().collect()).unwrap_or_default();
    let cond_breakpoints: std::collections::HashSet<usize> = file_bps
        .map(|m| m.iter().filter(|(_, bp)| bp.condition.is_some()).map(|(l, _)| *l).collect())
        .unwrap_or_default();
    let debug_current = app
        .debug
        .as_ref()
        .and_then(|d| d.current.as_ref())
        .filter(|(p, _)| *p == v.abs)
        .map(|(_, line)| *line);

    // The block cursor shows only on the active pane while the code view has
    // keyboard focus.
    let cursor = if pane == app.active && app.code_focused {
        v.caret
    } else {
        None
    };

    let mut code = CodeView::new(
        &v.lines,
        v.max_cols,
        app.font_size,
        app.line_height(),
        theme::FG,
        move |(line, col)| Message::SelectStart { pane, line, col },
        move |(line, col)| Message::SelectDrag { pane, line, col },
        move |(line, col), at| Message::ContextMenuOpened {
            pane,
            line,
            col,
            x: at.x,
            y: at.y,
        },
    )
    .selection(v.selection_ordered())
    .cursor(cursor)
    .highlights(app.code_highlights(pane, v))
    .sticky(app.sticky_headers(v))
    .bookmarks(marked)
    .breakpoints(breakpoints)
    .cond_breakpoints(cond_breakpoints)
    .debug_current(debug_current)
    .summaries(summaries)
    .inlay_hints(v.inlay_hints.clone(), theme::DIM)
    .inactive(v.inactive_lines.clone())
    .folds(v.visible_rows(), &v.fold_header_set, &v.collapsed)
    .on_fold(move |line| Message::FoldToggle { pane, line })
    .on_breakpoint(move |line| Message::BreakpointToggle { path: v.abs.clone(), line })
    .indent_guides(true)
    .git_gutter(v.git.as_deref())
    .blame(if pane == app.active && app.code_focused {
        app.blame_annotation(v)
    } else {
        None
    })
    .on_hover(move |(line, col), at| Message::HoverRequested {
        pane,
        line,
        col,
        x: at.x,
        y: at.y,
    })
    .on_hover_end(|| Message::HoverCleared);
    // The minimap is opt-in (toggle in the "More" menu); without the callback
    // the widget draws no minimap band at all.
    if app.show_minimap {
        code = code.on_minimap(move |fraction| Message::MinimapScrolled { pane, fraction });
    }

    let scroller = scrollable(code)
        .id(code_scroll_id(pane))
        .on_scroll(move |viewport| Message::CodeScrolled(pane, viewport))
        .direction(Direction::Both {
            vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
            horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
        })
        .style(theme::overlay_scrollbar)
        .width(Fill)
        .height(Fill);

    // File TL;DR banner: a one-line "what is this file" from the explain cache,
    // pinned above the code. Dismissable (toggle back via the More menu).
    let banner: Option<Element<'_, Message>> = if app.show_file_banner {
        app.explanations
            .get(&crate::explain::Node::File(v.abs.clone()))
            .filter(|c| !crate::explain::is_error_summary(&c.summary))
            .map(|c| file_banner(first_sentence(&c.summary)))
    } else {
        None
    };
    match banner {
        Some(b) => column![b, scroller].into(),
        None => scroller.into(),
    }
}

/// A one-line file summary pinned at the top of the code view.
fn file_banner<'a>(summary: String) -> Element<'a, Message> {
    container(
        row![
            text("›").size(12).color(theme::ACCENT),
            text(summary).size(12).color(theme::FG_MUTED).width(Fill),
            button(text("✕").size(11).color(theme::DIM))
                .style(theme::toolbar_button)
                .padding([0, 6])
                .on_press(Message::ToggleFileBanner),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .width(Fill)
    .padding([4, 10])
    .style(theme::panel)
    .into()
}

// ---------------------------------------------------------------- outline

/// The right sidebar: a tabbed panel with an Outline tab and an Explain tab
/// (mirrors the left sidebar's tabs). Hidden on narrow/split windows or with no
/// file open.
fn right_panel(app: &App) -> Option<Element<'_, Message>> {
    if !app.show_right_panel || app.split || app.window_width < 950.0 {
        return None;
    }
    app.active_viewer()?; // a file must be open

    // One cursor-following reading-context panel — no tab-dance. The top follows
    // the caret: the current function's summary, call-flow and quick actions.
    // The bottom is the file's outline, an annotated table of contents with the
    // current symbol highlighted, so "where am I" and "what's around me" sit
    // together.
    // Equal split: the explanation (which can stream several blocks) gets as much
    // room as the outline, rather than being squeezed into the smaller share.
    let context = container(explain_content(app)).height(iced::Length::FillPortion(1));
    let outline = column![section_header("OUTLINE"), outline_content(app)]
        .height(iced::Length::FillPortion(1));

    Some(
        container(column![context, hairline(), outline])
            .width(Length::Fixed(app.right_width))
            .height(Fill)
            .style(theme::panel)
            .into(),
    )
}

/// A 1px horizontal divider spanning the panel width.
fn hairline() -> Element<'static, Message> {
    container(space().width(Fill).height(1))
        .width(Fill)
        .height(1)
        .style(|_: &iced::Theme| iced::widget::container::Style {
            background: Some(theme::HAIRLINE.into()),
            ..Default::default()
        })
        .into()
}

/// The Outline tab's content: the active file's symbols, click to jump.
fn outline_content(app: &App) -> Element<'_, Message> {
    let Some(v) = app.active_viewer() else {
        return space().into();
    };
    if v.symbols.is_empty() {
        return container(text("No symbols in this file.").size(11).color(theme::DIM))
            .padding(10)
            .into();
    }
    // The symbol the reading cursor is currently inside, to highlight its row.
    let current = match &app.explain_view {
        Some(crate::explain::Node::Function { file, name }) if *file == v.abs => {
            Some(name.as_str())
        }
        _ => None,
    };
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for symbol in &v.symbols {
        let is_current = matches!(symbol.kind.as_str(), "function" | "method")
            && current == Some(symbol.name.as_str());
        // The reader's note/progress on this symbol (anchored by name, so it
        // follows the symbol across edits/re-scans).
        let note = crate::notes::find(&app.notes, &v.rel, &symbol.name);
        let understood = note.is_some_and(|n| n.understood);
        let has_text = note.is_some_and(|n| !n.text.is_empty());

        let label = row![
            text(short_kind(&symbol.kind))
                .size(10)
                .color(kind_color(&symbol.kind))
                .width(40),
            // Understood symbols dim, so the outline shows at a glance what's left.
            text(&symbol.name)
                .size(12)
                .color(if understood { theme::DIM } else { theme::FG })
                .wrapping(Wrapping::None),
        ]
        .spacing(4)
        .align_y(iced::Center);

        // Annotate each function/method with its one-line explanation, turning
        // the outline into a table of contents that says what each symbol does.
        // Same toggle and error-filter as the inline code summaries.
        let summary = if app.show_inline_summaries
            && matches!(symbol.kind.as_str(), "function" | "method")
        {
            let node = crate::explain::Node::Function { file: v.abs.clone(), name: symbol.name.clone() };
            app.explanations
                .get(&node)
                .filter(|c| !crate::explain::is_error_summary(&c.summary))
                .map(|c| c.summary.trim().to_string())
        } else {
            None
        };

        let mut col = Column::new().spacing(1).push(label);
        if let Some(full) = summary {
            // A one-line table-of-contents entry: the first sentence, truncated
            // with an ellipsis and clipped so it never wraps or overflows the
            // panel. The complete explanation shows in a bubble on hover.
            let clean = strip_backticks(&full);
            let one_line = truncate_ellipsis(&first_sentence(&clean), 52);
            let line = container(
                text(one_line).size(10).color(theme::DIM).wrapping(Wrapping::None),
            )
            .clip(true)
            .width(Fill)
            .padding(Padding { top: 0.0, right: 6.0, bottom: 0.0, left: 44.0 });
            let bubble = container(text(clean).size(11).color(theme::FG))
                .padding(Padding { top: 6.0, right: 9.0, bottom: 6.0, left: 9.0 })
                .max_width(320)
                .style(theme::modal_panel);
            col = col.push(tooltip(line, bubble, tooltip::Position::Bottom).gap(4));
        }
        if let Some(n) = note.filter(|n| !n.text.is_empty()) {
            // The reader's own note, in accent so it's distinct from the summary.
            col = col.push(
                container(
                    text(format!("\u{270e} {}", n.text)).size(10).color(theme::ACCENT).wrapping(Wrapping::Word),
                )
                .padding(Padding { top: 0.0, right: 4.0, bottom: 0.0, left: 44.0 }),
            );
        }

        let jump = button(col)
            .style(theme::list_row(is_current))
            .width(Fill)
            .padding(Padding { top: 4.0, right: 4.0, bottom: 4.0, left: 4.0 })
            .on_press(Message::OutlineJump(symbol.line));
        // Leading "understood" toggle and trailing note pencil sit outside the
        // jump button so each captures its own click.
        let (cg, gcolor) =
            if understood { (Glyph::CheckCircle, theme::ACCENT) } else { (Glyph::Circle, theme::DIM) };
        let toggle = button(glyph::icon(cg, gcolor, 13.0))
            .style(theme::list_row(false))
            .padding([5, 5])
            .on_press(Message::NoteToggleUnderstood { rel: v.rel.clone(), symbol: symbol.name.clone() });
        let pencil = button(glyph::icon(Glyph::Edit, if has_text { theme::ACCENT } else { theme::DIM }, 12.0))
            .style(theme::list_row(false))
            .padding([5, 5])
            .on_press(Message::NoteEditStart { rel: v.rel.clone(), symbol: symbol.name.clone() });
        // Top-align so the toggle circle and pencil sit on the kind-badge/name
        // line rather than floating in the middle of the multi-line row.
        rows.push(
            row![toggle, jump, pencil]
                .spacing(1)
                .align_y(iced::alignment::Vertical::Top)
                .into(),
        );
    }

    // Sub-label under "OUTLINE": the reader's manual "understood" coverage for
    // this file. Plain text, left-aligned with the section header — no decorative
    // leading circle (that read as a stray, non-clickable control). Explain-All
    // progress lives only in the status bar, so it isn't duplicated here.
    let names: Vec<String> = v.symbols.iter().map(|s| s.name.clone()).collect();
    let (done, total) = crate::notes::coverage(&app.notes, &v.rel, &names);
    let header_content: Element<'_, Message> =
        text(format!("{done}/{total} understood")).size(11).color(theme::FG_MUTED).into();
    let header = container(header_content)
        .padding(Padding { top: 2.0, right: 10.0, bottom: 4.0, left: 10.0 });

    // The wrapping column must be Fill so the scrollable has a bounded height to
    // scroll within — otherwise it grows to its content and never scrolls (which
    // made long outlines like main.rs's 224 symbols un-navigable).
    column![
        header,
        scrollable(Column::with_children(rows).width(Fill))
            .id(outline_scroll_id())
            .direction(Direction::Vertical(Scrollbar::new().width(6.0).scroller_width(6.0)))
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .height(Fill)
    .into()
}

fn short_kind(kind: &str) -> &'static str {
    match kind {
        "function" => "fn",
        "method" => "meth",
        "class" => "class",
        "struct" => "struct",
        "enum" => "enum",
        "union" => "union",
        "trait" => "trait",
        "interface" => "iface",
        "implementation" => "impl",
        "module" => "mod",
        "macro" => "macro",
        "constant" => "const",
        "type" => "type",
        _ => "sym",
    }
}

fn kind_color(kind: &str) -> iced::Color {
    match kind {
        "function" | "method" | "macro" => theme::rgb(0x61afef),
        "class" | "struct" | "enum" | "union" | "trait" | "interface" | "type" => {
            theme::rgb(0xe5c07b)
        }
        "module" | "implementation" => theme::rgb(0xc678dd),
        "constant" => theme::rgb(0xd19a66),
        _ => theme::DIM,
    }
}

// ---------------------------------------------------------------- status bar

fn statusbar(app: &App) -> Element<'_, Message> {
    // In time travel, report the revision being viewed — not the live document's
    // stats (its line count / a caret line that may not exist in this revision).
    let right = if let Some(tt) = &app.time_travel {
        let short: String =
            tt.commits.get(tt.idx).map(|c| c.sha.chars().take(8).collect()).unwrap_or_default();
        let scope = match &tt.scope {
            TimeScope::Symbol { name, kind, .. } => format!("  ·  {} {name}", short_kind(kind)),
            TimeScope::File => String::new(),
        };
        let lines = tt
            .viewer
            .as_ref()
            .map(|v| format!("  ·  {} lines", v.lines.len()))
            .unwrap_or_default();
        format!("Time travel  ·  {short}  ·  {}/{}{}{}", tt.idx + 1, tt.commits.len(), scope, lines)
    } else {
        match app.active_viewer() {
        Some(v) => {
            let lang = v
                .lang_key
                .and_then(crate::highlight::lang_name)
                .unwrap_or("Plain text");
            // 1-based line/column of the last click, when there is one.
            let pos = v
                .caret
                .map(|(l, c)| format!("Ln {}, Col {}  ·  ", l + 1, c + 1))
                .unwrap_or_default();
            // Language-server status for this file's language, when relevant.
            let lsp = v
                .lang_key
                .and_then(|k| app.lsp.get(k))
                .map(|slot| format!("  ·  LSP {}", slot.label()))
                .unwrap_or_default();
            // Diagnostic counts for this file.
            let diags = v
                .lang_key
                .and_then(|k| match app.lsp.get(k) {
                    Some(crate::LspSlot::Ready(c)) => Some(c.diagnostics(&v.abs)),
                    _ => None,
                })
                .map(|ds| {
                    let errs = ds.iter().filter(|d| d.severity == 1).count();
                    let warns = ds.iter().filter(|d| d.severity == 2).count();
                    if errs + warns == 0 {
                        String::new()
                    } else {
                        format!("  ·  ✘ {errs}  ⚠ {warns}")
                    }
                })
                .unwrap_or_default();
            format!("{}{}  ·  {} lines{}{}", pos, lang, v.lines.len(), diags, lsp)
        }
        None => String::new(),
        }
    };

    // Where code is read from — the one place local vs remote shows. Click it to
    // manage connections / switch hosts.
    let (conn_glyph, conn_color) = if app.connection.is_remote() {
        (Glyph::Remote, theme::ACCENT)
    } else {
        (Glyph::Circle, theme::DIM)
    };
    let conn_indicator = tooltip(
        button(
            row![
                glyph::icon(conn_glyph, conn_color, 12.0),
                text(app.connection.label()).size(11).color(if app.connection.is_remote() {
                    theme::ACCENT
                } else {
                    theme::FG_MUTED
                }),
            ]
            .spacing(4)
            .align_y(iced::Center),
        )
        .style(theme::toolbar_button)
        .padding([2, 8])
        .on_press(Message::OpenConnect),
        container(text("Connect to a remote host").size(11).color(theme::FG))
            .padding([3, 7])
            .style(theme::modal_panel),
        tooltip::Position::Top,
    );

    let mut bar = row![conn_indicator, text(&app.status).size(11)]
        .spacing(12)
        .align_y(iced::Center);
    // A prominent, always-visible progress chip while "Explain All" runs — the
    // pass is slow, so show how far along it is (the status text alone is easy to
    // miss / read as stuck).
    if app.explaining {
        let label = match app.explain_progress {
            Some((done, total)) if total > 0 => format!("Explaining {done}/{total}"),
            _ => "Explaining…".to_string(),
        };
        let mut chip = row![
            glyph::icon(Glyph::Sparkle, theme::ACCENT, 11.0),
            text(label).size(11).color(theme::ACCENT),
        ]
        .spacing(5)
        .align_y(iced::Center);
        // A short determinate bar once the total is known, so progress reads at a
        // glance instead of by parsing the counter.
        if let Some((done, total)) = app.explain_progress
            && total > 0
        {
            chip = chip.push(
                progress_bar(0.0..=total as f32, done as f32)
                    .length(90.0)
                    .girth(4.0)
                    .style(theme::progress),
            );
        }
        // Failures never hide behind the counter: a running tally in warn red.
        if app.explain_failed > 0 {
            chip = chip.push(
                text(format!("· {} failed", app.explain_failed)).size(11).color(theme::WARN),
            );
        }
        bar = bar.push(chip);
    }
    bar = bar.push(space().width(Fill));
    if let Some(chip) = refresh_chip(app) {
        bar = bar.push(chip);
    }
    bar = bar.push(text(right).size(11));
    // For Rust files, a small target control that drives the `#[cfg]` dimming
    // (read another platform's branches as the live ones). A plain button + our
    // own dropdown, so the label and chevron sit tight together — placed last so
    // the popup anchored to the bottom-right lines up under it.
    if app.active_viewer().and_then(|v| v.lang_key) == Some("rust") {
        let picker = button(
            row![
                text(app.reading_target.to_string()).size(11).color(theme::FG_MUTED),
                glyph::icon(Glyph::ChevronDown, theme::DIM, 12.0),
            ]
            .spacing(4)
            .align_y(iced::Center),
        )
        .style(theme::toolbar_button)
        .padding([1, 6])
        .on_press(Message::ToggleTargetMenu);
        bar = bar.push(picker);
    }

    container(bar.padding([3, 10]))
        .width(Fill)
        .style(theme::statusbar)
        .into()
}

/// A freshness indicator for the auto-refreshed understanding: shows whether a
/// refresh is running / queued, and force-refreshes on click (bypassing the 30s
/// auto cooldown). Hidden until there's something to keep fresh (an explanation
/// set exists) and an LLM key is configured.
fn refresh_chip(app: &App) -> Option<Element<'_, Message>> {
    if !app.llm_available || app.explanations.is_empty() {
        return None;
    }
    // (label, colour, clickable). A running pass is shown but not clickable.
    let (label, color, enabled) = if app.explaining {
        let l = match app.explain_progress {
            Some((done, total)) if total > 0 => format!("↻ Refreshing {done}/{total}…"),
            _ => "↻ Refreshing…".to_string(),
        };
        (l, theme::ACCENT, false)
    } else if app.generating_overview {
        ("↻ Refreshing overview…".to_string(), theme::ACCENT, false)
    } else if app.building_embeddings {
        ("↻ Refreshing index…".to_string(), theme::ACCENT, false)
    } else if app.refresh_pending {
        // Seconds left before the auto pass fires (click to skip the wait).
        let secs = app
            .last_auto_refresh
            .map(|t| crate::AUTO_REFRESH_MIN_INTERVAL.saturating_sub(t.elapsed()).as_secs() + 1)
            .unwrap_or(0);
        (format!("↻ Update queued · {secs}s"), theme::ACCENT, true)
    } else {
        ("↻ Up to date".to_string(), theme::DIM, true)
    };
    let mut b = button(text(label).size(11).color(color))
        .style(theme::toolbar_button)
        .padding([1, 8]);
    if enabled {
        b = b.on_press(Message::RefreshAll);
    }
    Some(b.into())
}

// ------------------------------------------------------ breakpoint condition

/// Modal to set a breakpoint's condition — the program only stops there when the
/// expression is true. Empty condition sets a plain breakpoint.
fn bp_condition_modal<'a>(
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
        .padding(Padding { top: 120.0, right: 0.0, bottom: 0.0, left: 0.0 })
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::BpConditionCancel))
}

/// Modal to attach an optional plain-text note to a bookmark.
fn bookmark_note_modal<'a>(
    _app: &'a App,
    edit: &'a (String, usize, String),
) -> Element<'a, Message> {
    let (rel, line, draft) = edit;
    let panel = container(
        column![
            text(format!("Note for {rel}:{line}")).size(14).color(theme::FG),
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
        .padding(Padding { top: 120.0, right: 0.0, bottom: 0.0, left: 0.0 })
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::BookmarkNoteCancel))
}

/// Modal to attach an optional plain-text reading note to a symbol.
fn reading_note_modal(edit: &(String, String, String)) -> Element<'_, Message> {
    let (rel, symbol, draft) = edit;
    let panel = container(
        column![
            text(format!("Note on {symbol}  ·  {rel}")).size(14).color(theme::FG),
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
        .padding(Padding { top: 120.0, right: 0.0, bottom: 0.0, left: 0.0 })
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::NoteEditCancel))
}

/// The "Why is this here?" popup: the git-grounded explanation of why a line or
/// selection exists, with the commit(s) it cites. Async — "Thinking…" until the
/// answer lands.
fn why_modal<'a>(app: &'a App, bw: &'a crate::BlameWhy) -> Element<'a, Message> {
    let mut col = Column::new().spacing(8).push(text(bw.title.clone()).size(14).color(theme::FG));
    // The commits it's grounded in.
    for (sha, subject) in &bw.commits {
        let subject: String = if subject.chars().count() > 52 {
            subject.chars().take(51).collect::<String>() + "…"
        } else {
            subject.clone()
        };
        col = col.push(
            row![
                text(sha.clone()).size(11).font(Font::MONOSPACE).color(theme::ACCENT),
                text(subject).size(11).color(theme::DIM).wrapping(Wrapping::None),
            ]
            .spacing(8),
        );
    }
    col = col.push(hairline());
    let body: Element<'_, Message> = if bw.loading {
        text("Thinking…").size(12).color(theme::DIM).into()
    } else {
        Column::with_children(render_prepared(app, &bw.prepared)).spacing(8).width(Fill).into()
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

    let panel = container(col).width(480).max_height(440).padding(16).style(theme::modal_panel);
    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .padding(Padding { top: 110.0, right: 0.0, bottom: 0.0, left: 0.0 })
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::BlameWhyClose))
}

// ---------------------------------------------------------------- finder modal

fn finder_modal(app: &App) -> Element<'_, Message> {
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

fn finder_file_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
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
                    text(dir).size(11).color(theme::DIM).wrapping(Wrapping::None),
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

fn finder_symbol_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
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
