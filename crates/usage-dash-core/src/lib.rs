//! usage-dash embeddable core. The real data + render logic extracted from
//! `~/Projects/usage-dash/src/{model,state,stats,budget,sources,render}.rs`,
//! driven by herdr's [`herdr_ui_plugin_api::FocusInput`] instead of cmux focus discovery.
//!
//! Public surface (Phase 2 contract, U-F1–U-F15):
//!   - [`refresh`] — resolve a [`FocusInput`] to a [`UsageSnapshot`]. Replaces the
//!     old `focus.rs`/`events.rs` cmux probing: the host already knows the focused
//!     pane's session id / cwd / agent, so we pick the source directly. Honors
//!     `is_self_focus` (sticky — caller keeps its previous snapshot).
//!   - [`draw`] — render a snapshot + interactive [`UsageView`] into a ratatui
//!     `Rect`. Reproduces the standalone panel exactly (STATE pill, ctx/5h/week/
//!     today bars, daily-budget %, AC/AR/AD/AU row, condensed stats line, the
//!     expandable state footer + Waiting question note, and the yellow tooltips).
//!   - [`handle`] — apply a UI [`Action`] (expand/collapse/stats toggle, scroll,
//!     hover, click) to a [`UsageView`].
//!
//! Data-layer modules (`model`, `state`, `stats`, `budget`, `sources`) are ported
//! verbatim from the standalone binary, including their unit tests. `focus.rs`
//! and `events.rs` are intentionally NOT ported — that role is now the host's.

pub mod budget;
pub mod model;
pub mod render;
pub mod sources;
pub mod state;
pub mod stats;
pub mod util;

pub use model::{Cli, SessionStats, ThreadState, UsageSnapshot, Window};
pub use render::{draw, Hits};

use herdr_ui_plugin_api::{AgentKind, FocusInput};
use ratatui::layout::Rect;
use ratatui::Frame;

/// Interactive UI state the host owns for the usage panel. Pure data (no PTYs,
/// no IO) so the host can construct/persist it freely. Mirrors the standalone
/// binary's loose `expanded/collapsed/stats_shown/scroll/hover` loop variables,
/// plus a cached `now` and the last frame's clickable [`Hits`].
#[derive(Debug, Clone)]
pub struct UsageView {
    /// Reveal the scrollable full state status under the footer (U-F11).
    pub expanded: bool,
    /// Fold the whole panel to a single summary line (U-F12, the `z` toggle).
    pub collapsed: bool,
    /// Show the compact STATS block (default on; U-F13, the `s` toggle).
    pub stats_shown: bool,
    /// Vertical scroll offset of the expanded full status.
    pub scroll: u16,
    /// Last hovered cell `(col, row)` for tooltip resolution (U-F14).
    pub hover: Option<(u16, u16)>,
    /// `now` (unix seconds) used for the countdown/abs-time trailers. The host
    /// refreshes this each frame; defaults to the real clock in [`UsageView::new`].
    pub now: i64,
    /// Clickable hit-rects from the last [`draw`] — used by [`handle`] to map a
    /// click onto the expand / stats toggle.
    pub hits: Hits,
}

impl Default for UsageView {
    fn default() -> Self {
        UsageView {
            expanded: false,
            collapsed: false,
            stats_shown: true,
            scroll: 0,
            hover: None,
            now: util::now_unix(),
            hits: Hits::default(),
        }
    }
}

impl UsageView {
    pub fn new() -> Self {
        Self::default()
    }
}

/// What [`refresh`] decided the host should do with its stored snapshot. Encodes
/// the standalone loop's sticky semantics: a self/dashboard-focused pane (or an
/// unresolved focus) keeps the previous snapshot rather than blanking the panel.
#[derive(Debug, Clone)]
pub enum RefreshOutcome {
    /// Replace the stored snapshot with this one. Boxed because `UsageSnapshot`
    /// is far larger than the other (unit) variants.
    Replace(Box<UsageSnapshot>),
    /// Keep the previously-stored snapshot (sticky: self-focus / unresolved).
    KeepPrevious,
    /// No focused agent session — clear the panel to the empty state.
    Clear,
}

/// Resolve a [`FocusInput`] to a usage snapshot decision.
///
/// Source selection (replaces cmux `focus.rs`):
///   - `AgentKind::Claude` -> by session id (the transcript `<sid>.jsonl`).
///   - `AgentKind::Codex`  -> by cwd (the matching rollout, newest mtime).
///   - `AgentKind::Pi`     -> by cwd (the sanitized session dir, newest jsonl).
///   - `AgentKind::None`   -> no agent session -> [`RefreshOutcome::Clear`].
///
/// `is_self_focus` is sticky: when the focused pane is the dashboard itself the
/// host keeps its previous snapshot (no flicker) -> [`RefreshOutcome::KeepPrevious`].
/// A missing id/cwd, or a source that yields nothing, is also treated as
/// sticky/keep rather than blanking — matching the standalone `Focused::Unknown`
/// arm. Only an explicit `AgentKind::None` clears the panel.
pub fn refresh(focus: FocusInput) -> RefreshOutcome {
    if focus.is_self_focus {
        return RefreshOutcome::KeepPrevious;
    }
    match focus.agent {
        AgentKind::None => RefreshOutcome::Clear,
        AgentKind::Claude => {
            // fork(wave-1): when the focused Claude pane is viewing a subagent,
            // `focus.subagent` is its name; the source overrides model + context
            // with that subagent's, keeping the account's plan-quota rows.
            // fork(wave-2): resolve by session id first, but fall back to the
            // newest transcript for the focused cwd across both roots when the
            // reported sid has no transcript on disk (herdr can report a sid that
            // never got a file), so a live cx pane isn't stuck on a stale snapshot.
            let sid = focus.session_id.as_deref().filter(|s| !s.is_empty());
            let cwd = focus
                .cwd
                .as_deref()
                .map(|c| c.to_string_lossy().into_owned());
            match sources::claude::snapshot_for_focus(
                sid,
                cwd.as_deref(),
                focus.subagent.as_deref(),
            ) {
                Some(s) => RefreshOutcome::Replace(Box::new(s)),
                None => RefreshOutcome::KeepPrevious,
            }
        }
        AgentKind::Codex => match focus.cwd.as_deref() {
            Some(cwd) => match sources::codex::snapshot_for_cwd(&cwd.to_string_lossy()) {
                Some(s) => RefreshOutcome::Replace(Box::new(s)),
                None => RefreshOutcome::KeepPrevious,
            },
            None => RefreshOutcome::KeepPrevious,
        },
        AgentKind::Pi => match focus.cwd.as_deref() {
            Some(cwd) => match sources::pi::snapshot_for_cwd(&cwd.to_string_lossy()) {
                Some(s) => RefreshOutcome::Replace(Box::new(s)),
                None => RefreshOutcome::KeepPrevious,
            },
            None => RefreshOutcome::KeepPrevious,
        },
    }
}

/// A UI intent for [`handle`]. The host maps its key/mouse events onto these;
/// keeping them abstract (not crossterm types) means the core never depends on a
/// terminal backend, only ratatui's geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Toggle the expanded full-status view (space/enter, U-F11). No-op when the
    /// last frame had no expandable state footer.
    ToggleExpand,
    /// Toggle the whole-panel collapse (the `z` key, U-F12).
    ToggleCollapse,
    /// Toggle the compact STATS block (the `s` key, U-F13).
    ToggleStats,
    /// Scroll the expanded full status down one row.
    ScrollDown,
    /// Scroll the expanded full status up one row.
    ScrollUp,
    /// Cursor moved to `(col, row)` — updates the hover target for tooltips (U-F14).
    Hover(u16, u16),
    /// Left click at `(col, row)` — toggles expand or stats if it lands on either
    /// clickable hit-rect from the last frame (U-F11/U-F13).
    Click(u16, u16),
}

/// Whether [`handle`] changed anything that requires the host to clear+redraw.
/// The standalone binary cleared the terminal on height-changing toggles
/// (expand/collapse/stats) to heal stray rows; the host should do the same when
/// this is `true`.
pub type NeedsClear = bool;

/// Apply a UI [`Action`] to the view. Returns whether the host should clear the
/// region before the next draw (height-changing toggles).
pub fn handle(action: Action, view: &mut UsageView) -> NeedsClear {
    match action {
        Action::ToggleExpand => {
            if view.hits.state.is_some() {
                view.expanded = !view.expanded;
                view.scroll = 0;
                true
            } else {
                false
            }
        }
        Action::ToggleCollapse => {
            view.collapsed = !view.collapsed;
            true
        }
        Action::ToggleStats => {
            view.stats_shown = !view.stats_shown;
            true
        }
        Action::ScrollDown => {
            if view.expanded {
                view.scroll = view.scroll.saturating_add(1);
            }
            false
        }
        Action::ScrollUp => {
            if view.expanded {
                view.scroll = view.scroll.saturating_sub(1);
            }
            false
        }
        Action::Hover(col, row) => {
            view.hover = Some((col, row));
            false
        }
        Action::Click(col, row) => {
            if view.hits.state.map(|r| hit(r, col, row)).unwrap_or(false) {
                view.expanded = !view.expanded;
                view.scroll = 0;
                true
            } else if view.hits.stats.map(|r| hit(r, col, row)).unwrap_or(false) {
                view.stats_shown = !view.stats_shown;
                true
            } else {
                false
            }
        }
    }
}

/// Is the (col,row) cell inside `r`? (ported from the standalone `main::hit`).
fn hit(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}

/// Convenience wrapper that renders into `area` and stores the resulting [`Hits`]
/// back on the view, so a later [`handle`] click resolves against this frame.
/// (The host may call [`render::draw`] directly instead and assign `view.hits`.)
pub fn render(f: &mut Frame, area: Rect, snap: Option<&UsageSnapshot>, view: &mut UsageView) {
    let hits = render::draw(f, area, snap, view);
    view.hits = hits;
    // Nothing expandable this frame -> drop stale expand/scroll state (mirrors the
    // standalone loop's `if hits.state.is_none()` cleanup).
    if view.hits.state.is_none() {
        view.expanded = false;
        view.scroll = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_focus_keeps_previous() {
        let focus = FocusInput {
            is_self_focus: true,
            agent: AgentKind::Claude,
            session_id: Some("abc".into()),
            ..Default::default()
        };
        assert!(matches!(refresh(focus), RefreshOutcome::KeepPrevious));
    }

    #[test]
    fn none_agent_clears() {
        let focus = FocusInput {
            agent: AgentKind::None,
            ..Default::default()
        };
        assert!(matches!(refresh(focus), RefreshOutcome::Clear));
    }

    #[test]
    fn claude_without_sid_keeps_previous() {
        let focus = FocusInput {
            agent: AgentKind::Claude,
            session_id: None,
            ..Default::default()
        };
        assert!(matches!(refresh(focus), RefreshOutcome::KeepPrevious));
    }

    #[test]
    fn toggle_collapse_and_stats_request_clear() {
        let mut v = UsageView::new();
        assert!(handle(Action::ToggleCollapse, &mut v));
        assert!(v.collapsed);
        assert!(handle(Action::ToggleStats, &mut v));
        assert!(!v.stats_shown);
    }

    #[test]
    fn expand_noop_without_state_hit() {
        let mut v = UsageView::new();
        // no hits.state this frame -> expand must not toggle, no clear requested
        assert!(!handle(Action::ToggleExpand, &mut v));
        assert!(!v.expanded);
    }

    #[test]
    fn expand_toggles_with_state_hit() {
        let mut v = UsageView::new();
        v.hits.state = Some(Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 1,
        });
        assert!(handle(Action::ToggleExpand, &mut v));
        assert!(v.expanded);
    }

    #[test]
    fn click_on_stats_hit_toggles_stats() {
        let mut v = UsageView::new();
        v.hits.stats = Some(Rect {
            x: 2,
            y: 3,
            width: 5,
            height: 1,
        });
        assert!(handle(Action::Click(3, 3), &mut v));
        assert!(!v.stats_shown);
        // a click outside any hit does nothing
        assert!(!handle(Action::Click(40, 40), &mut v));
    }

    #[test]
    fn scroll_only_when_expanded() {
        let mut v = UsageView::new();
        handle(Action::ScrollDown, &mut v);
        assert_eq!(v.scroll, 0); // not expanded -> ignored
        v.expanded = true;
        handle(Action::ScrollDown, &mut v);
        assert_eq!(v.scroll, 1);
        handle(Action::ScrollUp, &mut v);
        assert_eq!(v.scroll, 0);
    }
}
