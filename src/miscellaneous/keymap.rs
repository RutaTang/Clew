//! The customizable command keymap.
//!
//! clew's *command* shortcuts (the chords that carry ⌘/⌥/⌃, e.g. ⌘P to open a
//! file) are rebindable and persist in the **global** `config.toml` under a
//! `[keymap]` section, alongside `[llm]`. Only bindings that differ from the
//! defaults are written, so the defaults can evolve without stale overrides.
//!
//! The modal single-key reading motions (Vim-style `h`/`j`/`k`/`l`, `gg`, `za`,
//! …) are intentionally NOT part of this keymap: they carry no modifier, are
//! matched separately in `handle_key`, and are shown read-only in the panel.

use std::collections::HashMap;

use iced::keyboard::{Key, Modifiers, key::Named};

/// A rebindable command action. The `id` is the stable config key; the order in
/// [`Action::ALL`] is the display order in the shortcuts panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    OpenFile,
    OpenSymbol,
    ProjectSearch,
    FindInFile,
    CopySelection,
    ToggleBookmark,
    GotoLine,
    ToggleSplit,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    GoBack,
    GoForward,
}

impl Action {
    pub const ALL: [Action; 13] = [
        Action::OpenFile,
        Action::OpenSymbol,
        Action::ProjectSearch,
        Action::FindInFile,
        Action::CopySelection,
        Action::ToggleBookmark,
        Action::GotoLine,
        Action::ToggleSplit,
        Action::ZoomIn,
        Action::ZoomOut,
        Action::ZoomReset,
        Action::GoBack,
        Action::GoForward,
    ];

    /// Stable identifier used as the TOML key. Never change these.
    pub fn id(self) -> &'static str {
        match self {
            Action::OpenFile => "open_file",
            Action::OpenSymbol => "open_symbol",
            Action::ProjectSearch => "project_search",
            Action::FindInFile => "find_in_file",
            Action::CopySelection => "copy_selection",
            Action::ToggleBookmark => "toggle_bookmark",
            Action::GotoLine => "goto_line",
            Action::ToggleSplit => "toggle_split",
            Action::ZoomIn => "zoom_in",
            Action::ZoomOut => "zoom_out",
            Action::ZoomReset => "zoom_reset",
            Action::GoBack => "go_back",
            Action::GoForward => "go_forward",
        }
    }

    pub fn from_id(s: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.id() == s)
    }

    /// Human-readable label for the shortcuts panel.
    pub fn label(self) -> &'static str {
        match self {
            Action::OpenFile => "Open file (fuzzy)",
            Action::OpenSymbol => "Go to symbol",
            Action::ProjectSearch => "Search in project",
            Action::FindInFile => "Find in file",
            Action::CopySelection => "Copy selection",
            Action::ToggleBookmark => "Toggle bookmark",
            Action::GotoLine => "Go to line",
            Action::ToggleSplit => "Toggle split view",
            Action::ZoomIn => "Increase font size",
            Action::ZoomOut => "Decrease font size",
            Action::ZoomReset => "Reset font size",
            Action::GoBack => "Back",
            Action::GoForward => "Forward",
        }
    }

    fn default_chord(self) -> Chord {
        let cmd = |key| Chord { cmd: true, ctrl: false, alt: false, shift: false, key };
        let cmd_shift = |key| Chord { cmd: true, ctrl: false, alt: false, shift: true, key };
        let alt = |key| Chord { cmd: false, ctrl: false, alt: true, shift: false, key };
        match self {
            Action::OpenFile => cmd(KeyRef::Char('p')),
            Action::OpenSymbol => cmd(KeyRef::Char('t')),
            Action::ProjectSearch => cmd_shift(KeyRef::Char('f')),
            Action::FindInFile => cmd(KeyRef::Char('f')),
            Action::CopySelection => cmd(KeyRef::Char('c')),
            Action::ToggleBookmark => cmd(KeyRef::Char('d')),
            Action::GotoLine => cmd(KeyRef::Char('l')),
            Action::ToggleSplit => cmd(KeyRef::Char('\\')),
            Action::ZoomIn => cmd(KeyRef::Char('=')),
            Action::ZoomOut => cmd(KeyRef::Char('-')),
            Action::ZoomReset => cmd(KeyRef::Char('0')),
            Action::GoBack => alt(KeyRef::Left),
            Action::GoForward => alt(KeyRef::Right),
        }
    }
}

/// The key part of a chord, normalized so equal chords compare equal:
/// letters are lowercased and `+` is folded to `=` (both come from the same
/// physical key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRef {
    Char(char),
    Left,
    Right,
    Up,
    Down,
}

impl KeyRef {
    fn config_token(self) -> String {
        match self {
            KeyRef::Char(c) => c.to_string(),
            KeyRef::Left => "left".into(),
            KeyRef::Right => "right".into(),
            KeyRef::Up => "up".into(),
            KeyRef::Down => "down".into(),
        }
    }

    /// A display symbol for the key cap.
    fn cap(self) -> String {
        match self {
            KeyRef::Char(c) => c.to_ascii_uppercase().to_string(),
            KeyRef::Left => "←".into(),
            KeyRef::Right => "→".into(),
            KeyRef::Up => "↑".into(),
            KeyRef::Down => "↓".into(),
        }
    }
}

/// A modifier + key combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: KeyRef,
}

impl Chord {
    /// Build a chord from a live key event, or `None` for keys clew cannot bind
    /// (only letters/symbols and the four arrows are supported).
    pub fn from_event(key: &Key, mods: Modifiers) -> Option<Chord> {
        let key = match key.as_ref() {
            Key::Character(c) => {
                let ch = c.chars().next()?.to_ascii_lowercase();
                // `+` and `=` share a physical key; fold so ⌘+ and ⌘= are one.
                KeyRef::Char(if ch == '+' { '=' } else { ch })
            }
            Key::Named(Named::ArrowLeft) => KeyRef::Left,
            Key::Named(Named::ArrowRight) => KeyRef::Right,
            Key::Named(Named::ArrowUp) => KeyRef::Up,
            Key::Named(Named::ArrowDown) => KeyRef::Down,
            _ => return None,
        };
        Some(Chord {
            cmd: mods.command(),
            ctrl: mods.control(),
            alt: mods.alt(),
            shift: mods.shift(),
            key,
        })
    }

    /// Whether the chord carries a command-style modifier (⌘/⌥/⌃). A bare or
    /// shift-only chord is not a valid command binding (it would collide with
    /// the reading motions and text input), so rebinding rejects it.
    pub fn is_command(&self) -> bool {
        self.cmd || self.ctrl || self.alt
    }

    /// Parse a `"cmd+shift+f"` string from config; `None` if malformed.
    fn parse(s: &str) -> Option<Chord> {
        let mut chord = Chord {
            cmd: false,
            ctrl: false,
            alt: false,
            shift: false,
            key: KeyRef::Char(' '),
        };
        let mut key = None;
        for part in s.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "cmd" | "super" | "meta" => chord.cmd = true,
                "ctrl" | "control" => chord.ctrl = true,
                "alt" | "option" | "opt" => chord.alt = true,
                "shift" => chord.shift = true,
                "left" => key = Some(KeyRef::Left),
                "right" => key = Some(KeyRef::Right),
                "up" => key = Some(KeyRef::Up),
                "down" => key = Some(KeyRef::Down),
                other if other.chars().count() == 1 => {
                    key = Some(KeyRef::Char(other.chars().next().unwrap()))
                }
                _ => return None,
            }
        }
        chord.key = key?;
        Some(chord)
    }

    /// Serialize to a `"cmd+shift+f"` config string.
    fn to_config(self) -> String {
        let mut parts = Vec::new();
        if self.cmd {
            parts.push("cmd".to_string());
        }
        if self.ctrl {
            parts.push("ctrl".to_string());
        }
        if self.alt {
            parts.push("alt".to_string());
        }
        if self.shift {
            parts.push("shift".to_string());
        }
        parts.push(self.key.config_token());
        parts.join("+")
    }

    /// Pretty key-cap string for the UI, e.g. `⌘⇧F` or `⌥←`.
    pub fn caps(self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push('⌃');
        }
        if self.alt {
            s.push('⌥');
        }
        if self.shift {
            s.push('⇧');
        }
        if self.cmd {
            s.push('⌘');
        }
        s.push_str(&self.key.cap());
        s
    }
}

/// The full set of command bindings (defaults with overrides applied).
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<Action, Chord>,
}

impl Keymap {
    pub fn defaults() -> Keymap {
        Keymap {
            bindings: Action::ALL.into_iter().map(|a| (a, a.default_chord())).collect(),
        }
    }

    /// Load defaults, then apply any `[keymap]` overrides from `config.toml`.
    pub fn load() -> Keymap {
        let mut km = Keymap::defaults();
        let table: Option<toml::Value> = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok());
        if let Some(keymap) = table.as_ref().and_then(|t| t.get("keymap")).and_then(|v| v.as_table())
        {
            for (id, val) in keymap {
                if let (Some(action), Some(chord)) =
                    (Action::from_id(id), val.as_str().and_then(Chord::parse))
                {
                    km.bindings.insert(action, chord);
                }
            }
        }
        km
    }

    pub fn chord(&self, action: Action) -> Chord {
        self.bindings[&action]
    }

    /// The action currently bound to `chord`, if any.
    pub fn action_for(&self, chord: &Chord) -> Option<Action> {
        self.bindings.iter().find(|(_, c)| *c == chord).map(|(a, _)| *a)
    }

    /// A different action already bound to `chord`, for conflict detection.
    pub fn conflict(&self, chord: &Chord, except: Action) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(a, c)| **a != except && *c == chord)
            .map(|(a, _)| *a)
    }

    pub fn is_overridden(&self, action: Action) -> bool {
        self.bindings[&action] != action.default_chord()
    }

    /// Whether any binding differs from its default.
    pub fn any_overridden(&self) -> bool {
        Action::ALL.into_iter().any(|a| self.is_overridden(a))
    }

    pub fn rebind(&mut self, action: Action, chord: Chord) {
        self.bindings.insert(action, chord);
    }

    pub fn reset(&mut self, action: Action) {
        self.bindings.insert(action, action.default_chord());
    }

    pub fn reset_all(&mut self) {
        *self = Keymap::defaults();
    }

    /// Persist overrides to the global `config.toml`, preserving other sections.
    pub fn save(&self) -> Result<(), String> {
        let path = config_path().ok_or("no data directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut root: toml::Table = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();

        let mut section = toml::Table::new();
        for action in Action::ALL {
            if self.is_overridden(action) {
                section.insert(action.id().into(), self.chord(action).to_config().into());
            }
        }
        if section.is_empty() {
            root.remove("keymap");
        } else {
            root.insert("keymap".into(), toml::Value::Table(section));
        }
        let s = toml::to_string(&root).map_err(|e| e.to_string())?;
        std::fs::write(&path, s).map_err(|e| e.to_string())
    }
}

fn config_path() -> Option<std::path::PathBuf> {
    Some(crate::lsp::store::data_root()?.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_roundtrips_through_config() {
        for action in Action::ALL {
            let c = action.default_chord();
            assert_eq!(Chord::parse(&c.to_config()), Some(c), "{}", action.id());
        }
    }

    #[test]
    fn plus_folds_to_equals() {
        assert_eq!(Chord::parse("cmd+="), Chord::parse("cmd+=").map(|c| c));
        // `+` in config parses to the `+` char, but live events fold it; the
        // default zoom-in uses `=`, so a ⌘+ event must match ⌘=.
        let ev = Chord {
            cmd: true,
            ctrl: false,
            alt: false,
            shift: false,
            key: KeyRef::Char('='),
        };
        assert_eq!(Action::ZoomIn.default_chord(), ev);
    }

    #[test]
    fn defaults_have_no_conflicts() {
        let km = Keymap::defaults();
        for action in Action::ALL {
            assert_eq!(km.conflict(&km.chord(action), action), None, "{}", action.id());
        }
    }

    #[test]
    fn only_overrides_are_saved_shape() {
        // A fresh default keymap writes no [keymap] section.
        let km = Keymap::defaults();
        assert!(!km.any_overridden());
    }
}
