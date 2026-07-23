//! wasm-safe render model shared by the harness-manager host collector (which
//! inspects the focused Claude Code pane's session transcript, settings, and
//! process) and the harness-manager WASM plugin (which renders it). No I/O, no
//! clock, no ratatui — every display string is precomputed host-side, mirroring
//! the historial-dash model contract.

use serde::{Deserialize, Serialize};

/// Live-state color of a row's leading dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RowState {
    /// Enabled and idle (green).
    Idle,
    /// Fired within the last ~2s (red flash so the user sees WHEN it runs).
    Active,
    /// Disabled / deferred / archived (dim).
    Off,
    /// No live-state concept for this row (no dot).
    #[default]
    None,
}

/// One row in a harness-manager section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HarnessRow {
    /// Stable routing key (`"ctx:<n>"`, `"hook:<n>"`, `"mcp:<n>"`, `"plugin:<n>"`,
    /// `"skill:<n>"`) the host parses back out of a clicked `action_id` to find
    /// the source entry in its collector snapshot.
    pub key: String,
    /// Left-hand display text, already trimmed/truncatable by the renderer.
    pub label: String,
    /// Right-hand metadata text (sizes, counts, timings). May be empty.
    #[serde(default)]
    pub meta: String,
    /// Leading state dot.
    pub state: RowState,
    /// Hover tooltip text; empty hides the tooltip. Newlines split lines.
    #[serde(default)]
    pub tooltip: String,
    /// Whether a left-click opens this entry (host-resolved `harness:open:`).
    #[serde(default)]
    pub openable: bool,
    /// Whether a right-click offers the disable menu (session / profile),
    /// host-resolved via `harness:disable_session:` / `harness:disable_profile:`.
    #[serde(default)]
    pub can_disable: bool,
    /// Extra indentation level (context tree children render indented).
    #[serde(default)]
    pub indent: u8,
    /// Optional group this row belongs to (e.g. a hook event name like
    /// "PreToolUse"). Rows sharing a `group` are emitted contiguously so the UI
    /// can render one group header with the rows nested beneath it. `None` = the
    /// row stands alone. Backward-compatible: absent in old inputs.
    #[serde(default)]
    pub group: Option<String>,
    /// Tokens this row currently occupies in the live context window (loaded at
    /// conversation start). `None` when the row costs nothing now (e.g. a skill
    /// body that only loads on invoke) or when the cost is not determinable.
    #[serde(default)]
    pub tok_loaded: Option<u32>,
    /// Tokens this row would cost if invoked / recalled / fully loaded (e.g. a
    /// skill's SKILL.md body, a memory file recalled on demand). `None` when not
    /// applicable or not determinable.
    #[serde(default)]
    pub tok_full: Option<u32>,
    /// Short provenance for the token numbers — a measured real source, e.g.
    /// "desc 412B", "SKILL.md 8.1KB", "transcript usage", "calibrated est", or
    /// "not determinable". Empty hides it. Never a fabricated number's excuse.
    #[serde(default)]
    pub tok_src: Option<String>,
    /// When this row's context cost is incurred: "start" | "on-invoke" |
    /// "on-recall" | "on-spawn" | "deferred" | "evicted". `None` when it has no
    /// load event.
    #[serde(default)]
    pub load_when: Option<String>,
    /// For on-demand items (skill invoked, memory recalled, file read): a short
    /// human string for WHEN it last loaded this session (transcript timestamp,
    /// `HH:MM`). `None` for start-loaded or never-loaded rows.
    #[serde(default)]
    pub loaded_at: Option<String>,
    /// Config scope the item comes from: "global" (loads in every session —
    /// user-level settings/skills/agents/global CLAUDE.md) vs "session"
    /// (project files, this session's recalled memories / read files). `None`
    /// when scope has no meaning for the row.
    #[serde(default)]
    pub scope: Option<String>,
}

/// A titled section (Context, Session, Hooks, MCPs, Plugins, Skills).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSection {
    pub title: String,
    /// Right-aligned section summary (e.g. totals). May be empty.
    #[serde(default)]
    pub summary: String,
    /// Header aggregate string computed host-side (e.g. "x214 · 8.6s" for hooks,
    /// "12.4KB ~3.4k tok" for context, "1.2GB · 34% cpu" for the process tree).
    /// Distinct from `summary`: `agg` is the dense computed rollup the UI shows
    /// next to the section title; `summary` stays the simple count. Empty when
    /// there is nothing to aggregate. Backward-compatible.
    #[serde(default)]
    pub agg: Option<String>,
    /// Whether the UI should render this section collapsed by default (e.g. a
    /// long low-signal catalog). `false` = expanded. Backward-compatible.
    #[serde(default)]
    pub collapsed_default: bool,
    pub rows: Vec<HarnessRow>,
}

/// Full render model the host publishes each tick to `harness_input.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HarnessRenderModel {
    /// One-line target identity ("claude · <sid8> · <cwd basename>"). Empty when
    /// no Claude pane has been focused yet (the panel then shows a hint).
    #[serde(default)]
    pub target: String,
    pub sections: Vec<HarnessSection>,
}

impl HarnessRenderModel {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_roundtrips_through_json() {
        let model = HarnessRenderModel {
            target: "claude · 0ccc55a3 · herdr".to_string(),
            sections: vec![HarnessSection {
                title: "HOOKS".to_string(),
                summary: "7".to_string(),
                agg: Some("x214 · 8.6s".to_string()),
                collapsed_default: false,
                rows: vec![HarnessRow {
                    key: "hook:0".to_string(),
                    label: "pkg-guard".to_string(),
                    meta: "x41 avg 180ms".to_string(),
                    state: RowState::Idle,
                    tooltip: "runs before every Bash call".to_string(),
                    openable: true,
                    can_disable: true,
                    indent: 0,
                    group: Some("PreToolUse".to_string()),
                    tok_loaded: None,
                    tok_full: None,
                    tok_src: Some("not determinable".to_string()),
                    load_when: None,
                    loaded_at: None,
                    scope: Some("global".to_string()),
                }],
            }],
        };
        let json = serde_json::to_string(&model).expect("serialize");
        let back: HarnessRenderModel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(model, back);
    }

    #[test]
    fn default_model_is_empty() {
        assert!(HarnessRenderModel::default().is_empty());
    }
}
