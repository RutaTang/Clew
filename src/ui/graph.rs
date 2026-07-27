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
    graph_3d: bool,
    graph_spin: bool,
    extra: Option<Element<'a, Message>>,
    body: Element<'a, Message>,
) -> Element<'a, Message> {
    // Header controls are icon buttons; each names itself on hover (`chrome_tip`).
    let icon_btn = |g: Glyph, tip: &'static str, msg: Message| -> Element<'a, Message> {
        chrome_tip(
            button(glyph::icon(g, theme::fg_muted(), 16.0))
                .style(theme::toolbar_button)
                .padding([4, 8])
                .on_press(msg),
            tip,
            None,
        )
    };
    let mut header = row![text(title).size(17).color(theme::fg()), space().width(Fill)]
        .spacing(4)
        .align_y(iced::Center);
    if let Some(extra) = extra {
        header = header.push(extra);
    }
    // Map mode: a spin start/stop (3D only), then a 2D/3D projection toggle. Each
    // icon shows the action / the mode it switches to.
    if graph_mode && graph_3d {
        header = header.push(if graph_spin {
            icon_btn(Glyph::Pause, "Stop spinning", Message::GraphToggleSpin)
        } else {
            icon_btn(Glyph::Play, "Spin", Message::GraphToggleSpin)
        });
    }
    if graph_mode {
        header = header.push(if graph_3d {
            icon_btn(Glyph::Plane, "Flatten to 2D", Message::GraphToggle3D)
        } else {
            icon_btn(Glyph::Cube, "Show in 3D", Message::GraphToggle3D)
        });
    }
    header = header
        .push(if graph_mode {
            icon_btn(Glyph::List, "List view", Message::OverlayViewToggle)
        } else {
            icon_btn(Glyph::Graph, "Map view", Message::OverlayViewToggle)
        })
        .push(icon_btn(Glyph::Close, "Close", Message::CloseOverlay));
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
                    .color(theme::warning())
                    .into()
            } else if app.project_calls.precise {
                text("● LSP-precise").size(11).color(theme::accent()).into()
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
    graph_modal_frame(
        title,
        app.graph_mode,
        app.graph_3d,
        app.graph_spin,
        extra,
        body,
    )
}

/// The node-link map: a force-directed canvas plus a legend.
pub(crate) fn graph_map_view(app: &App) -> Element<'_, Message> {
    let overlay = app.overlay;
    let hint = |msg: &str| {
        container(text(msg.to_string()).size(12).color(theme::dim()))
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
        is_3d: app.graph_3d,
        spin: app.graph_spin,
    })
    .width(Fill)
    .height(Fill);
    let nav = if app.graph_3d {
        "drag to orbit"
    } else {
        "drag to pan"
    };
    let legend = if layout.total > layout.nodes.len() {
        format!(
            "Showing the {} most-connected of {} files · {nav} · drag a node to move it · scroll to zoom · click a node to open it",
            layout.nodes.len(),
            layout.total,
        )
    } else {
        format!(
            "{nav} · drag a node · scroll to zoom · size = degree · hue = language · rich = root, pale = deep · arrow → the file it imports · gold ring = cycle"
        )
    };
    column![map, text(legend).size(10).color(theme::dim())]
        .spacing(6)
        .height(iced::Length::Fill)
        .into()
}

/// A distinct base colour per graphed language, so a mixed-language project
/// reads by hue. The six are the languages clew fully supports in the
/// import/call graphs; their hues are spread warm → cool around the wheel
/// (orange · yellow · green · teal · cyan · blue) and kept saturated so they
/// survive being dimmed with depth. Anything else is filtered out of the graph
/// upstream, so the neutral fallback should not normally appear.
pub(crate) fn lang_dot_color(lang: Option<&str>) -> iced::Color {
    match lang {
        Some("rust") => theme::rgb(0xf2843c),
        Some("javascript") => theme::rgb(0xedd24e),
        Some("python") => theme::rgb(0x57c167),
        Some("dart") => theme::rgb(0x2fc79c),
        Some("go") => theme::rgb(0x2fc2e4),
        Some("typescript" | "tsx") => theme::rgb(0x5090f0),
        _ => theme::rgb(0x8a93a6),
    }
}

/// Shade a node's language colour by its *hierarchy* depth (0 = a root / entry
/// point that nothing imports, 1 = deepest). The hue is kept (so language stays
/// legible), and the paleness tracks depth: **roots are the full, rich language
/// colour** (entry points like main.rs stand out) while **deeper nodes fade
/// paler** — desaturated and lifted toward white. Being structural, the colour
/// stays stable as the graph rotates (unlike a camera-depth fog).
fn hier_shade(base: iced::Color, depth: f32) -> iced::Color {
    let t = depth.clamp(0.0, 1.0);
    let sat = 1.0 - 0.65 * t; // root full-saturation → deep toward grey
    let lum = 0.2126 * base.r + 0.7152 * base.g + 0.0722 * base.b;
    let desat = |c: f32| lum + (c - lum) * sat; // toward grey
    // The lightness axis fades deep nodes *away from the background* so they
    // recede yet stay visible: toward white on the dark canvas, toward a light
    // grey on the light one (never all the way to the near-white background).
    let (target, lift) = if theme::is_light() {
        (0.78, 0.62 * t)
    } else {
        (1.0, 0.55 * t)
    };
    let chan = |c: f32| {
        let d = desat(c);
        (d + (target - d) * lift).clamp(0.0, 1.0)
    };
    iced::Color::from_rgb(chan(base.r), chan(base.g), chan(base.b))
}

/// Structural greys for the map's edges and arrowheads, tuned per theme so the
/// faint lines read against either background.
fn edge_ink() -> iced::Color {
    if theme::is_light() {
        theme::rgb(0xaab0ba)
    } else {
        theme::rgb(0x5a6272)
    }
}
fn arrow_ink() -> iced::Color {
    if theme::is_light() {
        theme::rgb(0x878d99)
    } else {
        theme::rgb(0x9098ab)
    }
}

/// Ease-in-out ramp from 0 (at/below `e0`) to 1 (at/above `e1`).
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

pub(crate) struct GraphCanvas<'a> {
    pub(crate) layout: &'a crate::graphlayout::Layout,
    pub(crate) kind: crate::Overlay,
    /// Whether a wheel-scroll zooms (and is captured). True for the full-screen
    /// graph modal; false for the small map embedded in the scrollable Overview
    /// page, where capturing scroll would trap the page and hide the prose below.
    pub(crate) scroll_zooms: bool,
    /// Render/interact in 3D (orbit + perspective + depth) when true, else as a
    /// flat 2D plane (pan, no rotation, uniform depth).
    pub(crate) is_3d: bool,
    /// Whether the idle auto-spin is running (3D only).
    pub(crate) spin: bool,
}

/// Padding inside the canvas so node labels aren't clipped at the edges.
const GRAPH_PAD: f32 = 48.0;

/// Which kind of drag is in progress on the map.
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    /// Orbiting the camera (3D, press began on empty space).
    Orbit,
    /// Panning the view (2D, press began on empty space).
    Pan,
    /// Moving a single node, pinned to the cursor while the rest reacts.
    Node(usize),
}

/// Camera distance and focal length for the perspective projection, in world
/// units. `CAM` sits behind the graph looking at the origin; both comfortably
/// exceed the graph's radius so no node crosses behind the camera.
const CAM: f32 = 2400.0;
const FOCAL: f32 = 2400.0;

/// Live 3D-simulation + orbit camera + interaction state, persisted by the
/// canvas widget across frames. The force sim and the camera both run here
/// (stepped each `RedrawRequested`), so every graph animates in 3D, spins
/// gently, and its nodes can be grabbed and moved.
pub(crate) struct GraphState {
    /// Node positions/velocities in 3D world space. Empty until seeded for the
    /// current node set; a rebuilt graph (new `sig`) reseeds.
    pos: Vec<crate::graphlayout::V3>,
    vel: Vec<crate::graphlayout::V3>,
    sig: u64,
    /// Cooling factor for the physics; decays each frame so the layout settles.
    alpha: f32,
    last_frame: Option<std::time::Instant>,
    /// Orbit camera: yaw (around Y), pitch (around X), and a zoom multiplier.
    yaw: f32,
    pitch: f32,
    zoom: f32,
    /// Projection fit, recomputed each frame from the graph's radius (which is
    /// rotation-invariant, so spinning doesn't make it pulse): world → screen is
    /// `raw2d * fit_scale + fit_off`.
    fit_scale: f32,
    fit_off: (f32, f32),
    /// Extra pan offset, used only in 2D mode (3D re-centres on the origin).
    pan: iced::Vector,
    /// Whether the previous frame rendered in 3D, so a 2D → 3D switch can
    /// re-inflate the depth (2D collapses it flat).
    was_3d: bool,
    /// Per-node label opacity, eased each frame toward its target (depth fade ×
    /// whether decluttering keeps it). Animating this — rather than deciding
    /// draw/skip per frame — means labels cross-fade instead of popping as the
    /// graph rotates. Empty until seeded for the current node set.
    label_alpha: Vec<f32>,
    drag: Drag,
    /// Distance dragged since press — a tiny total means "click", not a drag.
    moved: f32,
    /// Last absolute cursor position while dragging.
    last_cursor: iced::Point,
}

impl Default for GraphState {
    fn default() -> Self {
        GraphState {
            pos: Vec::new(),
            vel: Vec::new(),
            sig: 0,
            alpha: 0.0,
            last_frame: None,
            yaw: 0.6,
            pitch: 0.35,
            zoom: 1.0,
            fit_scale: 1.0,
            fit_off: (0.0, 0.0),
            pan: iced::Vector::new(0.0, 0.0),
            was_3d: true,
            label_alpha: Vec::new(),
            drag: Drag::None,
            moved: 0.0,
            last_cursor: iced::Point::new(0.0, 0.0),
        }
    }
}

/// A cheap signature of the node set, so a rebuilt graph (different nodes) is
/// reseeded rather than animated from stale positions.
fn layout_sig(layout: &crate::graphlayout::Layout) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |v: u64| h = (h ^ v).wrapping_mul(0x100000001b3);
    mix(layout.nodes.len() as u64);
    mix(layout.edges.len() as u64);
    for n in &layout.nodes {
        for b in n.label.bytes() {
            mix(b as u64);
        }
    }
    h
}

/// Rotate a world point by the camera's yaw (around Y) then pitch (around X).
fn rotate(p: crate::graphlayout::V3, yaw: f32, pitch: f32) -> crate::graphlayout::V3 {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let x1 = p[0] * cy + p[2] * sy;
    let z1 = -p[0] * sy + p[2] * cy;
    let y1 = p[1];
    let y2 = y1 * cp - z1 * sp;
    let z2 = y1 * sp + z1 * cp;
    [x1, y2, z2]
}

impl GraphCanvas<'_> {
    /// Untransformed auto-fit pixel position of node `i` — the fallback used for
    /// the first frame before the live sim has seeded.
    fn node_fit(&self, i: usize, bounds: iced::Rectangle) -> iced::Point {
        let n = &self.layout.nodes[i];
        let w = (bounds.width - 2.0 * GRAPH_PAD).max(1.0);
        let h = (bounds.height - 2.0 * GRAPH_PAD).max(1.0);
        iced::Point::new(GRAPH_PAD + n.x * w, GRAPH_PAD + n.y * h)
    }

    /// Perspective-project a rotated point to raw 2D (before the fit) plus its
    /// camera depth (`z`, larger = nearer) and perspective factor.
    fn project_view(p: crate::graphlayout::V3) -> (f32, f32, f32, f32) {
        let persp = FOCAL / (CAM - p[2]).max(FOCAL * 0.15);
        (p[0] * persp, p[1] * persp, p[2], persp)
    }

    /// Screen position of node `i`: full 3D projection once seeded, else the
    /// static fallback. Returns `(x, y, depth, perspective)`.
    fn node_screen(
        &self,
        i: usize,
        bounds: iced::Rectangle,
        st: &GraphState,
    ) -> (f32, f32, f32, f32) {
        if st.pos.len() != self.layout.nodes.len() {
            let p = self.node_fit(i, bounds);
            return (p.x, p.y, 0.0, 1.0);
        }
        if self.is_3d {
            let (rx, ry, z, persp) = Self::project_view(rotate(st.pos[i], st.yaw, st.pitch));
            (
                rx * st.fit_scale + st.fit_off.0,
                ry * st.fit_scale + st.fit_off.1,
                z,
                persp,
            )
        } else {
            // Flat 2D: orthographic x/y with a pan, uniform depth (no fog/size).
            let p = st.pos[i];
            (
                p[0] * st.fit_scale + st.fit_off.0 + st.pan.x,
                p[1] * st.fit_scale + st.fit_off.1 + st.pan.y,
                0.0,
                1.0,
            )
        }
    }

    /// The node nearest `cursor` within a click radius, preferring the one
    /// nearest the camera when several overlap.
    fn hit(&self, cursor: iced::Point, bounds: iced::Rectangle, st: &GraphState) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for i in 0..self.layout.nodes.len() {
            let (x, y, z, _) = self.node_screen(i, bounds, st);
            if ((x - cursor.x).powi(2) + (y - cursor.y).powi(2)).sqrt() < 22.0
                && best.is_none_or(|(_, bz)| z > bz)
            {
                best = Some((i, z));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Move node `i` so it re-projects to `cursor`, keeping its current camera
    /// depth so a grab follows the pointer without jumping toward or away.
    fn drag_node_to(
        &self,
        i: usize,
        cursor: iced::Point,
        st: &GraphState,
    ) -> crate::graphlayout::V3 {
        if !self.is_3d {
            // Flat 2D: straight inverse of the orthographic transform.
            return [
                (cursor.x - st.fit_off.0 - st.pan.x) / st.fit_scale.max(1e-3),
                (cursor.y - st.fit_off.1 - st.pan.y) / st.fit_scale.max(1e-3),
                0.0,
            ];
        }
        let z2 = rotate(st.pos[i], st.yaw, st.pitch)[2];
        let persp = FOCAL / (CAM - z2).max(FOCAL * 0.15);
        // Undo fit → raw2d → view xy (divide out the perspective).
        let x1 = (cursor.x - st.fit_off.0) / st.fit_scale.max(1e-3) / persp;
        let y2 = (cursor.y - st.fit_off.1) / st.fit_scale.max(1e-3) / persp;
        // Un-rotate (inverse pitch, then inverse yaw).
        let (sy, cy) = st.yaw.sin_cos();
        let (sp, cp) = st.pitch.sin_cos();
        let y1 = y2 * cp + z2 * sp;
        let z1 = -y2 * sp + z2 * cp;
        [x1 * cy - z1 * sy, y1, x1 * sy + z1 * cy]
    }

    fn open_message(&self, i: usize) -> Message {
        let file = self.layout.nodes[i].file.clone();
        match self.kind {
            crate::Overlay::ProjectImports => Message::OverlayOpenImports(file),
            crate::Overlay::ProjectCalls => Message::OverlayOpenAt { abs: file, line: 1 },
        }
    }

    /// Advance the physics + camera one frame and refit. Reseeds if the node set
    /// changed. Always returns true (a visible graph spins gently, so it is
    /// effectively always animating).
    fn tick(
        &self,
        st: &mut GraphState,
        bounds: iced::Rectangle,
        now: std::time::Instant,
        cursor: iced::advanced::mouse::Cursor,
    ) -> bool {
        use crate::graphlayout::WORLD;
        let n = self.layout.nodes.len();
        if st.sig != layout_sig(self.layout) || st.pos.len() != n {
            // Seed from the static layout, lifted into 3D with a deterministic z
            // spread so the graph opens as a volume rather than a flat sheet.
            st.pos = self
                .layout
                .nodes
                .iter()
                .enumerate()
                .map(|(i, nd)| {
                    let z = (((i * 61) % 100) as f32 / 100.0 - 0.5) * WORLD * 0.8;
                    [(nd.x - 0.5) * WORLD, (nd.y - 0.5) * WORLD, z]
                })
                .collect();
            st.vel = vec![[0.0; 3]; n];
            st.alpha = 1.0;
            st.last_frame = None;
            st.sig = layout_sig(self.layout);
        }
        let dt = st
            .last_frame
            .map(|t| now.duration_since(t).as_secs_f32())
            .unwrap_or(1.0 / 60.0)
            .clamp(0.004, 0.05);
        st.last_frame = Some(now);

        // Coming back to 3D after a flat 2D view: re-inflate the depth and wake
        // the sim, so it re-expands into a volume instead of staying a flat sheet
        // seen edge-on.
        if self.is_3d && !st.was_3d {
            for (i, p) in st.pos.iter_mut().enumerate() {
                p[2] = (((i * 61) % 100) as f32 / 100.0 - 0.5) * WORLD * 0.8;
            }
            st.alpha = st.alpha.max(0.7);
        }
        st.was_3d = self.is_3d;

        let pinned = match st.drag {
            Drag::Node(i) => Some(i),
            _ => None,
        };
        if pinned.is_some() {
            st.alpha = st.alpha.max(0.3);
        }
        // Step the physics while warm or grabbing; skip the O(n²) once cooled.
        if pinned.is_some() || st.alpha > 0.02 {
            let k = crate::graphlayout::ideal_k(n);
            crate::graphlayout::fr_step3(
                &mut st.pos,
                &mut st.vel,
                &self.layout.edges,
                pinned,
                k,
                st.alpha,
                dt * 4.0,
            );
            st.alpha = (st.alpha * 0.985).max(0.0);
        }
        if self.is_3d {
            // Gentle idle spin (when enabled and not being dragged), so the depth
            // reads as 3D.
            if self.spin && st.drag == Drag::None {
                st.yaw += dt * 0.10;
            }
        } else {
            // Flat 2D: relax the depth toward the z=0 plane, so toggling over
            // from 3D collapses smoothly rather than snapping flat.
            let f = (1.0 - dt * 7.0).clamp(0.0, 1.0);
            for p in st.pos.iter_mut() {
                p[2] *= f;
            }
            for v in st.vel.iter_mut() {
                v[2] = 0.0;
            }
        }
        // Refit from a robust radius (a high percentile of node distances), so a
        // couple of far-flung outliers can't zoom the whole cluster into a tiny
        // knot. The radius is rotation-invariant, so the spin doesn't make the
        // framing pulse, and the origin always projects to the centre.
        let mut dists: Vec<f32> = st
            .pos
            .iter()
            .map(|p| (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt())
            .collect();
        dists.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = (dists.len().saturating_sub(1) as f32 * 0.9) as usize;
        let r = dists.get(idx).copied().unwrap_or(1.0).max(1.0);
        let persp0 = FOCAL / CAM;
        let fit = (bounds.width.min(bounds.height) - 2.0 * GRAPH_PAD) / (2.0 * r * persp0);
        st.fit_scale = fit.max(1e-4) * st.zoom;
        st.fit_off = (bounds.width * 0.5, bounds.height * 0.5);

        // --- Label opacities ------------------------------------------------
        // Compute each label's *target* opacity (depth fade × whether the
        // declutter keeps it), then ease the stored opacity toward it — so both
        // the depth fade and the discrete declutter flips resolve as smooth
        // cross-fades instead of pops as the graph rotates.
        if st.label_alpha.len() != n {
            st.label_alpha = vec![0.0; n];
        }
        let hovered = cursor
            .position_in(bounds)
            .and_then(|c| self.hit(c, bounds, st));
        // Project once (immutable borrow of `st`), before we mutate label_alpha.
        let lproj: Vec<(f32, f32, f32, f32)> =
            (0..n).map(|i| self.node_screen(i, bounds, st)).collect();
        let (mut minz, mut maxz) = (f32::MAX, f32::MIN);
        for p in &lproj {
            minz = minz.min(p.2);
            maxz = maxz.max(p.2);
        }
        let span = (maxz - minz).max(1.0);
        // Priority: hovered first, then nearest the camera.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            let ha = (hovered == Some(a)) as u8;
            let hb = (hovered == Some(b)) as u8;
            hb.cmp(&ha).then(
                lproj[b]
                    .2
                    .partial_cmp(&lproj[a].2)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        let mut placed: Vec<iced::Rectangle> = Vec::new();
        let mut target = vec![0.0f32; n];
        for &i in &order {
            let (x, y, z, persp) = lproj[i];
            let is_hover = hovered == Some(i);
            let fade = if is_hover || !self.is_3d {
                1.0
            } else {
                smoothstep(0.34, 0.82, (z - minz) / span)
            };
            if fade <= 0.006 {
                continue; // too deep to bother placing
            }
            let r =
                ((2.6 + self.layout.nodes[i].weight.sqrt() * 1.25) * (0.6 + 0.55 * persp)).max(1.2);
            let width = self.layout.nodes[i].label.chars().count() as f32 * 6.0 + 2.0;
            let flip = x + r + 3.0 + width > bounds.width - 2.0;
            let rect_x = if flip {
                x - r - 3.0 - width
            } else {
                x + r + 3.0
            };
            let rect = iced::Rectangle {
                x: rect_x,
                y: y - 6.5,
                width,
                height: 13.0,
            };
            if is_hover || !placed.iter().any(|pr| rects_overlap(*pr, rect)) {
                target[i] = fade;
                // Only a (near) fully-shown label reserves space, so a barely
                // there one can't hide a solid neighbour.
                if is_hover || fade > 0.6 {
                    placed.push(rect);
                }
            }
        }
        // Ease toward the target (time-based, ~0.12s to close most of the gap).
        let step = (dt * 9.0).min(1.0);
        for (la, t) in st.label_alpha.iter_mut().zip(&target) {
            *la += (t - *la) * step;
        }
        true
    }
}

/// Axis-aligned overlap test for label decluttering.
pub(crate) fn rects_overlap(a: iced::Rectangle, b: iced::Rectangle) -> bool {
    a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y
}

impl iced::widget::canvas::Program<Message> for GraphCanvas<'_> {
    type State = GraphState;

    fn draw(
        &self,
        state: &GraphState,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: iced::Rectangle,
        cursor: iced::advanced::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry> {
        use iced::widget::canvas::{Frame, Path, Stroke, Text};
        let mut frame = Frame::new(renderer, bounds.size());
        let n = self.layout.nodes.len();
        // Project every node once: (x, y, depth, perspective).
        let proj: Vec<(f32, f32, f32, f32)> =
            (0..n).map(|i| self.node_screen(i, bounds, state)).collect();
        let (mut minz, mut maxz) = (f32::MAX, f32::MIN);
        for p in &proj {
            minz = minz.min(p.2);
            maxz = maxz.max(p.2);
        }
        let span = (maxz - minz).max(1.0);
        let near = |z: f32| ((z - minz) / span).clamp(0.0, 1.0); // 0 far … 1 near
        let radius = |i: usize| -> f32 {
            let (_, _, _, persp) = proj[i];
            ((2.6 + self.layout.nodes[i].weight.sqrt() * 1.25) * (0.6 + 0.55 * persp)).max(1.2)
        };

        // Directed edges: a thin line plus a small arrowhead at the file being
        // imported (an arrow arriving at a node = it is imported; leaving = it
        // imports). Faded with depth like the rest of the scene.
        for &(a, b) in &self.layout.edges {
            let (ax, ay, az, _) = proj[a];
            let (bx, by, bz, _) = proj[b];
            let d = if self.is_3d {
                (near(az) + near(bz)) * 0.5
            } else {
                1.0
            };
            let (dx, dy) = (bx - ax, by - ay);
            let len = (dx * dx + dy * dy).sqrt();
            if len < 2.0 {
                continue;
            }
            let (ux, uy) = (dx / len, dy / len);
            let (nx, ny) = (-uy, ux); // perpendicular
            // From just off the importer to the imported node's near edge.
            let (sx, sy) = (ax + ux * (radius(a) + 1.0), ay + uy * (radius(a) + 1.0));
            let (tx, ty) = (bx - ux * (radius(b) + 1.0), by - uy * (radius(b) + 1.0));
            frame.stroke(
                &Path::line(iced::Point::new(sx, sy), iced::Point::new(tx, ty)),
                Stroke::default()
                    .with_width(0.6 + d * 0.4)
                    .with_color(theme::with_alpha(edge_ink(), 0.16 + 0.34 * d)),
            );
            // Small arrowhead at the imported end.
            let ah = 3.8; // length
            let aw = 1.6; // half-width
            let (cx, cy) = (tx - ux * ah, ty - uy * ah);
            let head = Path::new(|p| {
                p.move_to(iced::Point::new(tx, ty));
                p.line_to(iced::Point::new(cx + nx * aw, cy + ny * aw));
                p.line_to(iced::Point::new(cx - nx * aw, cy - ny * aw));
                p.close();
            });
            frame.fill(&head, theme::with_alpha(arrow_ink(), 0.26 + 0.44 * d));
        }

        let hovered = cursor
            .position_in(bounds)
            .and_then(|c| self.hit(c, bounds, state));

        // Nodes drawn far → near so nearer ones occlude; sized by perspective,
        // coloured by language + hierarchy depth. A file in an import cycle keeps
        // its hue and gains a gold ring.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            proj[a]
                .2
                .partial_cmp(&proj[b].2)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for &i in &order {
            let nd = &self.layout.nodes[i];
            let (x, y, _z, _) = proj[i];
            let r = radius(i);
            // Hue = language, paleness = hierarchy depth (stable across rotation).
            let base = lang_dot_color(crate::highlight::detect(&nd.file));
            let color = if hovered == Some(i) {
                theme::fg()
            } else {
                hier_shade(base, nd.depth)
            };
            frame.fill(&Path::circle(iced::Point::new(x, y), r), color);
            if nd.cyclic {
                frame.stroke(
                    &Path::circle(iced::Point::new(x, y), r + 1.5),
                    Stroke::default()
                        .with_width(1.3)
                        .with_color(theme::with_alpha(theme::warning(), 0.75)),
                );
            }
        }

        // Labels: just draw each at its eased opacity (`tick` already did the
        // depth fade + declutter into `label_alpha`), so they cross-fade in and
        // out smoothly as the graph rotates instead of popping.
        for i in 0..n {
            let la = state.label_alpha.get(i).copied().unwrap_or(0.0);
            if la < 0.01 {
                continue;
            }
            let (x, y, _z, _) = proj[i];
            let is_hover = hovered == Some(i);
            let nd = &self.layout.nodes[i];
            let r = radius(i);
            let color = if is_hover {
                theme::fg()
            } else {
                theme::fg_muted()
            };
            // Draw the label as a cached texture so it glides sub-pixel as the
            // map spins (canvas text snaps to the pixel grid → visible shake);
            // fall back to `fill_text` if no system font could be loaded.
            match super::graph_labels::label_texture(&nd.label, color) {
                Some(tex) => {
                    let flip = x + r + 3.0 + tex.width > bounds.width - 2.0;
                    let bx = if flip {
                        x - r - 3.0 - tex.width
                    } else {
                        x + r + 3.0
                    };
                    frame.draw_image(
                        iced::Rectangle::new(
                            iced::Point::new(bx, y - tex.height / 2.0),
                            iced::Size::new(tex.width, tex.height),
                        ),
                        iced::advanced::image::Image::new(tex.handle)
                            .filter_method(iced::advanced::image::FilterMethod::Linear)
                            .opacity(la.min(1.0)),
                    );
                }
                None => {
                    let width = nd.label.chars().count() as f32 * 6.0 + 2.0;
                    let flip = x + r + 3.0 + width > bounds.width - 2.0;
                    let (text_x, align_x) = if flip {
                        (x - r - 3.0, iced::alignment::Horizontal::Right)
                    } else {
                        (x + r + 3.0, iced::alignment::Horizontal::Left)
                    };
                    frame.fill_text(Text {
                        content: nd.label.clone(),
                        position: iced::Point::new(text_x, y),
                        color: theme::with_alpha(color, la),
                        size: 11.0.into(),
                        align_x: align_x.into(),
                        align_y: iced::alignment::Vertical::Center,
                        ..Text::default()
                    });
                }
            }
        }
        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        state: &mut GraphState,
        event: &iced::Event,
        bounds: iced::Rectangle,
        cursor: iced::advanced::mouse::Cursor,
    ) -> Option<iced::widget::canvas::Action<Message>> {
        use iced::mouse;
        use iced::widget::canvas::Action;
        match event {
            // Drive the live simulation: one physics step per frame, and keep
            // requesting frames until it cools (or a node is being dragged).
            iced::Event::Window(iced::window::Event::RedrawRequested(now)) => self
                .tick(state, bounds, *now, cursor)
                .then(|| Action::request_redraw()),
            // Zoom (dolly) — only where enabled; the embedded Overview map lets
            // the wheel fall through to the page.
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) if self.scroll_zooms => {
                cursor.position_in(bounds)?;
                let dy = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / 40.0,
                };
                state.zoom = (state.zoom * (1.0 + dy * 0.12)).clamp(0.3, 5.0);
                Some(Action::request_redraw().and_capture())
            }
            // Press: grab a node if one is under the cursor, else orbit the view.
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.position_in(bounds).is_some() =>
            {
                let cin = cursor.position_in(bounds)?;
                state.moved = 0.0;
                state.last_cursor = cursor.position().unwrap_or(cin);
                state.drag = match self.hit(cin, bounds, state) {
                    Some(i) => {
                        state.alpha = state.alpha.max(0.5);
                        Drag::Node(i)
                    }
                    None if self.is_3d => Drag::Orbit,
                    None => Drag::Pan,
                };
                Some(Action::request_redraw().and_capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.drag != Drag::None => {
                let abs = cursor.position().unwrap_or(state.last_cursor);
                let (dx, dy) = (abs.x - state.last_cursor.x, abs.y - state.last_cursor.y);
                state.moved += (dx * dx + dy * dy).sqrt();
                state.last_cursor = abs;
                match state.drag {
                    Drag::Orbit => {
                        state.yaw += dx * 0.008;
                        state.pitch = (state.pitch + dy * 0.008).clamp(-1.45, 1.45);
                    }
                    Drag::Pan => {
                        state.pan = iced::Vector::new(state.pan.x + dx, state.pan.y + dy);
                    }
                    Drag::Node(i) => {
                        if let Some(cin) = cursor.position_in(bounds)
                            && i < state.pos.len()
                        {
                            state.pos[i] = self.drag_node_to(i, cin, state);
                            state.vel[i] = [0.0; 3];
                            state.alpha = state.alpha.max(0.4);
                        }
                    }
                    Drag::None => {}
                }
                Some(Action::request_redraw().and_capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.drag != Drag::None =>
            {
                let was = state.drag;
                state.drag = Drag::None;
                // A grab that barely moved is a click → open the node.
                if let (Drag::Node(i), true) = (was, state.moved < 5.0) {
                    return Some(Action::publish(self.open_message(i)).and_capture());
                }
                // A moved node re-settles its neighbourhood.
                state.alpha = state.alpha.max(0.2);
                Some(Action::request_redraw().and_capture())
            }
            _ => None,
        }
    }

    fn mouse_interaction(
        &self,
        state: &GraphState,
        bounds: iced::Rectangle,
        cursor: iced::advanced::mouse::Cursor,
    ) -> iced::advanced::mouse::Interaction {
        use iced::advanced::mouse::Interaction;
        match state.drag {
            Drag::Orbit | Drag::Pan | Drag::Node(_) => return Interaction::Grabbing,
            Drag::None => {}
        }
        // A grab cursor over the map advertises that the view orbits and nodes
        // can be dragged (a plain click still opens a node).
        if cursor.is_over(bounds) {
            Interaction::Grab
        } else {
            Interaction::default()
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
                .color(theme::dim())
                .wrapping(Wrapping::None),
            space().width(Fill),
            text(format!("←{} →{}", g.fan_in(path), g.fan_out(path)))
                .size(10)
                .color(theme::dim())
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
                .color(theme::dim()),
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
        .color(theme::accent())
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
                        .color(theme::warning())
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
            container(text(externals.join("  ·  ")).size(11).color(theme::dim()))
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
                .color(theme::dim())
                .wrapping(Wrapping::None),
            space().width(Fill),
            text(trailing)
                .size(10)
                .color(theme::dim())
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
        return container(text(msg).size(12).color(theme::dim()))
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
        .color(theme::accent())
        .into(),
    );
    rows.push(
        text(if app.project_calls.precise {
            "LSP-precise — exact caller/callee edges."
        } else {
            "Name-based & approximate — Refine with LSP for exact edges."
        })
        .size(10)
        .color(theme::dim())
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
                    .color(theme::dim()),
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
