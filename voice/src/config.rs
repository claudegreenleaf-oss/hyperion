//! Settings from ~/.config/hyperion-voice/config.json; every field is optional.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

pub struct Config {
    /// Base URL of the Hyperion relay (tailnet).
    pub relay: String,
    /// Browser that shows full answers.
    pub browser_bundle: String,
    /// Wake word score needed to trigger (openWakeWord scale, 0 to 1).
    pub threshold: f32,
    /// Silence after speech that ends the question.
    pub silence_ms: u64,
    /// How long to wait for the answer before handing off to the browser.
    pub answer_budget_ms: u64,
    /// Wake word model: a file name in ~/.config/hyperion-voice/models or the app's
    /// Resources/models (the first one found wins).
    pub wake_model: String,
    /// Words the speech recognizer should expect (added to the built-in list).
    pub vocabulary: Vec<String>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get() -> &'static Config {
    CONFIG.get_or_init(load)
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

fn load() -> Config {
    let path = home().join(".config/hyperion-voice/config.json");
    let v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let s = |k: &str, d: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or(d).to_string();
    let n = |k: &str, d: f64| v.get(k).and_then(|x| x.as_f64()).unwrap_or(d);
    Config {
        relay: s("relay", "http://100.104.232.110:8790").trim_end_matches('/').to_string(),
        browser_bundle: s("browser_bundle", "net.imput.helium"),
        threshold: n("threshold", 0.5) as f32,
        silence_ms: n("silence_ms", 650.0) as u64,
        answer_budget_ms: n("answer_budget_ms", 2600.0) as u64,
        wake_model: s("wake_model", "hey_jarvis_v0.1.onnx"),
        vocabulary: v
            .get("vocabulary")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|w| w.as_str().map(String::from)).collect())
            .unwrap_or_default(),
    }
}

/// Append one line to ~/Library/Logs/HyperionVoice.log.
pub fn log(msg: impl AsRef<str>) {
    let path = home().join("Library/Logs/HyperionVoice.log");
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{t:.3} {}", msg.as_ref());
    }
}
