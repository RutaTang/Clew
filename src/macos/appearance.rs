//! Live macOS appearance following.
//!
//! When the theme preference is `System`, clew resolves the OS light/dark
//! appearance at launch and on window focus — but that misses a switch that
//! happens while clew is already frontmost (e.g. the automatic day/night change
//! at sunset). Two observers bridge such a switch into the update loop exactly
//! like the menu does — each pushes through a channel that [`subscription`]
//! turns into a [`SettingsMsg::SystemAppearanceChanged`]:
//!
//! - key-value observing of `NSApp.effectiveAppearance`, which fires once
//!   AppKit has ADOPTED the new appearance — so the read that follows
//!   ([`effective_is_light`]) sees it;
//! - `AppleInterfaceThemeChangedNotification` on the distributed notification
//!   center, which fires when the user's setting changes. It can arrive
//!   before AppKit updated `effectiveAppearance`, in which case the read that
//!   follows returns the OLD appearance and the theme would lag until the
//!   next window focus; it is kept as the second signal, never the only one.
//!
//! A redundant signal costs one re-read of the appearance: the handler is
//! idempotent.
//!
//! The appearance itself is read in-process from AppKit
//! ([`effective_is_light`]), never by spawning `defaults`.
//!
//! Neither observer can run without a live AppKit application, so no unit
//! test covers them. Manual check: with Appearance: System, keep clew
//! frontmost and switch System Settings → Appearance between Light and Dark;
//! the window re-themes at once, without a focus change.

use std::ffi::c_void;
use std::sync::{Mutex, Once, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AllocAnyThread, MainThreadMarker, class, define_class, msg_send, sel};
use objc2_app_kit::{NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication};
use objc2_foundation::{NSArray, NSString};

use iced::Subscription;
use iced::futures::StreamExt;
use iced::futures::channel::mpsc;

use crate::Message;
use crate::SettingsMsg;

// The channel a fired notification writes into; the subscription drains it.
//
// Replaced by every new subscription stream rather than set once. iced only
// runs this stream again after the subscription left the set and came back, and
// by then the previous receiver is gone: a set-once sender (what this used to
// be) kept pointing at that dead receiver, and every appearance change after
// the resubscribe was lost. Same fix as the menu bridge's `MENU_TX`.
static APPEARANCE_TX: Mutex<Option<mpsc::UnboundedSender<()>>> = Mutex::new(None);
// Keep the Objective-C observer alive for the whole process.
static OBSERVER: OnceLock<Retained<AppearanceObserver>> = OnceLock::new();
// Register the observer exactly once.
static INSTALLED: Once = Once::new();

define_class!(
    // A tiny Objective-C object whose `appearanceChanged:` forwards the OS theme
    // notification into the channel.
    #[unsafe(super(NSObject))]
    #[name = "ClewAppearanceObserver"]
    struct AppearanceObserver;

    impl AppearanceObserver {
        #[unsafe(method(appearanceChanged:))]
        fn appearance_changed(&self, _note: *mut AnyObject) {
            notify();
        }

        // The KVO callback for `NSApp.effectiveAppearance` (the only key
        // path this object observes): the appearance AppKit draws with has
        // changed, and reading it now returns the new one.
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            _key_path: *mut NSString,
            _object: *mut AnyObject,
            _change: *mut AnyObject,
            _context: *mut c_void,
        ) {
            notify();
        }
    }
);

/// Push one "the appearance may have changed" signal into the channel.
fn notify() {
    let tx = APPEARANCE_TX.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = tx.as_ref() {
        // A closed receiver means no stream is listening right now; the next
        // subscription start installs a fresh sender.
        let _ = tx.unbounded_send(());
    }
}

/// Whether AppKit currently resolves this app's appearance to a light one.
///
/// Read from `NSApp.effectiveAppearance` — the appearance AppKit itself draws
/// with, so it tracks the System Settings choice (including Auto) without a
/// subprocess. `None` off the main thread: the appearance API is main-thread
/// only, so callers there fall back to their last answer.
pub fn effective_is_light() -> Option<bool> {
    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);
    let appearance = app.effectiveAppearance();
    // SAFETY: both names are immutable `NSString` constants exported by AppKit,
    // valid for the life of the process.
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let candidates = NSArray::from_slice(&[aqua, dark]);
    let best: Retained<NSString> = appearance.bestMatchFromAppearancesWithNames(&candidates)?;
    Some(&*best != dark)
}

/// The iced subscription that turns OS appearance changes into messages. Add it
/// to the app's subscription set (macOS only).
pub fn subscription() -> Subscription<Message> {
    Subscription::run(stream)
}

fn stream() -> impl iced::futures::Stream<Item = Message> {
    let (tx, rx) = mpsc::unbounded::<()>();
    // The newest stream owns the channel (see `APPEARANCE_TX`).
    *APPEARANCE_TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    rx.map(|_| Message::Settings(SettingsMsg::SystemAppearanceChanged))
}

/// `NSKeyValueObservingOptionNew`.
const KVO_OPTION_NEW: usize = 0x01;

/// Register both observers (see the module doc). Safe to call repeatedly — it
/// runs once, and only on the main thread (its run loop delivers the
/// callbacks). The observer lives for the whole process, so neither is ever
/// removed.
pub fn install_once() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    INSTALLED.call_once(|| {
        let observer = OBSERVER.get_or_init(|| {
            let this = AppearanceObserver::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        });
        let app = NSApplication::sharedApplication(mtm);
        let key_path = NSString::from_str("effectiveAppearance");
        // SAFETY: `observer` implements the KVO callback and is retained for
        // the process's lifetime (`OBSERVER`), so it outlives the observation;
        // the context is unused (null).
        unsafe {
            let _: () = msg_send![
                &*app,
                addObserver: &**observer,
                forKeyPath: &*key_path,
                options: KVO_OPTION_NEW,
                context: std::ptr::null_mut::<c_void>(),
            ];
        }
        unsafe {
            let center: *mut AnyObject =
                msg_send![class!(NSDistributedNotificationCenter), defaultCenter];
            let name = NSString::from_str("AppleInterfaceThemeChangedNotification");
            let _: () = msg_send![
                center,
                addObserver: &**observer,
                selector: sel!(appearanceChanged:),
                name: &*name,
                object: None::<&AnyObject>,
            ];
        }
    });
}
