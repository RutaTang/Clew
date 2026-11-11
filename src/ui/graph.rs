//! Import/call graph modals, node-link canvas, and the graph list bodies.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// Modal frame shared by the project-graph overlays: a titled panel with a
/// List/Map toggle, over a dismissable backdrop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn graph_modal_frame<'a>(
    title: &'a str,
    graph_mode: bool,
    graph_3d: bool,
    graph_spin: bool,
    graph_heat: bool,
    extra: Option<Element<'a, Message>>,
    search: Option<Element<'a, Message>>,
    picked: Option<Element<'a, Message>>,
    window: iced::Size,
    body: Element<'a, Message>,
) -> Element<'a, Message> {
    // Header controls are icon buttons; each names itself on hover (`chrome_tip`).
    let icon_btn = |g: Glyph, tip: &'static str, msg: Message| -> Element<'a, Message> {
        chrome_tip(
            button(glyph::icon(g, theme::fg_muted(), 16.0))
                .style(theme::toolbar_button)
                .padding([4, 8])
                .on_press(super::heard::said(tip, msg)),
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
    if let Some(search) = search {
        header = header.push(search);
    }
    // Map mode: a spin start/stop (3D only), then a 2D/3D projection toggle. Each
    // icon shows the action / the mode it switches to.
    if graph_mode && graph_3d {
        header = header.push(if graph_spin {
            icon_btn(
                Glyph::Pause,
                tr!("Stop spinning"),
                Message::Graph(GraphMsg::ToggleSpin),
            )
        } else {
            icon_btn(
                Glyph::Play,
                tr!("Spin"),
                Message::Graph(GraphMsg::ToggleSpin),
            )
        });
    }
    // Back to where the map starts: its zoom, its pan, its turn.
    // Two choices side by side, the one in effect lit: what each is, in
    // words — shapes for 2D and the map read as ◇ and ▽.
    let pair = |a: (String, bool), b: (String, bool), msg: Message| -> Element<'a, Message> {
        // The chosen one filled in the accent, its word in the colour made
        // to be read on it: the ink, on a cyan accent, was white on cyan.
        let seg = |(label, on): (String, bool)| {
            button(text(label.clone()).size(ts::SMALL).color(if on {
                theme::on_accent()
            } else {
                theme::fg_muted()
            }))
            .style(theme::toggle(on))
            .padding([4, 8])
            .on_press_maybe((!on).then(|| super::heard::said(&label, msg.clone())))
        };
        row![seg(a), seg(b)].spacing(2).into()
    };
    if graph_mode {
        header = header.push(icon_btn(
            Glyph::Recenter,
            tr!("Fit the graph"),
            Message::Graph(GraphMsg::FitView),
        ));
    }
    if graph_mode {
        header = header.push(pair(
            ("2D".to_string(), !graph_3d),
            ("3D".to_string(), graph_3d),
            Message::Graph(GraphMsg::Toggle3D),
        ));
        // Heat: colour by how often each file changed, instead of by language.
        // A toggle: lit while on, its word the same either way — a dot
        // added after it widened the button, and moved those beside it.
        header = header.push(chrome_tip(
            button(text(tr!("Heat")).size(ts::SMALL).color(if graph_heat {
                theme::on_accent()
            } else {
                theme::fg_muted()
            }))
            .style(theme::toggle(graph_heat))
            .padding([4, 8])
            .on_press(super::heard::said(
                tr!("Heat"),
                Message::Graph(GraphMsg::ToggleHeat),
            )),
            if graph_heat {
                tr!("Color by language")
            } else {
                tr!("Color by change frequency (commits in the recent history)")
            },
            None,
        ));
    }
    header = header
        .push(pair(
            (tr!("Graph").to_string(), graph_mode),
            (tr!("List").to_string(), !graph_mode),
            Message::Graph(GraphMsg::OverlayViewToggle),
        ))
        .push(icon_btn(
            Glyph::Close,
            tr!("Close"),
            Message::Graph(GraphMsg::CloseOverlay),
        ));
    // As large as the window allows, an edge of it left round — it was
    // 760 × 640 in any window, a big map squeezed into the middle of a big
    // screen — and never smaller than that where the window has room.
    let width = (window.width - 2.0 * GRAPH_MODAL_EDGE)
        .min(GRAPH_MODAL_MAX_W)
        .max(GRAPH_MODAL_W.min(window.width - 2.0 * MODAL_MARGIN));
    let height = (window.height - 2.0 * GRAPH_MODAL_EDGE)
        .max(GRAPH_MODAL_MAX_H.min(window.height - 2.0 * MODAL_MARGIN));
    let mut parts = column![header].spacing(12);
    if let Some(picked) = picked {
        parts = parts.push(picked);
    }
    let panel = container(parts.push(body))
        .width(width)
        .max_height(height)
        .padding(MODAL_PAD)
        .style(theme::modal_panel);

    modal(
        panel,
        Placement::Center,
        Backdrop::Dim(Some(Message::Graph(GraphMsg::CloseOverlay))),
    )
}

/// A project graph's title, in English: titled as the strip, the menu and
/// the command bar name what opens it — and as voice puts it away ("close
/// the call graph").
pub(crate) fn graph_title(overlay: crate::Overlay) -> &'static str {
    match overlay {
        crate::Overlay::ProjectImports => tr_noop!("Import Graph"),
        crate::Overlay::ProjectCalls => tr_noop!("Call Graph"),
        crate::Overlay::ProjectTypes => tr_noop!("Type Graph"),
        crate::Overlay::ProjectMaps => tr_noop!("Project Graphs"),
    }
}

pub(crate) fn project_graph_modal(app: &App, overlay: crate::Overlay) -> Element<'_, Message> {
    // Its list's rows counted from 1, as voice picks them.
    super::heard::list();
    let title = crate::i18n::lookup(graph_title(overlay));
    // The call graph can be refined to exact LSP edges; show its control/status.
    let extra: Option<Element<'_, Message>> = match overlay {
        crate::Overlay::ProjectCalls => Some(if let Some(progress) = app.refine_progress_label() {
            // As wide as it reads at its end: counting, its left edge — and
            // what it said — shook at every file.
            let widest = match app.proj.project_calls.refine_progress {
                Some((_, total)) if total > 0 => {
                    tr!("Refining {done}/{total}…", done = total, total = total)
                }
                _ => progress.clone(),
            };
            super::steady::slot(
                text(progress.clone())
                    .size(ts::SMALL)
                    .color(theme::warning()),
                &progress,
                &[&widest],
                ts::SMALL,
            )
        } else if let Some(precise) = app.precise_label() {
            text(precise).size(ts::SMALL).color(theme::accent()).into()
        } else {
            // With no project there is nothing to refine.
            button(text(tr!("Refine with LSP")).size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([3, 10])
                .on_press_maybe(app.proj.project.is_some().then(|| {
                    super::heard::said(
                        tr!("Refine with LSP"),
                        Message::Graph(GraphMsg::RefineProjectCalls),
                    )
                }))
                .into()
        }),
        crate::Overlay::ProjectImports => None,
        crate::Overlay::ProjectTypes => app.docs_loading().then(|| {
            text(tr!("Building the API docs…"))
                .size(ts::SMALL)
                .color(theme::dim())
                .into()
        }),
        crate::Overlay::ProjectMaps => map_picker(app),
    };
    let body = if app.graph_mode {
        graph_map_view(app)
    } else {
        match overlay {
            crate::Overlay::ProjectImports => project_imports_body(app),
            crate::Overlay::ProjectCalls => project_calls_body(app),
            crate::Overlay::ProjectTypes => project_types_body(app),
            crate::Overlay::ProjectMaps => project_maps_body(app),
        }
    };
    // The node picked: what it is and where, and the way into it.
    let picked = app.proj.graph_pick.as_ref().filter(|_| app.graph_mode).map(
        |pick| -> Element<'_, Message> {
            let place = app
                .proj
                .project
                .as_ref()
                .and_then(|p| pick.file.strip_prefix(&p.root).ok())
                .map(|rel| rel.display().to_string())
                .unwrap_or_default();
            row![
                text(pick.label.as_str())
                    .size(ts::BASE)
                    .color(theme::fg())
                    .wrapping(iced::widget::text::Wrapping::None),
                container(
                    text(place)
                        .size(ts::SMALL)
                        .color(theme::dim())
                        .wrapping(iced::widget::text::Wrapping::None)
                )
                .width(Fill)
                .clip(true),
                button(text(tr!("Open")).size(ts::SMALL))
                    .style(theme::primary_button)
                    .padding([3, 12])
                    .on_press(super::heard::said(tr!("Open"), (*pick.open).clone())),
            ]
            .spacing(10)
            .align_y(iced::Center)
            .into()
        },
    );
    let search = app.graph_mode.then(|| graph_search_field(app));
    graph_modal_frame(
        title,
        app.graph_mode,
        app.graph_3d,
        app.graph_spin,
        app.graph_heat,
        extra,
        search,
        picked,
        iced::Size::new(app.window_width, app.window_height),
        body,
    )
}

/// Node `i` of the map `kind` laid out as `layout`, picked: what it is,
/// and how to open it — none for a node that opens nothing.
pub(crate) fn node_pick(
    layout: &crate::graphlayout::Layout,
    kind: crate::Overlay,
    i: usize,
) -> Option<crate::app::model::GraphPick> {
    let node = layout.nodes.get(i)?;
    let open = node_open_message(layout, kind, i)?;
    Some(crate::app::model::GraphPick {
        label: node.label.clone(),
        file: node.file.clone(),
        open: Box::new(open),
    })
}

/// The message that opens node `i` of the map `kind`, if it is part of
/// `layout`.
pub(crate) fn node_open_message(
    layout: &crate::graphlayout::Layout,
    kind: crate::Overlay,
    i: usize,
) -> Option<Message> {
    let node = layout.nodes.get(i)?;
    let file = node.file.clone();
    Some(match kind {
        crate::Overlay::ProjectImports => Message::Graph(GraphMsg::OverlayOpenImports(file)),
        crate::Overlay::ProjectCalls => {
            Message::Graph(GraphMsg::OverlayOpenAt { abs: file, line: 1 })
        }
        crate::Overlay::ProjectTypes => Message::Graph(GraphMsg::OverlayOpenAt {
            abs: file,
            line: node.line,
        }),
        // A node defined nowhere (a route's path) opens nothing.
        crate::Overlay::ProjectMaps if file.as_os_str().is_empty() => return None,
        crate::Overlay::ProjectMaps => Message::Graph(GraphMsg::OverlayOpenAt {
            abs: file,
            line: node.line,
        }),
    })
}

/// The nodes of `layout` the map's search `query` finds, best first: a
/// name that is the query, then one that starts with it (or a part of its
/// path does), then one that holds it — case aside — and among each, the
/// weightier first. None for an empty query.
pub(crate) fn graph_matches(layout: &crate::graphlayout::Layout, query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<(u8, usize)> = layout
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, node)| {
            let label = node.label.to_lowercase();
            let parts = || label.split(['/', '\\', ':', '.']);
            let rank = if label == query || parts().any(|p| p == query) {
                0
            } else if label.starts_with(&query) || parts().any(|p| p.starts_with(&query)) {
                1
            } else if label.contains(&query) {
                2
            } else {
                return None;
            };
            Some((rank, i))
        })
        .collect();
    found.sort_by(|(ra, a), (rb, b)| {
        ra.cmp(rb)
            .then(layout.nodes[*b].weight.total_cmp(&layout.nodes[*a].weight))
            .then(layout.nodes[*a].label.cmp(&layout.nodes[*b].label))
    });
    found.into_iter().map(|(_, i)| i).collect()
}

/// The map's search as the canvas draws it: which nodes it found, the one
/// the camera is on and its neighbours — lit, the rest dimmed.
#[derive(Debug, Clone, Default)]
pub(crate) struct Lit {
    pub(crate) found: Vec<bool>,
    pub(crate) near: Vec<bool>,
    pub(crate) focus: Option<usize>,
    /// What it is, for the scene cache: the query and the node in focus.
    pub(crate) key: u64,
}

impl Lit {
    /// Whether node `i` is drawn lit.
    pub(crate) fn lit(&self, i: usize) -> bool {
        self.focus == Some(i)
            || self.found.get(i).copied().unwrap_or(false)
            || self.near.get(i).copied().unwrap_or(false)
    }
}

/// The map's search, in its header: the field, how many it found and which
/// the camera is on, and the way to the one before and after.
pub(crate) fn graph_search_field(app: &App) -> Element<'_, Message> {
    let found = app.graph_matches().len();
    let searching = !app.graph_search.trim().is_empty();
    // As wide as it reads at its widest, so the field never shifts.
    let count: Element<'_, Message> = if !searching {
        slot()
    } else if found == 0 {
        text(tr!("No matches"))
            .size(ts::SMALL)
            .color(theme::dim())
            .into()
    } else {
        text(tr!(
            "{step} of {total}",
            step = app.graph_found % found + 1,
            total = found
        ))
        .size(ts::SMALL)
        .color(theme::fg_muted())
        .into()
    };
    let step = |g: Glyph, tip: &'static str, forward: bool| -> Element<'_, Message> {
        chrome_tip(
            button(glyph::icon(g, theme::fg_muted(), 14.0))
                .style(theme::toolbar_button)
                .padding([4, 6])
                .on_press_maybe((found > 1).then(|| {
                    super::heard::said(tip, Message::Graph(GraphMsg::SearchStep(forward)))
                })),
            tip,
            None,
        )
    };
    row![
        text_input(tr!("Find in the graph"), &app.graph_search)
            .id(super::graph_search_id())
            .on_input(|q| Message::Graph(GraphMsg::Search(q)))
            .on_submit(Message::Graph(GraphMsg::SearchStep(true)))
            .style(theme::input)
            .size(ts::SMALL)
            .padding([4, 8])
            .width(190),
        container(count)
            .width(72)
            .align_x(iced::alignment::Horizontal::Center),
        step(Glyph::ChevronLeft, tr!("Previous match"), false),
        step(Glyph::ChevronRight, tr!("Next match"), true),
    ]
    .spacing(2)
    .align_y(iced::Center)
    .into()
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
    let empty_msg = if app.proj.project.is_none() {
        // Not "Nothing to show.", which reads as a project with nothing in it.
        tr!("Open a project to see its graph")
    } else if app.proj.project_calls.building {
        tr!("Building call graph…")
    } else if app.scanning || app.proj.indexing {
        tr!("Indexing the project…")
    } else if app.proj.graph_layout_pending {
        // Laid out off the UI thread (`App::refresh_graph_layout`).
        tr!("Laying out the graph…")
    } else {
        tr!("Nothing to show.")
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
        )
        .with_visited(Some(trace_files(app)), app.debug.trace_rev)
        .with_fit(app.graph_fit_rev)
        .with_search(&app.graph_search, app.graph_found, app.graph_focus_rev),
    )
    .width(Fill)
    .height(Fill);
    map.into()
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
    /// The files the last debug run stopped in: their nodes are ringed, so
    /// the path the program actually took shows on the map.
    pub(crate) visited: Option<std::sync::Arc<std::collections::HashSet<std::path::PathBuf>>>,
    /// The generation of `visited`: part of the scene cache's key.
    pub(crate) visited_rev: u64,
    /// Bumped to put the camera back where it starts (`GraphMsg::FitView`).
    pub(crate) fit_rev: u64,
    /// The map's search, while one is typed: what it lit.
    pub(crate) lit: Option<Lit>,
    /// Bumped to send the camera to the search's node in focus, or back
    /// over the whole map once the search is cleared.
    pub(crate) focus_rev: u64,
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
            visited: None,
            visited_rev: 0,
            fit_rev: 0,
            lit: None,
            focus_rev: 0,
        }
    }

    /// Light what the search `query` finds, `found` being which of its
    /// matches (counted from 0, round) the camera goes to each time
    /// `focus_rev` moves on.
    pub(crate) fn with_search(mut self, query: &str, found: usize, focus_rev: u64) -> Self {
        self.focus_rev = focus_rev;
        let matches = graph_matches(self.layout, query);
        if query.trim().is_empty() {
            return self;
        }
        let n = self.layout.nodes.len();
        let focus = (!matches.is_empty()).then(|| matches[found % matches.len()]);
        let mut lit = Lit {
            found: vec![false; n],
            near: vec![false; n],
            focus,
            key: 0,
        };
        for &i in &matches {
            lit.found[i] = true;
        }
        if let Some(f) = focus {
            for &(a, b) in &self.layout.edges {
                if a == f && b < n {
                    lit.near[b] = true;
                } else if b == f && a < n {
                    lit.near[a] = true;
                }
            }
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        (query.trim().to_lowercase(), focus).hash(&mut h);
        lit.key = h.finish() | 1;
        self.lit = Some(lit);
        self
    }

    /// Put the camera back where it starts each time `rev` moves on.
    pub(crate) fn with_fit(mut self, rev: u64) -> Self {
        self.fit_rev = rev;
        self
    }

    /// Ring the nodes of the files in `visited` (the last debug run's
    /// stops), `trace_rev` being their generation.
    pub(crate) fn with_visited(
        mut self,
        visited: Option<std::sync::Arc<std::collections::HashSet<std::path::PathBuf>>>,
        trace_rev: u64,
    ) -> Self {
        self.visited = visited.filter(|v| !v.is_empty());
        self.visited_rev = if self.visited.is_some() { trace_rev } else { 0 };
        self
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
/// How much room past the right edge a label moved to its node's left
/// needs before it goes back to the right (px).
const LABEL_SIDE_SLACK: f32 = 24.0;
/// How far (px) outside every disc a click may land and still pick the
/// nearest node, so tiny discs stay easy to hit.
const HIT_SLOP: f32 = 16.0;
/// How far past its disc the pointer may stray from the node holding the
/// spin before letting it go.
const HOLD_SLACK: f32 = 28.0;
/// The longest a frame may turn the camera by: a late frame turns it as far
/// as the time that passed, so the spin keeps its pace through it — but a
/// map drawn again after a pause does not leap.
const MAX_TURN_DT: f32 = 0.25;
/// Idle spin speed (radians per second, 3D only).
const SPIN_RATE: f32 = 0.10;
/// How much of itself what a search did not light keeps.
const DIMMED: f32 = 0.22;
/// How far the camera zooms in, at least, on the node a search found.
const FOCUS_ZOOM: f32 = 2.4;

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

/// What the cached scene was drawn for: the hovered node, the theme's
/// revision, the layout's revision, the paint's (heat) revision and the
/// visited set's (trace) revision.
type SceneKey = (Option<usize>, u64, u64, u64, u64, u64);

/// Live 3D-simulation + orbit camera + interaction state, persisted by the
/// canvas widget across frames. The force sim and the camera both run here
/// (stepped each `RedrawRequested` while anything moves), so every graph
/// animates in 3D, can spin, and its nodes can be grabbed and moved.
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
    /// The camera still turns: the idle spin is on, or a fling or a spin
    /// that was just switched off has not come to rest.
    spinning: bool,
    /// A 3D → 2D switch is still relaxing depth toward the plane.
    flattening: bool,
    /// Turn rates of the camera, radians per second: what a released orbit
    /// flings it with, easing toward the idle spin (or to rest).
    yaw_vel: f32,
    pitch_vel: f32,
    /// The idle spin's rate as the turn eases toward it: itself eased in
    /// (`LEAD_TAU`), so a spin starting or stopping builds its acceleration
    /// from nought instead of jumping to it.
    spin_goal: f32,
    /// The camera angles at the last tick, for the rate an orbit drag turns.
    last_yaw: f32,
    last_pitch: f32,
    /// How 3D the picture is drawn: 1 in 3D, 0 flat, between while a switch
    /// eases from `dim_from` to the mode asked for (`dim_3d`), `dim_t` of
    /// the way there.
    dim: f32,
    dim_from: f32,
    dim_t: f32,
    dim_3d: bool,
    /// The zoom the wheel asked for; `zoom` eases to it. In 2D the point
    /// under the pointer (`zoom_at`, from the middle) stays put.
    zoom_target: f32,
    zoom_at: Option<iced::Vector>,
    /// The zoom's rate of change, in log space per second.
    zoom_vel: f32,
    /// What the camera looks at, and the scale that fits the graph in the
    /// frame (before the zoom): eased to where the graph is, so neither
    /// jumps as it settles, grows a depth or flattens.
    center: crate::graphlayout::V3,
    fit_base: f32,
    /// Where, at a scale of one, the middle of the picture is off the
    /// camera's centre: a 3D camera at rest centres what it shows by it.
    shift: (f32, f32),
    /// How fast `shift` is moving: it rides a spring, so a re-centring
    /// starts from rest.
    shift_vel: (f32, f32),
    /// Where the spring takes `shift`: its target, eased in (`LEAD_TAU`),
    /// so a re-centring builds its acceleration from nought.
    shift_goal: (f32, f32),
    /// The framing has not caught up with the graph yet.
    framing: bool,
    /// Which side of its node each label is drawn on: 0 right, 1 left,
    /// eased, so a label that must change sides cross-fades.
    label_side: Vec<f32>,
    label_side_target: Vec<f32>,
    /// Something changed the picture since the last tick (camera, zoom, pan,
    /// a dragged node), so the next tick must recompute.
    dirty: bool,
    /// The bounds and hovered node the last tick computed for.
    last_bounds: iced::Size,
    last_hover: Option<usize>,
    /// The node the pointer rests on, which holds the spin: the camera
    /// eases to a stop so it can be read, and turns again once the pointer
    /// leaves it. Kept while the pointer stays near it — it drifts on a
    /// little as the turn slows.
    hold: Option<usize>,
    /// The node the search sent the camera to: it is centred and zoomed
    /// on, and holds the spin, until the search moves on or is cleared.
    focus: Option<usize>,
    /// The `GraphCanvas::focus_rev` the camera last went by.
    focus_rev: u64,
    /// The search the cached scene and labels were laid out for.
    lit_key: u64,
    /// The node clicked once (its index, under the layout signature it was
    /// clicked in): a click on it again opens it.
    picked: Option<(usize, u64)>,
    /// The `GraphCanvas::fit_rev` the camera was last put back at.
    fit_rev: u64,
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
            label_alpha: Vec::new(),
            label_target: Vec::new(),
            label_w: Vec::new(),
            drag: Drag::None,
            moved: 0.0,
            last_cursor: iced::Point::new(0.0, 0.0),
            spinning: false,
            flattening: false,
            yaw_vel: 0.0,
            pitch_vel: 0.0,
            spin_goal: 0.0,
            last_yaw: 0.6,
            last_pitch: 0.35,
            dim: 1.0,
            dim_from: 1.0,
            dim_t: 1.0,
            dim_3d: true,
            zoom_target: 1.0,
            zoom_at: None,
            zoom_vel: 0.0,
            center: [0.0; 3],
            fit_base: 0.0,
            shift: (0.0, 0.0),
            shift_vel: (0.0, 0.0),
            shift_goal: (0.0, 0.0),
            framing: false,
            label_side: Vec::new(),
            label_side_target: Vec::new(),
            dirty: true,
            last_bounds: iced::Size::ZERO,
            last_hover: None,
            hold: None,
            focus: None,
            focus_rev: 0,
            lit_key: 0,
            picked: None,
            fit_rev: 0,
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
    let arrived = |now: &[f32], to: &[f32]| {
        now.len() == to.len() && now.iter().zip(to).all(|(a, t)| (a - t).abs() <= LABEL_EPS)
    };
    st.drag == Drag::None
        && st.alpha <= ALPHA_REST
        && !st.spinning
        && !st.flattening
        && st.dim_t >= 1.0
        && st.zoom == st.zoom_target
        && !st.framing
        && arrived(&st.label_alpha, &st.label_target)
        && arrived(&st.label_side, &st.label_side_target)
}

/// How long a switch between 3D and 2D takes, in seconds.
const DIM_SECS: f32 = 0.6;
/// Time constants (seconds) things ease toward where they are going with:
/// the framing, a label's opacity and its side, and a camera's
/// turn rate toward the idle spin or rest (`TURN_TAU`, slow, so a fling
/// coasts) — pitch settling sooner.
/// The zoom spring's rate (per second): settled in about a quarter second.
const ZOOM_RATE: f32 = 18.0;
/// The re-centring spring's rate: settled in about half a second.
const SHIFT_RATE: f32 = 9.0;
const FRAME_TAU: f32 = 0.14;
const LABEL_TAU: f32 = 0.11;
const TURN_TAU: f32 = 0.55;
const PITCH_TAU: f32 = 0.3;
/// Up to this many nodes no label fades with depth; the fade is full
/// this many nodes past that.
const FADE_FREE_NODES: f32 = 6.0;
const FADE_FULL_NODES: f32 = 24.0;
/// The lead-in a camera's start or stop eases its target in by, so the
/// motion's acceleration builds from nought rather than jumping.
const LEAD_TAU: f32 = 0.09;
/// Turns of the spin, and tilts of the camera, the 3D framing is measured
/// at, and the room it leaves for a node's farthest falling between two of
/// them (cos of half the larger step, inverted, with a margin).
const SPIN_SAMPLES: usize = 24;
const TILT_SAMPLES: usize = 11;
const SPIN_SAMPLE_SLACK: f32 = 1.016;
/// How far the camera tilts up or down, in radians.
const PITCH_LIMIT: f32 = 1.45;
/// Fastest a fling turns the camera, radians per second.
const MAX_TURN: f32 = 8.0;
/// The camera's angles a switch to 3D swings to from flat.
const HOME_YAW: f32 = 0.6;
const HOME_PITCH: f32 = 0.35;

/// The share of the way to its target an eased value covers in `dt`
/// seconds with time constant `tau` — the same in any frame rate.
fn ease_step(dt: f32, tau: f32) -> f32 {
    1.0 - (-dt / tau).exp()
}

/// Ease in and out, for a move from one state to another (`t` from 0 to
/// 1): it starts and stops with no speed and no acceleration, and its
/// acceleration never jumps (the quintic "smootherstep").
fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// One step of `dt` seconds of a critically damped spring at `rate`: where
/// a value `x` off its rest, moving at `v`, is then, and how fast — solved
/// exactly, so it gathers speed from rest and settles without overshoot at
/// any frame rate.
fn spring(x: f32, v: f32, rate: f32, dt: f32) -> (f32, f32) {
    let decay = (-rate * dt).exp();
    let k = v + rate * x;
    ((x + k * dt) * decay, (v - rate * k * dt) * decay)
}

/// `a` wrapped into (−π, π]: the same turn of the camera.
fn wrap_angle(a: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let w = (a + PI).rem_euclid(TAU) - PI;
    if w <= -PI { w + TAU } else { w }
}

/// A radius that holds the graph, robust to a lone far-flung node: the
/// largest of `dists` (sorted), but no more than half again past the
/// 90th percentile, so one outlier cannot shrink the rest into a knot.
fn robust_extent(dists: &[f32]) -> f32 {
    let Some(&most) = dists.last() else {
        return 1.0;
    };
    let p90 = dists[((dists.len() - 1) as f32 * 0.9) as usize];
    most.min(p90 * 1.5).max(1.0)
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

    /// Screen position of node `i`: the projection once seeded, else the
    /// static fallback. Returns `(x, y, depth, perspective)`.
    ///
    /// One projection for both modes, blended by how 3D the picture is
    /// (`dim`): in 3D the camera turns about the graph's centre with
    /// perspective and depth; flat, it looks straight on, orthographic, with
    /// the pan and one depth for all. Between, a switch is a camera move,
    /// never a jump.
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
        let e = st.dim;
        let p = st.pos[i];
        let q = [
            p[0] - st.center[0],
            p[1] - st.center[1],
            p[2] - st.center[2],
        ];
        let (rx, ry, z, persp3) = Self::project_view(rotate(q, st.yaw * e, st.pitch * e));
        // `project_view` scaled x and y by the perspective: undo the part a
        // flatter picture does not have.
        let persp = 1.0 + (persp3 - 1.0) * e;
        let (x, y) = (rx / persp3 * persp, ry / persp3 * persp);
        (
            (x - st.shift.0) * st.fit_scale + st.fit_off.0 + st.pan.x * (1.0 - e),
            (y - st.shift.1) * st.fit_scale + st.fit_off.1 + st.pan.y * (1.0 - e),
            z * e,
            persp,
        )
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
        // The inverse of `node_screen`, at the node's own depth.
        let e = st.dim;
        let (yaw, pitch) = (st.yaw * e, st.pitch * e);
        let c = st.center;
        let p = st.pos[i];
        let z2 = rotate([p[0] - c[0], p[1] - c[1], p[2] - c[2]], yaw, pitch)[2];
        let persp = 1.0 + (FOCAL / (CAM - z2).max(FOCAL * 0.15) - 1.0) * e;
        let scale = st.fit_scale.max(1e-3) * persp;
        let x1 = (cursor.x - st.fit_off.0 - st.pan.x * (1.0 - e)) / scale + st.shift.0 / persp;
        let y2 = (cursor.y - st.fit_off.1 - st.pan.y * (1.0 - e)) / scale + st.shift.1 / persp;
        // Un-rotate (inverse pitch, then inverse yaw).
        let (sy, cy) = yaw.sin_cos();
        let (sp, cp) = pitch.sin_cos();
        let y1 = y2 * cp + z2 * sp;
        let z1 = -y2 * sp + z2 * cp;
        [
            x1 * cy - z1 * sy + c[0],
            y1 + c[1],
            x1 * sy + z1 * cy + c[2],
        ]
    }

    /// The message that picks node `i` (see `GraphPick`), if it opens to
    /// anything.
    pub(crate) fn pick_message(&self, i: usize) -> Option<Message> {
        node_pick(self.layout, self.kind, i)
            .map(|pick| Message::Graph(GraphMsg::Picked(Some(pick))))
    }

    /// The message that opens node `i`, if it is still part of this layout.
    pub(crate) fn open_message(&self, i: usize) -> Option<Message> {
        node_open_message(self.layout, self.kind, i)
    }

    /// Whether the idle spin turns the camera now: in 3D with the spin on,
    /// nothing grabbed, and no node holding it.
    fn spins(&self, st: &GraphState) -> bool {
        self.is_3d && self.spin && st.drag == Drag::None && st.hold.is_none() && st.focus.is_none()
    }

    /// The node holding the spin, the pointer being where `cursor` says and
    /// over `hovered`: the hovered node, or the one held still while the
    /// pointer stays within [`HOLD_SLACK`] of its disc — it drifts on as
    /// the turn slows, and letting go there turned the camera again,
    /// under the pointer, in a loop.
    fn hold_for(
        &self,
        st: &GraphState,
        hovered: Option<usize>,
        cursor: iced::advanced::mouse::Cursor,
        bounds: iced::Rectangle,
    ) -> Option<usize> {
        if st.drag != Drag::None {
            return None;
        }
        let at = cursor.position_in(bounds)?;
        if hovered.is_some() {
            return hovered;
        }
        let held = st.hold.filter(|&i| i < self.layout.nodes.len())?;
        let (x, y, _, persp) = self.project_all(bounds, st).get(held).copied()?;
        let r = node_radius(self.layout.nodes[held].weight, persp);
        let near = ((x - at.x).powi(2) + (y - at.y).powi(2)).sqrt() <= r + HOLD_SLACK;
        near.then_some(held)
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
        st.label_side = vec![0.0; n];
        st.label_side_target = vec![0.0; n];
        // A new graph is framed where it is, not eased in from the last, and
        // drawn in the mode asked for, not switched into it.
        st.fit_base = 0.0;
        let dim = if self.is_3d { 1.0 } else { 0.0 };
        st.dim = dim;
        st.dim_from = dim;
        st.dim_t = 1.0;
        st.dim_3d = self.is_3d;
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
        // Frame time, as it passed: a map waking from rest, or drawn again
        // after it was out of sight, steps one frame, not the whole pause.
        let gap = st.last_frame.map(|t| now.duration_since(t).as_secs_f32());
        let dt = gap
            .filter(|&gap| gap <= 0.1)
            .unwrap_or(1.0 / 60.0)
            .clamp(0.004, 0.05);
        // The camera turns by the time that passed, a late frame's whole
        // gap: clamped as the physics step is, a slow frame slowed the spin
        // and the next caught up — a stutter, then a spin.
        let turn_dt = gap.unwrap_or(1.0 / 60.0).clamp(0.004, MAX_TURN_DT);
        st.last_frame = Some(now);

        // A switch between 3D and 2D: a camera move over `DIM_SECS`, from
        // wherever the last one had got to.
        if self.is_3d != st.dim_3d {
            if !self.is_3d && st.dim >= 0.999 {
                // The same view, by the shortest way round to face-on.
                st.yaw = wrap_angle(st.yaw);
            }
            if self.is_3d && st.dim <= 0.001 {
                // From flat: re-inflate the depth (2D collapsed it) and wake
                // the sim, so it opens into a volume again; the camera swings
                // to its home angles rather than wherever it was left.
                for (i, p) in st.pos.iter_mut().enumerate() {
                    p[2] = (((i * 61) % 100) as f32 / 100.0 - 0.5) * WORLD * 0.8;
                }
                st.alpha = st.alpha.max(0.7);
                st.yaw = HOME_YAW;
                st.pitch = HOME_PITCH;
            }
            st.dim_3d = self.is_3d;
            st.dim_from = st.dim;
            st.dim_t = 0.0;
            st.yaw_vel = 0.0;
            st.pitch_vel = 0.0;
            st.spin_goal = 0.0;
            st.touch();
        }
        let switching = st.dim_t < 1.0;
        if switching {
            st.dim_t = (st.dim_t + dt / DIM_SECS).min(1.0);
            let to = if self.is_3d { 1.0 } else { 0.0 };
            st.dim = st.dim_from + (to - st.dim_from) * ease_in_out(st.dim_t);
        }

        let pinned = match st.drag {
            Drag::Node(i, sig) if sig == st.sig && i < n => Some(i),
            _ => None,
        };
        if pinned.is_some() {
            st.alpha = st.alpha.max(0.3);
        }

        // The node under the pointer holds the spin (see `GraphState::hold`).
        let hovered = cursor
            .position_in(bounds)
            .and_then(|c| self.hit(c, bounds, st));
        st.hold = self.hold_for(st, hovered, cursor, bounds);
        // The node held is the one shown as hovered — its label kept, drawn
        // on top — while the pointer strays near it.
        let hovered = hovered.or(st.hold);
        // The camera's turn: followed from the pointer while orbiting (its
        // rate measured, to fling with on release), else coasting toward the
        // idle spin — or to rest — so a spin starts, stops and hands over
        // from a fling without a jolt.
        if st.drag == Drag::Orbit {
            let follow = ease_step(dt, 0.04);
            let yaw_rate = (st.yaw - st.last_yaw) / dt;
            let pitch_rate = (st.pitch - st.last_pitch) / dt;
            st.yaw_vel += (yaw_rate.clamp(-MAX_TURN, MAX_TURN) - st.yaw_vel) * follow;
            st.pitch_vel += (pitch_rate.clamp(-MAX_TURN, MAX_TURN) - st.pitch_vel) * follow;
        } else {
            let spin = if self.spins(st) { SPIN_RATE } else { 0.0 };
            st.spin_goal += (spin - st.spin_goal) * ease_step(turn_dt, LEAD_TAU);
            if (st.spin_goal - spin).abs() < 1e-4 {
                st.spin_goal = spin;
            }
            st.yaw_vel += (st.spin_goal - st.yaw_vel) * ease_step(turn_dt, TURN_TAU);
            st.pitch_vel *= 1.0 - ease_step(turn_dt, PITCH_TAU);
            if st.yaw_vel.abs() < 1e-3 && st.spin_goal == 0.0 {
                st.yaw_vel = 0.0;
            }
            if st.pitch_vel.abs() < 1e-3 {
                st.pitch_vel = 0.0;
            }
            st.yaw += st.yaw_vel * turn_dt;
            let pitch = st.pitch + st.pitch_vel * turn_dt;
            st.pitch = pitch.clamp(-PITCH_LIMIT, PITCH_LIMIT);
            if st.pitch != pitch {
                st.pitch_vel = 0.0;
            }
        }
        st.last_yaw = st.yaw;
        st.last_pitch = st.pitch;
        let turning = st.yaw_vel != 0.0 || st.pitch_vel != 0.0;
        st.spinning = turning || self.spins(st);

        // The zoom eases to where the wheel asked for; flat, about the
        // point under the pointer.
        let zooming = st.zoom != st.zoom_target;
        if zooming {
            let from = st.zoom;
            let to = st.zoom_target;
            // A critically damped spring, in log space (each step of the
            // wheel the same), solved exactly for the frame: it gathers
            // speed and settles without overshoot, at any frame rate.
            let (x, v) = spring((from / to).ln(), st.zoom_vel, ZOOM_RATE, dt);
            st.zoom_vel = v;
            if x.abs() < 1e-4 && st.zoom_vel.abs() < 1e-3 {
                st.zoom = to;
                st.zoom_vel = 0.0;
            } else {
                st.zoom = to * x.exp();
            }
            if let Some(at) = st.zoom_at.filter(|_| !self.is_3d) {
                let k = st.zoom / from;
                st.pan =
                    iced::Vector::new(at.x - (at.x - st.pan.x) * k, at.y - (at.y - st.pan.y) * k);
            }
            if st.zoom == to {
                st.zoom_at = None;
            }
        }

        st.flattening = !self.is_3d && st.pos.iter().any(|p| p[2].abs() > 0.5);
        let switching = switching || st.dim_t < 1.0;
        let physics = pinned.is_some() || st.alpha > ALPHA_REST;
        let resized = bounds.size() != st.last_bounds;
        let changed = st.dirty
            || physics
            || st.spinning
            || st.flattening
            || switching
            || zooming
            || st.framing
            || resized
            || hovered != st.last_hover;
        if !changed && graph_settled(st) {
            // At rest: the next frame, whenever it comes, starts afresh.
            st.last_frame = None;
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
        if !self.is_3d && !switching {
            // Flat 2D, the camera face-on: relax the depth toward the z=0
            // plane, where it no longer shows. (During the switch it would
            // move the picture under the camera's own move.)
            let f = 1.0 - ease_step(dt, 1.0 / 7.0);
            for p in st.pos.iter_mut() {
                p[2] *= f;
            }
            for v in st.vel.iter_mut() {
                v[2] = 0.0;
            }
        }

        // --- Framing ----------------------------------------------------------
        // The whole graph, centred. In 3D the camera turns about the graph's
        // centroid and fits a sphere round it — the same at every angle, so a
        // spin never makes the framing breathe; flat, it fits the graph's own
        // box, every bit of the frame used. Robust to a lone far-flung node.
        // Both eased toward, blended by how 3D the picture is; a new graph or
        // a resized frame takes its framing at once.
        let e = st.dim;
        let inner_w = (bounds.width - 2.0 * GRAPH_PAD).max(1.0);
        let inner_h = (bounds.height - 2.0 * GRAPH_PAD).max(1.0);
        let box_center: crate::graphlayout::V3 = std::array::from_fn(|axis| {
            let (lo, hi) = st.pos.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| {
                (lo.min(p[axis]), hi.max(p[axis]))
            });
            if n == 0 { 0.0 } else { (lo + hi) / 2.0 }
        });
        // In 3D: how far out each node is drawn — across and up — at the
        // farthest it gets at any angle the camera can take: a whole turn of
        // the spin, at every tilt an orbit reaches, its perspective
        // included. Turning the camera — the spin, a drag, a fling — then
        // never zooms: the framing holds at every angle of it. It was the
        // farthest of a turn at the tilt of the moment, and a drag up or
        // down zoomed the graph in and out under the hand.
        let (mut across, mut up): (Vec<f32>, Vec<f32>) = st
            .pos
            .iter()
            .map(|p| {
                let d = [
                    p[0] - box_center[0],
                    p[1] - box_center[1],
                    p[2] - box_center[2],
                ];
                let mut far = (0.0f32, 0.0f32);
                for t in 0..TILT_SAMPLES {
                    let pitch =
                        -PITCH_LIMIT + 2.0 * PITCH_LIMIT * t as f32 / (TILT_SAMPLES - 1) as f32;
                    for k in 0..SPIN_SAMPLES {
                        let yaw = k as f32 * std::f32::consts::TAU / SPIN_SAMPLES as f32;
                        let (rx, ry, _, _) = Self::project_view(rotate(d, yaw, pitch));
                        far = (far.0.max(rx.abs()), far.1.max(ry.abs()));
                    }
                }
                far
            })
            .unzip();
        across.sort_by(f32::total_cmp);
        up.sort_by(f32::total_cmp);
        // Sampled turns can miss a node's farthest by a hair: a little room.
        let fit_3d = (inner_w / (2.0 * robust_extent(&across)))
            .min(inner_h / (2.0 * robust_extent(&up)))
            / SPIN_SAMPLE_SLACK;
        let half = |axis: usize| {
            let mut d: Vec<f32> = st
                .pos
                .iter()
                .map(|p| (p[axis] - box_center[axis]).abs())
                .collect();
            d.sort_by(f32::total_cmp);
            robust_extent(&d)
        };
        let fit_2d = (inner_w / (2.0 * half(0))).min(inner_h / (2.0 * half(1)));
        // A 3D camera at rest centres the picture as it stands: the middle
        // of what it shows, not the pivot it turns about (from some angles
        // the graph sits off to one side of it). Turning — the spin, a fling,
        // an orbit — it centres on the pivot again, the one centre no angle
        // runs out of; the two ease into each other as the camera starts and
        // stops. The scale stays the whole turn's: no zooming in and out as
        // the camera is grabbed and let go.
        //
        // A node held is not a turn of the camera: the camera holds its
        // framing while the node is held, so the node stays under the hand —
        // neither re-centred for the grab nor followed as it is pulled out —
        // and eases to the graph as it has become once it is let go. (A new
        // frame, or a switch between 3D and 2D, still frames it at once.)
        let reframe = st.fit_base <= 0.0 || resized || switching;
        let held = matches!(st.drag, Drag::Node(..)) && !reframe;
        let still = !st.spinning && st.drag == Drag::None;
        let shift_to = if held {
            st.shift_goal
        } else if still && e > 0.0 {
            let (lo_x, hi_x, lo_y, hi_y) = st.pos.iter().fold(
                (f32::MAX, f32::MIN, f32::MAX, f32::MIN),
                |(a, b, c, d), p| {
                    let q = [
                        p[0] - box_center[0],
                        p[1] - box_center[1],
                        p[2] - box_center[2],
                    ];
                    let (rx, ry, _, persp3) =
                        Self::project_view(rotate(q, st.yaw * e, st.pitch * e));
                    let persp = 1.0 + (persp3 - 1.0) * e;
                    let (x, y) = (rx / persp3 * persp, ry / persp3 * persp);
                    (a.min(x), b.max(x), c.min(y), d.max(y))
                },
            );
            if n == 0 {
                (0.0, 0.0)
            } else {
                ((lo_x + hi_x) / 2.0, (lo_y + hi_y) / 2.0)
            }
        } else {
            (0.0, 0.0)
        };
        let (fit_to, center_to) = if held {
            (st.fit_base, st.center)
        } else {
            (fit_2d + (fit_3d - fit_2d) * e, box_center)
        };
        // The node a search found: the camera centres it — eased there as
        // any re-centring is.
        let focused = st.focus.filter(|&f| f < n && !held);
        let center_to = focused.map_or(center_to, |f| st.pos[f]);
        let shift_to = if focused.is_some() {
            (0.0, 0.0)
        } else {
            shift_to
        };
        // While the mode switches, the blend of the two framings moves as
        // smoothly as the switch does: followed, not eased after.
        if reframe {
            st.fit_base = fit_to;
            st.center = center_to;
            st.shift = shift_to;
            st.shift_vel = (0.0, 0.0);
            st.shift_goal = shift_to;
        } else {
            let step = ease_step(dt, FRAME_TAU);
            st.fit_base += (fit_to - st.fit_base) * step;
            for (now, to) in st.center.iter_mut().zip(center_to) {
                *now += (to - *now) * step;
            }
            // The re-centring jumps its target when the camera starts or
            // stops: the target eased in, then a critically damped spring
            // takes it there — from rest, its acceleration building.
            let lead = ease_step(dt, LEAD_TAU);
            let goal = (
                st.shift_goal.0 + (shift_to.0 - st.shift_goal.0) * lead,
                st.shift_goal.1 + (shift_to.1 - st.shift_goal.1) * lead,
            );
            st.shift_goal = goal;
            let (x, vx) = spring(st.shift.0 - goal.0, st.shift_vel.0, SHIFT_RATE, dt);
            let (y, vy) = spring(st.shift.1 - goal.1, st.shift_vel.1, SHIFT_RATE, dt);
            st.shift = (goal.0 + x, goal.1 + y);
            st.shift_vel = (vx, vy);
        }
        let near_fit = (st.fit_base / fit_to - 1.0).abs() < 1e-3;
        let near_center = (0..3).all(|c| (st.center[c] - center_to[c]).abs() < 0.05);
        let near_shift = (st.shift.0 - shift_to.0).abs() < 0.05
            && (st.shift.1 - shift_to.1).abs() < 0.05
            && st.shift_vel.0.abs() < 0.5
            && st.shift_vel.1.abs() < 0.5
            && (st.shift_goal.0 - shift_to.0).abs() < 0.05
            && (st.shift_goal.1 - shift_to.1).abs() < 0.05;
        st.framing = !(near_fit && near_center && near_shift);
        if !st.framing {
            st.fit_base = fit_to;
            st.center = center_to;
            st.shift = shift_to;
            st.shift_vel = (0.0, 0.0);
            st.shift_goal = shift_to;
        }
        st.fit_scale = st.fit_base.max(1e-4) * st.zoom;
        st.fit_off = (bounds.width * 0.5, bounds.height * 0.5);

        // --- Label opacities ------------------------------------------------
        // Compute each label's *target* opacity (depth fade × whether the
        // declutter keeps it) and side, then ease the stored ones toward
        // them — so the depth fade, the discrete declutter flips and a label
        // changing sides all resolve as cross-fades instead of pops.
        let lproj = self.project_all(bounds, st);
        let (mut minz, mut maxz) = (f32::MAX, f32::MIN);
        for p in &lproj {
            minz = minz.min(p.2);
            maxz = maxz.max(p.2);
        }
        let span = (maxz - minz).max(1.0);
        // The depth fade thins a crowd of labels; a few nodes keep all
        // their names. Measured against the nodes' own depths, it took the
        // farthest label away whatever the map, and a map of three files
        // named one of them. Full from a few dozen nodes up.
        let depth_fade = e * ((n as f32 - FADE_FREE_NODES) / FADE_FULL_NODES).clamp(0.0, 1.0);
        // Priority: hovered first, then what a search found (the node in
        // focus before the rest), then nearest the camera.
        let lit = self.lit.as_ref();
        let first = |i: usize| -> u8 {
            u8::from(hovered == Some(i)) * 4
                + u8::from(lit.is_some_and(|l| l.focus == Some(i))) * 2
                + u8::from(lit.is_some_and(|l| l.found.get(i) == Some(&true)))
        };
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            first(b)
                .cmp(&first(a))
                .then(lproj[b].2.total_cmp(&lproj[a].2))
        });
        let mut placed: Vec<iced::Rectangle> = Vec::new();
        let mut target = vec![0.0f32; n];
        let mut side = st.label_side_target.clone();
        side.resize(n, 0.0);
        for &i in &order {
            let (x, y, z, persp) = lproj[i];
            // The node in focus is named as the hovered one is: whatever
            // its depth, over whatever its label would cover.
            let is_hover = hovered == Some(i) || lit.is_some_and(|l| l.focus == Some(i));
            let fade = if is_hover {
                1.0
            } else {
                1.0 - depth_fade + depth_fade * smoothstep(0.34, 0.82, (z - minz) / span)
            };
            let r = node_radius(self.layout.nodes[i].weight, persp);
            // The width the label is actually drawn at, not a per-char guess.
            let width = st.label_w.get(i).copied().unwrap_or(0.0);
            // Left of its node when it would run off the right edge — and
            // back only once there is room to spare, so a node near the edge
            // does not flick its label from side to side.
            let past = x + r + 3.0 + width - (bounds.width - 2.0);
            side[i] = if side[i] >= 0.5 {
                if past > -LABEL_SIDE_SLACK { 1.0 } else { 0.0 }
            } else if past > 0.0 {
                1.0
            } else {
                0.0
            };
            if fade <= 0.006 {
                continue; // too deep to bother placing
            }
            let left = side[i] >= 0.5;
            let spot = if is_hover {
                Some((label_rect(x, y, r, width, left), left))
            } else {
                label_spot(&placed, x, y, r, width, left, bounds.width)
            };
            if let Some((rect, left)) = spot {
                side[i] = if left { 1.0 } else { 0.0 };
                target[i] = fade;
                // Only a (near) fully-shown label reserves space, so a barely
                // there one can't hide a solid neighbour.
                if is_hover || fade > 0.6 {
                    placed.push(rect);
                }
            }
        }
        let step = ease_step(dt, LABEL_TAU);
        for (now, to) in st
            .label_alpha
            .iter_mut()
            .zip(&target)
            .chain(st.label_side.iter_mut().zip(&side))
        {
            *now += (to - *now) * step;
            if (*now - to).abs() <= LABEL_EPS {
                *now = *to;
            }
        }
        st.label_target = target;
        st.label_side_target = side;
        !graph_settled(st)
    }

    /// Draw the scene: edges, then nodes far → near, then labels — all but the
    /// `hovered` node and its label, which the caller draws on top.
    fn draw_scene(
        &self,
        frame: &mut iced::widget::canvas::Frame,
        st: &GraphState,
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
            let d = 1.0 - st.dim + st.dim * (near(az) + near(bz)) * 0.5;
            // A search on: the links of the node in focus lit, links between
            // what it lit as they are, the rest stepped back.
            let (d, toward) = match &self.lit {
                Some(lit) if lit.focus == Some(a) || lit.focus == Some(b) => (1.0, true),
                Some(lit) if lit.lit(a) && lit.lit(b) => (d, false),
                Some(_) => (d * DIMMED, false),
                None => (d, false),
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
            let (edge, width) = if toward {
                (theme::with_alpha(theme::accent(), 0.75), 1.4)
            } else {
                (
                    theme::with_alpha(theme::graph_edge(), 0.16 + 0.34 * d),
                    0.6 + d * 0.4,
                )
            };
            frame.stroke(
                &Path::line(iced::Point::new(sx, sy), iced::Point::new(tx, ty)),
                Stroke::default().with_width(width).with_color(edge),
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
            let la = match &self.lit {
                Some(lit) if !lit.lit(i) => la * DIMMED,
                _ => la,
            };
            if la >= 0.01 {
                let side = st.label_side.get(i).copied().unwrap_or(0.0);
                self.draw_label(frame, i, p, radius(i), la, side, false);
            } else {
                self.keep_label(frame, i, p);
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
        // A search on: what it did not find steps back.
        let lit = self.lit.as_ref();
        let color = match lit {
            Some(lit) if !hover && !lit.lit(i) => theme::with_alpha(color, DIMMED),
            _ => color,
        };
        frame.fill(&Path::circle(iced::Point::new(x, y), r), color);
        // What it found is ringed in the accent; the one the camera is on,
        // more strongly.
        if let Some(lit) = lit {
            let (pad, width) = if lit.focus == Some(i) {
                (3.5, 2.4)
            } else if lit.found.get(i) == Some(&true) {
                (2.5, 1.4)
            } else {
                (0.0, 0.0)
            };
            if width > 0.0 {
                frame.stroke(
                    &Path::circle(iced::Point::new(x, y), r + pad),
                    Stroke::default()
                        .with_width(width)
                        .with_color(theme::accent()),
                );
            }
        }
        if self.visited.as_ref().is_some_and(|v| v.contains(&nd.file)) {
            // The last run stopped here: a ring in the live colour.
            frame.stroke(
                &Path::circle(iced::Point::new(x, y), r + 2.5),
                Stroke::default()
                    .with_width(1.6)
                    .with_color(theme::with_alpha(theme::success(), 0.9)),
            );
        }
        if nd.cyclic {
            frame.stroke(
                &Path::circle(iced::Point::new(x, y), r + 1.5),
                Stroke::default()
                    .with_width(1.3)
                    .with_color(theme::with_alpha(theme::warning(), 0.75)),
            );
        }
    }

    /// Keep a faded-out label's texture on the GPU: drawn invisibly, at a
    /// point, so it stays in the renderer's image atlas. One a frame does
    /// not draw is dropped from it as soon as any other image is added,
    /// and every label fading back in as the map turned was uploaded
    /// again, evicting others — work on every frame of a spin.
    fn keep_label(
        &self,
        frame: &mut iced::widget::canvas::Frame,
        i: usize,
        (x, y, _, _): (f32, f32, f32, f32),
    ) {
        let nd = &self.layout.nodes[i];
        if let Some(tex) = super::graph_labels::label_texture(&nd.label, theme::fg_muted()) {
            frame.draw_image(
                iced::Rectangle::new(iced::Point::new(x, y), iced::Size::new(1.0, 1.0)),
                iced::advanced::image::Image::new(tex.handle).opacity(0.0),
            );
        }
    }

    /// One node label at opacity `alpha`, on the node's right or — `side`
    /// 1 — its left; between, on both, cross-faded, as a label changing
    /// sides is drawn.
    #[allow(clippy::too_many_arguments)]
    fn draw_label(
        &self,
        frame: &mut iced::widget::canvas::Frame,
        i: usize,
        (x, y, _, _): (f32, f32, f32, f32),
        r: f32,
        alpha: f32,
        side: f32,
        hover: bool,
    ) {
        for (left, share) in [(false, 1.0 - side), (true, side)] {
            let a = alpha.min(1.0) * share;
            if a >= 0.01 {
                self.draw_label_at(frame, i, (x, y), r, a, left, hover);
            }
        }
    }

    /// One node label at opacity `alpha`, right of its node or `left` of it.
    #[allow(clippy::too_many_arguments)]
    fn draw_label_at(
        &self,
        frame: &mut iced::widget::canvas::Frame,
        i: usize,
        (x, y): (f32, f32),
        r: f32,
        alpha: f32,
        left: bool,
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
                let bx = if left {
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
                        .opacity(alpha),
                );
            }
            None => {
                let (text_x, align_x) = if left {
                    (x - r - 3.0, iced::alignment::Horizontal::Right)
                } else {
                    (x + r + 3.0, iced::alignment::Horizontal::Left)
                };
                frame.fill_text(Text {
                    content: nd.label.clone(),
                    position: iced::Point::new(text_x, y),
                    color: theme::with_alpha(color, alpha),
                    size: super::graph_labels::SIZE.into(),
                    align_x: align_x.into(),
                    align_y: iced::alignment::Vertical::Center,
                    ..Text::default()
                });
            }
        }
    }
}

/// Where a label goes beside its node at `(x, y)` of radius `r`: right of
/// it, or `left`.
fn label_rect(x: f32, y: f32, r: f32, width: f32, left: bool) -> iced::Rectangle {
    iced::Rectangle {
        x: if left {
            x - r - 3.0 - width
        } else {
            x + r + 3.0
        },
        y: y - 6.5,
        width,
        height: 13.0,
    }
}

/// A free place for a node's label among those `placed`: on its side
/// (`left` or right), else on the other where that is free and inside the
/// frame — a label its neighbour's took the place of was hidden, and left a
/// node unnamed with room to spare across it. `None`: no room either side.
fn label_spot(
    placed: &[iced::Rectangle],
    x: f32,
    y: f32,
    r: f32,
    width: f32,
    left: bool,
    frame_w: f32,
) -> Option<(iced::Rectangle, bool)> {
    let free = |rect: iced::Rectangle| !placed.iter().any(|p| rects_overlap(*p, rect));
    let own = label_rect(x, y, r, width, left);
    if free(own) {
        return Some((own, left));
    }
    let other = label_rect(x, y, r, width, !left);
    let inside = other.x >= 2.0 && other.x + other.width <= frame_w - 2.0;
    (inside && free(other)).then_some((other, !left))
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
            .and_then(|c| self.hit(c, bounds, state))
            .or(state.hold);
        // The scene is cached: a settled map redraws (another widget animating,
        // the cursor moving elsewhere) without re-tessellating a single edge.
        // `tick` and every interaction clear it; so does a change of the hovered
        // node, of the theme or of the layout's content, which it is drawn for.
        let key = (
            hovered,
            theme::revision(),
            self.rev,
            self.paint_rev,
            self.visited_rev,
            self.lit.as_ref().map_or(0, |lit| lit.key),
        );
        if state.scene_key.get() != Some(key) {
            state.scene.clear();
            state.scene_key.set(Some(key));
        }
        let scene = state.scene.draw(renderer, bounds.size(), |frame| {
            self.draw_scene(frame, state, &proj, hovered);
        });
        // The hovered node and its label, on top.
        let mut top = Frame::new(renderer, bounds.size());
        if let Some(i) = hovered.filter(|&i| i < proj.len()) {
            let r = node_radius(self.layout.nodes[i].weight, proj[i].3);
            self.draw_node(&mut top, i, proj[i], r, true);
            let la = state.label_alpha.get(i).copied().unwrap_or(1.0);
            let side = state.label_side.get(i).copied().unwrap_or(0.0);
            self.draw_label(&mut top, i, proj[i], r, la.max(0.01), side, true);
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
        // The camera put back where it starts, asked from the header.
        if state.fit_rev != self.fit_rev {
            state.fit_rev = self.fit_rev;
            state.focus = None;
            state.zoom_target = 1.0;
            state.zoom_at = None;
            state.zoom_vel = 0.0;
            state.pan = iced::Vector::new(0.0, 0.0);
            state.yaw = 0.6;
            state.pitch = 0.35;
            state.yaw_vel = 0.0;
            state.pitch_vel = 0.0;
            state.touch();
            if !matches!(
                event,
                iced::Event::Window(iced::window::Event::RedrawRequested(_))
            ) {
                return Some(Action::request_redraw());
            }
        }
        // The search sent the camera to a node — zoomed in on it, centred
        // (`tick`), the spin held — or, cleared, back over the whole map.
        let lit_key = self.lit.as_ref().map_or(0, |lit| lit.key);
        if state.focus_rev != self.focus_rev || state.lit_key != lit_key {
            if state.focus_rev != self.focus_rev {
                state.focus_rev = self.focus_rev;
                let focus = self
                    .lit
                    .as_ref()
                    .and_then(|lit| lit.focus)
                    .filter(|&f| f < self.layout.nodes.len());
                state.focus = focus;
                state.zoom_target = if focus.is_some() {
                    state.zoom_target.max(FOCUS_ZOOM)
                } else {
                    1.0
                };
                state.zoom_at = None;
                state.pan = iced::Vector::new(0.0, 0.0);
            }
            state.lit_key = lit_key;
            state.touch();
            if !matches!(
                event,
                iced::Event::Window(iced::window::Event::RedrawRequested(_))
            ) {
                return Some(Action::request_redraw());
            }
        }
        match event {
            // Drive the live simulation: one step per frame while anything
            // moves, requesting the next frame only then — a settled map stops
            // asking.
            iced::Event::Window(iced::window::Event::RedrawRequested(now)) => self
                .tick(state, bounds, *now, cursor)
                .then(Action::request_redraw),
            // A hover change re-ranks the labels (the hovered one always
            // shows), so let the next frame recompute.
            // A node holding the spin is let go once the pointer leaves it:
            // the next frame decides.
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.drag == Drag::None => {
                let hovered = cursor
                    .position_in(bounds)
                    .and_then(|c| self.hit(c, bounds, state));
                (hovered != state.last_hover || state.hold.is_some()).then(Action::request_redraw)
            }
            iced::Event::Mouse(mouse::Event::CursorLeft) if state.hold.is_some() => {
                Some(Action::request_redraw())
            }
            // Zoom (dolly) — only where enabled; the embedded Overview map lets
            // the wheel fall through to the page.
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) if self.scroll_zooms => {
                cursor.position_in(bounds)?;
                let at = cursor.position_in(bounds)?;
                let dy = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / 40.0,
                };
                // Eased to by the ticks; flat, the point under the pointer
                // stays where it is.
                state.zoom_target = (state.zoom_target * (1.0 + dy * 0.12)).clamp(0.3, 5.0);
                state.zoom_at = Some(iced::Vector::new(
                    at.x - bounds.width * 0.5,
                    at.y - bounds.height * 0.5,
                ));
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
                // A grab holds the camera: no fling or spin under the hand.
                state.yaw_vel = 0.0;
                state.pitch_vel = 0.0;
                state.spin_goal = 0.0;
                state.last_yaw = state.yaw;
                state.last_pitch = state.pitch;
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
                        state.pitch = (state.pitch + dy * 0.008).clamp(-PITCH_LIMIT, PITCH_LIMIT);
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
                // A grab that barely moved is a click — on a node still part of
                // the layout on screen: the first picks it (what it is, over
                // the map), a second opens it.
                if let Drag::Node(i, sig) = was
                    && state.moved < 5.0
                    && sig == state.sig
                    && sig == layout_sig(self.layout)
                {
                    let msg = if state.picked == Some((i, sig)) {
                        self.open_message(i)
                    } else {
                        state.picked = Some((i, sig));
                        self.pick_message(i)
                    };
                    if let Some(msg) = msg {
                        return Some(Action::publish(msg).and_capture());
                    }
                }
                // A click off the nodes puts the picked one down.
                if !matches!(was, Drag::Node(..)) && state.moved < 5.0 && state.picked.is_some() {
                    state.picked = None;
                    return Some(
                        Action::publish(Message::Graph(GraphMsg::Picked(None))).and_capture(),
                    );
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
/// A file at the project's root has no directory to name: none is shown
/// (it said ".", which reads as a stray mark).
pub(crate) fn import_file_row<'a>(app: &'a App, file: &RankedFile) -> Element<'a, Message> {
    let path = file.path.as_path();
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let dir = std::path::Path::new(&rel_of(app, path))
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
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
    .on_press(super::heard::row(
        || path.display().to_string(),
        Message::Graph(GraphMsg::OverlayOpenImports(path.to_path_buf())),
    ))
    .into()
}

pub(crate) fn project_imports_body(app: &App) -> Element<'_, Message> {
    let g = &app.proj.import_graph;
    if g.is_empty() {
        // An empty graph while the project is still being read — or its
        // imports resolved, which the import job does after the index — is
        // not a finding: say it is being built rather than "nothing found".
        let work = &app.proj.import_work;
        let msg = if app.scanning || app.proj.indexing {
            tr!("Indexing the project…")
        } else if work.running || !work.pending.is_empty() {
            tr!("Resolving imports…")
        } else {
            tr!("No imports found in this project.")
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
        text(
            [
                trn!("1 file", "{n} files", ranks.files),
                trn!(
                    "1 internal edge",
                    "{n} internal edges",
                    ranks.internal_edges
                ),
                trn!(
                    "1 external package",
                    "{n} external packages",
                    ranks.externals.len()
                ),
                trn!("1 cycle", "{n} cycles", app.proj.import_cycles.len()),
            ]
            .join(" · "),
        )
        .size(ts::BODY)
        .color(theme::accent())
        .into(),
    );

    // Cycles — a real structural smell worth surfacing first.
    if !app.proj.import_cycles.is_empty() {
        rows.push(section_header(tr!("Import cycles")));
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
    rows.push(section_header(tr!("Most depended-on (fan-in)")));
    for file in &ranks.by_fan_in {
        rows.push(import_file_row(app, file));
    }

    // Most dependencies (highest fan-out).
    rows.push(section_header(tr!("Most dependencies (fan-out)")));
    for file in &ranks.by_fan_out {
        rows.push(import_file_row(app, file));
    }

    rows.extend(churn_rows(app));

    // External packages the project pulls in.
    if !ranks.externals.is_empty() {
        rows.push(section_header(tr!("External packages")));
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
    .on_press(super::heard::row(
        || n.name.clone(),
        Message::Graph(GraphMsg::OverlayOpenAt {
            abs: n.file.clone(),
            line: n.line,
        }),
    ))
    .into()
}

pub(crate) fn project_calls_body(app: &App) -> Element<'_, Message> {
    let g = &app.proj.project_calls.graph;
    if g.is_empty() {
        // Empty while a build runs, or while the project is still being read
        // (the graph is linked from its symbol index), is not a finding: only
        // a build over the whole index says there are no functions.
        let msg = if app.proj.project_calls.building {
            tr!("Building call graph…")
        } else if app.scanning || app.proj.indexing {
            tr!("Indexing the project…")
        } else {
            tr!("No functions found in this project.")
        };
        return container(text(msg).size(ts::BODY).color(theme::dim()))
            .padding(8)
            .into();
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(
        text(format!(
            "{} · {}",
            trn!("1 function", "{n} functions", g.node_count()),
            trn!("1 call edge", "{n} call edges", g.edge_count()),
        ))
        .size(ts::BODY)
        .color(theme::accent())
        .into(),
    );
    // What the refinement left out, if anything: how precise the calls
    // are is the header's badge.
    if let Some(missing) = app.precise_summary() {
        rows.push(text(missing).size(ts::CAPTION).color(theme::dim()).into());
    }

    // Ranking walks the whole graph (and, for the test filter, the symbol
    // index): memoized per graph and index generation instead of redone per
    // repaint.
    let summary = app.proj.view_memo.calls_summary.get_or(
        (app.proj.project_calls.graph_rev, app.proj.symbol_index_rev),
        || calls_summary(app, g),
    );

    // Most-called functions (hubs), unique names only so the counts mean something.
    if !summary.hubs.is_empty() {
        rows.push(section_header(tr!("Most called (unique names)")));
        for &(id, callers) in &summary.hubs {
            rows.push(call_symbol_row(
                app,
                id,
                trn!("1 caller", "{n} callers", callers),
            ));
        }
    }

    let visits = trace_visits(app, g);
    if !visits.is_empty() {
        rows.push(section_header(tr!("Where the last debug run stopped")));
        for &(id, stops) in visits.iter().take(40) {
            rows.push(call_symbol_row(app, id, trn!("1 stop", "{n} stops", stops)));
        }
    }

    if !summary.entries.is_empty() {
        rows.push(section_header(tr!(
            "Entry points (main, routes, commands, handlers)"
        )));
        for &(id, kind) in summary.entries.iter().take(40) {
            rows.push(call_symbol_row(
                app,
                id,
                crate::i18n::lookup(kind).to_string(),
            ));
        }
        if summary.entries.len() > 40 {
            rows.push(
                container(
                    text(tr!("… and {n} more", n = summary.entries.len() - 40))
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
    rows.push(section_header(tr!("Uncalled (possibly dead)")));
    for &(id, out) in uncalled.iter().take(60) {
        rows.push(call_symbol_row(app, id, format!("→{out}")));
    }
    if uncalled.len() > 60 {
        rows.push(
            container(
                text(tr!("… and {n} more", n = uncalled.len() - 60))
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

/// A project graph's English — an edge's label, a line of a node's detail,
/// as `clew_core::maps` writes them — in the reader's language: a phrase
/// the catalogs know (`extern_text::MAP_WORDS`), or one of the shapes below
/// around a name. Code (`pub use`, `select`, `gen_server:call`) and names
/// stay as written.
pub(crate) fn map_text(english: &str) -> String {
    let said = crate::i18n::lookup(english);
    if said != english {
        return said.to_string();
    }
    let number = |s: &str| s.parse::<usize>().ok();
    if let Some(n) = english.strip_prefix("base ").and_then(number) {
        return tr!("base {n}", n = n).to_string();
    }
    if let Some(n) = english.strip_suffix(" lemmas").and_then(number) {
        return trn!("1 lemma", "{n} lemmas", n).to_string();
    }
    if let Some(order) = english.strip_prefix("MRO: ") {
        return tr!("MRO: {order}", order = order).to_string();
    }
    if let Some(scope) = english.strip_prefix("scope: ") {
        return tr!("scope: {scope}", scope = scope).to_string();
    }
    if let Some(props) = english.strip_prefix("props: ") {
        return tr!("props: {props}", props = props).to_string();
    }
    if let Some(sources) = english.strip_prefix("reads ") {
        return tr!("reads {sources}", sources = sources).to_string();
    }
    if let Some(kind) = english.strip_prefix("supervisor · ") {
        return tr!("supervisor · {kind}", kind = kind).to_string();
    }
    if let Some(name) = english.strip_prefix("alias of ") {
        return tr!("alias of {name}", name = name).to_string();
    }
    if let Some(target) = english.strip_prefix("delegate to ") {
        return tr!("delegate to {target}", target = target).to_string();
    }
    if let Some(module) = english.strip_prefix("imports ") {
        return tr!("imports {module}", module = module).to_string();
    }
    if let Some(lifetime) = english
        .strip_prefix("resolves to (")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return tr!("resolves to ({lifetime})", lifetime = lifetime).to_string();
    }
    if let Some((what, line)) = english.rsplit_once(" made at line ")
        && let Some(line) = number(line)
    {
        return tr!("{what} made at line {line}", what = what, line = line).to_string();
    }
    if let Some((project, system)) = english
        .strip_prefix("pulls in ")
        .and_then(|rest| rest.strip_suffix(" system headers"))
        .and_then(|rest| rest.split_once(" project files and "))
        && let (Some(project), Some(system)) = (number(project), number(system))
    {
        return tr!(
            "pulls in {project} project files and {system} system headers",
            project = project,
            system = system
        )
        .to_string();
    }
    // A column's line: `id serial · primary key → users`.
    match english.split_once(" · primary key") {
        Some((column, rest)) => format!("{column} · {}{rest}", tr!("primary key")),
        None => english.to_string(),
    }
}

/// The type map's list: how many types and relations, then the types most
/// others name, the base types with the most subtypes, the types naming the
/// most others, and the most changed files. Each type row opens its
/// definition.
pub(crate) fn project_types_body(app: &App) -> Element<'_, Message> {
    let g = &app.proj.type_graph;
    if g.is_empty() {
        // Said for what it is: the map is drawn from the API index, and an
        // index never built is no project without types — it said "No types
        // found" of a project of a thousand.
        let never_built = app.proj.docs.generation == 0;
        let (msg, build) = if app.docs_loading() {
            (tr!("Building the API docs…"), false)
        } else if app.scanning || app.proj.indexing {
            (tr!("Indexing the project…"), false)
        } else if never_built && !app.server.is_up() {
            (
                tr!(
                    "The type graph is drawn from the project's API index, which Clew's server builds — and it is not running."
                ),
                false,
            )
        } else if never_built {
            (
                tr!("The type graph is drawn from the project's API index, not built yet."),
                true,
            )
        } else {
            (tr!("No types found in this project."), false)
        };
        let mut said = column![text(msg).size(ts::BODY).color(theme::dim())].spacing(10);
        if build {
            said = said.push(
                button(text(tr!("Build the API Index")).size(ts::BODY))
                    .style(theme::primary_button)
                    .padding([4, 12])
                    .on_press(super::heard::said(
                        tr!("Build the API Index"),
                        Message::Docs(DocsMsg::Refresh),
                    )),
            );
        }
        return container(said).padding(8).into();
    }
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(
        text(format!(
            "{} · {} · {}",
            trn!("1 type", "{n} types", g.node_count()),
            trn!("1 relation", "{n} relations", g.edge_count()),
            trn!(
                "{n} inheritance link",
                "{n} inheritance links",
                g.inherits_count()
            ),
        ))
        .size(ts::BODY)
        .color(theme::accent())
        .into(),
    );
    const TOP: usize = 12;
    let referenced = g.most_referenced(TOP);
    if !referenced.is_empty() {
        rows.push(section_header(tr!(
            "Most referenced (used by the most types)"
        )));
        for id in referenced {
            rows.push(type_row(app, id, format!("{} ←", g.fan_in(id))));
        }
    }
    let derived = g.most_derived(TOP);
    if !derived.is_empty() {
        rows.push(section_header(tr!("Base types (most subtypes)")));
        for id in derived {
            rows.push(type_row(
                app,
                id,
                trn!("1 subtype", "{n} subtypes", g.subtypes(id)),
            ));
        }
    }
    let dependent = g.most_dependent(TOP);
    if !dependent.is_empty() {
        rows.push(section_header(tr!(
            "Most dependencies (uses the most types)"
        )));
        for id in dependent {
            rows.push(type_row(app, id, format!("→ {}", g.fan_out(id))));
        }
    }
    rows.extend(churn_rows(app));
    scrollable(Column::with_children(rows).spacing(3).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(iced::Length::Fill)
        .into()
}

/// A map in the Maps overlay's picker: its title and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MapChoice {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) nodes: usize,
}

impl std::fmt::Display for MapChoice {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The title, from clew-core, in the reader's language.
        write!(out, "{} ({})", crate::i18n::lookup(&self.title), self.nodes)
    }
}

/// The picker of the project's maps, while there is more than one.
pub(crate) fn map_picker(app: &App) -> Option<Element<'_, Message>> {
    let maps = &app.proj.maps;
    let choices: Vec<MapChoice> = maps
        .iter()
        .map(|m| MapChoice {
            id: m.id.clone(),
            title: m.title.clone(),
            nodes: m.nodes.len(),
        })
        .collect();
    let selected = app
        .selected_map_index()
        .and_then(|i| choices.get(i).cloned());
    match choices.len() {
        // While they are read, the body says so — once: the header said it
        // too, a line above.
        0 => None,
        _ => Some(
            {
                // Each map said by its name.
                if super::heard::gathering() {
                    for c in &choices {
                        super::heard::said(
                            &c.title,
                            Message::Graph(GraphMsg::SelectMap(c.id.clone())),
                        );
                    }
                }
                pick_list(choices, selected, |c: MapChoice| {
                    Message::Graph(GraphMsg::SelectMap(c.id))
                })
            }
            .style(theme::pick)
            .menu_style(theme::pick_menu)
            .text_size(ts::SMALL)
            .padding([3, 8])
            .into(),
        ),
    }
}

/// The most nodes the Maps overlay lists (the graph shows them all).
const MAP_LIST_MAX: usize = 400;

/// The Maps overlay's list: the map's own line on how to read it, then its
/// nodes by kind — each opening where it is defined, with its detail and the
/// arrows from it.
pub(crate) fn project_maps_body(app: &App) -> Element<'_, Message> {
    let hint = |msg: String| -> Element<'_, Message> {
        container(text(msg).size(ts::BODY).color(theme::dim()))
            .padding(8)
            .into()
    };
    let Some(at) = app.selected_map_index() else {
        return hint(if let Some(e) = &app.proj.maps_error {
            tr!("Couldn't read the project's graphs: {error}", error = e)
        } else if app.proj.maps_building || app.scanning {
            tr!("Reading the project's graphs…").into()
        } else {
            tr!("No graphs for this project.").into()
        });
    };
    let root = app
        .proj
        .project
        .as_ref()
        .map(|p| p.root.clone())
        .unwrap_or_default();
    let map = &app.proj.maps[at];
    let mut rows: Vec<Element<'_, Message>> = vec![
        text(format!(
            "{} · {} · {}",
            crate::i18n::lookup(&map.title),
            trn!("1 node", "{n} nodes", map.nodes.len()),
            trn!("1 arrow", "{n} arrows", map.edges.len())
        ))
        .size(ts::BODY)
        .color(theme::accent())
        .into(),
        // Its line on how to read it, from clew-core: in the reader's
        // language where the catalog has it.
        text(crate::i18n::lookup(&map.description).to_string())
            .size(ts::CAPTION)
            .color(theme::dim())
            .into(),
    ];
    if let Some(e) = &app.proj.maps_error {
        rows.push(
            text(tr!("Could not read the graphs: {error}", error = e))
                .size(ts::CAPTION)
                .color(theme::warning())
                .into(),
        );
    }
    let mut out_edges: Vec<Vec<(usize, &str)>> = vec![Vec::new(); map.nodes.len()];
    for e in &map.edges {
        if let Some(list) = out_edges.get_mut(e.from)
            && e.to < map.nodes.len()
        {
            list.push((e.to, e.label.as_str()));
        }
    }
    let mut kinds: Vec<&str> = Vec::new();
    for n in &map.nodes {
        if !kinds.contains(&n.kind.as_str()) {
            kinds.push(&n.kind);
        }
    }
    let mut listed = 0;
    'kinds: for kind in kinds {
        // A kind of node, as clew-core names it ("table", "route").
        rows.push(section_header(map_kind_label(kind)));
        for (i, n) in map.nodes.iter().enumerate().filter(|(_, n)| n.kind == kind) {
            if listed == MAP_LIST_MAX {
                break 'kinds;
            }
            listed += 1;
            let place = match n.file.is_empty() {
                true => String::new(),
                false => format!("{}:{}", n.file, n.line.max(1)),
            };
            let head = row![
                text(n.label.clone()).size(ts::SMALL).color(theme::fg()),
                space().width(Fill),
                text(place).size(ts::CAPTION).color(theme::dim()),
            ]
            .spacing(6);
            let mut button = button(head)
                .style(theme::list_row(false))
                .width(Fill)
                .padding([3, 8]);
            if !n.file.is_empty() {
                button = button.on_press(super::heard::row(
                    || n.label.clone(),
                    Message::Graph(GraphMsg::OverlayOpenAt {
                        abs: root.join(&n.file),
                        line: n.line.max(1),
                    }),
                ));
            }
            rows.push(button.into());
            let mut lines: Vec<String> = n
                .detail
                .lines()
                .take(12)
                .map(|l| format!("  {}", map_text(l)))
                .collect();
            if n.detail.lines().count() > 12 {
                lines.push("  …".into());
            }
            for (to, label) in &out_edges[i] {
                let target = &map.nodes[*to].label;
                lines.push(match label.is_empty() {
                    true => format!("  → {target}"),
                    false => format!("  → {target} ({})", map_text(label)),
                });
            }
            if !lines.is_empty() {
                rows.push(
                    text(lines.join("\n"))
                        .size(ts::CAPTION)
                        .color(theme::dim())
                        .into(),
                );
            }
        }
    }
    if listed < map.nodes.len() {
        rows.push(
            text(trn!(
                "1 more — the graph view shows every node",
                "{n} more — the graph view shows every node",
                map.nodes.len() - listed
            ))
            .size(ts::CAPTION)
            .color(theme::dim())
            .into(),
        );
    }
    scrollable(Column::with_children(rows).spacing(3).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(iced::Length::Fill)
        .into()
}

/// One type in the type map's list: its name, kind and file, with a
/// trailing count; opens its definition.
pub(crate) fn type_row(app: &App, id: usize, trailing: String) -> Element<'_, Message> {
    let t = &app.proj.type_graph.nodes[id];
    button(
        row![
            text(t.name.clone()).size(ts::SMALL).color(theme::fg()),
            text(t.kind.clone()).size(ts::CAPTION).color(theme::dim()),
            text(t.rel.clone()).size(ts::CAPTION).color(theme::dim()),
            space().width(Fill),
            text(trailing).size(ts::CAPTION).color(theme::accent()),
        ]
        .spacing(6),
    )
    .style(theme::list_row(false))
    .width(Fill)
    .padding([3, 8])
    .on_press(super::heard::row(
        || t.name.clone(),
        Message::Graph(GraphMsg::OverlayOpenAt {
            abs: t.file.clone(),
            line: t.line,
        }),
    ))
    .into()
}

/// The project-calls overlay's rankings for graph `g`: the 15 most-called
/// functions with their caller counts, and the uncalled ones with their
/// callee counts. Test functions are left out of the uncalled list — they are
/// always "uncalled" (the harness invokes them, not project code), so they
/// would swamp it as false positives — and so is what the language reaches
/// without a call naming it ([`Unnamed::reaches`]).
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
    let unnamed = Unnamed::of(app, g);
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
            .filter(|&id| !unnamed.reaches(app, g, id))
            .map(|id| (id, g.node(id).callee_count()))
            .collect(),
        entries: entries
            .into_iter()
            .map(|(id, kind)| (id, kind.label()))
            .collect(),
    }
}

/// What the language reaches without a call naming it, for
/// [`Unnamed::reaches`]: the names any call site calls, and each called
/// method's class, by name.
struct Unnamed<'a> {
    called: std::collections::HashSet<&'a str>,
    called_in: std::collections::HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> Unnamed<'a> {
    fn of(app: &'a App, g: &'a crate::projectcalls::ProjectCallGraph) -> Unnamed<'a> {
        let called = app
            .proj
            .calls_by_file
            .values()
            .flat_map(|c| c.calls.iter().map(|site| site.callee.as_str()))
            .collect();
        let mut called_in: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for id in 0..g.node_count() {
            let n = g.node(id);
            if n.caller_count() == 0 {
                continue;
            }
            if let Some(class) = owner_of(app, &n.file, n.line) {
                called_in.entry(n.name.as_str()).or_default().push(class);
            }
        }
        Unnamed { called, called_in }
    }

    /// Whether node `id` of `g`, which no call in the graph reaches, is
    /// reached all the same, by the language rather than by a call naming it:
    /// - a Python property, read as an attribute (`order.amount`);
    /// - a Python special method (`__str__`, `__eq__`, `__enter__`), run by
    ///   the operation it stands for — and an `__init__`, run when its class
    ///   is called (`Inventory()`, `raise OutOfStock(...)`), when a call
    ///   anywhere names the class: a language server reports no call of it;
    /// - a method overriding one a call reaches (`rule.amount_off(...)` of a
    ///   `Discount` runs each subclass's), when another class's method of
    ///   the name is called.
    fn reaches(&self, app: &App, g: &crate::projectcalls::ProjectCallGraph, id: usize) -> bool {
        let node = g.node(id);
        let calls = app.proj.calls_by_file.get(&node.file);
        if calls.is_some_and(|c| c.accessed.contains(&node.line)) {
            return true;
        }
        let owner = owner_of(app, &node.file, node.line);
        let python = calls.is_some_and(|c| c.lang == crate::highlight::Lang::Python);
        let name = node.name.as_str();
        if python && name.len() > 4 && name.starts_with("__") && name.ends_with("__") {
            return match owner.filter(|_| name == "__init__") {
                Some(class) => self.called.contains(class),
                None => true,
            };
        }
        owner.is_some_and(|own| {
            self.called_in
                .get(name)
                .is_some_and(|classes| classes.iter().any(|&class| class != own))
        })
    }
}

/// The class the method defined at `line` of `file` belongs to, as the
/// index's call facts say.
fn owner_of<'a>(app: &'a App, file: &std::path::Path, line: usize) -> Option<&'a str> {
    app.proj
        .calls_by_file
        .get(file)
        .and_then(|c| c.owners.get(&line))
        .map(String::as_str)
}

/// The files the last debug run stopped in, for the map's rings; memoized
/// per trace revision. Every frame with a source counts, not only the
/// innermost: a file on the stack was run through.
pub(crate) fn trace_files(
    app: &App,
) -> std::sync::Arc<std::collections::HashSet<std::path::PathBuf>> {
    app.proj
        .view_memo
        .trace_files
        .get_or(app.debug.trace_rev, || {
            app.debug
                .trace
                .iter()
                .flat_map(|s| s.frames.iter().filter_map(|f| f.path.clone()))
                .collect()
        })
}

/// The call graph's nodes the last run stopped in — the innermost frame
/// of each stop that the graph knows — with their stop counts, most first
/// then by name; memoized per graph and trace revision.
pub(crate) fn trace_visits(
    app: &App,
    g: &crate::projectcalls::ProjectCallGraph,
) -> std::sync::Arc<Vec<(usize, usize)>> {
    app.proj.view_memo.trace_visits.get_or(
        (app.proj.project_calls.graph_rev, app.debug.trace_rev),
        || {
            let mut counts: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();
            for stop in &app.debug.trace {
                let Some(id) = stop.frames.iter().find_map(|f| {
                    let path = f.path.as_deref()?;
                    g.id_of(path, &crate::short_frame_name(&f.name))
                }) else {
                    continue;
                };
                *counts.entry(id).or_default() += 1;
            }
            let mut visits: Vec<(usize, usize)> = counts.into_iter().collect();
            visits.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| g.node(a.0).name.cmp(&g.node(b.0).name))
                    .then_with(|| g.node(a.0).line.cmp(&g.node(b.0).line))
            });
            visits
        },
    )
}

/// The MOST CHANGED section of an overlay's list: the files with the most
/// commits over the recent history, each with its count and how long ago it
/// last changed, opening the file; a note while the history loads; nothing
/// for a project without git.
pub(crate) fn churn_rows(app: &App) -> Vec<Element<'_, Message>> {
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let Some(churn) = app.proj.churn.as_deref() else {
        if app.proj.churn_loading {
            rows.push(section_header(tr!("Most changed")));
            rows.push(
                container(
                    text(tr!("Reading the change history…"))
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
    rows.push(section_header(&trn!(
        "Most changed (last commit)",
        "Most changed (last {n} commits)",
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
                "{} · {}",
                trn!("1 commit", "{n} commits", f.commits),
                crate::i18n::ago(f.last, now)
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
                .on_press(super::heard::row(
                    || path.display().to_string(),
                    Message::Graph(GraphMsg::OverlayOpenAt { abs, line: 1 }),
                ))
                .into(),
        );
    }
    rows
}

/// Files the MOST CHANGED section lists at most.
pub(crate) const CHURN_ROWS: usize = 12;

// -------------------------------------------------- explanation overlay

/// A kind of map node, as its list's heading says it: clew-core's word for
/// it, in the reader's language. The two that name something else elsewhere
/// (the debugger stops on a "step" and at an "entry") have headings of
/// their own.
fn map_kind_label(kind: &str) -> &str {
    match kind {
        "step" => tr!("Build step"),
        "entry" => tr!("Entry point"),
        other => crate::i18n::lookup(other),
    }
}

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
                    line: 1,
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

    /// A small map names every node however it is turned: the depth fade
    /// took the farthest of three files' labels away, and the middle one's
    /// was faint.
    #[test]
    fn a_small_map_names_every_node() {
        let l = layout(3);
        let c = canvas(&l, true, false);
        let mut st = GraphState::default();
        assert!(frames_to_rest(&c, &mut st, 5_000).is_some());
        assert!(
            st.label_target.iter().all(|&t| t > 0.99),
            "{:?}",
            st.label_target
        );
        // A crowd still thins by depth.
        let crowd = layout(60);
        let c = canvas(&crowd, true, false);
        let mut st = GraphState::default();
        assert!(frames_to_rest(&c, &mut st, 20_000).is_some());
        assert!(st.label_target.iter().any(|&t| t < 0.5));
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

    /// A click (press + release without moving) on a live node picks it —
    /// what it is, over the map — and a second click opens it: a first
    /// click closed the map on a node the reader may only have wanted to
    /// look at. A click off the nodes puts the picked one down.
    #[test]
    fn a_click_on_a_node_picks_it_and_a_second_opens_it() {
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
        let click = |st: &mut GraphState, at| {
            let program = |e: &iced::Event, st: &mut GraphState| {
                <GraphCanvas<'_> as iced::widget::canvas::Program<Message>>::update(
                    &c, st, e, BOUNDS, at,
                )
            };
            let _ = program(&press, st);
            program(&release, st)
                .expect("the release is handled")
                .into_inner()
                .0
        };
        match click(&mut st, at) {
            Some(Message::Graph(GraphMsg::Picked(Some(pick)))) => {
                assert_eq!(pick.file, std::path::PathBuf::from("/p/file_1.rs"));
                assert!(matches!(
                    *pick.open,
                    Message::Graph(GraphMsg::OverlayOpenImports(_))
                ));
            }
            other => panic!("expected the node to be picked, got {other:?}"),
        }
        match click(&mut st, at) {
            Some(Message::Graph(GraphMsg::OverlayOpenImports(file))) => {
                assert_eq!(file, std::path::PathBuf::from("/p/file_1.rs"))
            }
            other => panic!("expected the node to open, got {other:?}"),
        }
        // A click on nothing puts it down; the next on it picks it again.
        let off = iced::advanced::mouse::Cursor::Available(Point::new(2.0, 2.0));
        assert!(matches!(
            click(&mut st, off),
            Some(Message::Graph(GraphMsg::Picked(None)))
        ));
        assert!(matches!(
            click(&mut st, at),
            Some(Message::Graph(GraphMsg::Picked(Some(_))))
        ));
    }

    /// A frame at `at` ms past `t0`, the pointer where `cursor` says.
    fn frame_at(
        c: &GraphCanvas<'_>,
        st: &mut GraphState,
        t0: std::time::Instant,
        at: f64,
        cursor: iced::advanced::mouse::Cursor,
    ) {
        let now = t0 + std::time::Duration::from_secs_f64(at / 1000.0);
        let _ = c.tick(st, BOUNDS, now, cursor);
    }

    /// A node the pointer rests on eases the spin to a stop — no frame
    /// turning the camera much less than the one before — and holds it
    /// while the pointer stays by the node, which drifts on as the turn
    /// slows; the pointer gone, the spin picks up again as gently.
    #[test]
    fn a_hovered_node_eases_the_spin_to_a_stop_and_lets_it_go() {
        use iced::advanced::mouse::Cursor;
        let l = layout(8);
        let c = canvas(&l, true, true);
        let mut st = GraphState::default();
        let t0 = std::time::Instant::now();
        let mut t = 0.0;
        // The physics settled, the spin at its rate.
        while t < 12_000.0 {
            frame_at(&c, &mut st, t0, t, Cursor::Unavailable);
            t += 16.667;
        }
        assert!((st.yaw_vel - SPIN_RATE).abs() < 1e-3, "{}", st.yaw_vel);

        // Onto node 3.
        let (x, y, _, _) = c.node_screen(3, BOUNDS, &st);
        let on = Cursor::Available(iced::Point::new(x, y));
        let mut rates = vec![st.yaw_vel];
        for _ in 0..240 {
            frame_at(&c, &mut st, t0, t, on);
            t += 16.667;
            assert_eq!(st.hold, Some(3), "let go while the pointer stayed");
            rates.push(st.yaw_vel);
        }
        assert!(st.yaw_vel.abs() < 1e-3, "still turning: {}", st.yaw_vel);
        let steps: Vec<f32> = rates.windows(2).map(|w| w[0] - w[1]).collect();
        assert!(steps.iter().all(|d| *d >= -1e-6), "sped up on the way down");
        let jolt = steps.iter().cloned().fold(0.0f32, f32::max);
        assert!(jolt < SPIN_RATE * 0.1, "stopped with a jolt: {jolt}");

        // Off the node, past where it counts as hovered but still by it:
        // held. Farther: let go.
        let (x, y, _, persp) = c.node_screen(3, BOUNDS, &st);
        let r = node_radius(l.nodes[3].weight, persp);
        let by = iced::Point::new(x + r + HIT_SLOP + 6.0, y);
        assert_eq!(c.hit(by, BOUNDS, &st), None, "no node is hovered there");
        frame_at(&c, &mut st, t0, t, Cursor::Available(by));
        t += 16.667;
        assert_eq!(st.hold, Some(3), "let go by the node");
        let past = iced::Point::new(x + r + HOLD_SLACK + 6.0, y);
        if c.hit(past, BOUNDS, &st).is_none() {
            frame_at(&c, &mut st, t0, t, Cursor::Available(past));
            t += 16.667;
            assert_eq!(st.hold, None, "held from afar");
        }

        // Away: the spin comes back, as gently.
        let away = Cursor::Available(iced::Point::new(2.0, 2.0));
        let mut rates = vec![st.yaw_vel];
        for _ in 0..240 {
            frame_at(&c, &mut st, t0, t, away);
            t += 16.667;
            rates.push(st.yaw_vel);
        }
        assert_eq!(st.hold, None);
        assert!((st.yaw_vel - SPIN_RATE).abs() < 2e-3, "{}", st.yaw_vel);
        let steps: Vec<f32> = rates.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(steps.iter().all(|d| *d >= -1e-6));
        assert!(steps.iter().cloned().fold(0.0f32, f32::max) < SPIN_RATE * 0.1);
    }

    /// The spin keeps its pace through late frames: a second of it turns
    /// the camera as far at five frames a second as at sixty. Clamped as the
    /// physics step is, a late frame turned it less, and the next frames
    /// caught up — a stutter, then a spin.
    #[test]
    fn the_spin_keeps_its_pace_through_late_frames() {
        use iced::advanced::mouse::Cursor;
        let l = layout(8);
        let c = canvas(&l, true, true);
        let t0 = std::time::Instant::now();
        // A second of the spin, after the same warm-up, with a frame every
        // `gap` ms.
        let turned = |gap: f64| {
            let mut st = GraphState::default();
            let mut t = 0.0;
            while t < 12_000.0 {
                frame_at(&c, &mut st, t0, t, Cursor::Unavailable);
                t += 16.667;
            }
            let from = st.yaw;
            let end = t + 1000.0;
            while t < end {
                t += gap;
                frame_at(&c, &mut st, t0, t, Cursor::Unavailable);
            }
            st.yaw - from
        };
        let smooth = turned(16.667);
        let late = turned(200.0);
        assert!(
            (late - smooth).abs() < smooth * 0.05,
            "at 5 fps it turned {late}, at 60 fps {smooth}"
        );
    }

    /// A frame at `at` ms past `t0` as the canvas gets it: an update for the
    /// redraw, which also takes what the view changed (a search's focus).
    fn redraw_at(c: &GraphCanvas<'_>, st: &mut GraphState, t0: std::time::Instant, at: f64) {
        let now = t0 + std::time::Duration::from_secs_f64(at / 1000.0);
        let _ = <GraphCanvas<'_> as iced::widget::canvas::Program<Message>>::update(
            c,
            st,
            &iced::Event::Window(iced::window::Event::RedrawRequested(now)),
            BOUNDS,
            iced::advanced::mouse::Cursor::Unavailable,
        );
    }

    /// The search finds a name that is the query first, then one that
    /// starts with it — a part of its path counts — then one that holds it;
    /// the weightier first among each; case aside; nothing for nothing.
    #[test]
    fn the_search_finds_the_best_matches_first() {
        let mut l = layout(8);
        l.nodes[2].label = "src/pricing.py".into();
        l.nodes[3].label = "src/pricing_rules.py".into();
        l.nodes[4].label = "tests/test_pricing.py".into();
        l.nodes[5].label = "Pricing".into();
        assert_eq!(graph_matches(&l, "pricing"), [5, 2, 3, 4]);
        assert_eq!(graph_matches(&l, "PRICING_R"), [3]);
        assert_eq!(graph_matches(&l, "  "), Vec::<usize>::new());
        assert_eq!(graph_matches(&l, "nothing"), Vec::<usize>::new());
        // Among the same rank, the weightier first.
        assert_eq!(graph_matches(&l, "file"), [7, 6, 1, 0]);
    }

    /// What a search lights: what it found, and the node in focus with its
    /// neighbours — the rest is dimmed; nothing typed, nothing lit.
    #[test]
    fn the_search_lights_its_finds_and_the_focus_neighbours() {
        let l = layout(8);
        let c = canvas(&l, true, false).with_search("file_4", 0, 1);
        let lit = c.lit.as_ref().expect("a search");
        assert_eq!(lit.focus, Some(4));
        let on: Vec<usize> = (0..8).filter(|&i| lit.lit(i)).collect();
        assert_eq!(on, [3, 4, 5], "the found node and its neighbours");
        assert!(canvas(&l, true, false).with_search(" ", 0, 1).lit.is_none());
        // Round: the third of one match is that match.
        let c = canvas(&l, true, false).with_search("file_4", 2, 1);
        assert_eq!(c.lit.as_ref().unwrap().focus, Some(4));
    }

    /// The camera goes to what the search found: centred, zoomed in, the
    /// spin held; the search cleared, back over the whole map, spinning.
    #[test]
    fn the_camera_flies_to_what_the_search_found() {
        let l = layout(8);
        let t0 = std::time::Instant::now();
        let mut st = GraphState::default();
        let mut t = 0.0;
        let plain = canvas(&l, true, true);
        while t < 12_000.0 {
            redraw_at(&plain, &mut st, t0, t);
            t += 16.667;
        }
        let found = canvas(&l, true, true).with_search("file_6", 0, 1);
        while t < 15_000.0 {
            redraw_at(&found, &mut st, t0, t);
            t += 16.667;
        }
        let (x, y, _, _) = found.node_screen(6, BOUNDS, &st);
        let mid = (BOUNDS.width / 2.0, BOUNDS.height / 2.0);
        assert!(
            (x - mid.0).abs() < 1.0 && (y - mid.1).abs() < 1.0,
            "not centred: {x},{y}"
        );
        assert!(st.zoom >= FOCUS_ZOOM - 1e-3, "not zoomed in: {}", st.zoom);
        assert!(st.yaw_vel.abs() < 1e-3, "still spinning");

        let cleared = canvas(&l, true, true).with_search("", 0, 2);
        while t < 20_000.0 {
            redraw_at(&cleared, &mut st, t0, t);
            t += 16.667;
        }
        assert!((st.zoom - 1.0).abs() < 1e-3, "{}", st.zoom);
        assert!((st.yaw_vel - SPIN_RATE).abs() < 2e-3, "{}", st.yaw_vel);
        assert_eq!(st.focus, None);
    }

    /// Fit puts the camera back where it starts: zoom, pan and turn.
    #[test]
    fn fit_puts_the_camera_back() {
        let l = layout(3);
        let mut st = GraphState {
            zoom_target: 3.0,
            pan: iced::Vector::new(120.0, -40.0),
            yaw: 2.0,
            ..Default::default()
        };
        let c = canvas(&l, true, false).with_fit(1);
        let redraw = iced::Event::Window(iced::window::Event::RedrawRequested(
            std::time::Instant::now(),
        ));
        let _ = <GraphCanvas<'_> as iced::widget::canvas::Program<Message>>::update(
            &c,
            &mut st,
            &redraw,
            BOUNDS,
            iced::advanced::mouse::Cursor::Unavailable,
        );
        assert_eq!(st.zoom_target, 1.0);
        assert_eq!(st.pan, iced::Vector::new(0.0, 0.0));
        assert!((st.yaw - 0.6).abs() < 0.2, "{}", st.yaw);
        // Once: it is not put back again until asked.
        st.zoom_target = 2.0;
        let _ = <GraphCanvas<'_> as iced::widget::canvas::Program<Message>>::update(
            &c,
            &mut st,
            &redraw,
            BOUNDS,
            iced::advanced::mouse::Cursor::Unavailable,
        );
        assert_eq!(st.zoom_target, 2.0);
    }

    /// Where each node is drawn.
    fn on_screen(c: &GraphCanvas<'_>, st: &GraphState) -> Vec<(f32, f32)> {
        (0..c.layout.nodes.len())
            .map(|i| {
                let p = c.node_screen(i, BOUNDS, st);
                (p.0, p.1)
            })
            .collect()
    }

    /// Ticks `frames` frames at 60 fps, the first `from` frames after
    /// `t0`: how far the node that moved most moved in each.
    fn steps(
        c: &GraphCanvas<'_>,
        st: &mut GraphState,
        t0: std::time::Instant,
        from: usize,
        frames: usize,
    ) -> Vec<f32> {
        let mut last = on_screen(c, st);
        (from..from + frames)
            .map(|f| {
                let now = t0 + std::time::Duration::from_micros(16_667 * f as u64);
                let _ = c.tick(st, BOUNDS, now, iced::advanced::mouse::Cursor::Unavailable);
                let here = on_screen(c, st);
                let most = here
                    .iter()
                    .zip(&last)
                    .map(|(a, b)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt())
                    .fold(0.0f32, f32::max);
                last = here;
                most
            })
            .collect()
    }

    /// The sharpest change of speed between two frames, px per frame, from
    /// a speed of `before`.
    fn jolt(before: f32, steps: &[f32]) -> f32 {
        std::iter::once(before)
            .chain(steps.iter().copied())
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    /// A map left to settle in the mode asked for, and when it got there.
    fn settled(l: &Layout, is_3d: bool) -> (GraphState, std::time::Instant, usize) {
        let mut st = GraphState::default();
        let t0 = std::time::Instant::now();
        let c = canvas(l, is_3d, false);
        let _ = steps(&c, &mut st, t0, 0, 1500);
        assert!(graph_settled(&st), "the map did not settle");
        (st, t0, 1500)
    }

    /// Switching between 3D and 2D moves the camera: it gathers speed and
    /// settles — no frame jumps. It used to snap the whole picture in one
    /// frame (over 100 px), the "flattening" after it unseen.
    #[test]
    fn switching_between_3d_and_2d_is_a_camera_move_not_a_jump() {
        let l = layout(40);
        let (mut st, t0, f) = settled(&l, true);
        let to_2d = steps(&canvas(&l, false, false), &mut st, t0, f, 60);
        assert!(to_2d[0] < 1.0, "it starts gently: {to_2d:?}");
        assert!(jolt(0.0, &to_2d) < 2.5, "{to_2d:?}");
        assert!(
            to_2d[40..].iter().all(|&d| d < 0.05),
            "and arrives: {to_2d:?}"
        );
        let _ = steps(&canvas(&l, false, false), &mut st, t0, f + 60, 900);
        assert!(graph_settled(&st));
        assert_eq!(st.dim, 0.0);
        let to_3d = steps(&canvas(&l, true, false), &mut st, t0, f + 960, 60);
        assert!(to_3d[0] < 1.5, "{to_3d:?}");
        assert!(jolt(0.0, &to_3d) < 3.0, "{to_3d:?}");
    }

    /// A step of the wheel eases the zoom in and settles on what was asked;
    /// flat, the point under the pointer stays where it is.
    #[test]
    fn the_wheel_zoom_eases_and_keeps_the_point_under_the_pointer() {
        let l = layout(40);
        let (mut st, t0, f) = settled(&l, false);
        let c = canvas(&l, false, false);
        let (x, y, _, _) = c.node_screen(7, BOUNDS, &st);
        let at = iced::advanced::mouse::Cursor::Available(Point::new(x, y));
        let wheel = iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
            delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
        });
        let _ = <GraphCanvas<'_> as iced::widget::canvas::Program<Message>>::update(
            &c, &mut st, &wheel, BOUNDS, at,
        );
        assert_eq!(st.zoom, 1.0, "not at once");
        let zoom = steps(&c, &mut st, t0, f, 60);
        assert!(zoom[0] < 2.0, "it starts gently: {zoom:?}");
        assert!(jolt(0.0, &zoom) < 2.5, "{zoom:?}");
        assert!(graph_settled(&st), "settled");
        assert!((st.zoom - 1.12).abs() < 1e-4, "{}", st.zoom);
        let (x2, y2, _, _) = c.node_screen(7, BOUNDS, &st);
        assert!(
            (x2 - x).abs() < 0.5 && (y2 - y).abs() < 0.5,
            "the node under the pointer moved: ({x}, {y}) → ({x2}, {y2})"
        );
    }

    /// A node grabbed in a still 3D map stays under the pointer, held still
    /// or pulled out: the camera holds its framing while the node is held.
    /// The grab used to re-centre the picture on the pivot, as a turn does,
    /// sliding the node out from under a pointer held still, and the camera
    /// followed a node pulled out.
    #[test]
    fn a_node_held_in_a_still_3d_map_stays_under_the_pointer() {
        let l = layout(40);
        let (mut st, t0, f) = settled(&l, true);
        assert!(
            st.shift.0.abs() + st.shift.1.abs() > 1.0,
            "precondition: at rest the picture is centred off the pivot: {:?}",
            st.shift
        );
        let c = canvas(&l, true, false);
        let (x, y, _, _) = c.node_screen(7, BOUNDS, &st);
        let program = |e: &iced::Event, st: &mut GraphState, at: Point| {
            <GraphCanvas<'_> as iced::widget::canvas::Program<Message>>::update(
                &c,
                st,
                e,
                BOUNDS,
                iced::advanced::mouse::Cursor::Available(at),
            )
        };
        let press =
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left));
        let _ = program(&press, &mut st, Point::new(x, y));
        let Drag::Node(i, _) = st.drag else {
            panic!("the press grabbed no node: {:?}", st.drag);
        };
        let (x, y, _, _) = c.node_screen(i, BOUNDS, &st);
        let _ = steps(&c, &mut st, t0, f, 60);
        let (x1, y1, _, _) = c.node_screen(i, BOUNDS, &st);
        assert!(
            (x1 - x).abs() < 0.5 && (y1 - y).abs() < 0.5,
            "held still, the node slid from ({x}, {y}) to ({x1}, {y1})"
        );
        // Pulled out toward the frame's edge, then held there.
        let out = Point::new(x + 120.0 * (x - BOUNDS.width / 2.0).signum(), y);
        let moved = iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: out });
        let _ = program(&moved, &mut st, out);
        let _ = steps(&c, &mut st, t0, f + 60, 60);
        let (x2, y2, _, _) = c.node_screen(i, BOUNDS, &st);
        assert!(
            (x2 - out.x).abs() < 0.5 && (y2 - out.y).abs() < 0.5,
            "pulled out to {out:?}, the node is drawn at ({x2}, {y2})"
        );
    }

    /// The idle spin gathers speed when switched on and coasts to a stop
    /// when switched off — the re-centring that goes with each riding a
    /// spring; a map woken from rest takes one frame's step, not the whole
    /// pause's.
    #[test]
    fn the_spin_starts_and_stops_without_a_jolt() {
        let l = layout(40);
        let (mut st, t0, f) = settled(&l, true);
        let on = steps(&canvas(&l, true, true), &mut st, t0, f + 600, 240);
        // Starting, the camera also moves from centring the picture at rest
        // to centring the turn: both from rest, on a spring.
        assert!(on[0] < 0.3, "{:?}", &on[..4]);
        assert!(jolt(0.0, &on) < 0.5, "{on:?}");
        let off = steps(&canvas(&l, true, false), &mut st, t0, f + 840, 600);
        assert!(jolt(on[on.len() - 1], &off) < 0.05, "{:?}", &off[..4]);
        assert_eq!(off.last(), Some(&0.0), "it came to rest");
        assert!(graph_settled(&st));
    }

    /// At rest the whole map is in its frame and centred in it: flat, its
    /// box fills the frame inside the padding; in 3D every node stays inside
    /// at any angle. The type map once ran off the top and bottom.
    #[test]
    fn a_map_at_rest_is_whole_and_centred_in_its_frame() {
        let l = layout(40);
        let (st, _, _) = settled(&l, false);
        let shown = on_screen(&canvas(&l, false, false), &st);
        let (lo_x, lo_y, hi_x, hi_y) = shown.iter().fold(
            (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
            |(a, b, c, d), p| (a.min(p.0), b.min(p.1), c.max(p.0), d.max(p.1)),
        );
        assert!(lo_x >= GRAPH_PAD - 0.5 && hi_x <= BOUNDS.width - GRAPH_PAD + 0.5);
        assert!(lo_y >= GRAPH_PAD - 0.5 && hi_y <= BOUNDS.height - GRAPH_PAD + 0.5);
        assert!(((lo_x + hi_x) / 2.0 - BOUNDS.width / 2.0).abs() < 1.0);
        assert!(((lo_y + hi_y) / 2.0 - BOUNDS.height / 2.0).abs() < 1.0);
        let filled = (hi_x - lo_x) / (BOUNDS.width - 2.0 * GRAPH_PAD);
        let filled = filled.max((hi_y - lo_y) / (BOUNDS.height - 2.0 * GRAPH_PAD));
        assert!(filled > 0.99, "it fills the frame: {filled}");

        // A 3D camera at rest: what it shows is centred.
        let (st, _, _) = settled(&l, true);
        let shown = on_screen(&canvas(&l, true, false), &st);
        let (lo_x, lo_y, hi_x, hi_y) = shown.iter().fold(
            (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
            |(a, b, c, d), p| (a.min(p.0), b.min(p.1), c.max(p.0), d.max(p.1)),
        );
        assert!(
            ((lo_x + hi_x) / 2.0 - BOUNDS.width / 2.0).abs() < 1.0,
            "{lo_x}..{hi_x}"
        );
        assert!(
            ((lo_y + hi_y) / 2.0 - BOUNDS.height / 2.0).abs() < 1.0,
            "{lo_y}..{hi_y}"
        );

        // Spinning, it frames the whole turn.
        let mut st = GraphState::default();
        let t0 = std::time::Instant::now();
        let c = canvas(&l, true, true);
        let _ = steps(&c, &mut st, t0, 0, 1500);
        assert!(
            st.shift.0.abs() < 0.05 && st.shift.1.abs() < 0.05,
            "{:?}",
            st.shift
        );
        // Every angle the camera can take — a whole turn of the spin, at
        // every tilt: always inside the padding, and out to its edge on one
        // axis at some angle — the frame used, not half of it.
        let (half_w, half_h) = (
            BOUNDS.width / 2.0 - GRAPH_PAD,
            BOUNDS.height / 2.0 - GRAPH_PAD,
        );
        let (mut far_x, mut far_y) = (0.0f32, 0.0f32);
        let (yaw, pitch) = (st.yaw, st.pitch);
        for t in 0..=40 {
            st.pitch = -PITCH_LIMIT + 2.0 * PITCH_LIMIT * t as f32 / 40.0;
            for k in 0..360 {
                st.yaw = yaw + k as f32 * std::f32::consts::TAU / 360.0;
                for (x, y) in on_screen(&c, &st) {
                    far_x = far_x.max((x - BOUNDS.width / 2.0).abs());
                    far_y = far_y.max((y - BOUNDS.height / 2.0).abs());
                }
            }
        }
        st.pitch = pitch;
        assert!(
            far_x <= half_w + 0.5 && far_y <= half_h + 0.5,
            "{far_x} {far_y}"
        );
        assert!(
            far_x >= half_w * 0.97 || far_y >= half_h * 0.97,
            "the turn reaches the frame's edge: {far_x} of {half_w}, {far_y} of {half_h}"
        );
        for k in 0..24 {
            st.yaw = k as f32 * 0.27;
            st.pitch = (k as f32 * 0.4).sin() * 1.4;
            for (x, y) in on_screen(&c, &st) {
                assert!(
                    (0.0..=BOUNDS.width).contains(&x) && (0.0..=BOUNDS.height).contains(&y),
                    "({x}, {y}) is out of the frame at yaw {} pitch {}",
                    st.yaw,
                    st.pitch
                );
            }
        }
    }

    /// Dragging the 3D map turns it and nothing more: tilted up and down,
    /// as an orbit tilts it, it is drawn at one scale throughout. The fit
    /// was the farthest a node got in a turn at the tilt of the moment, and
    /// a drag up or down zoomed the graph in and out under the hand.
    #[test]
    fn tilting_the_3d_map_never_zooms_it() {
        let l = layout(40);
        let (mut st, t0, f) = settled(&l, true);
        let c = canvas(&l, true, false);
        let scale = st.fit_scale;
        assert!(scale > 0.0);
        st.drag = Drag::Orbit;
        for (k, frame) in (f..f + 120).enumerate() {
            st.pitch = (k as f32 * 0.05).sin() * 1.4;
            st.yaw += 0.02;
            let now = t0 + std::time::Duration::from_micros(16_667 * frame as u64);
            let _ = c.tick(
                &mut st,
                BOUNDS,
                now,
                iced::advanced::mouse::Cursor::Unavailable,
            );
            assert!(
                (st.fit_scale - scale).abs() / scale < 1e-3,
                "zoomed at pitch {}: {} from {scale}",
                st.pitch,
                st.fit_scale
            );
        }
    }

    /// An orbit let go while moving keeps turning, slowing to a stop: the
    /// camera does not halt dead under a flick, nor run on forever.
    #[test]
    fn a_released_orbit_coasts_to_a_stop() {
        let l = layout(40);
        let (mut st, t0, f) = settled(&l, true);
        let c = canvas(&l, true, false);
        let event = |st: &mut GraphState, e: iced::Event, at: Point| {
            <GraphCanvas<'_> as iced::widget::canvas::Program<Message>>::update(
                &c,
                st,
                &e,
                BOUNDS,
                iced::advanced::mouse::Cursor::Available(at),
            )
        };
        let corner = Point::new(2.0, 2.0);
        let _ = event(
            &mut st,
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            corner,
        );
        assert_eq!(st.drag, Drag::Orbit);
        for k in 1..=6 {
            let at = Point::new(2.0 + 12.0 * k as f32, 2.0);
            let _ = event(
                &mut st,
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: at }),
                at,
            );
            let _ = steps(&c, &mut st, t0, f + k, 1);
        }
        let _ = event(
            &mut st,
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
            Point::new(74.0, 2.0),
        );
        let released = st.yaw;
        assert!(st.yaw_vel > 1.0, "flung: {}", st.yaw_vel);
        // The turn, frame by frame: only ever slower.
        let turns: Vec<f32> = (0..300)
            .map(|k| {
                let before = st.yaw;
                let _ = steps(&c, &mut st, t0, f + 7 + k, 1);
                st.yaw - before
            })
            .collect();
        assert!(
            st.yaw > released + 0.1,
            "it coasted on: {} → {}",
            released,
            st.yaw
        );
        assert!(
            turns.windows(2).all(|w| w[1] <= w[0] + 1e-6),
            "only ever slowing: {turns:?}"
        );
        assert!(graph_settled(&st), "and came to rest");
    }

    /// A label whose side of its node a neighbour's label has taken goes to
    /// the other side, when there is room there; it was hidden.
    #[test]
    fn a_crowded_label_takes_the_other_side_of_its_node() {
        // A neighbour's label already right of the node.
        let taken = [label_rect(100.0, 50.0, 5.0, 60.0, false)];
        let (rect, left) =
            label_spot(&taken, 100.0, 50.0, 5.0, 60.0, false, 800.0).expect("a place");
        assert!(left, "{rect:?}");
        assert!(rect.x + rect.width <= 100.0 - 5.0);
        // At the frame's left edge there is no other side.
        let near_edge = [label_rect(20.0, 50.0, 5.0, 60.0, false)];
        assert_eq!(
            label_spot(&near_edge, 20.0, 50.0, 5.0, 60.0, false, 800.0).map(|s| s.1),
            None
        );
        // Nothing in the way: its own side.
        assert_eq!(
            label_spot(&[], 100.0, 50.0, 5.0, 60.0, false, 800.0).map(|s| s.1),
            Some(false)
        );
    }

    /// A label that would run off the right edge is drawn left of its node,
    /// and one moving between the sides is eased across, not flipped.
    #[test]
    fn a_label_at_the_right_edge_sits_left_of_its_node() {
        let l = layout(40);
        let (mut st, t0, f) = settled(&l, false);
        let c = canvas(&l, false, false);
        let shown = on_screen(&c, &st);
        let right = (0..shown.len())
            .max_by(|&a, &b| shown[a].0.total_cmp(&shown[b].0))
            .unwrap();
        assert_eq!(st.label_side[right], 0.0, "room enough on the right");
        // Panned to the edge: its label goes over to the left, eased.
        st.pan.x = BOUNDS.width - shown[right].0 - 20.0;
        st.touch();
        let _ = steps(&c, &mut st, t0, f, 1);
        assert_eq!(st.label_side_target[right], 1.0);
        let side = st.label_side[right];
        assert!(side > 0.0 && side < 1.0, "eased: {side}");
        let _ = steps(&c, &mut st, t0, f + 1, 120);
        assert_eq!(st.label_side[right], 1.0);
    }
}
