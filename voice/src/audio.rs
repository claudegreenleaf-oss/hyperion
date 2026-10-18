//! Microphone capture with AVAudioEngine. Every buffer goes three ways: to the speech request
//! while a question is being heard, into a short pre-roll ring (so the first words after the
//! wake word are not lost), and, resampled to 16 kHz, to the worker thread that runs the wake
//! word model and end-of-speech detection.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use crate::config::{self, log};
use crate::objc::{main_after, main_async, ns, Obj};
use crate::{ctl, wake};

/// What the listener is doing, shared with the audio threads.
pub const IDLE: u8 = 0;
pub const LISTENING: u8 = 1;
pub const BUSY: u8 = 2;
pub static PHASE: AtomicU8 = AtomicU8::new(IDLE);

const PREROLL_S: f64 = 1.2;
const HISTORY_FRAMES: usize = 20; // 1.6 s of 80 ms frames, replayed when sound starts
const GATE_DB: f32 = 9.0; // above the noise floor counts as sound

struct Shared {
    ring: VecDeque<(Obj, u32)>,
    ring_frames: u64,
    target: Option<Obj>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { ring: VecDeque::new(), ring_frames: 0, target: None });

/// Start sending the microphone to `request` (an SFSpeechAudioBufferRecognitionRequest),
/// beginning with the pre-roll so nothing said right after the wake word is lost.
pub fn attach(request: &AnyObject) {
    let mut sh = SHARED.lock().unwrap();
    for (buf, _) in sh.ring.iter() {
        unsafe {
            let _: () = msg_send![request, appendAudioPCMBuffer: &*buf.0];
        }
    }
    sh.target = unsafe { Retained::retain(request as *const AnyObject as *mut AnyObject) }.map(Obj);
}

pub fn detach() {
    SHARED.lock().unwrap().target = None;
}

struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let last = input.len() as f64 - 1.0;
        while self.pos < last {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let a = if i < 0.0 { self.prev } else { input[i as usize] };
            let b = input[(i + 1.0) as usize];
            out.push((a + (b - a) * frac) * 32767.0);
            self.pos += self.step;
        }
        self.pos -= input.len() as f64;
        self.prev = input[input.len() - 1];
    }
}

unsafe fn copy_buffer(buf: *mut AnyObject, frames: u32) -> Option<Retained<AnyObject>> {
    let fmt: *mut AnyObject = msg_send![buf, format];
    let channels: u32 = msg_send![fmt, channelCount];
    let a: Allocated<AnyObject> = msg_send![class!(AVAudioPCMBuffer), alloc];
    let copy: Option<Retained<AnyObject>> = msg_send![a, initWithPCMFormat: fmt, frameCapacity: frames];
    let copy = copy?;
    let _: () = msg_send![&*copy, setFrameLength: frames];
    let src: *const *mut f32 = msg_send![buf, floatChannelData];
    let dst: *const *mut f32 = msg_send![&*copy, floatChannelData];
    if src.is_null() || dst.is_null() {
        return None;
    }
    for c in 0..channels as usize {
        std::ptr::copy_nonoverlapping(*src.add(c), *dst.add(c), frames as usize);
    }
    Some(copy)
}

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

extern "C" {
    fn AudioObjectGetPropertyDataSize(id: u32, addr: *const PropertyAddress, qsize: u32, q: *const c_void, size: *mut u32) -> i32;
    fn AudioObjectGetPropertyData(id: u32, addr: *const PropertyAddress, qsize: u32, q: *const c_void, size: *mut u32, data: *mut c_void) -> i32;
    fn AudioUnitSetProperty(unit: *mut c_void, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// The Mac's own microphone. A Bluetooth headset's microphone would drop the headset to call
/// quality for as long as we listen, so it is never used.
unsafe fn builtin_mic() -> Option<u32> {
    let null = std::ptr::null();
    let devices = PropertyAddress { selector: fourcc(b"dev#"), scope: fourcc(b"glob"), element: 0 };
    let mut size = 0u32;
    if AudioObjectGetPropertyDataSize(1, &devices, 0, null, &mut size) != 0 {
        return None;
    }
    let mut ids = vec![0u32; size as usize / 4];
    if AudioObjectGetPropertyData(1, &devices, 0, null, &mut size, ids.as_mut_ptr() as *mut c_void) != 0 {
        return None;
    }
    let transport = PropertyAddress { selector: fourcc(b"tran"), scope: fourcc(b"glob"), element: 0 };
    let inputs = PropertyAddress { selector: fourcc(b"stm#"), scope: fourcc(b"inpt"), element: 0 };
    ids.into_iter().find(|&id| {
        let (mut kind, mut size, mut streams) = (0u32, 4u32, 0u32);
        AudioObjectGetPropertyData(id, &transport, 0, null, &mut size, &mut kind as *mut u32 as *mut c_void) == 0
            && kind == fourcc(b"bltn")
            && AudioObjectGetPropertyDataSize(id, &inputs, 0, null, &mut streams) == 0
            && streams > 0
    })
}

/// The running engine; replaced whenever the audio hardware changes under it.
static ENGINE: Mutex<Option<Obj>> = Mutex::new(None);
static TX: OnceLock<Sender<Vec<f32>>> = OnceLock::new();
/// Buffers delivered so far; the watchdog restarts the engine when this stops moving.
static BUFFERS: AtomicU64 = AtomicU64::new(0);
static CHANGES: AtomicU64 = AtomicU64::new(0);
const WATCH_MS: u64 = 10_000;

/// Start the wake word worker and the microphone. Call on the main thread after mic permission.
/// If the microphone cannot start yet, the watchdog keeps trying.
pub fn start() {
    let (tx, rx) = channel::<Vec<f32>>();
    std::thread::Builder::new()
        .name("wake".into())
        .spawn(move || worker(rx))
        .expect("spawn wake worker");
    let _ = TX.set(tx);
    // A device connecting or disconnecting stops the engine; start a new one once it settles.
    let changed = RcBlock::new(|_note: *mut AnyObject| {
        let n = CHANGES.fetch_add(1, Ordering::AcqRel) + 1;
        main_after(500, move || {
            if CHANGES.load(Ordering::Acquire) == n {
                restart("the audio hardware changed");
            }
        });
    });
    unsafe {
        let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        let queue: *mut AnyObject = msg_send![class!(NSOperationQueue), mainQueue];
        let name = ns("AVAudioEngineConfigurationChangeNotification");
        let nil: *mut AnyObject = std::ptr::null_mut();
        let _: *mut AnyObject = msg_send![center, addObserverForName: &*name, object: nil, queue: queue, usingBlock: &*changed];
    }
    std::mem::forget(changed);
    unsafe { run() };
    watch(BUFFERS.load(Ordering::Acquire));
}

fn watch(seen: u64) {
    main_after(WATCH_MS, move || {
        if BUFFERS.load(Ordering::Acquire) == seen {
            restart("no audio for 10 s");
        }
        watch(BUFFERS.load(Ordering::Acquire));
    });
}

fn restart(why: &str) {
    log(format!("audio: restarting, {why}"));
    unsafe {
        if let Some(old) = ENGINE.lock().unwrap().take() {
            let input: *mut AnyObject = msg_send![&*old.0, inputNode];
            let _: () = msg_send![input, removeTapOnBus: 0usize];
            let _: () = msg_send![&*old.0, stop];
        }
        run();
    }
}

/// Build an engine on the built-in microphone, install the tap and start it.
unsafe fn run() -> bool {
    let engine: Retained<AnyObject> = msg_send![class!(AVAudioEngine), new];
    let input: *mut AnyObject = msg_send![&*engine, inputNode];
    let mic = builtin_mic();
    if let Some(id) = mic {
        let unit: *mut c_void = msg_send![input, audioUnit];
        // kAudioOutputUnitProperty_CurrentDevice, global scope.
        if unit.is_null() || AudioUnitSetProperty(unit, 2000, 0, 0, &id as *const u32 as *const c_void, 4) != 0 {
            log("audio: could not select the built-in microphone");
        }
    }
    let fmt: *mut AnyObject = msg_send![input, inputFormatForBus: 0usize];
    let sr: f64 = msg_send![fmt, sampleRate];
    if sr <= 0.0 {
        log("audio: input has no sample rate (no microphone?)");
        return false;
    }
    log(format!("audio: input {sr} Hz{}", if mic.is_some() { ", built-in microphone" } else { "" }));
    {
        let mut sh = SHARED.lock().unwrap();
        sh.ring.clear();
        sh.ring_frames = 0;
    }

    let tx = TX.get().expect("audio::start").clone();
    // The tap reads the rate from each buffer, so a device change cannot leave it stale.
    let resampler = Mutex::new(Resampler { step: 0.0, pos: 0.0, prev: 0.0 });
    let tap = RcBlock::new(move |buf: *mut AnyObject, _when: *mut AnyObject| {
        if buf.is_null() {
            return;
        }
        let frames: u32 = msg_send![buf, frameLength];
        let data: *const *mut f32 = msg_send![buf, floatChannelData];
        if frames == 0 || data.is_null() {
            return;
        }
        BUFFERS.fetch_add(1, Ordering::Release);
        let fmt: *mut AnyObject = msg_send![buf, format];
        let sr: f64 = msg_send![fmt, sampleRate];
        {
            let mut sh = SHARED.lock().unwrap();
            if let Some(t) = &sh.target {
                let _: () = msg_send![&*t.0, appendAudioPCMBuffer: buf];
            }
            if let Some(c) = copy_buffer(buf, frames) {
                sh.ring.push_back((Obj(c), frames));
                sh.ring_frames += frames as u64;
                while sh.ring_frames > (sr * PREROLL_S) as u64 {
                    match sh.ring.pop_front() {
                        Some((_, n)) => sh.ring_frames -= n as u64,
                        None => break,
                    }
                }
            }
        }
        let ch0 = std::slice::from_raw_parts(*data, frames as usize);
        let mut out = Vec::with_capacity(frames as usize / 2);
        let mut r = resampler.lock().unwrap();
        if r.step != sr / 16000.0 {
            *r = Resampler { step: sr / 16000.0, pos: 0.0, prev: 0.0 };
        }
        r.process(ch0, &mut out);
        let _ = tx_send(&tx, out);
    });
    let nil: *mut AnyObject = std::ptr::null_mut();
    let _: () = msg_send![input, installTapOnBus: 0usize, bufferSize: 4096u32, format: nil, block: &*tap];
    let _: () = msg_send![&*engine, prepare];
    let mut err: *mut AnyObject = std::ptr::null_mut();
    let ok: bool = msg_send![&*engine, startAndReturnError: &mut err];
    // The engine keeps the tap block alive until the tap is removed.
    drop(tap);
    if !ok {
        log("audio: engine failed to start");
        return false;
    }
    *ENGINE.lock().unwrap() = Some(Obj(engine));
    true
}

fn tx_send(tx: &Sender<Vec<f32>>, v: Vec<f32>) -> Result<(), ()> {
    tx.send(v).map_err(|_| ())
}

/// Wake word and end-of-speech detection on 80 ms frames.
fn worker(rx: Receiver<Vec<f32>>) {
    let cfg = config::get();
    let dir = crate::resources().join("models");
    let user = config::home().join(".config/hyperion-voice/models").join(&cfg.wake_model);
    let wake_path = if user.exists() { user } else { dir.join(&cfg.wake_model) };
    log(format!("wake: model {}", wake_path.display()));
    let mut det = match wake::Detector::new(&dir, &wake_path) {
        Ok(d) => d,
        Err(e) => {
            log(format!("wake: model load failed: {e}"));
            return;
        }
    };
    log("wake: ready");
    let mut buf: Vec<f32> = Vec::with_capacity(wake::FRAME * 4);
    // The models only run while there is sound: a quiet room costs nothing beyond the mic.
    // When sound starts, the last 1.6 s are replayed so detection sees full context.
    let mut history: VecDeque<Vec<f32>> = VecDeque::with_capacity(HISTORY_FRAMES + 1);
    let mut active_until = Instant::now();
    let mut active = false;
    let mut floor_db = -55.0f32;
    let mut listen = Listen::default();
    let mut cooldown_until = Instant::now();
    let mut last_phase = IDLE;

    while let Ok(chunk) = rx.recv() {
        buf.extend_from_slice(&chunk);
        while buf.len() >= wake::FRAME {
            let frame: Vec<f32> = buf.drain(..wake::FRAME).collect();
            let rms = (frame.iter().map(|x| x * x).sum::<f32>() / frame.len() as f32).sqrt();
            let db = 20.0 * (rms / 32768.0 + 1e-9).log10();
            let phase = PHASE.load(Ordering::Acquire);
            if phase != last_phase {
                if phase == LISTENING {
                    listen = Listen { started: Some(Instant::now()), ..Default::default() };
                }
                if phase == IDLE {
                    det.reset_scores();
                    cooldown_until = Instant::now() + std::time::Duration::from_millis(1500);
                }
                last_phase = phase;
            }

            if phase == IDLE {
                // Track the room's noise floor while nobody is talking to us.
                if db < floor_db + 6.0 {
                    floor_db = 0.95 * floor_db + 0.05 * db;
                } else {
                    floor_db += 0.002;
                }
            }

            let now = Instant::now();
            if phase == IDLE && db > floor_db + GATE_DB {
                active_until = now + std::time::Duration::from_millis(2500);
            }
            let was_active = active;
            active = phase == IDLE && now < active_until;
            let mut score = 0.0f32;
            if active {
                if !was_active {
                    det.restart();
                    for old in history.iter() {
                        score = score.max(det.push(old).unwrap_or(0.0));
                    }
                }
                score = score.max(match det.push(&frame) {
                    Ok(s) => s,
                    Err(e) => {
                        log(format!("wake: inference error: {e}"));
                        0.0
                    }
                });
            }
            history.push_back(frame.clone());
            if history.len() > HISTORY_FRAMES {
                history.pop_front();
            }
            if score > 0.3 {
                log(format!("wake: score {score:.2} phase {phase}"));
            }
            if phase == IDLE && score >= cfg.threshold && Instant::now() >= cooldown_until {
                if PHASE.compare_exchange(IDLE, LISTENING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    last_phase = LISTENING;
                    listen = Listen { started: Some(Instant::now()), ..Default::default() };
                    main_async(ctl::on_wake);
                }
            } else if phase == LISTENING {
                if let Some(end) = listen.frame(db, floor_db, cfg.silence_ms) {
                    PHASE.store(BUSY, Ordering::Release);
                    last_phase = BUSY;
                    main_async(move || ctl::on_end_of_speech(end));
                }
            }
        }
    }
}

#[derive(Default)]
struct Listen {
    started: Option<Instant>,
    loud_run: u32,
    speech: bool,
    silent_ms: u64,
}

impl Listen {
    /// Returns Some(heard_speech) when the question is over.
    fn frame(&mut self, db: f32, floor_db: f32, silence_ms: u64) -> Option<bool> {
        let threshold = (floor_db + 12.0).max(-50.0);
        if db > threshold {
            self.loud_run += 1;
            if self.loud_run >= 2 {
                self.speech = true;
            }
            self.silent_ms = 0;
        } else {
            self.loud_run = 0;
            self.silent_ms += 80;
        }
        let elapsed = self.started.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
        if self.speech && self.silent_ms >= silence_ms {
            return Some(true);
        }
        if !self.speech && elapsed > 4000 {
            return Some(false);
        }
        if elapsed > 12000 {
            return Some(self.speech);
        }
        None
    }
}
