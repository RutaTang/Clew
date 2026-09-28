//! macOS's own requests to quit: the Dock's Quit item, a log-out, a restart or
//! a shutdown, `quit app "clew"` in AppleScript — every quit that reaches
//! AppKit's `terminate:` rather than clew's menu (whose Quit is bridged to the
//! shell, see `crate::macos::menu`).
//!
//! `terminate:` first asks the application's delegate
//! (`applicationShouldTerminate:`). winit's delegate does not answer, so
//! AppKit went straight on to `exit(0)`: no window's teardown ran, and the
//! edits a window could not send to its host were lost without a word. The
//! delegate now answers `NSTerminateLater` and hands the request to the shell
//! ([`subscription`]), which carries it out as a quit — the question ⌘Q asks
//! when a window holds edits it cannot send, then every window's teardown —
//! and gives AppKit the outcome ([`reply`]): terminate, once the teardowns
//! are over, or stay running, when the user cancelled. A log-out cancelled
//! this way is cancelled; macOS says clew stopped it.
//!
//! What cannot be asked: a death that never reaches `terminate:`. Force Quit
//! and `kill -9` are SIGKILL, `kill` and launchd's stop are SIGTERM (not
//! caught), and a crash is a crash. None of them lets anything run, so edits a
//! window could not send go with the process, unasked, and a debug adapter is
//! left to notice its pipes closing. And a log-out or shutdown held up by the
//! question is subject to the system's own patience with applications that
//! do not quit, which clew can neither see nor extend; a question never
//! answered is a quit never carried out.
//!
//! The method is added to winit's own delegate class, at run time: winit
//! checks on every turn of its loop that the application's delegate is its
//! own, so clew cannot put one of its own in front of it. AppKit looks the
//! method up each time `terminate:` runs, so adding it once the application
//! runs is enough.
//!
//! None of this runs without a live AppKit application, so no unit test covers
//! it; the shell's side is tested with the platform injected
//! (`crate::shell`'s `Platform`). Manual check: with a debug session running,
//! quit clew from its Dock icon's menu — the debuggee stops with it; and with
//! a remote project whose connection is down and a bookmark added since, the
//! same quit asks first, in a sheet: Cancel keeps clew running, Quit Anyway
//! quits it.

use std::sync::{Mutex, Once, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Imp, NSObject, NSObjectProtocol, Sel};
use objc2::{AllocAnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
use objc2_foundation::{NSArray, NSRunLoopCommonModes};

use iced::Subscription;
use iced::futures::channel::mpsc;

// The channel a terminate request is written into; the subscription drains
// it. Replaced by every new subscription stream, like the menu bridge's
// `MENU_TX`: a set-once sender kept pointing at a receiver gone with an
// earlier stream.
static TERMINATE_TX: Mutex<Option<mpsc::UnboundedSender<()>>> = Mutex::new(None);
// Keep the Objective-C reply target alive for the whole process.
static REPLY_TARGET: OnceLock<Retained<ReplyTarget>> = OnceLock::new();
// Add the delegate method exactly once.
static INSTALLED: Once = Once::new();

define_class!(
    // A tiny Objective-C object whose methods give AppKit its answer, from
    // the run loop (see `reply`).
    #[unsafe(super(NSObject))]
    #[name = "ClewTerminateReply"]
    struct ReplyTarget;

    impl ReplyTarget {
        #[unsafe(method(terminate:))]
        fn terminate(&self, _unused: *mut AnyObject) {
            answer(true);
        }

        #[unsafe(method(stayRunning:))]
        fn stay_running(&self, _unused: *mut AnyObject) {
            answer(false);
        }
    }
);

/// Give AppKit the answer it waits for.
fn answer(terminate: bool) {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).replyToApplicationShouldTerminate(terminate);
    }
}

/// `applicationShouldTerminate:`, as added to the delegate's class: the
/// request goes to the shell, which answers later — or, when no shell is
/// listening, AppKit terminates now, as it did before.
unsafe extern "C-unwind" fn should_terminate(
    _delegate: *mut AnyObject,
    _cmd: Sel,
    _sender: *mut AnyObject,
) -> NSApplicationTerminateReply {
    let tx = TERMINATE_TX.lock().unwrap_or_else(|e| e.into_inner());
    match tx.as_ref().map(|tx| tx.unbounded_send(())) {
        Some(Ok(())) => NSApplicationTerminateReply::TerminateLater,
        _ => NSApplicationTerminateReply::TerminateNow,
    }
}

/// The iced subscription that turns AppKit's terminate requests into
/// messages, one `()` each. Add it to the app's subscription set (macOS
/// only).
pub fn subscription() -> Subscription<()> {
    Subscription::run(stream)
}

fn stream() -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded::<()>();
    // The newest stream owns the channel (see `TERMINATE_TX`).
    *TERMINATE_TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    rx
}

/// Answer the terminate request [`subscription`] delivered: `true` lets
/// AppKit terminate the process (it calls `exit`), `false` keeps clew
/// running.
///
/// Given from the run loop rather than from here: this runs inside iced's
/// update, inside winit's event handler, and the termination that follows
/// goes through winit's delegate, which must not re-enter that handler. In
/// the common modes, which include the modal-panel mode AppKit waits in.
pub fn reply(terminate: bool) {
    if MainThreadMarker::new().is_none() {
        return;
    }
    let target = REPLY_TARGET.get_or_init(|| {
        let this = ReplyTarget::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    });
    let selector = if terminate {
        sel!(terminate:)
    } else {
        sel!(stayRunning:)
    };
    // SAFETY: a constant Foundation defines, never written.
    let modes = NSArray::from_slice(&[unsafe { NSRunLoopCommonModes }]);
    // SAFETY: `target` implements `selector`, taking one (unused) object; it
    // is retained for the process's lifetime, and AppKit retains it and the
    // modes until the perform.
    unsafe {
        let _: () = msg_send![
            &**target,
            performSelector: selector,
            withObject: None::<&AnyObject>,
            afterDelay: 0.0f64,
            inModes: &*modes,
        ];
    }
}

/// Add `applicationShouldTerminate:` to the application delegate's class
/// (see the module doc). Safe to call repeatedly — it runs once, and only on
/// the main thread, with the application's delegate installed. A delegate
/// that answers already is left alone, and stderr says so: its answer, not
/// the shell's, then decides whether clew quits unasked.
pub fn install_once() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    INSTALLED.call_once(|| {
        let app = NSApplication::sharedApplication(mtm);
        let Some(delegate) = app.delegate() else {
            return;
        };
        let selector = sel!(applicationShouldTerminate:);
        if delegate.respondsToSelector(selector) {
            eprintln!(
                "[clew] the application delegate answers macOS's quit requests itself: \
                 they may quit unasked"
            );
            return;
        }
        let object: &AnyObject = (*delegate).as_ref();
        type ShouldTerminate = unsafe extern "C-unwind" fn(
            *mut AnyObject,
            Sel,
            *mut AnyObject,
        ) -> NSApplicationTerminateReply;
        // SAFETY: the runtime calls an `Imp` with the signature its type
        // encoding states, which is `should_terminate`'s: an `NSUInteger`
        // (`Q`) back, the receiver and the selector, then one object.
        let added = unsafe {
            let imp = std::mem::transmute::<ShouldTerminate, Imp>(should_terminate);
            objc2::ffi::class_addMethod(
                std::ptr::from_ref(object.class()).cast_mut(),
                selector,
                imp,
                c"Q@:@".as_ptr(),
            )
        };
        if !added.as_bool() {
            eprintln!("[clew] could not answer macOS's quit requests: it will quit unasked");
        }
    });
}
