//! The guest's view of the world: the snapshot `scripts/doctor.sh` writes into
//! the module state dir and the host injects as `HERDR_PLUGIN_UI_INPUT_JSON`.
//!
//! The guest is a zero-import wasm module: it cannot read `fixers.json`, cannot
//! spawn a process, and cannot know a wall clock. Everything it renders comes
//! from this one blob. That is also why the degradation rules live here and not
//! in the renderer — parsing NEVER fails, it downgrades:
//!
//! | input                          | result                                  |
//! |--------------------------------|-----------------------------------------|
//! | env var absent / empty         | `Loading` — "running checks…"           |
//! | not valid JSON                 | `Failed` — the parse error, verbatim    |
//! | valid JSON, `error` set        | `Failed` — the runner's own message     |
//! | valid JSON, `rows` empty       | `Empty` — "no fixers defined"           |
//! | valid JSON with rows           | `Ready`                                 |
//!
//! No input shape can panic the guest, so no input shape can take down the UI.

use serde::Deserialize;

/// The four states a fixer row can be in. `Working` is the state a row enters
/// the moment a fix (or its escalation to an agent) is in flight — the point of
/// the whole panel is that this is *visible* instead of dumping the user into a
/// terminal to watch a command scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixerState {
    Healthy,
    Broken,
    Unknown,
    Working,
}

impl FixerState {
    /// Parse the runner's `state` string. Anything unrecognised is `Unknown`
    /// rather than an error — a future runner may add states this build has
    /// never heard of, and an unreadable row must still render.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "healthy" => Self::Healthy,
            "broken" => Self::Broken,
            "working" => Self::Working,
            _ => Self::Unknown,
        }
    }

    /// True when clicking the row should run its `fix`.
    pub fn is_fixable(self) -> bool {
        matches!(self, Self::Broken)
    }
}

/// One row of the panel: one entry of `fixers.json` plus its current check
/// result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixerRow {
    pub id: String,
    pub label: String,
    pub state: FixerState,
    /// One human-readable line from the fixer's `detail` command; rendered as
    /// the row's subtitle.
    pub detail: String,
    /// Runner-supplied annotation, e.g. "escalated to an agent". Rendered after
    /// the detail when present.
    pub note: Option<String>,
}

/// Why the panel has nothing useful to show. Each variant renders its own
/// informative empty state; none of them is a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No snapshot on disk yet (first open, or the runner has not finished).
    Loading,
    /// The manifest or the snapshot could not be read. Carries the message the
    /// user needs in order to fix it themselves.
    Failed(String),
    /// The manifest parsed but declares no fixers.
    Empty,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub status: Status,
    /// Where the runner looked for the fixers manifest. Shown in every empty /
    /// failed state so the user knows which file to create or repair.
    pub manifest_path: String,
    /// Opaque generation marker (the runner's write timestamp). The guest never
    /// interprets it as a time — it only compares it for change, to expire the
    /// optimistic "working" flag it sets on click.
    pub generation: u64,
    pub rows: Vec<FixerRow>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: Status::Loading,
            manifest_path: String::new(),
            generation: 0,
            rows: Vec::new(),
        }
    }
}

/// How many rows sit in each state. `working` is counted separately from
/// `unknown` on purpose: "an agent is on it" is a different fact from "we could
/// not tell", and the collapsed button is the only place the user sees it while
/// the overlay is shut.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rollup {
    pub healthy: usize,
    pub broken: usize,
    pub unknown: usize,
    pub working: usize,
}

impl Snapshot {
    /// Per-state counts for the button and the overlay title.
    pub fn rollup(&self) -> Rollup {
        let mut out = Rollup::default();
        for row in &self.rows {
            match row.state {
                FixerState::Healthy => out.healthy += 1,
                FixerState::Broken => out.broken += 1,
                FixerState::Working => out.working += 1,
                FixerState::Unknown => out.unknown += 1,
            }
        }
        out
    }
}

// ── wire types ───────────────────────────────────────────────────────────────
// Deliberately permissive: every field optional, unknown fields ignored, so a
// newer runner writing extra keys never blanks the panel on an older guest.

#[derive(Debug, Deserialize)]
struct WireSnapshot {
    #[serde(default)]
    manifest_path: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    rows: Vec<WireRow>,
}

#[derive(Debug, Deserialize)]
struct WireRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    note: Option<String>,
}

/// Turn the injected blob into a [`Snapshot`]. Total: never panics, never
/// returns an error — the failure modes ARE render states.
pub fn parse(raw: Option<&str>) -> Snapshot {
    let raw = match raw {
        Some(raw) if !raw.trim().is_empty() => raw,
        _ => return Snapshot::default(),
    };
    let wire: WireSnapshot = match serde_json::from_str(raw) {
        Ok(wire) => wire,
        Err(err) => {
            return Snapshot {
                status: Status::Failed(format!("snapshot is not valid json: {err}")),
                ..Default::default()
            }
        }
    };

    let rows: Vec<FixerRow> = wire
        .rows
        .into_iter()
        .filter(|row| !row.id.is_empty())
        .map(|row| FixerRow {
            label: if row.label.is_empty() {
                row.id.clone()
            } else {
                row.label
            },
            id: row.id,
            state: FixerState::parse(&row.state),
            detail: row.detail,
            note: row.note.filter(|note| !note.is_empty()),
        })
        .collect();

    // A runner-reported error wins over row content: it means the manifest is
    // missing or malformed and whatever rows survived are not trustworthy.
    let status = match wire.error {
        Some(error) if !error.is_empty() => Status::Failed(error),
        _ if rows.is_empty() => Status::Empty,
        _ => Status::Ready,
    };

    Snapshot {
        status,
        manifest_path: wire.manifest_path,
        generation: wire.generation,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_input_is_loading_not_a_crash() {
        assert_eq!(parse(None).status, Status::Loading);
        assert_eq!(parse(Some("")).status, Status::Loading);
        assert_eq!(parse(Some("   \n ")).status, Status::Loading);
    }

    #[test]
    fn malformed_json_degrades_to_failed_with_the_reason() {
        let snap = parse(Some("{ this is not json"));
        match snap.status {
            Status::Failed(message) => assert!(message.contains("not valid json"), "{message}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(snap.rows.is_empty());
    }

    #[test]
    fn truncated_and_hostile_payloads_never_panic() {
        for raw in [
            "null",
            "[]",
            "{}",
            r#"{"rows":null}"#,
            r#"{"rows":[{}]}"#,
            r#"{"rows":[{"id":"cx","state":"nonsense"}]}"#,
            r#"{"rows":"cx"}"#,
            "{\"rows\":[{\"id\":\"cx\"",
        ] {
            let _ = parse(Some(raw));
        }
    }

    #[test]
    fn runner_error_beats_rows() {
        let snap = parse(Some(
            r#"{"manifest_path":"/tmp/fixers.json","error":"fixers.json not found","rows":[]}"#,
        ));
        assert_eq!(
            snap.status,
            Status::Failed("fixers.json not found".to_string())
        );
        assert_eq!(snap.manifest_path, "/tmp/fixers.json");
    }

    #[test]
    fn empty_manifest_is_empty_not_failed() {
        let snap = parse(Some(r#"{"manifest_path":"/tmp/f.json","rows":[]}"#));
        assert_eq!(snap.status, Status::Empty);
    }

    #[test]
    fn rows_parse_all_four_states() {
        let snap = parse(Some(
            r#"{"generation":7,"rows":[
                {"id":"cx","label":"cx / CCR","state":"healthy","detail":"gateway up"},
                {"id":"hr","label":"hr","state":"broken","detail":"socket missing"},
                {"id":"disk","label":"disk","state":"unknown","detail":"no smartctl"},
                {"id":"net","label":"net","state":"working","detail":"repairing","note":"escalated"}
            ]}"#,
        ));
        assert_eq!(snap.status, Status::Ready);
        assert_eq!(snap.generation, 7);
        let states: Vec<FixerState> = snap.rows.iter().map(|r| r.state).collect();
        assert_eq!(
            states,
            vec![
                FixerState::Healthy,
                FixerState::Broken,
                FixerState::Unknown,
                FixerState::Working
            ]
        );
        assert_eq!(snap.rows[3].note.as_deref(), Some("escalated"));
        assert_eq!(
            snap.rollup(),
            Rollup {
                healthy: 1,
                broken: 1,
                unknown: 1,
                working: 1,
            }
        );
    }

    #[test]
    fn unknown_state_string_falls_back_to_unknown() {
        assert_eq!(FixerState::parse("teleporting"), FixerState::Unknown);
        assert_eq!(FixerState::parse(""), FixerState::Unknown);
    }

    #[test]
    fn rows_without_an_id_are_dropped_and_missing_labels_fall_back() {
        let snap = parse(Some(
            r#"{"rows":[{"state":"broken"},{"id":"cx","state":"broken"}]}"#,
        ));
        assert_eq!(snap.rows.len(), 1);
        assert_eq!(snap.rows[0].label, "cx");
    }

    #[test]
    fn only_broken_rows_are_click_to_fix() {
        assert!(FixerState::Broken.is_fixable());
        assert!(!FixerState::Healthy.is_fixable());
        assert!(!FixerState::Unknown.is_fixable());
        assert!(!FixerState::Working.is_fixable());
    }
}
