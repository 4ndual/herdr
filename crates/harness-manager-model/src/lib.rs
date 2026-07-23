//! wasm-safe render model shared by the harness-manager host collector (which
//! inspects the focused Claude Code pane's session transcript, settings, and
//! process) and the harness-manager WASM plugin (which renders it). No I/O, no
//! clock, no ratatui — every display string is precomputed host-side, mirroring
//! the historial-dash model contract.

use serde::{Deserialize, Serialize};

/// Live-state color of a row's leading dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowState {
    /// Enabled and idle (green).
    Idle,
    /// Fired within the last ~2s (red flash so the user sees WHEN it runs).
    Active,
    /// Disabled / deferred / archived (dim).
    Off,
    /// No live-state concept for this row (no dot).
    None,
}

/// One row in a harness-manager section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

/// A titled section (Context, Session, Hooks, MCPs, Plugins, Skills).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSection {
    pub title: String,
    /// Right-aligned section summary (e.g. totals). May be empty.
    #[serde(default)]
    pub summary: String,
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
                rows: vec![HarnessRow {
                    key: "hook:0".to_string(),
                    label: "PreToolUse pkg-guard".to_string(),
                    meta: "x41 avg 180ms".to_string(),
                    state: RowState::Idle,
                    tooltip: "runs before every Bash call".to_string(),
                    openable: true,
                    can_disable: true,
                    indent: 0,
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
