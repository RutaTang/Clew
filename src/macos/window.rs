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

use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSApplication;

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

/// Strip the OS chrome from clew's (titled) windows and round their corners to
/// `radius` points.
///
/// Idempotent and cheap, so it is safe to call on open and on every resize —
/// which also re-applies it after leaving native fullscreen, where AppKit
/// rebuilds and re-shows the title bar. It relies on `MainThreadMarker`, so it
/// silently no-ops off the main thread (which never happens from iced's update
/// loop). If a window is not yet realized it simply finds no content view and
/// skips it.
pub fn configure_frameless(radius: f64) {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);

    // clew is single-window per process today, but iterate defensively rather
    // than guess an index.
    let windows: *mut AnyObject = unsafe { msg_send![&*app, windows] };
    if windows.is_null() {
        return;
    }
    let count: usize = unsafe { msg_send![windows, count] };
    for i in 0..count {
        let window: *mut AnyObject = unsafe { msg_send![windows, objectAtIndex: i] };
        if window.is_null() {
            continue;
        }
        unsafe {
            hide_titlebar(window);
            round_window(window, radius);
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
