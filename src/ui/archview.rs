//! The overview's architecture diagram: the project's parts as cards —
//! each its name, what it is, what it is for, what it holds and what it is
//! built on — in rows from what runs at the top to what everything rests
//! on at the bottom, arrows from each to what it uses, and what is around
//! the code (tests, examples, docs) in a row at the foot. A card or a part
//! of one opens where it starts. Laid out to the page's width, as tall as
//! it needs: the page scrolls, the diagram does not.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use iced::widget::canvas::{self, Frame, LineCap, LineDash, Path as Shape, Stroke, Text};
use iced::{Color, Point, Rectangle, Size};

use super::*;
use crate::architecture::{self, Architecture, Kind, Tier};

const CARD_MIN_W: f32 = 236.0;
const CARD_MAX_W: f32 = 330.0;
const GAP_X: f32 = 22.0;
/// Between rows of the layers: room for the arrows.
const LAYER_GAP: f32 = 52.0;
/// Between the rows one layer wraps into.
const WRAP_GAP: f32 = 18.0;
const PAD: f32 = 14.0;
const RADIUS: f32 = 8.0;
/// Room the card's words keep from its left edge beyond [`PAD`] (none: its
/// family's colour is its head's, not a strip's).
const STRIP_W: f32 = 0.0;
/// How far down a card its head — name, what it is, its size — reaches.
const HEAD_H: f32 = PAD + 18.0 + 2.0 + 15.0 + 7.0;
const TITLE: f32 = 14.0;
const LINE: f32 = 12.0;
const SMALL: f32 = 11.0;
const CHIP_H: f32 = 20.0;
const CHIP_GAP: f32 = 6.0;
const ABOUT_LINES: usize = 3;
const CHIP_ROWS: usize = 2;
/// Above the row of what is around the code: its label.
const FOOT_LABEL_H: f32 = 26.0;

/// The colour of a kind of part: one of the theme's own — its accent, or
/// one of the colours its code is written in, tuned to sit together on its
/// background — so each kind reads apart from the others in any theme, a
/// light one too, and a project's UI, pages, styles and hooks are not one
/// blue. What helps (utilities, config) is neutral; what is around the
/// code, dim.
pub(crate) fn kind_color(kind: Option<Kind>) -> Color {
    kind_color_in(kind, theme::tokens(), theme::active())
}

/// The kinds coloured apart, in the order they choose — the commonest
/// first: each its own colour of the theme's, first the one it is usually
/// drawn in (the apps the accent, libraries an escape's, the UI a
/// function's...), else the next of the theme's colours no kind before it
/// took — a theme that writes two kinds of code alike gave two kinds one
/// colour.
const COLOURED: [&[Kind]; 8] = [
    &[
        Kind::App,
        Kind::WebApp,
        Kind::MobileApp,
        Kind::DesktopApp,
        Kind::Cli,
        Kind::Server,
    ],
    &[Kind::Core, Kind::Library, Kind::Macros],
    &[Kind::Ui],
    &[Kind::Pages],
    &[Kind::Api, Kind::Services],
    &[Kind::Data, Kind::Models, Kind::Types],
    &[Kind::Styles, Kind::Assets],
    &[Kind::Hooks, Kind::State],
];

/// [`kind_color`] in the theme of syntax colours `t` and palette `p`.
fn kind_color_in(kind: Option<Kind>, t: &theme::TokenColors, p: &theme::Palette) -> Color {
    let Some(kind) = kind else {
        return p.fg_muted;
    };
    if kind.is_around() {
        return p.dim;
    }
    let Some(slot) = COLOURED.iter().position(|kinds| kinds.contains(&kind)) else {
        // What helps (utilities, config): neutral.
        return p.fg_muted;
    };
    let ink = |c: u32| Color::from_rgb8((c >> 16) as u8, (c >> 8) as u8, c as u8);
    // In the order of [`COLOURED`]: the commonest kinds choose first.
    let usual = [
        p.accent,
        ink(t.escape),
        ink(t.function),
        ink(t.keyword),
        ink(t.type_),
        ink(t.number),
        ink(t.string),
        ink(t.property),
    ];
    // The theme's other colours, then those the later kinds usually take:
    // a kind that lends one is not left to want it.
    let mut spare: Vec<Color> = [
        Some(t.parameter),
        Some(t.operator),
        t.definition,
        t.module,
        t.attribute,
        t.macro_,
    ]
    .into_iter()
    .flatten()
    .map(ink)
    .collect();
    spare.extend([p.info, p.success, p.warning, p.danger]);
    // A grey (a theme's parameters, its plain text) is no kind's colour:
    // it is what the helpers are drawn in.
    let grey = |c: &Color| c.r.max(c.g).max(c.b) - c.r.min(c.g).min(c.b) < 0.12;
    let mut taken: Vec<Color> = Vec::new();
    let mut mine = p.fg_muted;
    for (k, own) in usual.iter().enumerate().take(slot + 1) {
        let free = |c: &Color| !taken.contains(c) && !grey(c);
        let colour = if free(own) {
            *own
        } else if let Some(c) = spare
            .iter()
            .chain(&usual[k + 1..])
            .copied()
            .find(|c| free(c))
        {
            c
        } else {
            // A palette of fewer colours than kinds: the accent turned
            // round the colour wheel — as bright, as strong — to a hue no
            // kind has.
            (1..12)
                .map(|step| hue_turned(p.accent, step as f32 * 30.0))
                .find(|c| free(c))
                .unwrap_or(p.accent)
        };
        taken.push(colour);
        if k == slot {
            mine = colour;
        }
    }
    mine
}

/// `n` files, as the interface says it.
pub(crate) fn files_said(n: usize) -> String {
    if n == 1 {
        tr!("1 file").to_string()
    } else {
        tr!("{n} files", n = crate::i18n::number(n as u64)).to_string()
    }
}

/// One card, placed.
#[derive(Debug, Clone)]
pub(crate) struct Card {
    pub rect: Rectangle,
    pub title: String,
    pub badge: Option<String>,
    pub subtitle: String,
    pub about: Vec<String>,
    /// Whether `about` was cut: the tip says it whole.
    pub about_cut: bool,
    /// Each part's chip and its words: the part's index, or `None` for the
    /// chip that says how many more.
    pub chips: Vec<(Rectangle, String, Option<usize>)>,
    pub built_on: Option<String>,
    pub row: usize,
}

/// The diagram laid out to a width.
#[derive(Debug, Clone, Default)]
pub(crate) struct Placed {
    pub cards: Vec<Card>,
    pub height: f32,
    /// Where the foot row's label is, when there is one.
    pub foot: Option<f32>,
    pub width: f32,
}

thread_local! {
    /// The last layout, by the architecture's revision and the width: a
    /// layout measures every word it wraps, and the view is built at every
    /// frame.
    static PLACED: RefCell<Option<(u64, u32, Rc<Placed>)>> = const { RefCell::new(None) };
}

/// `arch` laid out `width` wide (cached by `rev`).
pub(crate) fn placed(arch: &Architecture, rev: u64, width: f32) -> Rc<Placed> {
    let key = width.round() as u32;
    if let Some(hit) = PLACED.with(|p| {
        p.borrow()
            .as_ref()
            .filter(|(r, w, _)| *r == rev && *w == key)
            .map(|(_, _, placed)| placed.clone())
    }) {
        return hit;
    }
    let placed = Rc::new(place(arch, width));
    PLACED.with(|p| *p.borrow_mut() = Some((rev, key, placed.clone())));
    placed
}

/// `text` in lines at most `max` wide, at most `lines` of them (the last
/// cut with an ellipsis), and whether any was cut.
fn wrap(text: &str, size: f32, font: iced::Font, max: f32, lines: usize) -> (Vec<String>, bool) {
    let width = |s: &str| super::steady::width_of(s, size, font);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    // Words, and in a language written without spaces, its characters.
    let mut tokens: Vec<String> = Vec::new();
    for word in text.split(' ') {
        if word.chars().any(|c| c as u32 >= 0x2E80) {
            let mut run = String::new();
            for c in word.chars() {
                run.push(c);
                if c as u32 >= 0x2E80 {
                    tokens.push(std::mem::take(&mut run));
                }
            }
            if !run.is_empty() {
                tokens.push(run);
            }
            if let Some(last) = tokens.last_mut() {
                last.push(' ');
            }
        } else {
            tokens.push(format!("{word} "));
        }
    }
    let mut cut = false;
    for token in tokens {
        let candidate = format!("{line}{token}");
        if width(candidate.trim_end()) <= max || line.is_empty() {
            line = candidate;
            continue;
        }
        out.push(line.trim_end().to_string());
        line = token;
        if out.len() == lines {
            cut = true;
            break;
        }
    }
    if !cut && !line.trim().is_empty() {
        out.push(line.trim_end().to_string());
    }
    if out.len() > lines {
        out.truncate(lines);
        cut = true;
    }
    if cut && let Some(last) = out.last_mut() {
        *last = fit(&format!("{last}…"), size, font, max);
        if !last.ends_with('…') {
            last.push('…');
        }
    }
    for l in &mut out {
        *l = fit(l, size, font, max);
    }
    (out, cut)
}

/// `text` cut to `max` wide, an ellipsis where it was cut.
fn fit(text: &str, size: f32, font: iced::Font, max: f32) -> String {
    if super::steady::width_of(text, size, font) <= max {
        return text.to_string();
    }
    let chars: Vec<char> = text.trim_end_matches('…').chars().collect();
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

/// A component's line under its name: its language and size.
fn subtitle(c: &architecture::Component) -> String {
    let mut bits: Vec<String> = Vec::new();
    if let Some(lang) = c.language.and_then(clew_core::highlight::lang_name) {
        bits.push(lang.to_string());
    }
    if c.members > 1 {
        bits.push(tr!("{n} packages", n = c.members).to_string());
    }
    bits.push(files_said(c.files));
    bits.join(" · ")
}

/// Lay `arch` out `width` wide.
pub(crate) fn place(arch: &Architecture, width: f32) -> Placed {
    let rows = architecture::layers(arch);
    let width = width.max(CARD_MIN_W + 2.0);
    let cols = (((width + GAP_X) / (CARD_MIN_W + GAP_X)).floor() as usize).max(1);
    let widest_row = rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(1)
        .min(cols)
        .max(1);
    // As wide as the room shares among the widest row, within bounds:
    // a row of two is not two cards a page wide each.
    let card_w = ((width - GAP_X * (widest_row as f32 - 1.0)) / widest_row as f32)
        .clamp(CARD_MIN_W.min(width), CARD_MAX_W);
    let inner = card_w - 2.0 * PAD - STRIP_W;
    let ui = crate::fonts::UI;
    let semibold = crate::fonts::UI_SEMIBOLD;
    let code = crate::fonts::CODE;
    // Each card's contents, measured.
    let mut cards: Vec<Card> = arch
        .components
        .iter()
        .map(|c| {
            let name = if architecture::is_other(c) {
                crate::i18n::lookup(architecture::OTHER).to_string()
            } else {
                c.name.clone()
            };
            let badge = c.kind.map(|k| crate::i18n::lookup(k.label()).to_string());
            let badge_w = badge
                .as_ref()
                .map_or(0.0, |b| super::steady::width_of(b, SMALL, ui) + 14.0);
            let title = fit(&name, TITLE, semibold, inner - badge_w - 6.0);
            let (about, about_cut) = match &c.about {
                Some(a) => wrap(a, LINE, ui, inner, ABOUT_LINES),
                None => (Vec::new(), false),
            };
            // The parts as chips, in rows; past the rows, how many more.
            let mut chips: Vec<(Rectangle, String, Option<usize>)> = Vec::new();
            let (mut x, mut row) = (0.0f32, 0usize);
            let total_more = c.more_parts;
            for (k, part) in c.parts.iter().enumerate() {
                let folder =
                    part.dir && part.name == part.rel.rsplit('/').next().unwrap_or(&part.rel);
                let label = if folder {
                    format!("{}/", part.name)
                } else {
                    part.name.clone()
                };
                let label = fit(&label, SMALL, code, inner - 16.0);
                let w = super::steady::width_of(&label, SMALL, code) + 14.0;
                if x > 0.0 && x + w > inner {
                    row += 1;
                    x = 0.0;
                }
                let more_left = c.parts.len() - k + total_more;
                if row >= CHIP_ROWS {
                    // No room: the rest, counted, on the last row.
                    let more = format!("+{more_left}");
                    let mw = super::steady::width_of(&more, SMALL, code) + 14.0;
                    while let Some(last) = chips.last() {
                        if last.0.x + last.0.width + CHIP_GAP + mw <= inner {
                            break;
                        }
                        chips.pop();
                    }
                    let shown: usize = chips.iter().filter(|c| c.2.is_some()).count();
                    let more = format!("+{}", c.parts.len() - shown + total_more);
                    let at = chips.last().map_or(0.0, |l| l.0.x + l.0.width + CHIP_GAP);
                    chips.push((
                        Rectangle::new(
                            Point::new(at, (CHIP_ROWS - 1) as f32 * (CHIP_H + CHIP_GAP)),
                            Size::new(mw, CHIP_H),
                        ),
                        more,
                        None,
                    ));
                    break;
                }
                chips.push((
                    Rectangle::new(
                        Point::new(x, row as f32 * (CHIP_H + CHIP_GAP)),
                        Size::new(w, CHIP_H),
                    ),
                    label,
                    Some(k),
                ));
                x += w + CHIP_GAP;
            }
            if total_more > 0 && chips.iter().all(|c| c.2.is_some()) {
                let more = format!("+{total_more}");
                let mw = super::steady::width_of(&more, SMALL, code) + 14.0;
                let (at, r) = match chips.last() {
                    Some(l) if l.0.x + l.0.width + CHIP_GAP + mw <= inner => {
                        (l.0.x + l.0.width + CHIP_GAP, l.0.y)
                    }
                    Some(l) if l.0.y < (CHIP_ROWS - 1) as f32 * (CHIP_H + CHIP_GAP) => {
                        (0.0, l.0.y + CHIP_H + CHIP_GAP)
                    }
                    Some(l) => {
                        let y = l.0.y;
                        chips.pop();
                        let at = chips
                            .last()
                            .filter(|p| p.0.y == y)
                            .map_or(0.0, |p| p.0.x + p.0.width + CHIP_GAP);
                        (at, y)
                    }
                    None => (0.0, 0.0),
                };
                chips.push((
                    Rectangle::new(Point::new(at, r), Size::new(mw, CHIP_H)),
                    more,
                    None,
                ));
            }
            let built_on = (!c.built_on.is_empty()).then(|| {
                fit(
                    &tr!("Built on {libraries}", libraries = c.built_on.join(", ")),
                    SMALL,
                    ui,
                    inner,
                )
            });
            Card {
                rect: Rectangle::new(Point::ORIGIN, Size::new(card_w, 0.0)),
                title,
                badge,
                subtitle: fit(&subtitle(c), SMALL, ui, inner),
                about,
                about_cut,
                chips,
                built_on,
                row: 0,
            }
        })
        .collect();
    let height_of = |card: &Card| -> f32 {
        let mut h = HEAD_H;
        if !card.about.is_empty() {
            h += 8.0 + card.about.len() as f32 * 16.0;
        }
        if let Some(last) = card
            .chips
            .iter()
            .map(|c| c.0.y + c.0.height)
            .reduce(f32::max)
        {
            h += 10.0 + last;
        }
        if card.built_on.is_some() {
            // Room for its hairline, and the line under it.
            h += 14.0 + 14.0;
        }
        h + PAD
    };
    // The rows: each layer, wrapped to the width, centred.
    let foot_layer = rows.last().filter(|row| {
        row.iter()
            .all(|&i| arch.components[i].kind.is_some_and(Kind::is_around))
    });
    let foot_index = foot_layer.map(|_| rows.len() - 1);
    let mut y = 0.0f32;
    let mut foot = None;
    let mut visual_row = 0usize;
    for (layer_index, layer) in rows.iter().enumerate() {
        if Some(layer_index) == foot_index {
            if layer_index > 0 {
                y += LAYER_GAP - WRAP_GAP;
            }
            foot = Some(y);
            y += FOOT_LABEL_H;
        } else if layer_index > 0 {
            y += LAYER_GAP;
        }
        for (k, chunk) in layer.chunks(cols).enumerate() {
            if k > 0 {
                y += WRAP_GAP;
            }
            let row_h = chunk
                .iter()
                .map(|&i| height_of(&cards[i]))
                .fold(0.0, f32::max);
            let row_w = chunk.len() as f32 * card_w + (chunk.len() as f32 - 1.0) * GAP_X;
            let mut x = ((width - row_w) / 2.0).max(0.0);
            for &i in chunk {
                cards[i].rect = Rectangle::new(Point::new(x, y), Size::new(card_w, row_h));
                cards[i].row = visual_row;
                x += card_w + GAP_X;
            }
            y += row_h;
            visual_row += 1;
        }
    }
    Placed {
        cards,
        height: y + 4.0,
        foot,
        width,
    }
}

/// What is under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    Card(usize),
    /// A card's part: its chip's index on the card.
    Chip(usize, usize),
}

impl Hit {
    fn card(self) -> usize {
        match self {
            Hit::Card(i) | Hit::Chip(i, _) => i,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ArchState {
    hover: Option<Hit>,
    pointer: Option<Point>,
    rev: u64,
}

/// The canvas: the architecture, as [`place`] lays it out.
pub(crate) struct ArchCanvas<'a> {
    pub arch: &'a Architecture,
    pub placed: Rc<Placed>,
    pub rev: u64,
}

/// A chip's place on the canvas.
fn chip_rect(card: &Card, chip: Rectangle) -> Rectangle {
    let top = chips_top(card);
    Rectangle::new(
        Point::new(card.rect.x + PAD + STRIP_W + chip.x, top + chip.y),
        chip.size(),
    )
}

fn chips_top(card: &Card) -> f32 {
    let mut y = card.rect.y + HEAD_H;
    if !card.about.is_empty() {
        y += 8.0 + card.about.len() as f32 * 16.0;
    }
    y + 10.0
}

impl ArchCanvas<'_> {
    fn hit(&self, at: Point) -> Option<Hit> {
        for (i, card) in self.placed.cards.iter().enumerate() {
            if !card.rect.contains(at) {
                continue;
            }
            for (k, (chip, _, part)) in card.chips.iter().enumerate() {
                if part.is_some() && chip_rect(card, *chip).contains(at) {
                    return Some(Hit::Chip(i, k));
                }
            }
            return Some(Hit::Card(i));
        }
        None
    }

    /// Where a click on `hit` goes: the part's file, or the one in its
    /// folder it starts from; the card's entry.
    fn target(&self, hit: Hit) -> Option<String> {
        let c = self.arch.components.get(hit.card())?;
        match hit {
            Hit::Chip(i, k) => {
                let part = self.placed.cards[i].chips.get(k)?.2?;
                c.parts.get(part).map(|p| p.open.clone())
            }
            Hit::Card(_) => c
                .entry
                .clone()
                .or_else(|| c.parts.first().map(|p| p.open.clone())),
        }
    }
}

impl canvas::Program<Message> for ArchCanvas<'_> {
    type State = ArchState;

    fn update(
        &self,
        state: &mut ArchState,
        event: &iced::Event,
        bounds: Rectangle,
        cursor: iced::advanced::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        use iced::mouse;
        use iced::widget::canvas::Action;
        if state.rev != self.rev {
            *state = ArchState {
                rev: self.rev,
                ..ArchState::default()
            };
        }
        match event {
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let at = cursor.position_in(bounds);
                state.pointer = at;
                let hover = at.and_then(|at| self.hit(at));
                let changed = hover != state.hover;
                state.hover = hover;
                (changed || hover.is_some()).then(Action::request_redraw)
            }
            iced::Event::Mouse(mouse::Event::CursorLeft) => {
                state.pointer = None;
                state.hover.take().is_some().then(Action::request_redraw)
            }
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let at = cursor.position_in(bounds)?;
                let rel = self.hit(at).and_then(|hit| self.target(hit))?;
                Some(
                    Action::publish(Message::Editor(EditorMsg::OpenRel { rel, line: None }))
                        .and_capture(),
                )
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &ArchState,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: iced::advanced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let placed = &self.placed;
        let lit = state.hover.map(Hit::card);
        // The tip, placed first: the cards' words under it are left out — a
        // canvas draws all its words over all its shapes, and they showed
        // through.
        let tip = match (state.hover, state.pointer) {
            (Some(hit), Some(at)) => tip(self.arch, placed, hit, at, bounds.size()),
            _ => None,
        };
        COVERED.with(|c| *c.borrow_mut() = tip.as_ref().map(|(r, _)| *r));
        if let Some(y) = placed.foot {
            draw_foot(&mut frame, y, placed.width);
        }
        draw_links(&mut frame, self.arch, placed, lit);
        for (i, card) in placed.cards.iter().enumerate() {
            let c = &self.arch.components[i];
            let near = lit.is_some_and(|l| {
                l == i
                    || self
                        .arch
                        .links
                        .iter()
                        .any(|k| (k.from == l && k.to == i) || (k.to == l && k.from == i))
            });
            let dim = lit.is_some() && !near;
            let chip_hover = match state.hover {
                Some(Hit::Chip(h, k)) if h == i => Some(k),
                _ => None,
            };
            draw_card(&mut frame, c, card, lit == Some(i), dim, chip_hover);
        }
        if let Some((at, lines)) = &tip {
            draw_tip(&mut frame, *at, lines);
        }
        COVERED.with(|c| *c.borrow_mut() = None);
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &ArchState,
        _bounds: Rectangle,
        _cursor: iced::advanced::mouse::Cursor,
    ) -> iced::advanced::mouse::Interaction {
        match state.hover {
            Some(hit) if self.target(hit).is_some() => iced::advanced::mouse::Interaction::Pointer,
            _ => iced::advanced::mouse::Interaction::default(),
        }
    }
}

thread_local! {
    /// Where the tip is, while a frame is drawn: no card's words there.
    static COVERED: RefCell<Option<Rectangle>> = const { RefCell::new(None) };
}

fn text(frame: &mut Frame, content: String, at: Point, size: f32, font: iced::Font, color: Color) {
    if let Some(tip) = COVERED.with(|c| *c.borrow()) {
        let w = super::steady::width_of(&content, size, font);
        let area = Rectangle::new(
            Point::new(at.x, at.y - size * 0.7),
            Size::new(w, size * 1.4),
        );
        if tip.intersects(&area) {
            return;
        }
    }
    text_on_top(frame, content, at, size, font, color);
}

/// Words drawn whatever is under them: the tip's own.
fn text_on_top(
    frame: &mut Frame,
    content: String,
    at: Point,
    size: f32,
    font: iced::Font,
    color: Color,
) {
    frame.fill_text(Text {
        content,
        position: at,
        color,
        size: size.into(),
        font,
        align_y: iced::alignment::Vertical::Center,
        ..Text::default()
    });
}

/// The label over the row of what is around the code, and a rule.
fn draw_foot(frame: &mut Frame, y: f32, width: f32) {
    let label = tr!("Around the code").to_string();
    let w = super::steady::width_of(&label, SMALL, crate::fonts::UI);
    let mid = y + FOOT_LABEL_H / 2.0 - 4.0;
    text(
        frame,
        label,
        Point::new(0.0, mid),
        SMALL,
        crate::fonts::UI,
        theme::dim(),
    );
    let rule = Shape::line(Point::new(w + 10.0, mid), Point::new(width, mid));
    let mut pen = Stroke::default()
        .with_width(1.0)
        .with_color(theme::with_alpha(theme::border(), 0.7));
    pen.line_dash = LineDash {
        segments: &[3.0, 4.0],
        offset: 0,
    };
    frame.stroke(&rule, pen);
}

/// A row's top, its bottom, and where its cards are across it.
type RowSpan = (f32, f32, Vec<(f32, f32)>);

/// `a` mixed toward `b` by `t`, opaque: lines that run together over one
/// another stay one line, not a darker one.
fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

/// `c` turned `degrees` round the colour wheel, as light and as strong.
fn hue_turned(c: Color, degrees: f32) -> Color {
    let (max, min) = (c.r.max(c.g).max(c.b), c.r.min(c.g).min(c.b));
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < 1e-4 {
        return c;
    }
    let sat = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let hue = if max == c.r {
        ((c.g - c.b) / d).rem_euclid(6.0)
    } else if max == c.g {
        (c.b - c.r) / d + 2.0
    } else {
        (c.r - c.g) / d + 4.0
    } * 60.0;
    let h = (hue + degrees).rem_euclid(360.0) / 60.0;
    let chroma = (1.0 - (2.0 * l - 1.0).abs()) * sat;
    let x = chroma * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = l - chroma / 2.0;
    Color::from_rgb(r + m, g + m, b + m)
}

/// `points`, a path of straight runs across and down, its corners rounded.
fn rounded(points: &[Point], radius: f32) -> Shape {
    Shape::new(|p| {
        let Some(first) = points.first() else {
            return;
        };
        p.move_to(*first);
        for w in points.windows(3) {
            let (a, c, b) = (w[0], w[1], w[2]);
            let (d1, d2) = (c.distance(a), c.distance(b));
            if d1 < 0.5 || d2 < 0.5 {
                p.line_to(c);
                continue;
            }
            let r = radius.min(d1 / 2.0).min(d2 / 2.0);
            let before = Point::new(c.x + (a.x - c.x) / d1 * r, c.y + (a.y - c.y) / d1 * r);
            let after = Point::new(c.x + (b.x - c.x) / d2 * r, c.y + (b.y - c.y) / d2 * r);
            p.line_to(before);
            p.quadratic_curve_to(c, after);
        }
        if let Some(last) = points.last() {
            p.line_to(*last);
        }
    })
}

/// Each link, from its user to what it uses, as an architecture diagram
/// draws it: down out of the user's foot — one trunk for all it uses —
/// along a lane of its own in the gap under its row, and down into the
/// head of what it uses, each user at a place of its own along that head.
/// A link past a row goes down between that row's cards, not behind them;
/// one from the foot row goes up the same way; one within a row, through
/// the gap under it. Lines are opaque, so those that share a trunk stay
/// one line.
fn draw_links(frame: &mut Frame, arch: &Architecture, placed: &Placed, lit: Option<usize>) {
    let cards = &placed.cards;
    // Each row's top and bottom and where its cards are.
    let mut rows: std::collections::BTreeMap<usize, RowSpan> = std::collections::BTreeMap::new();
    for card in cards {
        let r = card.rect;
        let row = rows
            .entry(card.row)
            .or_insert((r.y, r.y + r.height, Vec::new()));
        row.0 = row.0.min(r.y);
        row.1 = row.1.max(r.y + r.height);
        row.2.push((r.x, r.x + r.width));
    }
    for row in rows.values_mut() {
        row.2.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    let last_row = rows.keys().copied().max().unwrap_or(0);
    // The gap under row `g`: from its bottom to the next row's top (less
    // the foot's label, over the foot row).
    let gap = |g: usize| -> (f32, f32) {
        let top = rows.get(&g).map_or(0.0, |r| r.1);
        let mut bottom = rows.get(&(g + 1)).map_or(top + LAYER_GAP, |r| r.0);
        if g + 1 == last_row
            && let Some(foot) = placed.foot
        {
            bottom = bottom.min(foot + 2.0);
        }
        (top, bottom)
    };
    // How each link runs: down (or up), and through which gaps.
    struct Route {
        down: bool,
        gaps: Vec<usize>,
        between: Vec<usize>,
    }
    let route = |from: usize, to: usize| -> Route {
        let (ra, rb) = (cards[from].row, cards[to].row);
        if ra < rb {
            Route {
                down: true,
                gaps: (ra..rb).collect(),
                between: (ra + 1..rb).collect(),
            }
        } else if ra > rb {
            Route {
                down: false,
                gaps: (rb..ra).rev().collect(),
                between: (rb + 1..ra).rev().collect(),
            }
        } else {
            // Within a row: out of the foot, along the gap under it, and
            // up into the other's foot.
            Route {
                down: true,
                gaps: vec![ra],
                between: Vec::new(),
            }
        }
    };
    // A lane for each user in each gap it runs along, in the order of the
    // users across the page: none runs along another's.
    let mut lane_users: HashMap<usize, Vec<(f32, usize)>> = HashMap::new();
    for l in &arch.links {
        for g in route(l.from, l.to).gaps {
            let users = lane_users.entry(g).or_default();
            if !users.iter().any(|(_, u)| *u == l.from) {
                users.push((cards[l.from].rect.center_x(), l.from));
            }
        }
    }
    for users in lane_users.values_mut() {
        users.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    let lane = |g: usize, user: usize| -> f32 {
        let (top, bottom) = gap(g);
        let users = &lane_users[&g];
        let k = users.iter().position(|(_, u)| *u == user).unwrap_or(0);
        let (top, bottom) = (top + 10.0, bottom - 14.0);
        if bottom <= top {
            return (top + bottom) / 2.0;
        }
        top + (bottom - top) * (k as f32 + 0.5) / users.len() as f32
    };
    // Where each link comes into what it uses: its users along its head
    // (or foot), in their order across the page.
    let mut ends: HashMap<(usize, bool), Vec<(f32, usize)>> = HashMap::new();
    for (n, l) in arch.links.iter().enumerate() {
        let into_top = cards[l.from].row < cards[l.to].row;
        ends.entry((l.to, into_top))
            .or_default()
            .push((cards[l.from].rect.center_x(), n));
    }
    for list in ends.values_mut() {
        list.sort_by(|x, y| x.0.total_cmp(&y.0));
    }
    let end_at = |n: usize| -> Point {
        let l = &arch.links[n];
        let into_top = cards[l.from].row < cards[l.to].row;
        let r = cards[l.to].rect;
        let list = &ends[&(l.to, into_top)];
        let k = list.iter().position(|(_, m)| *m == n).unwrap_or(0);
        let span = (r.width * 0.5).min(36.0 * list.len() as f32);
        let x = r.center_x() - span / 2.0 + span * (k as f32 + 0.5) / list.len() as f32;
        Point::new(x, if into_top { r.y } else { r.y + r.height })
    };
    // Where a link goes down a row it passes: the gap between its cards
    // nearest where it is headed, clear of them — each user down a gap at
    // a place of its own, beside the others.
    let mut down_gap: HashMap<(usize, i32), Vec<usize>> = HashMap::new();
    let mut corridor = |row: usize, ideal: f32, user: usize| -> f32 {
        let Some((_, _, spans)) = rows.get(&row) else {
            return ideal;
        };
        let mut gaps: Vec<(f32, f32)> = Vec::new();
        let mut at = 2.0f32;
        for &(lo, hi) in spans {
            if lo - at >= 8.0 {
                gaps.push((at, lo));
            }
            at = hi;
        }
        if placed.width - 2.0 - at >= 8.0 {
            gaps.push((at, placed.width - 2.0));
        }
        let Some(&(lo, hi)) = gaps.iter().min_by(|a, b| {
            let off = |g: &(f32, f32)| (g.0 - ideal).max(ideal - g.1).max(0.0);
            off(a).total_cmp(&off(b))
        }) else {
            return ideal;
        };
        let users = down_gap.entry((row, lo as i32)).or_default();
        if !users.contains(&user) {
            users.push(user);
        }
        let k = users.iter().position(|u| *u == user).unwrap_or(0);
        let spread = match k {
            0 => 0.0,
            k if k % 2 == 1 => 8.0 * k.div_ceil(2) as f32,
            k => -8.0 * (k / 2) as f32,
        };
        let (lo, hi) = (lo + 14.0, hi - 14.0);
        if lo >= hi {
            return (lo + hi) / 2.0;
        }
        (ideal.clamp(lo, hi) + spread).clamp(lo, hi)
    };
    let background = theme::bg();
    // The faint first, the lit over them.
    let mut order: Vec<usize> = (0..arch.links.len()).collect();
    order.sort_by_key(|&n| {
        let l = &arch.links[n];
        lit.is_some_and(|c| l.from == c || l.to == c)
    });
    for n in order {
        let l = &arch.links[n];
        let r = route(l.from, l.to);
        let source = cards[l.from].rect;
        let start = Point::new(
            source.center_x(),
            if r.down {
                source.y + source.height
            } else {
                source.y
            },
        );
        let end = end_at(n);
        let is_lit = lit.is_some_and(|c| l.from == c || l.to == c);
        let around = arch.components[l.from].kind.is_some_and(Kind::is_around);
        let width = (1.2 + ((l.imports as f32) + 1.0).log2() * 0.16).min(2.2)
            + if is_lit { 0.5 } else { 0.0 };
        let color = if is_lit {
            theme::accent()
        } else if lit.is_some() {
            mix(background, theme::fg_muted(), 0.12)
        } else if around {
            mix(background, theme::fg_muted(), 0.22)
        } else {
            mix(background, theme::fg_muted(), 0.36)
        };
        // The runs: out of the user, along its lane, down each row it
        // passes in its corridor, along the last lane, into what it uses.
        let head = 6.0 + width;
        let dir = if r.down { 1.0 } else { -1.0 };
        let tip = end;
        let base = Point::new(end.x, end.y - dir * head);
        let same_row = cards[l.from].row == cards[l.to].row;
        let mut points = vec![start];
        let mut x = start.x;
        let mut gaps = r.gaps.iter().copied();
        let first = gaps.next().unwrap_or(cards[l.from].row);
        let mut y = lane(first, l.from);
        points.push(Point::new(x, y));
        for (k, &row) in r.between.iter().enumerate() {
            // Toward where it is headed, by how far down it has come.
            let t = (k as f32 + 1.0) / (r.between.len() as f32 + 1.0);
            let ideal = start.x + (end.x - start.x) * t;
            x = corridor(row, ideal, l.from);
            points.push(Point::new(x, y));
            let next = gaps.next().unwrap_or(first);
            y = lane(next, l.from);
            points.push(Point::new(x, y));
        }
        points.push(Point::new(end.x, y));
        // Within a row, it comes up into the other's foot.
        let (tip, base) = if same_row {
            let up = Point::new(end.x, cards[l.to].rect.y + cards[l.to].rect.height);
            (up, Point::new(up.x, up.y + head))
        } else {
            (tip, base)
        };
        points.push(base);
        points.dedup_by(|a, b| a.distance(*b) < 0.5);
        let mut pen = Stroke::default()
            .with_width(width)
            .with_color(color)
            .with_line_cap(LineCap::Round);
        if l.imports == 0 || around {
            pen.line_dash = LineDash {
                segments: &[4.0, 4.0],
                offset: 0,
            };
        }
        frame.stroke(&rounded(&points, 9.0), pen);
        // The head: a filled triangle into what is used.
        let (dx, dy) = (tip.x - base.x, tip.y - base.y);
        let len = (dx * dx + dy * dy).sqrt().max(0.001);
        let (ux, uy) = (dx / len, dy / len);
        let half = 3.2 + width * 0.5;
        let arrow = Shape::new(|p| {
            p.move_to(tip);
            p.line_to(Point::new(base.x - uy * half, base.y + ux * half));
            p.line_to(Point::new(base.x + uy * half, base.y - ux * half));
            p.close();
        });
        frame.fill(&arrow, color);
    }
}

fn draw_card(
    frame: &mut Frame,
    c: &architecture::Component,
    card: &Card,
    lit: bool,
    dim: bool,
    chip_hover: Option<usize>,
) {
    let fade = if dim { 0.45 } else { 1.0 };
    let a = |col: Color| theme::with_alpha(col, col.a * fade);
    let r = card.rect;
    let tier = c.kind.map(Kind::tier);
    let tint = kind_color(c.kind);
    let around = tier == Some(Tier::Around);
    let body = Shape::rounded_rectangle(r.position(), r.size(), RADIUS.into());
    frame.fill(&body, a(theme::bg_panel()));
    // Its head in its family's colour: what kind of part it is, at a
    // glance, across the diagram.
    let head_h = HEAD_H.min(r.height);
    let head = Shape::new(|p| {
        let (x, y, w, h, rr) = (r.x, r.y, r.width, head_h, RADIUS);
        p.move_to(Point::new(x, y + h));
        p.line_to(Point::new(x, y + rr));
        p.arc_to(Point::new(x, y), Point::new(x + rr, y), rr);
        p.line_to(Point::new(x + w - rr, y));
        p.arc_to(Point::new(x + w, y), Point::new(x + w, y + rr), rr);
        p.line_to(Point::new(x + w, y + h));
        p.close();
    });
    frame.fill(
        &head,
        a(theme::with_alpha(tint, if around { 0.06 } else { 0.13 })),
    );
    let under_head = Shape::line(
        Point::new(r.x + 1.0, r.y + head_h),
        Point::new(r.x + r.width - 1.0, r.y + head_h),
    );
    frame.stroke(
        &under_head,
        Stroke::default()
            .with_width(1.0)
            .with_color(a(theme::with_alpha(tint, if around { 0.12 } else { 0.22 }))),
    );
    let (border, width) = if lit {
        (theme::accent(), 1.6)
    } else if around {
        (theme::border(), 1.0)
    } else {
        (mix(theme::border(), tint, 0.38), 1.0)
    };
    let mut pen = Stroke::default().with_width(width).with_color(a(border));
    if around && !lit {
        pen.line_dash = LineDash {
            segments: &[4.0, 3.0],
            offset: 0,
        };
    }
    frame.stroke(&body, pen);
    let x = r.x + PAD + STRIP_W;
    let right = r.x + r.width - PAD;
    let mut y = r.y + PAD + 9.0;
    text(
        frame,
        card.title.clone(),
        Point::new(x, y),
        TITLE,
        crate::fonts::UI_SEMIBOLD,
        a(theme::fg()),
    );
    if let Some(badge) = &card.badge {
        let w = super::steady::width_of(badge, SMALL, crate::fonts::UI) + 14.0;
        let pill = Shape::rounded_rectangle(
            Point::new(right - w, y - 9.0),
            Size::new(w, 18.0),
            9.0.into(),
        );
        frame.fill(&pill, a(theme::with_alpha(tint, 0.16)));
        let neutral = matches!(c.kind, None | Some(Kind::Utilities | Kind::Config));
        let ink = if neutral || around {
            theme::fg_muted()
        } else {
            tint
        };
        text(
            frame,
            badge.clone(),
            Point::new(right - w + 7.0, y),
            SMALL,
            crate::fonts::UI,
            a(ink),
        );
    }
    y += 9.0 + 2.0 + 7.5;
    text(
        frame,
        card.subtitle.clone(),
        Point::new(x, y),
        SMALL,
        crate::fonts::UI,
        a(theme::dim()),
    );
    // Under the head, what it is for.
    y = r.y + HEAD_H;
    if !card.about.is_empty() {
        y += 8.0;
        for line in &card.about {
            text(
                frame,
                line.clone(),
                Point::new(x, y + 8.0),
                LINE,
                crate::fonts::UI,
                a(theme::fg_muted()),
            );
            y += 16.0;
        }
    }
    for (k, (chip, label, part)) in card.chips.iter().enumerate() {
        let at = chip_rect(card, *chip);
        let hover = chip_hover == Some(k);
        let shape = Shape::rounded_rectangle(at.position(), at.size(), 5.0.into());
        let fill = if hover {
            theme::with_alpha(theme::accent(), 0.18)
        } else {
            theme::with_alpha(theme::fg(), 0.06)
        };
        frame.fill(&shape, a(fill));
        let ink = match (part, hover) {
            (_, true) => theme::accent(),
            (None, _) => theme::dim(),
            _ => theme::fg_muted(),
        };
        text(
            frame,
            label.clone(),
            Point::new(at.x + 7.0, at.center_y()),
            SMALL,
            crate::fonts::CODE,
            a(ink),
        );
    }
    if let Some(built_on) = &card.built_on {
        // Its footer, set off by a hairline: what it is built on.
        let rule_y = r.y + r.height - PAD - 14.0 - 7.0;
        frame.stroke(
            &Shape::line(
                Point::new(r.x + PAD, rule_y),
                Point::new(r.x + r.width - PAD, rule_y),
            ),
            Stroke::default()
                .with_width(1.0)
                .with_color(a(theme::hairline())),
        );
        text(
            frame,
            built_on.clone(),
            Point::new(x, r.y + r.height - PAD - 7.0),
            SMALL,
            crate::fonts::UI,
            a(theme::dim()),
        );
    }
}

/// The tip by the pointer: a card's whole description, what it uses and
/// what uses it, and where a click goes; a part's path and size — where it
/// goes, and its lines.
fn tip(
    arch: &Architecture,
    placed: &Placed,
    hit: Hit,
    at: Point,
    room: Size,
) -> Option<(Rectangle, Vec<(String, bool)>)> {
    let i = hit.card();
    let c = &arch.components[i];
    let card = &placed.cards[i];
    let names = |ids: Vec<usize>| -> String {
        ids.iter()
            .map(|&j| arch.components[j].name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines: Vec<(String, bool)> = Vec::new();
    match hit {
        Hit::Chip(_, k) => {
            let part = card
                .chips
                .get(k)
                .and_then(|ch| ch.2)
                .and_then(|p| c.parts.get(p))?;
            lines.push((part.rel.clone(), true));
            if part.dir {
                lines.push((files_said(part.files), false));
            }
            lines.push((
                tr!("Click to open {file}", file = base_name(&part.open)).to_string(),
                false,
            ));
        }
        Hit::Card(_) => {
            lines.push((
                if c.rel.is_empty() {
                    c.name.clone()
                } else {
                    c.rel.clone()
                },
                true,
            ));
            if card.about_cut
                && let Some(about) = &c.about
            {
                let (wrapped, _) = wrap(about, LINE, crate::fonts::UI, 380.0, 6);
                lines.extend(wrapped.into_iter().map(|l| (l, false)));
            }
            let uses: Vec<usize> = arch
                .links
                .iter()
                .filter(|l| l.from == i)
                .map(|l| l.to)
                .collect();
            let used_by: Vec<usize> = arch
                .links
                .iter()
                .filter(|l| l.to == i)
                .map(|l| l.from)
                .collect();
            if !uses.is_empty() {
                lines.push((tr!("Uses {names}", names = names(uses)).to_string(), false));
            }
            if !used_by.is_empty() {
                lines.push((
                    tr!("Used by {names}", names = names(used_by)).to_string(),
                    false,
                ));
            }
            if let Some(entry) = c
                .entry
                .as_ref()
                .or_else(|| c.parts.first().map(|p| &p.open))
            {
                lines.push((
                    tr!("Click to open {file}", file = base_name(entry)).to_string(),
                    false,
                ));
            }
        }
    }
    let shown: Vec<(String, bool)> = lines
        .into_iter()
        .map(|(l, head)| {
            let font = if head {
                crate::fonts::UI_SEMIBOLD
            } else {
                crate::fonts::UI
            };
            (fit(&l, LINE, font, 400.0), head)
        })
        .collect();
    let w = shown
        .iter()
        .map(|(l, head)| {
            let font = if *head {
                crate::fonts::UI_SEMIBOLD
            } else {
                crate::fonts::UI
            };
            super::steady::width_of(l, LINE, font)
        })
        .fold(0.0, f32::max)
        + 20.0;
    let h = shown.len() as f32 * 17.0 + 12.0;
    let mut x = at.x + 14.0;
    if x + w > room.width {
        x = (at.x - w - 10.0).max(0.0);
    }
    let mut y = at.y + 18.0;
    if y + h > room.height {
        y = (at.y - h - 8.0).max(0.0);
    }
    Some((Rectangle::new(Point::new(x, y), Size::new(w, h)), shown))
}

fn draw_tip(frame: &mut Frame, at: Rectangle, shown: &[(String, bool)]) {
    let (x, y) = (at.x, at.y);
    let shape = Shape::rounded_rectangle(at.position(), at.size(), 6.0.into());
    frame.fill(&shape, theme::raised());
    frame.stroke(
        &shape,
        Stroke::default()
            .with_width(1.0)
            .with_color(theme::border()),
    );
    for (k, (line, head)) in shown.iter().enumerate() {
        let (font, color) = if *head {
            (crate::fonts::UI_SEMIBOLD, theme::fg())
        } else {
            (crate::fonts::UI, theme::fg_muted())
        };
        text_on_top(
            frame,
            line.clone(),
            Point::new(x + 10.0, y + 6.0 + 17.0 * (k as f32 + 0.5)),
            LINE,
            font,
            color,
        );
    }
}

fn base_name(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, name)| name)
}

/// The overview's architecture: its title, with how to read it behind an ⓘ
/// beside it, and the diagram, `width` wide — or nothing, with no source to
/// draw.
pub(crate) fn architecture_section(app: &App, width: f32) -> Option<Element<'_, Message>> {
    let arch = app.proj.overview.architecture.as_ref()?;
    if arch.components.is_empty() {
        return None;
    }
    let rev = app.proj.overview.arch_rev;
    let placed = placed(arch, rev, width);
    let how = match arch.basis {
        architecture::Basis::Packages => {
            tr!("Each box is one of the project's packages; an arrow points to what it uses.")
        }
        architecture::Basis::Folders => {
            tr!("Each box is a part of the project's source; an arrow points to what it uses.")
        }
    };
    let height = placed.height;
    Some(
        iced::widget::column![
            iced::widget::row![
                iced::widget::text(tr!("Architecture"))
                    .size(ts::SUBTITLE)
                    .color(theme::fg_muted()),
                super::toolbar::info_tip(how),
            ]
            .spacing(6)
            .align_y(iced::Center),
            iced::widget::Space::new().height(6),
            iced::widget::canvas::Canvas::new(ArchCanvas { arch, placed, rev })
                .width(Fill)
                .height(iced::Length::Fixed(height)),
        ]
        .spacing(4)
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In every theme, the kinds a project is made of are told apart by
    /// colour — each its own, from the theme's palette: an app, its pages,
    /// its UI, its styles, its hooks, its API, its core and its data — a
    /// theme that writes two kinds of code alike too.
    /// Turned round the wheel, a colour keeps its lightness and strength.
    #[test]
    fn a_hue_turns_and_keeps_its_strength() {
        let red = Color::from_rgb(0.8, 0.2, 0.2);
        let turned = hue_turned(red, 120.0);
        assert!(
            (turned.g - 0.8).abs() < 1e-3 && (turned.r - 0.2).abs() < 1e-3,
            "{turned:?}"
        );
        assert_eq!(
            hue_turned(Color::from_rgb(0.5, 0.5, 0.5), 90.0),
            Color::from_rgb(0.5, 0.5, 0.5)
        );
    }

    #[test]
    fn each_kind_has_a_colour_of_its_own_in_every_theme() {
        let kinds = [
            Kind::WebApp,
            Kind::Pages,
            Kind::Ui,
            Kind::Styles,
            Kind::Hooks,
            Kind::Api,
            Kind::Core,
            Kind::Data,
        ];
        for def in theme::THEMES {
            let p = &def.palette;
            let colors: Vec<Color> = kinds
                .iter()
                .map(|k| kind_color_in(Some(*k), &def.tokens, p))
                .collect();
            let mut unique: Vec<Color> = Vec::new();
            for c in &colors {
                if !unique.contains(c) {
                    unique.push(*c);
                }
            }
            assert_eq!(unique.len(), kinds.len(), "{}: {colors:?}", def.id);
            // None of them a grey, the colour helpers are drawn in.
            for c in &colors {
                let chroma = c.r.max(c.g).max(c.b) - c.r.min(c.g).min(c.b);
                assert!(chroma >= 0.12 || *c == p.accent, "{}: {c:?}", def.id);
            }
        }
    }
}
