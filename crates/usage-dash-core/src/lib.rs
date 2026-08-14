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

pub use model::{
    Cli, CliVersion, CliVersions, Freshness, SessionStats, ThreadState, UsageSnapshot,
    VersionToken, Window,
};
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
    /// A version token that has been clicked once while OUTDATED and is now
    /// awaiting a confirming second click.
    ///
    /// This is the confirmation step for an action that interrupts a running
    /// agent (update + resume restarts the pane). It is rendered inline in the
    /// caption rather than as a toast on purpose: toast delivery is a user
    /// setting and can be `off`, and a confirmation prompt that can be silently
    /// dropped is worse than none at all.
    pub version_confirm: Option<VersionToken>,
    /// Set when a confirmed version click should actually run. The embedder
    /// (wasm guest / host) drains this and invokes the corresponding plugin
    /// action; core never executes anything itself.
    pub version_action: Option<VersionToken>,
    /// Set when a click asked to RE-VERIFY an already-current token (a forced
    /// refetch rather than an update). Drained the same way.
    pub version_verify: Option<VersionToken>,
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
            version_confirm: None,
            version_action: None,
            version_verify: None,
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
            // newest transcript for the focused cwd when the reported sid has no
            // transcript on disk (herdr can report a sid that never got a file),
            // so a live pane isn't stuck on a stale snapshot.
            // Both references are passed: herdr populates `session_id` from its
            // own usage keying and `session_ref` from the agent's native resume
            // reference, and which one is filled depends on how the pane was
            // adopted. Reading only `session_id` meant a pane that reported just
            // the native ref fell through to the cwd fallback and rendered a
            // SIBLING pane's session.
            let sid = focus.session_id.as_deref().filter(|s| !s.is_empty());
            let session_ref = focus.session_ref.as_deref().filter(|s| !s.is_empty());
            let cwd = focus
                .cwd
                .as_deref()
                .map(|c| c.to_string_lossy().into_owned());
            match sources::claude::snapshot_for_focus(
                &[sid, session_ref],
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
        // No usage source reads jcode sessions yet, so keep whatever the panel
        // already shows rather than blanking it — same sticky rule as a source
        // that yields nothing.
        AgentKind::Jcode => RefreshOutcome::KeepPrevious,
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
    handle_with(action, None, view)
}

/// Like [`handle`], but with the snapshot available so a click on a version
/// token can be resolved against that token's [`Freshness`].
///
/// Without a snapshot every version token reads as `Unknown`, which is inert —
/// a click can then neither arm an update nor claim the version is current.
pub fn handle_with(
    action: Action,
    snap: Option<&UsageSnapshot>,
    view: &mut UsageView,
) -> NeedsClear {
    let freshness = |token: VersionToken| -> Freshness {
        let Some(snap) = snap else {
            return Freshness::Unknown;
        };
        match token {
            VersionToken::Claude => snap.versions.claude.freshness(),
        }
    };
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
            // Version tokens first: they sit on the caption border, outside the
            // body rects, so ordering is for clarity rather than overlap.
            if let Some(token) = version_token_at(view, col, row) {
                return handle_version_click(token, freshness, view);
            }
            // A click anywhere else abandons a pending confirmation. Clicking
            // away is the natural "no", and leaving a live confirm armed across
            // unrelated interactions risks a later stray click restarting a pane.
            view.version_confirm = None;

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

/// Which version token, if any, covers `(col,row)` in the last drawn frame.
fn version_token_at(view: &UsageView, col: u16, row: u16) -> Option<VersionToken> {
    if view
        .hits
        .claude_version
        .map(|r| hit(r, col, row))
        .unwrap_or(false)
    {
        return Some(VersionToken::Claude);
    }
    None
}

/// Resolve a click on a version token.
///
/// * `Outdated` — the first click ARMS a confirmation and does nothing else;
///   only a second click on the same token requests the update. The update
///   restarts the pane, so it must never be one stray click away.
/// * `Current` — a click is a cheap "check again now" (forced refetch).
/// * `Unknown` — inert. We do not know whether an update exists, so offering
///   one would be a guess.
fn handle_version_click(
    token: VersionToken,
    freshness: impl Fn(VersionToken) -> Freshness,
    view: &mut UsageView,
) -> NeedsClear {
    match freshness(token) {
        Freshness::Outdated => {
            if view.version_confirm == Some(token) {
                view.version_confirm = None;
                view.version_action = Some(token);
            } else {
                // Arming a new token also disarms any other one.
                view.version_confirm = Some(token);
            }
            true
        }
        Freshness::Current => {
            view.version_confirm = None;
            view.version_verify = Some(token);
            true
        }
        Freshness::Unknown => {
            view.version_confirm = None;
            false
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

#[cfg(test)]
mod version_click_tests {
    use super::*;
    use ratatui::layout::Rect;

    const CLAUDE_RECT: Rect = Rect {
        x: 10,
        y: 5,
        width: 7,
        height: 1,
    };

    fn view_with_tokens() -> UsageView {
        let mut v = UsageView::default();
        v.hits.claude_version = Some(CLAUDE_RECT);
        v
    }

    fn snap_with(claude: (&str, &str)) -> UsageSnapshot {
        let mut s = UsageSnapshot::empty(Cli::Claude);
        s.versions = CliVersions {
            claude: CliVersion {
                current: Some(claude.0.into()),
                latest: Some(claude.1.into()),
                checked_age_secs: Some(0),
            },
        };
        s
    }

    /// The safety property this whole confirm step exists for: updating restarts
    /// the pane, so ONE click must never be enough to trigger it.
    #[test]
    fn one_click_on_an_outdated_token_only_arms_a_confirmation() {
        let mut v = view_with_tokens();
        let snap = snap_with(("2.1.219", "2.1.220"));

        handle_with(Action::Click(12, 5), Some(&snap), &mut v);
        assert_eq!(v.version_confirm, Some(VersionToken::Claude));
        assert_eq!(
            v.version_action, None,
            "a single click must not request the update"
        );
    }

    #[test]
    fn a_second_click_confirms_and_requests_the_update() {
        let mut v = view_with_tokens();
        let snap = snap_with(("2.1.219", "2.1.220"));

        handle_with(Action::Click(12, 5), Some(&snap), &mut v);
        handle_with(Action::Click(12, 5), Some(&snap), &mut v);
        assert_eq!(v.version_action, Some(VersionToken::Claude));
        assert_eq!(
            v.version_confirm, None,
            "the confirmation is consumed, not left armed"
        );
    }

    /// Clicking elsewhere is the natural "no". Leaving a confirm armed across
    /// unrelated clicks would let a later stray click restart the pane.
    #[test]
    fn clicking_away_disarms_the_confirmation() {
        let mut v = view_with_tokens();
        let snap = snap_with(("2.1.219", "2.1.220"));

        handle_with(Action::Click(12, 5), Some(&snap), &mut v);
        assert_eq!(v.version_confirm, Some(VersionToken::Claude));

        handle_with(Action::Click(99, 99), Some(&snap), &mut v);
        assert_eq!(v.version_confirm, None);
        assert_eq!(v.version_action, None);
    }

    #[test]
    fn clicking_a_current_token_asks_for_a_recheck_not_an_update() {
        let mut v = view_with_tokens();
        let snap = snap_with(("2.1.220", "2.1.220"));

        handle_with(Action::Click(12, 5), Some(&snap), &mut v);
        assert_eq!(v.version_verify, Some(VersionToken::Claude));
        assert_eq!(v.version_action, None);
        assert_eq!(v.version_confirm, None);
    }

    /// An unknown version offers no action at all — we do not know that an
    /// update exists, so proposing one would be a guess.
    #[test]
    fn clicking_an_unknown_token_does_nothing() {
        let mut v = view_with_tokens();
        let mut snap = UsageSnapshot::empty(Cli::Claude);
        snap.versions.claude.current = Some("2.1.220".into()); // latest never fetched

        let changed = handle_with(Action::Click(12, 5), Some(&snap), &mut v);
        assert!(!changed);
        assert_eq!(v.version_confirm, None);
        assert_eq!(v.version_action, None);
        assert_eq!(v.version_verify, None);
    }

    /// Without a snapshot every token is Unknown, so clicks stay inert rather
    /// than acting on stale geometry.
    #[test]
    fn clicks_are_inert_without_a_snapshot() {
        let mut v = view_with_tokens();
        assert!(!handle_with(Action::Click(12, 5), None, &mut v));
        assert_eq!(v.version_action, None);
    }
}
