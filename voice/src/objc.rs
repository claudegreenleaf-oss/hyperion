//! Small helpers over objc2: strings, main-thread dispatch, a Send wrapper for retained objects.

use std::sync::Mutex;

use block2::RcBlock;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// Copy an NSString pointer into a Rust String (empty for nil).
pub unsafe fn string(p: *mut AnyObject) -> String {
    if p.is_null() {
        String::new()
    } else {
        (*(p as *const NSString)).to_string()
    }
}

/// A retained Objective-C object that may cross threads. Only used for objects Apple documents
/// as thread-safe for our use (audio buffers handed to the speech request).
pub struct Obj(pub Retained<AnyObject>);
unsafe impl Send for Obj {}
unsafe impl Sync for Obj {}

/// Run `f` on the main thread, from any thread, without blocking.
pub fn main_async<F: FnOnce() + Send + 'static>(f: F) {
    autoreleasepool(|_| {
        let cell = Mutex::new(Some(f));
        let block = RcBlock::new(move || {
            let f = cell.lock().ok().and_then(|mut g| g.take());
            if let Some(f) = f {
                autoreleasepool(|_| f());
            }
        });
        unsafe {
            let q: *mut AnyObject = msg_send![class!(NSOperationQueue), mainQueue];
            let _: () = msg_send![q, addOperationWithBlock: &*block];
        }
    });
}

/// Run `f` on the main thread after `ms` milliseconds.
pub fn main_after<F: FnOnce() + Send + 'static>(ms: u64, f: F) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        main_async(f);
    });
}
