//! A database schema drawn as tables: a card per table, its columns listed
//! with their types and what the schema says of each (primary key,
//! identity, unique, nullable or not), and a line from each foreign key's
//! columns to the ones they reference — the schema map of Project Graphs
//! (`clew_core::maps::sql`), and any map whose nodes have fields.
//!
//! The cards stand where [`crate::schemalayout`] puts them; the view pans
//! (drag), zooms (the wheel, about the pointer), fits (the header's button)
//! and moves a card dragged by its header or body. Pointing at a card lights
//! its lines and the cards they reach; pointing at a column, its own, and
//! says what is known of it. A click picks a table (the row over the map
//! names it, with Open); a second click opens it — or, on a column, the
//! line that defines the column.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use clew_protocol::{MapField, ProjectMap};
use iced::widget::canvas::{self, Frame, LineCap, LineDash, Path as Shape, Stroke, Text};
use iced::{Color, Point, Rectangle, Size, Vector};

use super::*;
use crate::schemalayout;

// -- measures (world units: 1 at zoom 1) ------------------------------------------

const HEADER_H: f32 = 36.0;
const ROW_H: f32 = 26.0;
const PAD_X: f32 = 12.0;
/// The two marks before a column's name: its key (primary key, identity,
/// unique) and whether it may be empty.
const MARKS_W: f32 = 34.0;
const NAME_TYPE_GAP: f32 = 18.0;
const MIN_W: f32 = 210.0;
const MAX_W: f32 = 440.0;
const RADIUS: f32 = 8.0;
const TITLE_SIZE: f32 = 13.0;
const NAME_SIZE: f32 = 12.5;
const TYPE_SIZE: f32 = 11.5;
const BADGE_SIZE: f32 = 9.5;
/// Below this zoom a card shows its name alone: the columns' words would
/// be too small to read.
const DETAIL_ZOOM: f32 = 0.45;
const MIN_ZOOM: f32 = 0.06;
const MAX_ZOOM: f32 = 2.5;
/// How far a press may move and still be a click.
const CLICK_SLOP: f32 = 4.0;
/// The room kept round the schema when it is fitted to the view.
const FIT_MARGIN: f32 = 48.0;
/// The least zoom the diagram opens at: a schema too large to read whole
/// opens at this size on its most joined table, and Fit shows it whole.
const OPEN_ZOOM: f32 = 0.75;

/// Whether `map` is drawn as tables: some node of it has fields.
pub(crate) fn is_tables(map: &ProjectMap) -> bool {
    map.nodes.iter().any(|n| !n.fields.is_empty())
}

/// What a card is, as drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Table,
    View,
    Enum,
    /// A table only referenced: defined in no file this database has.
    Elsewhere,
}

fn look(map: &ProjectMap, i: usize) -> Look {
    let n = &map.nodes[i];
    match n.kind.as_str() {
        "view" => Look::View,
        "enum" => Look::Enum,
        _ if n.fields.is_empty() && n.file.is_empty() => Look::Elsewhere,
        _ => Look::Table,
    }
}

/// The cards of a map, placed: each one's box in the world.
#[derive(Debug)]
pub(crate) struct Placed {
    cards: Vec<Rectangle>,
    /// What it was placed for, so a view keeps its own pan and zoom only
    /// over the same tables.
    sig: u64,
}

/// A card's size: as wide as its widest line reads, as tall as its rows.
fn card_size(map: &ProjectMap, i: usize) -> Size {
    let n = &map.nodes[i];
    let (schema, name) = split_label(&n.label);
    let title = super::steady::width_of(name, TITLE_SIZE, crate::fonts::UI_SEMIBOLD)
        + schema.map_or(0.0, |s| {
            super::steady::width_of(&format!("{s}."), TITLE_SIZE, crate::fonts::UI)
        });
    let badge = match look(map, i) {
        Look::View | Look::Enum => 46.0,
        _ => 0.0,
    };
    let mut w = PAD_X * 2.0 + 22.0 + title + badge;
    for f in &n.fields {
        let row = PAD_X * 2.0
            + MARKS_W
            + super::steady::width_of(&f.name, NAME_SIZE, crate::fonts::UI)
            + NAME_TYPE_GAP
            + super::steady::width_of(&f.ty, TYPE_SIZE, crate::fonts::CODE);
        w = w.max(row);
    }
    Size::new(
        w.clamp(MIN_W, MAX_W),
        HEADER_H + n.fields.len() as f32 * ROW_H + if n.fields.is_empty() { 0.0 } else { 4.0 },
    )
}

/// Which map a placing is of: the maps' list it is in, and its place there.
type PlacedKey = (usize, usize);

thread_local! {
    /// The last map placed, by which map it is: placing is cheap but not
    /// free, and the view is built every frame.
    static PLACED: RefCell<Option<(PlacedKey, Arc<Placed>)>> = const { RefCell::new(None) };
}

/// `map`'s cards placed, `key` naming the map (the maps' list it is in,
/// and its place there): placed once per map.
pub(crate) fn placed(map: &ProjectMap, key: (usize, usize)) -> Arc<Placed> {
    if let Some(hit) = PLACED.with(|p| {
        p.borrow()
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map(|(_, placed)| placed.clone())
    }) {
        return hit;
    }
    let sizes: Vec<Size> = (0..map.nodes.len()).map(|i| card_size(map, i)).collect();
    let links: Vec<(usize, usize)> = map.edges.iter().map(|e| (e.from, e.to)).collect();
    let at = schemalayout::place(
        &sizes
            .iter()
            .map(|s| schemalayout::Size {
                w: s.width,
                h: s.height,
            })
            .collect::<Vec<_>>(),
        &links,
        // About the shape of the window the dialog fills.
        1.7,
    );
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    map.id.hash(&mut h);
    for n in &map.nodes {
        n.label.hash(&mut h);
        n.fields.len().hash(&mut h);
    }
    let placed = Arc::new(Placed {
        cards: at
            .iter()
            .zip(&sizes)
            .map(|(&(x, y), &s)| Rectangle::new(Point::new(x, y), s))
            .collect(),
        sig: h.finish(),
    });
    PLACED.with(|p| *p.borrow_mut() = Some((key, placed.clone())));
    placed
}

/// `storage.objects` → (`storage`, `objects`).
fn split_label(label: &str) -> (Option<&str>, &str) {
    match label.rsplit_once('.') {
        Some((schema, name)) if !schema.is_empty() && !name.is_empty() => (Some(schema), name),
        _ => (None, label),
    }
}

/// The words the view draws, in the reader's language.
#[derive(Debug, Clone)]
pub(crate) struct Words {
    pub(crate) primary_key: String,
    pub(crate) identity: String,
    pub(crate) unique: String,
    pub(crate) nullable: String,
    pub(crate) not_null: String,
    pub(crate) generated: String,
    pub(crate) indexed: String,
    pub(crate) view: String,
    pub(crate) enumeration: String,
    pub(crate) elsewhere: String,
}

impl Words {
    pub(crate) fn now() -> Words {
        Words {
            primary_key: tr!("Primary key").into(),
            identity: tr!("Identity").into(),
            unique: tr!("Unique").into(),
            nullable: tr!("Nullable").into(),
            not_null: tr!("Non-nullable").into(),
            generated: tr!("Generated").into(),
            indexed: tr!("Indexed").into(),
            view: tr!("VIEW").into(),
            enumeration: tr!("ENUM").into(),
            elsewhere: tr!("Not made in this database").into(),
        }
    }
}

/// The schema view of the project map `map`, the `at`th of the app's maps.
pub(crate) fn schema_view<'a>(
    app: &'a App,
    map: &'a ProjectMap,
    at: usize,
) -> Element<'a, Message> {
    let key = (Arc::as_ptr(&app.proj.maps) as usize, at);
    let placed = placed(map, key);
    let root = app
        .proj
        .project
        .as_ref()
        .map(|p| p.root.clone())
        .unwrap_or_default();
    let matches = app.graph_matches();
    let query = app.graph_search.trim();
    let found: Vec<bool> = match query.is_empty() {
        true => Vec::new(),
        false => {
            let mut found = vec![false; map.nodes.len()];
            for &i in &matches {
                if let Some(f) = found.get_mut(i) {
                    *f = true;
                }
            }
            found
        }
    };
    let focus = (!matches.is_empty() && !query.is_empty())
        .then(|| matches[app.graph_found % matches.len()]);
    let pick = app.proj.graph_pick.as_ref().and_then(|p| {
        map.nodes
            .get(p.node)
            .filter(|n| n.label == p.label && root.join(&n.file) == p.file)
            .map(|_| p.node)
    });
    let mut schemas = map.nodes.iter().map(|n| split_label(&n.label).0);
    let first = schemas.next().flatten();
    let common_schema = first.is_some() && schemas.all(|s| s == first);
    let canvas = canvas::Canvas::new(SchemaCanvas {
        map,
        placed,
        common_schema,
        root,
        words: Words::now(),
        found,
        focus,
        focus_rev: app.graph_focus_rev,
        fit_rev: app.graph_fit_rev,
        pick,
    })
    .width(Fill)
    .height(Fill);
    super::clipped::Clipped::new(canvas).into()
}

pub(crate) struct SchemaCanvas<'a> {
    map: &'a ProjectMap,
    placed: Arc<Placed>,
    /// Every table is in one schema: its name is left off the cards (the
    /// tip still says it).
    common_schema: bool,
    root: PathBuf,
    words: Words,
    /// The cards the search found (empty: no search).
    found: Vec<bool>,
    /// The card the search is on.
    focus: Option<usize>,
    focus_rev: u64,
    fit_rev: u64,
    /// The card the app has picked.
    pick: Option<usize>,
}

/// What is under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    /// A card's header (or a card drawn too small for its rows).
    Card(usize),
    /// A column of a card.
    Field(usize, usize),
}

impl Hit {
    fn card(self) -> usize {
        match self {
            Hit::Card(i) | Hit::Field(i, _) => i,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    None,
    Pan,
    /// A card held, at this offset from its corner (in the world).
    Card(usize, Vector),
}

#[derive(Debug)]
pub(crate) struct SchemaState {
    /// The tables the view is for (`Placed::sig`).
    sig: u64,
    zoom: f32,
    /// Where the world's origin is on screen.
    pan: Vector,
    fitted: bool,
    fit_rev: u64,
    focus_rev: u64,
    /// Cards moved by hand: their corners.
    moved: Vec<Option<Point>>,
    drag: Drag,
    pressed: Option<(Point, Option<Hit>)>,
    travel: f32,
    last: Point,
    hover: Option<Hit>,
    /// Where the pointer is, for the tip.
    pointer: Option<Point>,
}

impl Default for SchemaState {
    fn default() -> Self {
        SchemaState {
            sig: 0,
            zoom: 1.0,
            pan: Vector::new(0.0, 0.0),
            fitted: false,
            fit_rev: 0,
            focus_rev: 0,
            moved: Vec::new(),
            drag: Drag::None,
            pressed: None,
            travel: 0.0,
            last: Point::ORIGIN,
            hover: None,
            pointer: None,
        }
    }
}

impl SchemaCanvas<'_> {
    /// Card `i`'s box in the world, where it was moved to if it was.
    fn card(&self, state: &SchemaState, i: usize) -> Rectangle {
        let placed = self.placed.cards[i];
        match state
            .moved
            .get(i)
            .copied()
            .flatten()
            .filter(|_| state.sig == self.placed.sig)
        {
            Some(at) => Rectangle::new(at, placed.size()),
            None => placed,
        }
    }

    /// The zoom and pan the view is drawn with: the state's, or — before
    /// it has any for these tables — the whole fitted to `bounds`.
    fn view(&self, state: &SchemaState, bounds: Size) -> (f32, Vector) {
        match state.fitted && state.sig == self.placed.sig {
            true => (state.zoom, state.pan),
            false => self.opening(state, bounds),
        }
    }

    /// Where the view opens: the whole, if it reads at that size; else, at
    /// a size that reads, on the table joined to the most others.
    fn opening(&self, state: &SchemaState, bounds: Size) -> (f32, Vector) {
        let (zoom, pan) = self.fit(state, bounds);
        if zoom >= OPEN_ZOOM * 0.8 {
            return (zoom, pan);
        }
        let mut degree = vec![0usize; self.placed.cards.len()];
        for e in &self.map.edges {
            if e.from != e.to && e.from < degree.len() && e.to < degree.len() {
                degree[e.from] += 1;
                degree[e.to] += 1;
            }
        }
        let Some(hub) = (0..degree.len()).max_by_key(|&i| (degree[i], std::cmp::Reverse(i))) else {
            return (zoom, pan);
        };
        let card = self.card(state, hub);
        let zoom = OPEN_ZOOM;
        // Its top in view, its middle across.
        let pan = Vector::new(
            bounds.width / 2.0 - (card.x + card.width / 2.0) * zoom,
            FIT_MARGIN - card.y * zoom
                + ((bounds.height - 2.0 * FIT_MARGIN) - card.height * zoom).max(0.0) / 2.0,
        );
        (zoom, pan)
    }

    /// The zoom and pan that show every card, at most at full size.
    fn fit(&self, state: &SchemaState, bounds: Size) -> (f32, Vector) {
        let n = self.placed.cards.len();
        if n == 0 {
            return (1.0, Vector::new(0.0, 0.0));
        }
        let mut all = self.card(state, 0);
        for i in 1..n {
            all = all.union(&self.card(state, i));
        }
        let zoom = ((bounds.width - 2.0 * FIT_MARGIN) / all.width.max(1.0))
            .min((bounds.height - 2.0 * FIT_MARGIN) / all.height.max(1.0))
            .clamp(MIN_ZOOM, 1.0);
        let pan = Vector::new(
            bounds.width / 2.0 - (all.x + all.width / 2.0) * zoom,
            bounds.height / 2.0 - (all.y + all.height / 2.0) * zoom,
        );
        (zoom, pan)
    }

    /// The zoom and pan that put card `i` in the middle, at least at a size
    /// its columns read at.
    fn centre_on(&self, state: &SchemaState, bounds: Size, i: usize) -> (f32, Vector) {
        let (zoom, _) = self.view(state, bounds);
        let card = self.card(state, i);
        let fits = ((bounds.width - 2.0 * FIT_MARGIN) / card.width)
            .min((bounds.height - 2.0 * FIT_MARGIN) / card.height)
            .min(1.0);
        let zoom = zoom.max(0.8_f32.min(fits)).clamp(MIN_ZOOM, MAX_ZOOM);
        let pan = Vector::new(
            bounds.width / 2.0 - (card.x + card.width / 2.0) * zoom,
            bounds.height / 2.0 - (card.y + card.height / 2.0) * zoom,
        );
        (zoom, pan)
    }

    /// What is at `at` (on screen).
    fn hit(&self, state: &SchemaState, bounds: Size, at: Point) -> Option<Hit> {
        let (zoom, pan) = self.view(state, bounds);
        let world = Point::new((at.x - pan.x) / zoom, (at.y - pan.y) / zoom);
        // The last drawn is on top.
        for i in (0..self.placed.cards.len()).rev() {
            let card = self.card(state, i);
            if !card.contains(world) {
                continue;
            }
            let row = ((world.y - card.y - HEADER_H) / ROW_H).floor();
            let fields = self.map.nodes[i].fields.len();
            if zoom >= DETAIL_ZOOM && row >= 0.0 && (row as usize) < fields {
                return Some(Hit::Field(i, row as usize));
            }
            return Some(Hit::Card(i));
        }
        None
    }

    /// The message that picks card `i`.
    fn pick_message(&self, i: usize) -> Option<Message> {
        let n = self.map.nodes.get(i)?;
        if n.file.is_empty() {
            return None;
        }
        let abs = self.root.join(&n.file);
        Some(Message::Graph(GraphMsg::Picked(Some(
            crate::app::model::GraphPick {
                node: i,
                label: n.label.clone(),
                file: abs.clone(),
                open: Box::new(Message::Graph(GraphMsg::OverlayOpenAt {
                    abs,
                    line: n.line.max(1),
                })),
            },
        ))))
    }

    /// The message that opens what `hit` is: a column's line, or its
    /// table's.
    fn open_message(&self, hit: Hit) -> Option<Message> {
        let n = self.map.nodes.get(hit.card())?;
        let (file, line) = match hit {
            Hit::Field(_, f) => {
                let field = n.fields.get(f)?;
                let file = if field.file.is_empty() {
                    &n.file
                } else {
                    &field.file
                };
                (file, if field.line > 0 { field.line } else { n.line })
            }
            Hit::Card(_) => (&n.file, n.line),
        };
        if file.is_empty() {
            return None;
        }
        Some(Message::Graph(GraphMsg::OverlayOpenAt {
            abs: self.root.join(file),
            line: line.max(1),
        }))
    }

    /// The card lit: the one under the pointer, else the one picked.
    fn lit_card(&self, state: &SchemaState) -> Option<usize> {
        state.hover.map(Hit::card).or(self.pick)
    }
}

/// A line of the map, as drawn: the two cards, the two rows (none: the
/// card as a whole).
struct Line {
    from: usize,
    to: usize,
    rows: Option<(usize, usize)>,
}

fn lines(map: &ProjectMap) -> Vec<Line> {
    let mut out = Vec::new();
    for e in &map.edges {
        if e.from >= map.nodes.len() || e.to >= map.nodes.len() {
            continue;
        }
        match e.fields.is_empty() {
            true => out.push(Line {
                from: e.from,
                to: e.to,
                rows: None,
            }),
            false => out.extend(e.fields.iter().map(|&(a, b)| Line {
                from: e.from,
                to: e.to,
                rows: Some((a, b)),
            })),
        }
    }
    out
}

impl canvas::Program<Message> for SchemaCanvas<'_> {
    type State = SchemaState;

    fn update(
        &self,
        state: &mut SchemaState,
        event: &iced::Event,
        bounds: Rectangle,
        cursor: iced::advanced::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        use iced::mouse;
        use iced::widget::canvas::Action;
        let size = bounds.size();
        let mut redraw = false;
        // Other tables (another map, the project read anew): the view starts
        // over, fitted.
        if state.sig != self.placed.sig {
            *state = SchemaState {
                sig: self.placed.sig,
                fit_rev: self.fit_rev,
                focus_rev: self.focus_rev,
                moved: vec![None; self.placed.cards.len()],
                ..SchemaState::default()
            };
        }
        if !state.fitted {
            (state.zoom, state.pan) = self.opening(state, size);
            state.fitted = true;
            redraw = true;
        }
        if state.fit_rev != self.fit_rev {
            state.fit_rev = self.fit_rev;
            state.moved = vec![None; self.placed.cards.len()];
            (state.zoom, state.pan) = self.fit(state, size);
            redraw = true;
        }
        if state.focus_rev != self.focus_rev {
            state.focus_rev = self.focus_rev;
            (state.zoom, state.pan) = match self.focus {
                Some(i) if i < self.placed.cards.len() => self.centre_on(state, size, i),
                _ => self.fit(state, size),
            };
            redraw = true;
        }
        let action = match event {
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let at = cursor.position_in(bounds)?;
                let dy = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / 40.0,
                };
                let zoom =
                    (state.zoom * 1.1f32.powf(dy.clamp(-3.0, 3.0))).clamp(MIN_ZOOM, MAX_ZOOM);
                // The point under the pointer stays under it.
                let k = zoom / state.zoom;
                state.pan = Vector::new(
                    at.x - (at.x - state.pan.x) * k,
                    at.y - (at.y - state.pan.y) * k,
                );
                state.zoom = zoom;
                state.hover = self.hit(state, size, at);
                Some(Action::request_redraw().and_capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let at = cursor.position_in(bounds)?;
                let hit = self.hit(state, size, at);
                state.pressed = Some((at, hit));
                state.travel = 0.0;
                state.last = at;
                state.drag = match hit {
                    Some(h) => {
                        let card = self.card(state, h.card());
                        let world = Point::new(
                            (at.x - state.pan.x) / state.zoom,
                            (at.y - state.pan.y) / state.zoom,
                        );
                        Drag::Card(h.card(), world - card.position())
                    }
                    None => Drag::Pan,
                };
                Some(Action::request_redraw().and_capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let at = cursor.position_in(bounds);
                state.pointer = at;
                match (state.drag, at) {
                    (Drag::None, Some(at)) => {
                        let hover = self.hit(state, size, at);
                        let changed = hover != state.hover;
                        state.hover = hover;
                        // The tip follows the pointer over a card.
                        (changed || hover.is_some()).then(Action::request_redraw)
                    }
                    (Drag::None, None) => {
                        let had = state.hover.take().is_some();
                        had.then(Action::request_redraw)
                    }
                    (drag, _) => {
                        let now = cursor
                            .position()
                            .map(|p| p - Vector::new(bounds.x, bounds.y))
                            .unwrap_or(state.last);
                        let step = now - state.last;
                        state.travel += (step.x * step.x + step.y * step.y).sqrt();
                        state.last = now;
                        match drag {
                            Drag::Pan => state.pan += step,
                            Drag::Card(i, grab) if state.travel >= CLICK_SLOP => {
                                let world = Point::new(
                                    (now.x - state.pan.x) / state.zoom,
                                    (now.y - state.pan.y) / state.zoom,
                                );
                                if let Some(slot) = state.moved.get_mut(i) {
                                    *slot = Some(world - grab);
                                }
                            }
                            _ => {}
                        }
                        Some(Action::request_redraw().and_capture())
                    }
                }
            }
            iced::Event::Mouse(mouse::Event::CursorLeft) => {
                state.pointer = None;
                state.hover.take().is_some().then(Action::request_redraw)
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.drag != Drag::None =>
            {
                state.drag = Drag::None;
                let pressed = state.pressed.take();
                if state.travel >= CLICK_SLOP {
                    return Some(Action::request_redraw().and_capture());
                }
                // A click: the first on a table picks it, the next opens what
                // it is on; one off the tables puts the pick down.
                let msg = match pressed.and_then(|(_, hit)| hit) {
                    Some(hit) if self.pick == Some(hit.card()) => self.open_message(hit),
                    Some(hit) => self.pick_message(hit.card()),
                    None if self.pick.is_some() => Some(Message::Graph(GraphMsg::Picked(None))),
                    None => None,
                };
                match msg {
                    Some(msg) => Some(Action::publish(msg).and_capture()),
                    None => Some(Action::request_redraw().and_capture()),
                }
            }
            _ => None,
        };
        match action {
            Some(a) => Some(a),
            None => redraw.then(Action::request_redraw),
        }
    }

    fn draw(
        &self,
        state: &SchemaState,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: iced::advanced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let size = bounds.size();
        let (zoom, pan) = self.view(state, size);
        let mut frame = Frame::new(renderer, size);
        // The legend and the tip are placed first: the cards' words under
        // them are left out.
        let tip_lines = match (state.hover, state.pointer, state.drag) {
            (Some(hover), Some(at), Drag::None) => Some((tip(self.map, hover, &self.words), at)),
            _ => None,
        };
        let legend_at = legend_rect(size, &self.words);
        let tip_at = tip_lines
            .as_ref()
            .and_then(|(lines, at)| tip_rect(size, *at, lines));
        COVERED.with(|c| *c.borrow_mut() = legend_at.into_iter().chain(tip_at).collect());
        let to_screen = |p: Point| Point::new(p.x * zoom + pan.x, p.y * zoom + pan.y);
        let screen_box = |r: Rectangle| {
            Rectangle::new(
                to_screen(r.position()),
                Size::new(r.width * zoom, r.height * zoom),
            )
        };
        let viewport = Rectangle::new(Point::ORIGIN, size);
        draw_dots(&mut frame, size, zoom, pan);
        let n = self.placed.cards.len();
        let boxes: Vec<Rectangle> = (0..n).map(|i| screen_box(self.card(state, i))).collect();
        let detail = zoom >= DETAIL_ZOOM;
        // What is lit: the card under the pointer (or picked) and what its
        // lines reach; a column under the pointer, its own lines.
        let lit_card = self.lit_card(state);
        let lit_field = match state.hover {
            Some(Hit::Field(i, f)) => Some((i, f)),
            _ => None,
        };
        let all_lines = lines(self.map);
        let line_lit = |l: &Line| -> bool {
            match (lit_field, l.rows) {
                (Some((i, f)), Some((a, b))) => (l.from == i && a == f) || (l.to == i && b == f),
                (Some((i, _)), None) => l.from == i || l.to == i,
                _ => lit_card.is_some_and(|c| l.from == c || l.to == c),
            }
        };
        let mut near = vec![false; n];
        let mut near_fields: Vec<(usize, usize)> = Vec::new();
        for l in all_lines.iter().filter(|l| line_lit(l)) {
            near[l.from] = true;
            near[l.to] = true;
            if let Some((a, b)) = l.rows {
                near_fields.push((l.from, a));
                near_fields.push((l.to, b));
            }
        }
        let searching = !self.found.is_empty();
        let dimmed = |i: usize| -> bool {
            match (lit_card, searching) {
                (Some(c), _) => c != i && !near[i],
                (None, true) => !self.found.get(i).copied().unwrap_or(false),
                (None, false) => false,
            }
        };
        // Lines under the cards: the faint first, the lit over them.
        let mut lit_lines = Vec::new();
        for l in &all_lines {
            if line_lit(l) {
                lit_lines.push(l);
                continue;
            }
            let faded = lit_card.is_some() || searching;
            draw_line(
                &mut frame, self.map, &boxes, l, detail, zoom, false, faded, viewport,
            );
        }
        for l in lit_lines {
            draw_line(
                &mut frame, self.map, &boxes, l, detail, zoom, true, false, viewport,
            );
        }
        for (i, b) in boxes.iter().enumerate() {
            if !b.intersects(&viewport) {
                continue;
            }
            let state_of = CardState {
                lit: lit_card == Some(i),
                picked: self.pick == Some(i),
                found: self.found.get(i).copied().unwrap_or(false),
                focus: self.focus == Some(i) && searching,
                dimmed: dimmed(i),
                hover_field: lit_field.filter(|(c, _)| *c == i).map(|(_, f)| f),
                near_fields: near_fields
                    .iter()
                    .filter(|(c, _)| *c == i)
                    .map(|(_, f)| *f)
                    .collect(),
            };
            draw_card(
                &mut frame,
                self.map,
                i,
                *b,
                zoom,
                &self.words,
                &state_of,
                self.common_schema,
            );
        }
        if let Some(at) = legend_at {
            draw_legend(&mut frame, at, &self.words);
        }
        if let (Some((lines, _)), Some(at)) = (tip_lines, tip_at) {
            draw_tip(&mut frame, at, &lines);
        }
        COVERED.with(|c| c.borrow_mut().clear());
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &SchemaState,
        bounds: Rectangle,
        cursor: iced::advanced::mouse::Cursor,
    ) -> iced::advanced::mouse::Interaction {
        use iced::advanced::mouse::Interaction;
        match state.drag {
            Drag::None if state.hover.is_some() => Interaction::Pointer,
            Drag::None if cursor.is_over(bounds) => Interaction::Grab,
            Drag::None => Interaction::default(),
            _ => Interaction::Grabbing,
        }
    }
}

// -- drawing --------------------------------------------------------------------------

/// The faint dots the cards sit on, as on a drawing board.
fn draw_dots(frame: &mut Frame, size: Size, zoom: f32, pan: Vector) {
    let step = 24.0 * zoom;
    if step < 9.0 {
        return;
    }
    let color = theme::with_alpha(theme::fg_muted(), 0.14);
    let r = (1.1 * zoom.sqrt()).clamp(0.6, 1.4);
    let x0 = pan.x.rem_euclid(step);
    let y0 = pan.y.rem_euclid(step);
    let dots = Shape::new(|p| {
        let mut y = y0;
        while y < size.height {
            let mut x = x0;
            while x < size.width {
                p.circle(Point::new(x, y), r);
                x += step;
            }
            y += step;
        }
    });
    frame.fill(&dots, color);
}

struct CardState {
    lit: bool,
    picked: bool,
    found: bool,
    focus: bool,
    dimmed: bool,
    hover_field: Option<usize>,
    near_fields: Vec<usize>,
}

/// `text` cut to `max` wide (an ellipsis where it was cut).
fn fit_text(text: &str, size: f32, font: iced::Font, max: f32) -> String {
    if max <= 0.0 {
        return String::new();
    }
    if super::steady::width_of(text, size, font) <= max {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if super::steady::width_of(&candidate, size, font) <= max {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

thread_local! {
    /// Where the legend and the tip are, while a frame is drawn: a canvas
    /// draws all its words over all its shapes, so the cards' words under
    /// them are left out — they showed through.
    static COVERED: RefCell<Vec<Rectangle>> = const { RefCell::new(Vec::new()) };
}

fn text_at(
    frame: &mut Frame,
    content: String,
    at: Point,
    size: f32,
    font: iced::Font,
    color: Color,
    right: bool,
) {
    let w = super::steady::width_of(&content, size, font);
    let area = Rectangle::new(
        Point::new(if right { at.x - w } else { at.x }, at.y - size * 0.7),
        Size::new(w, size * 1.4),
    );
    if COVERED.with(|c| c.borrow().iter().any(|r| r.intersects(&area))) {
        return;
    }
    text_on_top(frame, content, at, size, font, color, right);
}

/// Words drawn whatever is under them: the legend's and the tip's own.
fn text_on_top(
    frame: &mut Frame,
    content: String,
    at: Point,
    size: f32,
    font: iced::Font,
    color: Color,
    right: bool,
) {
    frame.fill_text(Text {
        content,
        position: at,
        color,
        size: size.into(),
        font,
        align_x: if right {
            iced::alignment::Horizontal::Right.into()
        } else {
            iced::alignment::Horizontal::Left.into()
        },
        align_y: iced::alignment::Vertical::Center,
        ..Text::default()
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_card(
    frame: &mut Frame,
    map: &ProjectMap,
    i: usize,
    b: Rectangle,
    zoom: f32,
    words: &Words,
    st: &CardState,
    common_schema: bool,
) {
    let node = &map.nodes[i];
    let kind = look(map, i);
    let fade = if st.dimmed { 0.38 } else { 1.0 };
    let a = |c: Color| theme::with_alpha(c, c.a * fade);
    let radius = (RADIUS * zoom).max(2.0);
    let body = Shape::rounded_rectangle(b.position(), b.size(), radius.into());
    frame.fill(&body, a(theme::bg_panel()));
    // The header, tinted by what the card is.
    let header_h = (HEADER_H * zoom).min(b.height);
    let tint = match kind {
        Look::Table => theme::with_alpha(theme::fg(), 0.05),
        Look::View => theme::with_alpha(theme::info(), 0.14),
        Look::Enum => theme::with_alpha(theme::warning(), 0.14),
        Look::Elsewhere => theme::with_alpha(theme::fg(), 0.02),
    };
    let tint = match st.found {
        true => theme::with_alpha(theme::accent(), 0.22),
        false => tint,
    };
    let header = Shape::new(|p| {
        let (x, y, w, h) = (b.x, b.y, b.width, header_h);
        let r = radius.min(h / 2.0);
        p.move_to(Point::new(x, y + h));
        p.line_to(Point::new(x, y + r));
        p.arc_to(Point::new(x, y), Point::new(x + r, y), r);
        p.line_to(Point::new(x + w - r, y));
        p.arc_to(Point::new(x + w, y), Point::new(x + w, y + r), r);
        p.line_to(Point::new(x + w, y + h));
        p.close();
    });
    frame.fill(&header, a(tint));
    // The border: the accent for the card picked or lit, the search's own.
    let (border, width) = if st.focus || st.picked {
        (theme::accent(), 2.0)
    } else if st.lit {
        (theme::with_alpha(theme::accent(), 0.7), 1.5)
    } else {
        (theme::border(), 1.0)
    };
    let mut pen = Stroke::default().with_width(width).with_color(a(border));
    if kind == Look::Elsewhere {
        pen.line_dash = LineDash {
            segments: &[4.0, 3.0],
            offset: 0,
        };
    }
    frame.stroke(&body, pen);
    if node.fields.is_empty() && header_h < b.height {
        // Nothing below the header.
    } else if !node.fields.is_empty() && zoom >= DETAIL_ZOOM {
        frame.stroke(
            &Shape::line(
                Point::new(b.x, b.y + header_h),
                Point::new(b.x + b.width, b.y + header_h),
            ),
            Stroke::default()
                .with_width(1.0)
                .with_color(a(theme::border())),
        );
    }
    // The title: the schema faint, the name strong.
    let (schema, name) = split_label(&node.label);
    let detail = zoom >= DETAIL_ZOOM;
    let pad = PAD_X * zoom;
    let title_size = if detail {
        TITLE_SIZE * zoom
    } else {
        // Small, a card shows its name alone, larger than its rows would
        // have it: one size for every card, a name too long cut.
        (TITLE_SIZE * zoom * 2.0).clamp(7.0, 14.0)
    };
    let mut x = b.x + pad;
    let title_y = if detail {
        b.y + header_h / 2.0
    } else {
        b.y + b.height.min(header_h * 1.4) / 2.0
    };
    if detail {
        draw_table_glyph(
            frame,
            Point::new(x + 6.0 * zoom, title_y),
            zoom,
            a(theme::fg_muted()),
            kind,
        );
        x += 22.0 * zoom;
    }
    let badge = match kind {
        Look::View => Some(words.view.as_str()),
        Look::Enum => Some(words.enumeration.as_str()),
        _ => None,
    };
    let badge_w = match (badge, detail) {
        (Some(w), true) => {
            super::steady::width_of(w, BADGE_SIZE * zoom, crate::fonts::UI_SEMIBOLD) + 8.0 * zoom
        }
        _ => 0.0,
    };
    let room = b.x + b.width - pad - badge_w - x;
    let name_color = match kind {
        Look::Elsewhere => theme::dim(),
        _ => theme::fg(),
    };
    if let Some(s) = schema.filter(|_| detail && !common_schema) {
        let prefix = format!("{s}.");
        let w = super::steady::width_of(&prefix, title_size, crate::fonts::UI);
        if w < room * 0.5 {
            text_at(
                frame,
                prefix,
                Point::new(x, title_y),
                title_size,
                crate::fonts::UI,
                a(theme::dim()),
                false,
            );
            x += w;
        }
    }
    let room = b.x + b.width - pad - badge_w - x;
    let shown_name = name;
    text_at(
        frame,
        fit_text(shown_name, title_size, crate::fonts::UI_SEMIBOLD, room),
        Point::new(x, title_y),
        title_size,
        crate::fonts::UI_SEMIBOLD,
        a(name_color),
        false,
    );
    if let (Some(word), true) = (badge, detail) {
        let size = BADGE_SIZE * zoom;
        let color = match kind {
            Look::View => theme::info(),
            _ => theme::warning(),
        };
        text_at(
            frame,
            word.to_string(),
            Point::new(b.x + b.width - pad, title_y),
            size,
            crate::fonts::UI_SEMIBOLD,
            a(color),
            true,
        );
    }
    if !detail {
        // Its rows as faint lines: a table still, too small to read.
        let rows = node.fields.len();
        let first = b.y + header_h;
        let step = ROW_H * zoom;
        if rows > 0 && step >= 3.0 {
            let lines = Shape::new(|p| {
                for k in 0..rows {
                    let y = first + (k as f32 + 0.5) * step;
                    let len =
                        (b.width - 2.0 * pad) * (0.45 + 0.4 * ((k * 7 + i * 3) % 5) as f32 / 5.0);
                    p.move_to(Point::new(b.x + pad, y));
                    p.line_to(Point::new(b.x + pad + len, y));
                }
            });
            frame.stroke(
                &lines,
                Stroke::default()
                    .with_width((step * 0.28).clamp(0.8, 3.0))
                    .with_color(a(theme::with_alpha(theme::fg_muted(), 0.18)))
                    .with_line_cap(LineCap::Round),
            );
        }
        return;
    }
    // The columns.
    for (f, field) in node.fields.iter().enumerate() {
        let top = b.y + header_h + f as f32 * ROW_H * zoom;
        let row = Rectangle::new(
            Point::new(b.x + 1.0, top),
            Size::new(b.width - 2.0, ROW_H * zoom),
        );
        if st.hover_field == Some(f) {
            frame.fill_rectangle(row.position(), row.size(), a(theme::bg_hover()));
        } else if st.near_fields.contains(&f) {
            frame.fill_rectangle(
                row.position(),
                row.size(),
                a(theme::with_alpha(theme::accent(), 0.12)),
            );
        }
        if f > 0 {
            frame.stroke(
                &Shape::line(
                    Point::new(b.x + pad, top),
                    Point::new(b.x + b.width - pad, top),
                ),
                Stroke::default()
                    .with_width(1.0)
                    .with_color(a(theme::with_alpha(theme::hairline(), 0.6))),
            );
        }
        let mid = top + ROW_H * zoom / 2.0;
        let mut x = b.x + pad;
        if kind != Look::Enum {
            draw_marks(frame, field, Point::new(x, mid), zoom, &|c| a(c));
            x += MARKS_W * zoom;
        } else {
            x += 4.0 * zoom;
        }
        let ty_w = match field.ty.is_empty() {
            true => 0.0,
            false => super::steady::width_of(&field.ty, TYPE_SIZE * zoom, crate::fonts::CODE)
                .min(b.width * 0.45),
        };
        let name_room = b.x + b.width - pad - x - ty_w - NAME_TYPE_GAP * zoom * 0.6;
        text_at(
            frame,
            fit_text(&field.name, NAME_SIZE * zoom, crate::fonts::UI, name_room),
            Point::new(x, mid),
            NAME_SIZE * zoom,
            crate::fonts::UI,
            a(theme::fg()),
            false,
        );
        if !field.ty.is_empty() {
            text_at(
                frame,
                fit_text(
                    &field.ty,
                    TYPE_SIZE * zoom,
                    crate::fonts::CODE,
                    b.width * 0.45,
                ),
                Point::new(b.x + b.width - pad, mid),
                TYPE_SIZE * zoom,
                crate::fonts::CODE,
                a(theme::dim()),
                true,
            );
        }
    }
}

/// A table's glyph: a grid of cells (a view's: an eye's outline; an
/// enum's: a list).
fn draw_table_glyph(frame: &mut Frame, at: Point, zoom: f32, color: Color, kind: Look) {
    let s = 5.5 * zoom;
    let pen = Stroke::default()
        .with_width((1.2 * zoom).max(0.8))
        .with_color(color);
    match kind {
        Look::View => {
            let eye = Shape::new(|p| {
                p.move_to(Point::new(at.x - s, at.y));
                p.quadratic_curve_to(Point::new(at.x, at.y - s * 1.2), Point::new(at.x + s, at.y));
                p.quadratic_curve_to(Point::new(at.x, at.y + s * 1.2), Point::new(at.x - s, at.y));
            });
            frame.stroke(&eye, pen);
            frame.fill(&Shape::circle(at, s * 0.35), color);
        }
        Look::Enum => {
            for k in -1..=1 {
                let y = at.y + k as f32 * s * 0.7;
                frame.fill(
                    &Shape::circle(Point::new(at.x - s * 0.8, y), s * 0.16),
                    color,
                );
                frame.stroke(
                    &Shape::line(Point::new(at.x - s * 0.4, y), Point::new(at.x + s, y)),
                    pen,
                );
            }
        }
        _ => {
            let r = Rectangle::new(
                Point::new(at.x - s, at.y - s * 0.85),
                Size::new(s * 2.0, s * 1.7),
            );
            frame.stroke(
                &Shape::rounded_rectangle(r.position(), r.size(), (1.5 * zoom).into()),
                pen,
            );
            frame.stroke(
                &Shape::line(
                    Point::new(r.x, at.y - s * 0.2),
                    Point::new(r.x + r.width, at.y - s * 0.2),
                ),
                pen,
            );
            frame.stroke(
                &Shape::line(
                    Point::new(at.x - s * 0.15, at.y - s * 0.2),
                    Point::new(at.x - s * 0.15, r.y + r.height),
                ),
                pen,
            );
        }
    }
}

/// A column's marks: its key (primary key, identity, unique — the first
/// that holds), then whether it may be empty.
fn draw_marks(frame: &mut Frame, f: &MapField, at: Point, zoom: f32, a: &dyn Fn(Color) -> Color) {
    let key_at = Point::new(at.x + 7.0 * zoom, at.y);
    let color = a(theme::fg_muted());
    if f.key {
        draw_key(frame, key_at, zoom, a(theme::warning()));
    } else if f.identity {
        draw_hash(frame, key_at, zoom, color);
    } else if f.unique {
        draw_fingerprint(frame, key_at, zoom, color);
    } else if f.generated {
        draw_spark(frame, key_at, zoom, color);
    }
    draw_diamond(
        frame,
        Point::new(at.x + 24.0 * zoom, at.y),
        zoom,
        color,
        !f.nullable,
    );
}

fn pen(zoom: f32, color: Color) -> Stroke<'static> {
    Stroke::default()
        .with_width((1.2 * zoom).max(0.7))
        .with_color(color)
        .with_line_cap(LineCap::Round)
}

fn draw_key(frame: &mut Frame, at: Point, zoom: f32, color: Color) {
    let s = zoom;
    let ring = Point::new(at.x - 2.8 * s, at.y);
    frame.stroke(&Shape::circle(ring, 2.6 * s), pen(zoom, color));
    let shaft = Shape::new(|p| {
        p.move_to(Point::new(ring.x + 2.6 * s, at.y));
        p.line_to(Point::new(at.x + 5.5 * s, at.y));
        p.move_to(Point::new(at.x + 3.2 * s, at.y));
        p.line_to(Point::new(at.x + 3.2 * s, at.y + 2.4 * s));
        p.move_to(Point::new(at.x + 5.2 * s, at.y));
        p.line_to(Point::new(at.x + 5.2 * s, at.y + 2.0 * s));
    });
    frame.stroke(&shaft, pen(zoom, color));
}

fn draw_hash(frame: &mut Frame, at: Point, zoom: f32, color: Color) {
    let s = zoom;
    let hash = Shape::new(|p| {
        p.move_to(Point::new(at.x - 1.4 * s, at.y - 4.0 * s));
        p.line_to(Point::new(at.x - 2.4 * s, at.y + 4.0 * s));
        p.move_to(Point::new(at.x + 2.4 * s, at.y - 4.0 * s));
        p.line_to(Point::new(at.x + 1.4 * s, at.y + 4.0 * s));
        p.move_to(Point::new(at.x - 4.0 * s, at.y - 1.5 * s));
        p.line_to(Point::new(at.x + 4.2 * s, at.y - 1.5 * s));
        p.move_to(Point::new(at.x - 4.4 * s, at.y + 1.5 * s));
        p.line_to(Point::new(at.x + 3.8 * s, at.y + 1.5 * s));
    });
    frame.stroke(&hash, pen(zoom, color));
}

fn draw_fingerprint(frame: &mut Frame, at: Point, zoom: f32, color: Color) {
    use iced::Radians;
    let s = zoom;
    let arcs = Shape::new(|p| {
        for (k, r) in [1.2f32, 2.6, 4.0].into_iter().enumerate() {
            let start = std::f32::consts::PI * (1.05 + 0.06 * k as f32);
            p.arc(canvas::path::Arc {
                center: Point::new(at.x, at.y + 1.2 * s),
                radius: r * s,
                start_angle: Radians(start),
                end_angle: Radians(std::f32::consts::TAU - 0.1),
            });
        }
    });
    frame.stroke(&arcs, pen(zoom, color));
}

fn draw_spark(frame: &mut Frame, at: Point, zoom: f32, color: Color) {
    let s = zoom;
    let spark = Shape::new(|p| {
        p.move_to(Point::new(at.x, at.y - 4.0 * s));
        p.line_to(Point::new(at.x, at.y + 4.0 * s));
        p.move_to(Point::new(at.x - 3.4 * s, at.y - 2.0 * s));
        p.line_to(Point::new(at.x + 3.4 * s, at.y + 2.0 * s));
        p.move_to(Point::new(at.x + 3.4 * s, at.y - 2.0 * s));
        p.line_to(Point::new(at.x - 3.4 * s, at.y + 2.0 * s));
    });
    frame.stroke(&spark, pen(zoom, color));
}

fn draw_diamond(frame: &mut Frame, at: Point, zoom: f32, color: Color, filled: bool) {
    let r = 3.4 * zoom;
    let d = Shape::new(|p| {
        p.move_to(Point::new(at.x, at.y - r));
        p.line_to(Point::new(at.x + r, at.y));
        p.line_to(Point::new(at.x, at.y + r));
        p.line_to(Point::new(at.x - r, at.y));
        p.close();
    });
    match filled {
        true => frame.fill(&d, color),
        false => frame.stroke(&d, pen(zoom, color)),
    }
}

/// The middle of row `row` of the card at `b`, on its `right` side or left.
fn row_end(b: Rectangle, row: usize, zoom: f32, right: bool) -> Point {
    Point::new(
        if right { b.x + b.width } else { b.x },
        b.y + HEADER_H * zoom + (row as f32 + 0.5) * ROW_H * zoom,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_line(
    frame: &mut Frame,
    map: &ProjectMap,
    boxes: &[Rectangle],
    l: &Line,
    detail: bool,
    zoom: f32,
    lit: bool,
    faded: bool,
    viewport: Rectangle,
) {
    let (a, b) = (boxes[l.from], boxes[l.to]);
    let span = a.union(&b);
    if !span.intersects(&viewport) {
        return;
    }
    // Out of the side that faces the other card; a card beside itself, or
    // two cards one over the other, both from their right.
    let self_line = l.from == l.to;
    let (from_right, to_right) = if self_line {
        (true, true)
    } else if a.x + a.width + 20.0 * zoom <= b.x {
        (true, false)
    } else if b.x + b.width + 20.0 * zoom <= a.x {
        (false, true)
    } else {
        (true, true)
    };
    let (start, end) = match (l.rows, detail) {
        (Some((ra, rb)), true) => (
            row_end(a, ra, zoom, from_right),
            row_end(b, rb, zoom, to_right),
        ),
        _ => {
            let mid = |r: Rectangle, right: bool| {
                Point::new(
                    if right { r.x + r.width } else { r.x },
                    r.y + (HEADER_H * zoom).min(r.height) / 2.0,
                )
            };
            (mid(a, from_right), mid(b, to_right))
        }
    };
    let reach = ((end.x - start.x).abs() / 2.0).max(36.0 * zoom);
    let out = |right: bool| if right { reach } else { -reach };
    let (c1, c2) = if from_right == to_right {
        // Both out to the right: a loop past the wider of the two.
        let x = (a.x + a.width).max(b.x + b.width) + 40.0 * zoom;
        (
            Point::new(x.max(start.x + 30.0 * zoom), start.y),
            Point::new(x.max(end.x + 30.0 * zoom), end.y),
        )
    } else {
        (
            Point::new(start.x + out(from_right), start.y),
            Point::new(end.x + out(to_right), end.y),
        )
    };
    let curve = Shape::new(|p| {
        p.move_to(start);
        p.bezier_curve_to(c1, c2, end);
    });
    let from_field = l.rows.and_then(|(ra, _)| map.nodes[l.from].fields.get(ra));
    let (color, width) = match (lit, faded) {
        (true, _) => (theme::accent(), 1.7),
        (false, true) => (theme::with_alpha(theme::graph_edge(), 0.22), 1.0),
        (false, false) => (theme::with_alpha(theme::graph_edge(), 0.75), 1.1),
    };
    let mut stroke = Stroke::default()
        .with_width(width * zoom.clamp(0.6, 1.3))
        .with_color(color);
    if !lit {
        stroke.line_dash = LineDash {
            segments: &[5.0, 4.0],
            offset: 0,
        };
    }
    frame.stroke(&curve, stroke);
    // The ends: a crow's foot where many rows may point (the key's column
    // is not unique), a bar where one; a dot at what is pointed at.
    let mark = Stroke::default()
        .with_width((1.3 * zoom).clamp(0.8, 1.8))
        .with_color(color);
    let dir = if from_right { 1.0 } else { -1.0 };
    let foot = 7.0 * zoom.clamp(0.5, 1.4);
    let one = from_field.is_some_and(|f| f.unique || f.key);
    if one {
        let x = start.x + dir * foot * 0.7;
        frame.stroke(
            &Shape::line(
                Point::new(x, start.y - foot * 0.6),
                Point::new(x, start.y + foot * 0.6),
            ),
            mark,
        );
    } else if l.rows.is_some() && detail {
        let tip = Point::new(start.x + dir * foot, start.y);
        let crow = Shape::new(|p| {
            p.move_to(Point::new(start.x, start.y - foot * 0.6));
            p.line_to(tip);
            p.move_to(Point::new(start.x, start.y + foot * 0.6));
            p.line_to(tip);
            p.move_to(start);
            p.line_to(tip);
        });
        frame.stroke(&crow, mark);
    }
    frame.fill(&Shape::circle(end, (2.4 * zoom).clamp(1.4, 3.2)), color);
}

const LEGEND_TEXT: f32 = 11.0;

fn legend_items(words: &Words) -> [(&str, u8); 5] {
    [
        (&words.primary_key, 0),
        (&words.identity, 1),
        (&words.unique, 2),
        (&words.nullable, 3),
        (&words.not_null, 4),
    ]
}

/// Where the legend goes: the bottom left, if the view is wide enough.
fn legend_rect(size: Size, words: &Words) -> Option<Rectangle> {
    let w = 16.0
        + legend_items(words)
            .iter()
            .map(|(word, _)| super::steady::width_of(word, LEGEND_TEXT, crate::fonts::UI) + 34.0)
            .sum::<f32>();
    let h = 28.0;
    (w <= size.width - 24.0 && size.height > 3.0 * h)
        .then(|| Rectangle::new(Point::new(12.0, size.height - h - 12.0), Size::new(w, h)))
}

/// The legend: what each mark on a column means.
fn draw_legend(frame: &mut Frame, at_box: Rectangle, words: &Words) {
    let items = legend_items(words);
    let font = crate::fonts::UI;
    let text_size = LEGEND_TEXT;
    let widths: Vec<f32> = items
        .iter()
        .map(|(w, _)| super::steady::width_of(w, text_size, font))
        .collect();
    let (at, h) = (at_box.position(), at_box.height);
    let w = at_box.width;
    let panel = Shape::rounded_rectangle(at, Size::new(w, h), 6.0.into());
    frame.fill(&panel, theme::with_alpha(theme::bg_panel(), 0.94));
    frame.stroke(
        &panel,
        Stroke::default()
            .with_width(1.0)
            .with_color(theme::border()),
    );
    let mut x = at.x + 12.0;
    let y = at.y + h / 2.0;
    let color = theme::fg_muted();
    for ((word, mark), width) in items.iter().zip(widths) {
        let c = Point::new(x + 6.0, y);
        match mark {
            0 => draw_key(frame, c, 1.0, theme::warning()),
            1 => draw_hash(frame, c, 1.0, color),
            2 => draw_fingerprint(frame, c, 1.0, color),
            3 => draw_diamond(frame, c, 1.0, color, false),
            _ => draw_diamond(frame, c, 1.0, color, true),
        }
        text_on_top(
            frame,
            word.to_string(),
            Point::new(x + 16.0, y),
            text_size,
            font,
            theme::fg_muted(),
            false,
        );
        x += width + 34.0;
    }
}

/// What the tip over `hit` says, a line each, the first its title.
fn tip(map: &ProjectMap, hit: Hit, words: &Words) -> Vec<String> {
    let node = &map.nodes[hit.card()];
    let place = |file: &str, line: usize| -> Option<String> {
        (!file.is_empty()).then(|| format!("{file}:{}", line.max(1)))
    };
    match hit {
        Hit::Field(i, f) => {
            let Some(field) = node.fields.get(f) else {
                return Vec::new();
            };
            if look(map, i) == Look::Enum {
                return vec![field.name.clone()];
            }
            let mut out = vec![format!("{} {}", field.name, field.ty).trim().to_string()];
            let mut marks = Vec::new();
            if field.key {
                marks.push(words.primary_key.clone());
            }
            if field.identity {
                marks.push(words.identity.clone());
            }
            if field.unique {
                marks.push(words.unique.clone());
            }
            if field.generated {
                marks.push(words.generated.clone());
            }
            marks.push(match field.nullable {
                true => words.nullable.clone(),
                false => words.not_null.clone(),
            });
            if field.indexed {
                marks.push(words.indexed.clone());
            }
            out.push(marks.join(" · "));
            if !field.default.is_empty() {
                out.push(match field.generated {
                    true => {
                        tr!("Generated from {expression}", expression = field.default).to_string()
                    }
                    false => tr!("Default {value}", value = field.default).to_string(),
                });
            }
            for e in &map.edges {
                for &(a, b) in &e.fields {
                    if e.from == i && a == f {
                        let target = &map.nodes[e.to];
                        let col = target.fields.get(b).map_or("", |c| c.name.as_str());
                        out.push(
                            tr!(
                                "References {target}",
                                target = format!("{}.{col}", target.label)
                            )
                            .to_string(),
                        );
                    }
                    if e.to == i && b == f {
                        let source = &map.nodes[e.from];
                        let col = source.fields.get(a).map_or("", |c| c.name.as_str());
                        out.push(
                            tr!(
                                "Referenced by {source}",
                                source = format!("{}.{col}", source.label)
                            )
                            .to_string(),
                        );
                    }
                }
            }
            if !field.doc.is_empty() {
                out.push(field.doc.clone());
            }
            let file = if field.file.is_empty() {
                &node.file
            } else {
                &field.file
            };
            out.extend(place(
                file,
                if field.line > 0 {
                    field.line
                } else {
                    node.line
                },
            ));
            out
        }
        Hit::Card(i) => {
            let mut out = vec![node.label.clone()];
            if look(map, i) == Look::Elsewhere {
                out.push(words.elsewhere.clone());
            }
            let detail: Vec<&str> = node.detail.lines().collect();
            for line in detail.iter().take(10) {
                out.push(super::graph::map_text(line));
            }
            if detail.len() > 10 {
                out.push(trn!("1 more", "{n} more", detail.len() - 10).to_string());
            }
            out.extend(place(&node.file, node.line));
            out
        }
    }
}

const TIP_LINE_H: f32 = 17.0;
const TIP_MAX_W: f32 = 420.0;

fn tip_style(k: usize) -> (f32, iced::Font) {
    match k {
        0 => (12.5, crate::fonts::UI_SEMIBOLD),
        _ => (11.5, crate::fonts::UI),
    }
}

/// Where the tip by the pointer at `at` goes: beside it, kept inside the
/// view.
fn tip_rect(size: Size, at: Point, lines: &[String]) -> Option<Rectangle> {
    if lines.is_empty() {
        return None;
    }
    let max_w = TIP_MAX_W.min(size.width - 24.0);
    let w = lines
        .iter()
        .enumerate()
        .map(|(k, l)| {
            let (sz, f) = tip_style(k);
            super::steady::width_of(l, sz, f).min(max_w - 20.0)
        })
        .fold(0.0, f32::max)
        + 20.0;
    let h = lines.len() as f32 * TIP_LINE_H + 12.0;
    let mut x = at.x + 16.0;
    let mut y = at.y + 18.0;
    if x + w > size.width - 6.0 {
        x = (at.x - 16.0 - w).max(6.0);
    }
    if y + h > size.height - 6.0 {
        y = (at.y - 12.0 - h).max(6.0);
    }
    Some(Rectangle::new(Point::new(x, y), Size::new(w, h)))
}

/// The tip: its lines in a card at `at`.
fn draw_tip(frame: &mut Frame, at: Rectangle, lines: &[String]) {
    let shown: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(k, l)| {
            let (sz, f) = tip_style(k);
            fit_text(l, sz, f, at.width - 20.0)
        })
        .collect();
    let (x, y, w, h) = (at.x, at.y, at.width, at.height);
    let card = Shape::rounded_rectangle(Point::new(x, y), Size::new(w, h), 6.0.into());
    frame.fill(&card, theme::bg_panel());
    frame.stroke(
        &card,
        Stroke::default()
            .with_width(1.0)
            .with_color(theme::border()),
    );
    for (k, line) in shown.into_iter().enumerate() {
        let (sz, f) = tip_style(k);
        let color = if k == 0 {
            theme::fg()
        } else {
            theme::fg_muted()
        };
        text_on_top(
            frame,
            line,
            Point::new(x + 10.0, y + 6.0 + TIP_LINE_H * (k as f32 + 0.5)),
            sz,
            f,
            color,
            false,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_protocol::{MapEdge, MapNode};
    use iced::advanced::mouse::Cursor;
    use iced::widget::canvas::Program;

    fn field(name: &str, ty: &str) -> MapField {
        MapField {
            name: name.into(),
            ty: ty.into(),
            nullable: true,
            ..Default::default()
        }
    }

    /// Supabase's buckets and objects, and a table joined to nothing.
    fn sample() -> ProjectMap {
        let mut id = field("id", "text");
        id.key = true;
        id.nullable = false;
        let mut name = field("name", "text");
        name.unique = true;
        name.nullable = false;
        let mut oid = field("id", "uuid");
        oid.key = true;
        oid.nullable = false;
        oid.default = "gen_random_uuid()".into();
        let mut bucket = field("bucket_id", "text");
        bucket.indexed = true;
        bucket.file = "m/0003.sql".into();
        bucket.line = 9;
        bucket.doc = "the bucket it is in".into();
        ProjectMap {
            id: "sql.schema".into(),
            title: "Database schema".into(),
            description: String::new(),
            nodes: vec![
                MapNode {
                    label: "storage.buckets".into(),
                    kind: "table".into(),
                    file: "m/0002.sql".into(),
                    line: 58,
                    detail: "row level security\npolicy p: select to authenticated".into(),
                    fields: vec![id, name],
                },
                MapNode {
                    label: "storage.objects".into(),
                    kind: "table".into(),
                    file: "m/0002.sql".into(),
                    line: 68,
                    detail: String::new(),
                    fields: vec![oid, bucket],
                },
                MapNode {
                    label: "storage.migrations".into(),
                    kind: "table".into(),
                    file: "m/0002.sql".into(),
                    line: 51,
                    detail: String::new(),
                    fields: vec![field("id", "integer")],
                },
            ],
            edges: vec![MapEdge {
                from: 1,
                to: 0,
                label: "bucket_id".into(),
                fields: vec![(1, 0)],
            }],
        }
    }

    fn canvas(map: &ProjectMap, pick: Option<usize>) -> SchemaCanvas<'_> {
        PLACED.with(|p| *p.borrow_mut() = None);
        SchemaCanvas {
            map,
            placed: placed(map, (0, usize::MAX)),
            common_schema: true,
            root: PathBuf::from("/p"),
            words: Words::now(),
            found: Vec::new(),
            focus: None,
            focus_rev: 0,
            fit_rev: 0,
            pick,
        }
    }

    #[test]
    fn cards_stand_apart_and_a_key_stands_left_of_what_it_references() {
        let map = sample();
        let c = canvas(&map, None);
        let cards = &c.placed.cards;
        for i in 0..cards.len() {
            for j in i + 1..cards.len() {
                assert!(!cards[i].intersects(&cards[j]), "{i} and {j}: {cards:?}");
            }
        }
        assert!(
            cards[1].x + cards[1].width < cards[0].x,
            "objects left of buckets: {cards:?}"
        );
        assert_eq!(cards[0].height, HEADER_H + 2.0 * ROW_H + 4.0);
        assert!(is_tables(&map));
    }

    #[test]
    fn a_columns_tip_says_what_the_schema_knows_of_it() {
        let map = sample();
        let words = Words::now();
        let tip = tip(&map, Hit::Field(1, 1), &words);
        assert_eq!(tip[0], "bucket_id text");
        assert_eq!(tip[1], "Nullable · Indexed");
        assert!(
            tip.contains(&"References storage.buckets.id".to_string()),
            "{tip:?}"
        );
        assert!(tip.contains(&"the bucket it is in".to_string()), "{tip:?}");
        assert_eq!(tip.last().unwrap(), "m/0003.sql:9", "where it was added");
        let referenced = super::tip(&map, Hit::Field(0, 0), &words);
        assert!(
            referenced.contains(&"Referenced by storage.objects.bucket_id".to_string()),
            "{referenced:?}"
        );
        assert_eq!(referenced[1], "Primary key · Non-nullable");
        let id = super::tip(&map, Hit::Field(1, 0), &words);
        assert!(
            id.contains(&"Default gen_random_uuid()".to_string()),
            "{id:?}"
        );
        let card = super::tip(&map, Hit::Card(0), &words);
        assert_eq!(
            card,
            [
                "storage.buckets",
                "row level security",
                "policy p: select to authenticated",
                "m/0002.sql:58"
            ]
        );
    }

    /// A click picks a table; the next, on a column, opens the line that
    /// defines the column; one off the tables puts the pick down.
    #[test]
    fn a_click_picks_a_table_and_the_next_opens_its_column() {
        let map = sample();
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(1200.0, 800.0));
        let click = |c: &SchemaCanvas<'_>, state: &mut SchemaState, at: Point| -> Option<Message> {
            let cursor = Cursor::Available(at);
            let _ = c.update(
                state,
                &iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: at }),
                bounds,
                cursor,
            );
            let _ = c.update(
                state,
                &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                bounds,
                cursor,
            );
            let released = c.update(
                state,
                &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )),
                bounds,
                cursor,
            )?;
            released.into_inner().0
        };
        let c = canvas(&map, None);
        let mut state = SchemaState::default();
        let _ = c.update(
            &mut state,
            &iced::Event::Mouse(iced::mouse::Event::CursorLeft),
            bounds,
            Cursor::Unavailable,
        );
        assert!(
            state.fitted && state.zoom >= DETAIL_ZOOM,
            "small schemas open whole and readable: {}",
            state.zoom
        );
        let (zoom, pan) = (state.zoom, state.pan);
        let row = |card: usize, row: usize| {
            let b = c.placed.cards[card];
            Point::new(
                (b.x + b.width / 2.0) * zoom + pan.x,
                (b.y + HEADER_H + (row as f32 + 0.5) * ROW_H) * zoom + pan.y,
            )
        };
        match click(&c, &mut state, row(1, 1)) {
            Some(Message::Graph(GraphMsg::Picked(Some(pick)))) => {
                assert_eq!((pick.node, pick.label.as_str()), (1, "storage.objects"));
                assert_eq!(pick.file, PathBuf::from("/p/m/0002.sql"));
            }
            other => panic!("{other:?}"),
        }
        let c = canvas(&map, Some(1));
        match click(&c, &mut state, row(1, 1)) {
            Some(Message::Graph(GraphMsg::OverlayOpenAt { abs, line })) => {
                assert_eq!((abs, line), (PathBuf::from("/p/m/0003.sql"), 9));
            }
            other => panic!("{other:?}"),
        }
        match click(&c, &mut state, Point::new(4.0, 4.0)) {
            Some(Message::Graph(GraphMsg::Picked(None))) => {}
            other => panic!("{other:?}"),
        }
        // A drag pans, and picks nothing.
        let cursor = Cursor::Available(Point::new(10.0, 10.0));
        let _ = c.update(
            &mut state,
            &iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            bounds,
            cursor,
        );
        let moved = Cursor::Available(Point::new(110.0, 60.0));
        let _ = c.update(
            &mut state,
            &iced::Event::Mouse(iced::mouse::Event::CursorMoved {
                position: Point::new(110.0, 60.0),
            }),
            bounds,
            moved,
        );
        let released = c.update(
            &mut state,
            &iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
            bounds,
            moved,
        );
        assert!(released.and_then(|a| a.into_inner().0).is_none());
        assert_eq!(state.pan, pan + Vector::new(100.0, 50.0));
    }

    #[test]
    fn a_databases_title_keeps_its_folder() {
        assert_eq!(
            super::super::graph::map_title("Database schema · migrations/tenant"),
            "Database schema · migrations/tenant"
        );
        assert_eq!(super::super::graph::map_title("Routes"), "Routes");
        assert_eq!(split_label("storage.objects"), (Some("storage"), "objects"));
        assert_eq!(split_label("users"), (None, "users"));
    }
}
