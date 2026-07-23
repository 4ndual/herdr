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
    fn read_recent_missing_file_is_empty() {
        let path = std::env::temp_dir().join("herdr-pane-hist-does-not-exist.jsonl");
        let _ = std::fs::remove_file(&path);
        assert!(read_recent_from(&path, 5).is_empty());
    }

    #[test]
    fn relative_time_buckets() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - 5_000, now), "just now");
        assert_eq!(relative_time(now - 120_000, now), "2m");
        assert_eq!(relative_time(now - 3 * 3600_000, now), "3h");
        assert_eq!(relative_time(now - 2 * 86_400_000, now), "2d");
        // Future timestamp clamps.
        assert_eq!(relative_time(now + 10_000, now), "just now");
    }
}
