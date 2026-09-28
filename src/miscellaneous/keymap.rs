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
//!
//! This table is also the single source of the menu bar's command items. Every
//! [`Action`] declares where it sits in the menu ([`Action::menu`]);
//! `crate::macos::menu` builds those items from the LIVE keymap. So a rebind
//! moves the menu's key equivalent with it (the old chord stops firing), and
//! adding an action here is all it takes for it to appear in the menu. The
//! menu's own commands that are not actions (Quit, Close Window, …) own their
//! chords too; [`MENU_RESERVED`] lists them so [`Keymap::conflict`] refuses a
//! binding AppKit would swallow first.
//!
//! What an action DOES has one home: `App::run_command_action`, which can look
//! at the app's state first (Explain All cancels a running pass; ⌘C in the
//! finder copies its text). The key handler calls it for a chord it sees
//! (`EditorMsg::KeyPressed`), and the menu sends
//! `Message::Editor(EditorMsg::RunAction(action))` — handled by that same
//! function — for a click on the action's item and for a chord AppKit matched
//! as the item's key equivalent. So a click, a menu-owned chord and a chord
//! the key handler sees can never disagree, and no action has to keep its
//! chord away from the menu.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use iced::keyboard::{Key, Modifiers, key::Named};

/// A rebindable command action. The `id` is the stable config key; the order in
/// [`Action::ALL`] is the display order in the shortcuts panel and, within each
/// [`MenuSection`], in the menu bar.
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
    ToggleAsk,
    StartDebug,
    CallGraph,
    ImportGraph,
    ExplainAll,
    ToggleDiff,
    TimeTravel,
    Walkthrough,
    LspServers,
    Shortcuts,
}

impl Action {
    pub const ALL: [Action; 23] = [
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
        Action::ToggleAsk,
        Action::StartDebug,
        Action::CallGraph,
        Action::ImportGraph,
        Action::ExplainAll,
        Action::ToggleDiff,
        Action::TimeTravel,
        Action::Walkthrough,
        Action::LspServers,
        Action::Shortcuts,
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
            Action::ToggleAsk => "toggle_ask",
            Action::StartDebug => "start_debug",
            Action::CallGraph => "call_graph",
            Action::ImportGraph => "import_graph",
            Action::ExplainAll => "explain_all",
            Action::ToggleDiff => "toggle_diff",
            Action::TimeTravel => "time_travel",
            Action::Walkthrough => "walkthrough",
            Action::LspServers => "lsp_servers",
            Action::Shortcuts => "shortcuts",
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
            Action::ToggleAsk => "Ask",
            Action::StartDebug => "Start debugging",
            Action::CallGraph => "Project call graph",
            Action::ImportGraph => "Project import graph",
            Action::ExplainAll => "Explain All",
            Action::ToggleDiff => "Diff vs HEAD",
            Action::TimeTravel => "Time Travel",
            Action::Walkthrough => "Walkthrough",
            Action::LspServers => "Language servers",
            Action::Shortcuts => "Keyboard shortcuts",
        }
    }

    /// Where the action appears in the menu bar, and under which title.
    ///
    /// Exhaustive on purpose: a new action cannot be added without deciding
    /// where the menu shows it, and once decided the menu picks it up with no
    /// edit to `crate::macos::menu`.
    pub fn menu(self) -> MenuPlacement {
        let at = |section, title| MenuPlacement { section, title };
        match self {
            Action::CopySelection => at(MenuSection::Edit, "Copy"),
            Action::FindInFile => at(MenuSection::Edit, "Find…"),
            Action::ProjectSearch => at(MenuSection::Edit, "Find in Files…"),
            Action::ToggleSplit => at(MenuSection::View, "Split Editor"),
            Action::ZoomIn => at(MenuSection::View, "Zoom In"),
            Action::ZoomOut => at(MenuSection::View, "Zoom Out"),
            Action::ZoomReset => at(MenuSection::View, "Actual Size"),
            Action::GoBack => at(MenuSection::Go, "Back"),
            Action::GoForward => at(MenuSection::Go, "Forward"),
            Action::OpenFile => at(MenuSection::Go, "Go to File…"),
            Action::OpenSymbol => at(MenuSection::Go, "Go to Symbol…"),
            Action::GotoLine => at(MenuSection::Go, "Go to Line…"),
            Action::ToggleBookmark => at(MenuSection::Go, "Toggle Bookmark"),
            Action::ToggleAsk => at(MenuSection::View, "Ask"),
            Action::StartDebug => at(MenuSection::View, "Start Debugging"),
            Action::CallGraph => at(MenuSection::View, "Call Graph"),
            Action::ImportGraph => at(MenuSection::View, "Import Graph"),
            Action::ExplainAll => at(MenuSection::View, "Explain All"),
            Action::ToggleDiff => at(MenuSection::View, "Diff vs HEAD"),
            Action::TimeTravel => at(MenuSection::View, "Time Travel"),
            Action::Walkthrough => at(MenuSection::View, "Walkthrough"),
            Action::LspServers => at(MenuSection::View, "Language Servers…"),
            Action::Shortcuts => at(MenuSection::View, "Keyboard Shortcuts…"),
        }
    }

    fn default_chord(self) -> Chord {
        match self {
            Action::OpenFile => Chord::cmd('p'),
            Action::OpenSymbol => Chord::cmd('t'),
            Action::ProjectSearch => Chord::cmd_shift('f'),
            Action::FindInFile => Chord::cmd('f'),
            Action::CopySelection => Chord::cmd('c'),
            Action::ToggleBookmark => Chord::cmd('d'),
            Action::GotoLine => Chord::cmd('l'),
            Action::ToggleSplit => Chord::cmd('\\'),
            Action::ZoomIn => Chord::cmd('='),
            Action::ZoomOut => Chord::cmd('-'),
            Action::ZoomReset => Chord::cmd('0'),
            Action::GoBack => Chord::alt_key(KeyRef::Left),
            Action::GoForward => Chord::alt_key(KeyRef::Right),
            // The feature panels share ⌘⇧ + a mnemonic letter; none collides
            // with a chord above or with one the menu bar owns (see
            // `MENU_RESERVED`).
            Action::ToggleAsk => Chord::cmd_shift('a'),
            Action::StartDebug => Chord::cmd_shift('d'),
            Action::CallGraph => Chord::cmd_shift('c'),
            Action::ImportGraph => Chord::cmd_shift('i'),
            Action::ExplainAll => Chord::cmd_shift('e'),
            Action::ToggleDiff => Chord::cmd_shift('g'),
            Action::TimeTravel => Chord::cmd_shift('h'),
            Action::Walkthrough => Chord::cmd_shift('j'),
            Action::LspServers => Chord::cmd_shift('l'),
            Action::Shortcuts => Chord::cmd('/'),
        }
    }
}

/// A top-level menu that holds keymap actions, in menu-bar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuSection {
    Edit,
    View,
    Go,
}

impl MenuSection {
    pub const ALL: [MenuSection; 3] = [MenuSection::Edit, MenuSection::View, MenuSection::Go];

    pub fn title(self) -> &'static str {
        match self {
            MenuSection::Edit => "Edit",
            MenuSection::View => "View",
            MenuSection::Go => "Go",
        }
    }
}

/// Where an action's menu item lives: which menu, and the item's title there
/// (menu titles follow macOS conventions, so they differ from [`Action::label`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuPlacement {
    pub section: MenuSection,
    pub title: &'static str,
}

/// Chords the menu bar owns for its own commands, which are not keymap actions.
///
/// AppKit matches a menu item's key equivalent before the key event ever
/// reaches clew, so an action bound to one of these could never fire; the
/// rebind UI refuses them through [`Keymap::conflict`]. `crate::macos::menu`
/// takes these items' key equivalents from this same list, so the two cannot
/// drift apart.
pub const MENU_RESERVED: [(Chord, &str); 6] = [
    (Chord::cmd('q'), "Quit clew"),
    (Chord::cmd('w'), "Close Window"),
    (Chord::cmd('n'), "New Window"),
    (Chord::cmd('o'), "Open Folder…"),
    (Chord::cmd(','), "Settings…"),
    (Chord::cmd('h'), "Hide clew"),
];

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

    /// The string AppKit expects as an `NSMenuItem` key equivalent: the
    /// unshifted character, or the private-use function-key code point for an
    /// arrow (`NSUpArrowFunctionKey` = U+F700 … `NSRightArrowFunctionKey` =
    /// U+F703).
    pub fn menu_key(self) -> String {
        match self {
            KeyRef::Char(c) => c.to_string(),
            KeyRef::Up => '\u{F700}'.to_string(),
            KeyRef::Down => '\u{F701}'.to_string(),
            KeyRef::Left => '\u{F702}'.to_string(),
            KeyRef::Right => '\u{F703}'.to_string(),
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

/// ⌘-chords a focused text field consumes for editing. AppKit must never own
/// one as a menu key equivalent: it would take the chord before the field saw
/// it, so ⌘C in the search box would copy the code selection instead.
const TEXT_EDITING: [Chord; 6] = [
    Chord::cmd('c'),
    Chord::cmd('x'),
    Chord::cmd('v'),
    Chord::cmd('a'),
    Chord::cmd('z'),
    Chord::cmd_shift('z'),
];

impl Chord {
    /// ⌘ + `c`.
    pub const fn cmd(c: char) -> Chord {
        Chord {
            cmd: true,
            ctrl: false,
            alt: false,
            shift: false,
            key: KeyRef::Char(c),
        }
    }

    /// ⌘⇧ + `c`.
    pub const fn cmd_shift(c: char) -> Chord {
        Chord {
            cmd: true,
            ctrl: false,
            alt: false,
            shift: true,
            key: KeyRef::Char(c),
        }
    }

    /// ⌥ + `key`.
    pub const fn alt_key(key: KeyRef) -> Chord {
        Chord {
            cmd: false,
            ctrl: false,
            alt: true,
            shift: false,
            key,
        }
    }

    /// Build a chord from a live key event, or `None` for keys clew cannot bind
    /// (only letters/symbols and the four arrows are supported).
    pub fn from_event(key: &Key, mods: Modifiers) -> Option<Chord> {
        let mut shift = mods.shift();
        let key = match key.as_ref() {
            Key::Character(c) => {
                let ch = c.chars().next()?.to_ascii_lowercase();
                // `+` and `=` share a physical key; fold so ⌘+ and ⌘= are one.
                // On a US layout `+` IS Shift+=, and the event carries that
                // Shift: folded with it, the press became ⌘⇧= and matched
                // nothing. The Shift was spent producing `+`.
                if ch == '+' {
                    shift = false;
                    KeyRef::Char('=')
                } else {
                    KeyRef::Char(ch)
                }
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
            shift,
            key,
        })
    }

    /// Whether the chord carries a command-style modifier (⌘/⌥/⌃). A bare or
    /// shift-only chord is not a valid command binding (it would collide with
    /// the reading motions and text input), so rebinding rejects it.
    pub fn is_command(&self) -> bool {
        self.cmd || self.ctrl || self.alt
    }

    /// Whether the menu bar may own this chord as a key equivalent.
    ///
    /// Only ⌘-chords, and not the ones text fields need: the text-editing
    /// chords above, and ⌘ + an arrow (line start/end, document start/end in
    /// a field). ⌥- and ⌃-chords are word motion and the Emacs-style bindings
    /// of every macOS text field. A chord that is not menu-safe still works as
    /// a binding — the in-app key handler dispatches it whenever no text field
    /// consumed the key — and its menu item stays click-only.
    pub fn menu_safe(&self) -> bool {
        self.cmd && matches!(self.key, KeyRef::Char(_)) && !TEXT_EDITING.contains(self)
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

/// What a chord is already taken by, when a rebind would collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// Another rebindable action.
    Action(Action),
    /// One of the menu bar's own commands (see [`MENU_RESERVED`]).
    Menu(&'static str),
}

impl Conflict {
    /// What to name in the "Already used by …" notice.
    pub fn label(self) -> &'static str {
        match self {
            Conflict::Action(a) => a.label(),
            Conflict::Menu(title) => title,
        }
    }
}

/// Hands every keymap value a distinct stamp, so a window can tell that its
/// keymap changed (or was replaced) across an update by comparing two numbers.
static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);

fn next_stamp() -> u64 {
    NEXT_STAMP.fetch_add(1, Ordering::Relaxed)
}

/// The full set of command bindings (defaults with overrides applied).
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<Action, Chord>,
    /// Changes on every mutation; see [`Keymap::stamp`].
    stamp: u64,
}

impl Keymap {
    pub fn defaults() -> Keymap {
        Keymap {
            bindings: Action::ALL
                .into_iter()
                .map(|a| (a, a.default_chord()))
                .collect(),
            stamp: next_stamp(),
        }
    }

    /// Load defaults, then apply any `[keymap]` overrides from `config.toml`.
    pub fn load() -> Keymap {
        let mut km = Keymap::defaults();
        if let Some(keymap) = clew_core::globalconfig::section("keymap").as_ref() {
            km.apply_overrides(keymap);
        }
        km
    }

    /// Apply `[keymap]` entries, skipping any the rebind UI would have refused.
    ///
    /// `config.toml` is hand-editable, so an entry can bind a chord the menu
    /// owns (it would never fire, and the menu would carry the key equivalent
    /// twice) or one another action already has (only one of the two could
    /// ever run). Those entries keep the action's default and are reported,
    /// instead of silently shadowing each other.
    fn apply_overrides(&mut self, section: &toml::Table) {
        for (id, val) in section {
            let (Some(action), Some(chord)) =
                (Action::from_id(id), val.as_str().and_then(Chord::parse))
            else {
                eprintln!("[clew] keymap: ignoring unknown or malformed entry `{id}`");
                continue;
            };
            if !chord.is_command() {
                eprintln!("[clew] keymap: `{id}` needs ⌘, ⌥ or ⌃ — keeping the default");
                continue;
            }
            if let Some(other) = self.conflict(&chord, action) {
                eprintln!(
                    "[clew] keymap: `{id}` = {} is already used by “{}” — keeping the default",
                    chord.caps(),
                    other.label()
                );
                continue;
            }
            self.bindings.insert(action, chord);
        }
        self.stamp = next_stamp();
    }

    pub fn chord(&self, action: Action) -> Chord {
        self.bindings[&action]
    }

    /// Identifies this keymap's current content: it changes on every rebind or
    /// reset, and two independently loaded keymaps never share one. The shell
    /// compares it across a window's update to learn that the bindings changed
    /// (then rebuilds the menu and hands the new bindings to the other
    /// windows) without comparing the maps themselves on every message.
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    /// The action currently bound to `chord`, if any.
    pub fn action_for(&self, chord: &Chord) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(_, c)| *c == chord)
            .map(|(a, _)| *a)
    }

    /// What `chord` is already used by — another action, or a command the menu
    /// bar owns — if binding it to `except` would collide.
    pub fn conflict(&self, chord: &Chord, except: Action) -> Option<Conflict> {
        if let Some((_, title)) = MENU_RESERVED.iter().find(|(c, _)| c == chord) {
            return Some(Conflict::Menu(title));
        }
        self.bindings
            .iter()
            .find(|(a, c)| **a != except && *c == chord)
            .map(|(a, _)| Conflict::Action(*a))
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
        self.stamp = next_stamp();
    }

    pub fn reset(&mut self, action: Action) {
        self.bindings.insert(action, action.default_chord());
        self.stamp = next_stamp();
    }

    pub fn reset_all(&mut self) {
        // `defaults()` draws a fresh stamp, so this counts as a change too.
        *self = Keymap::defaults();
    }

    /// Persist overrides to the global `config.toml`, preserving everything
    /// else in it (see `clew_core::globalconfig`).
    pub fn save(&self) -> Result<(), String> {
        let mut section = toml::Table::new();
        for action in Action::ALL {
            if self.is_overridden(action) {
                section.insert(action.id().into(), self.chord(action).to_config().into());
            }
        }
        // No overrides left: drop the section rather than leaving an empty one.
        clew_core::globalconfig::update_opt("keymap", (!section.is_empty()).then_some(section))
    }
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

    /// `+` and `=` are one physical key, and the live event is what is folded:
    /// a ⌘+ press and a ⌘= press must both reach the default zoom-in binding.
    /// On a US layout `+` is Shift+=, so the real ⌘+ event carries SHIFT as
    /// well — the case the fold used to miss (it produced ⌘⇧=, bound to
    /// nothing). A layout with a `+` key of its own sends it unshifted.
    #[test]
    fn plus_folds_to_equals() {
        let cmd = Modifiers::COMMAND;
        let equals = Chord::from_event(&Key::Character("=".into()), cmd).unwrap();
        for mods in [Modifiers::COMMAND | Modifiers::SHIFT, cmd] {
            let plus = Chord::from_event(&Key::Character("+".into()), mods).unwrap();
            assert_eq!(plus.key, KeyRef::Char('='));
            assert_eq!(plus, equals, "{mods:?}");
        }
        let km = Keymap::defaults();
        assert_eq!(km.action_for(&equals), Some(Action::ZoomIn));
        // Only the Shift that made the `+` is spent: other chords keep theirs.
        let find_all = Chord::from_event(
            &Key::Character("f".into()),
            Modifiers::COMMAND | Modifiers::SHIFT,
        )
        .unwrap();
        assert_eq!(km.action_for(&find_all), Some(Action::ProjectSearch));
        // Config spells it with `=` (a `+` cannot be written inside the
        // `+`-separated syntax at all), and that parses to the same chord.
        assert_eq!(Chord::parse("cmd+="), Some(equals));
    }

    #[test]
    fn defaults_have_no_conflicts() {
        let km = Keymap::defaults();
        for action in Action::ALL {
            assert_eq!(
                km.conflict(&km.chord(action), action),
                None,
                "{}",
                action.id()
            );
        }
    }

    /// `save` writes exactly the bindings that differ from the defaults — a
    /// default keymap writes no `[keymap]` section at all, and resetting the
    /// last override removes it again — and leaves the rest of `config.toml`
    /// as it was. What it writes loads back as the same bindings.
    #[test]
    fn only_overrides_are_saved() {
        let data = crate::app::tests::test_dir("keymap-save");
        std::fs::create_dir_all(&data).unwrap();
        let _env = crate::app::tests::data_dir_override(&data);
        let config = data.join("config.toml");
        std::fs::write(&config, "[llm]\nmodel = \"kept\"\n").unwrap();
        let read = || -> toml::Table {
            toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap()
        };

        let mut km = Keymap::defaults();
        km.save().unwrap();
        let saved = read();
        assert!(!saved.contains_key("keymap"), "defaults write no section");

        km.rebind(Action::GotoLine, Chord::cmd('g'));
        km.save().unwrap();
        let saved = read();
        let section = saved["keymap"].as_table().unwrap();
        assert_eq!(section.len(), 1, "only the override: {section:?}");
        assert_eq!(section["goto_line"].as_str(), Some("cmd+g"));
        assert_eq!(saved["llm"]["model"].as_str(), Some("kept"));
        let loaded = Keymap::load();
        assert_eq!(loaded.chord(Action::GotoLine), Chord::cmd('g'));
        assert!(
            Action::ALL
                .into_iter()
                .filter(|a| *a != Action::GotoLine)
                .all(|a| !loaded.is_overridden(a))
        );

        km.reset(Action::GotoLine);
        km.save().unwrap();
        let saved = read();
        assert!(
            !saved.contains_key("keymap"),
            "the last reset drops the section"
        );
        assert_eq!(saved["llm"]["model"].as_str(), Some("kept"));
    }

    /// The menu bar's own commands take their chords before clew sees the key
    /// event, so an action bound to one would never run. The rebind UI names
    /// the owner instead of accepting it.
    #[test]
    fn a_chord_the_menu_owns_is_a_conflict() {
        let km = Keymap::defaults();
        for (chord, title) in MENU_RESERVED {
            assert_eq!(
                km.conflict(&chord, Action::GotoLine),
                Some(Conflict::Menu(title)),
                "{}",
                chord.caps()
            );
        }
        assert_eq!(
            km.conflict(&Chord::cmd('q'), Action::GotoLine)
                .map(Conflict::label),
            Some("Quit clew")
        );
        // An action-owned chord still names the action.
        assert_eq!(
            km.conflict(&Chord::cmd('p'), Action::GotoLine),
            Some(Conflict::Action(Action::OpenFile))
        );
        // And rebinding an action to its own chord is not a conflict.
        assert_eq!(km.conflict(&Chord::cmd('p'), Action::OpenFile), None);
    }

    /// A hand-edited config can name a chord the rebind UI would refuse. It
    /// must not shadow the menu or another action: the entry is ignored and
    /// the action keeps its default.
    #[test]
    fn config_overrides_that_collide_are_ignored() {
        let mut km = Keymap::defaults();
        let table: toml::Table = toml::from_str(
            "goto_line = \"cmd+q\"\nopen_symbol = \"cmd+p\"\nzoom_in = \"j\"\n\
             toggle_split = \"cmd+shift+s\"\nnot_an_action = \"cmd+k\"\n",
        )
        .unwrap();
        km.apply_overrides(&table);
        assert_eq!(km.chord(Action::GotoLine), Action::GotoLine.default_chord());
        assert_eq!(
            km.chord(Action::OpenSymbol),
            Action::OpenSymbol.default_chord()
        );
        assert_eq!(km.chord(Action::ZoomIn), Action::ZoomIn.default_chord());
        // A clean override still applies.
        assert_eq!(km.chord(Action::ToggleSplit), Chord::cmd_shift('s'));
    }

    /// Only ⌘-chords outside the text-editing set may become key equivalents.
    #[test]
    fn menu_safety_keeps_text_editing_chords_with_text_fields() {
        assert!(Chord::cmd('p').menu_safe());
        assert!(Chord::cmd_shift('f').menu_safe());
        for editing in TEXT_EDITING {
            assert!(!editing.menu_safe(), "{}", editing.caps());
        }
        assert!(
            !Chord::alt_key(KeyRef::Left).menu_safe(),
            "⌥← is word motion"
        );
        let cmd_left = Chord {
            cmd: true,
            ..Chord::alt_key(KeyRef::Left)
        };
        assert!(!cmd_left.menu_safe(), "⌘← is line start in a text field");
        let ctrl_p = Chord {
            cmd: false,
            ctrl: true,
            ..Chord::cmd('p')
        };
        assert!(!ctrl_p.menu_safe(), "⌃P is a text-field motion");
    }

    /// Every mutation, and every fresh keymap, is visible as a stamp change —
    /// that is how the shell learns it has to rebuild the menu.
    #[test]
    fn stamps_change_with_every_mutation() {
        let mut km = Keymap::defaults();
        let first = km.stamp();
        km.rebind(Action::GotoLine, Chord::cmd('g'));
        let rebound = km.stamp();
        assert_ne!(first, rebound);
        km.reset(Action::GotoLine);
        assert_ne!(rebound, km.stamp());
        let reset = km.stamp();
        km.reset_all();
        assert_ne!(reset, km.stamp());
        assert_ne!(Keymap::defaults().stamp(), Keymap::defaults().stamp());
        // A clone is the same content, so it keeps the stamp.
        assert_eq!(km.clone().stamp(), km.stamp());
    }

    /// Ids are config keys and menu titles are what users see: both unique.
    #[test]
    fn ids_and_menu_titles_are_unique() {
        let mut ids: Vec<_> = Action::ALL.iter().map(|a| a.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Action::ALL.len());
        let mut titles: Vec<_> = Action::ALL.iter().map(|a| a.menu().title).collect();
        titles.sort_unstable();
        titles.dedup();
        assert_eq!(titles.len(), Action::ALL.len());
        for action in Action::ALL {
            assert_eq!(Action::from_id(action.id()), Some(action));
            assert!(!action.menu().title.is_empty());
        }
    }
}
