//! Usage-strip host glue (the `feat/native-dashes` fork) — usage-SPECIFIC by
//! design, not a generic seam.
//!
//! Drives the host-owned usage snapshot + view-toggle state that back the WASM
//! usage-dash plugin: the ~1 Hz refresh tick (which also republishes the render
//! input via [`App::write_usage_input_file`]) and the mouse routing onto the
//! strip. This is the wave-1 host-side data path for one specific dashboard.
//! The generic focused-session seam every plugin gets lives in
//! [`crate::plugin_focus`]; this module is the deliberate residual impurity that
//! keeps the strip interactive until the wave-2 rewrite lands.
//!
//! FOLLOW-UP (wave-2 exit — the single canonical plan for deleting all of this):
//! two changes ship together because the v3 `Action` rewrite is what makes both
//! coherent —
//!   1. guest-owned toggle state: the usage-dash guest handles its own
//!      collapse/stats/scroll via v3 `Action` instead of the host mirroring them
//!      into `App::usage_view`; and
//!   2. a plugin-side collector: a usage-dash helper process subscribes to the
//!      socket API (`events.subscribe`) and writes `usage_input.json` itself,
//!      instead of the host tick writing it.
//!
//! When both land, DELETE: this entire file (`src/usage_host.rs`) AND, in
//! `src/app/mod.rs`, the writer `write_usage_input_file` + `usage_render_input_json`,
//! the `sync_usage_view_hits_from_layers` toggle-mirror shim, the
//! `usage_view`/`usage_snapshot` fields, and the `USAGE_DASH_*` consts. At that
//! point the core delta is bridge infrastructure only, with zero usage-specific
//! host glue.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use usage_dash_core::{RefreshOutcome, UsageSnapshot};

use crate::api::schema::PluginManifestUiModule;
use crate::app::App;
use crate::plugin_collector::CollectorOutcome;

/// Refresh the usage snapshot from the focused pane. Called on the dash tick
/// (~1 Hz) from the main loop, applying the sticky semantics
/// ([`RefreshOutcome`]) — replace, keep, or clear the stored snapshot.
pub fn tick(app: &mut App) {
    let mut focus = crate::plugin_focus::focus_input(&app.state);
    // fork(wave-1): a Claude pane may be VIEWING one of its subagents. That is a
    // live screen-chrome fact (not in pure `AppState`), so we read the focused
    // pane's runtime detection text here — the same bottom-of-buffer snapshot the
    // agent detector consumes (`herdr agent read <pane> --source detection`) — and
    // parse the `Message @<name>…` / `Viewing @<name>` marker. Only meaningful for
    // Claude; other agents have no subagent view.
    if agent_supports_subagent_view(focus.agent) {
        focus.subagent = focused_subagent(app);
    }

    // fork(wave-2): if the usage-dash module declares a `collector`, the snapshot
    // is produced OUT OF PROCESS so collection logic hot-reloads (rebuild the
    // collector bin) without a host rebuild. Skip the in-host refresh and launch
    // the collector instead; its snapshot lands later via
    // AppEvent::CollectorFinished (sticky until then). When no module declares a
    // collector, the legacy in-host refresh path is unchanged (goal-dash, which
    // declares none, is unaffected).
    if let Some(module) = declared_collector_module(app) {
        crate::plugin_collector::launch_if_idle(
            app,
            crate::app::USAGE_DASH_PLUGIN_ID,
            &module,
            &focus,
        );
    } else {
        // Keyed by the pane this refresh was FOR. `KeepPrevious` now genuinely
        // means "previous for this pane" instead of "whatever was last stored".
        let key = focus.pane_id.clone();
        match (usage_dash_core::refresh(focus), key) {
            (RefreshOutcome::Replace(snap), Some(key)) => {
                app.usage_snapshots.insert(key, *snap);
            }
            (RefreshOutcome::Clear, Some(key)) => {
                app.usage_snapshots.remove(&key);
            }
            // Nothing focused: there is no pane to attribute a snapshot to, and
            // guessing is what this whole change removes.
            (_, None) | (RefreshOutcome::KeepPrevious, _) => {}
        }
    }
    app.evict_stale_usage_snapshots();

    // Interim data path: publish the fresh render input to the usage-dash
    // plugin's state dir, where item B's generic `ui_input_file` reader surfaces
    // it as HERDR_PLUGIN_UI_INPUT_JSON. The writer stays host-side on the tick
    // for wave 1 (a ~1 Hz cadence the countdown rendering needs; there is no
    // periodic plugin hook to drive it). The wave-2 purity path is a
    // plugin-owned collector subscribing to the socket API (events.subscribe).
    app.write_usage_input_file();
}

/// fork(wave-2): return an owned clone of the usage-dash UI module when it is
/// enabled and declares BOTH a `ui_input_file` and a `collector`. That pairing
/// is what routes this tick through the out-of-process collector; a module
/// missing either field keeps the legacy in-host refresh. Cloned so the caller
/// can take `&mut App` for the launch.
fn declared_collector_module(app: &App) -> Option<PluginManifestUiModule> {
    let plugin = app
        .state
        .installed_plugins
        .get(crate::app::USAGE_DASH_PLUGIN_ID)?;
    if !plugin.enabled {
        return None;
    }
    plugin
        .ui_modules
        .iter()
        .find(|module| module.ui_input_file.is_some() && module.collector.is_some())
        .cloned()
}

/// fork(wave-2): apply a collector's sticky outcome to the host usage snapshot.
/// A `replace` snapshot that fails to deserialize into [`UsageSnapshot`] is
/// treated as a failure (previous snapshot kept) so a malformed collector can
/// never blank or corrupt the strip. Returns the reason on failure so the caller
/// can warn once per distinct message.
/// `pane_key` is the pane the run was launched FOR, not the focused one: a
/// collector takes real time, and a result that lands after the user moved on
/// used to be stored as the NEW pane's numbers. `None` means the launch had no
/// focused pane, in which case there is nothing to attribute the result to.
pub fn apply_collector_outcome(
    app: &mut App,
    pane_key: Option<&str>,
    outcome: &CollectorOutcome,
) -> Result<(), String> {
    match outcome {
        CollectorOutcome::Replace(value) => {
            let snapshot = serde_json::from_value::<UsageSnapshot>(value.clone())
                .map_err(|err| format!("collector snapshot did not deserialize: {err}"))?;
            if let Some(key) = pane_key {
                app.usage_snapshots.insert(key.to_string(), snapshot);
            }
            Ok(())
        }
        CollectorOutcome::Keep => Ok(()),
        CollectorOutcome::Clear => {
            if let Some(key) = pane_key {
                app.usage_snapshots.remove(key);
            }
            Ok(())
        }
    }
}

/// Whether an agent has a subagent view worth reading off the screen at all.
///
/// Only Claude Code has one. Reading the bottom-buffer chrome of every other
/// runtime looking for a marker it cannot produce is pure cost on every tick,
/// and a documented predicate says WHY the check is narrow — the bare
/// `== AgentKind::Claude` it replaces read like an oversight. Deliberately not
/// a manifest field: the detection manifest structs are
/// `#[serde(deny_unknown_fields)]`, so a new key forces a `min_engine_version`
/// bump on every manifest in the wild.
fn agent_supports_subagent_view(agent: herdr_ui_plugin_api::AgentKind) -> bool {
    matches!(agent, herdr_ui_plugin_api::AgentKind::Claude)
}

/// fork(wave-1): resolve the subagent the focused pane is currently viewing by
/// reading its runtime detection text (bottom-of-buffer chrome) and parsing the
/// subagent-view marker. Returns `None` when nothing is focused, the runtime is
/// gone, or no marker is present. Read-only over the terminal runtime.
fn focused_subagent(app: &App) -> Option<String> {
    let ws = app.state.workspaces.get(app.state.active?)?;
    let pane_id = ws.focused_pane_id()?;
    let terminal_id = ws.terminal_id(pane_id)?;
    let runtime = app.terminal_runtimes.get(terminal_id)?;
    crate::plugin_focus::parse_subagent_marker(&runtime.detection_text())
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
pub fn route_mouse(app: &mut App, source_id: crate::app::InputSourceId, mouse: MouseEvent) -> bool {
    let (col, row) = (mouse.column, mouse.row);

    if in_rect(app.state.view.usage_strip_rect, col, row) {
        // A Drag/Up report that lands over the strip but belongs to a gesture
        // anchored elsewhere (a tab/workspace press, or any other chrome/pane
        // drag such as a split-border or scrollbar drag) must still reach its
        // owner. The strip only owns FRESH input over its own rows, not the
        // tail end of someone else's gesture; the strip sits at the bottom of
        // the screen, which is exactly where a drag or a stray coordinate is
        // likely to end up. Swallowing it here would silently drop the Up
        // that completes, say, a tab click whenever the pointer's path (real
        // or misreported) crosses the strip's rows.
        let gesture_in_flight = app.state.tab_presses.contains_key(&source_id)
            || app.state.workspace_presses.contains_key(&source_id)
            || app.state.drag.is_some();
        if gesture_in_flight
            && matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            )
        {
            return false;
        }

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
    use crate::api::schema::{
        InstalledPluginInfo, PluginHotReloadMode, PluginManifestUiModule, PluginUiCapability,
        PluginUiModuleKind, PluginUiSurface,
    };
    use crate::config::Config;
    use usage_dash_core::{Cli, UsageSnapshot};

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

    fn usage_module(
        ui_input_file: Option<&str>,
        collector: Option<Vec<String>>,
    ) -> PluginManifestUiModule {
        PluginManifestUiModule {
            rail_icon: None,
            rail_action: None,
            max_layers_per_pane: None,
            id: "usage".into(),
            title: "Usage".into(),
            description: None,
            platforms: None,
            kind: PluginUiModuleKind::Wasm,
            api_version: 3,
            module: "dist/usage.wasm".into(),
            hot_reload: PluginHotReloadMode::On,
            surfaces: vec![PluginUiSurface::UsageStrip],
            placement: None,
            capabilities: vec![PluginUiCapability::LayoutRead],
            ui_input_file: ui_input_file.map(str::to_string),
            image_file: None,
            graphics_roots: Vec::new(),
            collector,
            host_actions: Vec::new(),
            host_action_mode: None,
        }
    }

    fn install_usage_plugin(app: &mut App, enabled: bool, module: PluginManifestUiModule) {
        app.state.installed_plugins.insert(
            crate::app::USAGE_DASH_PLUGIN_ID.into(),
            InstalledPluginInfo {
                plugin_id: crate::app::USAGE_DASH_PLUGIN_ID.into(),
                name: "Usage Dashboard".into(),
                version: "0.1.0".into(),
                min_herdr_version: crate::build_info::BASE_VERSION.into(),
                description: None,
                manifest_path: "/tmp/usage-dash/herdr-plugin.toml".into(),
                plugin_root: "/tmp/usage-dash".into(),
                enabled,
                platforms: None,
                build: vec![],
                startup: Vec::new(),
                actions: vec![],
                events: vec![],
                panes: vec![],
                ui_modules: vec![module],
                link_handlers: vec![],
                source: Default::default(),
                warnings: vec![],
            },
        );
    }

    #[test]
    fn declared_collector_module_requires_both_fields_and_enabled() {
        let mut app = test_app();

        // Both fields present + enabled -> Some.
        install_usage_plugin(
            &mut app,
            true,
            usage_module(Some("usage_input.json"), Some(vec!["c".into()])),
        );
        assert!(declared_collector_module(&app).is_some());

        // Collector missing -> None (legacy in-host refresh path).
        install_usage_plugin(&mut app, true, usage_module(Some("usage_input.json"), None));
        assert!(declared_collector_module(&app).is_none());

        // ui_input_file missing -> None.
        install_usage_plugin(&mut app, true, usage_module(None, Some(vec!["c".into()])));
        assert!(declared_collector_module(&app).is_none());

        // Disabled plugin -> None even with both fields.
        install_usage_plugin(
            &mut app,
            false,
            usage_module(Some("usage_input.json"), Some(vec!["c".into()])),
        );
        assert!(declared_collector_module(&app).is_none());
    }

    /// The public pane id of whatever the test app has focused.
    /// Give the app a real workspace so focus resolves to a public pane id.
    fn seed_workspace(app: &mut App) {
        if app.state.workspaces.is_empty() {
            app.state.workspaces = vec![crate::workspace::Workspace::test_new("one")];
        }
        app.state.active = Some(0);
        app.state.selected = 0;
        // focus_input resolves pane -> terminal -> state.terminals; without the
        // terminal entries it returns the "nothing focused" default.
        app.state.ensure_test_terminals();
    }

    fn focused_pane_key(app: &App) -> String {
        crate::plugin_focus::focus_input(&app.state)
            .pane_id
            .expect("the test app has a focused pane")
    }

    /// Split the test app's focused tab and return both panes' public ids,
    /// (first, second), with focus left on the first.
    fn two_panes(app: &mut App) -> (String, String) {
        seed_workspace(app);
        let first = focused_pane_key(app);
        let ws = &mut app.state.workspaces[0];
        let new_pane = {
            let tab = ws.active_tab_mut().expect("an active tab");
            let new_pane = tab
                .layout
                .split_focused(ratatui::layout::Direction::Vertical);
            tab.panes.insert(
                new_pane,
                crate::pane::PaneState::new(crate::terminal::TerminalId::alloc()),
            );
            new_pane
        };
        let number = ws.next_public_pane_number;
        ws.next_public_pane_number += 1;
        ws.public_pane_numbers.insert(new_pane, number);
        let second = crate::workspace::public_pane_id_for_number(&ws.id, number);
        let root = app.state.workspaces[0]
            .active_tab()
            .expect("an active tab")
            .layout
            .pane_ids()[0];
        app.state.workspaces[0]
            .active_tab_mut()
            .expect("an active tab")
            .layout
            .focus_pane(root);
        (first, second)
    }

    /// The strip used to show the WRONG pane's numbers after a focus switch:
    /// one snapshot for the whole app meant "the last thing collected", not
    /// "this pane's usage".
    #[test]
    fn snapshot_is_keyed_per_pane() {
        let mut app = test_app();
        let (first, second) = two_panes(&mut app);
        app.usage_snapshots
            .insert(first.clone(), UsageSnapshot::empty(Cli::Claude));

        assert!(
            app.focused_usage_snapshot().is_some(),
            "the pane with a snapshot shows it"
        );

        focus_pane_by_public_id(&mut app, &second);
        assert!(
            app.focused_usage_snapshot().is_none(),
            "a pane with no snapshot of its own renders null, not its neighbour's"
        );
    }

    /// The actual race: a collector launched for pane A takes real time, and its
    /// result used to be stored as whatever was focused when it landed.
    #[test]
    fn late_collector_result_lands_on_its_requesting_pane_not_the_focused_one() {
        let mut app = test_app();
        let (first, second) = two_panes(&mut app);
        let value = serde_json::to_value(UsageSnapshot::empty(Cli::Codex)).unwrap();

        // The run was launched for `first`; focus moved to `second` meanwhile.
        focus_pane_by_public_id(&mut app, &second);
        apply_collector_outcome(&mut app, Some(&first), &CollectorOutcome::Replace(value)).unwrap();

        assert!(
            app.usage_snapshots.contains_key(&first),
            "the result belongs to the pane it was requested for"
        );
        assert!(
            app.focused_usage_snapshot().is_none(),
            "and must not be attributed to the pane that happens to be focused"
        );
    }

    fn focus_pane_by_public_id(app: &mut App, public_id: &str) {
        let ws = &mut app.state.workspaces[0];
        let target = ws
            .active_tab()
            .expect("an active tab")
            .layout
            .pane_ids()
            .into_iter()
            .find(|pane_id| {
                ws.public_pane_number(*pane_id)
                    .map(|n| crate::workspace::public_pane_id_for_number(&ws.id, n))
                    .as_deref()
                    == Some(public_id)
            })
            .expect("a pane with that public id");
        ws.active_tab_mut()
            .expect("an active tab")
            .layout
            .focus_pane(target);
    }

    /// A closed pane must not leave its snapshot behind: the map is keyed by a
    /// string the user can create at will.
    #[test]
    fn stale_pane_snapshots_are_evicted() {
        let mut app = test_app();
        seed_workspace(&mut app);
        let live = focused_pane_key(&app);
        app.usage_snapshots
            .insert(live.clone(), UsageSnapshot::empty(Cli::Claude));
        app.usage_snapshots
            .insert("w9:p9".to_string(), UsageSnapshot::empty(Cli::Claude));

        app.evict_stale_usage_snapshots();

        assert!(app.usage_snapshots.contains_key(&live));
        assert!(!app.usage_snapshots.contains_key("w9:p9"));
    }

    #[test]
    fn tick_with_collector_skips_builtin_refresh() {
        let mut app = test_app();
        install_usage_plugin(
            &mut app,
            true,
            usage_module(
                Some("usage_input.json"),
                Some(vec!["/nonexistent/collector".into()]),
            ),
        );
        // Sentinel snapshot the built-in refresh (agent=None -> Clear) would wipe,
        // keyed to the pane the test app actually has focused so eviction keeps it.
        seed_workspace(&mut app);
        let pane = focused_pane_key(&app);
        app.usage_snapshots
            .insert(pane.clone(), UsageSnapshot::empty(Cli::Claude));
        tick(&mut app);
        // Collector declared -> refresh skipped -> sentinel survives (the async
        // collector's real result arrives later via AppEvent, not on this tick).
        assert!(app.usage_snapshots.contains_key(&pane));
    }

    #[test]
    fn tick_without_collector_runs_builtin_refresh() {
        let mut app = test_app();
        install_usage_plugin(&mut app, true, usage_module(Some("usage_input.json"), None));
        seed_workspace(&mut app);
        let pane = focused_pane_key(&app);
        app.usage_snapshots
            .insert(pane.clone(), UsageSnapshot::empty(Cli::Claude));
        tick(&mut app);
        // No collector -> legacy refresh runs; default focus has no agent -> Clear.
        assert!(!app.usage_snapshots.contains_key(&pane));
    }

    #[test]
    fn apply_collector_outcome_transitions() {
        let mut app = test_app();

        // Replace with a valid serialized snapshot.
        let snapshot = UsageSnapshot::empty(Cli::Codex);
        let value = serde_json::to_value(&snapshot).unwrap();
        let pane = "w1:p1";
        apply_collector_outcome(&mut app, Some(pane), &CollectorOutcome::Replace(value)).unwrap();
        assert!(matches!(
            app.usage_snapshots.get(pane).map(|s| s.cli),
            Some(Cli::Codex)
        ));

        // Keep leaves it untouched.
        apply_collector_outcome(&mut app, Some(pane), &CollectorOutcome::Keep).unwrap();
        assert!(app.usage_snapshots.contains_key(pane));

        // Clear removes it.
        apply_collector_outcome(&mut app, Some(pane), &CollectorOutcome::Clear).unwrap();
        assert!(!app.usage_snapshots.contains_key(pane));

        // A malformed replace value is a failure and keeps the previous snapshot.
        app.usage_snapshots
            .insert(pane.to_string(), UsageSnapshot::empty(Cli::Claude));
        let err = apply_collector_outcome(
            &mut app,
            Some(pane),
            &CollectorOutcome::Replace(serde_json::json!({"not":"a snapshot"})),
        )
        .unwrap_err();
        assert!(err.contains("did not deserialize"), "{err}");
        assert!(app.usage_snapshots.contains_key(pane));
    }
}
