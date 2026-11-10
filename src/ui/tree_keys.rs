//! The Files view's tree, as one stop the keyboard can rest on (see
//! [`TreeKeys`]).

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::operation::Focusable;
use iced::advanced::widget::{Id, Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, mouse, overlay, renderer};
use iced::keyboard::{self, key};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

use crate::app::message::TreeKey;
use crate::{Message, WindowMsg};

/// `content` — the Files view's tree — made one focusable stop, as a text
/// field is: a click on it, F6, or ↓ from the filter above it gives it the
/// keyboard, and a click anywhere else takes it away. While it has the
/// keyboard, the keys that walk a tree are its own ([`TreeKey`]), and the
/// code under them sees none of them.
pub(crate) struct TreeKeys<'a> {
    content: Element<'a, Message>,
    id: Id,
    /// The code has the keyboard: the tree lets it go.
    release: bool,
}

impl<'a> TreeKeys<'a> {
    pub(crate) fn new(content: impl Into<Element<'a, Message>>, id: Id) -> Self {
        Self {
            content: content.into(),
            id,
            release: false,
        }
    }

    /// The window gave the keyboard to the code — a file opened from the
    /// tree is read, `j` and `k` moving in it, as before the tree took
    /// keys of its own — or something above the tree holds the keys (a
    /// dialog, the tutorial): the tree lets them go.
    pub(crate) fn release(mut self, release: bool) -> Self {
        self.release = release;
        self
    }
}

/// Whether it has the keyboard, and whether the window was last told so.
#[derive(Default)]
struct State {
    focused: bool,
    told: bool,
}

impl Focusable for State {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn focus(&mut self) {
        self.focused = true;
    }

    fn unfocus(&mut self) {
        self.focused = false;
    }
}

/// The tree move a key makes, when the tree has the keyboard: a plain key
/// (no ⌘, Ctrl or Alt — those are the window's shortcuts). `key` is the key
/// pressed, `typed` what it types with the modifiers held (Shift+`-` is
/// `_`).
pub(crate) fn tree_key(
    key: &keyboard::Key,
    typed: &keyboard::Key,
    modifiers: keyboard::Modifiers,
) -> Option<TreeKey> {
    if modifiers.command() || modifiers.control() || modifiers.alt() {
        return None;
    }
    // The reading keys walk the tree as they walk the code: `j` and `k`
    // down and up, `h` out of a folder, `l` into one.
    if !modifiers.shift()
        && let keyboard::Key::Character(c) = key.as_ref()
    {
        match c {
            "j" => return Some(TreeKey::Down),
            "k" => return Some(TreeKey::Up),
            "h" => return Some(TreeKey::Left),
            "l" => return Some(TreeKey::Right),
            _ => {}
        }
    }
    Some(match key.as_ref() {
        keyboard::Key::Named(named) => match named {
            key::Named::ArrowUp => TreeKey::Up,
            key::Named::ArrowDown => TreeKey::Down,
            key::Named::ArrowLeft => TreeKey::Left,
            key::Named::ArrowRight => TreeKey::Right,
            key::Named::Home => TreeKey::First,
            key::Named::End => TreeKey::Last,
            key::Named::PageUp => TreeKey::PageUp,
            key::Named::PageDown => TreeKey::PageDown,
            key::Named::Enter => TreeKey::Open,
            key::Named::Space => TreeKey::Preview,
            key::Named::Escape => TreeKey::Leave,
            _ => return None,
        },
        // A letter typed narrows the tree to the files it names: the
        // filter above takes it, and what comes after — as typed.
        keyboard::Key::Character(_) => {
            let keyboard::Key::Character(typed) = typed.as_ref() else {
                return None;
            };
            let c = typed.chars().next().filter(|c| !c.is_control())?;
            TreeKey::Type(c)
        }
        keyboard::Key::Unidentified => return None,
    })
}

impl Widget<Message, iced::Theme, iced::Renderer> for TreeKeys<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State>();
        operation.focusable(Some(&self.id), layout.bounds(), state);
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        if self.release {
            state.focused = false;
        }
        match event {
            // A press on the tree gives it the keyboard; one anywhere else
            // takes it away, as a text field's does.
            Event::Mouse(mouse::Event::ButtonPressed(_)) => {
                state.focused = cursor.is_over(layout.bounds());
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modified_key,
                modifiers,
                ..
            }) if state.focused => {
                if let Some(step) = tree_key(key, modified_key, *modifiers) {
                    // Esc gives the keyboard back, to the code.
                    if step == TreeKey::Leave {
                        state.focused = false;
                    }
                    shell.publish(Message::Window(WindowMsg::TreeKey(step)));
                    shell.capture_event();
                    shell.request_redraw();
                }
            }
            _ => {}
        }
        // The window is told when the keyboard comes or goes — by a key
        // elsewhere (F6, ↓ in the filter) as well as by a click — for the
        // row it rests on to be drawn so.
        if state.focused != state.told {
            state.told = state.focused;
            shell.publish(Message::Window(WindowMsg::TreeFocused(state.focused)));
            shell.request_redraw();
        }
        if shell.is_event_captured() {
            return;
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<TreeKeys<'a>> for Element<'a, Message> {
    fn from(keys: TreeKeys<'a>) -> Self {
        Element::new(keys)
    }
}
