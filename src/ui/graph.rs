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
    graph_heat: bool,
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
    let mut header = row![
        text(title).size(ts::TITLE).color(theme::fg()),
        space().width(Fill)
    ]
    .spacing(4)
    .align_y(iced::Center);
    if let Some(extra) = extra {
        header = header.push(extra);
    }
    // Map mode: a spin start/stop (3D only), then a 2D/3D projection toggle. Each
    // icon shows the action / the mode it switches to.
    if graph_mode && graph_3d {
        header = header.push(if graph_spin {
            icon_btn(
                Glyph::Pause,
                "Stop spinning",
                Message::Graph(GraphMsg::ToggleSpin),
            )
        } else {
            icon_btn(Glyph::Play, "Spin", Message::Graph(GraphMsg::ToggleSpin))
        });
    }
    if graph_mode {
        header = header.push(if graph_3d {
            icon_btn(
                Glyph::Plane,
                "Flatten to 2D",
                Message::Graph(GraphMsg::Toggle3D),
            )
        } else {
            icon_btn(
                Glyph::Cube,
                "Show in 3D",
                Message::Graph(GraphMsg::Toggle3D),
            )
        });
        // Heat: colour by how often each file changed, instead of by language.
        header = header.push(chrome_tip(
            button(
                text(if graph_heat { "Heat ●" } else { "Heat" })
                    .size(ts::SMALL)
                    .color(if graph_heat {
                        theme::danger()
                    } else {
                        theme::fg_muted()
                    }),
            )
            .style(theme::toolbar_button)
            .padding([4, 8])
            .on_press(Message::Graph(GraphMsg::ToggleHeat)),
            if graph_heat {
                "Colour by language"
            } else {
                "Colour by change frequency (commits in the recent history)"
            },
            None,
        ));
    }
    header = header
        .push(if graph_mode {
            icon_btn(
                Glyph::List,
                "List view",
                Message::Graph(GraphMsg::OverlayViewToggle),
            )
        } else {
            icon_btn(
                Glyph::Graph,
                "Map view",
                Message::Graph(GraphMsg::OverlayViewToggle),
            )
        })
        .push(icon_btn(
            Glyph::Close,
            "Close",
            Message::Graph(GraphMsg::CloseOverlay),
        ));
    let panel = container(column![header, body].spacing(12))
        .width(GRAPH_MODAL_W)
        .max_height(GRAPH_MODAL_MAX_H)
        .padding(MODAL_PAD)
        .style(theme::modal_panel);

    modal(
        panel,
        Placement::Center,
        Backdrop::Dim(Some(Message::Graph(GraphMsg::CloseOverlay))),
    )
}

pub(crate) fn project_graph_modal(app: &App, overlay: crate::Overlay) -> Element<'_, Message> {
    let title = match overlay {
        crate::Overlay::ProjectImports => "Project Import Graph",
        crate::Overlay::ProjectCalls => "Project Call Graph",
    };
    // The call graph can be refined to exact LSP edges; show its control/status.
    let extra: Option<Element<'_, Message>> = match overlay {
        crate::Overlay::ProjectCalls => Some(if let Some(progress) = app.refine_progress_label() {
            text(progress)
                .size(ts::SMALL)
                .color(theme::warning())
                .into()
        } else if let Some(precise) = app.precise_label() {
            text(precise).size(ts::SMALL).color(theme::accent()).into()
        } else {
            button(text("Refine with LSP").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([3, 10])
                .on_press(Message::Graph(GraphMsg::RefineProjectCalls))
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
    graph_modal_frame(
        title,
        app.graph_mode,
        app.graph_3d,
        app.graph_spin,
        app.graph_heat,
        extra,
        body,
    )
}

/// The node-link map: a force-directed canvas plus a legend.
pub(crate) fn graph_map_view(app: &App) -> Element<'_, Message> {
    let overlay = app.proj.overlay;
    let hint = |msg: &str| {
        container(text(msg.to_string()).size(ts::BODY).color(theme::dim()))
            .padding(8)
            .width(Fill)
            .height(iced::Length::Fill)
            .into()
    };
    // While the project is still being scanned/indexed the graph is legitimately
    // empty — say so, rather than "Nothing to show" (which reads as "no data").
    let empty_msg = if app.proj.project_calls.building {
        "Building call graph…"
    } else if app.scanning || app.proj.indexing {
        "Indexing the project…"
    } else if app.proj.graph_layout_pending {
        // Laid out off the UI thread (`App::refresh_graph_layout`).
        "Laying out the map…"
    } else {
        "Nothing to show."
    };
    let Some(layout) = &app.proj.graph_layout else {
        return hint(empty_msg);
    };
    if layout.nodes.is_empty() {
        return hint(empty_msg);
    }
    let Some(kind) = overlay else {
        return hint(empty_msg);
    };
    let map = iced::widget::canvas::Canvas::new(
        GraphCanvas::new(
            layout,
            app.proj.graph_layout_rev,
            kind,
            true,
            app.graph_3d,
            app.graph_spin,
        )
        .with_heat(
            app.graph_heat
                .then_some(app.proj.churn.as_deref())
                .flatten(),
            app.proj.churn_rev,
        ),
    )
    .width(Fill)
    .height(Fill);
    let nav = map_nav_hint(app.graph_3d);
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
    column![map, text(legend).size(ts::CAPTION).color(theme::dim())]
        .spacing(6)
        .height(iced::Length::Fill)
        .into()
}

/// A distinct base colour per graphed language (see `theme::language_hue`).
pub(crate) fn lang_dot_color(lang: Option<&str>) -> iced::Color {
    theme::language_hue(lang)
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

/// Ease-in-out ramp from 0 (at/below `e0`) to 1 (at/above `e1`).
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The next layout revision (see [`GraphCanvas::rev`]): process-wide, so two
/// layouts — of two projects, two windows, the overlay and the overview map —
/// never share one.
pub(crate) fn next_layout_rev() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The node-link map. It drives its own redraws while it moves (the physics
/// cooling, a drag, the 3D idle spin): `tick` asks for the next frame only
/// then, and a settled map asks for none.
pub(crate) struct GraphCanvas<'a> {
    pub(crate) layout: &'a crate::graphlayout::Layout,
    /// The revision of `layout`'s content (see [`next_layout_rev`]), bumped
    /// wherever the app installs or edits one — a rebuild of the same shape
    /// with other edges, cycle rings re-marked in place. The node-set
    /// signature decides when to RESEED the simulation; this decides when
    /// the cached scene is stale, which a same-shaped change used to leave
    /// drawn with the old edges and rings until the pointer happened to
    /// hover a node.
    pub(crate) rev: u64,
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
    /// Heat: colour every node by how often its file changed (see
    /// [`crate::Churn::heat_of`]) instead of by language; `None` colours by
    /// language.
    pub(crate) heat: Option<&'a crate::Churn>,
    /// The generation of `heat`'s data (and whether heat is on): part of the
    /// scene cache's key, so a new history or a toggle repaints the map.
    pub(crate) paint_rev: u64,
}

impl<'a> GraphCanvas<'a> {
    pub(crate) fn new(
        layout: &'a crate::graphlayout::Layout,
        rev: u64,
        kind: crate::Overlay,
        scroll_zooms: bool,
        is_3d: bool,
        spin: bool,
    ) -> Self {
        Self {
            layout,
            rev,
            kind,
            scroll_zooms,
            is_3d,
            spin,
            heat: None,
            paint_rev: 0,
        }
    }

    /// Colour by change frequency: `heat` is the history to colour by (or
    /// `None` for language colours), `churn_rev` its generation.
    pub(crate) fn with_heat(mut self, heat: Option<&'a crate::Churn>, churn_rev: u64) -> Self {
        self.heat = heat;
        // Off is 0; on is the generation with a high bit, so a toggle at the
        // same generation still repaints.
        self.paint_rev = if heat.is_some() {
            churn_rev | (1 << 63)
        } else {
            0
        };
        self
    }
}

/// The colour of a node on the heat scale: the muted colour of unchanged
/// code, warming through the warning colour to the danger colour for the
/// most changed file.
pub(crate) fn heat_color(heat: f32) -> iced::Color {
    let t = heat.clamp(0.0, 1.0);
    if t <= 0.0 {
        theme::mix(theme::dim(), theme::fg_muted(), 0.5)
    } else if t < 0.5 {
        theme::mix(theme::fg_muted(), theme::warning(), t * 2.0)
    } else {
        theme::mix(theme::warning(), theme::danger(), (t - 0.5) * 2.0)
    }
}

/// Padding inside the canvas so node labels aren't clipped at the edges.
const GRAPH_PAD: f32 = 48.0;
/// Below this cooling factor the force simulation stops stepping.
const ALPHA_REST: f32 = 0.02;
/// A label opacity this close to its target counts as arrived.
const LABEL_EPS: f32 = 0.004;
/// How far (px) outside every disc a click may land and still pick the
/// nearest node, so tiny discs stay easy to hit.
const HIT_SLOP: f32 = 16.0;
/// Idle spin speed (radians per second, 3D only).
const SPIN_RATE: f32 = 0.10;

/// Which kind of drag is in progress on the map.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Drag {
    None,
    /// Orbiting the camera (3D, press began on empty space).
    Orbit,
    /// Panning the view (2D, press began on empty space).
    Pan,
    /// Moving a single node, pinned to the cursor while the rest reacts. The
    /// second field is the signature of the node set the index belongs to: a
    /// layout replaced mid-drag must not have its node `i` opened or moved.
    Node(usize, u64),
}

/// Camera distance and focal length for the perspective projection, in world
/// units. `CAM` sits behind the graph looking at the origin; both comfortably
/// exceed the graph's radius so no node crosses behind the camera.
const CAM: f32 = 2400.0;
const FOCAL: f32 = 2400.0;

/// Live 3D-simulation + orbit camera + interaction state, persisted by the
/// canvas widget across frames. The force sim and the camera both run here
/// (stepped each `RedrawRequested` while anything moves), so every graph
/// animates in 3D, can spin, and its nodes can be grabbed and moved.
/// What the cached scene was drawn for: the hovered node, the theme's
/// revision, the layout's revision and the paint's (heat) revision.
type SceneKey = (Option<usize>, u64, u64, u64);

pub(crate) struct GraphState {
    /// Node positions/velocities in 3D world space. Empty until seeded for the
    /// current node set; a rebuilt graph (new `sig`) reseeds.
    pos: Vec<crate::graphlayout::V3>,
    vel: Vec<crate::graphlayout::V3>,
    sig: u64,
    /// The layout revision (`GraphCanvas::rev`) the last tick ran on.
    rev: u64,
    /// Cooling factor for the physics; decays over time so the layout settles.
    alpha: f32,
    last_frame: Option<std::time::Instant>,
    /// Orbit camera: yaw (around Y), pitch (around X), and a zoom multiplier.
    yaw: f32,
    pitch: f32,
    zoom: f32,
    /// Projection fit, recomputed from the graph's radius (which is
    /// rotation-invariant, so spinning doesn't make it pulse): world → screen is
    /// `raw2d * fit_scale + fit_off`.
    fit_scale: f32,
    fit_off: (f32, f32),
    /// Extra pan offset, used only in 2D mode (3D re-centres on the origin).
    pan: iced::Vector,
    /// Whether the previous frame rendered in 3D, so a 2D → 3D switch can
    /// re-inflate the depth (2D collapses it flat).
    was_3d: bool,
    /// Per-node label opacity, eased each frame toward `label_target` (depth
    /// fade × whether decluttering keeps it). Animating this — rather than
    /// deciding draw/skip per frame — means labels cross-fade instead of
    /// popping as the graph rotates. Empty until seeded for the current nodes.
    label_alpha: Vec<f32>,
    label_target: Vec<f32>,
    /// Per-node label width as drawn (see `graph_labels::label_width`),
    /// measured once per node set.
    label_w: Vec<f32>,
    drag: Drag,
    /// Distance dragged since press — a tiny total means "click", not a drag.
    moved: f32,
    /// Last absolute cursor position while dragging.
    last_cursor: iced::Point,
    /// The idle spin was running at the last tick.
    spinning: bool,
    /// A 3D → 2D switch is still relaxing depth toward the plane.
    flattening: bool,
    /// Something changed the picture since the last tick (camera, zoom, pan,
    /// a dragged node), so the next tick must recompute.
    dirty: bool,
    /// The bounds and hovered node the last tick computed for.
    last_bounds: iced::Size,
    last_hover: Option<usize>,
    /// The scene minus the hovered node, re-tessellated only when it changes.
    scene: iced::widget::canvas::Cache,
    /// What `scene` was drawn for: the hovered node, the theme revision and
    /// the layout revision.
    scene_key: std::cell::Cell<Option<SceneKey>>,
}

impl Default for GraphState {
    fn default() -> Self {
        GraphState {
            pos: Vec::new(),
            vel: Vec::new(),
            sig: 0,
            rev: 0,
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
            label_target: Vec::new(),
            label_w: Vec::new(),
            drag: Drag::None,
            moved: 0.0,
            last_cursor: iced::Point::new(0.0, 0.0),
            spinning: false,
            flattening: false,
            dirty: true,
            last_bounds: iced::Size::ZERO,
            last_hover: None,
            scene: iced::widget::canvas::Cache::new(),
            scene_key: std::cell::Cell::new(None),
        }
    }
}

impl GraphState {
    /// Mark the picture changed: the next tick recomputes and the cached
    /// scene is redrawn.
    fn touch(&mut self) {
        self.dirty = true;
        self.scene.clear();
    }
}

/// Whether the map has come to rest: nothing is being dragged, the physics
/// has cooled, the idle spin is off, a 2D flattening has finished and every
/// label has faded to where it is going. While this is false the canvas keeps
/// requesting frames; once it is true it stops, and a settled map costs
/// nothing until the next interaction.
pub(crate) fn graph_settled(st: &GraphState) -> bool {
    st.drag == Drag::None
        && st.alpha <= ALPHA_REST
        && !st.spinning
        && !st.flattening
        && st.label_alpha.len() == st.label_target.len()
        && st
            .label_alpha
            .iter()
            .zip(&st.label_target)
            .all(|(a, t)| (a - t).abs() <= LABEL_EPS)
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

/// On-screen radius of a node of `weight` at perspective factor `persp`.
fn node_radius(weight: f32, persp: f32) -> f32 {
    ((2.6 + weight.sqrt() * 1.25) * (0.6 + 0.55 * persp)).max(1.2)
}

/// A node as drawn, for hit-testing: its disc and its place in the draw order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Disc {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) r: f32,
    /// Camera depth (larger = nearer); discs are drawn far → near.
    pub(crate) depth: f32,
    pub(crate) index: usize,
}

/// Draw order: by depth, far first; equal depths (all of them, in 2D) in
/// index order. Later in this order = painted on top.
fn draw_order(a: &Disc, b: &Disc) -> std::cmp::Ordering {
    a.depth
        .partial_cmp(&b.depth)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(a.index.cmp(&b.index))
}

/// The node a click at `cursor` means: of the discs under it, the one painted
/// on top; with none under it, the nearest within [`HIT_SLOP`] of its edge
/// (on a tie, again the one on top).
///
/// The old test took the node nearest the *camera* within 22 px of the cursor,
/// never the one nearest the cursor; in 2D, where every depth is equal, that
/// was the lowest index while drawing puts the highest on top — so a click
/// opened the node underneath the one you saw.
pub(crate) fn pick_node(cursor: iced::Point, discs: &[Disc]) -> Option<usize> {
    let dist = |d: &Disc| ((d.x - cursor.x).powi(2) + (d.y - cursor.y).powi(2)).sqrt();
    if let Some(top) = discs
        .iter()
        .filter(|d| dist(d) <= d.r)
        .max_by(|a, b| draw_order(a, b))
    {
        return Some(top.index);
    }
    discs
        .iter()
        .filter(|d| dist(d) <= d.r + HIT_SLOP)
        .min_by(|a, b| {
            (dist(a) - a.r)
                .partial_cmp(&(dist(b) - b.r))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(draw_order(b, a))
        })
        .map(|d| d.index)
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

    /// Every node's projection, once.
    fn project_all(&self, bounds: iced::Rectangle, st: &GraphState) -> Vec<(f32, f32, f32, f32)> {
        (0..self.layout.nodes.len())
            .map(|i| self.node_screen(i, bounds, st))
            .collect()
    }

    /// The node under `cursor` (see [`pick_node`]).
    fn hit(&self, cursor: iced::Point, bounds: iced::Rectangle, st: &GraphState) -> Option<usize> {
        let discs: Vec<Disc> = self
            .project_all(bounds, st)
            .into_iter()
            .enumerate()
            .map(|(index, (x, y, depth, persp))| Disc {
                x,
                y,
                r: node_radius(self.layout.nodes[index].weight, persp),
                depth,
                index,
            })
            .collect();
        pick_node(cursor, &discs)
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

    /// The message that opens node `i`, if it is still part of this layout.
    fn open_message(&self, i: usize) -> Option<Message> {
        let file = self.layout.nodes.get(i)?.file.clone();
        Some(match self.kind {
            crate::Overlay::ProjectImports => Message::Graph(GraphMsg::OverlayOpenImports(file)),
            crate::Overlay::ProjectCalls => {
                Message::Graph(GraphMsg::OverlayOpenAt { abs: file, line: 1 })
            }
        })
    }

    /// Seed the simulation for the current node set, lifted into 3D with a
    /// deterministic z spread so the graph opens as a volume rather than a
    /// flat sheet. Any drag in progress belonged to the old node set and ends.
    fn reseed(&self, st: &mut GraphState) {
        use crate::graphlayout::WORLD;
        let n = self.layout.nodes.len();
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
        st.label_alpha = vec![0.0; n];
        st.label_target = vec![0.0; n];
        st.label_w = self
            .layout
            .nodes
            .iter()
            .map(|nd| super::graph_labels::label_width(&nd.label))
            .collect();
        st.alpha = 1.0;
        st.last_frame = None;
        st.sig = layout_sig(self.layout);
        st.drag = Drag::None;
        st.moved = 0.0;
        st.touch();
    }

    /// Advance the physics, camera and label fades by one frame, when anything
    /// is moving, and refit. Returns whether the map is still in motion (and so
    /// wants another frame) — `false` once it has settled, so a map at rest
    /// stops redrawing. Reseeds if the node set changed.
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
            self.reseed(st);
        } else if st.rev != self.rev {
            // The same nodes, other content (edges rebuilt, cycle rings
            // re-marked): redraw, and let the simulation relax onto the new
            // edges instead of keeping the old ones' equilibrium.
            st.alpha = st.alpha.max(0.3);
            st.touch();
        }
        st.rev = self.rev;
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
            st.touch();
        }
        st.was_3d = self.is_3d;

        let pinned = match st.drag {
            Drag::Node(i, sig) if sig == st.sig && i < n => Some(i),
            _ => None,
        };
        if pinned.is_some() {
            st.alpha = st.alpha.max(0.3);
        }
        st.spinning = self.is_3d && self.spin && st.drag == Drag::None;
        st.flattening = !self.is_3d && st.pos.iter().any(|p| p[2].abs() > 0.5);
        let hovered = cursor
            .position_in(bounds)
            .and_then(|c| self.hit(c, bounds, st));
        let physics = pinned.is_some() || st.alpha > ALPHA_REST;
        let changed = st.dirty
            || physics
            || st.spinning
            || st.flattening
            || bounds.size() != st.last_bounds
            || hovered != st.last_hover;
        if !changed && graph_settled(st) {
            return false;
        }
        st.dirty = false;
        st.last_bounds = bounds.size();
        st.last_hover = hovered;
        st.scene.clear();

        // Step the physics while warm or grabbing; skip the O(n²) once cooled.
        if physics {
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
            // Per unit of time, not per frame (a 120 Hz display used to cool
            // twice as fast as a 60 Hz one).
            st.alpha = crate::graphlayout::cool(st.alpha, dt * 4.0);
        }
        if st.spinning {
            // Gentle idle spin, so the depth reads as 3D.
            st.yaw += dt * SPIN_RATE;
        }
        if !self.is_3d {
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
        dists.sort_by(f32::total_cmp);
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
        let lproj = self.project_all(bounds, st);
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
            hb.cmp(&ha).then(lproj[b].2.total_cmp(&lproj[a].2))
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
            let r = node_radius(self.layout.nodes[i].weight, persp);
            // The width the label is actually drawn at, not a per-char guess.
            let width = st.label_w.get(i).copied().unwrap_or(0.0);
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
            if (*la - t).abs() <= LABEL_EPS {
                *la = *t;
            }
        }
        st.label_target = target;
        !graph_settled(st)
    }

    /// Draw the scene: edges, then nodes far → near, then labels — all but the
    /// `hovered` node and its label, which the caller draws on top.
    fn draw_scene(
        &self,
        frame: &mut iced::widget::canvas::Frame,
        st: &GraphState,
        bounds: iced::Rectangle,
        proj: &[(f32, f32, f32, f32)],
        hovered: Option<usize>,
    ) {
        use iced::widget::canvas::{Path, Stroke};
        let (mut minz, mut maxz) = (f32::MAX, f32::MIN);
        for p in proj {
            minz = minz.min(p.2);
            maxz = maxz.max(p.2);
        }
        let span = (maxz - minz).max(1.0);
        let near = |z: f32| ((z - minz) / span).clamp(0.0, 1.0); // 0 far … 1 near
        let radius = |i: usize| node_radius(self.layout.nodes[i].weight, proj[i].3);

        // Directed edges: a thin line plus a small arrowhead at the file being
        // imported (an arrow arriving at a node = it is imported; leaving = it
        // imports). Faded with depth like the rest of the scene.
        for &(a, b) in &self.layout.edges {
            let (Some(&(ax, ay, az, _)), Some(&(bx, by, bz, _))) = (proj.get(a), proj.get(b))
            else {
                continue;
            };
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
                    .with_color(theme::with_alpha(theme::graph_edge(), 0.16 + 0.34 * d)),
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
            frame.fill(
                &head,
                theme::with_alpha(theme::graph_arrow(), 0.26 + 0.44 * d),
            );
        }

        // Nodes drawn far → near so nearer ones occlude; sized by perspective,
        // coloured by language + hierarchy depth. A file in an import cycle keeps
        // its hue and gains a gold ring.
        let mut order: Vec<usize> = (0..proj.len()).collect();
        order.sort_by(|&a, &b| proj[a].2.total_cmp(&proj[b].2));
        for &i in order.iter().filter(|&&i| Some(i) != hovered) {
            self.draw_node(frame, i, proj[i], radius(i), false);
        }

        // Labels: just draw each at its eased opacity (`tick` already did the
        // depth fade + declutter into `label_alpha`), so they cross-fade in and
        // out smoothly as the graph rotates instead of popping.
        for (i, &p) in proj.iter().enumerate() {
            if Some(i) == hovered {
                continue;
            }
            let la = st.label_alpha.get(i).copied().unwrap_or(0.0);
            if la >= 0.01 {
                self.draw_label(frame, bounds, i, p, radius(i), la, false);
            }
        }
    }

    /// One node disc (and its cycle ring), in the hover color when `hover`.
    fn draw_node(
        &self,
        frame: &mut iced::widget::canvas::Frame,
        i: usize,
        (x, y, _, _): (f32, f32, f32, f32),
        r: f32,
        hover: bool,
    ) {
        use iced::widget::canvas::{Path, Stroke};
        let nd = &self.layout.nodes[i];
        // Hue = language, paleness = hierarchy depth (stable across rotation).
        let color = if hover {
            theme::fg()
        } else if let Some(churn) = self.heat {
            heat_color(churn.heat_of(&nd.file))
        } else {
            hier_shade(lang_dot_color(crate::highlight::detect(&nd.file)), nd.depth)
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

    /// One node label at opacity `alpha`, flipped to the node's left when it
    /// would run off the right edge.
    #[allow(clippy::too_many_arguments)]
    fn draw_label(
        &self,
        frame: &mut iced::widget::canvas::Frame,
        bounds: iced::Rectangle,
        i: usize,
        (x, y, _, _): (f32, f32, f32, f32),
        r: f32,
        alpha: f32,
        hover: bool,
    ) {
        use iced::widget::canvas::Text;
        let nd = &self.layout.nodes[i];
        let color = if hover {
            theme::fg()
        } else {
            theme::fg_muted()
        };
        // Draw the label as a cached texture so it glides sub-pixel as the
        // map spins (canvas text snaps to the pixel grid → visible shake);
        // fall back to `fill_text` for labels the texture font cannot draw.
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
                        .opacity(alpha.min(1.0)),
                );
            }
            None => {
                let width = super::graph_labels::label_width(&nd.label);
                let flip = x + r + 3.0 + width > bounds.width - 2.0;
                let (text_x, align_x) = if flip {
                    (x - r - 3.0, iced::alignment::Horizontal::Right)
                } else {
                    (x + r + 3.0, iced::alignment::Horizontal::Left)
                };
                frame.fill_text(Text {
                    content: nd.label.clone(),
                    position: iced::Point::new(text_x, y),
                    color: theme::with_alpha(color, alpha.min(1.0)),
                    size: super::graph_labels::SIZE.into(),
                    align_x: align_x.into(),
                    align_y: iced::alignment::Vertical::Center,
                    ..Text::default()
                });
            }
        }
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
        use iced::widget::canvas::Frame;
        let proj = self.project_all(bounds, state);
        let hovered = cursor
            .position_in(bounds)
            .and_then(|c| self.hit(c, bounds, state));
        // The scene is cached: a settled map redraws (another widget animating,
        // the cursor moving elsewhere) without re-tessellating a single edge.
        // `tick` and every interaction clear it; so does a change of the hovered
        // node, of the theme or of the layout's content, which it is drawn for.
        let key = (hovered, theme::revision(), self.rev, self.paint_rev);
        if state.scene_key.get() != Some(key) {
            state.scene.clear();
            state.scene_key.set(Some(key));
        }
        let scene = state.scene.draw(renderer, bounds.size(), |frame| {
            self.draw_scene(frame, state, bounds, &proj, hovered);
        });
        // The hovered node and its label, on top.
        let mut top = Frame::new(renderer, bounds.size());
        if let Some(i) = hovered.filter(|&i| i < proj.len()) {
            let r = node_radius(self.layout.nodes[i].weight, proj[i].3);
            self.draw_node(&mut top, i, proj[i], r, true);
            let la = state.label_alpha.get(i).copied().unwrap_or(1.0);
            self.draw_label(&mut top, bounds, i, proj[i], r, la.max(0.01), true);
        }
        vec![scene, top.into_geometry()]
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
            // Drive the live simulation: one step per frame while anything
            // moves, requesting the next frame only then — a settled map stops
            // asking.
            iced::Event::Window(iced::window::Event::RedrawRequested(now)) => self
                .tick(state, bounds, *now, cursor)
                .then(Action::request_redraw),
            // A hover change re-ranks the labels (the hovered one always
            // shows), so let the next frame recompute.
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.drag == Drag::None => {
                let hovered = cursor
                    .position_in(bounds)
                    .and_then(|c| self.hit(c, bounds, state));
                (hovered != state.last_hover).then(Action::request_redraw)
            }
            // Zoom (dolly) — only where enabled; the embedded Overview map lets
            // the wheel fall through to the page.
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) if self.scroll_zooms => {
                cursor.position_in(bounds)?;
                let dy = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / 40.0,
                };
                state.zoom = (state.zoom * (1.0 + dy * 0.12)).clamp(0.3, 5.0);
                state.touch();
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
                        Drag::Node(i, state.sig)
                    }
                    None if self.is_3d => Drag::Orbit,
                    None => Drag::Pan,
                };
                state.touch();
                Some(Action::request_redraw().and_capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
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
                    Drag::Node(i, sig) => {
                        if let Some(cin) = cursor.position_in(bounds)
                            && sig == state.sig
                            && i < state.pos.len()
                        {
                            state.pos[i] = self.drag_node_to(i, cin, state);
                            state.vel[i] = [0.0; 3];
                            state.alpha = state.alpha.max(0.4);
                        }
                    }
                    Drag::None => {}
                }
                state.touch();
                Some(Action::request_redraw().and_capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.drag != Drag::None =>
            {
                let was = state.drag;
                state.drag = Drag::None;
                state.touch();
                // A grab that barely moved is a click → open the node — if it
                // still belongs to the layout on screen.
                if let Drag::Node(i, sig) = was
                    && state.moved < 5.0
                    && sig == state.sig
                    && sig == layout_sig(self.layout)
                    && let Some(msg) = self.open_message(i)
                {
                    return Some(Action::publish(msg).and_capture());
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
        if state.drag != Drag::None {
            return Interaction::Grabbing;
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
pub(crate) fn import_file_row<'a>(app: &'a App, file: &RankedFile) -> Element<'a, Message> {
    let path = file.path.as_path();
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let dir = std::path::Path::new(&rel_of(app, path))
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".into());
    button(
        row![
            text(name.to_string())
                .size(ts::BODY)
                .wrapping(Wrapping::None),
            space().width(6),
            text(dir)
                .size(ts::CAPTION)
                .color(theme::dim())
                .wrapping(Wrapping::None),
            space().width(Fill),
            text(format!("←{} →{}", file.fan_in, file.fan_out))
                .size(ts::CAPTION)
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
    .on_press(Message::Graph(GraphMsg::OverlayOpenImports(
        path.to_path_buf(),
    )))
    .into()
}

/// What dragging the map's background does, as its caption says it: a 3D
/// map orbits, a 2D one pans. One wording for every map (the overview's
/// caption said "orbit" in 2D).
pub(crate) fn map_nav_hint(graph_3d: bool) -> &'static str {
    if graph_3d {
        "drag to orbit"
    } else {
        "drag to pan"
    }
}

pub(crate) fn project_imports_body(app: &App) -> Element<'_, Message> {
    let g = &app.proj.import_graph;
    if g.is_empty() {
        // An empty graph while the project is still being read — or its
        // imports resolved, which the import job does after the index — is
        // not a finding: say it is being built rather than "nothing found".
        let work = &app.proj.import_work;
        let msg = if app.scanning || app.proj.indexing {
            "Indexing the project…"
        } else if work.running || !work.pending.is_empty() {
            "Resolving imports…"
        } else {
            "No imports found in this project."
        };
        return container(text(msg).size(ts::BODY).color(theme::dim()))
            .padding(8)
            .into();
    }
    // Counting, collecting and ranking walk the whole graph: done by the
    // import job, off this thread, when the graph's structure changed.
    let ranks = &app.proj.import_ranks;

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(
        text(format!(
            "{} files · {} internal edges · {} external packages · {} cycles",
            ranks.files,
            ranks.internal_edges,
            ranks.externals.len(),
            app.proj.import_cycles.len(),
        ))
        .size(ts::BODY)
        .color(theme::accent())
        .into(),
    );

    // Cycles — a real structural smell worth surfacing first.
    if !app.proj.import_cycles.is_empty() {
        rows.push(section_header("IMPORT CYCLES"));
        for cycle in &app.proj.import_cycles {
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
                        .size(ts::SMALL)
                        .color(theme::warning())
                        .wrapping(Wrapping::None),
                )
                .padding([2, 8])
                .into(),
            );
        }
    }

    // Most depended-on (highest fan-in) — the architectural hubs.
    rows.push(section_header("MOST DEPENDED-ON (fan-in)"));
    for file in &ranks.by_fan_in {
        rows.push(import_file_row(app, file));
    }

    // Most dependencies (highest fan-out).
    rows.push(section_header("MOST DEPENDENCIES (fan-out)"));
    for file in &ranks.by_fan_out {
        rows.push(import_file_row(app, file));
    }

    rows.extend(churn_rows(app));

    // External packages the project pulls in.
    if !ranks.externals.is_empty() {
        rows.push(section_header("EXTERNAL PACKAGES"));
        rows.push(
            container(
                text(ranks.externals.join("  ·  "))
                    .size(ts::SMALL)
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

/// The import overlay's numbers for graph `g`: its file and edge counts, its
/// external packages, and the top 12 files by fan-in and by fan-out (ties by
/// path). Each count is taken once per file (a fan-out count builds a set),
/// not once per comparison.
pub(crate) fn import_ranks(g: &crate::imports::ImportGraph) -> ImportRanks {
    const TOP: usize = 12;
    let counted: Vec<RankedFile> = g
        .files()
        .into_iter()
        .map(|path| RankedFile {
            fan_in: g.fan_in(&path),
            fan_out: g.fan_out(&path),
            path,
        })
        .collect();
    let top = |count: fn(&RankedFile) -> usize| {
        let mut ranked: Vec<RankedFile> =
            counted.iter().filter(|f| count(f) > 0).cloned().collect();
        ranked.sort_unstable_by(|a, b| count(b).cmp(&count(a)).then_with(|| a.path.cmp(&b.path)));
        ranked.truncate(TOP);
        ranked
    };
    ImportRanks {
        files: counted.len(),
        internal_edges: g.internal_edge_count(),
        externals: g.external_packages(),
        by_fan_in: top(|f| f.fan_in),
        by_fan_out: top(|f| f.fan_out),
    }
}

/// A symbol row in the call overlay: name + file:line + a trailing count.
pub(crate) fn call_symbol_row<'a>(
    app: &'a App,
    id: usize,
    trailing: String,
) -> Element<'a, Message> {
    let n = app.proj.project_calls.graph.node(id);
    button(
        row![
            text(n.name.clone()).size(ts::BODY).wrapping(Wrapping::None),
            space().width(6),
            text(format!("{}:{}", rel_of(app, &n.file), n.line))
                .size(ts::CAPTION)
                .color(theme::dim())
                .wrapping(Wrapping::None),
            space().width(Fill),
            text(trailing)
                .size(ts::CAPTION)
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
    .on_press(Message::Graph(GraphMsg::OverlayOpenAt {
        abs: n.file.clone(),
        line: n.line,
    }))
    .into()
}

pub(crate) fn project_calls_body(app: &App) -> Element<'_, Message> {
    let g = &app.proj.project_calls.graph;
    if g.is_empty() {
        // Empty while a build runs, or while the project is still being read
        // (the graph is linked from its symbol index), is not a finding: only
        // a build over the whole index says there are no functions.
        let msg = if app.proj.project_calls.building {
            "Building call graph…"
        } else if app.scanning || app.proj.indexing {
            "Indexing the project…"
        } else {
            "No functions found in this project."
        };
        return container(text(msg).size(ts::BODY).color(theme::dim()))
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
        .size(ts::BODY)
        .color(theme::accent())
        .into(),
    );
    rows.push(
        text(app.precise_summary().unwrap_or_else(|| {
            "Name-based & approximate — Refine with LSP for exact edges.".to_string()
        }))
        .size(ts::CAPTION)
        .color(theme::dim())
        .into(),
    );

    // Ranking walks the whole graph (and, for the test filter, the symbol
    // index): memoized per graph and index generation instead of redone per
    // repaint.
    let summary = app.proj.view_memo.calls_summary.get_or(
        (app.proj.project_calls.graph_rev, app.proj.symbol_index_rev),
        || calls_summary(app, g),
    );

    // Most-called functions (hubs), unique names only so the counts mean something.
    if !summary.hubs.is_empty() {
        rows.push(section_header("MOST CALLED (unique names)"));
        for &(id, callers) in &summary.hubs {
            rows.push(call_symbol_row(app, id, format!("{callers} callers")));
        }
    }

    if !summary.entries.is_empty() {
        rows.push(section_header(
            "ENTRY POINTS (main, routes, commands, handlers)",
        ));
        for &(id, kind) in summary.entries.iter().take(40) {
            rows.push(call_symbol_row(app, id, kind.to_string()));
        }
        if summary.entries.len() > 40 {
            rows.push(
                container(
                    text(format!("… and {} more", summary.entries.len() - 40))
                        .size(ts::CAPTION)
                        .color(theme::dim()),
                )
                .padding([2, 8])
                .into(),
            );
        }
    }

    rows.extend(churn_rows(app));

    let uncalled = &summary.uncalled;
    rows.push(section_header("UNCALLED (possibly dead)"));
    for &(id, out) in uncalled.iter().take(60) {
        rows.push(call_symbol_row(app, id, format!("→{out}")));
    }
    if uncalled.len() > 60 {
        rows.push(
            container(
                text(format!("… and {} more", uncalled.len() - 60))
                    .size(ts::CAPTION)
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

/// The project-calls overlay's rankings for graph `g`: the 15 most-called
/// functions with their caller counts, and the uncalled ones with their
/// callee counts. Test functions are left out of the uncalled list — they are
/// always "uncalled" (the harness invokes them, not project code), so they
/// would swamp it as false positives.
pub(crate) fn calls_summary(app: &App, g: &crate::projectcalls::ProjectCallGraph) -> CallsSummary {
    let is_test_node = |id: usize| {
        let n = g.node(id);
        app.proj
            .symbol_index_by_file
            .get(&n.file)
            .and_then(|syms| syms.iter().find(|s| s.name == n.name && s.line == n.line))
            .map(|s| s.is_test)
            .unwrap_or(false)
    };
    let entry_of = |id: usize| {
        let n = g.node(id);
        app.entry_kind_of(&n.file, &n.name)
    };
    let mut entries: Vec<(usize, crate::index::EntryKind)> = (0..g.node_count())
        .filter_map(|id| entry_of(id).map(|kind| (id, kind)))
        .collect();
    entries.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| g.node(a.0).name.cmp(&g.node(b.0).name))
            .then_with(|| g.node(a.0).line.cmp(&g.node(b.0).line))
    });
    CallsSummary {
        hubs: g
            .most_called(15)
            .into_iter()
            .map(|id| (id, g.node(id).caller_count()))
            .collect(),
        uncalled: g
            .uncalled()
            .into_iter()
            .filter(|&id| !is_test_node(id) && entry_of(id).is_none())
            .map(|id| (id, g.node(id).callee_count()))
            .collect(),
        entries: entries
            .into_iter()
            .map(|(id, kind)| (id, kind.label()))
            .collect(),
    }
}

/// The MOST CHANGED section of an overlay's list: the files with the most
/// commits over the recent history, each with its count and how long ago it
/// last changed, opening the file; a note while the history loads; nothing
/// for a project without git.
pub(crate) fn churn_rows(app: &App) -> Vec<Element<'_, Message>> {
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let Some(churn) = app.proj.churn.as_deref() else {
        if app.proj.churn_loading {
            rows.push(section_header("MOST CHANGED"));
            rows.push(
                container(
                    text("Reading the change history…")
                        .size(ts::CAPTION)
                        .color(theme::dim()),
                )
                .padding([2, 8])
                .into(),
            );
        }
        return rows;
    };
    if churn.top.is_empty() {
        return rows;
    }
    rows.push(section_header(&format!(
        "MOST CHANGED (last {} commits)",
        churn.commits
    )));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let root = app.proj.project.as_ref().map(|p| p.root.clone());
    for f in churn.top.iter().take(CHURN_ROWS) {
        let path = std::path::Path::new(&f.rel);
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or(&f.rel);
        let dir = path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty());
        let mut line = row![text(name.to_string()).size(ts::SMALL).color(theme::fg()),].spacing(6);
        if let Some(dir) = dir {
            line = line.push(text(dir).size(ts::CAPTION).color(theme::dim()));
        }
        line = line.push(space().width(Fill)).push(
            text(format!(
                "{} {} · {}",
                f.commits,
                if f.commits == 1 { "commit" } else { "commits" },
                crate::git::relative_time(f.last, now)
            ))
            .size(ts::CAPTION)
            .color(heat_color(
                churn.heat_of(
                    &root
                        .as_ref()
                        .map_or_else(|| path.to_path_buf(), |r| r.join(path)),
                ),
            )),
        );
        let abs = root
            .as_ref()
            .map_or_else(|| path.to_path_buf(), |r| r.join(path));
        rows.push(
            button(line)
                .style(theme::list_row(false))
                .width(Fill)
                .padding([3, 8])
                .on_press(Message::Graph(GraphMsg::OverlayOpenAt { abs, line: 1 }))
                .into(),
        );
    }
    rows
}

/// Files the MOST CHANGED section lists at most.
pub(crate) const CHURN_ROWS: usize = 12;

// -------------------------------------------------- explanation overlay

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphlayout::{LNode, Layout};
    use iced::Point;

    fn disc(index: usize, x: f32, y: f32, r: f32, depth: f32) -> Disc {
        Disc {
            x,
            y,
            r,
            depth,
            index,
        }
    }

    /// E2-4: of two discs under the pointer, the one painted on top wins. In
    /// 2D every depth is equal and later nodes are painted over earlier ones;
    /// the old test kept the FIRST index, i.e. the node underneath.
    #[test]
    fn a_click_on_overlapping_discs_picks_the_one_on_top() {
        let flat = [
            disc(0, 100.0, 100.0, 8.0, 0.0),
            disc(1, 104.0, 100.0, 8.0, 0.0),
        ];
        assert_eq!(pick_node(Point::new(102.0, 100.0), &flat), Some(1));
        // In 3D the nearer disc is painted last, whatever its index.
        let deep = [
            disc(0, 100.0, 100.0, 8.0, 50.0),
            disc(1, 104.0, 100.0, 8.0, -50.0),
        ];
        assert_eq!(pick_node(Point::new(102.0, 100.0), &deep), Some(0));
    }

    /// E2-4: off every disc, the nearest one to the pointer wins — not the
    /// one nearest the camera, which is what the old test preferred.
    #[test]
    fn a_click_between_discs_picks_the_nearest_to_the_pointer() {
        let discs = [
            // Near the camera but farther from the pointer.
            disc(0, 100.0, 100.0, 3.0, 400.0),
            // Farther back but closest to the pointer.
            disc(1, 116.0, 100.0, 3.0, -400.0),
        ];
        assert_eq!(pick_node(Point::new(110.0, 100.0), &discs), Some(1));
    }

    #[test]
    fn a_click_far_from_every_disc_picks_nothing() {
        let discs = [disc(0, 100.0, 100.0, 5.0, 0.0)];
        assert_eq!(
            pick_node(Point::new(100.0 + 5.0 + HIT_SLOP + 1.0, 100.0), &discs),
            None
        );
        assert_eq!(
            pick_node(Point::new(100.0 + 5.0 + HIT_SLOP - 1.0, 100.0), &discs),
            Some(0)
        );
        assert_eq!(pick_node(Point::new(0.0, 0.0), &[]), None);
    }

    fn layout(n: usize) -> Layout {
        Layout {
            nodes: (0..n)
                .map(|i| LNode {
                    label: format!("file_{i}.rs"),
                    file: std::path::PathBuf::from(format!("/p/file_{i}.rs")),
                    x: (i as f32 * 0.37) % 1.0,
                    y: (i as f32 * 0.61) % 1.0,
                    weight: 1.0 + i as f32,
                    cyclic: false,
                    depth: 0.5,
                })
                .collect(),
            edges: (1..n).map(|i| (i - 1, i)).collect(),
            total: n,
        }
    }

    fn canvas(layout: &Layout, is_3d: bool, spin: bool) -> GraphCanvas<'_> {
        GraphCanvas::new(layout, 1, crate::Overlay::ProjectImports, true, is_3d, spin)
    }

    const BOUNDS: iced::Rectangle = iced::Rectangle {
        x: 0.0,
        y: 0.0,
        width: 700.0,
        height: 500.0,
    };

    /// Tick at 60 fps until the map reports rest; the frames it took.
    fn frames_to_rest(c: &GraphCanvas<'_>, st: &mut GraphState, limit: usize) -> Option<usize> {
        let start = std::time::Instant::now();
        (0..limit).find(|&f| {
            let now = start + std::time::Duration::from_micros(16_667 * f as u64);
            !c.tick(st, BOUNDS, now, iced::advanced::mouse::Cursor::Unavailable)
        })
    }

    /// E2-2: a map that is left alone comes to rest, reports it, and then
    /// stays quiet — it no longer asks for a frame forever.
    #[test]
    fn an_untouched_map_settles_and_stops_asking_for_frames() {
        let l = layout(6);
        let c = canvas(&l, true, false);
        let mut st = GraphState::default();
        let rest = frames_to_rest(&c, &mut st, 5_000).expect("the map never settled");
        assert!(rest > 10, "it settled before the physics even ran: {rest}");
        assert!(graph_settled(&st));
        // At rest nothing is recomputed: another tick moves nothing.
        let (yaw, alpha, pos) = (st.yaw, st.alpha, st.pos.clone());
        let later = std::time::Instant::now() + std::time::Duration::from_secs(600);
        assert!(!c.tick(
            &mut st,
            BOUNDS,
            later,
            iced::advanced::mouse::Cursor::Unavailable
        ));
        assert_eq!((st.yaw, st.alpha), (yaw, alpha));
        assert_eq!(st.pos, pos);
    }

    /// E2-2: a settled map whose layout CONTENT changed under the same node
    /// set (edges rebuilt, cycle rings re-marked in place) — a new revision,
    /// the same signature — redraws and relaxes onto it, then settles again;
    /// its cached scene used to keep the old edges and rings on screen.
    #[test]
    fn a_new_layout_revision_wakes_a_settled_map() {
        let l = layout(6);
        let mut st = GraphState::default();
        let first = GraphCanvas::new(&l, 1, crate::Overlay::ProjectImports, true, true, false);
        assert!(frames_to_rest(&first, &mut st, 5_000).is_some());
        let (sig, pos) = (st.sig, st.pos.clone());
        let later = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let cursor = iced::advanced::mouse::Cursor::Unavailable;
        assert!(!first.tick(&mut st, BOUNDS, later, cursor), "at rest");

        let edited = GraphCanvas::new(&l, 2, crate::Overlay::ProjectImports, true, true, false);
        assert!(
            edited.tick(&mut st, BOUNDS, later, cursor),
            "the edited layout was not redrawn"
        );
        assert_eq!(st.sig, sig, "the same nodes: no reseed");
        assert_eq!(st.pos.len(), pos.len());
        assert!(frames_to_rest(&edited, &mut st, 5_000).is_some());
    }

    /// Spinning is motion: it keeps the frames coming (and stops with it).
    #[test]
    fn a_spinning_map_never_settles() {
        let l = layout(4);
        let c = canvas(&l, true, true);
        let mut st = GraphState::default();
        assert_eq!(frames_to_rest(&c, &mut st, 600), None);
        assert!(st.spinning && !graph_settled(&st));
        // Spin turned off: it comes to rest.
        let still = canvas(&l, true, false);
        assert!(frames_to_rest(&still, &mut st, 5_000).is_some());
    }

    /// Flattening to 2D is animated, then over.
    #[test]
    fn switching_to_2d_settles_once_the_depth_is_flat() {
        let l = layout(5);
        let mut st = GraphState::default();
        assert!(frames_to_rest(&canvas(&l, true, false), &mut st, 5_000).is_some());
        let flat = canvas(&l, false, false);
        assert!(frames_to_rest(&flat, &mut st, 5_000).is_some());
        assert!(st.pos.iter().all(|p| p[2].abs() <= 0.5));
    }

    #[test]
    fn graph_settled_requires_every_kind_of_motion_to_be_over() {
        let mut st = GraphState::default();
        assert!(graph_settled(&st));
        st.alpha = 0.5;
        assert!(!graph_settled(&st));
        st.alpha = 0.0;
        st.drag = Drag::Orbit;
        assert!(!graph_settled(&st));
        st.drag = Drag::None;
        st.label_alpha = vec![0.3];
        st.label_target = vec![1.0];
        assert!(!graph_settled(&st), "a label still fading in");
        st.label_alpha = vec![1.0];
        assert!(graph_settled(&st));
        st.flattening = true;
        assert!(!graph_settled(&st));
    }

    /// E2-4: a layout swapped in mid-drag (a rebuilt graph lands async) must
    /// not have its node `i` moved or opened on release — the index belonged
    /// to the old node set, and past its end it used to panic.
    #[test]
    fn a_drag_does_not_survive_a_reseed() {
        let old = layout(8);
        let new = layout(3);
        let mut st = GraphState::default();
        let _ = canvas(&old, false, false).tick(
            &mut st,
            BOUNDS,
            std::time::Instant::now(),
            iced::advanced::mouse::Cursor::Unavailable,
        );
        st.drag = Drag::Node(7, st.sig);
        let c = canvas(&new, false, false);
        let _ = c.tick(
            &mut st,
            BOUNDS,
            std::time::Instant::now(),
            iced::advanced::mouse::Cursor::Unavailable,
        );
        assert_eq!(st.drag, Drag::None, "reseeding ends the old drag");

        // A release carrying a stale node index opens nothing.
        st.drag = Drag::Node(7, st.sig);
        st.moved = 0.0;
        let release = iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
            iced::mouse::Button::Left,
        ));
        let action = <GraphCanvas<'_> as iced::widget::canvas::Program<Message>>::update(
            &c,
            &mut st,
            &release,
            BOUNDS,
            iced::advanced::mouse::Cursor::Unavailable,
        );
        let (published, _, _) = action.expect("the release is handled").into_inner();
        assert!(published.is_none(), "a stale index opened a node");
        assert!(c.open_message(7).is_none());
        assert!(c.open_message(2).is_some());
    }

    /// A click (press + release without moving) on a live node opens it.
    #[test]
    fn a_click_on_a_node_opens_it() {
        let l = layout(3);
        let c = canvas(&l, false, false);
        let mut st = GraphState::default();
        let _ = c.tick(
            &mut st,
            BOUNDS,
            std::time::Instant::now(),
            iced::advanced::mouse::Cursor::Unavailable,
        );
        let (x, y, _, _) = c.node_screen(1, BOUNDS, &st);
        let at = iced::advanced::mouse::Cursor::Available(Point::new(x, y));
        let press =
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left));
        let release = iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
            iced::mouse::Button::Left,
        ));
        let program = |e: &iced::Event, st: &mut GraphState| {
            <GraphCanvas<'_> as iced::widget::canvas::Program<Message>>::update(
                &c, st, e, BOUNDS, at,
            )
        };
        let _ = program(&press, &mut st);
        assert!(matches!(st.drag, Drag::Node(1, _)));
        let (published, _, _) = program(&release, &mut st)
            .expect("the release is handled")
            .into_inner();
        match published {
            Some(Message::Graph(GraphMsg::OverlayOpenImports(file))) => {
                assert_eq!(file, std::path::PathBuf::from("/p/file_1.rs"))
            }
            other => panic!("expected the node to open, got {other:?}"),
        }
    }
}
