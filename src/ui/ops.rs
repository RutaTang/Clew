//! Widget operations the key handler runs against the live widget tree:
//! scrolling a list just enough to reveal a row, and cycling keyboard focus.

use iced::Rectangle;
use iced::Vector;
use iced::advanced::widget::operation::Operation;
use iced::advanced::widget::operation::scrollable::{AbsoluteOffset, Scrollable};
use iced::widget::Id;

use super::*;

/// The vertical offset that brings the content span `top..bottom` into a
/// viewport `viewport_h` tall currently scrolled to `offset`, moving as little
/// as possible — or `None` when the span is already fully visible. A span
/// taller than the viewport is aligned by its top.
pub(crate) fn reveal_offset(
    offset: f32,
    viewport_h: f32,
    content_h: f32,
    top: f32,
    bottom: f32,
) -> Option<f32> {
    let max = (content_h - viewport_h).max(0.0);
    let target = if top < offset || bottom - top > viewport_h {
        top
    } else if bottom > offset + viewport_h {
        bottom - viewport_h
    } else {
        return None;
    };
    Some(target.clamp(0.0, max))
}

/// Scrolls the scrollable `id` minimally so that the content span
/// `top..bottom` (in content coordinates) is visible.
pub(crate) fn reveal(id: Id, top: f32, bottom: f32) -> Task<Message> {
    iced::advanced::widget::operate(reveal_operation(id, top, bottom)).discard()
}

/// The widget operation behind [`reveal`].
pub(crate) fn reveal_operation(target: Id, top: f32, bottom: f32) -> impl Operation {
    struct Reveal {
        target: Id,
        top: f32,
        bottom: f32,
    }
    impl Operation for Reveal {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            bounds: Rectangle,
            content_bounds: Rectangle,
            translation: Vector,
            state: &mut dyn Scrollable,
        ) {
            if id != Some(&self.target) {
                return;
            }
            if let Some(y) = reveal_offset(
                translation.y,
                bounds.height,
                content_bounds.height,
                self.top,
                self.bottom,
            ) {
                state.scroll_to(AbsoluteOffset {
                    x: None,
                    y: Some(y),
                });
            }
        }
    }
    Reveal {
        target,
        top,
        bottom,
    }
}

/// Keep the finder's selected row in view (arrow keys move the selection past
/// the visible rows). Rows have a fixed height, so row `i` spans
/// `i * FINDER_ROW_H ..` in the list's content, plus a [`FINDER_HEAD_H`] for
/// each of the `headings` over it (the command bar's suggestions have them).
pub(crate) fn reveal_finder_selection(selected: usize, headings: usize) -> Task<Message> {
    let top = selected as f32 * FINDER_ROW_H + headings as f32 * FINDER_HEAD_H;
    reveal(finder_list_id(), top, top + FINDER_ROW_H)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_visible_row_does_not_scroll() {
        assert_eq!(reveal_offset(100.0, 300.0, 2000.0, 150.0, 176.0), None);
        // Touching both edges exactly still counts as visible.
        assert_eq!(reveal_offset(100.0, 300.0, 2000.0, 100.0, 400.0), None);
    }

    #[test]
    fn a_row_below_the_viewport_scrolls_just_enough_to_show_its_bottom() {
        assert_eq!(reveal_offset(0.0, 300.0, 2000.0, 390.0, 416.0), Some(116.0));
    }

    #[test]
    fn a_row_above_the_viewport_scrolls_to_its_top() {
        assert_eq!(reveal_offset(500.0, 300.0, 2000.0, 52.0, 78.0), Some(52.0));
    }

    #[test]
    fn reveal_offsets_never_leave_the_scrollable_range() {
        // The last row of a short list cannot scroll past the content end.
        assert_eq!(reveal_offset(0.0, 300.0, 320.0, 294.0, 320.0), Some(20.0));
        // A span taller than the viewport aligns its top.
        assert_eq!(reveal_offset(0.0, 100.0, 2000.0, 400.0, 700.0), Some(400.0));
    }
}
