//! Hyperion Voice: say "Hey Jarvis" and a question from any app. A short answer appears in the
//! notch; anything longer animates out of the notch into the browser, which comes to the front.
//!
//! Lives in the menu-bar-less accessory layer: no Dock icon, no window until it hears you.

mod audio;
mod browser;
mod config;
mod ctl;
mod notch;
mod objc;
mod relay;
mod speech;
mod wake;

use std::path::PathBuf;

use block2::RcBlock;
use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};

use crate::config::log;
use crate::objc::{main_async, ns, string};

/// The app bundle's Resources directory (models live in Resources/models).
pub fn resources() -> PathBuf {
    unsafe {
        let b: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
        let p: *mut AnyObject = msg_send![b, resourcePath];
        PathBuf::from(string(p))
    }
}

fn already_running() -> bool {
    unsafe {
        let id = ns("com.thomasgreenleaf.hyperion-voice");
        let apps: *mut AnyObject = msg_send![class!(NSRunningApplication), runningApplicationsWithBundleIdentifier: &*id];
        let n: usize = msg_send![apps, count];
        n > 1
    }
}

fn main() {
    log(format!("start, version {}", env!("CARGO_PKG_VERSION")));
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        // NSApplicationActivationPolicyAccessory: no Dock icon, no menu bar.
        let _: bool = msg_send![app, setActivationPolicy: 1isize];
        if already_running() {
            log("another copy is running; exiting");
            return;
        }
        notch::init();

        // Microphone first, then speech recognition, then start listening (ctl::on_ready).
        let media = ns("soun"); // AVMediaTypeAudio
        let granted = RcBlock::new(move |ok: Bool| {
            let ok = ok.as_bool();
            main_async(move || {
                if ok {
                    speech::init();
                } else {
                    log("microphone permission denied");
                }
            });
        });
        let _: () = msg_send![class!(AVCaptureDevice), requestAccessForMediaType: &*media, completionHandler: &*granted];

        let _: () = msg_send![app, run];
    }
}
