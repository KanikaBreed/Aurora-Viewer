//! `kAEGetURL` while this viewer runs (`LLAppDelegate`
//! `applicationWillFinishLaunching` / `handleGetURLEvent`,
//! `indra/newview/llappdelegate-objc.mm`).
//!
//! Called from `ExternalLinks::start` inside `App::new`, which runs after
//! `EventLoop::build` and before `run_app`. winit has created
//! `NSApplication` by then and has no URL-open event of its own, so the
//! handler has to be installed here. Launch Services sends the event to
//! the already running application; a second process is not started.

use super::{MAX_URL, Receiver};
use crossbeam_channel::Sender;
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{AllocAnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager};

/// `kInternetEventClass` and `kAEGetURL` (`'GURL'`).
const GURL: u32 = 0x4755_524C;
/// `keyDirectObject` (`'----'`).
const KEY_DIRECT: u32 = 0x2D2D_2D2D;

static LINKS: std::sync::Mutex<Option<Sender<String>>> = std::sync::Mutex::new(None);

fn deliver(url: &str) {
    let url = url.trim();
    if url.is_empty() || url.len() > MAX_URL {
        return;
    }
    let Ok(slot) = LINKS.lock() else {
        return;
    };
    if let Some(tx) = slot.as_ref() {
        let _ = tx.send(url.to_owned());
    }
}

fn url_of(event: &NSAppleEventDescriptor) -> Option<String> {
    // The direct object is the URL string. `paramDescriptorForKeyword:` is
    // not used through the generated binding: that method is behind the
    // CoreServices feature, which exists only to name this `OSType`.
    let direct: Option<Retained<NSAppleEventDescriptor>> = unsafe { msg_send![event, paramDescriptorForKeyword: KEY_DIRECT] };
    direct?.stringValue().map(|text| text.to_string())
}

define_class!(
    #[unsafe(super(NSObject))]
    struct UrlEvents;

    impl UrlEvents {
        #[unsafe(method(handleGetURLEvent:withReplyEvent:))]
        fn handle_get_url(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            if let Some(url) = url_of(event) {
                deliver(&url);
            }
        }
    }
);

impl UrlEvents {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn install() {
    static HANDLER: std::sync::OnceLock<Retained<UrlEvents>> = std::sync::OnceLock::new();
    if HANDLER.get().is_some() {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("startup links: not on the main thread, so Apple Events are not received");
        return;
    };
    // winit's event loop has usually done this already. A test, or a call
    // that races it, still needs the shared application before the manager.
    drop(objc2_app_kit::NSApplication::sharedApplication(mtm));
    let handler = UrlEvents::new();
    let manager = NSAppleEventManager::sharedAppleEventManager();
    let selector = sel!(handleGetURLEvent:withReplyEvent:);
    // SAFETY: the selector matches `handle_get_url`, both arguments are
    // object pointers, and the two codes are the Carbon `GURL` constants.
    // The manager keeps the handler; `HANDLER` keeps it too.
    unsafe {
        let _: () = msg_send![
            &*manager,
            setEventHandler: &*handler,
            andSelector: selector,
            forEventClass: GURL,
            andEventID: GURL
        ];
    }
    let _ = HANDLER.set(handler);
}

/// Owns the Apple Event handler. `Some` once the channel exists. The
/// viewer calls this once, on the main thread, after the event loop exists.
pub fn listen() -> Option<Receiver<String>> {
    let (tx, rx) = crossbeam_channel::unbounded();
    if let Ok(mut slot) = LINKS.lock() {
        *slot = Some(tx);
    }
    install();
    Some(rx)
}

#[cfg(test)]
mod tests {
    #[test]
    fn listen_returns_a_receiver() {
        // The test harness is not the main thread, so the Apple Event
        // handler itself is installed only when the viewer calls this.
        assert!(super::listen().is_some());
    }

    #[test]
    fn supported_follows_the_running_bundle() {
        let exe = std::env::current_exe().expect("this executable");
        assert_eq!(crate::url_scheme::supported(), crate::url_scheme::runs_from_bundle(&exe));
    }
}
