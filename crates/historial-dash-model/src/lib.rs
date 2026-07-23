//! wasm-safe render model shared by the historial-dash host collector (which
//! builds it from `pane_close_history::read_recent`) and the historial-dash
//! WASM plugin (which renders it). No I/O, no clock, no ratatui — every display
//! string (including the relative time) is precomputed host-side, mirroring the
//! goal-dash model contract.

use serde::{Deserialize, Serialize};

/// One row in the historial panel: a previously-closed pane, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorialRow {
    /// Stable routing key `"<ts_unix_ms>:<pane_id>"`. The host parses this back
    /// out of a clicked hit-rect's `action_id` to find the source record.
    pub key: String,
    /// Display agent label (e.g. "claude", "codex") or a fallback like "shell".
    pub agent: String,
    /// Pane label / title, already trimmed for display.
    pub label: String,
    /// Precomputed compact relative time ("just now", "5m", "3h", "2d").
    pub reltime: String,
    /// Working directory of the closed pane (shown muted as the detail line).
    pub cwd: String,
    /// Whether a resume plan can be rebuilt for this row (has an official agent
    /// session ref). Non-restorable rows render without the restore/fork
    /// affordances.
    pub restorable: bool,
}

/// Full render model the host publishes each tick to `historial_input.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HistorialRenderModel {
    /// Up to N most-recent closed panes, newest first.
    pub rows: Vec<HistorialRow>,
}

impl HistorialRenderModel {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_roundtrips_through_json() {
        let model = HistorialRenderModel {
            rows: vec![HistorialRow {
                key: "1700000000000:w_1_p_2".to_string(),
                agent: "claude".to_string(),
                label: "herdr".to_string(),
                reltime: "5m".to_string(),
                cwd: "/home/dev/herdr".to_string(),
                restorable: true,
            }],
        };
        let json = serde_json::to_string(&model).expect("serialize");
        let back: HistorialRenderModel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(model, back);
    }

    #[test]
    fn default_model_is_empty() {
        assert!(HistorialRenderModel::default().is_empty());
    }
}
