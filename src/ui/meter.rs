//! Bars that ease toward what they show, frame by frame: the microphone's
//! level meter ([`level_meter`]) and a bar for a level or for progress
//! ([`eased_bar`]).

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, renderer};
use iced::{Element, Event, Length, Rectangle, Size};
use std::time::Instant;

use crate::theme;

/// Width of one bar, and of the gap between two.
const BAR: f32 = 2.0;
/// Shortest a bar is drawn, at silence.
const LEAST: f32 = 2.0;
/// How a shown value eases toward the one it is given: the time constants
/// (seconds) it rises and falls with — 0 for at once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ease {
    rise: f32,
    fall: f32,
}

/// A sound level: quick to rise, so a word shows as it is said, slower to
/// fall, so the bars sink rather than flicker between syllables.
pub(crate) const LEVEL: Ease = Ease {
    rise: 0.045,
    fall: 0.16,
};

/// Progress: glides forward between the reports that come in batches; a
/// new run's drop back to nothing shows at once.
pub(crate) const PROGRESS: Ease = Ease {
    rise: 0.2,
    fall: 0.0,
};
/// A shown level this close to the heard one has arrived.
const ARRIVED: f32 = 0.002;

/// How loud the microphone hears, as bars in the accent: each of `heights`
/// grown with the `level` (0–1).
///
/// The level is read a few times a second; the meter eases toward each
/// reading frame by frame, asking for frames only until it gets there, so
/// it moves at the display's rate without the whole window being rebuilt
/// for it.
pub(crate) fn level_meter<'a, Message: 'a>(heights: &'a [f32], level: f32) -> Element<'a, Message> {
    Element::new(Meter {
        heights,
        level: level.clamp(0.0, 1.0),
    })
}

/// A horizontal bar `length` long and `girth` thick, filled to `share`
/// (0–1) in `style`'s colours — the shown fill eased toward `share` as
/// `ease` says, frame by frame, asking for frames only until it is there.
pub(crate) fn eased_bar<'a, Message: 'a>(
    share: f32,
    length: f32,
    girth: f32,
    style: fn(&iced::Theme) -> iced::widget::progress_bar::Style,
    ease: Ease,
) -> Element<'a, Message> {
    Element::new(Bar {
        share: share.clamp(0.0, 1.0),
        length,
        girth,
        style,
        ease,
    })
}

struct Meter<'a> {
    heights: &'a [f32],
    level: f32,
}

struct Bar {
    share: f32,
    length: f32,
    girth: f32,
    style: fn(&iced::Theme) -> iced::widget::progress_bar::Style,
    ease: Ease,
}

/// The level the meter shows, and when it last moved it.
struct Shown {
    level: f32,
    at: Option<Instant>,
}

/// How tall the bars grow at `level`: a third of their height at silence,
/// all of it at the loudest.
fn gain(level: f32) -> f32 {
    0.35 + 0.65 * level
}

/// `shown` moved toward `heard` for `dt` seconds, as `how` says.
fn ease(how: Ease, shown: f32, heard: f32, dt: f32) -> f32 {
    let tau = if heard > shown { how.rise } else { how.fall };
    if tau <= 0.0 {
        return heard;
    }
    let next = shown + (heard - shown) * (1.0 - (-dt / tau).exp());
    if (next - heard).abs() <= ARRIVED {
        heard
    } else {
        next
    }
}

/// One frame of a shown value: eased toward `heard` by the time since the
/// last (one frame's worth after a pause), and a frame asked for while it
/// is not there yet.
fn step<Message>(
    shown: &mut Shown,
    heard: f32,
    how: Ease,
    now: Instant,
    shell: &mut Shell<'_, Message>,
) {
    let dt = shown
        .at
        .map(|at| now.duration_since(at).as_secs_f32())
        .filter(|&gap| gap <= 0.1)
        .unwrap_or(1.0 / 60.0);
    shown.level = ease(how, shown.level, heard, dt);
    if shown.level == heard {
        shown.at = None;
    } else {
        shown.at = Some(now);
        shell.request_redraw();
    }
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Meter<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Shown>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Shown {
            level: 0.0,
            at: None,
        })
    }

    fn size(&self) -> Size<Length> {
        let n = self.heights.len() as f32;
        let tallest = self.heights.iter().copied().fold(0.0_f32, f32::max);
        Size::new(
            Length::Fixed((2.0 * n - 1.0).max(0.0) * BAR),
            Length::Fixed(tallest),
        )
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = <Self as Widget<Message, iced::Theme, iced::Renderer>>::size(self);
        layout::atomic(limits, size.width, size.height)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let Event::Window(iced::window::Event::RedrawRequested(now)) = event else {
            return;
        };
        step(
            tree.state.downcast_mut::<Shown>(),
            self.level,
            LEVEL,
            *now,
            shell,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &iced::Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        let bounds = layout.bounds();
        let gain = gain(tree.state.downcast_ref::<Shown>().level);
        let color = theme::accent();
        for (i, &h) in self.heights.iter().enumerate() {
            let height = (h * gain).max(LEAST);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: bounds.x + i as f32 * 2.0 * BAR,
                        y: bounds.center_y() - height / 2.0,
                        width: BAR,
                        height,
                    },
                    border: iced::border::rounded(BAR / 2.0),
                    // Unsnapped: a bar grows by fractions of a pixel, not
                    // in whole-pixel steps.
                    snap: false,
                    ..renderer::Quad::default()
                },
                color,
            );
        }
    }
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Bar {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Shown>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Shown {
            level: 0.0,
            at: None,
        })
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.length), Length::Fixed(self.girth))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(
            limits,
            Length::Fixed(self.length),
            Length::Fixed(self.girth),
        )
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        if let Event::Window(iced::window::Event::RedrawRequested(now)) = event {
            step(
                tree.state.downcast_mut::<Shown>(),
                self.share,
                self.ease,
                *now,
                shell,
            );
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        let bounds = layout.bounds();
        let style = (self.style)(theme);
        let quad = |bounds| renderer::Quad {
            bounds,
            border: style.border,
            snap: false,
            ..renderer::Quad::default()
        };
        renderer.fill_quad(quad(bounds), style.background);
        let shown = tree.state.downcast_ref::<Shown>().level;
        if shown > 0.0 {
            renderer.fill_quad(
                quad(Rectangle {
                    width: bounds.width * shown,
                    ..bounds
                }),
                style.bar,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shown level rises quickly and falls slowly, the same over a
    /// second whatever the frame rate, and arrives exactly.
    #[test]
    fn the_shown_level_eases_the_same_at_any_frame_rate() {
        let run = |fps: u32, heard: f32, from: f32, secs: f32| {
            let dt = 1.0 / fps as f32;
            (0..(secs * fps as f32).round() as usize)
                .fold(from, |shown, _| ease(LEVEL, shown, heard, dt))
        };
        // Rising: most of the way in a tenth of a second.
        let at60 = run(60, 1.0, 0.0, 0.1);
        let at144 = run(144, 1.0, 0.0, 0.1);
        assert!(at60 > 0.85, "{at60}");
        assert!((at60 - at144).abs() < 0.02, "{at60} vs {at144}");
        // Falling: slower.
        let fell = run(60, 0.0, 1.0, 0.1);
        assert!(fell > 0.4, "it sinks, not drops: {fell}");
        // And it gets there.
        assert_eq!(run(60, 0.5, 0.0, 2.0), 0.5);
        assert_eq!(run(60, 0.0, 1.0, 2.0), 0.0);
    }

    /// The meter asks for frames while its bars move toward a reading, and
    /// none once they are there: a quiet microphone costs nothing.
    #[test]
    fn the_meter_asks_for_frames_only_while_its_bars_move() {
        use iced::window::RedrawRequest;
        use iced_test::runtime::user_interface::{Cache, State, UserInterface};
        let mut renderer = iced_test::futures::futures::executor::block_on(
            <iced::Renderer as iced::advanced::renderer::Headless>::new(
                iced::Font::default(),
                iced::Pixels(16.0),
                None,
            ),
        )
        .expect("a headless renderer");
        let bars = [5.0, 9.0, 13.0];
        let t0 = Instant::now();
        let mut cache = Cache::default();
        let mut frame = |level: f32, k: u64, cache: Cache| {
            let mut ui = UserInterface::build(
                level_meter::<()>(&bars, level),
                Size::new(100.0, 40.0),
                cache,
                &mut renderer,
            );
            let now = t0 + std::time::Duration::from_micros(16_667 * k);
            let (state, _) = ui.update(
                &[Event::Window(iced::window::Event::RedrawRequested(now))],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Vec::new(),
            );
            let wants = matches!(
                state,
                State::Updated {
                    redraw_request: RedrawRequest::NextFrame,
                    ..
                }
            );
            (wants, ui.into_cache())
        };
        let mut asked = Vec::new();
        for k in 0..90 {
            let (wants, next) = frame(1.0, k, cache);
            cache = next;
            asked.push(wants);
        }
        assert!(asked[0], "a new reading moves the bars");
        assert!(!asked[89], "at rest, no frames: {asked:?}");
        let moving = asked.iter().take_while(|&&a| a).count();
        assert!(
            moving > 3 && asked[moving..].iter().all(|&a| !a),
            "{asked:?}"
        );
    }

    /// No step is ever a jump: each frame moves the shown level a share of
    /// what is left, never all of it at once until it is close.
    #[test]
    fn a_reading_is_eased_toward_not_jumped_to() {
        let first = ease(LEVEL, 0.0, 1.0, 1.0 / 60.0);
        assert!(first > 0.2 && first < 0.4, "{first}");
        let down = ease(LEVEL, 1.0, 0.0, 1.0 / 60.0);
        assert_eq!(
            ease(PROGRESS, 0.8, 0.0, 1.0 / 60.0),
            0.0,
            "a new run starts at nothing"
        );
        assert!(down > 0.85 && down < 0.95, "{down}");
    }
}
