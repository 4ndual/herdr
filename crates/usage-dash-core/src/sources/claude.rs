//! Claude Code usage source — reads PER-SESSION data directly, no statusline:
//!   • context tokens + model  ← the session transcript
//!       ~/.claude/projects/<sanitized-cwd>/<sid>.jsonl
//!       (last assistant message `usage`: input + cache_read + cache_creation)
//!   • 5h / weekly limit %      ← Anthropic usage API (see anthropic_usage.rs)
//!   • daily 12% tracker        ← weekly% delta since midnight (budget.rs)
//!
//! The context window (1M vs 200k) isn't in the transcript, so it's inferred
//! from the opus 1M setting and self-corrected if observed usage exceeds 200k.

use crate::budget;
use crate::model::{Cli, UsageSnapshot};
use crate::sources::anthropic_usage;
use crate::util;
use std::path::PathBuf;

const WINDOW_200K: u64 = 200_000;
const WINDOW_1M: u64 = 1_000_000;

fn projects_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/projects")
}

/// Locate the transcript `<sid>.jsonl` under any project dir (newest mtime wins).
pub fn transcript_for_sid(sid: &str) -> Option<PathBuf> {
    let want = format!("{sid}.jsonl");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(projects) = std::fs::read_dir(projects_root()) {
        for proj in projects.flatten() {
            let cand = proj.path().join(&want);
            if let Ok(meta) = std::fs::metadata(&cand) {
                let mt = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().map(|(b, _)| mt > *b).unwrap_or(true) {
                    best = Some((mt, cand));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

#[derive(Debug, Default, Clone)]
pub struct Transcript {
    pub model: Option<String>,
    pub ctx_tokens: Option<u64>, // current context = last assistant prompt size
    pub max_ctx_tokens: u64,     // largest seen this session (for window inference)
}

/// Scan a transcript for the LAST assistant message's model + context size.
/// Context size = input_tokens + cache_read_input_tokens + cache_creation_input_tokens.
pub fn parse_transcript(content: &str) -> Transcript {
    let mut out = Transcript::default();
    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let usage = match msg.get("usage") {
            Some(u) => u,
            None => continue,
        };
        let get = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let ctx = get("input_tokens") + get("cache_read_input_tokens") + get("cache_creation_input_tokens");
        if ctx > 0 {
            out.ctx_tokens = Some(ctx);
            out.max_ctx_tokens = out.max_ctx_tokens.max(ctx);
        }
        if let Some(m) = msg.get("model").and_then(|x| x.as_str()) {
            out.model = Some(m.to_string());
        }
    }
    out
}

/// Friendly model name from the raw API id. Unknown ids pass through.
pub fn model_display(id: &str) -> String {
    match id {
        "claude-opus-4-8" => "Opus 4.8".into(),
        "claude-opus-4-7" => "Opus 4.7".into(),
        "claude-sonnet-4-6" => "Sonnet 4.6".into(),
        "claude-haiku-4-5" | "claude-haiku-4-5-20251001" => "Haiku 4.5".into(),
        other => other.to_string(),
    }
}

/// Is opus 1M context enabled? Reads `~/.claude/settings.json` `.model` for a
/// `[1m]` variant (and honors CLAUDE_CODE_DISABLE_1M_CONTEXT). Best-effort.
fn one_m_enabled() -> bool {
    if std::env::var("CLAUDE_CODE_DISABLE_1M_CONTEXT").ok().as_deref() == Some("1") {
        return false;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let p = PathBuf::from(home).join(".claude/settings.json");
    if let Ok(raw) = std::fs::read_to_string(p) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
                return m.contains("1m");
            }
        }
    }
    false
}

/// Pick the context window for a model, self-correcting if usage exceeds 200k.
fn window_for(model_id: &str, observed_max: u64) -> u64 {
    let mut w = if model_id.contains("opus") && one_m_enabled() {
        WINDOW_1M
    } else {
        WINDOW_200K
    };
    if observed_max > w {
        w = WINDOW_1M;
    }
    w
}

pub fn snapshot_for_sid(sid: &str) -> Option<UsageSnapshot> {
    let path = transcript_for_sid(sid)?;
    let content = std::fs::read_to_string(&path).ok()?;
    let t = parse_transcript(&content);

    let mut s = UsageSnapshot::empty(Cli::Claude);
    let model_id = t.model.clone().unwrap_or_default();
    let window = window_for(&model_id, t.max_ctx_tokens);
    s.model = t.model.as_deref().map(|id| {
        let name = model_display(id);
        if window == WINDOW_1M {
            format!("{name} (1M context)")
        } else {
            name
        }
    });
    s.ctx_used_tokens = t.ctx_tokens;
    s.ctx_window = Some(window);
    s.fill_ctx_pct();
    s.source_age_secs = util::file_age_secs(&path);

    // main-thread state, inferred from the same transcript (no extra read).
    let (st, note) = crate::state::infer_claude(&content, s.source_age_secs);
    s.state = st;
    s.state_note = note;
    // Full untruncated last message for the click-to-expand view (only when the
    // turn is finished/awaiting — not mid-work, where the last message is stale).
    s.state_full = match st {
        crate::model::ThreadState::Waiting
        | crate::model::ThreadState::AwaitingReview
        | crate::model::ThreadState::Done
        | crate::model::ThreadState::Degraded => crate::state::full_claude(&content),
        _ => None,
    };

    // Per-session STATS (incremental — folds only bytes appended since last tick).
    s.stats = crate::stats::session_stats(&path.to_string_lossy(), &path);

    // 5h / weekly limits straight from Anthropic's usage API (account-global).
    if let Some((h5, wk)) = anthropic_usage::windows() {
        s.five_hour = h5;
        s.weekly = wk;
        if let Some(wkp) = s.weekly.used_pct {
            s.daily_used_pct = Some(budget::daily_delta("claude", &util::today_local(), wkp));
        }
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user"}}
{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":60,"cache_read_input_tokens":38634,"cache_creation_input_tokens":85989,"output_tokens":349}}}
{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":2,"cache_read_input_tokens":149701,"cache_creation_input_tokens":4352,"output_tokens":136}}}"#;

    #[test]
    fn parses_last_assistant_context() {
        let t = parse_transcript(TRANSCRIPT);
        assert_eq!(t.model.as_deref(), Some("claude-opus-4-8"));
        // last message: 2 + 149701 + 4352
        assert_eq!(t.ctx_tokens, Some(154055));
        // max over session: first message 60+38634+85989 = 124683 < 154055
        assert_eq!(t.max_ctx_tokens, 154055);
    }

    #[test]
    fn model_names_map() {
        assert_eq!(model_display("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_display("claude-sonnet-4-6"), "Sonnet 4.6");
        assert_eq!(model_display("future-model"), "future-model");
    }

    #[test]
    fn window_self_corrects_above_200k() {
        // sonnet has no 1M, but observed usage > 200k forces the 1M window
        assert_eq!(window_for("claude-sonnet-4-6", 50_000), WINDOW_200K);
        assert_eq!(window_for("claude-sonnet-4-6", 250_000), WINDOW_1M);
    }

    #[test]
    fn empty_transcript_yields_nothing() {
        let t = parse_transcript("garbage\n{not json}");
        assert!(t.model.is_none());
        assert!(t.ctx_tokens.is_none());
    }
}
