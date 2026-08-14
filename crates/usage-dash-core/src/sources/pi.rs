//! pi usage source. pi runs on the `openai-codex` backend and persists only
//! model changes + message content locally (NO token/limit data). So:
//!   - model    ← last `model_change.modelId`
//!   - ctx      ← ESTIMATED from message content (~chars/4), flagged approximate
//!   - 5h/week  ← MIRRORED from Codex's latest rate-limit snapshot (shared plan)
//!
//! Sessions live at `~/.pi/agent/sessions/--<cwd-sanitized>--/<ts>_<id>.jsonl`.

use crate::budget;
use crate::model::{sanitize_cwd, Cli, UsageSnapshot};
use crate::sources::codex;
use crate::util;
use std::path::PathBuf;

fn sessions_root() -> PathBuf {
    util::pi_sessions_root()
}

/// Context window by pi model id (gpt-5.4 backend). Conservative default.
fn ctx_window_for(model: Option<&str>) -> u64 {
    match model {
        Some(m) if m.contains("gpt-5") => 258_400,
        _ => 258_400,
    }
}

/// Newest jsonl in the cwd's sanitized session dir.
pub fn session_for_cwd(cwd: &str) -> Option<PathBuf> {
    let dir = sessions_root().join(sanitize_cwd(cwd));
    newest_jsonl_in(&dir)
}

fn newest_jsonl_in(dir: &std::path::Path) -> Option<PathBuf> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| {
            (
                e.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH),
                e.path(),
            )
        })
        .filter(|(_, p)| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort_by_key(|e| std::cmp::Reverse(e.0));
    files.into_iter().next().map(|(_, p)| p)
}

/// Newest pi session across ALL cwd dirs — fallback when the focused cwd is
/// unreadable.
pub fn newest_session() -> Option<PathBuf> {
    let root = sessions_root();
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for d in std::fs::read_dir(&root).ok()?.flatten() {
        if let Some((mt, p)) = newest_jsonl_in(&d.path()).and_then(|p| {
            std::fs::metadata(&p)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (t, p))
        }) {
            if best.as_ref().map(|(bt, _)| mt > *bt).unwrap_or(true) {
                best = Some((mt, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Last `model_change.modelId` in the session.
pub fn last_model(content: &str) -> Option<String> {
    let mut model = None;
    for line in content.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("model_change") {
                if let Some(m) = v.get("modelId").and_then(|x| x.as_str()) {
                    model = Some(m.to_string());
                }
            }
        }
    }
    model
}

/// Rough context-token estimate: sum text-bearing message content bytes / 4.
/// Deliberately approximate — pi exposes no real token count.
pub fn estimate_tokens(content: &str) -> u64 {
    let mut bytes: u64 = 0;
    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("message") | Some("custom_message") | Some("thinking") => {
                bytes += content_len(v.get("content"));
            }
            _ => {}
        }
    }
    bytes / 4
}

fn content_len(c: Option<&serde_json::Value>) -> u64 {
    match c {
        Some(serde_json::Value::String(s)) => s.len() as u64,
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .map(|item| {
                item.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.len() as u64)
                    .unwrap_or_else(|| item.to_string().len() as u64)
            })
            .sum(),
        Some(other) => other.to_string().len() as u64,
        None => 0,
    }
}

pub fn snapshot_for_cwd(cwd: &str) -> Option<UsageSnapshot> {
    let path = if cwd.is_empty() {
        newest_session()?
    } else {
        session_for_cwd(cwd).or_else(newest_session)?
    };
    let content = std::fs::read_to_string(&path).ok()?;
    let mut s = UsageSnapshot::empty(Cli::Pi);
    s.model = last_model(&content);
    s.ctx_window = Some(ctx_window_for(s.model.as_deref()));
    s.ctx_used_tokens = Some(estimate_tokens(&content));
    s.ctx_approx = true;
    s.fill_ctx_pct();
    s.source_age_secs = util::file_age_secs(&path);

    // mirror Codex's rate limits (shared OpenAI plan); prefer the freshest
    // live app-server reading for THIS session's model bucket, falling back to
    // the default bucket / newest rollout.
    if let Some((primary, secondary, plan)) = codex::fresh_rate_limits_for_model(s.model.as_deref())
    {
        s.five_hour = primary;
        s.five_hour.shared = true;
        s.weekly = secondary;
        s.weekly.shared = true;
        s.plan = plan;
        if let Some(wk) = s.weekly.used_pct {
            s.daily_used_pct = Some(budget::daily_delta("codex", &util::today_local(), wk));
        }
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_model_wins() {
        let c = r#"{"type":"model_change","modelId":"gpt-5.3"}
{"type":"message","content":"hi"}
{"type":"model_change","modelId":"gpt-5.4"}"#;
        assert_eq!(last_model(c).as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn estimate_sums_message_text() {
        // 40 chars of content across messages → ~10 tokens
        let c = r#"{"type":"message","content":"01234567890123456789"}
{"type":"custom_message","content":[{"text":"0123456789"},{"text":"0123456789"}]}
{"type":"session"}"#;
        assert_eq!(estimate_tokens(c), 10);
    }

    #[test]
    fn window_defaults_to_codex_size() {
        assert_eq!(ctx_window_for(Some("gpt-5.4")), 258_400);
        assert_eq!(ctx_window_for(None), 258_400);
    }
}
