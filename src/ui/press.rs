//! The window's buttons: iced's, each holding on to its press for jump
//! mode ([`super::jump`]) while the codes are up — so that every button a
//! pointer can press, a typed code can too, with no button left out.

use iced::widget::button as iced_button;
use iced::{Element, Length, Padding};

use crate::Message;

/// What a button's look is made of, as iced has it.
pub(crate) mod button {
    pub(crate) use iced::widget::button::{Status, Style};
}

/// A button showing `content`: iced's [`button`](iced::widget::button()),
/// marked for jump mode once it has a press.
pub(crate) fn button<'a>(content: impl Into<Element<'a, Message>>) -> Press<'a> {
    Press {
        inner: iced_button(content),
        press: None,
        name: None,
    }
}

/// A button being built ([`button()`]).
pub(crate) struct Press<'a> {
    inner: iced_button::Button<'a, Message>,
    /// Its press, kept only while a mark wants it ([`super::jump::wanted`]).
    press: Option<Message>,
    /// What it is called, as its control said when it was given its press.
    name: Option<String>,
}

impl<'a> Press<'a> {
    /// What pressing it sends.
    pub(crate) fn on_press(mut self, message: Message) -> Self {
        if super::jump::wanted() {
            self.name = super::jump::naming()
                .then(|| super::heard::name_of(&message))
                .flatten();
            self.press = Some(message.clone());
        }
        self.inner = self.inner.on_press(message);
        self
    }

    /// What pressing it sends, if anything: none leaves it disabled.
    pub(crate) fn on_press_maybe(self, message: Option<Message>) -> Self {
        match message {
            Some(message) => self.on_press(message),
            None => Press {
                inner: self.inner.on_press_maybe(None),
                press: None,
                name: None,
            },
        }
    }

    pub(crate) fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.inner = self.inner.padding(padding);
        self
    }

    pub(crate) fn width(mut self, width: impl Into<Length>) -> Self {
        self.inner = self.inner.width(width);
        self
    }

    pub(crate) fn height(mut self, height: impl Into<Length>) -> Self {
        self.inner = self.inner.height(height);
        self
    }

    pub(crate) fn style(
        mut self,
        style: impl Fn(&iced::Theme, button::Status) -> button::Style + 'a,
    ) -> Self {
        self.inner = self.inner.style(style);
        self
    }
}

impl<'a> From<Press<'a>> for Element<'a, Message> {
    fn from(press: Press<'a>) -> Self {
        match press.press {
            Some(message) => super::jump::mark_named(press.inner, message, press.name),
            None => press.inner.into(),
        }
    }
}
