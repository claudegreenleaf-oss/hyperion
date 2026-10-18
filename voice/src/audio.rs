//! Microphone capture with AVAudioEngine. Every buffer goes three ways: to the speech request
//! while a question is being heard, into a short pre-roll ring (so the first words after the
//! wake word are not lost), and, resampled to 16 kHz, to the worker thread that runs the wake
//! word model and end-of-speech detection.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use crate::config::{self, log};
use crate::objc::{main_async, Obj};
use crate::{ctl, wake};

/// What the listener is doing, shared with the audio threads.
pub const IDLE: u8 = 0;
pub const LISTENING: u8 = 1;
pub const BUSY: u8 = 2;
pub static PHASE: AtomicU8 = AtomicU8::new(IDLE);

const PREROLL_S: f64 = 1.2;

struct Shared {
    ring: VecDeque<(Obj, u32)>,
    ring_frames: u64,
    target: Option<Obj>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { ring: VecDeque::new(), ring_frames: 0, target: None });
static SAMPLE_RATE: Mutex<f64> = Mutex::new(48000.0);

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

/// Install the tap and start the engine. Call on the main thread after mic permission.
pub fn start() -> bool {
    unsafe {
        let engine: Retained<AnyObject> = msg_send![class!(AVAudioEngine), new];
        let input: *mut AnyObject = msg_send![&*engine, inputNode];
        let fmt: *mut AnyObject = msg_send![input, outputFormatForBus: 0usize];
        let sr: f64 = msg_send![fmt, sampleRate];
        if sr <= 0.0 {
            log("audio: input has no sample rate (no microphone?)");
            return false;
        }
        *SAMPLE_RATE.lock().unwrap() = sr;
        log(format!("audio: input {sr} Hz"));

        let (tx, rx) = channel::<Vec<f32>>();
        std::thread::Builder::new()
            .name("wake".into())
            .spawn(move || worker(rx))
            .expect("spawn wake worker");

        let resampler = Mutex::new(Resampler { step: sr / 16000.0, pos: 0.0, prev: 0.0 });
        let preroll_frames = (sr * PREROLL_S) as u64;
        let tap = RcBlock::new(move |buf: *mut AnyObject, _when: *mut AnyObject| {
            if buf.is_null() {
                return;
            }
            let frames: u32 = msg_send![buf, frameLength];
            let data: *const *mut f32 = msg_send![buf, floatChannelData];
            if frames == 0 || data.is_null() {
                return;
            }
            {
                let mut sh = SHARED.lock().unwrap();
                if let Some(t) = &sh.target {
                    let _: () = msg_send![&*t.0, appendAudioPCMBuffer: buf];
                }
                if let Some(c) = copy_buffer(buf, frames) {
                    sh.ring.push_back((Obj(c), frames));
                    sh.ring_frames += frames as u64;
                    while sh.ring_frames > preroll_frames {
                        match sh.ring.pop_front() {
                            Some((_, n)) => sh.ring_frames -= n as u64,
                            None => break,
                        }
                    }
                }
            }
            let ch0 = std::slice::from_raw_parts(*data, frames as usize);
            let mut out = Vec::with_capacity(frames as usize / 2);
            resampler.lock().unwrap().process(ch0, &mut out);
            let _ = tx_send(&tx, out);
        });
        let _: () = msg_send![input, installTapOnBus: 0usize, bufferSize: 4096u32, format: fmt, block: &*tap];
        let _: () = msg_send![&*engine, prepare];
        let mut err: *mut AnyObject = std::ptr::null_mut();
        let ok: bool = msg_send![&*engine, startAndReturnError: &mut err];
        if !ok {
            log("audio: engine failed to start");
            return false;
        }
        // The engine and tap live for the whole process.
        std::mem::forget(engine);
        std::mem::forget(tap);
        true
    }
}

fn tx_send(tx: &Sender<Vec<f32>>, v: Vec<f32>) -> Result<(), ()> {
    tx.send(v).map_err(|_| ())
}

/// Wake word and end-of-speech detection on 80 ms frames.
fn worker(rx: Receiver<Vec<f32>>) {
    let cfg = config::get();
    let dir = crate::resources().join("models");
    let mut det = match wake::Detector::new(&dir, &cfg.wake_model) {
        Ok(d) => d,
        Err(e) => {
            log(format!("wake: model load failed: {e}"));
            return;
        }
    };
    log("wake: ready");
    let mut buf: Vec<f32> = Vec::with_capacity(wake::FRAME * 4);
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

            let score = match det.push(&frame) {
                Ok(s) => s,
                Err(e) => {
                    log(format!("wake: inference error: {e}"));
                    0.0
                }
            };
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
