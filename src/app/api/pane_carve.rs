//! fork(+131): handlers for pane carves (`pane.carve.*`).
//!
//! ## How this differs from every other file in this directory
//!
//! `pane_graphics.rs` and `pane_text.rs` manage COMPOSITING state: retained
//! layers that reach the screen once per frame and that `PaneState`, the
//! parser, and scrollback never see. Their defining invariant is that a plugin
//! cannot change what `pane.read`, copy, search, agent detection or a resumed
//! agent observes.
//!
//! Carve does not have that invariant, and cannot. Reserving part of a pane
//! means the terminal must stop using those cells, and the only way to tell a
//! terminal that is to resize its PTY. The program reflows and its scrollback
//! is rewritten. That is ordinary resize behaviour — the user causes it every
//! time they split, zoom, or resize the window, and shells and agents handle it
//! as a normal fact of terminal life. What +127 deferred was not reflow itself
//! but doing it ACCIDENTALLY, as a side effect of painting. Doing it on purpose,
//! with an explicit edge, a validated floor, and host-side cleanup, is the
//! designed version of the same act.
//!
//! ## The three things that keep it safe
//!
//! 1. **A carve is an edge and a thickness, never a free box.** A PTY is a
//!    rectangle; there is no hole to carve in the middle. The type refuses to
//!    express what the host cannot honour.
//! 2. **A floor.** A carve that eats the pane is the zero-extent footgun (#40)
//!    in a new costume: a call that succeeds and leaves the user with a pane
//!    they can neither read nor type into.
//! 3. **Cleanup on owner loss.** A plugin gets no unload event, so a crashed or
//!    disabled plugin would otherwise leave a pane permanently shrunk with
//!    nothing on screen to blame. That lives in `clear_pane_carves_for_owner`
//!    and is called from the same disable/unlink paths as the layer maps.

use crate::api::schema::{
    PaneCarveClearParams, PaneCarveEdge, PaneCarveSetParams, ResponseResult,
    PANE_CARVE_MIN_TEXT_COLS, PANE_CARVE_MIN_TEXT_ROWS,
};
use crate::app::state::PaneCarve;
use crate::app::App;

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_pane_carve_set(
        &mut self,
        id: String,
        params: PaneCarveSetParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        if params.cells == 0 {
            // Rejected rather than read as "no carve": a zero band is a caller
            // whose arithmetic collapsed, and #40 is the standing lesson that a
            // zero extent must never be silently reinterpreted as something
            // else. Releasing is `pane.carve.clear`, which says so.
            return encode_error(
                id,
                "invalid_carve",
                "carve cells must be greater than zero; use pane.carve.clear to release",
            );
        }

        // Measured against the pane's CURRENT inner rect plus whatever other
        // owners already hold, so two plugins cannot each pass the floor check
        // alone and starve the pane together.
        let Some(inner) = self
            .state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .map(|info| info.inner_rect)
            .filter(|rect| rect.width > 0 && rect.height > 0)
        else {
            return encode_error(
                id,
                "pane_not_laid_out",
                "pane has no rect this frame; a carve cannot be measured against it",
            );
        };

        let owner = carve_owner(&params.owner);
        let (mut top, mut bottom, mut left, mut right) = self
            .state
            .pane_carve_totals_excluding(pane_id, owner.as_deref());
        match params.edge {
            PaneCarveEdge::Top => top = top.saturating_add(params.cells),
            PaneCarveEdge::Bottom => bottom = bottom.saturating_add(params.cells),
            PaneCarveEdge::Left => left = left.saturating_add(params.cells),
            PaneCarveEdge::Right => right = right.saturating_add(params.cells),
        }
        let remaining_cols = inner.width.saturating_sub(left.saturating_add(right));
        let remaining_rows = inner.height.saturating_sub(top.saturating_add(bottom));
        if remaining_cols < PANE_CARVE_MIN_TEXT_COLS || remaining_rows < PANE_CARVE_MIN_TEXT_ROWS {
            return encode_error(
                id,
                "carve_too_large",
                format!(
                    "carve would leave {remaining_cols}x{remaining_rows} cells of text; \
                     the minimum is {PANE_CARVE_MIN_TEXT_COLS}x{PANE_CARVE_MIN_TEXT_ROWS}"
                ),
            );
        }

        self.state.upsert_pane_carve(
            pane_id,
            PaneCarve {
                edge: params.edge,
                cells: params.cells,
                owner,
            },
        );
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_carve_clear(
        &mut self,
        id: String,
        params: PaneCarveClearParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        self.state
            .clear_pane_carves(pane_id, carve_owner(&params.owner).as_deref());
        encode_success(id, ResponseResult::Ok {})
    }
}

/// Empty owner means the call arrived over the socket, which is the privileged
/// caller. Same convention as `pane_graphics::layer_owner`.
fn carve_owner(owner: &str) -> Option<String> {
    let owner = owner.trim();
    if owner.is_empty() {
        None
    } else {
        Some(owner.to_string())
    }
}

fn pane_not_found(id: String, pane_id: &str) -> String {
    encode_error(id, "pane_not_found", format!("pane {pane_id} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ErrorResponse, Request};
    use crate::config::Config;
    use crate::workspace::Workspace;

    fn app_with_pane() -> (App, String, crate::layout::PaneId) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("carve")];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let public = app.public_pane_id(0, pane_id).expect("public id");
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 100, 50),
            inner_rect: ratatui::layout::Rect::new(0, 0, 100, 50),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        }];
        (app, public, pane_id)
    }

    fn set(public: &str, edge: &str, cells: u16) -> Request {
        serde_json::from_value(serde_json::json!({
            "id": "carve",
            "method": "pane.carve.set",
            "params": {"pane_id": public, "edge": edge, "cells": cells}
        }))
        .expect("request")
    }

    #[test]
    fn a_carve_reserves_cells_on_the_named_edge() {
        let (mut app, public, pane_id) = app_with_pane();
        app.handle_api_request(set(&public, "bottom", 8));
        assert_eq!(app.state.pane_carve_totals(pane_id), (0, 8, 0, 0));
    }

    /// A repeat from one owner REPLACES rather than accumulating: asking twice
    /// means "this is my band now", and accumulating would make a plugin that
    /// re-asserts its carve each frame shrink the pane to nothing.
    #[test]
    fn a_repeat_from_the_same_owner_replaces_rather_than_stacks() {
        let (mut app, public, pane_id) = app_with_pane();
        app.handle_api_request(set(&public, "bottom", 8));
        app.handle_api_request(set(&public, "bottom", 5));
        assert_eq!(app.state.pane_carve_totals(pane_id), (0, 5, 0, 0));
    }

    /// The floor is the #40 lesson restated: a call that succeeds and leaves a
    /// pane the user cannot read or type into is not a success.
    #[test]
    fn a_carve_that_would_eat_the_pane_is_refused() {
        let (mut app, public, pane_id) = app_with_pane();
        let response = app.handle_api_request(set(&public, "bottom", 49));
        let error: ErrorResponse = serde_json::from_str(&response).expect("error");
        assert_eq!(error.error.code, "carve_too_large");
        assert_eq!(app.state.pane_carve_totals(pane_id), (0, 0, 0, 0));
    }

    /// Zero is refused rather than read as "release". #40's whole finding was
    /// that a zero extent silently reinterpreted is how a typo becomes a
    /// user-visible defect.
    #[test]
    fn a_zero_carve_is_refused_rather_than_treated_as_a_release() {
        let (mut app, public, _) = app_with_pane();
        let response = app.handle_api_request(set(&public, "top", 0));
        let error: ErrorResponse = serde_json::from_str(&response).expect("error");
        assert_eq!(error.error.code, "invalid_carve");
    }

    /// Two owners cannot each pass the floor alone and starve the pane
    /// together, which is why the check sums the other owners' bands.
    #[test]
    fn the_floor_counts_every_owners_band_not_just_the_callers() {
        let (mut app, public, pane_id) = app_with_pane();
        app.state.upsert_pane_carve(
            pane_id,
            PaneCarve {
                edge: PaneCarveEdge::Top,
                cells: 40,
                owner: Some("plugin:other".to_string()),
            },
        );
        let response = app.handle_api_request(set(&public, "bottom", 10));
        let error: ErrorResponse = serde_json::from_str(&response).expect("error");
        assert_eq!(error.error.code, "carve_too_large");
    }

    /// The property that makes carve safe to hand to a plugin: when the owner
    /// goes away the pane re-expands on its own. Without this a crashed plugin
    /// leaves a permanently shrunken pane and nothing on screen to blame.
    #[test]
    fn a_dead_owners_carve_is_released_so_the_pane_re_expands() {
        let (mut app, _public, pane_id) = app_with_pane();
        app.state.upsert_pane_carve(
            pane_id,
            PaneCarve {
                edge: PaneCarveEdge::Bottom,
                cells: 6,
                owner: Some("plugin:doomed".to_string()),
            },
        );
        assert_eq!(app.state.pane_carve_totals(pane_id), (0, 6, 0, 0));
        assert_eq!(app.state.clear_pane_carves_for_owner("plugin:doomed"), 1);
        assert_eq!(app.state.pane_carve_totals(pane_id), (0, 0, 0, 0));
    }

    /// The socket caller outranks a plugin and clears the pane outright — the
    /// manual escape hatch if a carve ever outlives its owner.
    #[test]
    fn the_socket_caller_releases_every_owners_carve() {
        let (mut app, public, pane_id) = app_with_pane();
        app.state.upsert_pane_carve(
            pane_id,
            PaneCarve {
                edge: PaneCarveEdge::Top,
                cells: 4,
                owner: Some("plugin:a".to_string()),
            },
        );
        let request: Request = serde_json::from_value(serde_json::json!({
            "id": "clear",
            "method": "pane.carve.clear",
            "params": {"pane_id": public}
        }))
        .expect("request");
        app.handle_api_request(request);
        assert_eq!(app.state.pane_carve_totals(pane_id), (0, 0, 0, 0));
    }
}
