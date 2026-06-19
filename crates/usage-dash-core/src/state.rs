//! Inference of the focused agent's "main thread" state from its on-disk
//! transcript tail — NO pane scraping, NO statusline. Pure functions over the
//! file content + the file's mtime age (seconds), so they're fully unit-tested.
//!
//! States (user's whiteboard contract):
//!   Working         — actively producing a turn / a tool is running. Don't touch.
//!   Blocked         — a HUMAN response is REQUIRED; nothing proceeds until resolved.
//!   Degraded        — API errors / repeated tool failures / churn.
//!   Waiting         — turn ended by asking the user a question (note: the question).
//!   AwaitingReview  — work delivered, nothing asked; it's the user's move to review.
//!   Done            — delivered + parked (idle ≥ DONE_AFTER_SECS), nothing pending.
//!   Unknown         — signal ambiguous/unavailable → caller renders a dim dash.
//!
//! INVARIANT: never `Done`/`AwaitingReview` while a tool/question is pending.

use crate::model::ThreadState;
use serde_json::Value;

/// A pending tool that's been idle longer than this is still treated as Working
/// for the structural cases; the threshold only gates the pi best-effort path.
const WORKING_FRESH_SECS: u64 = 60;
/// A clean, idle-this-long turn end is `Done` rather than `Waiting`.
pub const DONE_AFTER_SECS: u64 = 600;

/// Tools whose pending (unresolved) use means a human must respond to continue.
fn is_block_tool(name: &str) -> bool {
    matches!(name, "AskUserQuestion" | "ExitPlanMode")
}

fn parse_lines(content: &str) -> Vec<Value> {
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn rec_type(v: &Value) -> &str {
    v.get("type").and_then(|t| t.as_str()).unwrap_or("")
}

/// Collapse whitespace and truncate to ~40 cols for the inline note.
fn summarize(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 40 {
        let s: String = flat.chars().take(39).collect();
        format!("{s}…")
    } else {
        flat
    }
}

/// The trailing question line, if the message ends in one (used as the Waiting note
/// so the dash shows *what* is being asked, not the opening sentence).
fn last_question(text: &str) -> Option<String> {
    for line in text.lines().rev() {
        let l = line.trim().trim_end_matches(|c: char| c == '*' || c == '_' || c == '`' || c == ')');
        if l.is_empty() {
            continue;
        }
        return if l.ends_with('?') { Some(l.to_string()) } else { None };
    }
    None
}

/// Map a clean (no pending tool, no error) turn end to a state + note. Shared by
/// all three CLIs so the Waiting / Awaiting Review / Done semantics stay identical:
///   - last message asks the user a question  → Waiting       (needs an answer)
///   - work delivered, parked ≥ DONE_AFTER_SECS → Done         (idle, reviewed/gone)
///   - work delivered, still fresh              → Awaiting Review (your move to review)
fn finished_state(last_text: Option<&str>, age: u64) -> (ThreadState, Option<String>) {
    if let Some(t) = last_text {
        if let Some(q) = last_question(t) {
            return (ThreadState::Waiting, Some(summarize(&q)));
        }
    }
    if age >= DONE_AFTER_SECS {
        return (ThreadState::Done, Some(format!("idle {}m", age / 60)));
    }
    let note = last_text
        .map(summarize)
        .filter(|s| !s.is_empty())
        .or_else(|| Some("awaiting your review".into()));
    (ThreadState::AwaitingReview, note)
}

// ---------- Claude ----------

/// First text block of an assistant `message.content[]`.
fn assistant_text(v: &Value) -> Option<String> {
    let content = v.get("message")?.get("content")?.as_array()?;
    for item in content {
        if item.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                if !t.trim().is_empty() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

/// Names of `tool_use` blocks in an assistant `message.content[]`.
fn assistant_tool_uses(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(content) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
        for item in content {
            if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(n) = item.get("name").and_then(|n| n.as_str()) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out
}

/// A `user` record carrying a `tool_result` (= a tool just completed).
fn is_tool_result_user(v: &Value) -> bool {
    if rec_type(v) != "user" {
        return false;
    }
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .map(|arr| arr.iter().any(|it| it.get("type").and_then(|t| t.as_str()) == Some("tool_result")))
        .unwrap_or(false)
}

fn is_permission_record(v: &Value) -> bool {
    rec_type(v).contains("permission")
}

/// A genuine new user prompt (not a tool_result, not a meta/local-command record).
/// Its presence after the last assistant turn means the human just spoke and the
/// agent now owes a response → Working, regardless of the prior turn's end state.
fn is_user_prompt(v: &Value) -> bool {
    if rec_type(v) != "user" {
        return false;
    }
    if is_tool_result_user(v) {
        return false;
    }
    if v.get("isMeta").and_then(|b| b.as_bool()) == Some(true) {
        return false;
    }
    true
}

/// Does this record evidence an API error / a cancelled hook / a failed tool?
fn is_error_record(v: &Value) -> bool {
    if v.get("isApiErrorMessage").and_then(|b| b.as_bool()) == Some(true) {
        return true;
    }
    let t = rec_type(v);
    if t == "hook_cancelled" {
        return true;
    }
    // assistant message whose text reads as an API error
    if let Some(txt) = assistant_text(v) {
        if txt.contains("API Error") || txt.contains("api error") {
            return true;
        }
    }
    // tool_result user with is_error: true
    if t == "user" {
        if let Some(arr) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
            if arr.iter().any(|it| {
                it.get("type").and_then(|x| x.as_str()) == Some("tool_result")
                    && it.get("is_error").and_then(|b| b.as_bool()) == Some(true)
            }) {
                return true;
            }
        }
    }
    false
}

pub fn infer_claude(transcript: &str, age: u64) -> (ThreadState, Option<String>) {
    let recs = parse_lines(transcript);
    if recs.is_empty() {
        return (ThreadState::Unknown, None);
    }

    let la_idx = match recs.iter().rposition(|v| rec_type(v) == "assistant") {
        Some(i) => i,
        None => {
            // No assistant turn yet — the user just spoke; agent is about to work.
            return if age <= WORKING_FRESH_SECS {
                (ThreadState::Working, Some("thinking".into()))
            } else {
                (ThreadState::Unknown, None)
            };
        }
    };
    let la = &recs[la_idx];
    let after = &recs[la_idx + 1..];

    // 0) A fresh user prompt sits after the last assistant turn → the human just
    //    spoke and the agent owes a response. Dominates the prior turn's state
    //    (this is the "thinking" window before any tool/text is emitted).
    if after.iter().any(is_user_prompt) {
        return (ThreadState::Working, Some("thinking".into()));
    }

    let tool_names = assistant_tool_uses(la);
    let result_after = after.iter().any(is_tool_result_user);
    let pending_tool = !tool_names.is_empty() && !result_after;

    // 1) Blocked — a human response is required to continue.
    if pending_tool {
        let last = tool_names.last().unwrap();
        if tool_names.iter().any(|n| is_block_tool(n)) {
            return (ThreadState::Blocked, Some("awaiting your input".into()));
        }
        if after.iter().any(is_permission_record) || is_permission_record(la) {
            return (ThreadState::Blocked, Some(format!("approve {last}?")));
        }
    }

    // 2) Degraded — errors at/after the last assistant turn (not yet recovered).
    let errs = recs[la_idx..].iter().filter(|v| is_error_record(v)).count();
    if errs >= 2 || is_error_record(la) {
        return (ThreadState::Degraded, Some(format!("{} API error(s)/retries", errs.max(1))));
    }

    // 3) Working — a tool is dispatched, or a tool just returned and the agent
    //    is generating the next step.
    if pending_tool {
        return (ThreadState::Working, Some(format!("Running {}", tool_names.last().unwrap())));
    }
    if result_after {
        return (ThreadState::Working, Some("processing".into()));
    }

    // 4) Clean turn end → Waiting (fresh) or Done (parked).
    let stop = la.get("message").and_then(|m| m.get("stop_reason")).and_then(|s| s.as_str());
    if stop == Some("tool_use") {
        // Ended on a tool_use that already has its result handled above; here it
        // has none and isn't recent enough to be a live tool → ambiguous.
        return (ThreadState::Unknown, None);
    }
    let last_text = assistant_text(la);
    finished_state(last_text.as_deref(), age)
}

// ---------- Codex ----------

pub fn infer_codex(content: &str, age: u64) -> (ThreadState, Option<String>) {
    let recs = parse_lines(content);
    if recs.is_empty() {
        return (ThreadState::Unknown, None);
    }

    let mut last_phase: Option<String> = None;
    let mut last_msg: Option<String> = None;
    let mut err_in_tail = 0usize;
    let tail_start = recs.len().saturating_sub(10);

    for (i, v) in recs.iter().enumerate() {
        let t = rec_type(v);
        let payload = v.get("payload");
        let ptype = payload.and_then(|p| p.get("type")).and_then(|x| x.as_str()).unwrap_or("");
        if ptype == "token_count" || t == "token_count" {
            continue;
        }
        if t == "event_msg" || t == "response_item" {
            if let Some(ph) = payload.and_then(|p| p.get("phase")).and_then(|x| x.as_str()) {
                last_phase = Some(ph.to_string());
                last_msg = payload
                    .and_then(|p| p.get("message").or_else(|| p.get("text")))
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
            }
            // explicit error markers only (avoid false positives on prose)
            if (ptype == "error" || t == "error") && i >= tail_start {
                err_in_tail += 1;
            }
        }
    }

    if err_in_tail >= 1 {
        return (ThreadState::Degraded, Some("codex error".into()));
    }

    match last_phase.as_deref() {
        Some("final_answer") => finished_state(last_msg.as_deref(), age),
        Some(_) => {
            let note = last_msg.map(|m| summarize(&m)).unwrap_or_else(|| "working".into());
            (ThreadState::Working, Some(note))
        }
        None => (ThreadState::Unknown, None),
    }
}

// ---------- pi ----------

fn pi_tool_name(v: &Value) -> Option<String> {
    let content = v.get("message")?.get("content")?.as_array()?;
    for item in content {
        if item.get("type").and_then(|t| t.as_str()) == Some("toolCall") {
            if let Some(n) = item.get("name").and_then(|n| n.as_str()) {
                return Some(n.to_string());
            }
        }
    }
    None
}

fn pi_text(v: &Value) -> Option<String> {
    match v.get("message")?.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            for it in arr {
                if it.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = it.get("text").and_then(|t| t.as_str()) {
                        return Some(t.to_string());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

pub fn infer_pi(content: &str, age: u64) -> (ThreadState, Option<String>) {
    let recs = parse_lines(content);
    if recs.is_empty() {
        return (ThreadState::Unknown, None);
    }

    // Errors: tool results flagged isError near the tail.
    let tail_start = recs.len().saturating_sub(10);
    let err = recs[tail_start..]
        .iter()
        .filter(|v| {
            rec_type(v) == "message"
                && v.get("message").and_then(|m| m.get("role")).and_then(|r| r.as_str()) == Some("toolResult")
                && v.get("message").and_then(|m| m.get("isError")).and_then(|b| b.as_bool()) == Some(true)
        })
        .count();
    if err >= 2 {
        return (ThreadState::Degraded, Some(format!("{err} tool errors")));
    }

    let last = match recs.iter().rev().find(|v| rec_type(v) == "message") {
        Some(v) => v,
        None => return (ThreadState::Unknown, None),
    };
    let msg = last.get("message");
    let role = msg.and_then(|m| m.get("role")).and_then(|r| r.as_str()).unwrap_or("");
    let stop = msg.and_then(|m| m.get("stopReason")).and_then(|s| s.as_str()).unwrap_or("");

    match (role, stop) {
        ("assistant", "stop") => finished_state(pi_text(last).as_deref(), age),
        ("assistant", "toolUse") => {
            // pi has no reliable streaming timing → only call it Working when the
            // file changed very recently; otherwise honestly Unknown.
            if age <= WORKING_FRESH_SECS {
                let n = pi_tool_name(last).unwrap_or_else(|| "tool".into());
                (ThreadState::Working, Some(format!("Running {n}")))
            } else {
                (ThreadState::Unknown, None)
            }
        }
        ("toolResult", _) => {
            if age <= WORKING_FRESH_SECS {
                (ThreadState::Working, Some("processing".into()))
            } else {
                (ThreadState::Unknown, None)
            }
        }
        _ => (ThreadState::Unknown, None),
    }
}

// ---------- full status text (for the click-to-expand view) ----------

/// Untruncated text of the agent's last assistant message (Claude).
pub fn full_claude(transcript: &str) -> Option<String> {
    let recs = parse_lines(transcript);
    let la = recs.iter().rev().find(|v| rec_type(v) == "assistant")?;
    assistant_text(la)
}

/// Untruncated text of codex's last phase message.
pub fn full_codex(content: &str) -> Option<String> {
    let recs = parse_lines(content);
    let mut last_msg: Option<String> = None;
    for v in &recs {
        let t = rec_type(v);
        let payload = v.get("payload");
        if (t == "event_msg" || t == "response_item")
            && payload.and_then(|p| p.get("phase")).is_some()
        {
            last_msg = payload
                .and_then(|p| p.get("message").or_else(|| p.get("text")))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
        }
    }
    last_msg
}

/// Untruncated text of pi's last message.
pub fn full_pi(content: &str) -> Option<String> {
    let recs = parse_lines(content);
    let last = recs.iter().rev().find(|v| rec_type(v) == "message")?;
    pi_text(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    // S1 — Working: assistant ended on a tool_use with no result yet.
    #[test]
    fn claude_working_pending_tool() {
        let t = r#"{"type":"user","message":{"role":"user","content":"go"}}
{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#;
        let (s, note) = infer_claude(t, 3);
        assert_eq!(s, ThreadState::Working);
        assert_eq!(note.as_deref(), Some("Running Bash"));
    }

    // S2 — Blocked: pending AskUserQuestion (human input required).
    #[test]
    fn claude_blocked_on_question() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"AskUserQuestion","input":{}}]}}"#;
        let (s, _) = infer_claude(t, 120);
        assert_eq!(s, ThreadState::Blocked);
    }

    // S2b — Blocked: pending tool with a permission record following.
    #[test]
    fn claude_blocked_on_permission() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{}}]}}
{"type":"permission-mode","permissionMode":"default"}"#;
        let (s, _) = infer_claude(t, 30);
        assert_eq!(s, ThreadState::Blocked);
    }

    // S3 — Waiting: turn ended by asking the user a question.
    #[test]
    fn claude_waiting_clean_end() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"All set, what next?"}]}}"#;
        let (s, note) = infer_claude(t, 30);
        assert_eq!(s, ThreadState::Waiting);
        assert_eq!(note.as_deref(), Some("All set, what next?"));
    }

    // S3b — Waiting note is the trailing QUESTION, not the opening sentence.
    #[test]
    fn claude_waiting_note_is_the_question() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Everything you asked for is done and tested.\n\nWant me to wire up the daily budget?"}]}}"#;
        let (s, note) = infer_claude(t, 30);
        assert_eq!(s, ThreadState::Waiting);
        assert_eq!(note.as_deref(), Some("Want me to wire up the daily budget?"));
    }

    // S3c — AwaitingReview: work delivered, fresh, NO question → user's move to review.
    #[test]
    fn claude_awaiting_review_when_no_question() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Shipped and tested, all green."}]}}"#;
        let (s, note) = infer_claude(t, 30);
        assert_eq!(s, ThreadState::AwaitingReview);
        assert_eq!(note.as_deref(), Some("Shipped and tested, all green."));
    }

    // S3d — a stale QUESTION stays Waiting (does NOT flip to Done on idle).
    #[test]
    fn claude_stale_question_stays_waiting() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Which approach do you prefer?"}]}}"#;
        let (s, _) = infer_claude(t, DONE_AFTER_SECS + 500);
        assert_eq!(s, ThreadState::Waiting);
    }

    // S3e — a fresh user prompt after a finished turn → Working (not the old state).
    #[test]
    fn claude_new_prompt_after_finish_is_working() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Shipped and tested, all green."}]}}
{"type":"user","message":{"role":"user","content":"now add a dropdown"}}"#;
        let (s, note) = infer_claude(t, 3);
        assert_eq!(s, ThreadState::Working);
        assert_eq!(note.as_deref(), Some("thinking"));
        // even if some time has passed before the agent emits anything
        let (s2, _) = infer_claude(t, 30);
        assert_eq!(s2, ThreadState::Working);
    }

    // S3f — a tool_result user after the turn is NOT a new prompt (still processes).
    #[test]
    fn claude_tool_result_user_is_not_a_new_prompt() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
        let (s, _) = infer_claude(t, 3);
        assert_eq!(s, ThreadState::Working); // "processing", not "thinking"
    }

    // S4 — Done: clean end_turn, idle past the threshold; AND never Done while pending.
    #[test]
    fn claude_done_when_idle_and_never_done_while_pending() {
        let clean = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"finished"}]}}"#;
        let (s, _) = infer_claude(clean, DONE_AFTER_SECS + 5);
        assert_eq!(s, ThreadState::Done);

        // pending tool, even very idle, must NOT be Done
        let pending = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#;
        let (s2, _) = infer_claude(pending, DONE_AFTER_SECS + 9999);
        assert_ne!(s2, ThreadState::Done);
    }

    // S5 — Degraded: two error signals at the tail.
    #[test]
    fn claude_degraded_on_errors() {
        let t = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"API Error: overloaded"}],"stop_reason":"end_turn"}}
{"type":"hook_cancelled","attachment":{"hookName":"x"}}"#;
        let (s, _) = infer_claude(t, 10);
        assert_eq!(s, ThreadState::Degraded);
    }

    // S6 — Codex phases. final_answer with no question → AwaitingReview.
    #[test]
    fn codex_final_answer_is_awaiting_review() {
        let c = r#"{"type":"event_msg","payload":{"phase":"final_answer","message":"Done — tests pass."}}"#;
        let (s, note) = infer_codex(c, 20);
        assert_eq!(s, ThreadState::AwaitingReview);
        assert_eq!(note.as_deref(), Some("Done — tests pass."));
    }
    #[test]
    fn codex_final_answer_question_is_waiting() {
        let c = r#"{"type":"event_msg","payload":{"phase":"final_answer","message":"Build is green. Want me to deploy?"}}"#;
        let (s, note) = infer_codex(c, 20);
        assert_eq!(s, ThreadState::Waiting);
        assert_eq!(note.as_deref(), Some("Build is green. Want me to deploy?"));
    }
    #[test]
    fn codex_commentary_is_working() {
        let c = r#"{"type":"event_msg","payload":{"phase":"final_answer","message":"old"}}
{"type":"event_msg","payload":{"phase":"commentary","message":"running build"}}"#;
        let (s, _) = infer_codex(c, 5);
        assert_eq!(s, ThreadState::Working);
    }

    // S7 — pi ambiguous: stale toolUse with no timing → Unknown.
    #[test]
    fn pi_stale_tooluse_is_unknown() {
        let c = r#"{"type":"message","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","name":"memory_search"}]}}"#;
        let (s, _) = infer_pi(c, 9999);
        assert_eq!(s, ThreadState::Unknown);
    }
    #[test]
    fn pi_fresh_tooluse_is_working() {
        let c = r#"{"type":"message","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","name":"memory_search"}]}}"#;
        let (s, note) = infer_pi(c, 5);
        assert_eq!(s, ThreadState::Working);
        assert_eq!(note.as_deref(), Some("Running memory_search"));
    }
    #[test]
    fn pi_clean_stop_is_awaiting_review() {
        let c = r#"{"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"hi there"}]}}"#;
        let (s, _) = infer_pi(c, 5);
        assert_eq!(s, ThreadState::AwaitingReview);
    }

    #[test]
    fn empty_is_unknown() {
        assert_eq!(infer_claude("garbage\n{bad}", 1).0, ThreadState::Unknown);
        assert_eq!(infer_codex("", 1).0, ThreadState::Unknown);
        assert_eq!(infer_pi("", 1).0, ThreadState::Unknown);
    }
}
