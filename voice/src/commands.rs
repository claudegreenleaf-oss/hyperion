//! Things the voice can do on the Mac: a few built-ins (apps, volume, screen brightness,
//! reminders) plus every Raycast command installed, launched through Raycast's deeplinks.
//! The catalog is sent with each spoken request; the relay's model picks an id and arguments.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CString};
use std::process::Command;
use std::sync::OnceLock;

use crate::config::{self, log};

struct Entry {
    line: String,
    mode: String,
    /// Lowercase words from the id, titles and descriptions, for matching a request.
    words: String,
}

fn entries() -> &'static HashMap<String, Entry> {
    static E: OnceLock<HashMap<String, Entry>> = OnceLock::new();
    E.get_or_init(load)
}

fn builtin() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("mac.open_app", "Open or switch to an app on this Mac", "name"),
        ("mac.quit_app", "Quit an app", "name"),
        ("mac.volume", "Set the Mac's output volume", "percent"),
        ("mac.mute", "Mute the Mac's sound", ""),
        ("mac.unmute", "Unmute the Mac's sound", ""),
        ("mac.brightness", "Set the built-in display's brightness", "percent"),
        ("mac.reminder", "Add a reminder to the Reminders app", "title, due"),
        ("mac.lock_screen", "Lock the screen", ""),
        ("mac.sleep", "Put the Mac to sleep", ""),
    ]
}

fn load() -> HashMap<String, Entry> {
    let mut map = HashMap::new();
    for (id, what, args) in builtin() {
        map.insert(id.to_string(), Entry { line: format!("{id} | {what} | {args}"), mode: "builtin".into(), words: String::new() });
    }
    let dir = config::home().join(".config/raycast/extensions");
    for ext in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(text) = std::fs::read_to_string(ext.path().join("package.json")) else { continue };
        let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let owner = pkg.get("owner").or_else(|| pkg.get("author")).and_then(|v| v.as_str()).unwrap_or("");
        let name = pkg.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let ext_title = pkg.get("title").and_then(|v| v.as_str()).unwrap_or(name);
        let ext_desc = pkg.get("description").and_then(|v| v.as_str()).unwrap_or("");
        if owner.is_empty() || name.is_empty() {
            continue;
        }
        for c in pkg.get("commands").and_then(|v| v.as_array()).into_iter().flatten() {
            let mode = c.get("mode").and_then(|v| v.as_str()).unwrap_or("view");
            if mode == "menu-bar" {
                continue;
            }
            let cmd = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let title = c.get("title").and_then(|v| v.as_str()).unwrap_or(cmd);
            let args: Vec<&str> = c
                .get("arguments")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                .collect();
            let desc = c.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let id = format!("rc:{owner}/{name}/{cmd}");
            let line = format!("{id} | {title} ({ext_title}) | {}", args.join(", "));
            let words = format!("{id} {title} {ext_title} {desc} {ext_desc}").to_lowercase();
            map.insert(id, Entry { line, mode: mode.to_string(), words });
        }
    }
    log(format!("commands: {} available", map.len()));
    map
}

/// Warm the catalog (reads every installed Raycast extension once).
pub fn catalog() -> usize {
    entries().len()
}

const SYNONYMS: &[(&str, &[&str])] = &[
    ("awake", &["caffeinate", "sleep"]),
    ("sleep", &["caffeinate"]),
    ("music", &["spotify", "play"]),
    ("song", &["spotify", "track"]),
    ("songs", &["spotify", "track"]),
    ("pause", &["play", "spotify"]),
    ("skip", &["next", "spotify"]),
    ("remind", &["reminder"]),
    ("note", &["notes"]),
    ("wallpaper", &["wallpaper", "unsplash"]),
];
const SKIP: &[&str] = &["the", "and", "for", "my", "me", "to", "a", "an", "of", "on", "in", "it", "please", "can", "you", "turn", "set", "some", "with", "this", "that"];

/// The commands worth showing the router for this request: the built-ins plus the Raycast
/// commands whose names or descriptions share the most words with it. Sending all ~270 made
/// every routing call take over a second; twenty-odd keeps it near 0.3 s.
pub fn catalog_for(q: &str) -> String {
    let mut words: Vec<String> = q
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1 && !SKIP.contains(w))
        .map(String::from)
        .collect();
    for (w, extra) in SYNONYMS {
        if words.iter().any(|x| x == w) {
            words.extend(extra.iter().map(|s| s.to_string()));
        }
    }
    let stems: Vec<String> = words.iter().map(|w| w.chars().take(5).collect()).collect();
    let mut scored: Vec<(usize, &str)> = entries()
        .iter()
        .filter(|(id, _)| id.starts_with("rc:"))
        .map(|(_, e)| (stems.iter().filter(|s| e.words.contains(s.as_str())).count(), e.line.as_str()))
        .filter(|(n, _)| *n > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let mut lines: Vec<&str> = entries().values().filter(|e| e.mode == "builtin").map(|e| e.line.as_str()).collect();
    lines.sort();
    lines.extend(scored.into_iter().take(12).map(|(_, l)| l));
    lines.join("\n")
}

fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn osascript(script: &str) -> Result<(), String> {
    let out = Command::new("/usr/bin/osascript").arg("-e").arg(script).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn percent(args: &HashMap<String, String>) -> Result<f64, String> {
    let v = args.get("percent").or_else(|| args.values().next()).ok_or("no level given")?;
    v.trim().trim_end_matches('%').parse::<f64>().map(|p| p.clamp(0.0, 100.0)).map_err(|_| format!("bad level {v:?}"))
}

extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn CGMainDisplayID() -> u32;
}

/// Built-in display brightness through DisplayServices (what the brightness keys use).
fn set_brightness(level: f64) -> Result<(), String> {
    unsafe {
        let path = CString::new("/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices").unwrap();
        let h = dlopen(path.as_ptr(), 1);
        if h.is_null() {
            return Err("DisplayServices unavailable".into());
        }
        let sym = CString::new("DisplayServicesSetBrightness").unwrap();
        let f = dlsym(h, sym.as_ptr());
        if f.is_null() {
            return Err("DisplayServicesSetBrightness missing".into());
        }
        let set: extern "C" fn(u32, f32) -> c_int = std::mem::transmute(f);
        match set(CGMainDisplayID(), (level / 100.0) as f32) {
            0 => Ok(()),
            e => Err(format!("DisplayServices error {e}")),
        }
    }
}

/// Lock the screen right away, the way the menu bar's Lock Screen does (login.framework).
fn lock_screen() -> Result<(), String> {
    unsafe {
        let path = CString::new("/System/Library/PrivateFrameworks/login.framework/Versions/Current/login").unwrap();
        let h = dlopen(path.as_ptr(), 1);
        if h.is_null() {
            return Err("login framework unavailable".into());
        }
        let sym = CString::new("SACLockScreenImmediate").unwrap();
        let f = dlsym(h, sym.as_ptr());
        if f.is_null() {
            return Err("SACLockScreenImmediate missing".into());
        }
        let lock: extern "C" fn() -> c_int = std::mem::transmute(f);
        lock();
        Ok(())
    }
}

/// "2026-10-19T09:00" -> (y, m, d, h, min)
fn parse_due(s: &str) -> Option<(u32, u32, u32, u32, u32)> {
    let s = s.trim();
    let (date, time) = s.split_once('T').unwrap_or((s, "09:00"));
    let mut d = date.split('-').filter_map(|x| x.parse::<u32>().ok());
    let mut t = time.split(':').filter_map(|x| x.get(..2).unwrap_or(x).parse::<u32>().ok());
    Some((d.next()?, d.next()?, d.next()?, t.next().unwrap_or(9), t.next().unwrap_or(0)))
}

fn reminder(args: &HashMap<String, String>) -> Result<(), String> {
    let title = args.get("title").cloned().unwrap_or_default();
    if title.is_empty() {
        return Err("no reminder text".into());
    }
    let due = args.get("due").and_then(|d| parse_due(d));
    let props = match due {
        Some((y, mo, d, h, mi)) => format!(
            r#"set d to current date
set day of d to 1
set year of d to {y}
set month of d to {mo}
set day of d to {d}
set hours of d to {h}
set minutes of d to {mi}
set seconds of d to 0
tell application "Reminders" to make new reminder with properties {{name:"{t}", remind me date:d}}"#,
            t = quote(&title)
        ),
        None => format!(r#"tell application "Reminders" to make new reminder with properties {{name:"{}"}}"#, quote(&title)),
    };
    osascript(&props)
}

fn raycast(path: &str, mode: &str, args: &HashMap<String, String>) -> Result<(), String> {
    let mut url = format!("raycast://extensions/{path}");
    let mut q = Vec::new();
    if mode == "no-view" {
        q.push("launchType=background".to_string());
    }
    if !args.is_empty() {
        let json = serde_json::to_string(args).unwrap_or_default();
        q.push(format!("arguments={}", encode(&json)));
    }
    if !q.is_empty() {
        url.push('?');
        url.push_str(&q.join("&"));
    }
    let mut cmd = Command::new("/usr/bin/open");
    if mode == "no-view" {
        cmd.arg("-g");
    }
    let st = cmd.arg(&url).status().map_err(|e| e.to_string())?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("open {url} failed"))
    }
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Run one command. Blocking (osascript, open); call off the main thread.
pub fn run(id: &str, args: &HashMap<String, String>) -> Result<(), String> {
    log(format!("command: {id} {args:?}"));
    let name = || args.get("name").or_else(|| args.values().next()).cloned().unwrap_or_default();
    match id {
        "mac.open_app" => {
            let st = Command::new("/usr/bin/open").args(["-a", &name()]).status().map_err(|e| e.to_string())?;
            if st.success() { Ok(()) } else { Err(format!("no app named {:?}", name())) }
        }
        "mac.quit_app" => osascript(&format!(r#"tell application "{}" to quit"#, quote(&name()))),
        "mac.volume" => osascript(&format!("set volume output volume {}", percent(args)?.round())),
        "mac.mute" => osascript("set volume with output muted"),
        "mac.unmute" => osascript("set volume without output muted"),
        "mac.brightness" => set_brightness(percent(args)?),
        "mac.reminder" => reminder(args),
        "mac.lock_screen" => lock_screen(),
        "mac.sleep" => Command::new("/usr/bin/pmset").arg("sleepnow").status().map(|_| ()).map_err(|e| e.to_string()),
        _ => match id.strip_prefix("rc:") {
            Some(path) => {
                let mode = entries().get(id).map(|e| e.mode.as_str()).ok_or("not an installed Raycast command")?;
                raycast(path, mode, args)
            }
            None => Err(format!("unknown command {id}")),
        },
    }
}
