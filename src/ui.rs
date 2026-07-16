//! All view code: toolbar, sidebar (files / search / marks), split code
//! panes, outline, status bar and the finder modal (files / symbols / :N).

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::text::Wrapping;
use iced::widget::{
    Column, button, center, column, container, mouse_area, opaque, row, scrollable, space, stack,
    text, text_input,
};
use iced::{Element, Fill, Font, Length, Padding};

use crate::codeview::CodeView;
use crate::finder::FinderMode;
use crate::fs_scan::DirNode;
use crate::viewer::Viewer;
use crate::{App, Message, SidebarTab, theme};

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

pub fn view(app: &App) -> Element<'_, Message> {
    let mut main = row![sidebar(app), pane_area(app)];
    if let Some(outline) = outline_pane(app) {
        main = main.push(outline);
    }
    let base: Element<'_, Message> =
        column![toolbar(app), main.height(Fill), statusbar(app)].into();

    if let Some(root) = &app.pending_consent {
        stack![base, consent_modal(root)].into()
    } else if let Some(consent) = &app.pending_lsp_consent {
        stack![base, lsp_consent_modal(consent)].into()
    } else if let Some(overlay) = app.overlay {
        stack![base, project_graph_modal(app, overlay)].into()
    } else if let Some(node) = &app.explain_view {
        stack![base, explanation_modal(app, node)].into()
    } else if app.server_panel {
        stack![base, server_panel_modal(app)].into()
    } else if app.finder.open {
        stack![base, finder_modal(app)].into()
    } else if let Some(menu) = &app.context_menu {
        stack![base, context_menu(menu)].into()
    } else if let Some(h) = app.hover.as_ref().filter(|h| h.text.is_some()) {
        stack![base, hover_tooltip(h)].into()
    } else {
        base
    }
}

// ---------------------------------------------------------------- hover tooltip

fn hover_tooltip(h: &crate::HoverState) -> Element<'_, Message> {
    let text_content = h.text.clone().unwrap_or_default();
    // Trim overly long hover text.
    let shown: String = if text_content.chars().count() > 1200 {
        text_content.chars().take(1200).collect::<String>() + "…"
    } else {
        text_content
    };

    let panel = container(
        scrollable(
            text(shown)
                .size(12)
                .font(Font::MONOSPACE)
                .color(theme::FG),
        )
        .height(iced::Length::Shrink),
    )
    .max_width(560)
    .max_height(320)
    .padding(8)
    .style(theme::modal_panel);

    // Position below-right of the hovered point.
    container(panel)
        .width(Fill)
        .height(Fill)
        .padding(Padding {
            top: h.y + 18.0,
            left: h.x,
            right: 0.0,
            bottom: 0.0,
        })
        .into()
}

// ---------------------------------------------------------------- context menu

fn context_menu(menu: &crate::ContextMenu) -> Element<'_, Message> {
    use crate::GotoKind;

    let item = |kind: GotoKind| {
        button(text(kind.label()).size(13))
            .style(theme::list_row(false))
            .width(Fill)
            .padding([5, 12])
            .on_press(Message::ContextGoto(kind))
    };

    let call_item = button(text("Call Hierarchy").size(13))
        .style(theme::list_row(false))
        .width(Fill)
        .padding([5, 12])
        .on_press(Message::CallHierarchyFromMenu);

    let panel = container(
        column![
            item(GotoKind::Definition),
            item(GotoKind::References),
            item(GotoKind::Implementation),
            item(GotoKind::TypeDefinition),
            call_item,
        ]
        .spacing(1),
    )
    .width(210)
    .padding(4)
    .style(theme::modal_panel);

    // Place the menu's top-left at the click point via padding.
    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .padding(Padding {
            top: menu.y,
            left: menu.x,
            right: 0.0,
            bottom: 0.0,
        });

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
        container(scrollable(Column::with_children(log_lines).spacing(1)).height(160)).padding([2, 8])
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
            scrollable(Column::with_children(rows).spacing(2).width(Fill)).height(iced::Length::Fill),
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

// -------------------------------------------------- project graph overlays

/// Path relative to the project root, for compact display in the overlays.
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
            let rect = iced::Rectangle {
                x: p.x + r + 3.0,
                y: p.y - 6.5,
                width,
                height: 13.0,
            };
            let is_hover = hovered == Some(i);
            if is_hover || !placed.iter().any(|pr| rects_overlap(*pr, rect)) {
                frame.fill_text(Text {
                    content: n.label.clone(),
                    position: iced::Point::new(rect.x, p.y),
                    color: if is_hover { theme::FG } else { theme::DIM },
                    size: 11.0.into(),
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

    // Uncalled functions — entry points, public API, or dead code.
    let uncalled = g.uncalled();
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

/// The Cmd+click explanation overlay: a file's/folder's architectural summary,
/// with a drill-down into the summaries it contains.
fn explanation_modal<'a>(app: &'a App, node: &'a crate::explain::Node) -> Element<'a, Message> {
    use crate::explain::Node;
    let summary = app
        .explanations
        .get(node)
        .map(|c| c.summary.clone())
        .unwrap_or_else(|| "Not explained yet — press Explain in the toolbar.".to_string());
    let title = match node {
        Node::Folder(p) => format!("📁 {}", rel_of(app, p)),
        Node::File(p) => rel_of(app, p),
        Node::Function { file, name } => format!("{name} · {}", rel_of(app, file)),
    };

    let mut rows: Vec<Element<'_, Message>> = vec![text(summary).size(13).color(theme::FG).into()];

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
            let short: String = sum.chars().take(100).collect();
            rows.push(
                button(
                    column![
                        text(explain_child_label(n)).size(12).color(theme::ACCENT),
                        text(short).size(10).color(theme::DIM),
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

    let panel = container(
        column![
            row![
                text(title).size(16).color(theme::FG).wrapping(Wrapping::None),
                space().width(Fill),
                button(text("Close").size(12))
                    .style(theme::toolbar_button)
                    .padding([3, 12])
                    .on_press(Message::CloseExplanation),
            ]
            .align_y(iced::Center),
            scrollable(Column::with_children(rows).spacing(6).width(Fill))
                .height(iced::Length::Fill),
        ]
        .spacing(12),
    )
    .width(680)
    .max_height(560)
    .padding(20)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::CloseExplanation))
}

fn section_header(label: &str) -> Element<'_, Message> {
    container(text(label.to_string()).size(11).color(theme::DIM))
        .padding(Padding {
            top: 10.0,
            right: 8.0,
            bottom: 2.0,
            left: 8.0,
        })
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
                text(format!("{}/.clew", root.display()))
                    .size(12)
                    .color(theme::ACCENT)
                    .font(Font::MONOSPACE)
                    .wrapping(Wrapping::None),
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

fn toolbar(app: &App) -> Element<'_, Message> {
    let nav = |label: &'static str, enabled: bool, msg: Message| {
        let mut b = button(text(label).size(14))
            .style(theme::toolbar_button)
            .padding([2, 9]);
        if enabled {
            b = b.on_press(msg);
        }
        b
    };
    let tool = |label: &'static str, msg: Message| {
        button(text(label).size(12))
            .style(theme::toolbar_button)
            .padding([3, 10])
            .on_press(msg)
    };

    let path_label: Element<'_, Message> = match app.active_viewer() {
        Some(v) => text(&v.rel).size(13).into(),
        None => text("").into(),
    };

    let mut bar = row![
        nav("←", app.history.can_back(), Message::GoBack),
        nav("→", app.history.can_forward(), Message::GoForward),
        path_label,
        space().width(Fill),
    ]
    .spacing(8)
    .align_y(iced::Center)
    .padding([6, 10]);

    if app.window_width >= 1000.0 {
        bar = bar.push(
            text("⌘P files · ⌘T symbols · ⌘⇧F search · ⌘D mark")
                .size(11)
                .color(theme::DIM),
        );
    }
    bar = bar
        .push(tool("Open Folder…", Message::OpenFolderPressed))
        .push(tool("Servers", Message::ToggleServerPanel))
        .push(tool(
            "Call Graph",
            Message::OpenOverlay(crate::Overlay::ProjectCalls),
        ))
        .push(tool(
            "Import Graph",
            Message::OpenOverlay(crate::Overlay::ProjectImports),
        ));
    // Explain appears only when an LLM key is configured.
    if app.llm_available {
        let label = match app.explain_progress {
            Some((done, total)) if total > 0 => format!("Explaining {done}/{total}…"),
            Some(_) => "Explaining…".to_string(),
            None => "Explain".to_string(),
        };
        let mut btn = button(text(label).size(12)).style(theme::toolbar_button).padding([3, 10]);
        if !app.explaining {
            btn = btn.on_press(Message::ExplainProject);
        }
        bar = bar.push(btn);
    }
    bar = bar
        .push(tool("Diff", Message::ToggleDiff))
        .push(tool("Split", Message::ToggleSplit))
        .push(tool("Outline", Message::ToggleOutline));

    container(bar).width(Fill).style(theme::panel).into()
}

// ---------------------------------------------------------------- sidebar

fn sidebar(app: &App) -> Element<'_, Message> {
    let tab = |label: &'static str, this: SidebarTab| {
        button(text(label).size(11))
            .style(theme::tab_button(app.sidebar == this))
            .width(Fill)
            .padding([5, 0])
            .on_press(Message::SidebarTabPicked(this))
    };
    // CALLS/IMPORTS are always present; their panels show a hint until populated.
    let tabs = row![
        tab("FILES", SidebarTab::Files),
        tab("SEARCH", SidebarTab::Search),
        tab("MARKS", SidebarTab::Marks),
        tab("CALLS", SidebarTab::Calls),
        tab("IMPORTS", SidebarTab::Imports),
    ];

    let content: Element<'_, Message> = match app.sidebar {
        SidebarTab::Files => files_tab(app),
        SidebarTab::Search => search_tab(app),
        SidebarTab::Marks => marks_tab(app),
        SidebarTab::Calls => calls_tab(app),
        SidebarTab::Imports => imports_tab(app),
    };

    // Narrow windows get a slimmer sidebar so the code pane keeps room.
    let width = if app.window_width < 700.0 {
        180.0
    } else if app.window_width < 1000.0 {
        220.0
    } else {
        280.0
    };
    container(column![tabs, content])
        .width(width)
        .height(Fill)
        .style(theme::panel)
        .into()
}

fn files_tab(app: &App) -> Element<'_, Message> {
    let Some(project) = &app.project else {
        let hint = if app.scanning {
            "Scanning…"
        } else {
            "No folder open"
        };
        return container(text(hint).size(12).color(theme::DIM))
            .padding(12)
            .into();
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    append_tree_rows(&mut rows, &project.tree, "", 0, app);
    scrollable(Column::with_children(rows).width(Fill))
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
        let arrow = if expanded { "▾ " } else { "▸ " };
        rows.push(
            button(
                text(format!("{arrow}{name}"))
                    .size(13)
                    .wrapping(Wrapping::None),
            )
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
        rows.push(
            button(
                text(format!("  {name}"))
                    .size(13)
                    .wrapping(Wrapping::None),
            )
            .style(theme::list_row(is_current))
            .width(Fill)
            .padding(pad)
            .on_press(Message::OpenRel { rel, line: None })
            .into(),
        );
    }
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

    // Match-option chips: case-sensitive, whole-word, regex.
    let chip = |label: &'static str, active: bool, opt: SearchOpt| -> Element<'_, Message> {
        button(text(label).size(12).font(Font::MONOSPACE))
            .style(theme::tab_button(active))
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
    col.push(scrollable(Column::with_children(rows).width(Fill)).height(Fill))
        .into()
}

fn marks_tab(app: &App) -> Element<'_, Message> {
    if app.bookmarks.is_empty() {
        return container(
            text("No bookmarks yet.\n⌘D marks the current line.")
                .size(12)
                .color(theme::DIM),
        )
        .padding(12)
        .into();
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for (idx, bm) in app.bookmarks.iter().enumerate() {
        if last_rel != Some(bm.rel.as_str()) {
            last_rel = Some(bm.rel.as_str());
            rows.push(group_header(&bm.rel));
        }
        rows.push(
            row![
                button(
                    row![
                        text(bm.line.to_string()).size(11).color(theme::DIM).width(36),
                        text(&bm.preview).size(12).wrapping(Wrapping::None),
                    ]
                    .spacing(4),
                )
                .style(theme::list_row(false))
                .width(Fill)
                .padding(Padding {
                    top: 1.0,
                    right: 2.0,
                    bottom: 1.0,
                    left: 8.0,
                })
                .on_press(Message::OpenRel {
                    rel: bm.rel.clone(),
                    line: Some(bm.line),
                }),
                button(text("✕").size(10).color(theme::DIM))
                    .style(theme::list_row(false))
                    .padding([3, 6])
                    .on_press(Message::BookmarkRemoved(idx)),
            ]
            .align_y(iced::Center)
            .into(),
        );
    }

    column![scrollable(Column::with_children(rows).width(Fill)).height(Fill)]
        .padding(Padding {
            top: 6.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
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

/// The call-hierarchy tree: a header with the root symbol + a callers/callees
/// toggle, then the lazily-expanded tree.
fn calls_tab(app: &App) -> Element<'_, Message> {
    let Some(tree) = &app.call_graph else {
        return container(
            column![
                text("No call hierarchy yet.").size(12).color(theme::DIM),
                space().height(6),
                text("Put the cursor on a function and press gc,")
                    .size(11)
                    .color(theme::DIM),
                text("or right-click it → Call Hierarchy.")
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
    col.push(scrollable(Column::with_children(rows).width(Fill)).height(Fill))
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
    col.push(scrollable(Column::with_children(rows).width(Fill)).height(Fill))
        .into()
}

fn group_header(rel: &str) -> Element<'_, Message> {
    container(
        text(rel)
            .size(11)
            .color(theme::ACCENT)
            .wrapping(Wrapping::None),
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
        return editor_shell(center(text("Scanning project…").color(theme::DIM)).into());
    }
    if app.project.is_none() {
        return editor_shell(welcome());
    }
    if !app.split {
        return pane_view(app, 0);
    }
    row![pane_view(app, 0), pane_view(app, 1)]
        .spacing(1)
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
        None => mouse_area(center(
            text("Pick a file from the tree, or press ⌘P")
                .size(14)
                .color(theme::DIM),
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
            vertical: Scrollbar::default(),
            horizontal: Scrollbar::default(),
        })
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

fn welcome() -> Element<'static, Message> {
    center(
        column![
            text("clew").size(46).color(theme::ACCENT),
            text("a reader for code").size(15).color(theme::DIM),
            space().height(14),
            button(text("Open Folder…").size(14))
                .style(theme::toolbar_button)
                .padding([8, 18])
                .on_press(Message::OpenFolderPressed),
            space().height(6),
            text("tip: `clew <path>` opens a project directly")
                .size(12)
                .color(theme::DIM),
        ]
        .spacing(6)
        .align_x(iced::Center),
    )
    .into()
}

fn code_pane<'a>(app: &'a App, pane: usize, v: &'a Viewer) -> Element<'a, Message> {
    // Bookmarked lines of this file, for the gutter marker.
    let marked: std::collections::HashSet<usize> = app
        .bookmarks
        .iter()
        .filter(|b| b.rel == v.rel)
        .map(|b| b.line)
        .collect();

    // The block cursor shows only on the active pane while the code view has
    // keyboard focus.
    let cursor = if pane == app.active && app.code_focused {
        v.caret
    } else {
        None
    };

    let code = CodeView::new(
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
    .folds(v.visible_rows(), &v.fold_header_set, &v.collapsed)
    .on_fold(move |line| Message::FoldToggle { pane, line })
    .indent_guides(true)
    .on_minimap(move |fraction| Message::MinimapScrolled { pane, fraction })
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
    });

    scrollable(code)
        .id(code_scroll_id(pane))
        .on_scroll(move |viewport| Message::CodeScrolled(pane, viewport))
        .direction(Direction::Both {
            vertical: Scrollbar::default(),
            horizontal: Scrollbar::default(),
        })
        .width(Fill)
        .height(Fill)
        .into()
}

// ---------------------------------------------------------------- outline

fn outline_pane(app: &App) -> Option<Element<'_, Message>> {
    // Hide the outline on narrow windows or when split: code panes first.
    if !app.show_outline || app.split || app.window_width < 950.0 {
        return None;
    }
    let symbols = &app.active_viewer()?.symbols;
    if symbols.is_empty() {
        return None;
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(
        container(text("OUTLINE").size(11).color(theme::DIM))
            .padding(Padding {
                top: 8.0,
                right: 8.0,
                bottom: 4.0,
                left: 10.0,
            })
            .into(),
    );
    for symbol in symbols {
        rows.push(
            button(
                row![
                    text(short_kind(&symbol.kind))
                        .size(10)
                        .color(kind_color(&symbol.kind))
                        .width(40),
                    text(&symbol.name).size(12).wrapping(Wrapping::None),
                ]
                .spacing(4)
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding {
                top: 2.0,
                right: 6.0,
                bottom: 2.0,
                left: 10.0,
            })
            .on_press(Message::OutlineJump(symbol.line))
            .into(),
        );
    }

    Some(
        container(scrollable(Column::with_children(rows).width(Fill)).height(Fill))
            .width(230)
            .height(Fill)
            .style(theme::panel)
            .into(),
    )
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
    let right = match app.active_viewer() {
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
    };

    container(
        row![
            text(&app.status).size(11),
            space().width(Fill),
            text(right).size(11),
        ]
        .padding([3, 10]),
    )
    .width(Fill)
    .style(theme::statusbar)
    .into()
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
            scrollable(Column::with_children(rows).width(Fill)).height(iced::Length::Shrink),
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
        rows.push(
            button(
                row![
                    text(name).size(13),
                    text(dir).size(11).color(theme::DIM).wrapping(Wrapping::None),
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
