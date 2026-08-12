//! Goal-panel host glue (the `feat/native-dashes` fork) — goal-SPECIFIC by
//! design, not a generic seam.
//!
//! Drives the host-owned goal core that backs the WASM goal-dash plugin: the
//! ~1 Hz refresh tick (which re-reads the focused session's goal/plan files and
//! runs the goal observer, then republishes the render input via
//! [`App::write_goal_input_file`]). The generic focused-session seam every
//! plugin gets lives in [`crate::plugin_focus`]; this module is the deliberate
//! residual impurity that keeps the goal panel populated until the wave-2
//! rewrite lands. It mirrors [`crate::usage_host`], the canonical wave-1 pattern.
//!
//! v1 EFFECT POLICY: the goal core still queues [`Effect`]s (auto-infer on a
//! fresh session, the goal observer, sort, clear+rebrief, copy) but this host
//! executes NONE of them — every effect is dropped with a `tracing::debug` log.
//! Interactive goal actions are a later wave: the read-only panel ships first,
//! and `HostEffect::SpawnHeadless` was deliberately deleted from the plugin API,
//! so there is no host path to run a headless goal action yet. The pre-purge
//! host (`src/dash.rs` before f4fd766) ran these effects directly; reviving that
//! is the interactive follow-up, not this wave.
//!
//! FOLLOW-UP (wave-2 exit — the single canonical plan for deleting all of this):
//! two changes ship together, mirroring the usage-dash exit —
//!   1. guest-owned interaction: the goal-dash guest handles its own
//!      actions via v3 `Action` (and a restored host effect path) instead of the
//!      host projecting `build_render_model` into a file each tick; and
//!   2. a plugin-side collector: a goal-dash helper process subscribes to the
//!      socket API (`events.subscribe`) and writes `goal_input.json` itself,
//!      instead of the host tick writing it.
//!
//! When both land, DELETE: this entire file (`src/goal_host.rs`) AND, in
//! `src/app/mod.rs`, the writer `write_goal_input_file` + `goal_render_input_json`,
//! the `goal_dash` field, and the `GOAL_DASH_*` consts. At that point the core
//! delta is bridge infrastructure only, with zero goal-specific host glue.

use goal_dash_core::effect::Effect;

use crate::app::App;

/// Refresh the goal core from the focused pane and republish its render input.
/// Called on the dash tick (~1 Hz) from the main loop next to
/// [`crate::usage_host::tick`]:
/// - `refresh` adopts the focused session (load on change / clear when none);
/// - `poll` re-reads the goal + plan files, runs the periodic observer, and
///   finalizes a pending sort, draining any queued effects;
/// - the drained effects are dropped (v1 policy — see the module header);
/// - the fresh render model is written to the goal-dash plugin's input file.
pub fn tick(app: &mut App) {
    let focus = crate::plugin_focus::focus_input(&app.state);
    app.goal_dash.refresh(focus);
    let effects = app.goal_dash.poll();
    drop_effects(effects);

    // Interim data path: publish the fresh render input to the goal-dash
    // plugin's state dir, where the generic `ui_input_file` reader surfaces it
    // as HERDR_PLUGIN_UI_INPUT_JSON. The writer stays host-side on the tick for
    // wave 1 (there is no periodic plugin hook to drive it); the wave-2 purity
    // path is a plugin-owned collector subscribing to the socket API.
    app.write_goal_input_file();
}

/// Drop the goal core's queued effects (v1 policy — this host executes none).
/// Each dropped interactive effect is logged at debug so the missing action is
/// visible when tracing, without stalling or mutating the host.
fn drop_effects(effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            // State already mutated in-core; the tick republishes regardless.
            Effect::Refresh => {}
            Effect::Status(msg) => tracing::debug!(target: "goal", "{msg}"),
            Effect::SpawnHeadless { mode, sid, .. } => {
                tracing::debug!(target: "goal", ?mode, %sid, "dropping goal SpawnHeadless (v1: no interactive goal actions)")
            }
            Effect::SendPaneKeys { lines } => {
                tracing::debug!(target: "goal", lines = lines.len(), "dropping goal SendPaneKeys (v1: no interactive goal actions)")
            }
            Effect::CopyText(_) => {
                tracing::debug!(target: "goal", "dropping goal CopyText (v1: no interactive goal actions)")
            }
        }
    }
}
