//! Fresh Codex rate limits via the Codex CLI **app-server** (`codex app-server`),
//! which speaks JSON-RPC over stdio. This is the only way to read the account's
//! live 5h/weekly utilization WITHOUT spending a paid turn — the REST endpoints
//! all 403 behind a WAF, and the per-session rollout `token_count` events can be
//! arbitrarily stale (the freshest rollout may be minutes/hours old).
//!
//! Handshake (verified empirically against codex-cli 0.139.0):
//!   -> {"jsonrpc":"2.0","id":1,"method":"initialize",
//!        "params":{"clientInfo":{"name":..,"version":..},
//!                  "capabilities":{"experimentalApi":true}}}
//!   <- {"id":1,"result":{"userAgent":..,"codexHome":..,..}}
//!   -> {"jsonrpc":"2.0","method":"initialized"}                  (notification)
//!   -> {"jsonrpc":"2.0","id":2,"method":"account/rateLimits/read"}
//!   <- {"id":2,"result":{"rateLimits":{
//!         "primary":{"usedPercent":27,"windowDurationMins":300,"resetsAt":..},
//!         "secondary":{"usedPercent":100,"windowDurationMins":10080,"resetsAt":..},
//!         "planType":"prolite", ..}}}
//! Note: server interleaves unrelated notifications (e.g.
//! `remoteControl/status/changed`) — we read until we see our matching `id`.
//! `usedPercent` is an integer here (the rollout reports it as a float).
//!
//! Result is cached to ~/.codex/dash2_ratelimits.json with a 60s TTL so the
//! render tick never spawns the app-server every frame; on failure we fall back
//! to the last good cache.

use crate::model::Window;
use crate::util;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CACHE_TTL_SECS: u64 = 60;
/// Hard ceiling on how long we'll wait for the app-server handshake+read.
const READ_TIMEOUT: Duration = Duration::from_secs(8);

fn cache_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".codex/dash2_ratelimits.json")
}

/// Fresh `(primary=5h, secondary=weekly, plan_type)` from the app-server, cached.
/// `None` when the app-server is unavailable AND no cache exists.
pub fn fresh_rate_limits() -> Option<(Window, Window, Option<String>)> {
    let raw = cached_or_fetch()?;
    parse(&raw)
}

/// Fresh rate limits for the bucket metering `model`, falling back to the
/// default top-level bucket when no per-model bucket self-describes that model.
/// `None` when the app-server is unavailable AND no cache exists.
pub fn fresh_rate_limits_for_model(model: Option<&str>) -> Option<(Window, Window, Option<String>)> {
    let raw = cached_or_fetch()?;
    parse_for_model(&raw, model)
}

/// Use the cache if fresh; otherwise spawn the app-server and rewrite it. Falls
/// back to a stale cache if the live call fails (better than blanking).
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
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &body).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
            Some(body)
        }
        None => std::fs::read_to_string(&path).ok(),
    }
}

/// Spawn `codex app-server`, run the JSON-RPC handshake, and return the raw
/// `result` object of `account/rateLimits/read` as a JSON string. Always kills
/// the child before returning.
fn fetch_live() -> Option<String> {
    let mut child = Command::new("codex")
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let result = run_protocol(&mut child);

    // Clean teardown regardless of outcome.
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Drive the JSON-RPC exchange over the child's stdio. Bounded by READ_TIMEOUT.
fn run_protocol(child: &mut std::process::Child) -> Option<String> {
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let mut reader = BufReader::new(stdout);

    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "usage-dash", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true }
        }
    });
    stdin.write_all(init.to_string().as_bytes()).ok()?;
    stdin.write_all(b"\n").ok()?;
    let initialized = serde_json::json!({ "jsonrpc": "2.0", "method": "initialized" });
    stdin.write_all(initialized.to_string().as_bytes()).ok()?;
    stdin.write_all(b"\n").ok()?;
    let read_req =
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read" });
    stdin.write_all(read_req.to_string().as_bytes()).ok()?;
    stdin.write_all(b"\n").ok()?;
    stdin.flush().ok()?;

    // Read line-delimited messages until we see the response with id == 2, or
    // we exceed the timeout. The server may interleave notifications.
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut line = String::new();
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None; // EOF
        }
        let v: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("id").and_then(|x| x.as_i64()) == Some(2) {
            return v.get("result").map(|r| r.to_string());
        }
    }
}

/// Pure parser over the `result` object of `account/rateLimits/read`.
/// Maps `rateLimits.primary` → 5h and `rateLimits.secondary` → weekly.
pub fn parse(raw: &str) -> Option<(Window, Window, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let rl = v.get("rateLimits")?;
    let primary = win(rl.get("primary"));
    let secondary = win(rl.get("secondary"));
    if primary.used_pct.is_none() && secondary.used_pct.is_none() {
        return None;
    }
    let plan = rl
        .get("planType")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Some((primary, secondary, plan))
}

/// Pick the per-model rate-limit bucket whose `limitName` self-describes
/// `model`, parsing its primary/secondary windows. When `model` is `None` or no
/// bucket's `limitName` matches the model, fall back to the default top-level
/// `parse` (the historical single-bucket view).
///
/// The link is DERIVED, never hardcoded: each entry in `rateLimitsByLimitId`
/// carries a `limitName` (e.g. `"GPT-5.3-Codex-Spark"`) that names the model the
/// bucket meters. We match it against the session's model string (e.g.
/// `"gpt-5.3-codex-spark"`) under a normalization that drops case and every
/// non-alphanumeric separator, so `gpt53codexspark == gpt53codexspark`.
pub fn parse_for_model(raw: &str, model: Option<&str>) -> Option<(Window, Window, Option<String>)> {
    let want = match model.map(normalize_model) {
        Some(w) if !w.is_empty() => w,
        _ => return parse(raw),
    };
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if let Some(serde_json::Value::Object(buckets)) = v.get("rateLimitsByLimitId") {
        for bucket in buckets.values() {
            let name = bucket.get("limitName").and_then(|x| x.as_str());
            if name.map(normalize_model).as_deref() == Some(want.as_str()) {
                let primary = win(bucket.get("primary"));
                let secondary = win(bucket.get("secondary"));
                if primary.used_pct.is_some() || secondary.used_pct.is_some() {
                    let plan = bucket
                        .get("planType")
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                    return Some((primary, secondary, plan));
                }
            }
        }
    }
    // No bucket names this model (e.g. the default model, whose limitName is
    // null) — keep the historical default-bucket behavior.
    parse(raw)
}

/// Lowercase, keep only ASCII alphanumerics. Makes the session model string and
/// a bucket's `limitName` comparable regardless of `-`/`.`/space/case.
fn normalize_model(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn win(v: Option<&serde_json::Value>) -> Window {
    let v = match v {
        Some(serde_json::Value::Object(_)) => v.unwrap(),
        _ => return Window::default(),
    };
    Window {
        // app-server reports usedPercent as an integer; accept float too.
        used_pct: v.get("usedPercent").and_then(|x| x.as_f64()).map(|f| f as f32),
        resets_at: v.get("resetsAt").and_then(|x| x.as_i64()),
        shared: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESULT: &str = r#"{"rateLimits":{"limitId":"codex","limitName":null,
        "primary":{"usedPercent":27,"windowDurationMins":300,"resetsAt":1781714276},
        "secondary":{"usedPercent":100,"windowDurationMins":10080,"resetsAt":1781861650},
        "credits":{"hasCredits":false,"unlimited":false,"balance":"0"},
        "individualLimit":null,"planType":"prolite","rateLimitReachedType":"rate_limit_reached"}}"#;

    #[test]
    fn parses_appserver_result() {
        let (p, s, plan) = parse(RESULT).unwrap();
        assert_eq!(p.used_pct, Some(27.0));
        assert_eq!(p.resets_at, Some(1781714276));
        assert_eq!(s.used_pct, Some(100.0));
        assert_eq!(s.resets_at, Some(1781861650));
        assert_eq!(plan.as_deref(), Some("prolite"));
        assert!(!p.shared && !s.shared);
    }

    #[test]
    fn float_used_percent_also_accepted() {
        let raw = r#"{"rateLimits":{"primary":{"usedPercent":12.5},"secondary":{"usedPercent":0}}}"#;
        let (p, s, _) = parse(raw).unwrap();
        assert_eq!(p.used_pct, Some(12.5));
        assert_eq!(s.used_pct, Some(0.0));
    }

    #[test]
    fn missing_ratelimits_is_none() {
        assert!(parse(r#"{"other":1}"#).is_none());
        assert!(parse(r#"{"rateLimits":{}}"#).is_none());
    }

    // Real shape from `account/rateLimits/read`: a default `codex` bucket (its
    // `limitName` is null) plus a per-model bucket that self-describes its model.
    const MULTI: &str = r#"{"rateLimits":{"limitId":"codex","limitName":null,
        "primary":{"usedPercent":27,"resetsAt":111},
        "secondary":{"usedPercent":100,"resetsAt":222},"planType":"prolite"},
        "rateLimitsByLimitId":{
          "codex_bengalfox":{"limitId":"codex_bengalfox","limitName":"GPT-5.3-Codex-Spark",
            "primary":{"usedPercent":3,"resetsAt":333},
            "secondary":{"usedPercent":4,"resetsAt":444},"planType":"prolite"},
          "codex":{"limitId":"codex","limitName":null,
            "primary":{"usedPercent":27,"resetsAt":111},
            "secondary":{"usedPercent":100,"resetsAt":222},"planType":"prolite"}}}"#;

    #[test]
    fn selects_bucket_by_model_limit_name() {
        // Session model normalizes to the same key as the bucket's limitName.
        let (p, s, plan) = parse_for_model(MULTI, Some("gpt-5.3-codex-spark")).unwrap();
        assert_eq!(p.used_pct, Some(3.0));
        assert_eq!(p.resets_at, Some(333));
        assert_eq!(s.used_pct, Some(4.0));
        assert_eq!(s.resets_at, Some(444));
        assert_eq!(plan.as_deref(), Some("prolite"));
    }

    #[test]
    fn falls_back_to_default_when_no_bucket_matches() {
        // The default model (gpt-5.4) is metered by the `codex` bucket whose
        // limitName is null, so nothing self-describes it → default top-level.
        let (p, s, _) = parse_for_model(MULTI, Some("gpt-5.4")).unwrap();
        assert_eq!(p.used_pct, Some(27.0)); // default bucket, not bengalfox
        assert_eq!(s.used_pct, Some(100.0));
    }

    #[test]
    fn none_model_uses_default_bucket() {
        let (p, _, _) = parse_for_model(MULTI, None).unwrap();
        assert_eq!(p.used_pct, Some(27.0));
    }

    #[test]
    fn normalize_model_strips_separators_and_case() {
        assert_eq!(normalize_model("gpt-5.3-codex-spark"), "gpt53codexspark");
        assert_eq!(normalize_model("GPT-5.3-Codex-Spark"), "gpt53codexspark");
        assert_eq!(normalize_model("gpt-5.4"), "gpt54");
        assert_eq!(normalize_model(""), "");
    }
}
