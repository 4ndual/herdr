//! Native dashboard integration glue (the `feat/native-dashes` fork).
//!
//! Bridges herdr's [`AppState`] to the embeddable dashboard cores
//! (`goal-dash-core`, `usage-dash-core`) without leaking herdr types into them:
//! the cores only ever see a [`dash_common::FocusInput`]. This module builds
//! that input from the currently focused pane each frame, and (later phases)
//! routes the cores' [`dash_common::Effect`]s back onto herdr machinery.

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use dash_common::{AgentKind, Effect, FocusInput, HeadlessMode};
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
    // Interim item-C data path: publish the fresh render input to the usage-dash
    // plugin's state dir, where item B's generic ui_input_file reader surfaces it
    // as HERDR_PLUGIN_UI_INPUT_JSON. This host-side writer (and this call site)
    // relocate into the plugin's own collector process in items D/E.
    app.write_usage_input_file();

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
            Effect::CopyText(text) => {
                app.state.request_clipboard_write = Some(text.into_bytes());
            }
            Effect::SpawnHeadless {
                agent,
                mode,
                sid,
                session_ref,
                cwd,
            } => spawn_headless(agent, mode, sid, session_ref, cwd),
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

/// Run the matching headless goal action on the focused backend on a detached
/// OS thread. Fire-and-forget: the next [`tick`] reloads the written file via
/// `poll`, so the UI never blocks on the call.
fn spawn_headless(
    agent: AgentKind,
    mode: HeadlessMode,
    sid: String,
    session_ref: Option<String>,
    cwd: Option<std::path::PathBuf>,
) {
    let _ = std::thread::Builder::new()
        .name("dash-headless".into())
        .spawn(move || match mode {
            HeadlessMode::Infer => {
                goal_dash_core::infer::run_infer_for(agent, session_ref.as_deref(), cwd.as_deref(), Some(sid));
            }
            HeadlessMode::InferTree => {
                goal_dash_core::infer::run_infer_tree_for(agent, session_ref.as_deref(), cwd.as_deref(), &sid);
            }
            HeadlessMode::Observe => {
                goal_dash_core::infer::run_observe_for(agent, session_ref.as_deref(), cwd.as_deref(), &sid);
            }
            HeadlessMode::Sort => {
                goal_dash_core::infer::run_sort_for(agent, session_ref.as_deref(), cwd.as_deref(), &sid);
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
    _usage_area: Rect,
    goal_area: Rect,
    goal: &mut goal_dash_core::App,
    _usage_view: &mut usage_dash_core::UsageView,
    _usage_snapshot: Option<&usage_dash_core::UsageSnapshot>,
    palette: &crate::app::state::Palette,
) {
    // feat/native-dashes: the usage strip is now rendered exclusively by the
    // built-in WASM plugin (`builtin.usage-dash`) via `render_ui_plugin_layers`.
    // There is no native fallback — a missing `.wasm` leaves the strip empty.
    if goal_area.width > 0 && goal_area.height > 0 {
        goal_dash_core::draw(frame, goal_area, goal);
        recolor_to_palette(frame, goal_area, palette);
    }
}

/// Re-map a dash region's named-ANSI foreground/background colors onto herdr's
/// resolved [`Palette`](crate::app::state::Palette) so the baked-in dashes are
/// themed by the host (Catppuccin by default) and stay visually consistent with
/// herdr's own RGB widgets — instead of the terminal's 16-color ANSI palette.
///
/// The cores deliberately render in semantic *named* colors (`Color::Green` =
/// "good", `Color::Yellow` = "attention", `Color::DarkGray` = "dim", …) and the
/// standalone binaries keep that terminal-adaptive look. Inside herdr we lift
/// each named color to the matching palette role. Anything already truecolor
/// (the selection bar's explicit `Rgb`, indexed/reset cells) is left untouched.
fn recolor_to_palette(frame: &mut Frame, area: Rect, palette: &crate::app::state::Palette) {
    let buf = frame.buffer_mut();
    let x1 = area.x.saturating_add(area.width);
    let y1 = area.y.saturating_add(area.height);
    for y in area.y..y1 {
        for x in area.x..x1 {
            let cell = &mut buf[(x, y)];
            if let Some(fg) = map_named_color(cell.fg, palette) {
                cell.fg = fg;
            }
            if let Some(bg) = map_named_color(cell.bg, palette) {
                cell.bg = bg;
            }
        }
    }
}

/// Lift a single semantic *named* ANSI color to its matching palette role.
/// Returns `None` for colors that should pass through untouched (already
/// truecolor `Rgb`, indexed, `Reset`, or `Black`) so we never flatten a
/// deliberately-chosen RGB or the terminal's own background.
fn map_named_color(
    c: ratatui::style::Color,
    palette: &crate::app::state::Palette,
) -> Option<ratatui::style::Color> {
    use ratatui::style::Color;
    Some(match c {
        Color::Green | Color::LightGreen => palette.green,
        Color::Yellow | Color::LightYellow => palette.yellow,
        Color::Red | Color::LightRed => palette.red,
        Color::Blue | Color::LightBlue => palette.blue,
        Color::Cyan | Color::LightCyan => palette.teal,
        Color::Magenta | Color::LightMagenta => palette.mauve,
        Color::Gray => palette.subtext0,
        Color::DarkGray => palette.overlay0,
        Color::White => palette.text,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::AppState;
    use crate::config::Config;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[test]
    fn agent_label_maps_known_agents_and_collapses_rest() {
        assert_eq!(map_agent(Agent::Claude), AgentKind::Claude);
        assert_eq!(map_agent(Agent::Codex), AgentKind::Codex);
        assert_eq!(map_agent(Agent::Pi), AgentKind::Pi);
        assert_eq!(map_agent(Agent::Gemini), AgentKind::None);
        assert_eq!(map_agent(Agent::Cursor), AgentKind::None);
    }

    #[test]
    fn named_colors_lift_to_catppuccin_palette_roles() {
        use crate::app::state::Palette;
        use ratatui::style::Color;
        let p = Palette::catppuccin();
        // semantic named colors map to the matching palette role …
        assert_eq!(map_named_color(Color::Green, &p), Some(p.green));
        assert_eq!(map_named_color(Color::Yellow, &p), Some(p.yellow));
        assert_eq!(map_named_color(Color::Red, &p), Some(p.red));
        assert_eq!(map_named_color(Color::Magenta, &p), Some(p.mauve));
        assert_eq!(map_named_color(Color::Cyan, &p), Some(p.teal));
        assert_eq!(map_named_color(Color::DarkGray, &p), Some(p.overlay0));
        assert_eq!(map_named_color(Color::White, &p), Some(p.text));
        // catppuccin green is a specific truecolor, not the terminal's ANSI green
        assert_eq!(
            map_named_color(Color::Green, &p),
            Some(Color::Rgb(166, 227, 161))
        );
        // … while explicit truecolor / reset pass through untouched.
        assert_eq!(map_named_color(Color::Rgb(246, 165, 160), &p), None);
        assert_eq!(map_named_color(Color::Reset, &p), None);
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

    #[test]
    fn route_mouse_hover_over_goal_panel_is_consumed_without_stealing_focus() {
        let mut app = test_app();
        app.state.goal_panel_visible = true;
        app.state.goal_panel_focused = false;
        app.state.view.goal_dash_area = Rect::new(10, 4, 20, 8);

        let consumed = route_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 12,
                row: 6,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );

        assert!(consumed);
        assert!(!app.state.goal_panel_focused);
    }

    #[test]
    fn route_mouse_click_focuses_goal_panel() {
        let mut app = test_app();
        app.state.goal_panel_visible = true;
        app.state.goal_panel_focused = false;
        app.state.view.goal_dash_area = Rect::new(10, 4, 20, 8);

        let consumed = route_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 6,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        );

        assert!(consumed);
        assert!(app.state.goal_panel_focused);
    }

    #[test]
    fn copy_effect_routes_to_host_clipboard_buffer() {
        let mut app = test_app();
        run_effects(
            &mut app,
            vec![Effect::CopyText("Ship the native goal pane".into())],
        );
        assert_eq!(
            app.state.request_clipboard_write,
            Some(b"Ship the native goal pane".to_vec())
        );
    }
}
