//! Small helpers over objc2: strings, main-thread dispatch, a Send wrapper for retained objects.

use std::ffi::c_void;
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

extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_after_f(when: u64, queue: *const u8, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

extern "C" fn run_later(context: *mut c_void) {
    let f = unsafe { Box::from_raw(context as *mut Box<dyn FnOnce()>) };
    autoreleasepool(|_| f());
}

/// Run `f` on the main thread after `ms` milliseconds (a GCD timer, no thread).
pub fn main_after<F: FnOnce() + Send + 'static>(ms: u64, f: F) {
    let f: Box<Box<dyn FnOnce()>> = Box::new(Box::new(f));
    unsafe {
        let when = dispatch_time(0, ms as i64 * 1_000_000);
        dispatch_after_f(when, std::ptr::addr_of!(_dispatch_main_q), Box::into_raw(f) as *mut c_void, run_later);
    }
}
