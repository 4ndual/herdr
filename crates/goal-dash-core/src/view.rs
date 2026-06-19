//! View-layer enums shared by the goal Model (`state.rs`) and its renderer
//! (`render.rs`), carved from goal-widget `src/main.rs` (~2055–2102). The
//! `Par`/`St` parent/state enums and their helpers live in [`crate::model`];
//! this module holds only the things the TUI adds on top: the action set, the
//! flattened-tree row type, edit/history targets, and the active view.

use ratatui::style::Color;

use crate::model::{Par, St};

/// An action. Item actions (Approve/Edit/Infer/Log/State/Discard) surface as
/// icon chips under the selected node; the bottom bar shows whole-session ones.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Approve,
    Edit,
    Infer,
    Log,
    AddSub,
    State,
    Discard,
    ClearRebrief,
    Sort,
}

/// The single bottom bar: only whole-session actions. (action, label, hover tooltip.)
pub const SESSION: [(Act, &str, &str); 3] = [
    (
        Act::ClearRebrief,
        " ⟲ Clear ",
        "clear & re-brief — a TRUE /clear, then re-brief the session",
    ),
    (
        Act::Sort,
        " ⌖ Sort ",
        "auto-sort — let the agent reorganize the goal tree",
    ),
    (Act::AddSub, " ＋ Sub ", "add a subgoal under the current goal"),
];

/// A contextual chip rendered under the selected node: its action, icon glyph, and hover tooltip.
#[derive(Clone)]
pub struct ChipSpec {
    pub act: Act,
    pub glyph: String,
    pub tip: String,
}

/// A selectable row in the flattened tree.
#[derive(Clone, Copy, PartialEq)]
pub enum Node {
    Main,
    Current,
    Section(Par, St), // collapsible group header
    Sub(u64),
}

/// Which entity a ☰ Log view is showing (so restore writes back to the right place).
#[derive(Clone, PartialEq)]
pub enum HistTarget {
    Main,
    Current,
    Sub(u64),
}

/// What an in-progress edit will write when committed.
#[derive(Clone, PartialEq)]
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

#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Tree,
    History,
    Proposals,
    PromptLog,
}

/// The source/approval badge glyph + colour for a goal node.
pub fn badge_marker(source: &str, approved: bool) -> (&'static str, Color) {
    if approved {
        ("✓", Color::Green)
    } else if source == "user" {
        ("•", Color::Green)
    } else {
        ("~", Color::Yellow)
    }
}
