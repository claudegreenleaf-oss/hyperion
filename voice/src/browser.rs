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

fn find_window(origin: &str) -> String {
    format!(
        r#"set w to missing value
    repeat with x in windows
      try
        if (URL of active tab of x) starts with "{origin}/a" then
          set w to x
          exit repeat
        end if
      end try
    end repeat"#
    )
}

/// Bounds of the window an answer would land in, if the browser is already running:
/// (left, top, right, bottom) in AppleScript screen coordinates.
pub fn answer_window_bounds(bundle: &str, origin: &str) -> Option<(f64, f64, f64, f64)> {
    let script = format!(
        r#"if application id "{bundle}" is not running then return ""
tell application id "{bundle}"
    if (count of windows) is 0 then return ""
    {find}
    if w is missing value then set w to window 1
    set b to bounds of w
    return ((item 1 of b) as text) & "," & ((item 2 of b) as text) & "," & ((item 3 of b) as text) & "," & ((item 4 of b) as text)
end tell"#,
        find = find_window(origin)
    );
    let s = osascript(&script)?;
    let v: Vec<f64> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    (v.len() == 4).then(|| (v[0], v[1], v[2], v[3]))
}

/// Show `url` in the answer window (made if needed) and bring it to the front.
pub fn show(bundle: &str, origin: &str, url: &str) {
    let url = url.replace('"', "%22");
    let script = format!(
        r#"tell application id "{bundle}"
    {find}
    if w is missing value then set w to make new window
    set URL of active tab of w to "{url}"
    set index of w to 1
    activate
end tell"#,
        find = find_window(origin)
    );
    if osascript(&script).is_none() {
        // Fall back to a plain open, which at least brings the page up front.
        let _ = Command::new("/usr/bin/open").args(["-b", bundle, &url]).status();
    }
}
