//! The native macOS menu bar (clew / File / Edit / View / Go).
//!
//! clew runs frameless, but the app menu bar lives at the top of the screen,
//! independent of window chrome. Menu clicks are bridged into the iced update
//! loop exactly like server events: an item's `tag` is pushed through a channel
//! that [`subscription`] turns back into a [`Message`]. Standard items
//! (About / Hide / Quit / Close) use AppKit's own selectors and never touch the
//! bridge.
//!
//! Only globally-safe shortcuts get real key equivalents (⌘P, ⌘\, zoom, …) —
//! those match clew's default keymap, so AppKit simply owns them instead of the
//! in-app key handler, with identical behavior. Chords that matter *inside* text
//! fields (⌘C copy, ⌥←/→ word motion) are deliberately left click-only so the
//! existing text-editing behavior is untouched.

use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AllocAnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::NSString;

use iced::Subscription;
use iced::futures::StreamExt;
use iced::futures::channel::mpsc;

use crate::Message;
use crate::SidebarTab;
use crate::finder::FinderMode;

// Command tags: a stable id per bridged menu item, mapped back to a Message.
const SETTINGS: isize = 1;
const OPEN_FOLDER: isize = 2;
const CONNECT: isize = 3;
const COPY: isize = 4;
const FIND: isize = 5;
const FIND_IN_FILES: isize = 6;
const TOGGLE_SIDEBAR: isize = 7;
const TOGGLE_RIGHT: isize = 8;
const SPLIT: isize = 9;
const ZOOM_IN: isize = 10;
const ZOOM_OUT: isize = 11;
const ZOOM_RESET: isize = 12;
const OVERVIEW: isize = 13;
const STATS: isize = 14;
const FULLSCREEN: isize = 15;
const BACK: isize = 16;
const FORWARD: isize = 17;
const GOTO_FILE: isize = 18;
const GOTO_SYMBOL: isize = 19;
const GOTO_LINE: isize = 20;
const NEW_WINDOW: isize = 21;
const CLOSE_WINDOW: isize = 22;

/// What a menu click resolves to: an app message for the focused window, or a
/// shell-level window command. Keeps window management out of the App layer.
#[derive(Debug, Clone)]
pub enum MenuCmd {
    App(Message),
    NewWindow,
}

/// Map a clicked item's tag to a menu command.
fn command_for(tag: isize) -> Option<MenuCmd> {
    if tag == NEW_WINDOW {
        return Some(MenuCmd::NewWindow);
    }
    message_for(tag).map(MenuCmd::App)
}

/// Map a clicked item's tag back to the Message the update loop should run —
/// the exact same Messages the in-app command handler dispatches.
fn message_for(tag: isize) -> Option<Message> {
    Some(match tag {
        SETTINGS => Message::OpenSettings,
        OPEN_FOLDER => Message::OpenFolderPressed,
        CONNECT => Message::OpenConnect,
        COPY => Message::CopySelection,
        FIND => Message::FindOpened,
        FIND_IN_FILES => Message::SidebarTabPicked(SidebarTab::Search),
        TOGGLE_SIDEBAR => Message::ToggleLeftSidebar,
        TOGGLE_RIGHT => Message::ToggleRightPanel,
        SPLIT => Message::ToggleSplit,
        ZOOM_IN => Message::FontSizeDelta(1.0),
        ZOOM_OUT => Message::FontSizeDelta(-1.0),
        ZOOM_RESET => Message::FontSizeReset,
        OVERVIEW => Message::ShowOverview,
        STATS => Message::ShowStats,
        FULLSCREEN => Message::ToggleFullscreen,
        BACK => Message::GoBack,
        FORWARD => Message::GoForward,
        GOTO_FILE => Message::FinderOpened(FinderMode::Files),
        GOTO_SYMBOL => Message::FinderOpened(FinderMode::Symbols),
        GOTO_LINE => Message::GotoLineRequested,
        CLOSE_WINDOW => Message::CloseWindow,
        _ => return None,
    })
}

// The channel a menu click writes its tag into; the subscription drains it.
static MENU_TX: OnceLock<mpsc::UnboundedSender<isize>> = OnceLock::new();
// Keep the Objective-C target alive for the whole process (menu items hold it).
static MENU_TARGET: OnceLock<Retained<MenuTarget>> = OnceLock::new();
// Install the menu bar exactly once.
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
            if let Some(tx) = MENU_TX.get() {
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
    // First subscription wins; a resubscribe reuses the original channel.
    let _ = MENU_TX.set(tx);
    rx.filter_map(|tag| async move { command_for(tag) })
}

/// Build and install the menu bar. Safe to call repeatedly — it runs once, and
/// only on the main thread (no-ops elsewhere, which never happens from the iced
/// update loop).
pub fn install_once() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    INSTALLED.call_once(|| install(mtm));
}

fn install(mtm: MainThreadMarker) {
    let target = MENU_TARGET.get_or_init(|| {
        let this = MenuTarget::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    });

    let main = NSMenu::new(mtm);

    main.addItem(&submenu(mtm, "clew", target, &[
        Std::new("About clew", sel!(orderFrontStandardAboutPanel:), "", NSEventModifierFlags::empty()).into(),
        Entry::Separator,
        Cmd::new("Settings…", SETTINGS, ",", NSEventModifierFlags::Command).into(),
        Entry::Separator,
        Std::new("Hide clew", sel!(hide:), "h", NSEventModifierFlags::Command).into(),
        Entry::Separator,
        Std::new("Quit clew", sel!(terminate:), "q", NSEventModifierFlags::Command).into(),
    ]));

    main.addItem(&submenu(mtm, "File", target, &[
        Cmd::new("New Window", NEW_WINDOW, "n", NSEventModifierFlags::Command).into(),
        Entry::Separator,
        Cmd::new("Open Folder…", OPEN_FOLDER, "o", NSEventModifierFlags::Command).into(),
        Cmd::click("Connect to Remote…", CONNECT).into(),
        Entry::Separator,
        // Bridged (not performClose:) — a frameless window ignores performClose:.
        Cmd::new("Close Window", CLOSE_WINDOW, "w", NSEventModifierFlags::Command).into(),
    ]));

    main.addItem(&submenu(mtm, "Edit", target, &[
        // Copy is click-only so ⌘C keeps flowing to text fields / the in-app
        // handler unchanged.
        Cmd::click("Copy", COPY).into(),
        Entry::Separator,
        Cmd::new("Find…", FIND, "f", NSEventModifierFlags::Command).into(),
        Cmd::new(
            "Find in Files…",
            FIND_IN_FILES,
            "f",
            NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
        )
        .into(),
    ]));

    main.addItem(&submenu(mtm, "View", target, &[
        Cmd::click("Toggle Sidebar", TOGGLE_SIDEBAR).into(),
        Cmd::click("Toggle Right Panel", TOGGLE_RIGHT).into(),
        Cmd::new("Split Editor", SPLIT, "\\", NSEventModifierFlags::Command).into(),
        Entry::Separator,
        Cmd::new("Zoom In", ZOOM_IN, "=", NSEventModifierFlags::Command).into(),
        Cmd::new("Zoom Out", ZOOM_OUT, "-", NSEventModifierFlags::Command).into(),
        Cmd::new("Actual Size", ZOOM_RESET, "0", NSEventModifierFlags::Command).into(),
        Entry::Separator,
        Cmd::click("Overview", OVERVIEW).into(),
        Cmd::click("Stats", STATS).into(),
        Entry::Separator,
        Cmd::click("Toggle Full Screen", FULLSCREEN).into(),
    ]));

    main.addItem(&submenu(mtm, "Go", target, &[
        // Back/Forward are click-only: their default chord is ⌥←/→, which text
        // fields need for word motion.
        Cmd::click("Back", BACK).into(),
        Cmd::click("Forward", FORWARD).into(),
        Entry::Separator,
        Cmd::new("Go to File…", GOTO_FILE, "p", NSEventModifierFlags::Command).into(),
        Cmd::new("Go to Symbol…", GOTO_SYMBOL, "t", NSEventModifierFlags::Command).into(),
        Cmd::new("Go to Line…", GOTO_LINE, "l", NSEventModifierFlags::Command).into(),
    ]));

    let app = NSApplication::sharedApplication(mtm);
    app.setMainMenu(Some(&main));
}

// A menu entry to build: a bridged command, a standard AppKit action, or a
// separator.
enum Entry {
    Bridged(Cmd),
    Standard(Std),
    Separator,
}

struct Cmd {
    title: &'static str,
    tag: isize,
    key: &'static str,
    mask: NSEventModifierFlags,
}

impl Cmd {
    fn new(title: &'static str, tag: isize, key: &'static str, mask: NSEventModifierFlags) -> Self {
        Self { title, tag, key, mask }
    }
    /// A click-only command (no key equivalent).
    fn click(title: &'static str, tag: isize) -> Self {
        Self { title, tag, key: "", mask: NSEventModifierFlags::empty() }
    }
}
impl From<Cmd> for Entry {
    fn from(c: Cmd) -> Self {
        Entry::Bridged(c)
    }
}

struct Std {
    title: &'static str,
    action: objc2::runtime::Sel,
    key: &'static str,
    mask: NSEventModifierFlags,
}
impl Std {
    fn new(
        title: &'static str,
        action: objc2::runtime::Sel,
        key: &'static str,
        mask: NSEventModifierFlags,
    ) -> Self {
        Self { title, action, key, mask }
    }
}
impl From<Std> for Entry {
    fn from(s: Std) -> Self {
        Entry::Standard(s)
    }
}

/// Build a top-level menu (`title`) holding `entries`, wrapped in the carrier
/// `NSMenuItem` the main menu bar wants.
fn submenu(
    mtm: MainThreadMarker,
    title: &str,
    target: &MenuTarget,
    entries: &[Entry],
) -> Retained<NSMenuItem> {
    let menu = NSMenu::new(mtm);
    menu.setTitle(&NSString::from_str(title));
    for entry in entries {
        match entry {
            Entry::Separator => menu.addItem(&NSMenuItem::separatorItem(mtm)),
            Entry::Bridged(c) => {
                let item = make_item(mtm, c.title, c.key, c.mask);
                unsafe {
                    item.setTag(c.tag);
                    item.setTarget(Some(target));
                    item.setAction(Some(sel!(dispatch:)));
                }
                menu.addItem(&item);
            }
            Entry::Standard(s) => {
                let item = make_item(mtm, s.title, s.key, s.mask);
                unsafe { item.setAction(Some(s.action)) };
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
    key: &str,
    mask: NSEventModifierFlags,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    if !key.is_empty() {
        item.setKeyEquivalent(&NSString::from_str(key));
        item.setKeyEquivalentModifierMask(mask);
    }
    item
}
