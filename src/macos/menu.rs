//! The native macOS menu bar (clew / File / Edit / View / Go).
//!
//! clew runs frameless, but the app menu bar lives at the top of the screen,
//! independent of window chrome. Menu clicks are bridged into the iced update
//! loop exactly like server events: an item's `tag` is pushed through a channel
//! that [`subscription`] turns back into a [`Message`]. Only About and Hide are
//! standard items that use AppKit's own selectors and never touch the bridge.
//! Close Window and Quit are bridged on purpose: a frameless window ignores
//! `performClose:`, and `terminate:` exits the process without any window ever
//! being asked to close, which skips the shell's teardown entirely — unless the
//! application's delegate holds it back, which clew's now does only to hand it
//! to the shell from inside AppKit's own wait (`crate::macos::terminate`).
//!
//! The command items are generated from the keymap ([`crate::keymap`]): every
//! keymap action has a menu placement, and its item carries the action's
//! CURRENT chord as its key equivalent. The shell rebuilds the menu whenever a
//! window's keymap changes, so a rebound chord moves with its action and the
//! old one stops firing — the menu can no longer hold a hard-coded ⌘L after the
//! user moved Go to Line to ⌘G. Chords a focused text field needs (⌘C, ⌥←, …)
//! are never handed to AppKit (see `Chord::menu_safe`): those items stay
//! click-only and the in-app key handler dispatches the chord whenever no text
//! field consumed it. The menu's own commands (Quit, Close Window, …) take their
//! chords from `keymap::MENU_RESERVED`, the same list the rebind UI refuses.
//!
//! A click on an action's item — or its chord, when AppKit matched it as the
//! item's key equivalent — turns into a message in ONE place,
//! [`action_message`]: `Message::Editor(EditorMsg::RunAction(action))`, which
//! the app handles with `App::run_command_action`, the same function the key
//! handler runs for a chord it sees. There is no second, state-blind table, so
//! a click and its chord cannot disagree, and an action whose effect depends
//! on the app's state (⌘⇧E cancels a running Explain All pass) may own its
//! chord like any other.
//!
//! [`menu_model`] is the whole menu as plain data, so its structure is tested
//! without a main thread; [`install_once`] / [`rebuild`] only translate it to
//! AppKit objects.

use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{AllocAnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::NSString;

use iced::Subscription;
use iced::futures::StreamExt;
use iced::futures::channel::mpsc;

use crate::Message;
use crate::keymap::{Action, Chord, Keymap, MENU_RESERVED, MenuSection};
use crate::{ConnectMsg, EditorMsg, OverviewMsg, ProjectMsg, SettingsMsg, UpdaterMsg, WindowMsg};

// Command tags: a stable id per bridged menu item that is NOT a keymap action,
// mapped back to a Message. Keymap actions use `ACTION_TAG_BASE + index`.
const SETTINGS: isize = 1;
const OPEN_FOLDER: isize = 2;
const CONNECT: isize = 3;
const TOGGLE_SIDEBAR: isize = 7;
const TOGGLE_RIGHT: isize = 8;
const OVERVIEW: isize = 13;
const STATS: isize = 14;
const FULLSCREEN: isize = 15;
const NEW_WINDOW: isize = 21;
const CLOSE_WINDOW: isize = 22;
const THEME_SYSTEM: isize = 23;
const THEME_LIGHT: isize = 24;
const THEME_DARK: isize = 25;
const CHECK_UPDATES: isize = 26;
const QUIT: isize = 27;

/// First tag of the keymap-action items: `ACTION_TAG_BASE + i` is
/// `Action::ALL[i]`. Well clear of the fixed tags above.
const ACTION_TAG_BASE: isize = 1000;

/// What a menu click resolves to: an app message for the focused window, or a
/// shell-level window command. Keeps window management out of the App layer.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // transient, one per menu click
pub enum MenuCmd {
    App(Message),
    NewWindow,
    /// Quit the whole app — the shell's job, not one window's.
    Quit,
}

/// Map a clicked item's tag to a menu command.
fn command_for(tag: isize) -> Option<MenuCmd> {
    if tag == NEW_WINDOW {
        return Some(MenuCmd::NewWindow);
    }
    if tag == QUIT {
        return Some(MenuCmd::Quit);
    }
    if let Some(action) = action_for_tag(tag) {
        return Some(MenuCmd::App(action_message(action)));
    }
    message_for(tag).map(MenuCmd::App)
}

/// The message a click on `action`'s item sends — the menu's ONLY way of
/// turning an action into a message: the app runs it through
/// `App::run_command_action`, exactly like the chord.
fn action_message(action: Action) -> Message {
    Message::Editor(EditorMsg::RunAction(action))
}

/// The keymap action a tag names, if it is one of theirs.
fn action_for_tag(tag: isize) -> Option<Action> {
    let index = usize::try_from(tag.checked_sub(ACTION_TAG_BASE)?).ok()?;
    Action::ALL.get(index).copied()
}

fn tag_for_action(action: Action) -> isize {
    let index = Action::ALL
        .iter()
        .position(|a| *a == action)
        .expect("every action is in Action::ALL");
    ACTION_TAG_BASE + index as isize
}

/// Map a clicked non-action item's tag back to the Message the update loop
/// should run.
fn message_for(tag: isize) -> Option<Message> {
    Some(match tag {
        SETTINGS => Message::Settings(SettingsMsg::Open),
        OPEN_FOLDER => Message::Project(ProjectMsg::OpenFolderPressed),
        CONNECT => Message::Connect(ConnectMsg::Open),
        TOGGLE_SIDEBAR => Message::Window(WindowMsg::ToggleLeftSidebar),
        TOGGLE_RIGHT => Message::Window(WindowMsg::ToggleRightPanel),
        OVERVIEW => Message::Overview(OverviewMsg::Show),
        STATS => Message::Overview(OverviewMsg::ShowStats),
        THEME_SYSTEM => {
            Message::Settings(SettingsMsg::SetThemePref(crate::theme::ThemePref::System))
        }
        THEME_LIGHT => Message::Settings(SettingsMsg::SetThemePref(crate::theme::ThemePref::Light)),
        THEME_DARK => Message::Settings(SettingsMsg::SetThemePref(crate::theme::ThemePref::Dark)),
        CHECK_UPDATES => Message::Updater(UpdaterMsg::Check),
        FULLSCREEN => Message::Window(WindowMsg::ToggleFullscreen),
        CLOSE_WINDOW => Message::Window(WindowMsg::Close),
        _ => return None,
    })
}

// The channel a menu click writes its tag into; the subscription drains it.
//
// Replaced by every new subscription stream rather than set once. iced only
// runs this stream again after the subscription left the set and came back, and
// by then the previous receiver is gone: a set-once sender (what this used to
// be) kept pointing at that dead receiver, and every menu click after the
// resubscribe vanished.
static MENU_TX: Mutex<Option<mpsc::UnboundedSender<isize>>> = Mutex::new(None);
// Keep the Objective-C target alive for the whole process (menu items hold it).
static MENU_TARGET: OnceLock<Retained<MenuTarget>> = OnceLock::new();
// Install the menu bar exactly once; later changes go through `rebuild`.
static INSTALLED: std::sync::Once = std::sync::Once::new();

define_class!(
    // A tiny Objective-C object: every bridged menu item targets it, and its
    // `dispatch:` forwards the sender's tag into the channel.
    #[unsafe(super(NSObject))]
    #[name = "ClewMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(dispatch:))]
        fn dispatch(&self, sender: *mut AnyObject) {
            if sender.is_null() {
                return;
            }
            let tag: isize = unsafe { msg_send![sender, tag] };
            let tx = MENU_TX.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(tx) = tx.as_ref() {
                let _ = tx.unbounded_send(tag);
            }
        }
    }
);

/// The iced subscription that turns menu clicks into commands. Add it to the
/// app's subscription set (macOS only).
pub fn subscription() -> Subscription<MenuCmd> {
    Subscription::run(stream)
}

fn stream() -> impl iced::futures::Stream<Item = MenuCmd> {
    let (tx, rx) = mpsc::unbounded::<isize>();
    // The newest stream owns the channel (see `MENU_TX`).
    *MENU_TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    rx.filter_map(|tag| async move { command_for(tag) })
}

/// Build and install the menu bar from `keymap`. Safe to call repeatedly — it
/// runs once, and only on the main thread (no-ops elsewhere, which never
/// happens from the iced update loop). Later keymap changes go through
/// [`rebuild`].
pub fn install_once(keymap: &Keymap) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    INSTALLED.call_once(|| set_main_menu(mtm, keymap));
}

/// Replace the menu bar with one built from `keymap`, after a rebind or reset.
/// No-op before the first install and off the main thread.
pub fn rebuild(keymap: &Keymap) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if INSTALLED.is_completed() {
        set_main_menu(mtm, keymap);
    }
}

/// A key equivalent as AppKit wants it: the key string and its modifier mask.
#[derive(Debug, Clone, PartialEq)]
struct KeyEquivalent {
    key: String,
    mask: NSEventModifierFlags,
}

impl KeyEquivalent {
    fn of(chord: Chord) -> KeyEquivalent {
        let mut mask = NSEventModifierFlags::empty();
        if chord.cmd {
            mask |= NSEventModifierFlags::Command;
        }
        if chord.shift {
            mask |= NSEventModifierFlags::Shift;
        }
        if chord.alt {
            mask |= NSEventModifierFlags::Option;
        }
        if chord.ctrl {
            mask |= NSEventModifierFlags::Control;
        }
        KeyEquivalent {
            key: chord.key.menu_key(),
            mask,
        }
    }
}

/// One menu entry, before AppKit sees it.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    /// Routed through the bridge by tag.
    Bridged {
        title: &'static str,
        tag: isize,
        key: Option<KeyEquivalent>,
    },
    /// A standard AppKit selector (About, Hide).
    Standard {
        title: &'static str,
        action: Sel,
        key: Option<KeyEquivalent>,
    },
    Separator,
}

/// One top-level menu.
#[derive(Debug, Clone, PartialEq)]
struct MenuSpec {
    title: &'static str,
    entries: Vec<Entry>,
}

/// The key equivalent of a menu-owned command, from `MENU_RESERVED`.
fn reserved_key(title: &str) -> Option<KeyEquivalent> {
    MENU_RESERVED
        .iter()
        .find(|(_, t)| *t == title)
        .map(|(chord, _)| KeyEquivalent::of(*chord))
}

/// A bridged, non-action command whose chord (if any) is menu-owned.
fn command(title: &'static str, tag: isize) -> Entry {
    Entry::Bridged {
        title,
        tag,
        key: reserved_key(title),
    }
}

/// The "clew" menu. Split out so the one entry whose routing is load-bearing —
/// Quit — can be checked without a main thread or an NSMenu.
fn app_menu_entries() -> Vec<Entry> {
    vec![
        Entry::Standard {
            title: "About clew",
            action: sel!(orderFrontStandardAboutPanel:),
            key: None,
        },
        Entry::Separator,
        command("Check for Updates…", CHECK_UPDATES),
        Entry::Separator,
        command("Settings…", SETTINGS),
        Entry::Separator,
        Entry::Standard {
            title: "Hide clew",
            action: sel!(hide:),
            key: reserved_key("Hide clew"),
        },
        Entry::Separator,
        // Bridged, NOT `terminate:`. AppKit's `terminate:` calls `exit(0)` from
        // inside `-[NSApplication run]`: no window is ever told to close, so no
        // `window::Event::Closed` reaches the shell, no `on_window_closed` runs,
        // and no Rust destructor runs either — a debug adapter and the debuggee
        // it launched were left with nothing that would ever stop them. Routing
        // ⌘Q through the bridge puts the quit back in the shell's hands, where
        // the teardown lives, directly (the terminate requests macOS makes
        // itself reach it too, through `crate::macos::terminate`).
        command("Quit clew", QUIT),
    ]
}

/// The keymap actions placed in `section`, in `Action::ALL` order, each with
/// its current chord as the key equivalent when the menu may own it: a chord
/// no text field needs (`Chord::menu_safe`). Whatever the action does, a
/// menu-owned chord runs it through the same table as the key handler (see
/// [`action_message`]).
fn action_entries(keymap: &Keymap, section: MenuSection) -> Vec<Entry> {
    Action::ALL
        .into_iter()
        .filter(|a| a.menu().section == section)
        .map(|action| {
            let chord = keymap.chord(action);
            Entry::Bridged {
                title: action.menu().title,
                tag: tag_for_action(action),
                key: chord.menu_safe().then(|| KeyEquivalent::of(chord)),
            }
        })
        .collect()
}

/// The fixed items a section shows before and after its keymap actions.
fn section_frame(section: MenuSection) -> (Vec<Entry>, Vec<Entry>) {
    match section {
        MenuSection::Edit => (Vec::new(), Vec::new()),
        MenuSection::View => (
            vec![
                command("Toggle Sidebar", TOGGLE_SIDEBAR),
                command("Toggle Right Panel", TOGGLE_RIGHT),
            ],
            vec![
                command("Overview", OVERVIEW),
                command("Stats", STATS),
                Entry::Separator,
                command("Appearance: System", THEME_SYSTEM),
                command("Appearance: Light", THEME_LIGHT),
                command("Appearance: Dark", THEME_DARK),
                Entry::Separator,
                command("Toggle Full Screen", FULLSCREEN),
            ],
        ),
        MenuSection::Go => (Vec::new(), Vec::new()),
    }
}

/// The whole menu bar as data, built from the live keymap.
///
/// A chord is only ever given to ONE item: should a keymap chord and another
/// item's ever coincide (the rebind UI and config loading both refuse that,
/// so this is belt and braces), the later item stays click-only rather than
/// letting AppKit pick one of two targets for the same keystroke.
fn menu_model(keymap: &Keymap) -> Vec<MenuSpec> {
    let mut menus = vec![
        MenuSpec {
            title: "clew",
            entries: app_menu_entries(),
        },
        MenuSpec {
            title: "File",
            entries: vec![
                command("New Window", NEW_WINDOW),
                Entry::Separator,
                command("Open Folder…", OPEN_FOLDER),
                command("Connect to Remote…", CONNECT),
                Entry::Separator,
                // Bridged (not performClose:) — a frameless window ignores
                // performClose:.
                command("Close Window", CLOSE_WINDOW),
            ],
        },
    ];
    for section in MenuSection::ALL {
        let (before, after) = section_frame(section);
        let mut entries = before;
        let actions = action_entries(keymap, section);
        for block in [actions, after] {
            if block.is_empty() {
                continue;
            }
            if !entries.is_empty() {
                entries.push(Entry::Separator);
            }
            entries.extend(block);
        }
        menus.push(MenuSpec {
            title: section.title(),
            entries,
        });
    }
    let mut taken: Vec<KeyEquivalent> = Vec::new();
    for menu in &mut menus {
        for entry in &mut menu.entries {
            let key = match entry {
                Entry::Bridged { key, .. } | Entry::Standard { key, .. } => key,
                Entry::Separator => continue,
            };
            if let Some(k) = key.as_ref() {
                if taken.contains(k) {
                    *key = None;
                } else {
                    taken.push(k.clone());
                }
            }
        }
    }
    menus
}

fn set_main_menu(mtm: MainThreadMarker, keymap: &Keymap) {
    let target = MENU_TARGET.get_or_init(|| {
        let this = MenuTarget::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    });
    let main = NSMenu::new(mtm);
    for spec in menu_model(keymap) {
        main.addItem(&submenu(mtm, &spec, target));
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&main));
}

/// Build one top-level menu, wrapped in the carrier `NSMenuItem` the main menu
/// bar wants.
fn submenu(mtm: MainThreadMarker, spec: &MenuSpec, target: &MenuTarget) -> Retained<NSMenuItem> {
    let menu = NSMenu::new(mtm);
    menu.setTitle(&NSString::from_str(spec.title));
    for entry in &spec.entries {
        match entry {
            Entry::Separator => menu.addItem(&NSMenuItem::separatorItem(mtm)),
            Entry::Bridged { title, tag, key } => {
                let item = make_item(mtm, title, key.as_ref());
                unsafe {
                    item.setTag(*tag);
                    item.setTarget(Some(target));
                    item.setAction(Some(sel!(dispatch:)));
                }
                menu.addItem(&item);
            }
            Entry::Standard { title, action, key } => {
                let item = make_item(mtm, title, key.as_ref());
                unsafe { item.setAction(Some(*action)) };
                menu.addItem(&item);
            }
        }
    }
    let carrier = NSMenuItem::new(mtm);
    carrier.setSubmenu(Some(&menu));
    carrier
}

fn make_item(
    mtm: MainThreadMarker,
    title: &str,
    key: Option<&KeyEquivalent>,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    if let Some(k) = key {
        item.setKeyEquivalent(&NSString::from_str(&k.key));
        item.setKeyEquivalentModifierMask(k.mask);
    }
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry of the model with its key equivalent, flattened.
    fn keyed(menus: &[MenuSpec]) -> Vec<(&'static str, KeyEquivalent)> {
        menus
            .iter()
            .flat_map(|m| m.entries.iter())
            .filter_map(|e| match e {
                Entry::Bridged {
                    title,
                    key: Some(k),
                    ..
                }
                | Entry::Standard {
                    title,
                    key: Some(k),
                    ..
                } => Some((*title, k.clone())),
                _ => None,
            })
            .collect()
    }

    fn entry<'a>(menus: &'a [MenuSpec], title: &str) -> &'a Entry {
        menus
            .iter()
            .flat_map(|m| m.entries.iter())
            .find(|e| match e {
                Entry::Bridged { title: t, .. } | Entry::Standard { title: t, .. } => *t == title,
                Entry::Separator => false,
            })
            .unwrap_or_else(|| panic!("no menu item {title:?}"))
    }

    /// ⌘Q must reach the shell, not AppKit. `terminate:` calls `exit(0)` from
    /// inside `-[NSApplication run]`: no window is asked to close, so nothing
    /// emits `window::Event::Closed`, no `on_window_closed` runs and no Rust
    /// destructor runs — a debug adapter and the debuggee it launched were left
    /// with nothing left alive that could stop them. The entry carrying ⌘Q has
    /// to be a bridged QUIT, and QUIT has to resolve to the shell's own quit
    /// rather than being folded into a per-window `Message`.
    #[test]
    fn quit_goes_through_the_bridge_and_not_through_appkits_terminate() {
        let quit_key = KeyEquivalent::of(Chord::cmd('q'));
        let quit = app_menu_entries()
            .into_iter()
            .find(|e| match e {
                Entry::Bridged { key, .. } | Entry::Standard { key, .. } => {
                    key.as_ref() == Some(&quit_key)
                }
                Entry::Separator => false,
            })
            .expect("the app menu still offers a ⌘Q");
        match quit {
            Entry::Bridged { tag, key, .. } => {
                assert_eq!(tag, QUIT);
                assert_eq!(key.unwrap().mask, NSEventModifierFlags::Command);
            }
            _ => panic!("⌘Q reaches AppKit again, which skips the whole teardown"),
        }
        assert!(
            matches!(command_for(QUIT), Some(MenuCmd::Quit)),
            "the QUIT tag must resolve to the shell's quit"
        );
        assert!(
            message_for(QUIT).is_none(),
            "quit is not a per-window message; sending it to one window would \
             leave the others' work running"
        );
    }

    /// The finding: the menu hard-coded ⌘L for Go to Line, so after a rebind
    /// AppKit kept firing the action on the OLD chord (and swallowed the new
    /// one's key for nothing). Items now carry the live keymap's chord.
    #[test]
    fn a_rebound_action_moves_its_key_equivalent_with_it() {
        let mut km = Keymap::defaults();
        let before = menu_model(&km);
        assert_eq!(
            entry(&before, "Go to Line…"),
            &Entry::Bridged {
                title: "Go to Line…",
                tag: tag_for_action(Action::GotoLine),
                key: Some(KeyEquivalent::of(Chord::cmd('l'))),
            }
        );

        km.rebind(Action::GotoLine, Chord::cmd('g'));
        let after = menu_model(&km);
        let keys = keyed(&after);
        assert!(keys.contains(&("Go to Line…", KeyEquivalent::of(Chord::cmd('g')))));
        assert!(
            !keys
                .iter()
                .any(|(_, k)| *k == KeyEquivalent::of(Chord::cmd('l'))),
            "nothing may keep the chord the action moved away from"
        );
    }

    /// Chords a text field needs are never given to AppKit, whatever the
    /// keymap says: their items stay click-only.
    #[test]
    fn text_editing_chords_never_become_key_equivalents() {
        let mut km = Keymap::defaults();
        // Default ⌘C (copy) and ⌥←/⌥→ (back / forward).
        let model = menu_model(&km);
        for title in ["Copy", "Back", "Forward"] {
            match entry(&model, title) {
                Entry::Bridged { key, .. } => assert_eq!(key, &None, "{title}"),
                other => panic!("{title} is not bridged: {other:?}"),
            }
        }
        // A rebind onto an editing chord stays out of the menu too.
        km.rebind(Action::OpenFile, Chord::cmd('v'));
        match entry(&menu_model(&km), "Go to File…") {
            Entry::Bridged { key, .. } => assert_eq!(key, &None),
            other => panic!("not bridged: {other:?}"),
        }
    }

    /// The finding: AppKit takes a chord the menu owns before the key handler
    /// sees it, and the menu used to answer it from a second, state-blind
    /// table — so ⌘⇧E always sent "start a pass", never
    /// `run_command_action`'s cancel, and a second, billed pass started beside
    /// the running one. Every item — click and key equivalent alike — now
    /// sends `RunAction`, the message the app answers with that same
    /// function, so every action (Explain All included) owns its menu-safe
    /// chord again.
    #[test]
    fn every_action_item_runs_the_one_dispatch_table() {
        let km = Keymap::defaults();
        let model = menu_model(&km);
        for action in Action::ALL {
            match command_for(tag_for_action(action)) {
                Some(MenuCmd::App(Message::Editor(EditorMsg::RunAction(sent)))) => {
                    assert_eq!(sent, action, "{}", action.id());
                }
                other => panic!("{} sends {other:?}, not RunAction", action.id()),
            }
            let chord = km.chord(action);
            match entry(&model, action.menu().title) {
                Entry::Bridged { key, .. } => assert_eq!(
                    key,
                    &chord.menu_safe().then(|| KeyEquivalent::of(chord)),
                    "{}",
                    action.id()
                ),
                other => panic!("not bridged: {other:?}"),
            }
        }
        match entry(&model, "Explain All") {
            Entry::Bridged { key, .. } => {
                assert_eq!(key, &Some(KeyEquivalent::of(Chord::cmd_shift('e'))));
            }
            other => panic!("not bridged: {other:?}"),
        }
    }

    /// Every action is in the menu, and no keystroke has two owners.
    #[test]
    fn every_action_appears_once_and_no_chord_is_shared() {
        let model = menu_model(&Keymap::defaults());
        for action in Action::ALL {
            let hits = model
                .iter()
                .filter(|m| m.title == action.menu().section.title())
                .flat_map(|m| m.entries.iter())
                .filter(
                    |e| matches!(e, Entry::Bridged { tag, .. } if *tag == tag_for_action(action)),
                )
                .count();
            assert_eq!(hits, 1, "{} must appear exactly once", action.id());
            assert!(
                matches!(command_for(tag_for_action(action)), Some(MenuCmd::App(_))),
                "{}'s tag must route to its message",
                action.id()
            );
        }
        let keys = keyed(&model);
        for (i, (title, key)) in keys.iter().enumerate() {
            assert!(
                !keys[i + 1..].iter().any(|(_, k)| k == key),
                "{title}'s key equivalent is shared"
            );
        }
        // The menu-owned commands carry exactly the reserved chords.
        for (chord, title) in MENU_RESERVED {
            assert!(
                keys.contains(&(title, KeyEquivalent::of(chord))),
                "{title} should own {}",
                chord.caps()
            );
        }
    }

    /// Unknown tags resolve to nothing rather than to some action.
    #[test]
    fn tags_outside_the_known_ranges_resolve_to_nothing() {
        assert!(command_for(0).is_none());
        assert!(command_for(999).is_none());
        assert!(command_for(ACTION_TAG_BASE + Action::ALL.len() as isize).is_none());
        assert!(command_for(-5).is_none());
    }
}
