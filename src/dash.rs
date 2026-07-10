//! Focused-session bridge glue (the `feat/native-dashes` fork).
//!
//! Builds the [`dash_common::FocusInput`] the WASM UI bridge delivers to every
//! plugin (the `FocusedSession` capability, `HERDR_FOCUS_*` env, and
//! `UiEvent::FocusChanged`) from the currently focused pane, and routes mouse
//! events onto the native usage strip. Nothing here knows what a specific
//! dashboard is beyond the usage strip's mouse seam (removed when usage-dash
//! becomes a pure plugin).

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use usage_dash_core::RefreshOutcome;

use crate::agent_resume::AgentSessionRefKind;
use crate::app::{App, AppState};
use crate::detect::{parse_agent_label, Agent};
use dash_common::{AgentKind, FocusInput};

/// Map herdr's detected agent to the bridge-facing [`AgentKind`]. Agents the
/// bridge doesn't model collapse to [`AgentKind::None`] (treated as a plain
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
/// any link in that chain is absent, so a plugin simply holds its last snapshot
/// rather than thrash.
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
    // herdr's own accessor that unifies both, so the bridge reads it instead of
    // a single field (reading only `hook_authority` left the strip blank for
    // every real agent). Fall back to the effective detected agent so a freshly
    // detected pane with no session ref yet still surfaces its agent.
    let identity = terminal.current_session_identity_for_persistence();
    let agent = identity
        .as_ref()
        .and_then(|(_, label, _, _)| parse_agent_label(label))
        .or_else(|| terminal.effective_known_agent())
        .map(map_agent)
        .unwrap_or(AgentKind::None);
    let (session_id, session_ref) = match identity {
        Some((_, _, AgentSessionRefKind::Id, value)) => (Some(value.clone()), Some(value)),
        Some((_, _, AgentSessionRefKind::Path, value)) => {
            let sid = std::path::Path::new(&value)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());
            (sid, Some(value))
        }
        None => (None, None),
    };

    FocusInput {
        session_id,
        session_ref,
        cwd: Some(terminal.cwd.clone()),
        agent,
        // Set true only when a plugin surface itself holds focus; wired when
        // input routing lands. Until then a pane is always focused.
        is_self_focus: false,
    }
}

/// Refresh the usage snapshot from the focused pane. Called on the dash tick
/// (~1 Hz) from the main loop, applying the sticky semantics
/// ([`RefreshOutcome`]) — replace, keep, or clear the stored snapshot.
pub fn tick(app: &mut App) {
    let focus = focus_input(&app.state);
    match usage_dash_core::refresh(focus) {
        RefreshOutcome::Replace(snap) => app.usage_snapshot = Some(*snap),
        RefreshOutcome::Clear => app.usage_snapshot = None,
        RefreshOutcome::KeepPrevious => {}
    }
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

/// Route a mouse event onto the native usage strip when it lands there. Returns
/// true if consumed. Events outside the strip are left for the host.
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

    false
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
    /// reading only `hook_authority` left the strip blank for every real agent.
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
                source: "herdr:codex".to_string(),
                agent: "codex".to_string(),
                session_ref: crate::agent_resume::AgentSessionRef::id(sid)
                    .expect("valid session id"),
            });

        let fi = focus_input(&state);
        assert_eq!(fi.agent, AgentKind::Codex);
        assert_eq!(fi.session_id.as_deref(), Some(sid));
        assert_eq!(fi.session_ref.as_deref(), Some(sid));
    }

    #[test]
    fn focus_input_derives_pi_session_id_from_path_ref() {
        let mut state = AppState::test_with_adversarial_identity_state();
        let ws = state
            .active
            .and_then(|i| state.workspaces.get(i))
            .expect("active workspace");
        let pane_id = ws.focused_pane_id().expect("focused pane");
        let term_id = ws.terminal_id(pane_id).expect("terminal id").clone();

        let path = "/Users/andual/.pi/agent/sessions/--Users-andual--/2026-06-19T17-00-37-280Z_019ee0d3-cc20-7b86-972d-4882847039df.jsonl";
        state
            .terminals
            .get_mut(&term_id)
            .expect("terminal state")
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "herdr:pi".to_string(),
                agent: "pi".to_string(),
                session_ref: crate::agent_resume::AgentSessionRef::path(path)
                    .expect("valid session path"),
            });

        let fi = focus_input(&state);
        assert_eq!(fi.agent, AgentKind::Pi);
        assert_eq!(fi.session_ref.as_deref(), Some(path));
        assert_eq!(
            fi.session_id.as_deref(),
            Some("2026-06-19T17-00-37-280Z_019ee0d3-cc20-7b86-972d-4882847039df")
        );
    }
}
