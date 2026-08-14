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
    util::claude_config_root().join("dash2/anthropic_usage.json")
}

/// Credential files to try, most specific first: the active config root, then
/// the default root when a harness pointed `CLAUDE_CONFIG_DIR` somewhere else.
fn credential_files() -> Vec<PathBuf> {
    let mut paths = vec![util::claude_config_root().join(".credentials.json")];
    let default_root =
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude/.credentials.json");
    if !paths.contains(&default_root) {
        paths.push(default_root);
    }
    paths
}

/// One credential blob's token plus its expiry, if it carries an OAuth section.
///
/// Pure over the file body so the expiry policy is testable without a keychain,
/// a home directory or a clock.
fn token_from_credentials(raw: &str, now_ms: i64) -> Option<(String, bool)> {
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    let token = v
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|x| x.as_str())
        .filter(|t| !t.is_empty())?;
    // `expiresAt` is epoch MILLIseconds. Missing means "unknown", which is
    // treated as live: the endpoint is the authority on whether a token works.
    let live = v
        .pointer("/claudeAiOauth/expiresAt")
        .and_then(|x| x.as_i64())
        .map(|exp| exp > now_ms)
        .unwrap_or(true);
    Some((token.to_string(), live))
}

/// OAuth access token, from whichever store this machine keeps it in.
///
/// Claude Code puts the same `claudeAiOauth` blob in the macOS keychain and in
/// `<config root>/.credentials.json`; only the latter exists on Linux, so
/// reading the keychain alone left the 5h/weekly rows permanently blank there.
/// The chain is keychain (macOS only) → credential files → give up.
///
/// Expiry is a preference, not a filter: Claude Code refreshes the file in
/// place, so a token that looks stale here may already have been rotated, and a
/// single read-only GET is cheaper than being wrong. An unexpired token wins;
/// otherwise the first token found is used and the endpoint decides.
fn oauth_token() -> Option<String> {
    let now_ms = util::now_unix() * 1000;
    let mut stale: Option<String> = None;

    #[cfg(target_os = "macos")]
    {
        if let Some(raw) = keychain_blob() {
            match token_from_credentials(&raw, now_ms) {
                Some((token, true)) => return Some(token),
                Some((token, false)) => stale = stale.or(Some(token)),
                None => {}
            }
        }
    }

    for path in credential_files() {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        match token_from_credentials(&raw, now_ms) {
            Some((token, true)) => return Some(token),
            Some((token, false)) => stale = stale.or(Some(token)),
            None => {}
        }
    }

    stale
}

/// The raw credential blob Claude Code stores in the macOS keychain.
#[cfg(target_os = "macos")]
fn keychain_blob() -> Option<String> {
    let out = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
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
        used_pct: v
            .get("utilization")
            .and_then(|x| x.as_f64())
            .map(|f| f as f32),
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

    const CREDS: &str = r#"{"claudeAiOauth":{"accessToken":"tok-abc","refreshToken":"r",
      "expiresAt":2000000000000,"scopes":["user:inference"],"subscriptionType":"max"}}"#;

    #[test]
    fn reads_token_from_a_credentials_blob() {
        let (token, live) = token_from_credentials(CREDS, 1_000_000_000_000).unwrap();
        assert_eq!(token, "tok-abc");
        assert!(live);
    }

    #[test]
    fn an_expired_token_is_still_offered_but_marked_stale() {
        let (token, live) = token_from_credentials(CREDS, 3_000_000_000_000).unwrap();
        assert_eq!(token, "tok-abc");
        assert!(!live, "expiresAt in the past must not read as live");
    }

    #[test]
    fn a_blob_without_an_expiry_is_treated_as_live() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"tok-xyz"}}"#;
        let (token, live) = token_from_credentials(raw, 9_999_999_999_999).unwrap();
        assert_eq!(token, "tok-xyz");
        assert!(live);
    }

    #[test]
    fn blobs_without_a_usable_token_are_rejected() {
        assert!(token_from_credentials(r#"{"mcpOAuth":{}}"#, 0).is_none());
        assert!(token_from_credentials(r#"{"claudeAiOauth":{"accessToken":""}}"#, 0).is_none());
        assert!(token_from_credentials("not json", 0).is_none());
    }

    #[test]
    fn credential_files_are_probed_specific_root_first() {
        let paths = credential_files();
        assert!(!paths.is_empty());
        assert!(paths
            .iter()
            .all(|p| p.file_name().and_then(|n| n.to_str()) == Some(".credentials.json")));
        // No duplicate probe when CLAUDE_CONFIG_DIR is unset or already default.
        let mut seen = paths.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), paths.len());
    }

    #[test]
    fn missing_windows_are_default() {
        let (h5, wk) = parse(r#"{"other":1}"#).unwrap();
        assert!(h5.used_pct.is_none());
        assert!(wk.used_pct.is_none());
    }
}
