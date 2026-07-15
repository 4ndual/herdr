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
use std::path::{Path, PathBuf};

const WINDOW_200K: u64 = 200_000;
const WINDOW_1M: u64 = 1_000_000;

fn projects_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/projects")
}

/// Every `.../claude/projects` dir to scan for a session transcript: the native
/// Claude Code root plus each CCR profile's mirrored root. "cx" sessions (Claude
/// Code driving GPT models through the CCR gateway) write their transcripts under
/// `~/.claude-code-router/profiles/<profile>/claude/projects/…`, not `~/.claude`.
fn projects_roots() -> Vec<PathBuf> {
    let mut roots = vec![projects_root()];
    let home = std::env::var("HOME").unwrap_or_default();
    let ccr = PathBuf::from(home).join(".claude-code-router/profiles");
    if let Ok(profiles) = std::fs::read_dir(&ccr) {
        for prof in profiles.flatten() {
            let p = prof.path().join("claude/projects");
            if p.is_dir() {
                roots.push(p);
            }
        }
    }
    roots
}

/// Locate the transcript `<sid>.jsonl` under any project dir across every root
/// (native + CCR profiles); newest mtime wins.
pub fn transcript_for_sid(sid: &str) -> Option<PathBuf> {
    let want = format!("{sid}.jsonl");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for root in projects_roots() {
        if let Ok(projects) = std::fs::read_dir(&root) {
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
    }
    best.map(|(_, p)| p)
}

/// True when a transcript's last assistant model is a GPT/Codex model routed
/// through CCR, meaning the session must be attributed to the Codex plan (its
/// identity, colors, and quota) — never Anthropic. The rule: after stripping an
/// optional `"Codex Plan/"` prefix, the id contains "codex", starts with "gpt-",
/// or is an `o<digit>` reasoning model (o3, o4-mini, …). Case-insensitive.
pub fn is_gpt_model(model: &str) -> bool {
    let m = crate::sources::codex::strip_codex_plan_prefix(model).to_ascii_lowercase();
    if m.contains("codex") || m.starts_with("gpt-") {
        return true;
    }
    let mut chars = m.chars();
    chars.next() == Some('o') && chars.next().map(|c| c.is_ascii_digit()).unwrap_or(false)
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
        let ctx = get("input_tokens")
            + get("cache_read_input_tokens")
            + get("cache_creation_input_tokens");
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
    if std::env::var("CLAUDE_CODE_DISABLE_1M_CONTEXT")
        .ok()
        .as_deref()
        == Some("1")
    {
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
    let model_id = t.model.clone().unwrap_or_default();

    // A cx session (Claude Code driving a GPT model via CCR) is attributed to
    // the Codex plan — never Anthropic. The LAST assistant model decides: if the
    // user flipped back to a claude-* model, the session stays Claude.
    if t.model.as_deref().map(is_gpt_model).unwrap_or(false) {
        return gpt_snapshot_for_sid(&content, &path, &t);
    }

    let mut s = UsageSnapshot::empty(Cli::Claude);
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

/// Build a Codex-attributed snapshot for a cx session. Context/model/stats/state
/// come from the same Claude Code transcript (cx *is* Claude Code), but quota is
/// the Codex plan's and the context window comes from the CCR model catalog.
/// Crucially, this path NEVER calls the Anthropic usage API.
fn gpt_snapshot_for_sid(content: &str, path: &Path, t: &Transcript) -> Option<UsageSnapshot> {
    let mut s = UsageSnapshot::empty(Cli::Codex);
    let raw_model = t.model.clone().unwrap_or_default();
    let model_id = crate::sources::codex::strip_codex_plan_prefix(&raw_model).to_string();

    match crate::sources::codex::ctx_window_from_catalog(&model_id) {
        Some(w) => s.ctx_window = Some(w),
        None => s.ctx_approx = true, // catalog missing/unmatched → % is approximate
    }
    // GPT turns report input/output tokens with cache_* always 0, so the existing
    // context sum (input + cache_read + cache_creation) is exactly the input size.
    s.ctx_used_tokens = t.ctx_tokens;
    s.model = Some(model_id.clone());
    s.fill_ctx_pct();
    s.source_age_secs = util::file_age_secs(path);

    // Main-thread state + full last message: cx is Claude Code, so the same
    // transcript-based inference applies unchanged.
    let (st, note) = crate::state::infer_claude(content, s.source_age_secs);
    s.state = st;
    s.state_note = note;
    s.state_full = match st {
        crate::model::ThreadState::Waiting
        | crate::model::ThreadState::AwaitingReview
        | crate::model::ThreadState::Done
        | crate::model::ThreadState::Degraded => crate::state::full_claude(content),
        _ => None,
    };
    s.stats = crate::stats::session_stats(&path.to_string_lossy(), path);

    // Codex plan quota (separate account from Claude) — window-length mapped.
    if let Some((h5, wk, plan)) =
        crate::sources::codex::fresh_rate_limits_for_model(Some(&model_id))
    {
        s.five_hour = h5;
        s.weekly = wk;
        s.plan = plan;
        if let Some(wkp) = s.weekly.used_pct {
            s.daily_used_pct = Some(budget::daily_delta("codex", &util::today_local(), wkp));
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

    #[test]
    fn gpt_model_detection() {
        assert!(is_gpt_model("gpt-5.6-sol"));
        assert!(is_gpt_model("gpt-5.5"));
        assert!(is_gpt_model("Codex Plan/gpt-5.6-luna"));
        assert!(is_gpt_model("o3"));
        assert!(is_gpt_model("o4-mini"));
        assert!(is_gpt_model("some-codex-thing"));
        assert!(!is_gpt_model("claude-opus-4-8"));
        assert!(!is_gpt_model("claude-fable-5"));
        assert!(!is_gpt_model("opus")); // 'o' then 'p', not a digit
    }

    // ---- cx (CCR-tree) integration fixtures ----

    // Each test gets its own temp HOME; cargo nextest isolates tests in separate
    // processes, so mutating the HOME env var here is safe.
    struct TempHome {
        dir: PathBuf,
    }

    impl TempHome {
        fn new(tag: &str) -> Self {
            let uniq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("herdr-cx-{tag}-{uniq}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("HOME", &dir);
            TempHome { dir }
        }

        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const GPT_TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user"}}
{"type":"assistant","message":{"model":"gpt-5.6-sol","usage":{"input_tokens":12000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":300}}}"#;

    const CATALOG: &str = r#"{"models":[
        {"slug":"Codex Plan/gpt-5.6-sol","context_window":128000},
        {"slug":"Codex Plan/gpt-5.5","context_window":1050000}]}"#;

    // prolite: single weekly window reported as `primary`.
    const RATELIMITS: &str = r#"{"rateLimits":{"limitId":"codex","limitName":null,
        "primary":{"usedPercent":11,"windowDurationMins":10080,"resetsAt":1784489140},
        "secondary":null,"planType":"prolite"}}"#;

    #[test]
    fn cx_transcript_found_in_ccr_root() {
        let h = TempHome::new("found");
        let sid = "abc-123";
        let want = h.write(
            &format!(".claude-code-router/profiles/default/claude/projects/-tmp-proj/{sid}.jsonl"),
            GPT_TRANSCRIPT,
        );
        let got = transcript_for_sid(sid).expect("transcript located in CCR tree");
        assert_eq!(got, want);
    }

    #[test]
    fn cx_gpt_session_is_codex_with_codex_quota() {
        let h = TempHome::new("gpt");
        let sid = "sid-gpt";
        h.write(
            &format!(".claude-code-router/profiles/default/claude/projects/-tmp-proj/{sid}.jsonl"),
            GPT_TRANSCRIPT,
        );
        h.write(".codex/ccr-model-catalog.json", CATALOG);
        // Fresh cache (age < 60s) so no `codex app-server` process is spawned.
        h.write(".codex/dash2_ratelimits.json", RATELIMITS);

        let s = snapshot_for_sid(sid).expect("snapshot");
        assert_eq!(s.cli, Cli::Codex); // Codex identity, never Anthropic
        assert_eq!(s.model.as_deref(), Some("gpt-5.6-sol")); // raw, stripped id
        assert_eq!(s.ctx_window, Some(128000)); // from CCR catalog
        assert_eq!(s.ctx_used_tokens, Some(12000));
        assert_eq!(s.plan.as_deref(), Some("prolite"));
        // Window-length mapping: the 10080-min window is weekly, 5h stays empty.
        assert!(s.five_hour.used_pct.is_none());
        assert_eq!(s.weekly.used_pct, Some(11.0));
    }

    #[test]
    fn cx_missing_catalog_marks_ctx_approx() {
        let h = TempHome::new("nocatalog");
        let sid = "sid-noc";
        h.write(
            &format!(".claude-code-router/profiles/default/claude/projects/-tmp-proj/{sid}.jsonl"),
            GPT_TRANSCRIPT,
        );
        h.write(".codex/dash2_ratelimits.json", RATELIMITS);
        let s = snapshot_for_sid(sid).expect("snapshot");
        assert_eq!(s.cli, Cli::Codex);
        assert!(s.ctx_window.is_none());
        assert!(s.ctx_approx);
    }

    #[test]
    fn ccr_tree_claude_turn_stays_claude() {
        let h = TempHome::new("claude");
        let sid = "sid-claude";
        // Last assistant turn is a claude model (user flipped back) inside the CCR tree.
        let transcript = r#"{"type":"assistant","message":{"model":"gpt-5.6-sol","usage":{"input_tokens":10,"output_tokens":1}}}
{"type":"assistant","message":{"model":"claude-fable-5","usage":{"input_tokens":5000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":50}}}"#;
        h.write(
            &format!(".claude-code-router/profiles/default/claude/projects/-tmp-proj/{sid}.jsonl"),
            transcript,
        );
        // Fresh anthropic cache so the Claude path never hits the network.
        h.write(
            ".claude/dash2/anthropic_usage.json",
            r#"{"five_hour":{"utilization":10.0,"resets_at":"2026-07-15T12:00:00+00:00"},"seven_day":{"utilization":20.0,"resets_at":"2026-07-20T00:00:00+00:00"}}"#,
        );
        let s = snapshot_for_sid(sid).expect("snapshot");
        assert_eq!(s.cli, Cli::Claude); // claude last turn → Anthropic identity
    }
}
