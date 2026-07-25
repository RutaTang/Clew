//! Window-chrome tweaks for the frameless window.
//!
//! clew runs frameless (`decorations: false`) so it can draw its own window
//! controls in the toolbar. A borderless macOS window, however, has square
//! corners: the OS only rounds the corners of windows that own a title bar.
//! We restore the rounded corners here by clipping the window's content layer
//! and letting the (non-opaque) window composite the rounded shape over the
//! desktop.

use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSApplication;

/// Minimize the key (focused) window — the one whose minimize control was just
/// clicked. A borderless window lacks the `miniaturizable` style mask, so
/// winit / iced's `set_minimized` is a no-op; calling `miniaturize:` on the
/// NSWindow directly works regardless.
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

/// Round clew's window corners to `radius` points.
///
/// Idempotent and cheap, so it is safe to call on every resize. It relies on
/// `MainThreadMarker`, so it silently no-ops when called off the main thread
/// (which never happens from iced's update loop). If the window is not yet
/// realized it simply finds no content view and returns.
pub fn round_corners(radius: f64) {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);

    // clew is single-window, but iterate defensively rather than guess an index.
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
            // A non-opaque window lets the masked-away corners show through to
            // whatever is behind the window; keep the drop shadow.
            let _: () = msg_send![window, setOpaque: false];
            let _: () = msg_send![window, setHasShadow: true];

            let content_view: *mut AnyObject = msg_send![window, contentView];
            if content_view.is_null() {
                continue;
            }
            // wgpu backs the view with a CAMetalLayer; rounding + clipping that
            // layer rounds the rendered content itself.
            let _: () = msg_send![content_view, setWantsLayer: true];
            let layer: *mut AnyObject = msg_send![content_view, layer];
            if layer.is_null() {
                continue;
            }
            let _: () = msg_send![layer, setCornerRadius: radius];
            let _: () = msg_send![layer, setMasksToBounds: true];
        }
    }
}
