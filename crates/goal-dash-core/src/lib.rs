//! goal-widget embeddable core. **Phase 2 stub** — the real implementation is
//! carved from `~/Projects/goal-widget/src/main.rs` (App, `.goal.json` resolver,
//! history/plans/time-gate loaders, proposals/constraints, the `Act` handlers,
//! and `claude_infer`/infer-tree/observe/sort) and exposes:
//! `refresh(FocusInput) -> GoalSnapshot`, `draw(f, area, &snap, &view)`,
//! and `handle(key, &mut model) -> Vec<Effect>` (G-F1–G-F16).
//!
//! Phase 2 carves the goal-widget core into the modules below. `refresh`,
//! `handle`, and `draw` remain the Phase-3 host contract and are wired as the
//! herdr fork gains its goal panel.

pub mod context;
pub mod infer;
pub mod model;
pub mod store;
pub mod timegate;

pub use model::{GoalDoc, GoalState, Links, Par, Proposal, St, Sub, Tree};
pub use store::GoalStore;

use dash_common::{Effect, FocusInput};

/// Placeholder snapshot; Phase 2 replaces with the full goal tree model.
#[derive(Debug, Clone, Default)]
pub struct GoalSnapshot {
    pub focus: FocusInput,
}

/// Phase-2 contract: build a snapshot for the focused session.
pub fn refresh(focus: FocusInput) -> GoalSnapshot {
    GoalSnapshot { focus }
}

/// Phase-2 contract: handle an action/key, returning host effects.
pub fn handle(_action: &str, _snap: &mut GoalSnapshot) -> Vec<Effect> {
    Vec::new()
}
