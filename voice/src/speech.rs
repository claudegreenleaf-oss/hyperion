//! The question after the wake word, transcribed by Apple's on-device recognizer (Speech
//! framework). No model ships with the app and audio never leaves the Mac.

use std::cell::RefCell;

use block2::RcBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send, sel};

use crate::audio;
use crate::config::log;
use crate::ctl;
use crate::objc::{main_async, ns, string};

const VOCAB: &[&str] = &[
    "REA", "Restrictive Early Action", "Early Action", "Early Decision", "Regular Decision", "supplemental",
    "supplemental essays", "Common App", "FAFSA", "CSS Profile", "AP Physics C", "AP Bio", "Canvas", "Naviance",
    "SAT", "ACT", "Harvard", "Stanford", "Yale", "MIT", "Princeton", "Menlo-Atherton", "M-A Chronicle",
    "SCOTUS", "Hyperion", "Helium", "Karabiner", "AeroSpace", "GitHub", "Tailscale", "Claude", "Anthropic",
];

struct Rec {
    recognizer: Retained<AnyObject>,
    request: Option<Retained<AnyObject>>,
    task: Option<Retained<AnyObject>>,
}

thread_local! {
    static REC: RefCell<Option<Rec>> = const { RefCell::new(None) };
}

/// Ask for speech recognition permission, then create the recognizer; reports to
/// ctl::on_ready. Main thread.
pub fn init() {
    let block = RcBlock::new(move |status: isize| {
        // SFSpeechRecognizerAuthorizationStatusAuthorized == 3. The handler may arrive on any
        // queue; the recognizer is made on main.
        let ok = status == 3;
        main_async(move || create(ok));
    });
    unsafe {
        let _: () = msg_send![class!(SFSpeechRecognizer), requestAuthorization: &*block];
    }
}

fn create(authorized: bool) {
    if !authorized {
        log("speech: permission denied");
        ctl::on_ready(false);
        return;
    }
    unsafe {
        let loc: *mut AnyObject = msg_send![class!(NSLocale), localeWithLocaleIdentifier: &*ns("en-US")];
        let a: Allocated<AnyObject> = msg_send![class!(SFSpeechRecognizer), alloc];
        let r: Option<Retained<AnyObject>> = msg_send![a, initWithLocale: loc];
        let Some(recognizer) = r else {
            log("speech: no recognizer for en-US");
            ctl::on_ready(false);
            return;
        };
        let on_device: bool = msg_send![&*recognizer, supportsOnDeviceRecognition];
        log(format!("speech: ready, on-device {on_device}"));
        REC.with(|r| *r.borrow_mut() = Some(Rec { recognizer, request: None, task: None }));
    }
    ctl::on_ready(true);
}

/// Start transcribing, pre-roll first. Results arrive as ctl::on_speech(gen, text, final).
pub fn start(gen: u64) {
    REC.with(|r| {
        let mut r = r.borrow_mut();
        let Some(rec) = r.as_mut() else { return };
        unsafe {
            if let Some(t) = rec.task.take() {
                let _: () = msg_send![&*t, cancel];
            }
            let req: Retained<AnyObject> = msg_send![class!(SFSpeechAudioBufferRecognitionRequest), new];
            let _: () = msg_send![&*req, setShouldReportPartialResults: true];
            let on_device: bool = msg_send![&*rec.recognizer, supportsOnDeviceRecognition];
            if on_device {
                let _: () = msg_send![&*req, setRequiresOnDeviceRecognition: true];
            }
            let _: () = msg_send![&*req, setTaskHint: 2isize]; // SFSpeechRecognitionTaskHintSearch
            // Bias recognition toward words Thomas actually searches for.
            let words: Retained<AnyObject> = msg_send![class!(NSMutableArray), new];
            for w in VOCAB.iter().map(|s| s.to_string()).chain(crate::config::get().vocabulary.iter().cloned()) {
                let _: () = msg_send![&*words, addObject: &*ns(&w)];
            }
            let _: () = msg_send![&*req, setContextualStrings: &*words];
            let responds: bool = msg_send![&*req, respondsToSelector: sel!(setAddsPunctuation:)];
            if responds {
                let _: () = msg_send![&*req, setAddsPunctuation: false];
            }
            let handler = RcBlock::new(move |result: *mut AnyObject, error: *mut AnyObject| {
                if !result.is_null() {
                    let tr: *mut AnyObject = msg_send![result, bestTranscription];
                    let s: *mut AnyObject = msg_send![tr, formattedString];
                    let text = string(s);
                    let fin: bool = msg_send![result, isFinal];
                    main_async(move || ctl::on_speech(gen, text, fin));
                } else if !error.is_null() {
                    main_async(move || ctl::on_speech_error(gen));
                }
            });
            let task: Option<Retained<AnyObject>> =
                msg_send![&*rec.recognizer, recognitionTaskWithRequest: &*req, resultHandler: &*handler];
            audio::attach(&req);
            rec.request = Some(req);
            rec.task = task;
        }
    });
}

/// The question is over: stop feeding audio and let the recognizer finish.
pub fn finish() {
    audio::detach();
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            if let Some(req) = rec.request.take() {
                unsafe {
                    let _: () = msg_send![&*req, endAudio];
                }
            }
        }
    });
}

pub fn cancel() {
    audio::detach();
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            rec.request = None;
            if let Some(t) = rec.task.take() {
                unsafe {
                    let _: () = msg_send![&*t, cancel];
                }
            }
        }
    });
}
