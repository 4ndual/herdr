//! Native dashboard integration glue (the `feat/native-dashes` fork).
//!
//! Bridges herdr's [`AppState`] to the embeddable dashboard cores
//! (`goal-dash-core`, `usage-dash-core`) without leaking herdr types into them:
//! the cores only ever see a [`dash_common::FocusInput`]. This module builds
//! that input from the currently focused pane each frame, and (later phases)
//! routes the cores' [`dash_common::Effect`]s back onto herdr machinery.

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use dash_common::{AgentKind, ClaudeMode, Effect, FocusInput};
use ratatui::layout::Rect;
use ratatui::Frame;
use usage_dash_core::RefreshOutcome;

use crate::agent_resume::AgentSessionRefKind;
use crate::app::{App, AppState};
use crate::detect::{parse_agent_label, Agent};

/// Map herdr's detected agent to the core-facing [`AgentKind`]. Agents the
/// dashboards don't model collapse to [`AgentKind::None`] (treated as a plain
/// pane for source resolution).
fn map_agent(agent: Agent) -> AgentKind {
    match agent {
        Agent::Claude => AgentKind::Claude,
        Agent::Codex => AgentKind::Codex,
        Agent::Pi => AgentKind::Pi,
        _ => AgentKind::None,
    }
}

/// Build the [`FocusInput`] for the focused pane of the active workspace.
///
/// Pure read over [`AppState`]: resolves active workspace → focused pane →
/// attached terminal, then reads that terminal's cwd and hook authority (agent
/// label + session ref). Returns the default (nothing focused) input whenever
/// any link in that chain is absent, so the cores simply hold their last
/// snapshot rather than thrash.
pub fn focus_input(state: &AppState) -> FocusInput {
    let Some(ws_idx) = state.active else {
        return FocusInput::default();
    };
    let Some(ws) = state.workspaces.get(ws_idx) else {
        return FocusInput::default();
    };
    let Some(pane_id) = ws.focused_pane_id() else {
        return FocusInput::default();
    };
    let Some(terminal_id) = ws.terminal_id(pane_id) else {
        return FocusInput::default();
    };
    let Some(terminal) = state.terminals.get(terminal_id) else {
        return FocusInput::default();
    };

    // herdr resolves the focused agent's session through two distinct paths:
    // the full-lifecycle `hook_authority` and the reserved native-source path
    // (`herdr:claude` / `herdr:codex` / …) that the shipped claude/codex/pi
    // integrations actually use — the latter lands in `persisted_agent_session`,
    // not `hook_authority`. `current_session_identity_for_persistence()` is
    // herdr's own accessor that unifies both, so the dash reads it instead of a
    // single field (reading only `hook_authority` left the strip blank for every
    // real agent). Fall back to the effective detected agent so a freshly
    // detected pane with no session ref yet still surfaces its agent.
    let identity = terminal.current_session_identity_for_persistence();
    let agent = identity
        .as_ref()
        .and_then(|(_, label, _, _)| parse_agent_label(label))
        .or_else(|| terminal.effective_known_agent())
        .map(map_agent)
        .unwrap_or(AgentKind::None);
    // Only an Id-kind ref is a usable session id; Path refs key on cwd.
    let session_id = identity
        .filter(|(_, _, kind, _)| *kind == AgentSessionRefKind::Id)
        .map(|(_, _, _, value)| value);

    FocusInput {
        session_id,
        cwd: Some(terminal.cwd.clone()),
        agent,
        // Set true only when a herdr dashboard region itself holds focus; wired
        // when input routing lands (P3.6). Until then a pane is always focused.
        is_self_focus: false,
    }
}

/// Refresh both dashboard cores from the focused pane and execute any effects
/// the goal core queues. Called on the dash tick (~1 Hz) from the main loop.
///
/// - Usage: resolve the [`FocusInput`] to a snapshot, applying the sticky
///   semantics ([`RefreshOutcome`]) — replace, keep, or clear the stored one.
/// - Goal: `refresh` adopts the focused session, then `poll` re-reads the goal
///   and plan files, runs the periodic observer, and finalizes a pending sort;
///   both drain into the same effect queue.
pub fn tick(app: &mut App) {
    let focus = focus_input(&app.state);

    match usage_dash_core::refresh(focus.clone()) {
        RefreshOutcome::Replace(snap) => app.usage_snapshot = Some(*snap),
        RefreshOutcome::Clear => app.usage_snapshot = None,
        RefreshOutcome::KeepPrevious => {}
    }

    app.goal_dash.refresh(focus);
    let effects = app.goal_dash.poll();
    run_effects(app, effects);
}

/// Carry out the host side of the goal core's [`Effect`]s.
fn run_effects(app: &mut App, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            // State already mutated in-core; the tick repaints regardless.
            Effect::Refresh => {}
            Effect::Status(msg) => tracing::debug!(target: "dash", "{msg}"),
            Effect::SendPaneKeys { lines } => send_pane_keys(app, lines),
            Effect::SpawnClaude { mode, sid } => spawn_claude(mode, sid),
        }
    }
}

/// Type each line into the focused agent's pane followed by a carriage return
/// (the clear+rebrief sequence). Non-blocking: a full pane input buffer drops
/// the keys rather than stalling the render loop.
fn send_pane_keys(app: &mut App, lines: Vec<String>) {
    let Some(ws_idx) = app.state.active else {
        return;
    };
    let Some(runtime) = app
        .state
        .focused_runtime_in_workspace(&app.terminal_runtimes, ws_idx)
    else {
        return;
    };
    for mut line in lines {
        line.push('\r');
        let _ = runtime.try_send_bytes(Bytes::from(line.into_bytes()));
    }
}

/// Run the matching headless `claude -p` for a goal inference action off the
/// async runtime (the `run_*` entry points block on a subprocess and persist to
/// the goal file). Fire-and-forget: the next [`tick`] reloads the written file
/// via `poll`, so the UI never blocks on the call.
fn spawn_claude(mode: ClaudeMode, sid: String) {
    tokio::task::spawn_blocking(move || match mode {
        ClaudeMode::Infer => {
            goal_dash_core::infer::run_infer(Some(sid));
        }
        ClaudeMode::InferTree => {
            goal_dash_core::infer::run_infer_tree(&sid);
        }
        ClaudeMode::Observe => {
            goal_dash_core::infer::run_observe(&sid);
        }
        ClaudeMode::Sort => {
            goal_dash_core::infer::run_sort(&sid);
        }
    });
}

/// Is `(col, row)` inside a non-empty rect?
fn in_rect(r: Rect, col: u16, row: u16) -> bool {
    r.width > 0
        && r.height > 0
        && col >= r.x
        && col < r.x + r.width
        && row >= r.y
        && row < r.y + r.height
}

/// Map a crossterm key to a goal-panel intent. `None` for keys the panel
/// ignores (so the host can fall through to its own handling).
fn key_to_goal_action(key: &KeyEvent) -> Option<goal_dash_core::GoalAction> {
    use goal_dash_core::GoalAction as G;
    Some(match key.code {
        KeyCode::Up => G::Up,
        KeyCode::Down => G::Down,
        KeyCode::Left => G::Left,
        KeyCode::Right | KeyCode::Tab => G::Right,
        KeyCode::Enter => G::Enter,
        KeyCode::Char(' ') => G::Space,
        KeyCode::Esc => G::Back,
        KeyCode::Backspace => G::Backspace,
        KeyCode::Char(c) => G::Char(c),
        _ => return None,
    })
}

/// Route a key to the goal panel when it holds keyboard focus. Returns true if
/// the key was consumed (so the host must not also forward it to the pane).
///
/// Esc at the tree root releases focus back to the focused pane; in a sub-view
/// it backs out one level (handled by the core). The caller is responsible for
/// not routing the prefix key here, so the leader chord still works.
pub fn route_key_if_focused(app: &mut App, key: &KeyEvent) -> bool {
    if !(app.state.goal_panel_focused && app.state.goal_panel_visible) {
        return false;
    }
    if key.code == KeyCode::Esc && app.goal_dash.is_tree_root() {
        app.state.goal_panel_focused = false;
        return true;
    }
    let Some(action) = key_to_goal_action(key) else {
        return false;
    };
    handle_goal(app, action);
    true
}

/// Route a mouse event onto whichever dashboard region it lands in. Returns true
/// if consumed. A left click on the goal panel also grabs keyboard focus; moves
/// update hover for tooltips. Events outside both regions are left for the host.
pub fn route_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    let (col, row) = (mouse.column, mouse.row);

    if in_rect(app.state.view.usage_dash_area, col, row) {
        use usage_dash_core::Action as U;
        let action = match mouse.kind {
            MouseEventKind::Moved => U::Hover(col, row),
            MouseEventKind::Down(MouseButton::Left) => U::Click(col, row),
            MouseEventKind::ScrollDown => U::ScrollDown,
            MouseEventKind::ScrollUp => U::ScrollUp,
            _ => return true, // swallow other events over the strip
        };
        usage_dash_core::handle(action, &mut app.usage_view);
        return true;
    }

    if in_rect(app.state.view.goal_dash_area, col, row) {
        use goal_dash_core::GoalAction as G;
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.state.goal_panel_focused = true;
        }
        let action = match mouse.kind {
            MouseEventKind::Moved => G::Hover(col, row),
            MouseEventKind::Down(MouseButton::Left) => G::Click(col, row),
            _ => return true,
        };
        handle_goal(app, action);
        return true;
    }

    false
}

/// Apply a UI [`GoalAction`](goal_dash_core::GoalAction) to the goal core and
/// run any effects it returns.
fn handle_goal(app: &mut App, action: goal_dash_core::GoalAction) {
    let effects = app.goal_dash.handle(action);
    run_effects(app, effects);
}

/// Draw the native dashboard regions for this frame, called from the app's
/// render closure after the main UI is painted (so they sit on top of the panes
/// area, never under them). Each region is skipped when its rect is empty
/// (`compute_view` collapses the usage strip when there's no room and the goal
/// panel when it's toggled off). Both cores record their hit-rects back onto the
/// passed-in view/model so a later `handle` resolves clicks against this frame.
pub fn draw(
    frame: &mut Frame,
    usage_area: Rect,
    goal_area: Rect,
    goal: &mut goal_dash_core::App,
    usage_view: &mut usage_dash_core::UsageView,
    usage_snapshot: Option<&usage_dash_core::UsageSnapshot>,
) {
    if usage_area.width > 0 && usage_area.height > 0 {
        usage_dash_core::render(frame, usage_area, usage_snapshot, usage_view);
    }
    if goal_area.width > 0 && goal_area.height > 0 {
        goal_dash_core::draw(frame, goal_area, goal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::AppState;

    #[test]
    fn agent_label_maps_known_agents_and_collapses_rest() {
        assert_eq!(map_agent(Agent::Claude), AgentKind::Claude);
        assert_eq!(map_agent(Agent::Codex), AgentKind::Codex);
        assert_eq!(map_agent(Agent::Pi), AgentKind::Pi);
        assert_eq!(map_agent(Agent::Gemini), AgentKind::None);
        assert_eq!(map_agent(Agent::Cursor), AgentKind::None);
    }

    #[test]
    fn no_active_workspace_yields_default_focus() {
        let state = AppState::test_new(); // active: None, no workspaces
        let fi = focus_input(&state);
        assert_eq!(fi.agent, AgentKind::None);
        assert!(fi.session_id.is_none());
        assert!(fi.cwd.is_none());
        assert!(!fi.is_self_focus);
    }

    /// Regression guard: the shipped claude/codex/pi integrations report their
    /// session through reserved native sources (`herdr:claude`, …), which land
    /// in the terminal's `persisted_agent_session` rather than `hook_authority`.
    /// `focus_input` must resolve the agent + session id from that path too —
    /// reading only `hook_authority` left the native dashes blank for every real
    /// agent.
    #[test]
    fn focus_input_resolves_reserved_source_persisted_session() {
        let mut state = AppState::test_with_adversarial_identity_state();
        let ws = state
            .active
            .and_then(|i| state.workspaces.get(i))
            .expect("active workspace");
        let pane_id = ws.focused_pane_id().expect("focused pane");
        let term_id = ws.terminal_id(pane_id).expect("terminal id").clone();

        let sid = "11111111-1111-1111-1111-111111111111";
        state
            .terminals
            .get_mut(&term_id)
            .expect("terminal state")
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "herdr:claude".to_string(),
                agent: "claude".to_string(),
                session_ref: crate::agent_resume::AgentSessionRef::id(sid)
                    .expect("valid session id"),
            });

        let fi = focus_input(&state);
        assert_eq!(fi.agent, AgentKind::Claude);
        assert_eq!(fi.session_id.as_deref(), Some(sid));
    }
}
