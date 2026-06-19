//! Codex usage source. Reads `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//! Per-line events of interest:
//!   {"type":"session_meta","payload":{"cwd":"…"}}            (first line)
//!   {"type":"turn_context","payload":{"model":"gpt-5.4"}}    (latest = current model)
//!   {"type":"token_count","payload":{
//!       "info":{"model_context_window":258400,
//!               "total_token_usage":{"total_tokens":15877,...}},
//!       "rate_limits":{"primary":{"used_percent":0.0,"window_minutes":300,"resets_at":..},
//!                      "secondary":{"used_percent":96.0,"window_minutes":10080,"resets_at":..},
//!                      "plan_type":"prolite"}}}

use crate::budget;
use crate::model::{Cli, UsageSnapshot, Window};
use crate::sources::codex_appserver;
use crate::util;
use std::path::{Path, PathBuf};

fn sessions_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".codex/sessions")
}

/// All rollout files, newest mtime first.
fn rollouts_by_mtime(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    // sessions/<Y>/<M>/<D>/rollout-*.jsonl
    for y in read_dirs(root) {
        for m in read_dirs(&y) {
            for d in read_dirs(&m) {
                if let Ok(rd) = std::fs::read_dir(&d) {
                    for e in rd.flatten() {
                        let p = e.path();
                        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                            let mt = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                            out.push((mt, p));
                        }
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, p)| p).collect()
}

fn read_dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v.reverse(); // newest-named first
    v
}

fn first_line_cwd(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let line = content.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("payload")?.get("cwd")?.as_str().map(str::to_string)
}

/// Newest rollout whose session_meta cwd matches; falls back to newest overall.
pub fn latest_rollout_for_cwd(cwd: &str) -> Option<PathBuf> {
    let all = rollouts_by_mtime(&sessions_root());
    for p in &all {
        if first_line_cwd(p).as_deref() == Some(cwd) {
            return Some(p.clone());
        }
    }
    all.into_iter().next()
}

#[derive(Debug, Default, Clone)]
pub struct CodexParsed {
    pub model: Option<String>,
    pub ctx_window: Option<u64>,
    pub total_tokens: Option<u64>,
    pub primary: Window,   // 5h
    pub secondary: Window, // weekly
    pub plan: Option<String>,
}

/// Scan a rollout's lines for the LAST token_count and LAST turn_context.
/// Real rollouts wrap token_count as `{"type":"event_msg","payload":{"type":"token_count",…}}`,
/// while turn_context is top-level `{"type":"turn_context","payload":{…}}`.
pub fn parse_rollout(content: &str) -> CodexParsed {
    let mut out = CodexParsed::default();
    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let top_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let payload = v.get("payload");
        let payload_type = payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()).unwrap_or("");

        if top_type == "turn_context" {
            if let Some(m) = payload.and_then(|p| p.get("model")).and_then(|x| x.as_str()) {
                out.model = Some(m.to_string());
            }
        }
        if top_type == "token_count" || payload_type == "token_count" {
            let p = match payload {
                Some(p) => p,
                None => continue,
            };
            if let Some(w) = p.pointer("/info/model_context_window").and_then(|x| x.as_u64()) {
                out.ctx_window = Some(w);
            }
            if let Some(t) = p
                .pointer("/info/total_token_usage/total_tokens")
                .and_then(|x| x.as_u64())
            {
                out.total_tokens = Some(t);
            }
            if let Some(rl) = p.get("rate_limits") {
                out.primary = rl_window(rl.get("primary"));
                out.secondary = rl_window(rl.get("secondary"));
                if let Some(pl) = rl.get("plan_type").and_then(|x| x.as_str()) {
                    out.plan = Some(pl.to_string());
                }
            }
        }
    }
    out
}

fn rl_window(v: Option<&serde_json::Value>) -> Window {
    let v = match v {
        Some(v) => v,
        None => return Window::default(),
    };
    Window {
        used_pct: v.get("used_percent").and_then(|x| x.as_f64()).map(|f| f as f32),
        resets_at: v.get("resets_at").and_then(|x| x.as_i64()),
        shared: false,
    }
}

/// Build a snapshot for the focused Codex session (matched by cwd).
pub fn snapshot_for_cwd(cwd: &str) -> Option<UsageSnapshot> {
    let path = latest_rollout_for_cwd(cwd)?;
    let content = std::fs::read_to_string(&path).ok()?;
    let p = parse_rollout(&content);
    let mut s = UsageSnapshot::empty(Cli::Codex);
    let model = p.model.clone();
    s.model = p.model;
    s.ctx_window = p.ctx_window;
    s.ctx_used_tokens = p.total_tokens;
    // Context/model come from this session's rollout, but the rate limits are
    // account-global and the rollout's token_count can be arbitrarily stale.
    // Prefer the freshest live source (app-server, cached 60s); fall back to the
    // newest rollout's token_count only when the app-server is unavailable.
    s.five_hour = p.primary;
    s.weekly = p.secondary;
    s.plan = p.plan;
    if let Some((pri, sec, plan)) = fresh_rate_limits_for_model(model.as_deref()) {
        s.five_hour = pri;
        s.weekly = sec;
        if plan.is_some() {
            s.plan = plan;
        }
    }
    s.source_age_secs = util::file_age_secs(&path);
    s.fill_ctx_pct();
    if let Some(wk) = s.weekly.used_pct {
        s.daily_used_pct = Some(budget::daily_delta("codex", &util::today_local(), wk));
    }
    Some(s)
}

/// Freshest account-global rate limits: try the live app-server first (cached),
/// then fall back to the newest rollout's `token_count` event. This is what both
/// the focused Codex snapshot and pi (shared OpenAI plan) consume.
pub fn fresh_rate_limits() -> Option<(Window, Window, Option<String>)> {
    codex_appserver::fresh_rate_limits().or_else(latest_codex_rate_limits)
}

/// Like [`fresh_rate_limits`] but selects the app-server bucket metering `model`
/// (e.g. the `codex_bengalfox` bucket for `gpt-5.3-codex-spark`). When no bucket
/// self-describes the model — or the app-server is unavailable — falls back to
/// the default bucket / newest rollout, exactly as [`fresh_rate_limits`].
pub fn fresh_rate_limits_for_model(
    model: Option<&str>,
) -> Option<(Window, Window, Option<String>)> {
    codex_appserver::fresh_rate_limits_for_model(model).or_else(latest_codex_rate_limits)
}

/// Newest token_count rate-limits across ALL rollouts — app-server fallback.
pub fn latest_codex_rate_limits() -> Option<(Window, Window, Option<String>)> {
    for p in rollouts_by_mtime(&sessions_root()) {
        if let Ok(content) = std::fs::read_to_string(&p) {
            let parsed = parse_rollout(&content);
            if parsed.primary.used_pct.is_some() || parsed.secondary.used_pct.is_some() {
                return Some((parsed.primary, parsed.secondary, parsed.plan));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLLOUT: &str = r#"{"type":"session_meta","payload":{"cwd":"/tmp/proj"}}
{"type":"turn_context","payload":{"model":"gpt-5.3"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":258400,"total_token_usage":{"total_tokens":1000}},"rate_limits":{"primary":{"used_percent":5.0,"window_minutes":300,"resets_at":100},"secondary":{"used_percent":50.0,"window_minutes":10080,"resets_at":200},"plan_type":"prolite"}}}
{"type":"turn_context","payload":{"model":"gpt-5.4"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":258400,"total_token_usage":{"total_tokens":15877}},"rate_limits":{"primary":{"used_percent":0.0,"window_minutes":300,"resets_at":1781714276},"secondary":{"used_percent":96.0,"window_minutes":10080,"resets_at":1781861649},"plan_type":"prolite"}}}"#;

    #[test]
    fn parse_takes_last_of_each() {
        let p = parse_rollout(ROLLOUT);
        assert_eq!(p.model.as_deref(), Some("gpt-5.4")); // last turn_context
        assert_eq!(p.total_tokens, Some(15877)); // last token_count
        assert_eq!(p.ctx_window, Some(258400));
        assert_eq!(p.primary.used_pct, Some(0.0));
        assert_eq!(p.secondary.used_pct, Some(96.0));
        assert_eq!(p.secondary.resets_at, Some(1781861649));
        assert_eq!(p.plan.as_deref(), Some("prolite"));
    }

    #[test]
    fn empty_rollout_is_all_none() {
        let p = parse_rollout("garbage\n{not json}");
        assert!(p.model.is_none());
        assert!(p.total_tokens.is_none());
        assert!(p.primary.used_pct.is_none());
    }
}
