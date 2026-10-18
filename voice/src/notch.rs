//! The notch, drawn like the Dynamic Island: one black shape that grows out of the camera
//! housing, its top corners flaring into the menu bar and its bottom corners rounded, moved by
//! springs (CASpringAnimation on a CAShapeLayer's path). Playful in motion (an overshooting
//! spring as it opens, content that drops into place, a check that pops), restrained in look
//! (pure black, San Francisco, generous margins).
//!
//! Light by construction: the window never resizes during normal use, so every change is a GPU
//! path animation plus a content fade; animations stop when not shown, and when idle the window
//! is ordered out entirely.

use std::cell::RefCell;
use std::ffi::c_void;

use block2::RcBlock;
use objc2::encode::{Encode, Encoding};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::ctl;
use crate::objc::{main_after, main_async, ns};

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

#[repr(C)]
#[derive(Clone, Copy)]
struct Transform3D([f64; 16]);

const D: Encoding = f64::ENCODING;
unsafe impl Encode for Transform3D {
    const ENCODING: Encoding = Encoding::Struct("CATransform3D", &[D, D, D, D, D, D, D, D, D, D, D, D, D, D, D, D]);
}

impl Transform3D {
    /// Scale by `s` about (cx, cy). View-backed layers anchor at their corner, so the centre
    /// has to be put back by translation.
    fn scale_about(s: f64, cx: f64, cy: f64) -> Self {
        let mut m = [0.0; 16];
        m[0] = s;
        m[5] = s;
        m[10] = 1.0;
        m[15] = 1.0;
        m[12] = cx * (1.0 - s);
        m[13] = cy * (1.0 - s);
        Transform3D(m)
    }
}

extern "C" {
    fn CGPathCreateMutable() -> *mut c_void;
    fn CGPathMoveToPoint(p: *mut c_void, m: *const c_void, x: f64, y: f64);
    fn CGPathAddLineToPoint(p: *mut c_void, m: *const c_void, x: f64, y: f64);
    fn CGPathAddQuadCurveToPoint(p: *mut c_void, m: *const c_void, cx: f64, cy: f64, x: f64, y: f64);
    fn CGPathCloseSubpath(p: *mut c_void);
    fn CGPathRelease(p: *const c_void);
    fn CACurrentMediaTime() -> f64;
}

// Window big enough for the tallest answer; transparent everywhere outside the shape, so clicks
// there pass through to whatever is underneath.
const WIN_W: f64 = 760.0;
const WIN_H: f64 = 540.0;
const PAD: f64 = 22.0;
// The waveform's colours, Siri-like: blue, violet, pink, amber.
const WAVE: [(f64, f64, f64); 4] = [(0.35, 0.62, 1.0), (0.62, 0.45, 1.0), (1.0, 0.42, 0.72), (1.0, 0.7, 0.35)];

/// The shape's geometry: body width and height below the screen top, flare radius at the top
/// corners, rounding at the bottom corners.
#[derive(Clone, Copy)]
struct Shape {
    w: f64,
    h: f64,
    top_r: f64,
    bot_r: f64,
}

struct Notch {
    panel: Retained<AnyObject>,
    root: Retained<AnyObject>,
    holder: Retained<AnyObject>,
    shape: Retained<AnyObject>,
    content: Retained<AnyObject>,
    bars: Retained<AnyObject>,
    bar_layers: Vec<*mut AnyObject>,
    icon: Retained<AnyObject>,
    title: Retained<AnyObject>,
    ear: Retained<AnyObject>,
    pills: Vec<(Retained<AnyObject>, Retained<AnyObject>)>,
    row_urls: Vec<String>,
    row_rects: Vec<NSRect>,
    screen: NSRect,
    primary_h: f64,
    notch_w: f64,
    notch_h: f64,
    current: Shape,
    shown: bool,
    seq: u64,
}

thread_local! {
    static N: RefCell<Option<Notch>> = const { RefCell::new(None) };
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

unsafe fn white(alpha: f64) -> *mut AnyObject {
    msg_send![class!(NSColor), colorWithWhite: 1.0f64, alpha: alpha]
}

unsafe fn rgb(r: f64, g: f64, b: f64) -> *mut AnyObject {
    msg_send![class!(NSColor), colorWithSRGBRed: r, green: g, blue: b, alpha: 1.0f64]
}

unsafe fn cg(color: *mut AnyObject) -> *const c_void {
    msg_send![color, CGColor]
}

unsafe fn number(v: f64) -> Retained<AnyObject> {
    msg_send![class!(NSNumber), numberWithDouble: v]
}

unsafe fn label(size: f64, weight: f64, alpha: f64, wrap: bool) -> Retained<AnyObject> {
    let empty = ns("");
    let l: Retained<AnyObject> = if wrap {
        msg_send![class!(NSTextField), wrappingLabelWithString: &*empty]
    } else {
        msg_send![class!(NSTextField), labelWithString: &*empty]
    };
    let font: *mut AnyObject = msg_send![class!(NSFont), systemFontOfSize: size, weight: weight];
    let _: () = msg_send![&*l, setFont: font];
    let _: () = msg_send![&*l, setTextColor: white(alpha)];
    let _: () = msg_send![&*l, setDrawsBackground: false];
    let _: () = msg_send![&*l, setSelectable: false];
    if !wrap {
        let _: () = msg_send![&*l, setLineBreakMode: 4usize]; // truncate the tail
    }
    l
}

unsafe fn layer_view(r: NSRect) -> Retained<AnyObject> {
    let a: Allocated<AnyObject> = msg_send![class!(NSView), alloc];
    let v: Retained<AnyObject> = msg_send![a, initWithFrame: r];
    let _: () = msg_send![&*v, setWantsLayer: true];
    v
}

unsafe fn symbol(name: &str, size: f64, weight: f64) -> *mut AnyObject {
    let img: *mut AnyObject = msg_send![class!(NSImage), imageWithSystemSymbolName: &*ns(name),
        accessibilityDescription: std::ptr::null_mut::<AnyObject>()];
    if img.is_null() {
        return img;
    }
    let cfg: *mut AnyObject = msg_send![class!(NSImageSymbolConfiguration), configurationWithPointSize: size, weight: weight];
    msg_send![img, imageWithSymbolConfiguration: cfg]
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

/// Path for the shape centred at `cx` with its top edge at `top` (flipped coordinates, y down).
/// Same element structure for every shape, so a spring can interpolate between any two.
unsafe fn path(cx: f64, top: f64, s: Shape) -> *mut c_void {
    let p = CGPathCreateMutable();
    let n = std::ptr::null();
    let (l, r) = (cx - s.w / 2.0 - s.top_r, cx + s.w / 2.0 + s.top_r);
    let (t, b) = (top, top + s.h);
    let br = s.bot_r.min(s.h / 2.0).min(s.w / 2.0);
    CGPathMoveToPoint(p, n, l, t);
    CGPathAddQuadCurveToPoint(p, n, l + s.top_r, t, l + s.top_r, t + s.top_r);
    CGPathAddLineToPoint(p, n, l + s.top_r, b - br);
    CGPathAddQuadCurveToPoint(p, n, l + s.top_r, b, l + s.top_r + br, b);
    CGPathAddLineToPoint(p, n, r - s.top_r - br, b);
    CGPathAddQuadCurveToPoint(p, n, r - s.top_r, b, r - s.top_r, b - br);
    CGPathAddLineToPoint(p, n, r - s.top_r, t + s.top_r);
    CGPathAddQuadCurveToPoint(p, n, r - s.top_r, t, r, t);
    CGPathCloseSubpath(p);
    p
}

/// Path for a floating rounded rectangle (the browser window), with the same structure.
unsafe fn window_path(x: f64, y: f64, w: f64, h: f64) -> *mut c_void {
    path(x + w / 2.0, y, Shape { w, h, top_r: 0.0, bot_r: 12.0 })
}

unsafe fn spring(key_path: &str, duration: f64, bounce: f64) -> Retained<AnyObject> {
    let a: Allocated<AnyObject> = msg_send![class!(CASpringAnimation), alloc];
    let anim: Retained<AnyObject> = msg_send![a, initWithPerceptualDuration: duration, bounce: bounce];
    let _: () = msg_send![&*anim, setKeyPath: &*ns(key_path)];
    let settle: f64 = msg_send![&*anim, settlingDuration];
    let _: () = msg_send![&*anim, setDuration: settle];
    anim
}

/// Spring the shape layer from where it is now (mid-animation included) to `to`.
unsafe fn spring_to(layer: &AnyObject, to: *mut c_void, duration: f64, bounce: f64) -> f64 {
    let pres: *mut AnyObject = msg_send![layer, presentationLayer];
    let from: *const c_void = if pres.is_null() { msg_send![layer, path] } else { msg_send![pres, path] };
    let anim = spring("path", duration, bounce);
    if !from.is_null() {
        let _: () = msg_send![&*anim, setFromValue: from as *mut AnyObject];
    }
    let _: () = msg_send![&*anim, setToValue: to as *mut AnyObject];
    let settle: f64 = msg_send![&*anim, duration];
    let _: () = msg_send![class!(CATransaction), begin];
    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
    let _: () = msg_send![layer, setPath: to as *const c_void];
    let _: () = msg_send![class!(CATransaction), commit];
    let _: () = msg_send![layer, addAnimation: &*anim, forKey: &*ns("path")];
    CGPathRelease(to);
    settle
}

/// Resize the flipped holder and shape layers instantly (standalone layers animate by default).
unsafe fn layers_cover(holder: &AnyObject, shape: &AnyObject, w: f64, h: f64) {
    let _: () = msg_send![class!(CATransaction), begin];
    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
    let _: () = msg_send![holder, setFrame: rect(0.0, 0.0, w, h)];
    let _: () = msg_send![shape, setFrame: rect(0.0, 0.0, w, h)];
    let _: () = msg_send![class!(CATransaction), commit];
}

unsafe fn set_path_now(layer: &AnyObject, p: *mut c_void) {
    let _: () = msg_send![class!(CATransaction), begin];
    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
    let _: () = msg_send![layer, removeAllAnimations];
    let _: () = msg_send![layer, setPath: p as *const c_void];
    let _: () = msg_send![class!(CATransaction), commit];
    CGPathRelease(p);
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
            (180.0, 28.0)
        };
        let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
        let primary: *mut AnyObject = msg_send![screens, objectAtIndex: 0usize];
        let pf: NSRect = msg_send![primary, frame];

        let wr = rect(frame.origin.x + (frame.size.width - WIN_W) / 2.0, frame.origin.y + frame.size.height - WIN_H, WIN_W, WIN_H);
        let a: Allocated<AnyObject> = msg_send![class!(NSPanel), alloc];
        // Borderless and non-activating: it never takes focus from what you are doing.
        let panel: Retained<AnyObject> =
            msg_send![a, initWithContentRect: wr, styleMask: (1usize << 7), backing: 2usize, defer: false];
        let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![&*panel, setLevel: 101isize];
        let _: () = msg_send![&*panel, setOpaque: false];
        let _: () = msg_send![&*panel, setBackgroundColor: clear];
        let _: () = msg_send![&*panel, setHasShadow: false];
        let _: () = msg_send![&*panel, setHidesOnDeactivate: false];
        let _: () = msg_send![&*panel, setReleasedWhenClosed: false];
        let _: () = msg_send![&*panel, setCollectionBehavior: 1usize | 16 | 64 | 256];

        let root = layer_view(rect(0.0, 0.0, WIN_W, WIN_H));
        let _: () = msg_send![&*panel, setContentView: &*root];
        let shape_view = layer_view(rect(0.0, 0.0, WIN_W, WIN_H));
        let _: () = msg_send![&*shape_view, setAutoresizingMask: 18usize];
        let _: () = msg_send![&*root, addSubview: &*shape_view];
        let rl: *mut AnyObject = msg_send![&*shape_view, layer];

        // A flipped holder layer so the shape's path is in screen-like coordinates (y down).
        let holder: Retained<AnyObject> = msg_send![class!(CALayer), layer];
        let _: () = msg_send![&*holder, setFrame: rect(0.0, 0.0, WIN_W, WIN_H)];
        let _: () = msg_send![&*holder, setGeometryFlipped: true];
        let _: () = msg_send![rl, addSublayer: &*holder];
        let shape: Retained<AnyObject> = msg_send![class!(CAShapeLayer), layer];
        let black: *mut AnyObject = msg_send![class!(NSColor), blackColor];
        let _: () = msg_send![&*shape, setFrame: rect(0.0, 0.0, WIN_W, WIN_H)];
        let _: () = msg_send![&*shape, setFillColor: cg(black)];
        let _: () = msg_send![&*holder, addSublayer: &*shape];

        // All text and icons live in one view that fades and settles as a unit.
        let content = layer_view(rect(0.0, 0.0, WIN_W, WIN_H));
        let _: () = msg_send![&*content, setAutoresizingMask: 18usize];
        let _: () = msg_send![&*root, addSubview: &*content];

        let bars = layer_view(rect(0.0, 0.0, 24.0, 20.0));
        let bl: *mut AnyObject = msg_send![&*bars, layer];
        let mut bar_layers = Vec::new();
        for (i, (r, g, b)) in WAVE.iter().enumerate() {
            let bar: *mut AnyObject = msg_send![class!(CALayer), layer];
            let _: () = msg_send![bar, setFrame: rect(1.0 + i as f64 * 6.0, 2.0, 3.5, 16.0)];
            let _: () = msg_send![bar, setCornerRadius: 1.75f64];
            let _: () = msg_send![bar, setBackgroundColor: cg(rgb(*r, *g, *b))];
            let _: () = msg_send![bl, addSublayer: bar];
            bar_layers.push(bar);
        }
        let _: () = msg_send![&*content, addSubview: &*bars];

        let icon: Retained<AnyObject> = msg_send![class!(NSImageView), new];
        let _: () = msg_send![&*icon, setWantsLayer: true];
        let _: () = msg_send![&*content, addSubview: &*icon];
        let title = label(15.0, 0.23, 0.96, true);
        let ear = label(11.5, 0.23, 0.5, false);
        let _: () = msg_send![&*content, addSubview: &*title];
        let _: () = msg_send![&*content, addSubview: &*ear];

        let mut pills = Vec::new();
        for _ in 0..3 {
            let p = layer_view(rect(0.0, 0.0, 10.0, 10.0));
            let pl: *mut AnyObject = msg_send![&*p, layer];
            let _: () = msg_send![pl, setCornerRadius: 10.0f64];
            let _: () = msg_send![pl, setBackgroundColor: cg(white(0.08))];
            let _: () = msg_send![pl, setBorderWidth: 0.5f64];
            let _: () = msg_send![pl, setBorderColor: cg(white(0.1))];
            let t = label(12.5, 0.0, 0.78, false);
            let _: () = msg_send![&*p, addSubview: &*t];
            let _: () = msg_send![&*content, addSubview: &*p];
            pills.push((p, t));
        }

        // Clicks on the shape come through a local monitor (no subclass needed).
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

        let collapsed = Shape { w: notch_w, h: notch_h, top_r: 6.0, bot_r: 10.0 };
        set_path_now(&shape, path(WIN_W / 2.0, 0.0, collapsed));
        N.with(|n| {
            *n.borrow_mut() = Some(Notch {
                panel, root, holder, shape, content, bars, bar_layers, icon, title, ear, pills,
                row_urls: Vec::new(),
                row_rects: Vec::new(),
                screen: frame,
                primary_h: pf.size.height,
                notch_w,
                notch_h,
                current: collapsed,
                shown: false,
                seq: 0,
            })
        });
    }
}

impl Notch {
    fn collapsed(&self) -> Shape {
        Shape { w: self.notch_w, h: self.notch_h, top_r: 6.0, bot_r: 10.0 }
    }

    /// Left edge of the body, in window coordinates.
    fn body_left(&self, s: Shape) -> f64 {
        (WIN_W - s.w) / 2.0
    }

    /// y (window coordinates, y up) of something `from_top` points below the screen top.
    fn y(&self, from_top: f64, height: f64) -> f64 {
        WIN_H - from_top - height
    }

    fn window_frame(&self) -> NSRect {
        rect(self.screen.origin.x + (self.screen.size.width - WIN_W) / 2.0,
             self.screen.origin.y + self.screen.size.height - WIN_H, WIN_W, WIN_H)
    }

    unsafe fn show_window(&mut self) {
        if !self.shown {
            let _: () = msg_send![&*self.panel, setFrame: self.window_frame(), display: false];
            layers_cover(&self.holder, &self.shape, WIN_W, WIN_H);
            set_path_now(&self.shape, path(WIN_W / 2.0, 0.0, self.collapsed()));
            self.current = self.collapsed();
            let _: () = msg_send![&*self.root, setAlphaValue: 1.0f64];
            let _: () = msg_send![&*self.panel, orderFrontRegardless];
            self.shown = true;
        }
    }

    /// The waveform: each bar breathes on its own rhythm; slower and dimmer while thinking.
    unsafe fn bars_animating(&self, on: bool, slow: bool) {
        for (i, b) in self.bar_layers.iter().enumerate() {
            let _: () = msg_send![*b, removeAllAnimations];
            if !on {
                continue;
            }
            let anim: *mut AnyObject = msg_send![class!(CABasicAnimation), animationWithKeyPath: &*ns("transform.scale.y")];
            let _: () = msg_send![anim, setFromValue: &*number(if slow { 0.45 } else { 0.25 })];
            let _: () = msg_send![anim, setToValue: &*number(1.0)];
            let d = if slow { 0.8 + 0.1 * i as f64 } else { 0.32 + 0.06 * ((i * 7) % 4) as f64 };
            let _: () = msg_send![anim, setDuration: d];
            let _: () = msg_send![anim, setAutoreverses: true];
            let _: () = msg_send![anim, setRepeatCount: f32::INFINITY];
            let ease: *mut AnyObject = msg_send![class!(CAMediaTimingFunction), functionWithName: &*ns("easeInEaseOut")];
            let _: () = msg_send![anim, setTimingFunction: ease];
            let _: () = msg_send![anim, setBeginTime: CACurrentMediaTime() + 0.09 * i as f64];
            let _: () = msg_send![*b, addAnimation: anim, forKey: &*ns("wave")];
        }
        let alpha: f64 = if slow { 0.55 } else { 1.0 };
        let _: () = msg_send![&*self.bars, setAlphaValue: alpha];
    }

    /// Hide every content element; each state then shows only what it uses.
    unsafe fn clear(&mut self) {
        for v in [&self.bars, &self.icon, &self.title, &self.ear] {
            let _: () = msg_send![&**v, setHidden: true];
        }
        for (p, _) in &self.pills {
            let _: () = msg_send![&**p, setHidden: true];
        }
        self.row_urls.clear();
        self.row_rects.clear();
        self.bars_animating(false, false);
    }

    /// Spring the shape to `s` (an overshoot when it grows), with the content fading out and
    /// then dropping into place once the shape has nearly arrived.
    unsafe fn morph(&mut self, s: Shape, bounce: f64) {
        if self.shown && (s.w - self.current.w).abs() < 0.5 && (s.h - self.current.h).abs() < 0.5 {
            // Same size (listening to thinking): swap the content in place, no flash.
            return;
        }
        self.seq += 1;
        self.show_window();
        let seq = self.seq;
        let _: () = msg_send![&*self.content, setAlphaValue: 0.0f64];
        spring_to(&self.shape, path(WIN_W / 2.0, 0.0, s), 0.5, bounce);
        self.current = s;
        main_after(120, move || {
            with(|n| unsafe {
                if n.seq == seq {
                    fade(&n.content, 1.0, 0.22);
                    settle_in(&n.content);
                }
            });
        });
    }
}

unsafe fn fade(view: &AnyObject, to: f64, secs: f64) {
    let v = view as *const AnyObject as usize;
    let changes = RcBlock::new(move |ctx: *mut AnyObject| {
        let _: () = msg_send![ctx, setDuration: secs];
        let anim: *mut AnyObject = msg_send![v as *mut AnyObject, animator];
        let _: () = msg_send![anim, setAlphaValue: to];
    });
    let _: () = msg_send![class!(NSAnimationContext), runAnimationGroup: &*changes];
}

/// Content drops a few points into place with a soft spring.
unsafe fn settle_in(view: &AnyObject) {
    let layer: *mut AnyObject = msg_send![view, layer];
    if layer.is_null() {
        return;
    }
    let anim = spring("transform.translation.y", 0.45, 0.35);
    let _: () = msg_send![&*anim, setFromValue: &*number(7.0)];
    let _: () = msg_send![&*anim, setToValue: &*number(0.0)];
    let _: () = msg_send![layer, addAnimation: &*anim, forKey: &*ns("settle")];
}

/// A playful pop for an icon (the check after a command): it springs up from small, past full
/// size, and settles.
unsafe fn pop(view: &AnyObject) {
    let layer: *mut AnyObject = msg_send![view, layer];
    if layer.is_null() {
        return;
    }
    let b: NSRect = msg_send![layer, bounds];
    let from = Transform3D::scale_about(0.3, b.size.width / 2.0, b.size.height / 2.0);
    let to = Transform3D::scale_about(1.0, 0.0, 0.0);
    let fv: *mut AnyObject = msg_send![class!(NSValue), valueWithCATransform3D: from];
    let tv: *mut AnyObject = msg_send![class!(NSValue), valueWithCATransform3D: to];
    let anim = spring("transform", 0.5, 0.55);
    let _: () = msg_send![&*anim, setFromValue: fv];
    let _: () = msg_send![&*anim, setToValue: tv];
    let _: () = msg_send![layer, addAnimation: &*anim, forKey: &*ns("pop")];
}

fn with<R>(f: impl FnOnce(&mut Notch) -> R) -> Option<R> {
    N.with(|n| n.borrow_mut().as_mut().map(f))
}

/// One line under the camera: an animated waveform (or icon) and text.
unsafe fn one_line(n: &mut Notch, s: Shape, text: &str, waveform: bool, slow: bool, icon: Option<(&str, *mut AnyObject)>) {
    n.clear();
    let left = n.body_left(s) + PAD;
    let band = s.h - n.notch_h;
    let row_top = n.notch_h + (band - 20.0) / 2.0 - 3.0;
    let mut x = left;
    if waveform {
        let _: () = msg_send![&*n.bars, setFrame: rect(x, n.y(row_top, 20.0), 24.0, 20.0)];
        let _: () = msg_send![&*n.bars, setHidden: false];
        n.bars_animating(true, slow);
        x += 34.0;
    } else if let Some((name, tint)) = icon {
        let img = symbol(name, 17.0, 0.3);
        let _: () = msg_send![&*n.icon, setImage: img];
        let _: () = msg_send![&*n.icon, setContentTintColor: tint];
        let _: () = msg_send![&*n.icon, setFrame: rect(x, n.y(row_top, 20.0), 20.0, 20.0)];
        let _: () = msg_send![&*n.icon, setHidden: false];
        x += 32.0;
    }
    let w = n.body_left(s) + s.w - PAD - x;
    let _: () = msg_send![&*n.title, setStringValue: &*ns(text)];
    let _: () = msg_send![&*n.title, setMaximumNumberOfLines: 1isize];
    let _: () = msg_send![&*n.title, setLineBreakMode: 3usize]; // keep the newest words: truncate the head
    let _: () = msg_send![&*n.title, setFrame: rect(x, n.y(row_top, 20.0), w, 20.0)];
    let _: () = msg_send![&*n.title, setHidden: false];
}

fn line_shape(n: &Notch, w: f64) -> Shape {
    Shape { w: w.max(n.notch_w + 140.0), h: n.notch_h + 46.0, top_r: 12.0, bot_r: 23.0 }
}

/// Grow out of the notch and show what is being heard.
pub fn listening(text: &str) {
    with(|n| unsafe {
        let s = line_shape(n, 440.0);
        one_line(n, s, if text.is_empty() { "Listening…" } else { text }, true, false, None);
        n.morph(s, 0.32);
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
        let s = line_shape(n, 440.0);
        one_line(n, s, query, true, true, None);
        n.morph(s, 0.15);
    });
}

/// A command ran: a short confirmation with a check that pops.
pub fn done(text: &str) {
    with(|n| unsafe {
        let s = line_shape(n, 380.0);
        let ok = !text.starts_with("Couldn't");
        let tint: *mut AnyObject = if ok { msg_send![class!(NSColor), systemGreenColor] } else { msg_send![class!(NSColor), systemOrangeColor] };
        one_line(n, s, text, false, false, Some((if ok { "checkmark.circle.fill" } else { "exclamationmark.circle.fill" }, tint)));
        n.morph(s, 0.38);
        let seq = n.seq;
        main_after(130, move || {
            with(|n| unsafe {
                if n.seq == seq {
                    pop(&n.icon);
                }
            });
        });
    });
}

/// The answer: the source in the ear beside the camera, the answer below, then up to three
/// sources as pills to click.
pub fn answer(text: &str, source: &str, rows: &[(String, String)]) {
    with(|n| unsafe {
        n.clear();
        let w = 590.0;
        let inner = w - 2.0 * PAD;
        let left = (WIN_W - w) / 2.0 + PAD;
        // Measure the answer at the width it will wrap to.
        let _: () = msg_send![&*n.title, setMaximumNumberOfLines: 14isize];
        let _: () = msg_send![&*n.title, setLineBreakMode: 0usize];
        let _: () = msg_send![&*n.title, setStringValue: &*ns(text)];
        let cell: *mut AnyObject = msg_send![&*n.title, cell];
        let size: NSSize = msg_send![cell, cellSizeForBounds: rect(0.0, 0.0, inner, 2000.0)];
        let text_h = size.height.ceil().clamp(20.0, 300.0);

        let ear_w = (w - n.notch_w) / 2.0 - PAD - 10.0;
        if !source.is_empty() && ear_w > 40.0 {
            let _: () = msg_send![&*n.ear, setStringValue: &*ns(source)];
            let _: () = msg_send![&*n.ear, setFrame: rect(left + 2.0, n.y((n.notch_h - 15.0) / 2.0 + 1.0, 15.0), ear_w, 15.0)];
            let _: () = msg_send![&*n.ear, setHidden: false];
        }
        let top = n.notch_h + 12.0;
        let _: () = msg_send![&*n.title, setFrame: rect(left, n.y(top, text_h), inner, text_h)];
        let _: () = msg_send![&*n.title, setHidden: false];

        let mut y = top + text_h + 16.0;
        for (i, (label, url)) in rows.iter().take(3).enumerate() {
            let (p, t) = &n.pills[i];
            let fr = rect(left, n.y(y, 30.0), inner, 30.0);
            let _: () = msg_send![&**p, setFrame: fr];
            let _: () = msg_send![&**t, setFrame: rect(14.0, 7.0, inner - 28.0, 16.0)];
            let _: () = msg_send![&**t, setStringValue: &*ns(&format!("↗  {label}"))];
            let _: () = msg_send![&**p, setHidden: false];
            n.row_rects.push(fr);
            n.row_urls.push(url.clone());
            y += 36.0;
        }
        let h = if rows.is_empty() { y - 16.0 + 20.0 } else { y - 6.0 + 14.0 };
        // Room is left below for the spring's overshoot.
        n.morph(Shape { w, h: h.min(WIN_H - 36.0), top_r: 14.0, bot_r: 30.0 }, 0.26);
    });
}

/// Which source pill, if any, is under a click (window coordinates).
pub fn row_at(x: f64, y: f64) -> Option<String> {
    with(|n| {
        n.row_rects.iter().zip(n.row_urls.iter()).find_map(|(r, u)| {
            (x >= r.origin.x && x <= r.origin.x + r.size.width && y >= r.origin.y && y <= r.origin.y + r.size.height)
                .then(|| u.clone())
        })
    })
    .flatten()
}

/// Spring back into the camera housing, then take the window away.
pub fn collapse() {
    with(|n| unsafe {
        if !n.shown {
            return;
        }
        n.seq += 1;
        let seq = n.seq;
        fade(&n.content, 0.0, 0.12);
        n.bars_animating(false, false);
        let settle = spring_to(&n.shape, path(WIN_W / 2.0, 0.0, n.collapsed()), 0.38, 0.08);
        n.current = n.collapsed();
        main_after((settle * 1000.0) as u64 + 20, move || {
            if with(|n| n.seq == seq).unwrap_or(false) {
                hide();
            }
        });
    });
}

thread_local! {
    static PENDING: RefCell<Option<(u64, Box<dyn FnOnce()>)>> = const { RefCell::new(None) };
}

fn run_pending(seq: u64) {
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

/// Grow into the browser window's frame (AppleScript bounds: left, top, right, bottom from the
/// primary screen's top-left), run `then`, and fade away.
pub fn handoff(bounds: Option<(f64, f64, f64, f64)>, then: impl FnOnce() + 'static) {
    let then: Box<dyn FnOnce()> = Box::new(then);
    with(move |n| unsafe {
        n.show_window();
        n.seq += 1;
        let seq = n.seq;
        n.bars_animating(false, false);
        let _: () = msg_send![&*n.content, setAlphaValue: 0.0f64];
        // Cover the whole screen for the morph (instant: the window is transparent).
        let sf = n.screen;
        let _: () = msg_send![&*n.panel, setFrame: sf, display: false];
        layers_cover(&n.holder, &n.shape, sf.size.width, sf.size.height);
        set_path_now(&n.shape, path(sf.size.width / 2.0, 0.0, n.current));
        let (x, y, w, h) = match bounds {
            Some((l, t, r, b)) if r > l && b > t => (l - sf.origin.x, t - (n.primary_h - sf.origin.y - sf.size.height), r - l, b - t),
            _ => {
                let w = (sf.size.width * 0.82).min(1500.0);
                let h = (sf.size.height * 0.84).min(1000.0);
                ((sf.size.width - w) / 2.0, (sf.size.height - h) / 2.0, w, h)
            }
        };
        let settle = spring_to(&n.shape, window_path(x, y, w, h), 0.36, 0.05);
        PENDING.with(|p| *p.borrow_mut() = Some((seq, then)));
        main_after((settle * 600.0) as u64, move || {
            run_pending(seq);
            with(|n| unsafe {
                if n.seq == seq {
                    fade(&n.root, 0.0, 0.18);
                }
            });
            main_after(260, move || {
                if with(|n| n.seq == seq).unwrap_or(false) {
                    hide();
                }
            });
        });
    });
}

/// Take the window away entirely (nothing is drawn or animated while idle).
pub fn hide() {
    with(|n| unsafe {
        if n.shown {
            n.seq += 1;
            n.clear();
            let _: () = msg_send![&*n.panel, orderOut: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![&*n.root, setAlphaValue: 1.0f64];
            n.shown = false;
        }
    });
}
