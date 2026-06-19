//! Claude rate-limit source: queries Anthropic's first-party usage endpoint
//! directly (the same data Claude Code's `/usage` shows), using the OAuth token
//! Claude Code already stored in the macOS keychain. This is the ONLY way to get
//! Claude's 5h/weekly limit utilization — it is never written to any per-session
//! file (unlike Codex/pi rollouts). Read-only GET of the user's own account data.
//!
//! Endpoint: GET https://api.anthropic.com/api/oauth/usage
//!   header: Authorization: Bearer <token>, anthropic-beta: oauth-2025-04-20
//! Shape: {"five_hour":{"utilization":39.0,"resets_at":"…+00:00"},
//!         "seven_day":{"utilization":45.0,"resets_at":"…"}, …}
//!
//! Result is cached to ~/.claude/dash2/anthropic_usage.json with a short TTL so
//! the 250ms render tick never hammers the API.

use crate::model::Window;
use crate::util;
use std::path::PathBuf;
use std::process::Command;

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA: &str = "oauth-2025-04-20";
const CACHE_TTL_SECS: u64 = 60;

fn cache_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/dash2/anthropic_usage.json")
}

/// OAuth access token from the macOS keychain entry Claude Code maintains.
fn oauth_token() -> Option<String> {
    let out = Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    v.pointer("/claudeAiOauth/accessToken")
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

/// Both windows (5h, weekly) for the account. None if the token or network fail.
pub fn windows() -> Option<(Window, Window)> {
    let raw = cached_or_fetch()?;
    parse(&raw)
}

/// Use the cache if fresh; otherwise fetch live and rewrite it. Falls back to a
/// stale cache if the live fetch fails (better than blanking the panel).
fn cached_or_fetch() -> Option<String> {
    let path = cache_path();
    if util::file_age_secs(&path) < CACHE_TTL_SECS {
        if let Ok(s) = std::fs::read_to_string(&path) {
            return Some(s);
        }
    }
    match fetch_live() {
        Some(body) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, &body);
            Some(body)
        }
        // network/token failure → last good cache if any
        None => std::fs::read_to_string(&path).ok(),
    }
}

fn fetch_live() -> Option<String> {
    let token = oauth_token()?;
    let out = Command::new("curl")
        .args([
            "-sS",
            "-m",
            "15",
            "-H",
            &format!("Authorization: Bearer {token}"),
            "-H",
            &format!("anthropic-beta: {BETA}"),
            ENDPOINT,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8(out.stdout).ok()?;
    // Reject error payloads / non-JSON so we don't cache garbage.
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    if v.get("five_hour").is_none() && v.get("seven_day").is_none() {
        return None;
    }
    Some(body)
}

/// Pure parser over the usage JSON body.
pub fn parse(raw: &str) -> Option<(Window, Window)> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    Some((win(v.get("five_hour")), win(v.get("seven_day"))))
}

fn win(v: Option<&serde_json::Value>) -> Window {
    let v = match v {
        Some(v) => v,
        None => return Window::default(),
    };
    Window {
        used_pct: v.get("utilization").and_then(|x| x.as_f64()).map(|f| f as f32),
        resets_at: v
            .get("resets_at")
            .and_then(|x| x.as_str())
            .and_then(util::iso2epoch_utc),
        shared: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usage_body() {
        let raw = r#"{"five_hour":{"utilization":39.0,"resets_at":"2026-06-17T16:40:00.835972+00:00"},
          "seven_day":{"utilization":45.0,"resets_at":"2026-06-22T04:00:00.835992+00:00"},
          "seven_day_sonnet":{"utilization":2.0,"resets_at":"2026-06-22T04:00:00+00:00"}}"#;
        let (h5, wk) = parse(raw).unwrap();
        assert_eq!(h5.used_pct, Some(39.0));
        assert_eq!(wk.used_pct, Some(45.0));
        assert!(h5.resets_at.is_some());
        assert!(wk.resets_at.unwrap() > h5.resets_at.unwrap());
    }

    #[test]
    fn missing_windows_are_default() {
        let (h5, wk) = parse(r#"{"other":1}"#).unwrap();
        assert!(h5.used_pct.is_none());
        assert!(wk.used_pct.is_none());
    }
}
