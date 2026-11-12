//! The drop-down open in the window ([`Picker`], drawn by `ui::picker`):
//! what its rows send, and the row the keys rest on. Opened by a click on
//! it, or by Enter, Space or its jump code from the keyboard; the arrows,
//! Home, End and Page Up / Down walk its rows, a letter goes to the next
//! row beginning with it, Enter or Space picks, Esc or Tab puts it away.

use crate::app::prelude::*;
use crate::*;

/// An open drop-down.
#[derive(Debug, Clone)]
pub struct Picker {
    /// Which drop-down it is (`ui::picker`'s key of its rows).
    pub key: u64,
    /// Each row's text, and what picking it sends.
    pub picks: Vec<(String, Message)>,
    /// The row the keys rest on.
    pub at: usize,
    /// The row chosen when it opened.
    pub chosen: Option<usize>,
    /// What it was opened over (`ui::ActiveOverlay::scene`): a key once
    /// that is gone — a dialog closed by its ×, Settings by a click away —
    /// is not this drop-down's, now not drawn.
    pub owner: Option<String>,
    /// How many times the window had been built as it opened
    /// (`ui::picker::drawn`).
    pub built: u64,
}

#[derive(Debug, Clone)]
pub enum PickerMsg {
    /// Opened: by a click on it, or by Enter, Space or its jump code.
    Open(Picker),
    /// Put away, nothing picked: a click away from it, or on it again.
    Close,
    /// Row `n` picked.
    Pick(usize),
}

impl App {
    pub(crate) fn update_picker(&mut self, message: PickerMsg) -> Task<Message> {
        match message {
            PickerMsg::Open(mut picker) => {
                if picker.picks.is_empty() {
                    return Task::none();
                }
                picker.at = picker.at.min(picker.picks.len() - 1);
                picker.owner = self.picker_owner();
                picker.built = crate::ui::picker::built();
                let n = picker.picks.len();
                let at = picker.at;
                self.picker = Some(picker);
                keep_in_view(at, n)
            }
            PickerMsg::Close => {
                self.picker = None;
                Task::none()
            }
            PickerMsg::Pick(n) => {
                let Some(picker) = self.picker.take() else {
                    return Task::none();
                };
                match picker.picks.into_iter().nth(n) {
                    Some((_, send)) => self.update(send),
                    None => Task::none(),
                }
            }
        }
    }

    /// What a drop-down opened now is opened over: a dialog, or what else
    /// takes every press — not a peek at a symbol under the pointer, which
    /// comes and goes as the pointer moves (a list opened over one went
    /// with it, under the pointer on its way to a row).
    fn picker_owner(&self) -> Option<String> {
        crate::ui::active_overlay(self)
            .filter(|o| o.holds_keys())
            .map(|o| o.scene())
    }

    /// Whether the open drop-down is still the one the window shows: what
    /// it was opened over is still there, and its list was drawn.
    fn picker_stands(&self, picker: &Picker) -> bool {
        picker.owner == self.picker_owner() && crate::ui::picker::drawn(picker.key, picker.built)
    }

    /// An open drop-down whose dialog went, that a dialog came up over — a
    /// question that takes every press — or whose list is no longer drawn
    /// (its rows changed) is put away: it stood open over the question,
    /// and came back once it was answered.
    pub(crate) fn drop_stale_picker(&mut self) {
        if self.picker.as_ref().is_some_and(|p| !self.picker_stands(p)) {
            self.picker = None;
        }
    }

    /// A row of the open drop-down picked other than from its list — by
    /// voice, which says each row — puts it away, picked: it stood open
    /// on the row it had.
    pub(crate) fn picked_elsewhere(&mut self, message: &Message) {
        let Some(picker) = &self.picker else {
            return;
        };
        if matches!(message, Message::Picker(_)) {
            return;
        }
        let kind = std::mem::discriminant(message);
        let said = std::cell::OnceCell::new();
        if picker.picks.iter().any(|(_, send)| {
            std::mem::discriminant(send) == kind
                && format!("{send:?}") == *said.get_or_init(|| format!("{message:?}"))
        }) {
            self.picker = None;
        }
    }

    /// A key while a drop-down is open: its rows'. `None` when it is not
    /// its — the drop-down put away under it (its dialog gone), Tab (which
    /// puts it away and moves the ring on), a shortcut.
    pub(crate) fn picker_key(
        &mut self,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Option<Task<Message>> {
        use keyboard::Key;
        use keyboard::key::Named;
        let stands = self.picker_stands(self.picker.as_ref()?);
        if !stands {
            self.picker = None;
            return None;
        }
        let picker = self.picker.as_mut()?;
        let n = picker.picks.len();
        let chord = modifiers.command() || modifiers.control() || modifiers.alt();
        if chord {
            self.picker = None;
            return None;
        }
        let at = picker.at;
        let to = match key.as_ref() {
            Key::Named(Named::ArrowDown) => (at + 1).min(n - 1),
            Key::Named(Named::ArrowUp) => at.saturating_sub(1),
            Key::Named(Named::Home) => 0,
            Key::Named(Named::End) => n - 1,
            Key::Named(Named::PageDown) => (at + PAGE).min(n - 1),
            Key::Named(Named::PageUp) => at.saturating_sub(PAGE),
            Key::Named(Named::Enter | Named::Space) | Key::Character(" ") => {
                return Some(self.update_picker(PickerMsg::Pick(at)));
            }
            Key::Named(Named::Escape) => {
                self.picker = None;
                return Some(Task::none());
            }
            Key::Named(Named::Tab) => {
                self.picker = None;
                return None;
            }
            // A letter: the next row beginning with it, round the end.
            Key::Character(typed) => {
                let typed = typed.to_lowercase();
                (1..=n)
                    .map(|step| (at + step) % n)
                    .find(|&i| picker.picks[i].0.to_lowercase().starts_with(&typed))
                    .unwrap_or(at)
            }
            // Anything else is held: it acted behind the list.
            _ => return Some(Task::none()),
        };
        picker.at = to;
        Some(keep_in_view(to, n))
    }
}

/// How many rows Page Up and Page Down go.
const PAGE: usize = 8;

/// The open list scrolled so its row `at` of `n` is in view: as far down it
/// as the row is down the rows, which always shows it.
fn keep_in_view(at: usize, n: usize) -> Task<Message> {
    let y = if n > 1 {
        at as f32 / (n - 1) as f32
    } else {
        0.0
    };
    iced::widget::operation::snap_to(
        crate::ui::picker::list_id(),
        iced::widget::scrollable::RelativeOffset { x: 0.0, y },
    )
}
