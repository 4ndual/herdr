//! Focused-session bridge seam (the `feat/native-dashes` fork).
//!
//! Builds the [`herdr_ui_plugin_api::FocusInput`] the WASM UI bridge delivers to
//! every plugin (the `FocusedSession` capability, `HERDR_FOCUS_*` env, and
//! `UiEvent::FocusChanged`) from the currently focused pane. This is generic
//! bridge infrastructure — it knows nothing about any specific dashboard.

use crate::agent_resume::AgentSessionRefKind;
use crate::app::AppState;
use crate::detect::{parse_agent_label, Agent};
use herdr_ui_plugin_api::{AgentKind, FocusInput};
use std::sync::OnceLock;

/// fork(wave-1): extract the name of the subagent a Claude pane is currently
/// VIEWING from its bottom-screen chrome, `@` stripped. Claude Code's subagent
/// view shows a `❯ Message @<name>…` prompt (the ellipsis is U+2026 or `...`)
/// and a `Viewing @<name> · esc to return` banner; either marker resolves the
/// name. The agent-tray list (`⏺ main` / `◯ probe1`) is deliberately NOT used —
/// it is present whether or not a subview is open. Pure over the screen text so
/// it is unit-testable without a PTY.
///
/// fork(wave-2): Claude Code v2.1.210 dropped the `Message @<name>…` prompt and
/// the `Viewing @<name>` banner from the subagent view; the only remaining
/// marker is a full-width box-drawing separator line embedding the name, e.g.
/// `───────────────────── @probe-demo ──`. Match that per-line separator FIRST,
/// keeping the two legacy prompts as fallbacks for older Claude Code builds.
pub fn parse_subagent_marker(screen: &str) -> Option<String> {
    // `─{3,} @<name> ─+` is the v2.1.210 subagent-view header separator. Anchored
    // per line (`(?m)`) so an in-transcript chat line mentioning `@foo` — which is
    // not a `─`-bounded separator — can never match.
    static SEPARATOR: OnceLock<regex::Regex> = OnceLock::new();
    // `Message @<name>…` is the stable bottom-chrome prompt while viewing a
    // subagent; `Viewing @<name>` is the top banner used as a secondary signal.
    static MESSAGE: OnceLock<regex::Regex> = OnceLock::new();
    static VIEWING: OnceLock<regex::Regex> = OnceLock::new();
    let separator = SEPARATOR
        .get_or_init(|| regex::Regex::new(r"(?m)^─{3,}\s*@([A-Za-z0-9_-]+)\s*─+\s*$").unwrap());
    let message = MESSAGE
        .get_or_init(|| regex::Regex::new(r"Message @([A-Za-z0-9_-]+)\s*(?:…|\.\.\.)").unwrap());
    let viewing = VIEWING.get_or_init(|| regex::Regex::new(r"Viewing @([A-Za-z0-9_-]+)").unwrap());
    separator
        .captures(screen)
        .or_else(|| message.captures(screen))
        .or_else(|| viewing.captures(screen))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

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
        // fork(wave-1): the subagent view is a live screen-chrome fact not held
        // in pure `AppState`, so `focus_input` leaves this `None`; the usage host
        // tick fills it from the focused pane's runtime detection text (which is
        // only reachable there). See `usage_host::tick`.
        subagent: None,
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
    fn subagent_marker_extracts_from_message_prompt_with_ellipsis() {
        // Real bottom chrome while viewing @probe1 (ellipsis is U+2026).
        let screen = "Viewing @probe1 · esc to return\n\
                      some transcript body\n\
                      ❯ Message @probe1…\n\
                      ─────────── @probe1 ──────────";
        assert_eq!(parse_subagent_marker(screen).as_deref(), Some("probe1"));
    }

    #[test]
    fn subagent_marker_accepts_ascii_ellipsis_and_hyphenated_names() {
        let screen = "❯ Message @my-sub_2...";
        assert_eq!(parse_subagent_marker(screen).as_deref(), Some("my-sub_2"));
    }

    #[test]
    fn subagent_marker_falls_back_to_viewing_banner() {
        // No Message prompt, only the Viewing banner present.
        let screen = "Viewing @helper · esc to return\nbody text";
        assert_eq!(parse_subagent_marker(screen).as_deref(), Some("helper"));
    }

    #[test]
    fn subagent_marker_extracts_from_v2_210_separator_header() {
        // fork(wave-2): verbatim bottom-buffer separator captured live from a
        // Claude Code v2.1.210 pane viewing @probe-demo (no Message/Viewing chrome).
        let screen = "some transcript body\n\
                      ────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────── @probe-demo ──\n";
        assert_eq!(parse_subagent_marker(screen).as_deref(), Some("probe-demo"));
    }

    #[test]
    fn subagent_marker_separator_ignores_chat_mention_and_manage_row() {
        // A chat line mentioning @foo is NOT a `─`-bounded separator.
        assert_eq!(
            parse_subagent_marker("please ask @foo to review this\n"),
            None
        );
        // The manage-subagents list row is not a separator either.
        assert_eq!(
            parse_subagent_marker("❯ ⏺ probe-demo  running · 12s\n"),
            None
        );
    }

    #[test]
    fn subagent_marker_absent_when_not_viewing() {
        // Bare prompt + agent tray (tray alone must NOT trigger a match).
        let screen = "⏺ main\n◯ probe1  idle\n❯ ";
        assert_eq!(parse_subagent_marker(screen), None);
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
