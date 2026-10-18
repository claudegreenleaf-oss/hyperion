//! Talking to the browser with AppleScript: where its answer window is, and showing a URL in
//! that window (reused across questions, so answers never pile up as tabs) in front.

use std::process::Command;

fn osascript(script: &str) -> Option<String> {
    let out = Command::new("/usr/bin/osascript").arg("-e").arg(script).output().ok()?;
    if !out.status.success() {
        crate::config::log(format!("browser: osascript failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Bounds of the browser's front window, if it is running: (left, top, right, bottom) in
/// AppleScript screen coordinates. The notch grows into this rectangle.
pub fn front_window_bounds(bundle: &str) -> Option<(f64, f64, f64, f64)> {
    let script = format!(
        r#"if application id "{bundle}" is not running then return ""
tell application id "{bundle}"
    if (count of windows) is 0 then return ""
    set b to bounds of window 1
    return ((item 1 of b) as text) & "," & ((item 2 of b) as text) & "," & ((item 3 of b) as text) & "," & ((item 4 of b) as text)
end tell"#
    );
    let s = osascript(&script)?;
    let v: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    (v.len() == 4).then(|| (v[0], v[1], v[2], v[3]))
}

/// Open `url` in a new tab of the browser and bring it to the front, like clicking a link.
pub fn show(bundle: &str, url: &str) {
    let _ = Command::new("/usr/bin/open").args(["-b", bundle, url]).status();
}

/// Google results for a query, as the browser itself would show them.
pub fn google(q: &str) -> String {
    let mut out = String::from("https://www.google.com/search?q=");
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
