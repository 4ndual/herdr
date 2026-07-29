//! fork(+128): handlers for the retained text-overlay layers (`pane.text.*`).
//!
//! The companion to `pane_graphics.rs`, and deliberately much smaller: there is
//! no decode step, no `source_path` file read, and no streaming mode, because
//! the payload is characters that arrive whole on one JSON line. What it does
//! share is everything that governs a layer's LIFETIME — `(owner, layer_id)`
//! identity, the per-owner layer cap, and the privilege asymmetry that lets a
//! socket caller clear a pane a plugin may only clear its own share of.
//!
//! ## The invariant this file must not break
//!
//! Nothing here may touch `PaneState`, the parser, or scrollback. The overlay
//! reaches the screen only through `pane_text_paint`, which composites into the
//! ratatui buffer for one frame. That is what keeps `pane.read`, copy, search,
//! agent detection and agent resume reading real program output, so a plugin
//! cannot make an agent believe it printed something it did not. If a future
//! change makes an overlay durable or visible to a reader, it has stopped being
//! an overlay and needs its own capability and its own review.

use crate::api::schema::{
    PaneTextClearParams, PaneTextSetParams, ResponseResult, PANE_TEXT_SET_MAX_BYTES,
};
use crate::app::state::PaneTextLayer;
use crate::app::App;

use super::responses::{encode_error, encode_success};

/// Mirrors `PANE_GRAPHICS_LAYER_ID_MAX_LEN`: a `layer_id` is a short opaque
/// handle, not a place to stash data in host state.
const PANE_TEXT_LAYER_ID_MAX_LEN: usize = 128;

/// Ceiling on text layers one owner may hold on one pane.
///
/// Matches the graphics default rather than inventing a second number. The
/// bound is what makes a plugin's worst-case retained bytes predictable, and
/// two different limits for two nearly identical APIs would be a trap for the
/// author who learned the first one.
const PANE_TEXT_LAYER_LIMIT: usize = 8;

impl App {
    pub(super) fn handle_pane_text_set(&mut self, id: String, params: PaneTextSetParams) -> String {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        if params.layer_id.len() > PANE_TEXT_LAYER_ID_MAX_LEN {
            return encode_error(id, "invalid_layer", "layer_id is too long");
        }
        if params.lines.is_empty() {
            // Refused rather than accepted as an empty layer. An empty `set` can
            // only mean the caller meant `pane.text.clear` or built its lines
            // wrongly; retaining a layer that paints nothing would make both
            // mistakes look like success and show up as an overlay that is
            // "installed" but invisible — the exact silent failure this API is
            // supposed to avoid.
            return encode_error(
                id,
                "invalid_text",
                "lines must not be empty; use pane.text.clear to remove a layer",
            );
        }

        let bytes: usize = params
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.len())
            .sum();
        if bytes > PANE_TEXT_SET_MAX_BYTES {
            return encode_error(id, "text_too_large", "overlay text is too large");
        }
        // Control characters are stripped, not refused. An overlay is painted
        // cell by cell, so an embedded ESC or newline is never interpreted as an
        // escape sequence — but it would consume a cell and render as garbage,
        // and refusing the whole call over one stray `\n` in a plugin's format
        // string would be a worse experience than dropping it.
        let lines: Vec<_> = params
            .lines
            .into_iter()
            .map(|mut line| {
                for span in &mut line.spans {
                    span.text.retain(|ch| !ch.is_control());
                }
                line
            })
            .collect();

        let owner = layer_owner(&params.owner);
        if let Some(owner) = owner.as_deref() {
            let held = self
                .state
                .pane_text_layers
                .get(&pane_id)
                .map(|layers| {
                    layers
                        .iter()
                        .filter(|layer| {
                            layer.owner.as_deref() == Some(owner)
                                && layer.layer_id != params.layer_id
                        })
                        .count()
                })
                .unwrap_or(0);
            // Counted excluding this layer_id so a plugin refreshing a layer it
            // already holds is never refused at the limit — only genuinely
            // adding a new one is.
            if held >= PANE_TEXT_LAYER_LIMIT {
                return encode_error(id, "layer_limit", "too many text layers on this pane");
            }
        }

        self.state.upsert_pane_text_layer(
            pane_id,
            PaneTextLayer {
                lines,
                render: params.placement,
                owner,
                layer_id: params.layer_id,
            },
        );

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_text_clear(
        &mut self,
        id: String,
        params: PaneTextClearParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        self.state.clear_pane_text_layers(
            pane_id,
            layer_owner(&params.owner).as_deref(),
            params.layer_id.as_deref(),
        );
        encode_success(id, ResponseResult::Ok {})
    }
}

/// Empty owner means the call arrived over the socket, which is the privileged
/// caller. Same convention as `pane_graphics::layer_owner`.
fn layer_owner(owner: &str) -> Option<String> {
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
    use crate::api::schema::{
        ErrorResponse, PaneGraphicsPlacementParams, PaneTextLineParams, PaneTextSpanParams,
        SuccessResponse,
    };
    use crate::config::Config;
    use crate::workspace::Workspace;

    fn app_with_test_workspace() -> (App, String) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("pane-text")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let public_pane_id = app.public_pane_id(0, pane_id).expect("public pane id");
        (app, public_pane_id)
    }

    fn line(text: &str) -> PaneTextLineParams {
        PaneTextLineParams {
            spans: vec![PaneTextSpanParams {
                text: text.to_string(),
                ..Default::default()
            }],
        }
    }

    fn set_params(pane: &str, owner: &str, layer_id: &str) -> PaneTextSetParams {
        PaneTextSetParams {
            pane_id: pane.to_string(),
            owner: owner.to_string(),
            layer_id: layer_id.to_string(),
            lines: vec![line("hello")],
            placement: PaneGraphicsPlacementParams::default(),
        }
    }

    fn is_ok(raw: &str) -> bool {
        serde_json::from_str::<SuccessResponse>(raw).is_ok()
    }

    fn error_code(raw: &str) -> String {
        serde_json::from_str::<ErrorResponse>(raw)
            .expect("error response")
            .error
            .code
    }

    #[test]
    fn a_set_retains_a_layer_that_paints_the_given_text() {
        let (mut app, pane) = app_with_test_workspace();
        let raw = app.handle_pane_text_set("1".into(), set_params(&pane, "plugin:a", ""));
        assert!(is_ok(&raw));
        let layers = app
            .state
            .pane_text_layers
            .values()
            .next()
            .expect("a layer was retained");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].lines[0].spans[0].text, "hello");
    }

    /// The whole point of the `(owner, layer_id)` pair. Before +127 taught the
    /// graphics side this lesson, a second `set` from one plugin replaced the
    /// first; text overlays must not repeat it.
    #[test]
    fn two_layer_ids_from_one_owner_coexist_rather_than_replacing() {
        let (mut app, pane) = app_with_test_workspace();
        app.handle_pane_text_set("1".into(), set_params(&pane, "plugin:a", "top"));
        app.handle_pane_text_set("2".into(), set_params(&pane, "plugin:a", "bottom"));
        let layers = app.state.pane_text_layers.values().next().expect("layers");
        assert_eq!(layers.len(), 2);
    }

    #[test]
    fn resetting_the_same_layer_id_replaces_it_in_place() {
        let (mut app, pane) = app_with_test_workspace();
        app.handle_pane_text_set("1".into(), set_params(&pane, "plugin:a", "only"));
        let mut second = set_params(&pane, "plugin:a", "only");
        second.lines = vec![line("replaced")];
        app.handle_pane_text_set("2".into(), second);
        let layers = app.state.pane_text_layers.values().next().expect("layers");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].lines[0].spans[0].text, "replaced");
    }

    /// A plugin must never be able to erase another plugin's overlay — the same
    /// rule `pane.graphics.clear` enforces.
    #[test]
    fn a_plugin_clear_leaves_another_owners_layer_alone() {
        let (mut app, pane) = app_with_test_workspace();
        app.handle_pane_text_set("1".into(), set_params(&pane, "plugin:a", ""));
        app.handle_pane_text_set("2".into(), set_params(&pane, "plugin:b", ""));
        let raw = app.handle_pane_text_clear(
            "3".into(),
            PaneTextClearParams {
                pane_id: pane.clone(),
                layer_id: None,
                owner: "plugin:a".into(),
            },
        );
        assert!(is_ok(&raw));
        let layers = app.state.pane_text_layers.values().next().expect("layers");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].owner.as_deref(), Some("plugin:b"));
    }

    /// The socket caller is the user, and outranks every plugin.
    #[test]
    fn a_socket_clear_wipes_the_pane_including_plugin_layers() {
        let (mut app, pane) = app_with_test_workspace();
        app.handle_pane_text_set("1".into(), set_params(&pane, "plugin:a", ""));
        app.handle_pane_text_set("2".into(), set_params(&pane, "plugin:b", ""));
        app.handle_pane_text_clear(
            "3".into(),
            PaneTextClearParams {
                pane_id: pane.clone(),
                layer_id: None,
                owner: String::new(),
            },
        );
        assert!(app.state.pane_text_layers.is_empty());
    }

    /// Disabling a plugin has to take its overlays with it: there is no unload
    /// event, so nothing else will.
    #[test]
    fn owner_cleanup_removes_every_overlay_that_plugin_held() {
        let (mut app, pane) = app_with_test_workspace();
        app.handle_pane_text_set("1".into(), set_params(&pane, "plugin:a", "one"));
        app.handle_pane_text_set("2".into(), set_params(&pane, "plugin:a", "two"));
        app.handle_pane_text_set("3".into(), set_params(&pane, "plugin:b", ""));
        assert_eq!(app.state.clear_pane_text_layers_for_owner("plugin:a"), 2);
        let layers = app.state.pane_text_layers.values().next().expect("layers");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].owner.as_deref(), Some("plugin:b"));
    }

    #[test]
    fn an_empty_set_is_refused_rather_than_retained_as_an_invisible_layer() {
        let (mut app, pane) = app_with_test_workspace();
        let mut params = set_params(&pane, "plugin:a", "");
        params.lines.clear();
        let raw = app.handle_pane_text_set("1".into(), params);
        assert_eq!(error_code(&raw), "invalid_text");
        assert!(app.state.pane_text_layers.is_empty());
    }

    #[test]
    fn control_characters_are_stripped_so_an_overlay_cannot_emit_escapes() {
        let (mut app, pane) = app_with_test_workspace();
        let mut params = set_params(&pane, "plugin:a", "");
        params.lines = vec![line("a\u{1b}[31mb\nc")];
        app.handle_pane_text_set("1".into(), params);
        let layers = app.state.pane_text_layers.values().next().expect("layers");
        assert_eq!(layers[0].lines[0].spans[0].text, "a[31mbc");
    }

    #[test]
    fn a_plugin_cannot_hold_more_layers_than_the_cap() {
        let (mut app, pane) = app_with_test_workspace();
        for n in 0..PANE_TEXT_LAYER_LIMIT {
            let raw = app
                .handle_pane_text_set(n.to_string(), set_params(&pane, "plugin:a", &n.to_string()));
            assert!(is_ok(&raw), "layer {n} should fit under the cap");
        }
        let raw = app.handle_pane_text_set("over".into(), set_params(&pane, "plugin:a", "over"));
        assert_eq!(error_code(&raw), "layer_limit");
    }

    /// Refreshing an existing layer is not "adding one", so a plugin sitting at
    /// the cap must still be able to update what it already painted.
    #[test]
    fn refreshing_an_existing_layer_at_the_cap_still_succeeds() {
        let (mut app, pane) = app_with_test_workspace();
        for n in 0..PANE_TEXT_LAYER_LIMIT {
            app.handle_pane_text_set(n.to_string(), set_params(&pane, "plugin:a", &n.to_string()));
        }
        let raw = app.handle_pane_text_set("again".into(), set_params(&pane, "plugin:a", "0"));
        assert!(is_ok(&raw));
    }

    /// Regression guard for the gap the +128 sandbox E2E caught: the bridge in
    /// `app/mod.rs` must stamp `owner` on both text methods. An unstamped call
    /// still paints, so the bug is invisible on screen — it shows up only as a
    /// layer recorded with the privileged `None` owner, which the plugin-disable
    /// cleanup cannot find and any other plugin may clear.
    ///
    /// Asserted here, at the handler, because that is what the two behaviours
    /// downstream actually key on. If the stamping arm is ever dropped, the
    /// owner-cleanup and cross-plugin-isolation tests above stop meaning
    /// anything for real guests while still passing.
    #[test]
    fn a_stamped_owner_is_what_makes_cleanup_and_isolation_work() {
        let (mut app, pane) = app_with_test_workspace();
        app.handle_pane_text_set("1".into(), set_params(&pane, "plugin:a", ""));
        let layers = app.state.pane_text_layers.values().next().expect("layers");
        assert_eq!(
            layers[0].owner.as_deref(),
            Some("plugin:a"),
            "an owned call must not be recorded as the privileged socket caller"
        );

        // And the converse: an empty owner really is the socket caller, so the
        // privilege split is a property of the value and not of the call path.
        let (mut app, pane) = app_with_test_workspace();
        app.handle_pane_text_set("1".into(), set_params(&pane, "", ""));
        let layers = app.state.pane_text_layers.values().next().expect("layers");
        assert_eq!(layers[0].owner, None);
    }

    #[test]
    fn an_unknown_pane_is_refused() {
        let (mut app, _pane) = app_with_test_workspace();
        let raw = app.handle_pane_text_set("1".into(), set_params("w9:p9", "plugin:a", ""));
        assert_eq!(error_code(&raw), "pane_not_found");
    }
}
