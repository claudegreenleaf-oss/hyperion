//! The listener's state machine. Everything here runs on the main thread.
//!
//! Idle -> (wake word) Listening -> (silence) Finishing -> (final transcript) Thinking ->
//!   short answer: Showing in the notch, then back to Idle
//!   anything else, or no answer in time: the notch grows into the browser, which comes forward.

use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::time::Instant;

use serde_json::Value;

use crate::audio::{BUSY, IDLE, LISTENING, PHASE};
use crate::config::{self, log};
use crate::objc::main_after;
use crate::{browser, notch, relay, speech};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Starting,
    Idle,
    Listening,
    Finishing,
    Thinking,
    Showing,
}

struct Ctl {
    phase: Phase,
    gen: u64,
    heard: String,
    query: String,
    woke_at: Instant,
    asked_at: Instant,
    prefetched: Vec<String>,
    last_prefetch: Instant,
    bounds: Option<(f64, f64, f64, f64)>,
    passage: Option<(String, String)>,
    search_q: String,
    sources: Vec<(String, String)>,
    decided: bool,
}

thread_local! {
    static C: RefCell<Ctl> = RefCell::new(Ctl {
        phase: Phase::Starting,
        gen: 0,
        heard: String::new(),
        query: String::new(),
        woke_at: Instant::now(),
        asked_at: Instant::now(),
        prefetched: Vec::new(),
        last_prefetch: Instant::now(),
        bounds: None,
        passage: None,
        search_q: String::new(),
        sources: Vec::new(),
        decided: false,
    });
}

fn with<R>(f: impl FnOnce(&mut Ctl) -> R) -> R {
    C.with(|c| f(&mut c.borrow_mut()))
}

fn set_phase(c: &mut Ctl, p: Phase) {
    c.phase = p;
    PHASE.store(
        match p {
            Phase::Idle | Phase::Starting => IDLE,
            Phase::Listening => LISTENING,
            _ => BUSY,
        },
        Ordering::Release,
    );
}

/// Strip the wake word the pre-roll caught ("hey jarvis, when is ..." -> "when is ...").
fn strip_wake(t: &str) -> String {
    let mut s = t.trim().to_string();
    loop {
        let lower = s.to_lowercase();
        let Some(w) = ["hey jarvis", "hi jarvis", "a jarvis", "jarvis", "hey", "hi"].iter().find(|w| {
            lower.starts_with(*w) && !lower[w.len()..].starts_with(|ch: char| ch.is_alphanumeric())
        }) else {
            break;
        };
        s = s[w.len()..].trim_start_matches([',', '.', ' ', '!', '?']).to_string();
    }
    s.trim().to_string()
}

pub fn on_ready(ok: bool) {
    if !ok {
        log("ctl: not ready (speech permission or recognizer missing)");
        return;
    }
    if !crate::audio::start() {
        return;
    }
    with(|c| set_phase(c, Phase::Idle));
    log("ctl: listening for the wake word");
}

pub fn on_wake() {
    let gen = with(|c| {
        if c.phase != Phase::Idle && c.phase != Phase::Showing {
            // Keep the audio threads in step with what we are really doing.
            let p = c.phase;
            set_phase(c, p);
            return None;
        }
        c.gen += 1;
        c.heard.clear();
        c.query.clear();
        c.prefetched.clear();
        c.passage = None;
        c.search_q.clear();
        c.sources.clear();
        c.bounds = None;
        c.decided = false;
        c.woke_at = Instant::now();
        set_phase(c, Phase::Listening);
        Some(c.gen)
    });
    let Some(gen) = gen else { return };
    log(format!("ctl: wake (gen {gen})"));
    notch::listening("");
    speech::start(gen);
    // Safety net: never listen forever even if end-of-speech detection misses.
    main_after(15000, move || {
        if with(|c| c.gen == gen && c.phase == Phase::Listening) {
            on_end_of_speech(true);
        }
    });
}

pub fn on_speech(gen: u64, text: String, fin: bool) {
    let text = strip_wake(&text);
    let act = with(|c| {
        if c.gen != gen {
            return None;
        }
        if !text.is_empty() {
            c.heard = text.clone();
        }
        Some((c.phase, c.heard.clone()))
    });
    let Some((phase, heard)) = act else { return };
    match phase {
        Phase::Listening if !fin => {
            if !heard.is_empty() {
                notch::transcript(&heard);
                maybe_prefetch(&heard);
            }
        }
        Phase::Listening | Phase::Finishing if fin => ask(gen),
        _ => {}
    }
}

fn maybe_prefetch(heard: &str) {
    let words = heard.split_whitespace().count();
    let go = with(|c| {
        if words < 3 || c.last_prefetch.elapsed().as_millis() < 350 || c.prefetched.iter().any(|p| p == heard) {
            return false;
        }
        c.last_prefetch = Instant::now();
        c.prefetched.push(heard.to_string());
        true
    });
    if go {
        relay::prefetch(heard.to_string());
    }
}

pub fn on_speech_error(gen: u64) {
    let phase = with(|c| if c.gen == gen { Some(c.phase) } else { None });
    if matches!(phase, Some(Phase::Listening) | Some(Phase::Finishing)) {
        ask(gen);
    }
}

/// The worker heard silence after speech (or nothing at all).
pub fn on_end_of_speech(heard_speech: bool) {
    let gen = with(|c| {
        if c.phase != Phase::Listening {
            let p = c.phase;
            set_phase(c, p);
            return None;
        }
        set_phase(c, Phase::Finishing);
        Some(c.gen)
    });
    let Some(gen) = gen else { return };
    if !heard_speech && with(|c| c.heard.is_empty()) {
        log("ctl: nothing said after the wake word");
        speech::cancel();
        reset(gen);
        return;
    }
    speech::finish();
    // The final transcript normally lands within a few hundred ms; don't wait longer than this.
    main_after(900, move || {
        if with(|c| c.gen == gen && c.phase == Phase::Finishing) {
            ask(gen);
        }
    });
}

fn ask(gen: u64) {
    let q = with(|c| {
        if c.gen != gen || !matches!(c.phase, Phase::Listening | Phase::Finishing) {
            return None;
        }
        set_phase(c, Phase::Thinking);
        c.query = c.heard.clone();
        c.asked_at = Instant::now();
        Some(c.query.clone())
    });
    let Some(q) = q else { return };
    speech::cancel();
    if q.is_empty() {
        reset(gen);
        return;
    }
    let spoke_ms = with(|c| c.woke_at.elapsed().as_millis());
    log(format!("ctl: asking {q:?} ({spoke_ms} ms after wake)"));
    notch::thinking(&q);
    relay::ask(q, on_relay, gen);
    let cfg = config::get();
    let bundle = cfg.browser_bundle.clone();
    std::thread::spawn(move || {
        let b = browser::front_window_bounds(&bundle);
        crate::objc::main_async(move || with(|c| if c.gen == gen { c.bounds = b }));
    });
    main_after(cfg.answer_budget_ms, move || decide_timeout(gen));
}

fn on_relay(gen: u64, ev: Value) {
    if with(|c| c.gen != gen || c.phase != Phase::Thinking || c.decided) {
        return;
    }
    let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
    let s = |k: &str| ev.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match ty {
        "passage" => with(|c| c.passage = Some((s("text"), s("host")))),
        "query" => with(|c| c.search_q = s("q")),
        "sources" => {
            let rows: Vec<(String, String)> = ev
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(3)
                .map(|r| {
                    let g = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    (format!("{}  ·  {}", g("host"), g("title")), g("url"))
                })
                .collect();
            with(|c| c.sources = rows);
        }
        "open" => {
            log(format!("ctl: open {} ({})", s("url"), s("label")));
            open_url(gen, s("url"));
        }
        "command" => {
            let id = s("id");
            let done = s("done");
            let args: std::collections::HashMap<String, String> = ev
                .get("args")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect())
                .unwrap_or_default();
            with(|c| c.decided = true);
            std::thread::spawn(move || {
                let res = crate::commands::run(&id, &args);
                crate::objc::main_async(move || match res {
                    Ok(()) => finish_with(gen, &done, 1600),
                    Err(e) => {
                        log(format!("ctl: command failed: {e}"));
                        finish_with(gen, &format!("Couldn't: {e}"), 3000);
                    }
                });
            });
        }
        "answer" => {
            let easy = ev.get("easy").and_then(Value::as_bool).unwrap_or(false);
            let short = s("short");
            let ms = with(|c| c.asked_at.elapsed().as_millis());
            log(format!("ctl: answer easy={easy} by={} in {ms} ms: {short:?}", s("by")));
            if easy && !short.is_empty() {
                show_answer(gen, &short, &s("host"));
            } else {
                show_best_effort(gen);
            }
        }
        "error" => show_best_effort(gen),
        _ => {}
    }
}

fn decide_timeout(gen: u64) {
    if with(|c| c.gen == gen && c.phase == Phase::Thinking && !c.decided) {
        log("ctl: no answer in time; showing what we have");
        show_best_effort(gen);
    }
}

/// Everything stays in the notch: with no direct answer, show the best passage (or a note)
/// and the top results to click.
fn show_best_effort(gen: u64) {
    let (text, host) = with(|c| match &c.passage {
        Some((t, h)) if !t.is_empty() => (t.replace(" · ", "\n"), h.clone()),
        _ => (if c.sources.is_empty() { "No answer found".to_string() } else { "No direct answer. Top results:".to_string() }, String::new()),
    });
    show_answer(gen, &text, &host);
}

fn show_answer(gen: u64, text: &str, host: &str) {
    let rows = with(|c| {
        c.decided = true;
        set_phase(c, Phase::Showing);
        c.sources.clone()
    });
    notch::answer(text, host, &rows);
    let dwell = (5000 + 45 * text.chars().count() as u64).min(25000);
    main_after(dwell, move || {
        if with(|c| c.gen == gen && c.phase == Phase::Showing) {
            notch::collapse();
            reset(gen);
        }
    });
}

/// A short confirmation in the notch, then fold away.
fn finish_with(gen: u64, text: &str, ms: u64) {
    if with(|c| c.gen != gen) {
        return;
    }
    with(|c| set_phase(c, Phase::Showing));
    notch::done(text);
    main_after(ms, move || {
        if with(|c| c.gen == gen && c.phase == Phase::Showing) {
            notch::collapse();
            reset(gen);
        }
    });
}

/// Grow the notch into the browser window and open `url` there.
fn open_url(gen: u64, url: String) {
    let bounds = with(|c| {
        c.decided = true;
        set_phase(c, Phase::Thinking);
        c.bounds
    });
    let bundle = config::get().browser_bundle.clone();
    notch::handoff(bounds, move || {
        std::thread::spawn(move || browser::show(&bundle, &url));
    });
    main_after(700, move || reset(gen));
}

/// Click on the notch: a source row opens in the browser; anywhere else folds the notch away
/// (or cancels while listening).
pub fn on_click(x: f64, y: f64) {
    let (phase, gen) = with(|c| (c.phase, c.gen));
    match phase {
        Phase::Showing => match notch::row_at(x, y) {
            Some(url) => open_url(gen, url),
            None => {
                notch::collapse();
                reset(gen);
            }
        },
        Phase::Listening | Phase::Finishing => {
            speech::cancel();
            notch::collapse();
            reset(gen);
        }
        _ => {}
    }
}

fn reset(gen: u64) {
    with(|c| {
        if c.gen == gen {
            set_phase(c, Phase::Idle);
        }
    });
    let idle = with(|c| c.phase == Phase::Idle);
    if idle {
        main_after(400, move || {
            if with(|c| c.gen == gen && c.phase == Phase::Idle) {
                notch::hide();
            }
        });
    }
}
