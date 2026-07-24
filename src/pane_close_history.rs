//! Server-side pane-close history ("historial").
//!
//! Every time a pane is torn down through a real close path (user- or
//! API-initiated pane/tab/workspace close — never server shutdown), the runtime
//! appends one JSON line describing the pane to a durable log so a later UI can
//! offer to restore or fork the session. This is a shared runtime/session fact
//! (per the runtime/client boundary guardrail), so it is recorded server-side
//! with a neutral name, independent of any TUI surface.
//!
//! The log lives at `state_dir()/pane-history.jsonl` (respecting
//! `XDG_STATE_HOME`, i.e. `~/.local/state/herdr/pane-history.jsonl` on Unix).
//! Writes are best-effort: an IO error is logged via `tracing` and never blocks
//! or panics the close path.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Basename of the append-only history log inside the state dir.
const HISTORY_FILE: &str = "pane-history.jsonl";

/// Cap how much of the tail we read back for the recent view, so a long-lived
/// log never forces an unbounded read.
#[allow(dead_code)] // consumed by the deferred historial panel (Feature B)
const MAX_READ_BYTES: u64 = 512 * 1024;

/// One recorded pane close. Serialized as a single JSON line.
///
/// `session_source`/`session_kind`/`session_value` mirror the API
/// `AgentSessionInfo` a live `pane get` exposes; they are absent for a plain
/// shell pane with no detected agent session. `session_source` is retained (on
/// top of the fields named in the feature spec) because it is required to
/// rebuild an `agent_resume` plan for restore/fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCloseRecord {
    pub ts_unix_ms: u64,
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_value: Option<String>,
    pub cwd: String,
    pub workspace_id: String,
    /// Workspace display name at close time (staged with the record so the
    /// historial hover tooltip can name where the pane last lived without a
    /// later workspace lookup). Absent on records written before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    /// First user prompt from the agent transcript (claude/cx sessions), cut
    /// char-safe host-side. Extracted once when the record is flushed; absent
    /// for shells/codex/pi and on older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
    /// Last user prompt from the agent transcript, same rules as `first_prompt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
}

impl PaneCloseRecord {
    /// Rebuild the [`crate::agent_resume::AgentResumePlan`] for this closed pane,
    /// if it carried an official (restorable) agent session. `None` for plain
    /// shells or non-official sources — such rows render without restore
    /// affordances.
    pub fn resume_plan(&self) -> Option<crate::agent_resume::AgentResumePlan> {
        use crate::agent_resume::{AgentSessionRef, AgentSessionRefKind};
        let source = self.session_source.as_deref()?;
        // A cx session is Claude Code launched through the gateway: it is
        // *persisted* with the display agent "cx" (so the row and the [x] filter
        // read cx), but it resumes through the same `herdr:claude` source —
        // cx-resume re-dispatches it back up as cx. Normalize cx -> claude here so
        // the plan lookup (and therefore `restorable()`, which gates whether the
        // row survives projection at all) treats it as the claude session it is.
        // Without this, every cx row is judged non-restorable and dropped from the
        // historial entirely.
        let agent = match self.agent.as_deref()? {
            "cx" => "claude",
            other => other,
        };
        let kind = match self.session_kind.as_deref()? {
            "id" => AgentSessionRefKind::Id,
            "path" => AgentSessionRefKind::Path,
            _ => return None,
        };
        let value = self.session_value.clone()?;
        crate::agent_resume::plan(source, agent, &AgentSessionRef { kind, value })
    }

    /// Whether a resume plan can be rebuilt (drives the restore/fork/delete row
    /// affordances in the historial panel).
    pub fn restorable(&self) -> bool {
        self.resume_plan().is_some()
    }

    /// Argv to FORK (branch) this session into a new tab, transforming the plain
    /// resume argv per agent. Agents without a known fork flag fall back to the
    /// resume argv (best-effort continue rather than nothing).
    pub fn fork_argv(&self) -> Option<Vec<String>> {
        let plan = self.resume_plan()?;
        let mut argv = plan.argv;
        match plan.agent.as_str() {
            // cx-resume <id> --fork-session
            "claude" => argv.push("--fork-session".to_string()),
            // codex resume <id> -> codex fork <id>
            "codex" => {
                if argv.get(1).map(|s| s == "resume").unwrap_or(false) {
                    argv[1] = "fork".to_string();
                }
            }
            // pi --session <path> -> pi --fork <path>
            "pi" => {
                if let Some(pos) = argv.iter().position(|s| s == "--session") {
                    argv[pos] = "--fork".to_string();
                }
            }
            _ => {}
        }
        Some(argv)
    }
}

/// Absolute path of the history log.
pub fn history_path() -> PathBuf {
    crate::config::state_dir().join(HISTORY_FILE)
}

/// Append `records` to the history log, one JSON line each. Best-effort: creates
/// the state dir if needed, and on any IO/serialization error logs a warning and
/// returns without propagating — the caller (a pane-close path) must never be
/// blocked by history IO.
pub fn append_records(records: &[PaneCloseRecord]) {
    if records.is_empty() {
        return;
    }
    append_records_to(&history_path(), records);
}

/// Append implementation against an explicit path (unit-testable).
fn append_records_to(path: &std::path::Path, records: &[PaneCloseRecord]) {
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                path = %parent.display(),
                error = %err,
                "failed to create pane-history state dir; skipping record"
            );
            return;
        }
    }

    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "failed to open pane-history log for append; skipping record"
            );
            return;
        }
    };

    let mut buf = String::new();
    for record in records {
        match serde_json::to_string(record) {
            Ok(line) => {
                buf.push_str(&line);
                buf.push('\n');
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to serialize pane-history record; skipping it"
                );
            }
        }
    }

    if !buf.is_empty() {
        if let Err(err) = file.write_all(buf.as_bytes()) {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "failed to append pane-history records"
            );
        }
    }
}

/// Read up to `limit` most-recent records, newest first. Best-effort: a missing
/// or unreadable log yields an empty list. Malformed lines are skipped.
#[allow(dead_code)] // historial panel query API (Feature B follow-up)
pub fn read_recent(limit: usize) -> Vec<PaneCloseRecord> {
    read_recent_from(&history_path(), limit)
}

/// Read implementation against an explicit path (unit-testable).
#[allow(dead_code)]
fn read_recent_from(path: &std::path::Path, limit: usize) -> Vec<PaneCloseRecord> {
    if limit == 0 {
        return Vec::new();
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    // Only read the tail so a large log stays cheap.
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut reader = std::io::BufReader::new(file);
    if len > MAX_READ_BYTES {
        use std::io::Seek;
        // Skip to the tail window; the first (likely partial) line is dropped.
        if reader
            .seek(std::io::SeekFrom::Start(len - MAX_READ_BYTES))
            .is_ok()
        {
            let mut discard = String::new();
            let _ = reader.read_line(&mut discard);
        }
    }

    let mut records: Vec<PaneCloseRecord> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<PaneCloseRecord>(trimmed) {
            records.push(record);
        }
    }

    records.reverse();
    records.truncate(limit);
    records
}

/// Remove the record whose `(ts_unix_ms, pane_id)` matches, rewriting the log.
/// Best-effort; returns whether a line was removed. Used by the historial
/// panel's right-click "delete" action.
#[allow(dead_code)] // historial right-click delete (Feature B follow-up)
pub fn delete_record(ts_unix_ms: u64, pane_id: &str) -> bool {
    delete_record_from(&history_path(), ts_unix_ms, pane_id)
}

#[allow(dead_code)]
fn delete_record_from(path: &std::path::Path, ts_unix_ms: u64, pane_id: &str) -> bool {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let reader = std::io::BufReader::new(file);
    let mut kept: Vec<String> = Vec::new();
    let mut removed = false;
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let matches = serde_json::from_str::<PaneCloseRecord>(trimmed)
            .map(|record| record.ts_unix_ms == ts_unix_ms && record.pane_id == pane_id)
            .unwrap_or(false);
        if matches {
            removed = true;
        } else {
            kept.push(line);
        }
    }
    if !removed {
        return false;
    }
    let mut body = kept.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    if let Err(err) = std::fs::write(path, body.as_bytes()) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "failed to rewrite pane-history log after delete"
        );
        return false;
    }
    true
}

/// Max chars kept for each extracted prompt (char-safe cut). The historial
/// hover tooltip renders these, so a very long first/last prompt is clipped
/// here once rather than bloating the durable log or the render model.
const PROMPT_MAX_CHARS: usize = 240;

/// Char-safe truncate to at most `max` chars (never splits a multibyte char).
fn truncate_chars(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    trimmed.chars().take(max).collect()
}

/// Base dirs that may hold a claude/cx session transcript, newest-preferred at
/// call sites. `~/.claude` plus every `~/.claude-code-router/profiles/*/claude`
/// (the cx router writes per-profile claude homes). Best-effort: unreadable dirs
/// are skipped.
fn claude_transcript_bases() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(home) = dirs_home() {
        bases.push(home.join(".claude"));
        let profiles = home.join(".claude-code-router").join("profiles");
        if let Ok(entries) = std::fs::read_dir(&profiles) {
            for entry in entries.flatten() {
                let claude = entry.path().join("claude");
                if claude.is_dir() {
                    bases.push(claude);
                }
            }
        }
    }
    bases
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Encode a cwd into the transcript project-dir name: every non-alphanumeric
/// byte becomes `-` (mirrors the claude-code / archive-pane.sh encoding).
fn encode_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Extract the first and last user prompt (char-safe, capped at
/// [`PROMPT_MAX_CHARS`]) from a claude/cx session transcript for `record`, if it
/// names an `id`-kind session whose jsonl can be located. Returns
/// `(first, last)`; either/both `None` when unavailable. Best-effort and
/// filesystem-only — never panics. Codex/pi/terminal records return `(None,
/// None)` (no claude transcript to read).
pub fn extract_prompts(record: &PaneCloseRecord) -> (Option<String>, Option<String>) {
    let Some((path, _is_cx)) = find_transcript(record) else {
        return (None, None);
    };
    read_user_prompts(&path).unwrap_or((None, None))
}

/// Locate the claude/cx session transcript jsonl for `record`, returning its
/// path and whether it is a cx (CCR router) transcript (found under
/// `~/.claude-code-router/profiles/*`). `None` for non-`id`/non-claude sessions
/// or when no file exists. A cx session is still reported by the runtime as
/// `agent = "claude"` (cx *is* Claude Code), so both agent labels are accepted.
fn find_transcript(record: &PaneCloseRecord) -> Option<(PathBuf, bool)> {
    session_transcript_path(
        record.agent.as_deref(),
        record.session_kind.as_deref(),
        record.session_value.as_deref(),
        &record.cwd,
    )
}

/// Locate the claude/cx session transcript jsonl for an `id`-kind claude/cx
/// session identified by `session_value` running in `cwd`. Returns the jsonl
/// path and whether it is a cx (CCR router) transcript. `None` for
/// non-`id`/non-claude sessions or when no file exists. Shared by the historial
/// close path and by restart auto-continue, which needs to inspect a live
/// resumed pane's transcript by its session ref.
pub fn session_transcript_path(
    agent: Option<&str>,
    session_kind: Option<&str>,
    session_value: Option<&str>,
    cwd: &str,
) -> Option<(PathBuf, bool)> {
    if session_kind != Some("id") {
        return None;
    }
    if !matches!(agent, Some("claude") | Some("cx")) {
        return None;
    }
    let value = session_value?;
    let enc = encode_cwd(cwd);
    for base in claude_transcript_bases() {
        let path = base
            .join("projects")
            .join(&enc)
            .join(format!("{value}.jsonl"));
        if path.is_file() {
            let is_cx = base
                .to_string_lossy()
                .contains(".claude-code-router/profiles/");
            return Some((path, is_cx));
        }
    }
    None
}

/// Whether a claude/cx transcript's last turn was left in flight (interrupted)
/// rather than finished. Scans for the last user/assistant record and returns:
/// a `user` record last (a prompt or tool result awaiting a response) → `true`;
/// an `assistant` record whose `stop_reason` is not a clean turn end
/// (`end_turn`/`stop_sequence`) — e.g. `tool_use`, `max_tokens`, or a partial
/// record with no `stop_reason` — → `true`; a cleanly ended assistant turn or a
/// transcript with no content records → `false`. Filesystem-only, best-effort,
/// never panics.
pub fn transcript_last_turn_interrupted(path: &std::path::Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    let mut last_interrupted: Option<bool> = None;
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if let Some(interrupted) = record_turn_interrupted(&value) {
            last_interrupted = Some(interrupted);
        }
    }
    last_interrupted.unwrap_or(false)
}

/// Classify a transcript entry as a content turn record and whether it leaves
/// the turn in flight. `Some(true)` = interrupted content record, `Some(false)`
/// = cleanly ended assistant turn, `None` = not a user/assistant content record
/// (meta records like `system`, `summary`, `last-prompt`, `queue-operation`).
fn record_turn_interrupted(value: &serde_json::Value) -> Option<bool> {
    let role = record_role(value)?;
    match role {
        "user" => Some(true),
        "assistant" => {
            let stop = value
                .get("message")
                .and_then(|m| m.get("stop_reason"))
                .and_then(|s| s.as_str());
            Some(!matches!(stop, Some("end_turn") | Some("stop_sequence")))
        }
        _ => None,
    }
}

/// The role of a content record: the flat `type: "user"|"assistant"` shape or
/// the nested `message.role` shape. `None` for non-content records.
fn record_role(value: &serde_json::Value) -> Option<&str> {
    if let Some(role) = value
        .get("message")
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
    {
        if matches!(role, "user" | "assistant") {
            return Some(role);
        }
    }
    match value.get("type").and_then(|t| t.as_str()) {
        Some(t @ ("user" | "assistant")) => Some(t),
        _ => None,
    }
}

/// Resolve the display agent kind for a claude/cx `record` from its transcript
/// location: `"cx"` when the transcript lives under the CCR router profiles,
/// `"claude"` when under `~/.claude`, `None` when neither applies (leave the
/// record's existing agent untouched — codex/pi/terminal). Filesystem-only,
/// best-effort; called once at flush time.
pub fn resolve_display_agent(record: &PaneCloseRecord) -> Option<String> {
    let (_, is_cx) = find_transcript(record)?;
    Some(if is_cx { "cx" } else { "claude" }.to_string())
}

/// Whether `record`'s session is a subagent run: its transcript jsonl is an
/// `agent-*` file (Claude Code stores subagent transcripts as `agent-<id>.jsonl`).
/// Derived purely from the session value, so it needs no filesystem access.
pub fn is_subagent_record(record: &PaneCloseRecord) -> bool {
    record
        .session_value
        .as_deref()
        .map(|v| v.starts_with("agent-"))
        .unwrap_or(false)
}

/// Read the user prompts out of a session jsonl. Returns `None` when the file is
/// missing/unreadable or has no user messages; otherwise `Some((first, last))`
/// with `last == first` collapsed to `None` when there is a single message.
fn read_user_prompts(path: &std::path::Path) -> Option<(Option<String>, Option<String>)> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut prompts: Vec<String> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if !is_user_message(&value) {
            continue;
        }
        if let Some(text) = message_text(&value) {
            let text = text.trim();
            if !text.is_empty() {
                prompts.push(text.to_string());
            }
        }
    }
    if prompts.is_empty() {
        return None;
    }
    let first = truncate_chars(&prompts[0], PROMPT_MAX_CHARS);
    let last = if prompts.len() == 1 {
        None
    } else {
        Some(truncate_chars(&prompts[prompts.len() - 1], PROMPT_MAX_CHARS))
    };
    Some((Some(first), last))
}

/// Whether a transcript entry is a user message (either the flat `type: "user"`
/// shape or the nested `message.role: "user"` shape).
fn is_user_message(value: &serde_json::Value) -> bool {
    value.get("type").and_then(|t| t.as_str()) == Some("user")
        || value
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            == Some("user")
}

/// Pull the display text out of a message entry: `message.content`/`content` may
/// be a plain string or an array of content blocks (join every `type: "text"`
/// block's `text`). Mirrors the archive-pane.sh jq extraction.
fn message_text(value: &serde_json::Value) -> Option<String> {
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| value.get("content"))?;
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let joined = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" ");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

/// Current wall-clock time in unix milliseconds (saturating; 0 on a clock
/// before the epoch, which is never expected).
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|dur| dur.as_millis() as u64)
        .unwrap_or(0)
}

/// Compact human relative-time label ("just now", "5m", "3h", "2d") for a past
/// millisecond timestamp relative to `now_ms`. Future timestamps clamp to
/// "just now".
#[allow(dead_code)] // historial row rendering (Feature B follow-up)
pub fn relative_time(ts_unix_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(ts_unix_ms) / 1000;
    if secs < 45 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m", (secs / 60).max(1))
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: u64, pane: &str) -> PaneCloseRecord {
        PaneCloseRecord {
            ts_unix_ms: ts,
            pane_id: pane.to_string(),
            label: Some("claude · herdr".to_string()),
            agent: Some("claude".to_string()),
            session_source: Some("herdr:claude".to_string()),
            session_kind: Some("id".to_string()),
            session_value: Some("abc-123".to_string()),
            cwd: "/home/dev/herdr".to_string(),
            workspace_id: "ws-1".to_string(),
            workspace_name: Some("herdr".to_string()),
            first_prompt: None,
            last_prompt: None,
        }
    }

    #[test]
    fn record_serializes_to_single_json_line_with_expected_fields() {
        let line = serde_json::to_string(&sample(1_700_000_000_000, "pane-7")).unwrap();
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ts_unix_ms"], 1_700_000_000_000u64);
        assert_eq!(value["pane_id"], "pane-7");
        assert_eq!(value["agent"], "claude");
        assert_eq!(value["session_kind"], "id");
        assert_eq!(value["session_value"], "abc-123");
        assert_eq!(value["cwd"], "/home/dev/herdr");
        assert_eq!(value["workspace_id"], "ws-1");
    }

    #[test]
    fn optional_agent_session_fields_are_omitted_when_absent() {
        let mut record = sample(1, "p");
        record.agent = None;
        record.session_source = None;
        record.session_kind = None;
        record.session_value = None;
        record.label = None;
        let value: serde_json::Value = serde_json::to_value(&record).unwrap();
        assert!(value.get("agent").is_none());
        assert!(value.get("session_kind").is_none());
        assert!(value.get("label").is_none());
        // Required fields stay present.
        assert!(value.get("cwd").is_some());
        assert!(value.get("workspace_id").is_some());
    }

    #[test]
    fn append_then_read_recent_returns_newest_first() {
        let dir = std::env::temp_dir().join(format!("herdr-pane-hist-{}", now_unix_ms()));
        let path = dir.join("pane-history.jsonl");
        append_records_to(&path, &[sample(10, "a"), sample(20, "b")]);
        append_records_to(&path, &[sample(30, "c")]);

        let recent = read_recent_from(&path, 10);
        let ids: Vec<&str> = recent.iter().map(|r| r.pane_id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b", "a"]);

        let limited = read_recent_from(&path, 2);
        let ids: Vec<&str> = limited.iter().map(|r| r.pane_id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_recent_skips_malformed_lines() {
        let dir = std::env::temp_dir().join(format!("herdr-pane-hist-bad-{}", now_unix_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pane-history.jsonl");
        let good = serde_json::to_string(&sample(5, "ok")).unwrap();
        std::fs::write(&path, format!("{good}\nnot json\n\n")).unwrap();

        let recent = read_recent_from(&path, 10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].pane_id, "ok");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_record_removes_only_the_matching_line() {
        let dir = std::env::temp_dir().join(format!("herdr-pane-hist-del-{}", now_unix_ms()));
        let path = dir.join("pane-history.jsonl");
        append_records_to(&path, &[sample(1, "a"), sample(2, "b"), sample(3, "c")]);

        assert!(delete_record_from(&path, 2, "b"));
        let after = read_recent_from(&path, 10);
        let remaining: Vec<&str> = after.iter().map(|r| r.pane_id.as_str()).collect();
        assert_eq!(remaining, vec!["c", "a"]);

        // A non-matching delete is a no-op.
        assert!(!delete_record_from(&path, 999, "z"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_plan_rebuilds_for_official_agent_session() {
        let record = sample(1, "p");
        let plan = record.resume_plan().expect("claude session is restorable");
        assert_eq!(plan.argv, vec!["cx-resume", "abc-123"]);
        assert!(record.restorable());
    }

    #[test]
    fn resume_plan_treats_cx_display_agent_as_restorable_claude() {
        // Regression: a cx session persists with display agent "cx" but source
        // "herdr:claude". It must stay restorable (else it is dropped from the
        // historial) and resume through cx-resume, exactly like a claude row.
        let mut record = sample(1, "p");
        record.agent = Some("cx".to_string());
        record.session_source = Some("herdr:claude".to_string());
        assert!(record.restorable(), "cx rows must be restorable");
        let plan = record.resume_plan().expect("cx session resolves a plan");
        assert_eq!(plan.argv, vec!["cx-resume", "abc-123"]);
        assert_eq!(
            record.fork_argv().expect("cx forkable"),
            vec!["cx-resume", "abc-123", "--fork-session"]
        );
    }

    #[test]
    fn resume_plan_absent_without_session_fields() {
        let mut record = sample(1, "p");
        record.session_source = None;
        record.session_kind = None;
        record.session_value = None;
        assert!(record.resume_plan().is_none());
        assert!(!record.restorable());
    }

    #[test]
    fn fork_argv_appends_fork_flag_for_claude() {
        let record = sample(1, "p");
        assert_eq!(
            record.fork_argv().expect("forkable"),
            vec!["cx-resume", "abc-123", "--fork-session"]
        );
    }

    #[test]
    fn fork_argv_transforms_codex_and_pi() {
        let mut codex = sample(1, "p");
        codex.session_source = Some("herdr:codex".to_string());
        codex.agent = Some("codex".to_string());
        codex.session_value = Some("cx-1".to_string());
        assert_eq!(
            codex.fork_argv().expect("forkable"),
            vec!["codex", "fork", "cx-1"]
        );

        let mut pi = sample(1, "p");
        pi.session_source = Some("herdr:pi".to_string());
        pi.agent = Some("pi".to_string());
        pi.session_kind = Some("path".to_string());
        pi.session_value = Some("/tmp/pi-session.jsonl".to_string());
        assert_eq!(
            pi.fork_argv().expect("forkable"),
            vec!["pi", "--fork", "/tmp/pi-session.jsonl"]
        );
    }

    #[test]
    fn read_recent_missing_file_is_empty() {
        let path = std::env::temp_dir().join("herdr-pane-hist-does-not-exist.jsonl");
        let _ = std::fs::remove_file(&path);
        assert!(read_recent_from(&path, 5).is_empty());
    }

    #[test]
    fn read_user_prompts_extracts_first_and_last_across_shapes() {
        let dir = std::env::temp_dir().join(format!("herdr-hist-prompts-{}", now_unix_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        // Mix flat `type:user` (string content) and nested `message.role:user`
        // (array-of-blocks content); an assistant line must be ignored.
        let body = concat!(
            r#"{"type":"user","message":{"role":"user","content":"first hello"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":"ignored"}}"#,
            "\n",
            r#"{"message":{"role":"user","content":[{"type":"text","text":"middle"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"last bye"}]}}"#,
            "\n",
        );
        std::fs::write(&path, body).unwrap();

        let (first, last) = read_user_prompts(&path).expect("prompts present");
        assert_eq!(first.as_deref(), Some("first hello"));
        assert_eq!(last.as_deref(), Some("last bye"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_user_prompts_single_message_has_no_last() {
        let dir = std::env::temp_dir().join(format!("herdr-hist-prompts-one-{}", now_unix_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"only\"}}\n",
        )
        .unwrap();
        let (first, last) = read_user_prompts(&path).expect("one prompt");
        assert_eq!(first.as_deref(), Some("only"));
        assert!(last.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_prompts_skips_non_id_and_non_claude() {
        let mut rec = sample(1, "p");
        rec.session_kind = Some("path".to_string());
        assert_eq!(extract_prompts(&rec), (None, None));
        let mut rec = sample(1, "p");
        rec.agent = Some("codex".to_string());
        assert_eq!(extract_prompts(&rec), (None, None));
    }

    #[test]
    fn is_subagent_record_detects_agent_prefixed_session() {
        let mut rec = sample(1, "p");
        rec.session_value = Some("agent-deadbeef-1234".to_string());
        assert!(is_subagent_record(&rec));
        rec.session_value = Some("11112222-3333".to_string());
        assert!(!is_subagent_record(&rec));
        rec.session_value = None;
        assert!(!is_subagent_record(&rec));
    }

    #[test]
    fn encode_cwd_maps_non_alnum_to_dashes() {
        assert_eq!(encode_cwd("/home/dev/herdr"), "-home-dev-herdr");
        assert_eq!(encode_cwd("/a_b.c"), "-a-b-c");
    }

    #[test]
    fn relative_time_buckets() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - 5_000, now), "just now");
        assert_eq!(relative_time(now - 120_000, now), "2m");
        assert_eq!(relative_time(now - 3 * 3_600_000, now), "3h");
        assert_eq!(relative_time(now - 2 * 86_400_000, now), "2d");
        // Future timestamp clamps.
        assert_eq!(relative_time(now + 10_000, now), "just now");
    }

    fn write_transcript(name: &str, lines: &[&str]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herdr-transcript-test-{}-{}.jsonl",
            name,
            now_unix_ms()
        ));
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn interrupted_when_last_content_record_is_a_user_prompt() {
        let path = write_transcript(
            "user-last",
            &[
                r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":"do the next thing"}}"#,
                r#"{"type":"last-prompt"}"#,
            ],
        );
        assert!(transcript_last_turn_interrupted(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn not_interrupted_when_last_assistant_turn_ended_cleanly() {
        let path = write_transcript(
            "assistant-end",
            &[
                r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"hello"}]}}"#,
                r#"{"type":"last-prompt"}"#,
                r#"{"type":"queue-operation"}"#,
            ],
        );
        assert!(!transcript_last_turn_interrupted(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn interrupted_when_last_assistant_record_awaits_tool_use() {
        let path = write_transcript(
            "assistant-tooluse",
            &[
                r#"{"type":"user","message":{"role":"user","content":"run it"}}"#,
                r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
            ],
        );
        assert!(transcript_last_turn_interrupted(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn interrupted_when_last_assistant_record_is_partial_without_stop_reason() {
        let path = write_transcript(
            "assistant-partial",
            &[
                r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"..."}]}}"#,
            ],
        );
        assert!(transcript_last_turn_interrupted(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn not_interrupted_when_no_content_records_or_file_missing() {
        let path = write_transcript(
            "meta-only",
            &[
                r#"{"type":"system"}"#,
                r#"{"type":"summary"}"#,
                r#"{"type":"last-prompt"}"#,
            ],
        );
        assert!(!transcript_last_turn_interrupted(&path));
        let _ = std::fs::remove_file(&path);

        let missing = std::env::temp_dir().join("herdr-transcript-test-does-not-exist.jsonl");
        assert!(!transcript_last_turn_interrupted(&missing));
    }
}
