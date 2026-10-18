//! Microphone capture with an AudioQueue on the Mac's own microphone, at 16 kHz mono. Every
//! buffer goes three ways: to the speech request while a question is being heard, into a short
//! pre-roll ring (so the first words after the wake word are not lost), and to the worker
//! thread that runs the wake word model and end-of-speech detection.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use objc2::rc::{autoreleasepool, Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use crate::config::{self, log};
use crate::objc::{main_after, main_async, Obj};
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

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

/// AudioStreamBasicDescription.
#[repr(C)]
struct StreamFormat {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

/// AudioQueueBuffer.
#[repr(C)]
struct QueueBuffer {
    capacity: u32,
    data: *mut c_void,
    size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_description_count: u32,
}

type InputCallback = extern "C" fn(*mut c_void, *mut c_void, *mut QueueBuffer, *const c_void, u32, *const c_void);

extern "C" {
    fn AudioObjectGetPropertyDataSize(id: u32, addr: *const PropertyAddress, qsize: u32, q: *const c_void, size: *mut u32) -> i32;
    fn AudioObjectGetPropertyData(id: u32, addr: *const PropertyAddress, qsize: u32, q: *const c_void, size: *mut u32, data: *mut c_void) -> i32;
    fn AudioQueueNewInput(format: *const StreamFormat, callback: InputCallback, user: *mut c_void, run_loop: *const c_void, mode: *const c_void, flags: u32, queue: *mut *mut c_void) -> i32;
    fn AudioQueueSetProperty(queue: *mut c_void, id: u32, data: *const c_void, size: u32) -> i32;
    fn AudioQueueAllocateBuffer(queue: *mut c_void, bytes: u32, buf: *mut *mut QueueBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(queue: *mut c_void, buf: *mut QueueBuffer, n: u32, descs: *const c_void) -> i32;
    fn AudioQueueStart(queue: *mut c_void, at: *const c_void) -> i32;
    fn AudioQueueDispose(queue: *mut c_void, immediate: u8) -> i32;
    fn CFRelease(cf: *const c_void);
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// The unique ID (a CFString, owned by the caller) of the Mac's own microphone. A Bluetooth
/// headset's microphone would drop the headset to call quality for as long as we listen, so
/// it is never chosen.
unsafe fn builtin_mic() -> Option<*const c_void> {
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
    let id = ids.into_iter().find(|&id| {
        let (mut kind, mut size, mut streams) = (0u32, 4u32, 0u32);
        AudioObjectGetPropertyData(id, &transport, 0, null, &mut size, &mut kind as *mut u32 as *mut c_void) == 0
            && kind == fourcc(b"bltn")
            && AudioObjectGetPropertyDataSize(id, &inputs, 0, null, &mut streams) == 0
            && streams > 0
    })?;
    let uid = PropertyAddress { selector: fourcc(b"uid "), scope: fourcc(b"glob"), element: 0 };
    let mut s: *const c_void = null;
    let mut size = std::mem::size_of::<*const c_void>() as u32;
    let ok = AudioObjectGetPropertyData(id, &uid, 0, null, &mut size, &mut s as *mut *const c_void as *mut c_void) == 0;
    (ok && !s.is_null()).then_some(s)
}

const RATE: f64 = 16000.0;
const BUFFER_FRAMES: u32 = 1600; // 100 ms

/// The running input queue (an AudioQueueRef), replaced by the watchdog if it stalls.
static QUEUE: Mutex<usize> = Mutex::new(0);
static TX: OnceLock<Sender<Vec<f32>>> = OnceLock::new();
/// 16 kHz mono float, the format of every buffer handed to the speech request.
static FORMAT: OnceLock<Obj> = OnceLock::new();
/// Buffers delivered so far; the watchdog restarts the queue when this stops moving.
static BUFFERS: AtomicU64 = AtomicU64::new(0);
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
    unsafe {
        let a: Allocated<AnyObject> = msg_send![class!(AVAudioFormat), alloc];
        let f: Option<Retained<AnyObject>> = msg_send![a, initStandardFormatWithSampleRate: RATE, channels: 1u32];
        let _ = FORMAT.set(Obj(f.expect("AVAudioFormat")));
        if run() {
            log("audio: listening");
        }
    }
    watch(BUFFERS.load(Ordering::Acquire));
}

fn watch(seen: u64) {
    main_after(WATCH_MS, move || {
        if BUFFERS.load(Ordering::Acquire) == seen {
            log("audio: no sound for 10 s, restarting the microphone");
            unsafe {
                if run() {
                    log("audio: listening");
                }
            }
        }
        watch(BUFFERS.load(Ordering::Acquire));
    });
}

/// Open the built-in microphone (or the default input if there is none) at 16 kHz mono,
/// closing any queue already open. The system converts the sample rate.
unsafe fn run() -> bool {
    let mut q = QUEUE.lock().unwrap();
    if *q != 0 {
        AudioQueueDispose(*q as *mut c_void, 1);
        *q = 0;
    }
    {
        let mut sh = SHARED.lock().unwrap();
        sh.ring.clear();
        sh.ring_frames = 0;
    }
    let format = StreamFormat {
        sample_rate: RATE,
        format_id: fourcc(b"lpcm"),
        format_flags: 1 | 8, // float, packed
        bytes_per_packet: 4,
        frames_per_packet: 1,
        bytes_per_frame: 4,
        channels_per_frame: 1,
        bits_per_channel: 32,
        reserved: 0,
    };
    let null = std::ptr::null();
    let mut queue: *mut c_void = std::ptr::null_mut();
    let st = AudioQueueNewInput(&format, on_input, std::ptr::null_mut(), null, null, 0, &mut queue);
    if st != 0 || queue.is_null() {
        log(format!("audio: could not open the microphone ({st})"));
        return false;
    }
    match builtin_mic() {
        Some(uid) => {
            let size = std::mem::size_of::<*const c_void>() as u32;
            if AudioQueueSetProperty(queue, fourcc(b"aqcd"), &uid as *const *const c_void as *const c_void, size) != 0 {
                log("audio: could not select the built-in microphone");
            }
            CFRelease(uid);
        }
        None => log("audio: no built-in microphone, using the default input"),
    }
    for _ in 0..3 {
        let mut buf: *mut QueueBuffer = std::ptr::null_mut();
        if AudioQueueAllocateBuffer(queue, BUFFER_FRAMES * 4, &mut buf) == 0 {
            AudioQueueEnqueueBuffer(queue, buf, 0, null);
        }
    }
    let st = AudioQueueStart(queue, null);
    if st != 0 {
        log(format!("audio: microphone failed to start ({st})"));
        AudioQueueDispose(queue, 1);
        return false;
    }
    *q = queue as usize;
    true
}

/// Runs on the queue's own thread for every 100 ms of audio.
extern "C" fn on_input(_user: *mut c_void, queue: *mut c_void, buf: *mut QueueBuffer, _t: *const c_void, _n: u32, _d: *const c_void) {
    unsafe {
        let b = &*buf;
        let frames = b.size / 4;
        if frames > 0 && !b.data.is_null() {
            let samples = std::slice::from_raw_parts(b.data as *const f32, frames as usize);
            autoreleasepool(|_| deliver(samples));
        }
        AudioQueueEnqueueBuffer(queue, buf, 0, std::ptr::null());
    }
}

/// Hand 16 kHz samples to the speech request, the pre-roll ring and the wake word worker.
unsafe fn deliver(samples: &[f32]) {
    BUFFERS.fetch_add(1, Ordering::Release);
    let frames = samples.len() as u32;
    let Some(fmt) = FORMAT.get() else { return };
    let a: Allocated<AnyObject> = msg_send![class!(AVAudioPCMBuffer), alloc];
    let pcm: Option<Retained<AnyObject>> = msg_send![a, initWithPCMFormat: &*fmt.0, frameCapacity: frames];
    if let Some(pcm) = pcm {
        let _: () = msg_send![&*pcm, setFrameLength: frames];
        let dst: *const *mut f32 = msg_send![&*pcm, floatChannelData];
        if !dst.is_null() {
            std::ptr::copy_nonoverlapping(samples.as_ptr(), *dst, samples.len());
            let mut sh = SHARED.lock().unwrap();
            if let Some(t) = &sh.target {
                let _: () = msg_send![&*t.0, appendAudioPCMBuffer: &*pcm];
            }
            sh.ring.push_back((Obj(pcm), frames));
            sh.ring_frames += frames as u64;
            while sh.ring_frames > (RATE * PREROLL_S) as u64 {
                match sh.ring.pop_front() {
                    Some((_, n)) => sh.ring_frames -= n as u64,
                    None => break,
                }
            }
        }
    }
    if let Some(tx) = TX.get() {
        let _ = tx_send(tx, samples.iter().map(|x| x * 32767.0).collect());
    }
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
