//! Native dashboard integration glue (the `feat/native-dashes` fork).
//!
//! Bridges herdr's [`AppState`] to the embeddable dashboard cores
//! (`goal-dash-core`, `usage-dash-core`) without leaking herdr types into them:
//! the cores only ever see a [`dash_common::FocusInput`]. This module builds
//! that input from the currently focused pane each frame, and (later phases)
//! routes the cores' [`dash_common::Effect`]s back onto herdr machinery.

use bytes::Bytes;
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

    let (agent, session_id) = match terminal.hook_authority.as_ref() {
        Some(ha) => {
            let agent = parse_agent_label(&ha.agent_label)
                .map(map_agent)
                .unwrap_or(AgentKind::None);
            // Only an Id-kind ref is a usable session id; Path refs key on cwd.
            let session_id = ha
                .session_ref
                .as_ref()
                .filter(|r| r.kind == AgentSessionRefKind::Id)
                .map(|r| r.value.clone());
            (agent, session_id)
        }
        None => (AgentKind::None, None),
    };

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
        RefreshOutcome::Replace(snap) => app.usage_snapshot = Some(snap),
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
}
