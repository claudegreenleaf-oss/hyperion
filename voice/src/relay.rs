//! The relay's /v1/answer stream over the tailnet: newline-delimited JSON events.

use std::io::{BufRead, BufReader};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::{self, log};
use crate::objc::main_async;

fn agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout_read(Duration::from_secs(10))
            .max_idle_connections_per_host(4)
            .build()
    })
}

fn now() -> String {
    std::process::Command::new("/bin/date")
        .arg("+%Y-%m-%d %H:%M %A")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn body(q: &str, prefetch: bool) -> Value {
    json!({"q": q, "voice": true, "prefetch": prefetch, "now": now(), "commands": crate::commands::catalog()})
}

/// Start the relay working on a query we might ask (a partial transcript), so the pages it
/// fetches are cached by the time the real question arrives.
pub fn prefetch(q: String) {
    std::thread::spawn(move || {
        let url = format!("{}/v1/answer", config::get().relay);
        let _ = agent().post(&url).send_json(body(&q, true));
    });
}

/// Ask; every event is delivered to `on_event` on the main thread.
pub fn ask(q: String, on_event: fn(u64, Value), gen: u64) {
    std::thread::spawn(move || {
        let url = format!("{}/v1/answer", config::get().relay);
        let resp = match agent().post(&url).send_json(body(&q, false)) {
            Ok(r) => r,
            Err(e) => {
                log(format!("relay: {e}"));
                main_async(move || on_event(gen, json!({"type": "error", "error": "relay unreachable"})));
                return;
            }
        };
        for line in BufReader::new(resp.into_reader()).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                main_async(move || on_event(gen, v));
            }
        }
    });
}
