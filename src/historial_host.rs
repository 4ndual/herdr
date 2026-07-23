//! Historial-panel host glue (the pane-close-history fork) — mirrors
//! [`crate::goal_host`] and [`crate::usage_host`], the wave-1 host-collector
//! pattern.
//!
//! The historial WASM plugin (`plugins/historial-dash-ui`, id
//! `builtin.historial-dash`) is a pure renderer: it draws whatever
//! [`historial_dash_model::HistorialRenderModel`] the host publishes into its
//! `historial_input.json`. This tick reads the recent slice of the durable
//! pane-close log ([`crate::pane_close_history::read_recent`]), precomputes every
//! display string (agent, label, relative time, restorability) host-side — the
//! guest can reach neither a clock nor the disk — and republishes the model each
//! ~1 Hz dash tick via [`App::write_historial_input_file`].
//!
//! Unlike goal-dash there is no host-owned core to poll and no effect queue: the
//! model is projected straight from the log, so this module is a single thin
//! tick with the serialization living next to the other input writers in
//! `app/mod.rs`.

use crate::app::App;

/// Republish the historial render input from the recent pane-close log. Called on
/// the dash tick (~1 Hz) next to [`crate::goal_host::tick`].
pub fn tick(app: &mut App) {
    app.write_historial_input_file();
}
