//! wasm-safe serde data model for the goal dashboard.
//!
//! This is the per-frame render projection the host (`goal-dash-core::state::App`)
//! hands the WASM render plugin each frame, serialized into the
//! `HERDR_GOAL_MODEL_JSON` env key. It is a pure-data leaf: **no I/O, no ratatui,
//! no chrono, no `std::process`/`std::fs`** — only `serde`. Every value the native
//! `render.rs` reads off `App` to draw a frame appears here, with all
//! clock/filesystem-derived values (`minutes_until`, `fmt_updated`) precomputed
//! host-side and shipped as plain fields/strings.
//!
//! The guest renderer accumulates the hit-rect caches into [`GoalHits`] and ships
//! them back to the host (via the layer's free-form `data` field), which writes
//! them onto `App` so the existing `App::handle(GoalAction::Click)` resolves
//! clicks unchanged.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// View / node / target enums (mirrors of goal-dash-core::view, made serde-safe)
// ---------------------------------------------------------------------------

/// Mirror of `goal_dash_core::view::View`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum View {
    Tree,
    History,
    Proposals,
    PromptLog,
}

/// Mirror of `goal_dash_core::model::Par` (sub-goal parent bucket).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Par {
    Current,
    Main,
    Indep,
}

/// Mirror of `goal_dash_core::model::St` (sub-goal lifecycle state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum St {
    Active,
    Paused,
    Done,
    Discarded,
}

/// Mirror of `goal_dash_core::view::Node` (a flattened tree row).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Main,
    Current,
    Section(Par, St),
    Sub(u64),
}

/// Mirror of `goal_dash_core::view::EditTarget` (what an in-progress edit writes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EditTarget {
    Main,
    Current,
    Sub(u64),
    NewSub,
    TimeMain,
    TimeCurrent,
    ConstraintMain,
    ConstraintCurrent,
}

/// Serde mirror of `goal_dash_core::view::Act`. Carried verbatim in [`GoalHits`]
/// so the host can map a chip/control click back to the native `Act`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActTag {
    Approve,
    Edit,
    Infer,
    Log,
    Copy,
    AddSub,
    State,
    Discard,
    ClearRebrief,
    Sort,
}

// ---------------------------------------------------------------------------
// Data projections (plain copies of goal model data the renderer reads)
// ---------------------------------------------------------------------------

/// A goal node's render-relevant fields (subset of `model::GoalState` plus the
/// host-precomputed time labels for its meta line — the guest can't call chrono).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalNodeData {
    pub text: String,
    pub source: String,
    pub approved: bool,
    /// `set_at` raw stamp (only used through `set_label`).
    #[serde(default)]
    pub set_label: String,
    /// `updated_at` raw stamp (only used through `changed_label`).
    #[serde(default)]
    pub changed_label: String,
    /// `goal_dash_core::timegate::est_label(est_minutes)` precomputed host-side.
    #[serde(default)]
    pub est_label: String,
    /// `short_time(deadline)` or "—" precomputed host-side.
    #[serde(default)]
    pub due_label: String,
    /// `minutes_until(deadline)` precomputed host-side (None ⇒ "/time-gate to estimate").
    #[serde(default)]
    pub minutes_until: Option<i64>,
    /// True when CURRENT was changed by the Goal Observer (source==agent && reason).
    #[serde(default)]
    pub agent_changed: bool,
}

/// A sub-goal's render-relevant fields (mirror of `model::Sub`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubData {
    pub id: u64,
    pub text: String,
    pub source: String,
    pub approved: bool,
    pub state: String,
    pub parent: String,
}

/// A per-goal constraint (mirror of `model::GoalConstraint`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConstraintData {
    pub entity: String,
    pub text: String,
    pub source: String,
}

/// A pending proposal (mirror of `model::Proposal`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProposalData {
    pub kind: String,
    pub text: String,
    pub reason: String,
}

/// A prompt-log entry (mirror of `model::PromptEntry`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptEntryData {
    pub ts: String,
    pub paraphrase: String,
    pub action: String,
    pub reason: String,
}

/// A goal history entry (subset of `model::GoalState`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryData {
    pub text: String,
    pub source: String,
    pub approved: bool,
    pub updated_at: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A project-scoped plan row (subset of `timegate::PlanMeta` plus precomputed
/// labels; the guest can't call `fmt_updated`/`est_label`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanData {
    pub basename: String,
    pub title: String,
    /// `fmt_updated(mtime_ms)` precomputed host-side.
    pub updated_label: String,
    /// `est_label(p50)` precomputed host-side.
    pub est_label: String,
    /// True for the newest plan (i==0): gets the ▶ "current" marker + green title.
    #[serde(default)]
    pub is_current: bool,
    /// True when this plan's body is expanded (current-unless-folded / in expanded set).
    pub expanded: bool,
    /// The lazily-cached body, present only when expanded (else None).
    #[serde(default)]
    pub body: Option<String>,
}

/// A contextual chip rendered under the selected node (mirror of `view::ChipSpec`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChipData {
    pub act: ActTag,
    pub glyph: String,
    pub tip: String,
}

// ---------------------------------------------------------------------------
// The complete render projection
// ---------------------------------------------------------------------------

/// Everything the goal renderer reads off `App` to draw one frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalRenderModel {
    pub view: View,
    /// `app.current_sid.is_some()` — when false, the tree shows the no-session hint.
    pub has_session: bool,
    /// MAIN goal (None ⇒ default/unset).
    pub main: Option<GoalNodeData>,
    /// CURRENT goal.
    pub current: GoalNodeData,
    /// All sub-goals (the renderer flattens these via the same section logic).
    pub subs: Vec<SubData>,
    /// Sections flipped from their default (all-collapsed) state.
    pub toggled: Vec<(Par, St)>,
    pub constraints: Vec<ConstraintData>,
    /// Entity keys whose constraints section is unfolded.
    pub cons_expanded: Vec<String>,
    /// (entity, index) of the constraint highlighted for delete.
    pub cons_sel: Option<(String, usize)>,
    pub proposals: Vec<ProposalData>,
    pub sel_prop: usize,
    /// Proposal indices whose reason dropdown is open.
    pub prop_expanded: Vec<usize>,
    pub plog: Vec<PromptEntryData>,
    pub history: Vec<HistoryData>,
    pub sel_hist: usize,
    /// The precomputed visible node list (rebuilt host-side via `rebuild_nodes`).
    pub nodes: Vec<Node>,
    pub sel_node: usize,
    pub sel_ctrl: usize,
    /// Chip specs for the selected node (empty when none / section selected).
    pub sel_chips: Vec<ChipData>,
    /// Resolved project label for plans; "" ⇒ hide the plans section.
    pub plans_label: String,
    pub plans: Vec<PlanData>,
    pub plans_section_open: bool,
    pub paraphrase: String,
    pub status: String,
    pub editing: bool,
    pub edit_target: EditTarget,
    pub edit_buf: String,
    /// Hover position (col,row, 0-based) from the latest mouse-move; drives tooltips.
    pub hover: Option<(u16, u16)>,
}

impl Default for View {
    fn default() -> Self {
        View::Tree
    }
}

impl Default for EditTarget {
    fn default() -> Self {
        EditTarget::Main
    }
}

// ---------------------------------------------------------------------------
// Hit-rect transport (serde-friendly Rect + all of App's rect caches)
// ---------------------------------------------------------------------------

/// A serde-friendly rect (ratatui `Rect` is not shipped across the wasm boundary).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RectData {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl RectData {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        RectData {
            x,
            y,
            width,
            height,
        }
    }
}

/// All hit-rect caches the native `draw` produces, with serde-friendly payloads.
/// The host writes these back onto `App` (via `apply_hits`) so the existing
/// `App::handle(GoalAction::Click)` resolves clicks unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalHits {
    pub node_rects: Vec<(RectData, usize)>,
    pub cons_header_rects: Vec<(RectData, String)>,
    pub chip_rects: Vec<(RectData, ActTag)>,
    pub controls: Vec<(RectData, ActTag)>,
    pub plans_head_rect: Option<RectData>,
    pub plans_row_rects: Vec<(RectData, String)>,
    pub plog_rects: Vec<(RectData, usize)>,
    pub prop_rects: Vec<(RectData, usize)>,
    pub hist_rects: Vec<(RectData, usize)>,
    pub banner_rect: Option<RectData>,
    pub tip_rects: Vec<(RectData, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_json_round_trips() {
        let mut m = GoalRenderModel::default();
        m.view = View::Tree;
        m.has_session = true;
        m.current = GoalNodeData {
            text: "ship it".into(),
            source: "user".into(),
            approved: true,
            est_label: "2h".into(),
            due_label: "06-21 18:00".into(),
            minutes_until: Some(45),
            ..Default::default()
        };
        m.nodes = vec![
            Node::Main,
            Node::Section(Par::Current, St::Active),
            Node::Sub(3),
        ];
        m.sel_chips = vec![ChipData {
            act: ActTag::Edit,
            glyph: "✎".into(),
            tip: "edit".into(),
        }];
        let json = serde_json::to_string(&m).unwrap();
        let back: GoalRenderModel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.current.text, "ship it");
        assert_eq!(back.nodes.len(), 3);
        assert_eq!(back.sel_chips[0].act, ActTag::Edit);
    }

    #[test]
    fn hits_json_round_trips() {
        let mut h = GoalHits::default();
        h.node_rects.push((RectData::new(0, 1, 40, 1), 2));
        h.chip_rects
            .push((RectData::new(1, 2, 3, 1), ActTag::Approve));
        h.banner_rect = Some(RectData::new(0, 0, 20, 1));
        let json = serde_json::to_string(&h).unwrap();
        let back: GoalHits = serde_json::from_str(&json).unwrap();
        assert_eq!(back.node_rects[0].1, 2);
        assert_eq!(back.chip_rects[0].1, ActTag::Approve);
        assert_eq!(back.banner_rect.unwrap().width, 20);
    }
}
