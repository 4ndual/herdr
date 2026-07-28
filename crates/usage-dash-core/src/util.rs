//! Small clock/time helpers. Kept separate so source adapters can be unit-tested
//! with injected `now`/`today` values while production reads the real clock.
//!
//! Ported verbatim from usage-dash `src/util.rs`.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Local calendar date `YYYY-MM-DD`. Uses the system `date` (honors local TZ);
/// std alone has no local-time API. Cheap and called sparingly.
pub fn today_local() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Age in seconds of a file's last modification, vs now.
pub fn file_age_secs(path: &std::path::Path) -> u64 {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return u64::MAX,
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (now_unix() - mtime).max(0) as u64
}

/// Parse an RFC3339/ISO-8601 UTC timestamp ("2026-06-17T16:40:00.835972+00:00")
/// to unix seconds. Drops fractional seconds and the offset (the usage API
/// always returns UTC `+00:00`) and defers to system `date -u` for the calendar
/// math (std has no date parser). Returns None on anything unparseable.
pub fn iso2epoch_utc(s: &str) -> Option<i64> {
    // keep up to the seconds field: "YYYY-MM-DDTHH:MM:SS"
    let core: String = s
        .chars()
        .take_while(|&c| c != '.' && c != '+' && c != 'Z')
        .collect();
    if core.len() < 19 {
        return None;
    }
    let out = std::process::Command::new("date")
        .args(["-u", "-j", "-f", "%Y-%m-%dT%H:%M:%S", &core, "+%s"])
        .output()
        .ok()?;
    String::from_utf8(out.stdout)
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
}
