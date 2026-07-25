//! Live macOS appearance following.
//!
//! When the theme preference is `System`, clew resolves the OS light/dark
//! appearance at launch and on window focus — but that misses a switch that
//! happens while clew is already frontmost (e.g. the automatic day/night change
//! at sunset). This observes `AppleInterfaceThemeChangedNotification` on the
//! distributed notification center and bridges it into the update loop exactly
//! like the menu does: the callback pushes through a channel that
//! [`subscription`] turns into a [`Message::SystemAppearanceChanged`].

use std::sync::{Once, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AllocAnyThread, MainThreadMarker, class, define_class, msg_send, sel};
use objc2_foundation::NSString;

use iced::Subscription;
use iced::futures::StreamExt;
use iced::futures::channel::mpsc;

use crate::Message;

// The channel a fired notification writes into; the subscription drains it.
static APPEARANCE_TX: OnceLock<mpsc::UnboundedSender<()>> = OnceLock::new();
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
            if let Some(tx) = APPEARANCE_TX.get() {
                let _ = tx.unbounded_send(());
            }
        }
    }
);

/// The iced subscription that turns OS appearance changes into messages. Add it
/// to the app's subscription set (macOS only).
pub fn subscription() -> Subscription<Message> {
    Subscription::run(stream)
}

fn stream() -> impl iced::futures::Stream<Item = Message> {
    let (tx, rx) = mpsc::unbounded::<()>();
    // First subscription wins; a resubscribe reuses the original channel.
    let _ = APPEARANCE_TX.set(tx);
    rx.map(|_| Message::SystemAppearanceChanged)
}

/// Register the distributed-notification observer. Safe to call repeatedly — it
/// runs once, and only on the main thread (its run loop delivers the callback).
pub fn install_once() {
    let Some(_mtm) = MainThreadMarker::new() else {
        return;
    };
    INSTALLED.call_once(|| {
        let observer = OBSERVER.get_or_init(|| {
            let this = AppearanceObserver::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        });
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
