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
//! When both land, DELETE: this entire file (`src/usage_host.rs`) AND, in
//! `src/app/mod.rs`, the writer `write_usage_input_file` + `usage_render_input_json`,
//! the `sync_usage_view_hits_from_layers` toggle-mirror shim, the
//! `usage_view`/`usage_snapshot` fields, and the `USAGE_DASH_*` consts. At that
//! point the core delta is bridge infrastructure only, with zero usage-specific
//! host glue.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use usage_dash_core::RefreshOutcome;

use crate::app::App;

/// Refresh the usage snapshot from the focused pane. Called on the dash tick
/// (~1 Hz) from the main loop, applying the sticky semantics
/// ([`RefreshOutcome`]) — replace, keep, or clear the stored snapshot.
pub fn tick(app: &mut App) {
    let focus = crate::plugin_focus::focus_input(&app.state);
    match usage_dash_core::refresh(focus) {
        RefreshOutcome::Replace(snap) => app.usage_snapshot = Some(*snap),
        RefreshOutcome::Clear => app.usage_snapshot = None,
        RefreshOutcome::KeepPrevious => {}
    }

    // Interim data path: publish the fresh render input to the usage-dash
    // plugin's state dir, where item B's generic `ui_input_file` reader surfaces
    // it as HERDR_PLUGIN_UI_INPUT_JSON. The writer stays host-side on the tick
    // for wave 1 (a ~1 Hz cadence the countdown rendering needs; there is no
    // periodic plugin hook to drive it). The wave-2 purity path is a
    // plugin-owned collector subscribing to the socket API (events.subscribe).
    app.write_usage_input_file();
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

    if in_rect(app.state.view.usage_strip_rect, col, row) {
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
