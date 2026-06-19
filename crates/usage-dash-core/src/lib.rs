//! usage-dash embeddable core. **Phase 2 stub** — the real implementation is
//! extracted from `~/Projects/usage-dash/src/{state,model,stats,budget,sources,render}.rs`
//! and exposes: `refresh(FocusInput) -> UsageSnapshot`, `draw(f, area, &snap, &view)`,
//! and `handle(key, &mut view)` for the expand/tooltip interactions (U-F1–U-F15).
//!
//! Kept as a compiling placeholder so the workspace builds during Phase 1.

use dash_common::FocusInput;

/// Placeholder snapshot; Phase 2 replaces with the full usage model.
#[derive(Debug, Clone, Default)]
pub struct UsageSnapshot {
    pub focus: FocusInput,
}

/// Phase-2 contract: build a snapshot for the focused session.
pub fn refresh(focus: FocusInput) -> UsageSnapshot {
    UsageSnapshot { focus }
}
