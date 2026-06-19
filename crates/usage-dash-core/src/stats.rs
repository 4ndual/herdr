//! Per-session STATS derivation from the focused session's OWN transcript.
//!
//! Scope: the currently focused session only — NOT global account usage. All
//! numbers come straight from the same Claude transcript JSONL the rest of the
//! app reads (`~/.claude/projects/<proj>/<sid>.jsonl`); nothing is scraped from
//! the statusline and nothing is fabricated.
//!
//! Two layers:
//!   • pure fold (`fold_stats` + `finalize`) — no IO, fully unit-tested.
//!   • incremental cache (`session_stats`) — keeps a per-key byte offset so each
//!     1 s refresh folds only the bytes appended since last read; resets on
//!     file-shrink / sid-change. This keeps the per-tick cost ~O(new bytes)
//!     even for multi-MB transcripts.
//!
//! Field paths (verified live 2026-06-17 against real transcripts):
//!   autocompact  : {type:"system", subtype:"compact_boundary", compactMetadata:{preTokens,postTokens}}
//!   tokens       : assistant message.usage.{output_tokens,input_tokens,cache_read_input_tokens,cache_creation_input_tokens}
//!   tool calls   : assistant message.content[] {type:"tool_use", name}
//!   agent launch : the above with name == "Agent" | "Task"
//!   agent done   : user record with toolUseResult.{agentId, agentType}
//!   timestamps   : every record's top-level `timestamp` (ISO-8601 UTC)

use crate::model::SessionStats;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

/// Running accumulator folded over transcript lines. The cache layer owns
/// `byte_offset`; the pure fold never touches it.
#[derive(Debug, Clone, Default)]
pub struct StatsAcc {
    pub byte_offset: u64,
    pub out_tokens: u64,
    pub ctx_tokens: u64, // latest assistant context size (input + both caches)
    pub cache_read: u64,
    pub cache_creation: u64,
    pub assistant_turns: u32,
    pub user_turns: u32,
    pub tool_calls: u32,
    pub autocompacts: u32,
    pub freed_tokens: u64,
    pub agent_launches: u32, // Agent/Task tool_use blocks seen
    pub agent_done: u32,     // toolUseResult.agentId records seen
    pub agent_types: BTreeSet<String>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub busy_secs: u64,
    pub idle_secs: u64,
    pub last_event_ts: Option<i64>, // for busy/idle gap attribution
}

/// Pure ISO-8601 UTC → unix seconds. Handles "YYYY-MM-DDTHH:MM:SS[.fff][Z|+00:00]".
/// No subprocess (unlike `util::iso2epoch_utc`) so it's cheap per-record and
/// testable without the `date` binary. Returns None on anything unparseable.
pub fn iso_to_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let y = num(0, 4)?;
    let mo = num(5, 7)?;
    let d = num(8, 10)?;
    let h = num(11, 13)?;
    let mi = num(14, 16)?;
    let se = num(17, 19)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm). `m` in 1..=12.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

fn is_meta(v: &serde_json::Value) -> bool {
    v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false)
}

/// Does a user record carry a tool_result block (i.e. it's a tool return, not a
/// genuine human prompt)?
fn has_tool_result(v: &serde_json::Value) -> bool {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .any(|it| it.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        })
        .unwrap_or(false)
}

/// Fold the given (complete-line) transcript text into the accumulator. The
/// caller must pass only whole, newline-terminated lines so no record is split.
pub fn fold_stats(mut acc: StatsAcc, text: &str) -> StatsAcc {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(iso_to_epoch);

        // session span
        if let Some(t) = ts {
            acc.first_ts = Some(acc.first_ts.map_or(t, |f| f.min(t)));
            acc.last_ts = Some(acc.last_ts.map_or(t, |l| l.max(t)));
        }

        // busy/idle: attribute the gap leading up to this record by what the
        // agent was doing during it. Assistant output or a tool return → busy;
        // a genuine user prompt → idle (agent waiting). Skip meta records.
        let advances = match typ {
            "assistant" => true,
            "user" => !is_meta(&v),
            _ => false,
        };
        if advances {
            if let Some(t) = ts {
                if let Some(prev) = acc.last_event_ts {
                    let gap = (t - prev).max(0) as u64;
                    let busy = typ == "assistant" || has_tool_result(&v);
                    if busy {
                        acc.busy_secs += gap;
                    } else {
                        acc.idle_secs += gap;
                    }
                }
                acc.last_event_ts = Some(t);
            }
        }

        match typ {
            "assistant" => {
                acc.assistant_turns += 1;
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                    acc.out_tokens += g("output_tokens");
                    acc.cache_read += g("cache_read_input_tokens");
                    acc.cache_creation += g("cache_creation_input_tokens");
                    let ctx = g("input_tokens")
                        + g("cache_read_input_tokens")
                        + g("cache_creation_input_tokens");
                    if ctx > 0 {
                        acc.ctx_tokens = ctx; // latest wins
                    }
                }
                if let Some(content) = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for item in content {
                        if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            acc.tool_calls += 1;
                            let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
                            if name == "Agent" || name == "Task" {
                                acc.agent_launches += 1;
                            }
                        }
                    }
                }
            }
            "user" => {
                if !is_meta(&v) && !has_tool_result(&v) {
                    acc.user_turns += 1;
                }
                if let Some(tur) = v.get("toolUseResult") {
                    if tur.get("agentId").and_then(|a| a.as_str()).is_some() {
                        acc.agent_done += 1;
                        if let Some(at) = tur.get("agentType").and_then(|a| a.as_str()) {
                            acc.agent_types.insert(at.to_string());
                        }
                    }
                }
            }
            "system" => {
                if v.get("subtype").and_then(|s| s.as_str()) == Some("compact_boundary") {
                    acc.autocompacts += 1;
                    if let Some(cm) = v.get("compactMetadata") {
                        let pre = cm.get("preTokens").and_then(|x| x.as_u64()).unwrap_or(0);
                        let post = cm.get("postTokens").and_then(|x| x.as_u64()).unwrap_or(0);
                        acc.freed_tokens += pre.saturating_sub(post);
                    }
                }
            }
            _ => {}
        }
    }
    acc
}

/// Project the accumulator into the renderable snapshot struct.
pub fn finalize(acc: &StatsAcc) -> SessionStats {
    let cache_total = acc.cache_read + acc.cache_creation;
    let cache_pct = if cache_total > 0 {
        Some(acc.cache_read as f32 / cache_total as f32 * 100.0)
    } else {
        None
    };
    let age_secs = match (acc.first_ts, acc.last_ts) {
        (Some(a), Some(b)) if b >= a => (b - a) as u64,
        _ => 0,
    };
    SessionStats {
        autocompacts: acc.autocompacts,
        freed_tokens: acc.freed_tokens,
        agents_running: acc.agent_launches.saturating_sub(acc.agent_done),
        agents_done: acc.agent_done,
        agent_types: acc.agent_types.len() as u32,
        out_tokens: acc.out_tokens,
        ctx_tokens: acc.ctx_tokens,
        cache_pct,
        assistant_turns: acc.assistant_turns,
        user_turns: acc.user_turns,
        tool_calls: acc.tool_calls,
        age_secs,
        busy_secs: acc.busy_secs,
        idle_secs: acc.idle_secs,
    }
}

// ---- incremental cache layer (IO) ----

// Per-key accumulator cache, keyed by the transcript path. Survives across the
// 1 s refresh ticks so we only read appended bytes each time.
static CACHE: Mutex<Option<HashMap<String, StatsAcc>>> = Mutex::new(None);

/// Incremental session stats for `key` (the transcript path string), reading the
/// file at `path`. Folds only bytes appended since the last call; resets the
/// accumulator if the file shrank (truncation / new session reusing the name).
pub fn session_stats(key: &str, path: &Path) -> Option<SessionStats> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();

    let mut guard = CACHE.lock().ok()?;
    let map = guard.get_or_insert_with(HashMap::new);
    let mut acc = map.remove(key).unwrap_or_default();

    // Reset on shrink/truncation (file replaced, or a new session reused the
    // path) — a stale offset past EOF would otherwise mis-read.
    if len < acc.byte_offset {
        acc = StatsAcc::default();
    }

    if len > acc.byte_offset && file.seek(SeekFrom::Start(acc.byte_offset)).is_ok() {
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_ok() {
            // Only fold up to the last complete line; leave a trailing
            // partial line for the next tick (advance offset to the last \n).
            let consumed = match buf.rfind('\n') {
                Some(i) => i + 1,
                None => 0,
            };
            if consumed > 0 {
                acc = fold_stats(acc, &buf[..consumed]);
                acc.byte_offset += consumed as u64;
            }
        }
    }

    let out = finalize(&acc);
    map.insert(key.to_string(), acc);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> String {
        format!(r#""timestamp":"{s}""#)
    }

    #[test]
    fn iso_parses_utc_variants() {
        // 2021-01-01T00:00:00Z = 1609459200
        assert_eq!(iso_to_epoch("2021-01-01T00:00:00Z"), Some(1609459200));
        assert_eq!(
            iso_to_epoch("2021-01-01T00:00:00.123456+00:00"),
            Some(1609459200)
        );
        assert_eq!(iso_to_epoch("2026-06-17T13:11:38.554Z"), Some(1781701898));
        assert_eq!(iso_to_epoch("garbage"), None);
    }

    #[test]
    fn autocompacts_count_and_freed_tokens() {
        let t = format!(
            "{{\"type\":\"system\",\"subtype\":\"compact_boundary\",{},\"compactMetadata\":{{\"preTokens\":288678,\"postTokens\":17739}}}}\n\
             {{\"type\":\"system\",\"subtype\":\"compact_boundary\",{},\"compactMetadata\":{{\"preTokens\":200000,\"postTokens\":20000}}}}\n\
             {{\"type\":\"system\",\"subtype\":\"turn_duration\",{}}}",
            ts("2026-06-17T14:35:29Z"), ts("2026-06-17T15:00:00Z"), ts("2026-06-17T15:01:00Z")
        );
        let s = finalize(&fold_stats(StatsAcc::default(), &t));
        assert_eq!(s.autocompacts, 2);
        assert_eq!(s.freed_tokens, 270939 + 180000);
    }

    #[test]
    fn tokens_summed_and_ctx_latest() {
        let t = format!(
            "{{\"type\":\"assistant\",{},\"message\":{{\"usage\":{{\"input_tokens\":60,\"cache_read_input_tokens\":38000,\"cache_creation_input_tokens\":2000,\"output_tokens\":100}}}}}}\n\
             {{\"type\":\"assistant\",{},\"message\":{{\"usage\":{{\"input_tokens\":2,\"cache_read_input_tokens\":149701,\"cache_creation_input_tokens\":4352,\"output_tokens\":250}}}}}}",
            ts("2026-06-17T13:00:00Z"), ts("2026-06-17T13:00:05Z")
        );
        let s = finalize(&fold_stats(StatsAcc::default(), &t));
        assert_eq!(s.out_tokens, 350);
        assert_eq!(s.ctx_tokens, 2 + 149701 + 4352); // latest assistant
                                                     // cache% = read/(read+creation) = (38000+149701)/(38000+149701+2000+4352)
        let expected = (38000.0 + 149701.0) / (38000.0 + 149701.0 + 2000.0 + 4352.0) * 100.0;
        assert!((s.cache_pct.unwrap() - expected).abs() < 0.01);
    }

    #[test]
    fn turns_and_tool_calls() {
        let t = format!(
            "{{\"type\":\"user\",{},\"message\":{{\"content\":\"hi\"}}}}\n\
             {{\"type\":\"assistant\",{},\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"Bash\",\"id\":\"t1\"}},{{\"type\":\"text\",\"text\":\"ok\"}}],\"usage\":{{\"output_tokens\":5}}}}}}\n\
             {{\"type\":\"user\",{},\"isMeta\":true,\"message\":{{\"content\":\"meta\"}}}}\n\
             {{\"type\":\"user\",{},\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t1\"}}]}}}}",
            ts("2026-06-17T13:00:00Z"), ts("2026-06-17T13:00:05Z"), ts("2026-06-17T13:00:06Z"), ts("2026-06-17T13:00:07Z")
        );
        let s = finalize(&fold_stats(StatsAcc::default(), &t));
        assert_eq!(s.assistant_turns, 1);
        assert_eq!(s.user_turns, 1); // genuine prompt only (meta + tool_result excluded)
        assert_eq!(s.tool_calls, 1);
    }

    #[test]
    fn agents_running_done_types() {
        // two Agent launches, one completes (Explore), so AR=1, AD=1, AU=1
        let t = format!(
            "{{\"type\":\"assistant\",{},\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"Agent\",\"id\":\"a1\"}},{{\"type\":\"tool_use\",\"name\":\"Agent\",\"id\":\"a2\"}}]}}}}\n\
             {{\"type\":\"user\",{},\"toolUseResult\":{{\"agentId\":\"ab8e\",\"agentType\":\"Explore\"}},\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"a1\"}}]}}}}",
            ts("2026-06-17T13:00:00Z"), ts("2026-06-17T13:05:00Z")
        );
        let s = finalize(&fold_stats(StatsAcc::default(), &t));
        assert_eq!(s.agents_running, 1);
        assert_eq!(s.agents_done, 1);
        assert_eq!(s.agent_types, 1);
    }

    #[test]
    fn busy_idle_from_gaps() {
        // user prompt @0, assistant @+30 (busy 30), assistant @+40 (busy +10),
        // user prompt @+100 (idle 60 leading to it)
        let t = format!(
            "{{\"type\":\"user\",{},\"message\":{{\"content\":\"go\"}}}}\n\
             {{\"type\":\"assistant\",{},\"message\":{{\"usage\":{{\"output_tokens\":1}}}}}}\n\
             {{\"type\":\"assistant\",{},\"message\":{{\"usage\":{{\"output_tokens\":1}}}}}}\n\
             {{\"type\":\"user\",{},\"message\":{{\"content\":\"next\"}}}}",
            ts("2026-06-17T13:00:00Z"),
            ts("2026-06-17T13:00:30Z"),
            ts("2026-06-17T13:00:40Z"),
            ts("2026-06-17T13:02:20Z")
        );
        let s = finalize(&fold_stats(StatsAcc::default(), &t));
        assert_eq!(s.busy_secs, 40); // 30 + 10
        assert_eq!(s.idle_secs, 100); // gap before the 2nd user prompt
        assert_eq!(s.age_secs, 140);
    }

    #[test]
    fn incremental_fold_equals_full_pass() {
        let l1 = format!(
            "{{\"type\":\"assistant\",{},\"message\":{{\"usage\":{{\"output_tokens\":100}}}}}}\n",
            ts("2026-06-17T13:00:00Z")
        );
        let l2 = format!(
            "{{\"type\":\"assistant\",{},\"message\":{{\"usage\":{{\"output_tokens\":200}}}}}}\n",
            ts("2026-06-17T13:00:05Z")
        );
        let full = finalize(&fold_stats(StatsAcc::default(), &format!("{l1}{l2}")));
        let part = fold_stats(StatsAcc::default(), &l1);
        let two = finalize(&fold_stats(part, &l2));
        assert_eq!(full.out_tokens, two.out_tokens);
        assert_eq!(full.out_tokens, 300);
        assert_eq!(full, two);
    }
}
