//! The notch: a black panel flush with the top of the screen that grows out of the camera
//! housing. Collapsed it is exactly the notch, so it is invisible; it grows to listen, grows a
//! bit more to show a short answer, or grows into the browser window when the answer needs one.

use std::cell::RefCell;
use std::ffi::c_void;

use block2::RcBlock;
use objc2::encode::{Encode, Encoding};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::ctl;
use crate::objc::{main_async, ns};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EdgeInsets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

unsafe impl Encode for EdgeInsets {
    const ENCODING: Encoding = Encoding::Struct("NSEdgeInsets", &[f64::ENCODING, f64::ENCODING, f64::ENCODING, f64::ENCODING]);
}

struct Notch {
    panel: Retained<AnyObject>,
    root: Retained<AnyObject>,
    dot: Retained<AnyObject>,
    title: Retained<AnyObject>,
    sub: Retained<AnyObject>,
    rows: Vec<Retained<AnyObject>>,
    row_urls: Vec<String>,
    row_rects: Vec<NSRect>,
    screen: NSRect,
    primary_h: f64,
    notch_w: f64,
    notch_h: f64,
    shown: bool,
    seq: u64,
    showing_rows: bool,
}

thread_local! {
    static N: RefCell<Option<Notch>> = const { RefCell::new(None) };
}

const LISTEN_W: f64 = 460.0;
const ANSWER_W: f64 = 620.0;

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

unsafe fn color(name: &str) -> *mut AnyObject {
    match name {
        "black" => msg_send![class!(NSColor), blackColor],
        "white" => msg_send![class!(NSColor), whiteColor],
        "clear" => msg_send![class!(NSColor), clearColor],
        "green" => msg_send![class!(NSColor), systemGreenColor],
        _ => msg_send![class!(NSColor), secondaryLabelColor],
    }
}

unsafe fn label(size: f64, weight: f64, col: &str, lines: isize) -> Retained<AnyObject> {
    let empty = ns("");
    let l: Retained<AnyObject> = if lines > 1 {
        msg_send![class!(NSTextField), wrappingLabelWithString: &*empty]
    } else {
        msg_send![class!(NSTextField), labelWithString: &*empty]
    };
    let font: *mut AnyObject = msg_send![class!(NSFont), systemFontOfSize: size, weight: weight];
    let _: () = msg_send![&*l, setFont: font];
    let _: () = msg_send![&*l, setTextColor: color(col)];
    let _: () = msg_send![&*l, setMaximumNumberOfLines: lines];
    let mode: usize = if lines > 1 { 0 } else { 4 }; // word wrapping, or truncate the tail
    let _: () = msg_send![&*l, setLineBreakMode: mode];
    let cell: *mut AnyObject = msg_send![&*l, cell];
    let _: () = msg_send![cell, setTruncatesLastVisibleLine: true];
    // Keep a fixed size, stay centered horizontally and pinned to the top while the panel grows.
    let _: () = msg_send![&*l, setAutoresizingMask: 1usize | 4 | 8];
    l
}

unsafe fn notch_screen() -> (*mut AnyObject, EdgeInsets) {
    let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
    let count: usize = msg_send![screens, count];
    for i in 0..count {
        let s: *mut AnyObject = msg_send![screens, objectAtIndex: i];
        let ins: EdgeInsets = msg_send![s, safeAreaInsets];
        if ins.top > 0.0 {
            return (s, ins);
        }
    }
    (msg_send![class!(NSScreen), mainScreen], EdgeInsets::default())
}

/// Build the (hidden) panel. Main thread, once.
pub fn init() {
    unsafe {
        let (screen, ins) = notch_screen();
        let frame: NSRect = msg_send![screen, frame];
        let (notch_w, notch_h) = if ins.top > 0.0 {
            let l: NSRect = msg_send![screen, auxiliaryTopLeftArea];
            let r: NSRect = msg_send![screen, auxiliaryTopRightArea];
            (frame.size.width - l.size.width - r.size.width, ins.top)
        } else {
            (190.0, 30.0)
        };
        let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
        let primary: *mut AnyObject = msg_send![screens, objectAtIndex: 0usize];
        let pf: NSRect = msg_send![primary, frame];

        let start = rect(0.0, 0.0, notch_w, notch_h);
        let a: Allocated<AnyObject> = msg_send![class!(NSPanel), alloc];
        // Borderless, non-activating: it never steals focus from what you are doing.
        let panel: Retained<AnyObject> =
            msg_send![a, initWithContentRect: start, styleMask: (1usize << 7), backing: 2usize, defer: false];
        let _: () = msg_send![&*panel, setLevel: 101isize];
        let _: () = msg_send![&*panel, setOpaque: false];
        let _: () = msg_send![&*panel, setBackgroundColor: color("clear")];
        let _: () = msg_send![&*panel, setHasShadow: true];
        let _: () = msg_send![&*panel, setHidesOnDeactivate: false];
        let _: () = msg_send![&*panel, setReleasedWhenClosed: false];
        let _: () = msg_send![&*panel, setCollectionBehavior: 1usize | 16 | 64 | 256];

        let a: Allocated<AnyObject> = msg_send![class!(NSView), alloc];
        let root: Retained<AnyObject> = msg_send![a, initWithFrame: start];
        let _: () = msg_send![&*root, setWantsLayer: true];
        let layer: *mut AnyObject = msg_send![&*root, layer];
        let black: *const c_void = msg_send![color("black"), CGColor];
        let _: () = msg_send![layer, setBackgroundColor: black];
        let _: () = msg_send![layer, setCornerRadius: 20.0f64];
        let _: () = msg_send![layer, setMaskedCorners: 3usize]; // the two bottom corners
        let _: () = msg_send![&*panel, setContentView: &*root];

        let a: Allocated<AnyObject> = msg_send![class!(NSView), alloc];
        let dot: Retained<AnyObject> = msg_send![a, initWithFrame: rect(0.0, 0.0, 8.0, 8.0)];
        let _: () = msg_send![&*dot, setWantsLayer: true];
        let dl: *mut AnyObject = msg_send![&*dot, layer];
        let green: *const c_void = msg_send![color("green"), CGColor];
        let _: () = msg_send![dl, setBackgroundColor: green];
        let _: () = msg_send![dl, setCornerRadius: 4.0f64];
        let _: () = msg_send![&*dot, setAutoresizingMask: 1usize | 4 | 8];
        pulse(dl);

        let title = label(15.0, 0.23, "white", 16);
        let sub = label(12.0, 0.0, "gray", 1);
        let _: () = msg_send![&*root, addSubview: &*dot];
        let _: () = msg_send![&*root, addSubview: &*title];
        let _: () = msg_send![&*root, addSubview: &*sub];
        let mut rows = Vec::new();
        for _ in 0..3 {
            let r = label(13.0, 0.0, "white", 1);
            let _: () = msg_send![&*r, setHidden: true];
            let _: () = msg_send![&*root, addSubview: &*r];
            rows.push(r);
        }

        // Clicks on the panel arrive through a local monitor (no subclass needed).
        let pptr = Retained::as_ptr(&panel) as usize;
        let handler = RcBlock::new(move |ev: *mut AnyObject| -> *mut AnyObject {
            let w: *mut AnyObject = msg_send![ev, window];
            if w as usize == pptr {
                let p: NSPoint = msg_send![ev, locationInWindow];
                let (x, y) = (p.x, p.y);
                main_async(move || ctl::on_click(x, y));
            }
            ev
        });
        let _: *mut AnyObject =
            msg_send![class!(NSEvent), addLocalMonitorForEventsMatchingMask: (1u64 << 1), handler: &*handler];
        std::mem::forget(handler);

        N.with(|n| {
            *n.borrow_mut() = Some(Notch {
                panel,
                root,
                dot,
                title,
                sub,
                rows,
                row_urls: Vec::new(),
                row_rects: Vec::new(),
                screen: frame,
                primary_h: pf.size.height,
                notch_w,
                notch_h,
                shown: false,
                seq: 0,
                showing_rows: false,
            })
        });
    }
}

unsafe fn pulse(layer: *mut AnyObject) {
    let anim: *mut AnyObject = msg_send![class!(CABasicAnimation), animationWithKeyPath: &*ns("opacity")];
    let from: Retained<AnyObject> = msg_send![class!(NSNumber), numberWithFloat: 1.0f32];
    let to: Retained<AnyObject> = msg_send![class!(NSNumber), numberWithFloat: 0.25f32];
    let _: () = msg_send![anim, setFromValue: &*from];
    let _: () = msg_send![anim, setToValue: &*to];
    let _: () = msg_send![anim, setDuration: 0.6f64];
    let _: () = msg_send![anim, setAutoreverses: true];
    let _: () = msg_send![anim, setRepeatCount: f32::INFINITY];
    let _: () = msg_send![layer, addAnimation: anim, forKey: &*ns("pulse")];
}

impl Notch {
    fn frame_for(&self, w: f64, h: f64) -> NSRect {
        let x = self.screen.origin.x + (self.screen.size.width - w) / 2.0;
        let y = self.screen.origin.y + self.screen.size.height - h;
        rect(x, y, w, h)
    }

    fn collapsed(&self) -> NSRect {
        self.frame_for(self.notch_w, self.notch_h)
    }

    /// Lay the text out in the panel's current (final) frame: title, the line under it, then
    /// up to three clickable source rows.
    unsafe fn layout(&mut self, title_lines: f64, show_dot: bool) {
        let cur: NSRect = msg_send![&*self.panel, frame];
        let (w, h) = (cur.size.width, cur.size.height);
        let pad = 24.0;
        let x0 = if show_dot { pad + 18.0 } else { pad };
        let tw = (w - x0 - pad).max(10.0);
        let title_h = 20.0 * title_lines;
        let top = self.notch_h + 10.0;
        let _: () = msg_send![&*self.title, setMaximumNumberOfLines: title_lines as isize];
        let _: () = msg_send![&*self.title, setFrame: rect(x0, h - top - title_h, tw, title_h)];
        let _: () = msg_send![&*self.sub, setFrame: rect(x0, h - top - title_h - 20.0, tw, 16.0)];
        let _: () = msg_send![&*self.dot, setFrame: rect(pad, h - top - 14.0, 8.0, 8.0)];
        let _: () = msg_send![&*self.dot, setHidden: !show_dot];
        let _: () = msg_send![&*self.title, setHidden: false];
        let _: () = msg_send![&*self.sub, setHidden: false];
        self.row_rects.clear();
        let mut y = h - top - title_h - 20.0 - 10.0;
        for (i, r) in self.rows.iter().enumerate() {
            if i < self.row_urls.len() {
                y -= 22.0;
                let fr = rect(x0, y, tw, 18.0);
                let _: () = msg_send![&**r, setFrame: fr];
                let _: () = msg_send![&**r, setHidden: false];
                self.row_rects.push(fr);
            } else {
                let _: () = msg_send![&**r, setHidden: true];
            }
        }
    }

    unsafe fn hide_text(&self) {
        let _: () = msg_send![&*self.title, setHidden: true];
        let _: () = msg_send![&*self.sub, setHidden: true];
        let _: () = msg_send![&*self.dot, setHidden: true];
        for r in &self.rows {
            let _: () = msg_send![&**r, setHidden: true];
        }
    }

    /// Animate the panel to `to`; afterwards (on a fresh main-loop turn) run `done` unless a
    /// newer animation has started.
    unsafe fn animate(&mut self, to: NSRect, secs: f64, done: Box<dyn FnOnce()>) {
        self.seq += 1;
        let seq = self.seq;
        PENDING.with(|p| *p.borrow_mut() = Some((seq, done)));
        let p = Retained::as_ptr(&self.panel) as usize;
        let changes = RcBlock::new(move |ctx: *mut AnyObject| {
            let _: () = msg_send![ctx, setDuration: secs];
            let anim: *mut AnyObject = msg_send![p as *mut AnyObject, animator];
            let _: () = msg_send![anim, setFrame: to, display: true];
        });
        let completion = RcBlock::new(move || main_async(move || finish(seq)));
        let _: () = msg_send![class!(NSAnimationContext), runAnimationGroup: &*changes, completionHandler: &*completion];
    }

    unsafe fn ensure_shown(&mut self) {
        if !self.shown {
            let _: () = msg_send![&*self.panel, setFrame: self.collapsed(), display: true];
            let _: () = msg_send![&*self.panel, orderFrontRegardless];
            self.shown = true;
        }
    }

    unsafe fn set_text(&self, title: &str, sub: &str) {
        let _: () = msg_send![&*self.title, setStringValue: &*ns(title)];
        let _: () = msg_send![&*self.sub, setStringValue: &*ns(sub)];
    }

    /// Hide the text, grow or shrink to `target`, then show the text laid out for it.
    unsafe fn transition(&mut self, target: NSRect, secs: f64, title: &str, sub: &str, lines: f64, dot: bool) {
        self.ensure_shown();
        if !self.showing_rows {
            self.row_urls.clear();
        }
        self.showing_rows = false;
        self.hide_text();
        self.set_text(title, sub);
        self.animate(target, secs, Box::new(move || {
            with(|n| unsafe { n.layout(lines, dot) });
        }));
    }
}

thread_local! {
    static PENDING: RefCell<Option<(u64, Box<dyn FnOnce()>)>> = const { RefCell::new(None) };
}

fn finish(seq: u64) {
    let job = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        match p.as_ref() {
            Some((s, _)) if *s == seq => p.take().map(|(_, f)| f),
            _ => None,
        }
    });
    if let Some(f) = job {
        f();
    }
}

fn with<R>(f: impl FnOnce(&mut Notch) -> R) -> Option<R> {
    N.with(|n| n.borrow_mut().as_mut().map(f))
}

/// Grow out of the notch and show what is being heard.
pub fn listening(text: &str) {
    with(|n| unsafe {
        let target = n.frame_for(LISTEN_W, n.notch_h + 52.0);
        n.transition(target, 0.22, if text.is_empty() { "Listening" } else { text }, "", 1.0, true);
    });
}

/// Update the live transcript in place.
pub fn transcript(text: &str) {
    with(|n| unsafe {
        let _: () = msg_send![&*n.title, setStringValue: &*ns(text)];
    });
}

pub fn thinking(query: &str) {
    with(|n| unsafe {
        let target = n.frame_for(LISTEN_W, n.notch_h + 70.0);
        n.transition(target, 0.16, query, "Finding the answer", 1.0, true);
    });
}

fn line_count(text: &str) -> f64 {
    text.split('\n').map(|l| ((l.chars().count().max(1) as f64) / 66.0).ceil()).sum::<f64>().clamp(1.0, 16.0)
}

/// The answer: grow just enough to show it, with up to three sources (label, url) to click.
pub fn answer(text: &str, source: &str, rows: &[(String, String)]) {
    with(|n| unsafe {
        let lines = line_count(text);
        let shown = rows.len().min(3);
        let rows_h = if shown > 0 { 10.0 + 22.0 * shown as f64 } else { 0.0 };
        let target = n.frame_for(ANSWER_W, n.notch_h + 10.0 + 20.0 * lines + 20.0 + rows_h + 18.0);
        n.row_urls = rows.iter().take(3).map(|(_, u)| u.clone()).collect();
        for (r, (label, _)) in n.rows.iter().zip(rows.iter()) {
            let _: () = msg_send![&**r, setStringValue: &*ns(&format!("↗  {label}"))];
        }
        n.showing_rows = true;
        n.transition(target, 0.24, text, source, lines, false);
    });
}

/// A command ran: a short confirmation.
pub fn done(text: &str) {
    with(|n| unsafe {
        let target = n.frame_for(LISTEN_W, n.notch_h + 52.0);
        n.transition(target, 0.2, &format!("✓  {text}"), "", 1.0, false);
    });
}

/// Which source row, if any, is under a click (window coordinates).
pub fn row_at(x: f64, y: f64) -> Option<String> {
    with(|n| {
        n.row_rects.iter().zip(n.row_urls.iter()).find_map(|(r, u)| {
            (x >= r.origin.x && x <= r.origin.x + r.size.width && y >= r.origin.y - 2.0 && y <= r.origin.y + r.size.height + 2.0)
                .then(|| u.clone())
        })
    })
    .flatten()
}

/// Grow into the browser window's frame (AppleScript bounds: left, top, right, bottom from the
/// primary screen's top-left), run `then`, and disappear.
pub fn handoff(bounds: Option<(f64, f64, f64, f64)>, then: impl FnOnce() + 'static) {
    let then: Box<dyn FnOnce()> = Box::new(then);
    with(move |n| unsafe {
        n.ensure_shown();
        n.hide_text();
        let target = match bounds {
            Some((l, t, r, b)) if r > l && b > t => rect(l, n.primary_h - b, r - l, b - t),
            _ => {
                let w = (n.screen.size.width * 0.82).min(1500.0);
                let h = (n.screen.size.height * 0.84).min(1000.0);
                rect(n.screen.origin.x + (n.screen.size.width - w) / 2.0,
                     n.screen.origin.y + (n.screen.size.height - h) / 2.0 - 20.0, w, h)
            }
        };
        n.animate(target, 0.28, Box::new(move || {
            then();
            crate::objc::main_after(280, hide);
        }));
    });
}

pub fn hide() {
    with(|n| unsafe {
        if n.shown {
            n.seq += 1;
            n.hide_text();
            let _: () = msg_send![&*n.panel, orderOut: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![&*n.panel, setFrame: n.collapsed(), display: false];
            n.shown = false;
        }
    });
}

/// Shrink back into the notch, then hide.
pub fn collapse() {
    with(|n| unsafe {
        if !n.shown {
            return;
        }
        n.hide_text();
        let to = n.collapsed();
        n.animate(to, 0.2, Box::new(hide));
    });
}
