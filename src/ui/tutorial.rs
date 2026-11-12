//! The interactive-tutorial overlay: a spotlight that dims everything except the
//! region a step is about, plus a callout card placed next to it.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use crate::app::tutorial::{Anchor, steps};
use crate::ui::StripTool;
use crate::ui::slide::{GLIDE, eased, slide};

/// Half the side of the spotlight hole over a single strip tool.
const TOOL_HOLE_HALF: f32 = 19.0;
/// Gap between the callout card and the region it points at.
const CARD_GAP: f32 = 24.0;

/// The screen rectangle a step points at, or `None` for a full-window dim (the
/// welcome / summary steps, or a step whose panel happens to be closed).
///
/// Every number comes from the chrome that is actually drawn: the top bar's
/// height and the chat field's place (`ui::toolbar`), the strip's slots and
/// its menus, the update banner's height when one shows (it pushes the body
/// down), the sheet's margins and header (`ui::sheet`), the panels the view
/// actually shows (`right_panel_shown`) with the divider strip beside the
/// sidebar and the bottom panel (the margin's lies over its edge),
/// and the status bar's height.
pub(crate) fn region_rect(app: &App, anchor: Anchor) -> Option<iced::Rectangle> {
    let ww = app.window_width;
    let wh = app.window_height;
    let divider = crate::resize::THICKNESS;
    let sidebar = if app.show_left_sidebar {
        app.sidebar_width
    } else {
        0.0
    };
    // The sidebar's divider lies over its edge, taking no room of its own.
    let sidebar_edge = if app.show_left_sidebar { sidebar } else { 0.0 };
    let right = if right_panel_shown(app) {
        super::panes::margin_w(app)
    } else {
        0.0
    };
    // The margin's divider lies over its edge, taking no room of its own.
    let right_edge = right;
    let bottom = if app.show_bottom {
        app.bottom_shown_height() + divider
    } else {
        0.0
    };
    let top = TOOLBAR_H + update_banner_height(app);
    let bot = wh - STATUSBAR_H;
    // The sheet: between the sidebar and the strip, its gap kept to the
    // strip, down to the status bar.
    let sheet_right = ww - STRIP_W - SHEET_GAP;
    let sheet_bottom = bot;
    // Its panes start under its header and end above the bottom panel.
    let main_top = top + sheet_header_height(app);
    let main_bot = sheet_bottom - bottom;
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        Some(iced::Rectangle {
            x,
            y,
            width: w,
            height: h,
        })
    };
    match anchor {
        Anchor::Center => None,
        // The top bar's left cluster: the window buttons (or the app menu),
        // the sidebar toggle, back / forward.
        Anchor::ToolbarLeft => rect(0.0, 0.0, left_cluster_w(app) + NAV_W, TOOLBAR_H),
        Anchor::AskCorner => {
            let r = super::askfloat::ask_corner_rect(app);
            rect(r.x, r.y, r.width, r.height)
        }
        // The ⋯, while its rows are on the strip in its place: them.
        Anchor::Strip(StripTool::More) if super::toolbar::tools_on_strip(app) => Some(
            tools_menu_rows_rect(app, 0, super::toolbar::TOOLS_MENU_ROWS.len()),
        ),
        Anchor::Strip(tool) => {
            let c = strip_tool_center(app, tool);
            rect(
                c.x - TOOL_HOLE_HALF,
                c.y - TOOL_HOLE_HALF,
                2.0 * TOOL_HOLE_HALF,
                2.0 * TOOL_HOLE_HALF,
            )
        }
        // The four ways to run, from Run to Fake Debug; folded into the
        // bug, the bug.
        Anchor::RunTools => {
            let (first, last) = run_tools_span(app);
            rect(
                first.x - TOOL_HOLE_HALF,
                first.y - TOOL_HOLE_HALF,
                2.0 * TOOL_HOLE_HALF,
                last.y - first.y + 2.0 * TOOL_HOLE_HALF,
            )
        }
        // A row (or run of rows) in the opened ⋯ menu. The hole hugs the item
        // column exactly (no bleed past the panel edge), so the inset outline
        // frames it evenly without catching a neighbour.
        Anchor::ToolbarMenu { first, count } => Some(tools_menu_rows_rect(app, first, count)),
        Anchor::FileTools => file_tools_rect(app),
        Anchor::ViewMenu => rect(
            view_menu_left(app),
            view_menu_top(app),
            view_menu_w(),
            view_menu_height(),
        ),
        Anchor::Sidebar if sidebar > 1.0 => rect(0.0, top, sidebar, bot - top),
        // The margin: its notes, beside the code.
        Anchor::RightTop if right > 1.0 => {
            rect(sheet_right - right, main_top, right, main_bot - main_top)
        }
        Anchor::Main => rect(
            sidebar_edge,
            main_top,
            sheet_right - sidebar_edge - right_edge,
            main_bot - main_top,
        ),
        _ => None,
    }
}

/// Centres of the first and last of the strip's ways to run: Run and Fake
/// Debug, or — folded into the bug's menu where the strip is short — the
/// bug, twice.
fn run_tools_span(app: &App) -> (iced::Point, iced::Point) {
    if super::toolbar::runs_on_strip(app) {
        (
            strip_tool_center(app, StripTool::Run),
            strip_tool_center(app, StripTool::FakeDebug),
        )
    } else {
        let bug = strip_tool_center(app, StripTool::RunMenu);
        (bug, bug)
    }
}

/// The rectangle `t` (eased) of the way from `a` to `b`. Nothing — the whole
/// window dimmed — is a hole of no size in the middle of the other: the hole
/// opens out of it, or closes into it.
fn between(
    a: Option<iced::Rectangle>,
    b: Option<iced::Rectangle>,
    t: f32,
) -> Option<iced::Rectangle> {
    let point = |r: iced::Rectangle| iced::Rectangle {
        x: r.center_x(),
        y: r.center_y(),
        width: 0.0,
        height: 0.0,
    };
    let mix = |a: iced::Rectangle, b: iced::Rectangle| iced::Rectangle {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
        width: a.width + (b.width - a.width) * t,
        height: a.height + (b.height - a.height) * t,
    };
    match (a, b) {
        (Some(a), Some(b)) => Some(mix(a, b)),
        (None, Some(b)) => Some(mix(point(b), b)),
        (Some(a), None) if t < 1.0 => Some(mix(a, point(a))),
        _ => None,
    }
}

/// The spotlight's way from one hole to the next: where it set out from,
/// where it is going, when it set out, and the frame being drawn — and
/// when the tour began, for the veil coming up.
#[derive(Debug, Default)]
pub(crate) struct Glide {
    from: Option<iced::Rectangle>,
    to: Option<iced::Rectangle>,
    since: Option<std::time::Instant>,
    now: Option<std::time::Instant>,
    began: Option<std::time::Instant>,
}

impl Glide {
    /// How far along the glide is at `now` (eased), 1 once it is there.
    fn progress(&self, now: std::time::Instant) -> f32 {
        match self.since {
            Some(since) => eased(now.duration_since(since).as_secs_f32() / GLIDE.as_secs_f32()),
            None => 1.0,
        }
    }

    /// A frame at `now`, the hole to show being `hole`: a new hole sets
    /// out from where the last was drawn. Whether another frame is wanted.
    pub(crate) fn frame(&mut self, hole: Option<iced::Rectangle>, now: std::time::Instant) -> bool {
        self.began.get_or_insert(now);
        if self.to != hole {
            let here = self.now.map_or(self.to, |then| {
                between(self.from, self.to, self.progress(then))
            });
            self.from = here;
            self.to = hole;
            self.since = Some(now);
        }
        self.now = Some(now);
        let settled = self.progress(now) >= 1.0;
        let up = self
            .began
            .is_some_and(|began| now.duration_since(began) >= GLIDE);
        !(settled && up)
    }

    /// The hole to draw, and how far up the veil is (0 to 1): `hole` as
    /// asked, until a frame has seen it.
    pub(crate) fn shown(&self, hole: Option<iced::Rectangle>) -> (Option<iced::Rectangle>, f32) {
        let Some(now) = self.now.filter(|_| self.to == hole) else {
            return (hole, 1.0);
        };
        let shade = self.began.map_or(1.0, |began| {
            eased(now.duration_since(began).as_secs_f32() / GLIDE.as_secs_f32())
        });
        (between(self.from, self.to, self.progress(now)), shade)
    }
}

/// Canvas that dims the window except `hole` (if any) and outlines it — the
/// spotlight. Drawn as four dim rectangles around the hole so the highlighted
/// region shows through at full brightness.
struct Spotlight {
    hole: Option<iced::Rectangle>,
    /// How far the accent outline sits from the hole edge. Positive = just
    /// outside (frames without covering the region's own content); negative =
    /// just inside (for tightly-packed rows like the ⋯ menu, so the frame never
    /// spills onto the panel edge or the next row).
    outline_pad: f32,
}

impl iced::widget::canvas::Program<Message> for Spotlight {
    type State = Glide;

    /// Each frame: the hole moved, it glides there from where it is drawn
    /// now; and a frame asked for while it is on its way.
    fn update(
        &self,
        glide: &mut Glide,
        event: &iced::widget::canvas::Event,
        _bounds: iced::Rectangle,
        _cursor: iced::advanced::mouse::Cursor,
    ) -> Option<iced::widget::canvas::Action<Message>> {
        let iced::Event::Window(iced::window::Event::RedrawRequested(now)) = event else {
            return None;
        };
        glide
            .frame(self.hole, *now)
            .then(iced::widget::canvas::Action::request_redraw)
    }

    fn draw(
        &self,
        glide: &Glide,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: iced::Rectangle,
        _cursor: iced::advanced::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry> {
        use iced::widget::canvas::{Frame, Path, Stroke};
        let mut frame = Frame::new(renderer, bounds.size());
        let (hole, shade) = glide.shown(self.hole);
        let dim = theme::scrim(0.6 * shade);
        let pt = iced::Point::new;
        let sz = iced::Size::new;
        let w = bounds.width;
        let h = bounds.height;

        // Dim the window, its corners rounded as the window's are (so
        // they stay clear), all but the hole.
        let veil = Path::new(|p| {
            p.rounded_rectangle(pt(0.0, 0.0), sz(w, h), theme::window_corners().into());
            if let Some(r) = hole {
                p.rectangle(pt(r.x, r.y), sz(r.width, r.height));
            }
        });
        frame.fill(
            &veil,
            iced::widget::canvas::Fill {
                style: iced::widget::canvas::Style::Solid(dim),
                rule: iced::widget::canvas::fill::Rule::EvenOdd,
            },
        );
        if let Some(r) = hole {
            // Accent outline offset from the region by `outline_pad`:
            // positive frames just outside (so it never covers the region's
            // own content, e.g. the breadcrumb or first code line); negative
            // frames just inside (for tight ⋯-menu rows, so it hugs the row
            // without spilling onto the panel edge or the neighbouring row).
            let pad = self.outline_pad;
            let ox = (r.x - pad).max(0.0);
            let oy = (r.y - pad).max(0.0);
            let ow = ((r.x + r.width + pad).min(w) - ox).max(0.0);
            let oh = ((r.y + r.height + pad).min(h) - oy).max(0.0);
            let outline = Path::rounded_rectangle(pt(ox, oy), sz(ow, oh), 6.0.into());
            frame.stroke(
                &outline,
                Stroke::default()
                    .with_color(theme::accent())
                    .with_width(2.0),
            );
        }
        vec![frame.into_geometry()]
    }
}

pub(crate) fn tutorial_overlay(app: &App) -> Element<'_, Message> {
    let Some(step_i) = app.tutorial else {
        return slot();
    };
    let script = steps(app);
    let total = script.len();
    let Some(step) = script.get(step_i) else {
        return slot();
    };

    // The callout card: its chapter and how far along · title · body ·
    // controls.
    let chapter: Element<'_, Message> = match step.chapter.title() {
        Some(title) => text(crate::i18n::lookup(title).to_string())
            .size(ts::SMALL)
            .color(theme::accent())
            .into(),
        None => slot(),
    };
    let count = text(tr!("{step} of {total}", step = step_i + 1, total = total))
        .size(ts::SMALL)
        .color(theme::dim());
    let progress = column![
        row![chapter, space().width(Fill), count].align_y(iced::Center),
        progress_bar((step_i + 1) as f32 / total as f32),
    ]
    .spacing(6);
    let title = text(step.title.clone()).size(ts::TITLE).color(theme::fg());
    let body = text(step.body.clone())
        .size(ts::BASE)
        .color(theme::fg_muted())
        .wrapping(Wrapping::Word);

    let back: Element<'_, Message> = if step_i > 0 {
        button(text(tr!("Back")).size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([4, 12])
            .on_press(super::heard::said(
                tr!("Back"),
                Message::Tutorial(TutorialMsg::Step(-1)),
            ))
            .into()
    } else {
        slot()
    };
    let last = step_i + 1 == total;
    let next_label = if last { tr!("Done") } else { tr!("Next") };
    let next = button(text(next_label).size(ts::BODY))
        .style(theme::primary_button)
        .padding([4, 16])
        .on_press(super::heard::said(
            next_label,
            Message::Tutorial(TutorialMsg::Step(1)),
        ));
    // "Skip tour" is the subtle exit, kept left and muted. Small horizontal
    // padding so its text lines up with the title/body above it, rather than
    // sitting indented by a wider button's internal padding.
    let skip = button(text(tr!("Skip tour")).size(ts::BODY).color(theme::dim()))
        .style(theme::toolbar_button)
        .padding([4, 6])
        .on_press(super::heard::said(
            tr!("Skip tour"),
            Message::Tutorial(TutorialMsg::Exit),
        ));

    // Back and Next form the primary control group on the right; Skip sits on the
    // left. On the last step Next reads "Done".
    let controls = row![back, next].spacing(8).align_y(iced::Center);
    let card = container(
        column![
            progress,
            title,
            body,
            row![skip, space().width(Fill), controls].align_y(iced::Center),
        ]
        .spacing(12),
    )
    .width(TUTORIAL_CARD_W)
    .padding(18)
    .style(theme::modal_panel);

    // Place the card next to the region the step is about, from the same
    // chrome metrics the spotlight uses (see `region_rect`).
    use iced::alignment::{Horizontal, Vertical};
    let gap = CARD_GAP;
    // The top of the body: under the toolbar and, when one shows, the update
    // banner.
    let body_top = TOOLBAR_H + update_banner_height(app);
    // The ⋯ dropdown's left edge, measured from the window's right edge.
    let menu_left = TOOLS_MENU_RIGHT + tools_menu_w(app);
    let lo = body_top + 14.0;
    let hi = (app.window_height - 260.0).max(lo);
    let (ax, ay, pad) = match step.anchor {
        Anchor::Center | Anchor::Main => (Horizontal::Center, Vertical::Center, Padding::ZERO),
        Anchor::Sidebar => (
            Horizontal::Left,
            Vertical::Center,
            Padding {
                left: app.sidebar_width + gap,
                ..Padding::ZERO
            },
        ),
        Anchor::ToolbarLeft => (
            Horizontal::Left,
            Vertical::Top,
            Padding {
                top: body_top + gap,
                left: gap,
                ..Padding::ZERO
            },
        ),
        // Ask is open in the bottom-right corner in this step: the card goes
        // to the bottom left, clear of it.
        Anchor::AskCorner => (
            Horizontal::Left,
            Vertical::Bottom,
            Padding {
                bottom: STATUSBAR_H + gap,
                left: gap,
                ..Padding::ZERO
            },
        ),
        // The ⋯ button with its menu open: the card to the menu's left, so
        // the two read as one.
        Anchor::Strip(StripTool::More) => (
            Horizontal::Right,
            Vertical::Top,
            Padding {
                top: (tools_menu_top(app) + 16.0).clamp(lo, hi),
                right: menu_left + gap,
                ..Padding::ZERO
            },
        ),
        // The four ways to run: the card just left of them, level with
        // their middle.
        Anchor::RunTools => {
            let (first, last) = run_tools_span(app);
            (
                Horizontal::Right,
                Vertical::Top,
                Padding {
                    top: ((first.y + last.y) / 2.0 - 60.0).clamp(lo, hi),
                    right: STRIP_W + gap,
                    ..Padding::ZERO
                },
            )
        }
        // A tool on the strip: the card just left of it, level with it.
        Anchor::Strip(tool) => (
            Horizontal::Right,
            Vertical::Top,
            Padding {
                top: (strip_tool_center(app, tool).y - 60.0).clamp(lo, hi),
                right: STRIP_W + gap,
                ..Padding::ZERO
            },
        ),
        // For a menu row, the card's middle level with the row it points
        // at, tracking the row down the menu.
        Anchor::ToolbarMenu { first, count } => {
            let rows = tools_menu_rows_rect(app, first, count);
            let center = rows.y + rows.height / 2.0;
            // Beside the strip, when the rows are on it; else the menu.
            let beside = if super::toolbar::tools_on_strip(app) {
                STRIP_W
            } else {
                menu_left
            };
            (
                Horizontal::Right,
                Vertical::Top,
                Padding {
                    top: (center - 88.0).clamp(lo, hi),
                    right: beside + gap,
                    ..Padding::ZERO
                },
            )
        }
        // Under the tools, from where they start.
        Anchor::FileTools => (
            Horizontal::Left,
            Vertical::Top,
            Padding {
                top: body_top + SHEET_HEADER_ROW_H + gap,
                left: file_tools_rect(app)
                    .map_or(gap, |r| r.x)
                    .min(app.window_width - TUTORIAL_CARD_W - STRIP_W - gap),
                ..Padding::ZERO
            },
        ),
        // Beside the open menu.
        Anchor::ViewMenu => (
            Horizontal::Left,
            Vertical::Top,
            Padding {
                top: view_menu_top(app),
                left: (view_menu_left(app) + view_menu_w() + gap)
                    .min(app.window_width - TUTORIAL_CARD_W - STRIP_W - gap),
                ..Padding::ZERO
            },
        ),
        Anchor::RightTop => (
            Horizontal::Right,
            Vertical::Top,
            Padding {
                top: body_top + 34.0,
                right: STRIP_W + SHEET_GAP + super::panes::margin_w(app) + gap,
                ..Padding::ZERO
            },
        ),
    };

    // Menu rows sit shoulder-to-shoulder, so their frame is drawn just INSIDE
    // the row; every other region frames just outside itself.
    let outline_pad = if matches!(step.anchor, Anchor::ToolbarMenu { .. }) {
        -2.0
    } else {
        2.5
    };
    let spotlight = iced::widget::canvas::Canvas::new(Spotlight {
        hole: region_rect(app, step.anchor),
        outline_pad,
    })
    .width(Fill)
    .height(Fill);

    // The card slides to each step's place, as the spotlight glides there.
    let card_layer = container(slide(opaque(card)))
        .width(Fill)
        .height(Fill)
        .align_x(ax)
        .align_y(ay)
        .padding(pad);

    // Block clicks to the app while the tour runs; clicking the dimmed backdrop
    // does nothing (leaving is via Skip / Done), so a stray click can't drop it.
    opaque(mouse_area(stack![spotlight, card_layer]).on_press(Message::Noop))
}

/// How far along the tour is, as a thin bar across the card: `done` of 1.
fn progress_bar<'a>(done: f32) -> Element<'a, Message> {
    let track = |_: &iced::Theme| container::Style {
        background: Some(theme::hairline().into()),
        border: iced::border::rounded(1.5),
        ..container::Style::default()
    };
    let fill = |_: &iced::Theme| container::Style {
        background: Some(theme::accent().into()),
        border: iced::border::rounded(1.5),
        ..container::Style::default()
    };
    let done = done.clamp(0.0, 1.0);
    // Portions of a row: what is done, and what is not.
    let parts = |share: f32| ((share * 1000.0).round() as u16).max(1);
    container(row![
        container(space())
            .width(Length::FillPortion(parts(done)))
            .height(3)
            .style(fill),
        space().width(Length::FillPortion(parts(1.0 - done))),
    ])
    .width(Fill)
    .height(3)
    .style(track)
    .into()
}

/// The sheet before the tour (see `crate::app::tutorial::TourPrep`): what
/// the tour needs, one step at a time — the step it is on lit and its
/// button the one to press, a step done ticked — and Start the Tour once
/// what it cannot do without is there.
pub(crate) fn tour_prep_sheet<'a>(
    app: &'a App,
    prep: &'a crate::app::tutorial::TourPrep,
) -> Element<'a, Message> {
    use super::chrome::{Backdrop, CONFIRM_PAD, DIALOG_W, Placement, modal};
    use crate::app::tutorial::Need;
    let current = prep.current(app);
    let sample = prep.sample.as_ref();

    let header = row![
        text(tr!("Before the tour"))
            .size(ts::TITLE)
            .color(theme::fg()),
        space().width(Fill),
        super::chrome::close_button_saying(
            super::heard::said(tr!("Not now"), Message::Tutorial(TutorialMsg::PrepClose)),
            tr!("Not now"),
        ),
    ]
    .align_y(iced::Center);
    let intro = text(tr!(
        "The tour shows Clew on Tea Shop, a small project it comes with, in a window of its own. It needs a few things first."
    ))
    .size(ts::BASE)
    .color(theme::fg_muted())
    .wrapping(Wrapping::Word);

    // One step: its marker (a tick when done, its number lit when it is
    // the one to do, else dim), its name, what it is for, and its buttons.
    // A model is ticked once it answered: a key alone is checked first.
    use crate::app::state::ConnectionTest;
    let tested = &app.model_check.result;
    let model_ok = prep.met(app, Need::Model);
    let step = |number: usize,
                need: Need,
                name: &'a str,
                optional: bool,
                about: String,
                actions: Vec<Element<'a, Message>>|
     -> Element<'a, Message> {
        let done = prep.met(app, need);
        let on = current == Some(need);
        // Tea Shop ready, with a model to explain it: what is ready is a
        // spend the tour starts, not a thing done — ticked quietly, where
        // a green tick read "nothing to think about".
        let spends = need == Need::Server && app.llm_available;
        let marker: Element<'a, Message> = if done && spends {
            container(glyph::icon(Glyph::Check, theme::fg_muted(), 12.0))
                .center(22)
                .style(|_: &iced::Theme| container::Style {
                    border: iced::border::rounded(11)
                        .width(1.0)
                        .color(theme::hairline()),
                    ..container::Style::default()
                })
                .into()
        } else if done {
            container(glyph::icon(Glyph::Check, theme::bg(), 12.0))
                .center(22)
                .style(|_: &iced::Theme| container::Style {
                    background: Some(theme::success().into()),
                    border: iced::border::rounded(11),
                    ..container::Style::default()
                })
                .into()
        } else {
            container(text(number.to_string()).size(ts::SMALL).color(if on {
                theme::bg()
            } else {
                theme::dim()
            }))
            .center(22)
            .style(move |_: &iced::Theme| container::Style {
                background: on.then(|| theme::accent().into()),
                border: iced::border::rounded(11)
                    .width(if on { 0.0 } else { 1.0 })
                    .color(theme::hairline()),
                ..container::Style::default()
            })
            .into()
        };
        let mut title = row![text(name).size(ts::EMPHASIS).color(if done || on {
            theme::fg()
        } else {
            theme::fg_muted()
        })]
        .spacing(8)
        .align_y(iced::Center);
        if optional {
            title = title.push(text(tr!("Optional")).size(ts::SMALL).color(theme::dim()));
        }
        let mut lines = column![
            title,
            text(about)
                .size(ts::BODY)
                .color(theme::fg_muted())
                .wrapping(Wrapping::Word),
        ]
        .spacing(4);
        if !done && !actions.is_empty() {
            lines = lines.push(row(actions).spacing(8).padding(Padding {
                top: 4.0,
                ..Padding::ZERO
            }));
        }
        row![marker, lines.width(Fill)].spacing(14).into()
    };
    // A step's button: the lit step's the primary one, every other quiet.
    let action = |need: Need, label: &'a str, msg: Message| -> Element<'a, Message> {
        let style = if current == Some(need) {
            theme::primary_button
        } else {
            theme::secondary_button
        };
        button(text(label).size(ts::BODY))
            .style(style)
            .padding([4, 12])
            .on_press(super::heard::said(label, msg))
            .into()
    };

    // "All set" only once the model answered: with a key and a provider
    // that could not be reached, it said "All set", and the tour then set
    // up some 45 calls that failed.
    let model = match (app.llm_available, tested) {
        (false, _) => tr!(
            "Clew explains code and answers questions with a language model you choose. Add its API key."
        )
        .to_string(),
        (true, ConnectionTest::Answered(_)) => tr!("All set.").to_string(),
        (true, ConnectionTest::Testing(_)) => tr!("Checking that it answers…").to_string(),
        (true, ConnectionTest::Failed(why)) => crate::i18n::message(why),
        (true, ConnectionTest::Untried) => {
            tr!("Key added. Check that it answers before the tour spends calls on it.")
                .to_string()
        }
    };
    let embeddings = if app.embed_available {
        tr!("All set.").to_string()
    } else if prep.skip_embeddings {
        tr!("Skipped: the tour shows where it is, without results.").to_string()
    } else {
        tr!("Search code by what it does. With a model on this computer, or a service.").to_string()
    };
    let shop = match sample {
        None => tr!("Getting Tea Shop ready…").to_string(),
        Some(Err(e)) => e.clone(),
        Some(Ok((_, census))) => tea_shop_line(
            census,
            app.llm_available,
            app.embed_available,
            census.calls.and_then(|n| app.explain_estimate(n)),
        ),
    };

    let steps = column![
        step(
            1,
            Need::Model,
            tr!("A language model"),
            false,
            model,
            if app.llm_available && !model_ok {
                // One tiny call, as Settings' Test connection makes; and
                // the settings it may be about.
                vec![
                    super::toolbar::tip_at(
                        super::form::ai_action(
                            tr!("Test connection"),
                            (!matches!(tested, ConnectionTest::Testing(_)))
                                .then_some(Message::Settings(SettingsMsg::TestSavedConnection)),
                        ),
                        tr!("Sends one tiny AI request to check the key (a fraction of a cent)"),
                        None,
                        iced::widget::tooltip::Position::Top,
                    ),
                    button(text(tr!("Settings…")).size(ts::BODY))
                        .style(theme::toolbar_button)
                        .padding([4, 12])
                        .on_press(super::heard::said(
                            tr!("Settings…"),
                            Message::Tutorial(TutorialMsg::SetUp(
                                crate::SettingsPage::LanguageModel,
                            )),
                        ))
                        .into(),
                ]
            } else {
                vec![action(
                    Need::Model,
                    tr!("Add an API Key…"),
                    Message::Tutorial(TutorialMsg::SetUp(crate::SettingsPage::LanguageModel)),
                )]
            },
        ),
        step(
            2,
            Need::Embeddings,
            tr!("Find by meaning"),
            true,
            embeddings,
            vec![
                action(
                    Need::Embeddings,
                    tr!("Set Up…"),
                    Message::Tutorial(TutorialMsg::SetUp(crate::SettingsPage::Embeddings)),
                ),
                button(text(tr!("Skip")).size(ts::BODY))
                    .style(theme::toolbar_button)
                    .padding([4, 12])
                    .on_press(super::heard::said(
                        tr!("Skip"),
                        Message::Tutorial(TutorialMsg::SkipEmbeddings),
                    ))
                    .into(),
            ],
        ),
        step(3, Need::Server, tr!("Tea Shop"), false, shop, Vec::new()),
    ]
    .spacing(18);

    let ready = prep.ready(app);
    // The tour explains Tea Shop with the model: marked as what spends AI
    // calls, as every such control is.
    let start = button(super::toolbar::ai_words(
        tr!("Start the tour"),
        ts::BASE,
        theme::on_accent(),
    ))
    .style(theme::primary_button)
    .padding([6, 16])
    .on_press_maybe(
        ready
            .then(|| super::heard::said(tr!("Start the tour"), Message::Tutorial(TutorialMsg::Go))),
    );
    // Why it cannot start yet, said beside it: in a tip alone (which a
    // held button may not show), a press on the tour's main button did
    // nothing, with no word why.
    let waiting: Element<'_, Message> = if ready {
        space().width(Fill).into()
    } else {
        text(if !app.llm_available {
            tr!("The tour needs a language model first (step 1).")
        } else if !prep.met(app, Need::Model) {
            tr!("The tour starts once the model answers a test (step 1).")
        } else {
            tr!("The tour starts once Tea Shop is ready (step 3).")
        })
        .size(ts::SMALL)
        .color(theme::dim())
        .wrapping(Wrapping::WordOrGlyph)
        .width(Fill)
        .into()
    };
    let start: Element<'_, Message> = start.into();
    let not_now = button(text(tr!("Not now")).size(ts::BASE))
        .style(theme::toolbar_button)
        .padding([6, 16])
        .on_press(super::heard::said(
            tr!("Not now"),
            Message::Tutorial(TutorialMsg::PrepClose),
        ));

    let panel = container(
        column![
            header,
            intro,
            steps,
            row![waiting, not_now, start]
                .spacing(10)
                .align_y(iced::Center),
        ]
        .spacing(18),
    )
    .width(DIALOG_W)
    .padding(CONFIRM_PAD)
    .style(theme::modal_panel);
    modal(panel, Placement::Center, Backdrop::Dim(None))
}

/// What the tour's step for its sample, Tea Shop, says once the sample is
/// looked at: its language server — ready, or set up as the tour starts —
/// and, with a model to explain it with, what explaining it takes. With
/// none, the step above it, the model's, says so: said here too, "Needs an
/// API key in Settings." read as the server's.
pub(crate) fn tea_shop_line(
    census: &crate::app::setup::Census,
    llm_available: bool,
    embed_available: bool,
    estimate: Option<crate::app::usage::Estimate>,
) -> String {
    let servers = if census.servers.is_empty() {
        tr!("Its language server is ready.").to_string()
    } else {
        tr!(
            "Set up as the tour starts: {servers}.",
            servers = crate::app::setup::servers_line(&census.servers)
        )
    };
    if !llm_available {
        return servers;
    }
    // What explaining it takes, as the Explain sheet says it: the calls,
    // their tokens and their money — not a count alone, the one estimate
    // in the app that left the money out.
    let calls = match (census.calls, estimate) {
        (Some(n), Some(e)) if n > 0 => {
            let said = e.said(n);
            if embed_available {
                format!("{said} {}", tr!("Also builds the semantic index."))
            } else {
                said
            }
        }
        _ => crate::app::setup::explain_line(true, Some(census.calls), embed_available),
    };
    format!("{servers} {calls}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Tea Shop's step says what its server takes — and, with no model to
    /// explain it with, nothing of a key: the model's step says so, and here
    /// "Needs an API key in Settings." read as the server's.
    #[test]
    fn tea_shops_step_says_nothing_of_a_key() {
        let census = crate::app::setup::Census {
            servers: vec![crate::app::setup::SetupServer {
                language: "python".into(),
                name: "pyright".into(),
                fetch: crate::app::setup::Fetch::Nothing,
                builds: false,
            }],
            calls: Some(12),
        };
        let without = tea_shop_line(&census, false, false, None);
        assert!(without.contains("pyright"), "{without}");
        assert!(!without.contains("API key"), "{without}");
        let with = tea_shop_line(&census, true, false, None);
        assert!(with.contains("AI call"), "what explaining takes: {with}");
        // With an estimate, its tokens and money too, as the Explain sheet
        // says them.
        let estimate = crate::app::usage::Estimate {
            tokens: 20_400,
            cost: Some(0.0123),
            from_history: false,
        };
        let priced = tea_shop_line(&census, true, false, Some(estimate));
        assert!(priced.contains(&estimate.said(12)), "{priced}");
        assert!(priced.contains("tokens"), "{priced}");
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> iced::Rectangle {
        iced::Rectangle {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// The spotlight's veil comes up as the tour begins; a hole opens out
    /// of its own middle, glides from one step's region to the next and
    /// closes into its middle again — frames asked for while it moves,
    /// none once it is there.
    #[test]
    fn the_spotlight_glides_from_step_to_step_and_settles() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut glide = Glide::default();

        // The welcome: no hole, the veil coming up.
        assert!(glide.frame(None, ms(0)));
        let (hole, shade) = glide.shown(None);
        assert_eq!((hole, shade), (None, 0.0));
        assert!(!glide.frame(None, ms(400)), "up, and still: no frames");
        assert_eq!(glide.shown(None), (None, 1.0));

        // A hole opens out of its middle.
        let a = rect(100.0, 100.0, 200.0, 100.0);
        assert!(glide.frame(Some(a), ms(500)));
        let opening = glide.shown(Some(a)).0.unwrap();
        assert_eq!((opening.center_x(), opening.width), (200.0, 0.0));
        assert!(glide.frame(Some(a), ms(600)));
        let opened = glide.shown(Some(a)).0.unwrap();
        assert!(opened.width > 100.0 && opened.width < 200.0, "{opened:?}");
        assert!(!glide.frame(Some(a), ms(900)));
        assert_eq!(glide.shown(Some(a)).0, Some(a));

        // On to the next region: from this one, not from nowhere.
        let b = rect(500.0, 300.0, 40.0, 40.0);
        assert!(glide.frame(Some(b), ms(1000)));
        assert_eq!(glide.shown(Some(b)).0, Some(a));
        assert!(glide.frame(Some(b), ms(1100)));
        let on_the_way = glide.shown(Some(b)).0.unwrap();
        assert!(on_the_way.x > a.x && on_the_way.x < b.x, "{on_the_way:?}");
        assert!(!glide.frame(Some(b), ms(1400)));
        assert_eq!(glide.shown(Some(b)).0, Some(b));

        // A hole not yet seen by a frame is drawn where it is asked.
        assert_eq!(glide.shown(Some(a)), (Some(a), 1.0));

        // Back to no hole: it closes into its middle, then is gone.
        assert!(glide.frame(None, ms(2000)));
        assert!(glide.frame(None, ms(2100)));
        let closing = glide.shown(None).0.unwrap();
        assert!(closing.width < b.width && closing.center_x() == b.center_x());
        assert!(!glide.frame(None, ms(2400)));
        assert_eq!(glide.shown(None).0, None);
    }
}
