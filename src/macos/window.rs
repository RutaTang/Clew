//! Window-chrome tweaks that give clew a frameless *look* on a standard window.
//!
//! clew draws its own window controls in the toolbar, so it wants no visible OS
//! title bar. But a plain borderless window is invisible to tiling window
//! managers (AeroSpace, yabai): they drive windows through the Accessibility API
//! and only manage windows reporting the `AXStandardWindow` subrole, which a
//! borderless window lacks. So the window stays a real titled window (see
//! `window_settings`) and we strip its chrome here instead:
//!
//!   - hide the native traffic-light buttons — clew draws its own,
//!   - hide the title-bar view, so the transparent full-size-content title bar
//!     stops intercepting clicks and drags; the top strip then falls through to
//!     clew's content, which captures its own button clicks and starts a window
//!     drag from empty toolbar space (exactly the old borderless behaviour),
//!   - round the corners, which a title bar would otherwise give for free but a
//!     hidden one does not.
//!
//! It also does what the shell's questions need of a window (`crate::shell`):
//! bring it where the user sees it before a sheet is begun on it
//! ([`bring_forward`]), and end its sheets before it closes, or before a
//! question of clew's is begun on it ([`end_sheets`]).

use iced::window::raw_window_handle::RawWindowHandle;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{MainThreadMarker, class, msg_send};
use objc2_app_kit::{
    NSApplication, NSModalResponseAbort, NSRequestUserAttentionType, NSView, NSWindow,
};

// NSWindowButton values for `standardWindowButton:`.
const NS_WINDOW_CLOSE_BUTTON: usize = 0;
const NS_WINDOW_MINIATURIZE_BUTTON: usize = 1;
const NS_WINDOW_ZOOM_BUTTON: usize = 2;

/// Minimize the key (focused) window — the one whose minimize control was just
/// clicked. Calling `miniaturize:` on the NSWindow directly is robust regardless
/// of how winit routes minimize, and works from clew's own toolbar button.
pub fn minimize_key_window() {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let key: *mut AnyObject = unsafe { msg_send![&*app, keyWindow] };
    if key.is_null() {
        return;
    }
    unsafe {
        let nil: *mut AnyObject = std::ptr::null_mut();
        let _: () = msg_send![key, miniaturize: nil];
    }
}

/// The `NSWindow` of one of clew's windows, from the handle a `window::run`
/// callback is given; `None` for a handle that is not AppKit's.
fn ns_window(window: &dyn iced::window::Window) -> Option<Retained<NSWindow>> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return None;
    };
    // SAFETY: winit's handle is the window's content view, alive as long as
    // the window, and the callback runs on the main thread.
    let view = unsafe { Retained::retain(appkit.ns_view.as_ptr().cast::<NSView>()) }?;
    view.window()
}

/// Bring `window` where the user sees it, before a question is asked in a
/// sheet on it: clew active — shown again first, when it was hidden (⌘H) —
/// and the window out of the Dock, in front and key.
///
/// A sheet is begun on its window whatever state that is in, and rfd does
/// no more than begin it: the Dock's Quit, or a log-out, while clew was
/// hidden or the window minimized, asked where nobody could see it, the quit
/// seemed to hang, and a log-out ended with macOS reporting that clew
/// interrupted it.
///
/// But the question can come seconds after the close was asked for — it
/// waited on the host — while the user works in another app. There clew
/// comes forward only when `activate`: the user asked this of clew, or
/// macOS waits on the answer. Otherwise nothing is taken from the app the
/// user is in, and the window is ordered in front of clew's others. Either
/// way, clew not active, its Dock icon asks for attention — activation is
/// only a request since macOS 14, which can be refused — until clew is
/// active when `critical`, macOS waiting on the answer, and once otherwise:
/// a bouncing icon is for what cannot wait.
pub fn bring_forward(window: &dyn iced::window::Window, activate: bool, critical: bool) {
    let Some(window) = ns_window(window) else {
        return;
    };
    let app = NSApplication::sharedApplication(MainThreadMarker::from(&*window));
    let active = app.isActive();
    if active || activate {
        if app.isHidden() {
            app.unhide(None);
        }
        if window.isMiniaturized() {
            window.deminiaturize(None);
        }
        window.makeKeyAndOrderFront(None);
        // `activate` is macOS 14's; this one runs everywhere clew does.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    } else {
        window.orderFront(None);
    }
    if !active {
        app.requestUserAttention(if critical {
            NSRequestUserAttentionType::CriticalRequest
        } else {
            NSRequestUserAttentionType::InformationalRequest
        });
    }
}

/// End every sheet on `window`, presented or queued, as if cancelled —
/// before the window closes, or before a question of clew's takes the place
/// of the one it shows.
///
/// A sheet left on a window that closes is AppKit's to show or finish
/// later, on a window iced no longer has; and when rfd's alert finishes, it
/// brings forward the window that was key when it was built — one that may
/// be closed by then, and came back.
pub fn end_sheets(window: &dyn iced::window::Window) {
    let Some(window) = ns_window(window) else {
        return;
    };
    // Presented first, then queued: the queued ones are ended first, so
    // none is presented as the one in front of it goes.
    let sheets: Vec<Retained<NSWindow>> = window.sheets().iter().collect();
    for sheet in sheets.iter().rev() {
        window.endSheet_returnCode(sheet, NSModalResponseAbort);
    }
}

/// Strip the OS chrome from clew's (titled) windows and round their corners to
/// `radius` points.
///
/// Idempotent and cheap, so it is safe to call on open and on every resize —
/// which also re-applies it after leaving native fullscreen, where AppKit
/// rebuilds and re-shows the title bar. It relies on `MainThreadMarker`, so it
/// silently no-ops off the main thread (which never happens from iced's update
/// loop). If a window is not yet realized it simply finds no content view and
/// skips it.
///
/// Only clew's OWN windows are touched ([`is_clew_window`]). `NSApp.windows`
/// also lists the About panel and every open/save panel and alert rfd shows;
/// restyling those hid their title bars and close buttons and made them
/// non-opaque — a file dialog with no visible way to cancel it.
pub fn configure_frameless(radius: f64) {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);

    // clew runs one iced window per `App` (see `crate::shell`), so there can
    // be several; each is configured on every call.
    let windows: *mut AnyObject = unsafe { msg_send![&*app, windows] };
    if windows.is_null() {
        return;
    }
    let count: usize = unsafe { msg_send![windows, count] };
    for i in 0..count {
        let window: *mut AnyObject = unsafe { msg_send![windows, objectAtIndex: i] };
        if window.is_null() || !unsafe { is_clew_window(window) } {
            continue;
        }
        unsafe {
            hide_titlebar(window);
            round_window(window, radius);
        }
    }
}

/// The class winit gives every window it creates — and so every iced window,
/// and nothing else in the process.
const WINIT_WINDOW_CLASS: &std::ffi::CStr = c"WinitWindow";

/// Whether `window` is one clew opened, rather than a panel AppKit or a dialog
/// crate put on screen.
///
/// winit's `WinitWindow` is the positive test. Should a winit update ever
/// rename it, the class stops resolving and the fallback excludes `NSPanel`
/// and its subclasses (About, NSOpenPanel/NSSavePanel, alerts), which covers
/// every non-clew window clew itself shows.
///
/// # Safety
/// `window` must be a live `NSWindow`, on the main thread.
unsafe fn is_clew_window(window: *mut AnyObject) -> bool {
    match AnyClass::get(WINIT_WINDOW_CLASS) {
        Some(winit) => unsafe { msg_send![window, isKindOfClass: winit] },
        None => {
            let panel: bool = unsafe { msg_send![window, isKindOfClass: class!(NSPanel)] };
            !panel
        }
    }
}

/// Hide the native traffic-light buttons and the whole title-bar view.
///
/// The buttons are subviews of the title-bar view, itself a subview of the
/// title-bar *container* view. Hiding the container removes the last mouse
/// interceptor over the top strip, so clew's content receives every click and
/// drag there.
unsafe fn hide_titlebar(window: *mut AnyObject) {
    let mut container: *mut AnyObject = std::ptr::null_mut();
    for button_id in [
        NS_WINDOW_CLOSE_BUTTON,
        NS_WINDOW_MINIATURIZE_BUTTON,
        NS_WINDOW_ZOOM_BUTTON,
    ] {
        let button: *mut AnyObject = unsafe { msg_send![window, standardWindowButton: button_id] };
        if button.is_null() {
            continue;
        }
        unsafe {
            let _: () = msg_send![button, setHidden: true];
        }
        if container.is_null() {
            // button -> NSTitlebarView -> NSTitlebarContainerView
            let titlebar: *mut AnyObject = unsafe { msg_send![button, superview] };
            if !titlebar.is_null() {
                container = unsafe { msg_send![titlebar, superview] };
            }
        }
    }
    if !container.is_null() {
        unsafe {
            let _: () = msg_send![container, setHidden: true];
        }
    }
}

/// Round `window`'s corners by clipping its content layer, letting the
/// (non-opaque) window composite the rounded shape over the desktop.
unsafe fn round_window(window: *mut AnyObject, radius: f64) {
    unsafe {
        // A non-opaque window lets the masked-away corners show through to
        // whatever is behind the window; keep the drop shadow.
        let _: () = msg_send![window, setOpaque: false];
        let _: () = msg_send![window, setHasShadow: true];

        let content_view: *mut AnyObject = msg_send![window, contentView];
        if content_view.is_null() {
            return;
        }
        // wgpu backs the view with a CAMetalLayer; rounding + clipping that
        // layer rounds the rendered content itself.
        let _: () = msg_send![content_view, setWantsLayer: true];
        let layer: *mut AnyObject = msg_send![content_view, layer];
        if layer.is_null() {
            return;
        }
        let _: () = msg_send![layer, setCornerRadius: radius];
        let _: () = msg_send![layer, setMasksToBounds: true];
    }
}
