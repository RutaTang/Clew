//! Import/call graph modals, node-link canvas, and the graph list bodies.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// Modal frame shared by the project-graph overlays: a titled panel with a
/// List/Map toggle, over a dismissable backdrop.
pub(crate) fn graph_modal_frame<'a>(
    title: &'a str,
    graph_mode: bool,
    extra: Option<Element<'a, Message>>,
    body: Element<'a, Message>,
) -> Element<'a, Message> {
    let toggle_label = if graph_mode { "List" } else { "Map" };
    let mut header = row![text(title).size(17).color(theme::FG), space().width(Fill),]
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
    let panel = container(column![header, body].spacing(12))
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

pub(crate) fn project_graph_modal(app: &App, overlay: crate::Overlay) -> Element<'_, Message> {
    let title = match overlay {
        crate::Overlay::ProjectImports => "Project Import Graph",
        crate::Overlay::ProjectCalls => "Project Call Graph",
    };
    // The call graph can be refined to exact LSP edges; show its control/status.
    let extra: Option<Element<'_, Message>> = match overlay {
        crate::Overlay::ProjectCalls => Some(
            if let Some((done, total)) = app.project_calls.refine_progress {
                text(format!("Refining {done}/{total}…"))
                    .size(11)
                    .color(theme::rgb(0xe5c07b))
                    .into()
            } else if app.project_calls.precise {
                text("● LSP-precise").size(11).color(theme::ACCENT).into()
            } else {
                button(text("Refine with LSP").size(11))
                    .style(theme::toolbar_button)
                    .padding([3, 10])
                    .on_press(Message::RefineProjectCalls)
                    .into()
            },
        ),
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
pub(crate) fn graph_map_view(app: &App) -> Element<'_, Message> {
    let overlay = app.overlay;
    let hint = |msg: &str| {
        container(text(msg.to_string()).size(12).color(theme::DIM))
            .padding(8)
            .width(Fill)
            .height(iced::Length::Fill)
            .into()
    };
    // While the project is still being scanned/indexed the graph is legitimately
    // empty — say so, rather than "Nothing to show" (which reads as "no data").
    let empty_msg = if app.project_calls.building {
        "Building call graph…"
    } else if app.scanning || app.indexing {
        "Indexing the project…"
    } else {
        "Nothing to show."
    };
    let Some(layout) = &app.graph_layout else {
        return hint(empty_msg);
    };
    if layout.nodes.is_empty() {
        return hint(empty_msg);
    }
    let Some(kind) = overlay else {
        return hint(empty_msg);
    };
    let map = iced::widget::canvas::Canvas::new(GraphCanvas {
        layout,
        kind,
        scroll_zooms: true,
    })
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
/// A stable node colour per language for the import/call graphs, so a
/// mixed-language project reads by hue. Unknown/unsupported files fall back to
/// the accent (the previous single colour), so a single-language project looks
/// unchanged.
pub(crate) fn lang_dot_color(lang: Option<&str>) -> iced::Color {
    match lang {
        Some("rust") => theme::rgb(0xe0803c),
        Some("typescript" | "tsx") => theme::rgb(0x4a9eee),
        Some("javascript") => theme::rgb(0xd8c05a),
        Some("python") => theme::rgb(0x5db85d),
        Some("go") => theme::rgb(0x4ec9d0),
        Some("dart") => theme::rgb(0x35b8c4),
        Some("java") => theme::rgb(0xcc7a3a),
        Some("c") => theme::rgb(0x8895a8),
        Some("cpp") => theme::rgb(0x9a78c8),
        _ => theme::ACCENT,
    }
}

pub(crate) struct GraphCanvas<'a> {
    pub(crate) layout: &'a crate::graphlayout::Layout,
    pub(crate) kind: crate::Overlay,
    /// Whether a wheel-scroll zooms (and is captured). True for the full-screen
    /// graph modal; false for the small map embedded in the scrollable Overview
    /// page, where capturing scroll would trap the page and hide the prose below.
    pub(crate) scroll_zooms: bool,
}

/// Padding inside the canvas so node labels aren't clipped at the edges.
const GRAPH_PAD: f32 = 48.0;

/// Pan/zoom view state for a map, persisted by the canvas widget across frames.
#[derive(Clone, Copy)]
pub(crate) struct MapView {
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
pub(crate) fn rects_overlap(a: iced::Rectangle, b: iced::Rectangle) -> bool {
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

        // Circles for every node, coloured by the file's language so a
        // mixed-language project separates by hue at a glance (Rust orange, TS
        // blue, Python green, …); a file in an import cycle keeps its language
        // colour and gains a gold ring.
        for (i, n) in self.layout.nodes.iter().enumerate() {
            let p = self.node_screen(i, bounds, state);
            let r = 3.5 + n.weight.sqrt() * 1.8;
            let base = lang_dot_color(crate::highlight::detect(&n.file));
            let color = if hovered == Some(i) { theme::FG } else { base };
            frame.fill(&Path::circle(p, r), color);
            if n.cyclic {
                frame.stroke(
                    &Path::circle(p, r + 1.5),
                    Stroke::default()
                        .with_width(1.5)
                        .with_color(theme::rgb(0xe5c07b)),
                );
            }
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
                (
                    p.x - r - 3.0 - width,
                    p.x - r - 3.0,
                    iced::alignment::Horizontal::Right,
                )
            } else {
                (
                    p.x + r + 3.0,
                    p.x + r + 3.0,
                    iced::alignment::Horizontal::Left,
                )
            };
            let rect = iced::Rectangle {
                x: rect_x,
                y: p.y - 6.5,
                width,
                height: 13.0,
            };
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
            // Zoom around the cursor — but only where zoom is enabled; in the
            // embedded Overview map we let the wheel fall through to the page.
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) if self.scroll_zooms => {
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
        match cursor
            .position_in(bounds)
            .and_then(|c| self.hit(c, bounds, state))
        {
            Some(_) => Interaction::Pointer,
            None if cursor.is_over(bounds) => Interaction::Grab,
            None => Interaction::default(),
        }
    }
}

/// Which macOS-style window control an icon draws.
#[derive(Clone, Copy)]
pub(crate) enum TrafficIcon {
    Close,
    Minimize,
    /// `true` while the window is fullscreen (draws the collapse variant).
    Fullscreen(bool),
}

/// Draws the traffic-light glyphs by hand so they match the native macOS
/// weight: thin round-capped strokes for the ✕ and −, and two solid triangles
/// with a diagonal gap for the fullscreen control. Font glyphs (Nerd Font)
/// render far too bold/large at this size, so we stroke/fill directly.
pub(crate) struct TrafficGlyph {
    pub(crate) icon: TrafficIcon,
    pub(crate) color: iced::Color,
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
                    frame.fill(
                        &tri([(pad, pad), (pad + leg, pad), (pad, pad + leg)]),
                        self.color,
                    );
                    frame.fill(
                        &tri([
                            (m - pad, m - pad),
                            (m - pad - leg, m - pad),
                            (m - pad, m - pad - leg),
                        ]),
                        self.color,
                    );
                }
            }
        }
        vec![frame.into_geometry()]
    }
}

/// A file row in the import overlay: name + directory + fan-in/out counts.
pub(crate) fn import_file_row<'a>(app: &'a App, path: &std::path::Path) -> Element<'a, Message> {
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
            text(dir)
                .size(10)
                .color(theme::DIM)
                .wrapping(Wrapping::None),
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
    // Left padding matches `section_header` (10) so the file name lines up with
    // the section title above it.
    .padding(Padding {
        top: 2.0,
        right: 10.0,
        bottom: 2.0,
        left: 10.0,
    })
    .on_press(Message::OverlayOpenImports(path.to_path_buf()))
    .into()
}

pub(crate) fn project_imports_body(app: &App) -> Element<'_, Message> {
    let g = &app.import_graph;
    if g.is_empty() {
        return container(
            text("No imports found in this project.")
                .size(12)
                .color(theme::DIM),
        )
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
                .map(|p| {
                    p.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string()
                })
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
            container(text(externals.join("  ·  ")).size(11).color(theme::DIM))
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
pub(crate) fn call_symbol_row<'a>(
    app: &'a App,
    id: usize,
    trailing: String,
) -> Element<'a, Message> {
    let n = app.project_calls.graph.node(id);
    button(
        row![
            text(n.name.clone()).size(12).wrapping(Wrapping::None),
            space().width(6),
            text(format!("{}:{}", rel_of(app, &n.file), n.line))
                .size(10)
                .color(theme::DIM)
                .wrapping(Wrapping::None),
            space().width(Fill),
            text(trailing)
                .size(10)
                .color(theme::DIM)
                .wrapping(Wrapping::None),
        ]
        .align_y(iced::Center),
    )
    .style(theme::list_row(false))
    .width(Fill)
    // Left padding matches `section_header` (10) so a row's name lines up with
    // the section title above it.
    .padding(Padding {
        top: 2.0,
        right: 10.0,
        bottom: 2.0,
        left: 10.0,
    })
    .on_press(Message::OverlayOpenAt {
        abs: n.file.clone(),
        line: n.line,
    })
    .into()
}

pub(crate) fn project_calls_body(app: &App) -> Element<'_, Message> {
    let g = &app.project_calls.graph;
    if g.is_empty() {
        let msg = if app.project_calls.building {
            "Building call graph…"
        } else {
            "No functions found in this project."
        };
        return container(text(msg).size(12).color(theme::DIM))
            .padding(8)
            .into();
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
        text(if app.project_calls.precise {
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
    let uncalled: Vec<usize> = g
        .uncalled()
        .into_iter()
        .filter(|&id| !is_test_node(id))
        .collect();
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
