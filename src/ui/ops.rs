//! Widget operations the key handler runs against the live widget tree:
//! scrolling a list just enough to reveal a row, and cycling keyboard focus.

use iced::Rectangle;
use iced::Vector;
use iced::advanced::widget::operation::focusable::Focusable;
use iced::advanced::widget::operation::scrollable::{AbsoluteOffset, Scrollable};
use iced::advanced::widget::operation::{self, Operation, Outcome};
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
/// `i * FINDER_ROW_H ..` in the list's content.
pub(crate) fn reveal_finder_selection(selected: usize) -> Task<Message> {
    let top = selected as f32 * FINDER_ROW_H;
    reveal(finder_list_id(), top, top + FINDER_ROW_H)
}

/// What the first pass of [`cycle_focus`] learns about the focusable widgets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FocusCensus {
    pub(crate) forward: bool,
    /// Whether an open modal (a container with [`modal_scope_id`]) was seen.
    pub(crate) scoped: bool,
    pub(crate) in_scope: usize,
    pub(crate) in_scope_focused: Option<usize>,
    pub(crate) total: usize,
    pub(crate) focused: Option<usize>,
}

impl FocusCensus {
    /// The index to focus next among the widgets the cycle covers — inside
    /// the modal when one is open, else all of them — wrapping at either end;
    /// `None` when there is nothing focusable.
    pub(crate) fn next_index(&self) -> Option<usize> {
        let (count, focused) = if self.scoped {
            (self.in_scope, self.in_scope_focused)
        } else {
            (self.total, self.focused)
        };
        if count == 0 {
            return None;
        }
        Some(match (focused, self.forward) {
            (Some(i), true) => (i + 1) % count,
            (Some(i), false) => (i + count - 1) % count,
            (None, true) => 0,
            (None, false) => count - 1,
        })
    }
}

/// Tracks whether a traversal is inside the modal scope: `Container` reports
/// itself (`container`) and then immediately traverses its children, so the
/// flag set by the scope's `container` call is consumed by that `traverse`.
#[derive(Default)]
struct ScopeTracker {
    entering: bool,
    depth: usize,
}

impl ScopeTracker {
    fn container(&mut self, id: Option<&Id>) {
        self.entering = id == Some(&modal_scope_id());
    }
    fn inside(&self) -> bool {
        self.depth > 0
    }
    /// Called at the start of a `traverse`: whether it enters the scope.
    fn enter(&mut self) -> bool {
        let entering = std::mem::take(&mut self.entering);
        if entering {
            self.depth += 1;
        }
        entering
    }
    /// Called at the end of a `traverse` with what [`enter`](Self::enter) said.
    fn leave(&mut self, entered: bool) {
        if entered {
            self.depth -= 1;
        }
    }
}

/// Move keyboard focus to the next (`forward`) or previous focusable widget,
/// wrapping around. While a modal is open the cycle stays inside it — the
/// fields under its backdrop cannot be seen, so focusing one would send typing
/// somewhere invisible — and any focus left behind the modal is cleared.
pub(crate) fn cycle_focus(forward: bool) -> Task<Message> {
    iced::advanced::widget::operate(focus_cycle(forward)).discard()
}

/// The two-pass operation behind [`cycle_focus`], exposed for tests.
pub(crate) fn focus_cycle(forward: bool) -> impl Operation<()> {
    struct Census {
        census: FocusCensus,
        scope: ScopeTracker,
    }
    impl Operation<FocusCensus> for Census {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<FocusCensus>)) {
            let entered = self.scope.enter();
            self.census.scoped |= entered;
            operate(self);
            self.scope.leave(entered);
        }
        fn container(&mut self, id: Option<&Id>, _bounds: Rectangle) {
            self.scope.container(id);
        }
        fn focusable(&mut self, _id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
            if state.is_focused() {
                self.census.focused = Some(self.census.total);
            }
            self.census.total += 1;
            if self.scope.inside() {
                if state.is_focused() {
                    self.census.in_scope_focused = Some(self.census.in_scope);
                }
                self.census.in_scope += 1;
            }
        }
        fn finish(&self) -> Outcome<FocusCensus> {
            Outcome::Some(self.census)
        }
    }

    struct Apply {
        census: FocusCensus,
        target: Option<usize>,
        scope: ScopeTracker,
        seen_in_scope: usize,
        seen: usize,
    }
    impl Operation<()> for Apply {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<()>)) {
            let entered = self.scope.enter();
            operate(self);
            self.scope.leave(entered);
        }
        fn container(&mut self, id: Option<&Id>, _bounds: Rectangle) {
            self.scope.container(id);
        }
        fn focusable(&mut self, _id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
            let index = if self.census.scoped {
                if !self.scope.inside() {
                    // Behind the open modal: never left holding focus.
                    state.unfocus();
                    return;
                }
                let i = self.seen_in_scope;
                self.seen_in_scope += 1;
                i
            } else {
                let i = self.seen;
                self.seen += 1;
                i
            };
            if Some(index) == self.target {
                state.focus();
            } else {
                state.unfocus();
            }
        }
    }

    // `then` takes a plain `fn`, so the direction travels inside the census.
    operation::then(
        Census {
            census: FocusCensus {
                forward,
                ..FocusCensus::default()
            },
            scope: ScopeTracker::default(),
        },
        |census: FocusCensus| Apply {
            census,
            target: census.next_index(),
            scope: ScopeTracker::default(),
            seen_in_scope: 0,
            seen: 0,
        },
    )
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

    #[test]
    fn focus_cycles_forward_and_back_with_wraparound() {
        let census = |forward, focused| FocusCensus {
            forward,
            scoped: false,
            in_scope: 0,
            in_scope_focused: None,
            total: 3,
            focused,
        };
        assert_eq!(census(true, None).next_index(), Some(0));
        assert_eq!(census(true, Some(0)).next_index(), Some(1));
        assert_eq!(census(true, Some(2)).next_index(), Some(0));
        assert_eq!(census(false, None).next_index(), Some(2));
        assert_eq!(census(false, Some(0)).next_index(), Some(2));
        assert_eq!(census(false, Some(2)).next_index(), Some(1));
    }

    #[test]
    fn a_modal_confines_the_cycle_to_its_own_fields() {
        let open = FocusCensus {
            forward: true,
            scoped: true,
            in_scope: 2,
            in_scope_focused: Some(1),
            total: 5,
            focused: Some(4),
        };
        assert_eq!(open.next_index(), Some(0));
        let empty_modal = FocusCensus {
            in_scope: 0,
            in_scope_focused: None,
            ..open
        };
        assert_eq!(empty_modal.next_index(), None);
    }
}
