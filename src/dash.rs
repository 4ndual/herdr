//! Native dashboard integration glue (the `feat/native-dashes` fork).
//!
//! Bridges herdr's [`AppState`] to the embeddable dashboard cores
//! (`goal-dash-core`, `usage-dash-core`) without leaking herdr types into them:
//! the cores only ever see a [`dash_common::FocusInput`]. This module builds
//! that input from the currently focused pane each frame, and (later phases)
//! routes the cores' [`dash_common::Effect`]s back onto herdr machinery.

use dash_common::{AgentKind, FocusInput};

use crate::agent_resume::AgentSessionRefKind;
use crate::app::AppState;
use crate::detect::{parse_agent_label, Agent};

/// Map herdr's detected agent to the core-facing [`AgentKind`]. Agents the
/// dashboards don't model collapse to [`AgentKind::None`] (treated as a plain
/// pane for source resolution).
#[allow(dead_code)] // consumed by focus_input, wired into the tick loop in P3.5
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
#[allow(dead_code)] // wired into the main tick loop in P3.5
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
