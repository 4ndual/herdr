use base64::Engine;

use crate::api::schema::{
    PaneGraphicsClearParams, PaneGraphicsSetParams, PaneGraphicsStreamParams, ResponseResult,
    PANE_GRAPHICS_DIRECT_FILE_MAX_BYTES, PANE_GRAPHICS_MAX_LAYERS_PER_PANE,
    PANE_GRAPHICS_PRIMARY_LAYER_ID, PANE_GRAPHICS_SET_MAX_BYTES, PANE_GRAPHICS_STREAM_MAX_BYTES,
};
use crate::app::pane_graphics::{Key as PaneGraphicsKey, Layer, Slot};
use crate::app::App;
use crate::layout::PaneId;

use super::responses::{encode_error, encode_success};

impl App {
    /// Whether graphics for a pane can be placed on the surface rendered by this client.
    ///
    /// This deliberately follows the pane layout rather than pane existence: inactive
    /// workspaces/tabs and panes hidden by zoom are not placeable. Short-lived UI modes do not
    /// suspend the producer because the pane becomes visible again without a layout event.
    fn pane_graphics_visible(&self, ws_idx: usize, pane_id: PaneId) -> bool {
        if self.state.active != Some(ws_idx) {
            return false;
        }
        let Some(tab) = self.state.workspaces[ws_idx].active_tab() else {
            return false;
        };
        if tab.zoomed {
            tab.layout.focused() == pane_id
        } else {
            tab.layout.pane_ids().contains(&pane_id)
        }
    }

    pub(super) fn handle_pane_graphics_info(
        &mut self,
        id: String,
        target: crate::api::schema::PaneTarget,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };
        let pane_visible = self.pane_graphics_visible(ws_idx, pane_id);
        if !self.state.host_cell_size.is_known() {
            return encode_error(id, "cell_size_unavailable", "host cell size is unavailable");
        }
        // fork(+130): resolved from the laid-out view rather than from the
        // pane's PTY size. They agree while a pane is visible, but the view rect
        // is what placements are measured against, so a caller that clamps to it
        // clamps to the same box the compositor will clip to. Absent for a pane
        // that is not laid out this frame (a background tab), which is a real
        // answer and not an error — the pane exists, it just has no extent right
        // now, and `cell_*_px` is still useful to a caller preparing an image.
        let extents = self
            .state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .map(|info| info.inner_rect)
            .filter(|rect| rect.width > 0 && rect.height > 0);
        let file_frame_directory = self
            .direct_graphics_available
            .then(|| self.pane_graphics_files.source_directory().ok())
            .flatten();
        let direct = file_frame_directory.is_some();
        encode_success(
            id,
            ResponseResult::PaneGraphicsInfo {
                cell_width_px: self.state.host_cell_size.width_px,
                cell_height_px: self.state.host_cell_size.height_px,
                cols: extents.map(|rect| rect.width),
                rows: extents.map(|rect| rect.height),
                pane_visible,
                file_frame_directory: file_frame_directory
                    .map(|directory| directory.to_string_lossy().into_owned()),
                file_frame_formats: if direct {
                    vec!["rgba".into(), "bgra".into()]
                } else {
                    Vec::new()
                },
                file_frame_max_bytes: direct.then_some(PANE_GRAPHICS_STREAM_MAX_BYTES),
                file_frame_direct_max_bytes: direct.then_some(PANE_GRAPHICS_DIRECT_FILE_MAX_BYTES),
                // Damage metadata never changes the complete canonical frame contract.
                file_frame_damage: true,
                max_layers_per_pane: PANE_GRAPHICS_MAX_LAYERS_PER_PANE,
                pixel_mouse: self.pixel_mouse_available,
                file_frame_transport: direct.then(|| "direct-kitty".into()),
            },
        )
    }

    pub(crate) fn attach_pane_graphics_stream_active(
        &mut self,
        params: &PaneGraphicsStreamParams,
        active: std::sync::Arc<std::sync::atomic::AtomicBool>,
        response: &str,
    ) {
        if serde_json::from_str::<crate::api::schema::SuccessResponse>(response).is_err() {
            return;
        }
        let Some((_, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return;
        };
        let Ok(key) = graphics_key(pane_id, params.layer_id.as_deref()) else {
            return;
        };
        self.pane_graphics
            .attach_stream_active(&key, &params.owner, active);
    }

    pub(super) fn handle_pane_graphics_set(
        &mut self,
        id: String,
        params: PaneGraphicsSetParams,
    ) -> String {
        let Some((_, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let key = match graphics_key(pane_id, params.layer_id.as_deref()) {
            Ok(key) => key,
            Err(message) => return encode_error(id, "invalid_layer_id", message),
        };
        self.reclaim_inactive_stream(&key);
        if self
            .pane_graphics
            .slots
            .get(&key)
            .is_some_and(|slot| slot.stream_owner.is_some())
        {
            return encode_error(
                id,
                "stream_conflict",
                "pane graphics layer has an active stream",
            );
        }
        // fork: no owner-exclusivity check. Layers COEXIST, one per owner, so a
        // `set` replaces the caller's own layer and leaves other owners' layers
        // untouched. This supersedes the earlier `owner_conflict` refusal, which
        // assumed one layer per pane; with a list per pane there is nothing to
        // conflict over, and refusing would stop an image plugin and the user's
        // CLI from painting the same pane for no benefit.
        let owner = layer_owner(&params.owner);
        self.set_pane_graphics_layer(id, pane_id, key, params, owner)
    }

    /// fork(+127): re-anchored onto upstream's `set_layer` (Runtime/`Slot`
    /// engine, which drives the kitty incremental encoder and the new
    /// aggregate resource caps) while keeping the fork's own concerns: data
    /// may come from `source_path` (allowlisted for plugins) as well as
    /// `data`/`data_base64`, and a `(owner, layer_id)` caller is subject to a
    /// per-owner layer ceiling. `state.pane_graphics_layers` is written too —
    /// it is what the half-block fallback painter (`pane_graphics_paint.rs`)
    /// reads, so a layer must land in both stores to render on both backends.
    fn set_pane_graphics_layer(
        &mut self,
        id: String,
        pane_id: PaneId,
        key: PaneGraphicsKey,
        mut params: PaneGraphicsSetParams,
        owner: Option<String>,
    ) -> String {
        if params.image_width == 0 || params.image_height == 0 {
            return encode_error(
                id,
                "invalid_image",
                "image_width and image_height must be greater than zero",
            );
        }
        // fork: `source_path` is capped at the STREAM limit, not the SET limit.
        // The 512 KiB SET cap exists only because `data_base64` rides one JSON
        // line on the socket; a host-side file read has no such constraint, and
        // capping it at 512 KiB would exclude images that are otherwise fine
        // (3 of the 15 files in the real Claude image cache exceed it).
        let from_source_path = params.data.is_none() && !params.source_path.is_empty();
        let max_bytes = if params.data.is_some() || from_source_path {
            PANE_GRAPHICS_STREAM_MAX_BYTES
        } else {
            PANE_GRAPHICS_SET_MAX_BYTES
        };
        let data = match &params.data {
            Some(data) => data.clone(),
            None if from_source_path => {
                match self.read_pane_graphics_source(&params.source_path, owner.as_deref()) {
                    Ok(data) => data,
                    Err((code, message)) => return encode_error(id, code, message),
                }
            }
            None => match base64::engine::general_purpose::STANDARD.decode(&params.data_base64) {
                Ok(data) => data,
                Err(_) => {
                    return encode_error(id, "invalid_image", "data_base64 is not valid base64");
                }
            },
        };
        if data.is_empty() {
            return encode_error(id, "invalid_image", "image data must not be empty");
        }
        if data.len() > max_bytes {
            return encode_error(id, "image_too_large", "image data is too large");
        }
        match expected_len(params.format, params.image_width, params.image_height) {
            Ok(Some(expected)) if data.len() != expected => {
                return encode_error(
                    id,
                    "invalid_image",
                    "image data length does not match format and dimensions",
                );
            }
            Ok(_) => {}
            Err(()) => {
                return encode_error(id, "invalid_image", "image dimensions are too large");
            }
        }

        // fork(+127): with `(owner, layer_id)` identity a caller can hold many
        // layers per pane, so the count needs a ceiling. Each layer retains its
        // decoded bytes for the lifetime of the layer, so an unbounded count is
        // an unbounded host allocation driven by a sandboxed guest. Refused
        // explicitly rather than dropped, because a silently-ignored `set` is
        // precisely the failure a plugin author cannot diagnose from outside.
        //
        // Only counted when this is a NEW layer: replacing one of your own is
        // always allowed, or a plugin at the cap could never update its images.
        if !self
            .state
            .pane_graphics_layer_exists(pane_id, owner.as_deref(), &key.1)
        {
            let total = self
                .state
                .pane_graphics_owner_layer_total(pane_id, owner.as_deref());
            let limit = self.pane_graphics_layer_limit(owner.as_deref());
            if total >= limit {
                tracing::warn!(
                    owner = owner.as_deref().unwrap_or("<socket>"),
                    total,
                    limit,
                    "pane.graphics.set refused: owner is at its per-pane layer limit"
                );
                return encode_error(
                    id,
                    "layer_limit_reached",
                    format!("owner already holds {total} layers on this pane (limit {limit})"),
                );
            }
        }

        // Feed the resolved bytes back so `set_layer` (which only reads
        // `params.data`/`data_base64`) sees the same bytes this handler just
        // validated, regardless of whether they came from `data`,
        // `data_base64`, or `source_path`.
        params.data = Some(data.clone());
        let response = self.set_layer(id, key.clone(), params.clone());
        if serde_json::from_str::<crate::api::schema::SuccessResponse>(&response).is_ok() {
            let layer = PaneGraphicsLayer::new_with_layer_id(
                params.format,
                params.image_width,
                params.image_height,
                data,
                params.placement,
                owner,
                key.1,
            );
            self.state.upsert_pane_graphics_layer(pane_id, layer);
        }
        response
    }

    /// fork: read the bytes for a `source_path` set, enforcing the allowlist.
    ///
    /// The two callers are deliberately treated differently:
    ///
    /// * `owner == None` — a socket API caller. Unrestricted, because it grants
    ///   no new power: anything that can open the herdr socket can already read
    ///   the file itself and send it as `data_base64`. Refusing here would buy
    ///   nothing and break the efficient path for the user's own CLI.
    /// * `owner == Some(plugin)` — a UI plugin. Restricted to the roots that
    ///   plugin's manifest declared, because it DOES grant new power: the wasm
    ///   guest has no WASI and no host imports, so without an allowlist this
    ///   would turn a sandboxed module into an arbitrary-file-read primitive
    ///   with the host as its confused deputy.
    ///
    /// Canonicalisation happens before the prefix comparison, on both sides, so
    /// neither `..` nor a symlink planted inside an allowed root can escape it.
    /// The comparison is component-wise rather than string-prefix so that a root
    /// of `/a/img` does not accidentally admit `/a/imgsecrets/x.png`.
    /// fork(+127): how many layers this owner may hold on one pane.
    ///
    /// A plugin may raise or lower it with `max_layers_per_pane` in its manifest,
    /// clamped to `PANE_GRAPHICS_MAX_LAYER_LIMIT` — declared up front and visible
    /// in `plugin list` before anyone enables it, like every other reach a plugin
    /// asks for. The socket caller is the user and is not limited.
    fn pane_graphics_layer_limit(&self, owner: Option<&str>) -> usize {
        let Some(plugin_id) = owner.and_then(|owner| owner.strip_prefix("plugin:")) else {
            return usize::MAX;
        };
        self.ui_plugins
            .max_layers_per_pane_for_plugin(plugin_id)
            .unwrap_or(PANE_GRAPHICS_DEFAULT_LAYER_LIMIT)
            .min(PANE_GRAPHICS_MAX_LAYER_LIMIT)
    }

    fn read_pane_graphics_source(
        &self,
        source_path: &str,
        owner: Option<&str>,
    ) -> Result<Vec<u8>, (&'static str, String)> {
        let requested = std::path::Path::new(source_path);
        if !requested.is_absolute() {
            return Err((
                "invalid_source_path",
                "source_path must be an absolute path".to_string(),
            ));
        }
        let canonical = requested.canonicalize().map_err(|_| {
            (
                "source_not_found",
                "source_path does not resolve to an existing file".to_string(),
            )
        })?;
        let metadata = canonical.metadata().map_err(|_| {
            (
                "source_not_found",
                "source_path does not resolve to an existing file".to_string(),
            )
        })?;
        if !metadata.is_file() {
            return Err((
                "invalid_source_path",
                "source_path must name a regular file".to_string(),
            ));
        }
        if metadata.len() > PANE_GRAPHICS_STREAM_MAX_BYTES as u64 {
            return Err(("image_too_large", "image data is too large".to_string()));
        }

        if let Some(plugin_id) = owner.and_then(|owner| owner.strip_prefix("plugin:")) {
            let roots = self.ui_plugins.graphics_roots_for_plugin(plugin_id);
            let permitted = roots.iter().any(|root| {
                std::path::Path::new(root)
                    .canonicalize()
                    .is_ok_and(|root| canonical.starts_with(&root))
            });
            if !permitted {
                return Err((
                    "source_not_allowed",
                    "source_path is not under a graphics_roots entry declared by this plugin"
                        .to_string(),
                ));
            }
        }

        std::fs::read(&canonical).map_err(|_| {
            (
                "source_not_found",
                "source_path could not be read".to_string(),
            )
        })
    }

    pub(super) fn handle_pane_graphics_clear(
        &mut self,
        id: String,
        params: PaneGraphicsClearParams,
    ) -> String {
        let Some((_, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let key = match graphics_key(pane_id, params.layer_id.as_deref()) {
            Ok(key) => key,
            Err(message) => return encode_error(id, "invalid_layer_id", message),
        };
        self.reclaim_inactive_stream(&key);
        if self
            .pane_graphics
            .slots
            .get(&key)
            .is_some_and(|slot| slot.stream_owner.is_some())
        {
            return encode_error(
                id,
                "stream_conflict",
                "pane graphics layer has an active stream",
            );
        }
        // fork: a plugin clears only its own layer; the socket caller (`None`)
        // clears the whole pane. That asymmetry is deliberate and matches the
        // owner doc on `PaneGraphicsLayer`: the user at the CLI outranks any
        // plugin and needs one call that reliably wipes a pane, while a plugin
        // must never be able to erase another plugin's image.
        let owner = layer_owner(&params.owner);
        self.state
            .clear_pane_graphics_layers(pane_id, owner.as_deref(), params.layer_id.as_deref());
        // fork(+127): re-anchored onto upstream's Runtime `slots`: mirror the
        // same owner-scoped-vs-whole-pane asymmetry there so a layer set via
        // `set_layer` does not outlive its `state.pane_graphics_layers` twin.
        match owner {
            None => {
                let before = self.pane_graphics.slots.len();
                self.pane_graphics.slots.retain(|(id, _), _| *id != pane_id);
                if self.pane_graphics.slots.len() != before {
                    self.pane_graphics.mark_changed();
                }
            }
            Some(_) => {
                if self.pane_graphics.slots.remove(&key).is_some() {
                    self.pane_graphics.mark_changed();
                }
            }
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_graphics_stream_set(
        &mut self,
        id: String,
        params: PaneGraphicsSetParams,
    ) -> String {
        let Some((_, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let key = match graphics_key(pane_id, params.layer_id.as_deref()) {
            Ok(key) => key,
            Err(message) => return encode_error(id, "invalid_layer_id", message),
        };
        match self.pane_graphics.slots.get(&key) {
            Some(slot)
                if slot.stream_owner.as_ref() == Some(&params.owner) && slot.stream_is_active() =>
            {
                // fork: re-anchored onto upstream's Runtime-checked stream
                // ownership, but still dual-writes through
                // `set_pane_graphics_layer` (owner `None` — see the doc on
                // `set_pane_graphics_layer`'s stream caller) so a streamed
                // layer keeps rendering on the half-block backend too.
                self.set_pane_graphics_layer(id, pane_id, key, params, None)
            }
            Some(slot) if slot.stream_owner.as_ref() == Some(&params.owner) => {
                encode_error(id, "stream_closed", "pane graphics stream is not active")
            }
            Some(_) => encode_error(
                id,
                "stream_conflict",
                "pane graphics stream owner does not match active stream",
            ),
            None => encode_error(id, "stream_closed", "pane graphics stream is not active"),
        }
    }

    pub(super) fn handle_pane_graphics_stream_open(
        &mut self,
        id: String,
        params: PaneGraphicsStreamParams,
    ) -> String {
        if params.owner.is_empty() {
            return encode_error(
                id,
                "invalid_stream",
                "pane graphics stream owner is required",
            );
        }
        let Some((_, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let key = match graphics_key(pane_id, params.layer_id.as_deref()) {
            Ok(key) => key,
            Err(message) => return encode_error(id, "invalid_layer_id", message),
        };
        let stale = self
            .pane_graphics
            .slots
            .get(&key)
            .is_some_and(|slot| slot.stream_owner.is_some() && !slot.stream_is_active());
        if stale {
            self.pane_graphics.slots.remove(&key);
        }
        if self
            .pane_graphics
            .slots
            .get(&key)
            .and_then(|slot| slot.stream_owner.as_ref())
            .is_some()
        {
            return encode_error(
                id,
                "stream_conflict",
                "pane graphics layer has an active stream",
            );
        }
        let exists = self.pane_graphics.slots.contains_key(&key);
        if !self.pane_graphics.can_add_slot(&key)
            || (!exists
                && self.pane_graphics.layer_count(pane_id) >= PANE_GRAPHICS_MAX_LAYERS_PER_PANE)
        {
            return encode_error(id, "layer_limit", "pane graphics layer limit reached");
        }
        let Some(host_image_id) = self.pane_graphics.reserve_image_id(&key) else {
            return encode_error(id, "layer_limit", "pane graphics image ids are exhausted");
        };
        // fork: a stream is an EXCLUSIVE claim on the pane, so opening one drops
        // every owner's layer, not just the opener's — including layers plugins
        // set earlier, which get no notification. That is deliberate and
        // unchanged from upstream: `stream_conflict` already refuses every other
        // `set`/`clear` while a stream is active, so co-existing layers cannot be
        // kept coherent with a stream anyway. Nothing can be added back until the
        // stream closes, which is why the close path can still remove the whole
        // pane entry rather than a single owner's layer. Re-anchored onto
        // upstream's Runtime `slots` below for the stream-liveness bookkeeping
        // itself.
        self.state
            .pane_graphics_streams
            .insert(pane_id, params.owner.clone());
        self.state.pane_graphics_layers.remove(&pane_id);
        self.state.pane_graphics_revision = self.state.pane_graphics_revision.wrapping_add(1);
        self.pane_graphics.slots.insert(
            key,
            Slot {
                host_image_id,
                layer: None,
                stream_owner: Some(params.owner),
                stream_active: Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                    true,
                ))),
                direct_gate: None,
            },
        );
        self.pane_graphics.mark_changed();
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_graphics_stream_close(
        &mut self,
        id: String,
        params: PaneGraphicsStreamParams,
    ) -> String {
        let Some((_, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        if let Ok(key) = graphics_key(pane_id, params.layer_id.as_deref()) {
            if self
                .pane_graphics
                .slots
                .get(&key)
                .and_then(|slot| slot.stream_owner.as_ref())
                .is_some_and(|owner| owner == &params.owner)
            {
                self.pane_graphics.slots.remove(&key);
                self.pane_graphics.mark_changed();
            }
        }
        // fork: mirror `stream_open`'s exclusive-claim teardown in
        // `state.pane_graphics_streams`/`pane_graphics_layers`.
        if self
            .state
            .pane_graphics_streams
            .get(&pane_id)
            .is_some_and(|owner| owner == &params.owner)
        {
            self.state.pane_graphics_streams.remove(&pane_id);
            self.state.pane_graphics_layers.remove(&pane_id);
            self.state.pane_graphics_revision = self.state.pane_graphics_revision.wrapping_add(1);
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_pane_graphics_stream_direct(
        &mut self,
        id: String,
        params: crate::api::schema::PaneGraphicsDirectParams,
    ) -> String {
        let Some((_, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let key = match graphics_key(pane_id, params.layer_id.as_deref()) {
            Ok(key) => key,
            Err(message) => return encode_error(id, "invalid_layer_id", message),
        };
        let owner_matches = self.pane_graphics.slots.get(&key).is_some_and(|slot| {
            slot.stream_owner.as_deref() == Some(params.owner.as_str()) && slot.stream_is_active()
        });
        if !owner_matches {
            return encode_error(id, "stream_closed", "pane graphics stream is not active");
        }
        if params.image_width == 0 || params.image_height == 0 {
            return encode_error(
                id,
                "invalid_image",
                "image_width and image_height must be greater than zero",
            );
        }
        if !matches!(
            params.format,
            crate::api::schema::PaneGraphicsFormat::Rgba
                | crate::api::schema::PaneGraphicsFormat::Bgra
        ) {
            return encode_error(id, "invalid_image", "direct frames require rgba or bgra");
        }
        let primary = key.1 == PANE_GRAPHICS_PRIMARY_LAYER_ID;
        let direct = self.direct_graphics_available
            && primary
            && params.format == crate::api::schema::PaneGraphicsFormat::Rgba;
        let max_bytes = if direct {
            PANE_GRAPHICS_DIRECT_FILE_MAX_BYTES
        } else {
            PANE_GRAPHICS_STREAM_MAX_BYTES
        };
        let expected_len =
            match expected_len(params.format, params.image_width, params.image_height) {
                Ok(Some(len)) if len <= max_bytes => len,
                _ => return encode_error(id, "invalid_image", "invalid direct RGBA dimensions"),
            };
        let lease = match self
            .pane_graphics_files
            .lease(std::path::Path::new(&params.path), expected_len)
        {
            Ok(lease) => lease,
            Err(err) => return encode_error(id, "invalid_frame_file", err.to_string()),
        };
        if !direct && !self.pane_graphics.can_store_inline(&key, expected_len) {
            return encode_error(
                id,
                "graphics_budget_exceeded",
                "pane graphics inline memory limit reached",
            );
        }
        let layer = if direct {
            Layer::direct(
                params.image_width,
                params.image_height,
                lease,
                params.placement,
                params.z_index,
            )
        } else {
            let mut data = match lease.copy_rgba() {
                Ok(data) => data,
                Err(err) => return encode_error(id, "invalid_frame_file", err.to_string()),
            };
            canonicalize_bgra(params.format, &mut data);
            Layer::inline(
                crate::api::schema::PaneGraphicsFormat::Rgba,
                params.image_width,
                params.image_height,
                data,
                params.placement,
                params.z_index,
            )
        };
        let Some(slot) = self.pane_graphics.slots.get_mut(&key) else {
            return encode_error(id, "stream_closed", "pane graphics stream is not active");
        };
        if !slot.stream_is_active() {
            return encode_error(id, "stream_closed", "pane graphics stream is not active");
        }
        slot.layer = Some(layer);
        slot.direct_gate = None;
        self.pane_graphics.mark_changed();
        encode_success(
            id,
            ResponseResult::PaneGraphicsFrameAck {
                sequence: params.sequence,
                revision: params.revision,
            },
        )
    }

    fn set_layer(
        &mut self,
        id: String,
        key: PaneGraphicsKey,
        params: PaneGraphicsSetParams,
    ) -> String {
        if !self.pane_graphics.can_add_slot(&key)
            || (!self.pane_graphics.slots.contains_key(&key)
                && self.pane_graphics.layer_count(key.0) >= PANE_GRAPHICS_MAX_LAYERS_PER_PANE)
        {
            return encode_error(id, "layer_limit", "pane graphics layer limit reached");
        }
        if params.image_width == 0 || params.image_height == 0 {
            return encode_error(
                id,
                "invalid_image",
                "image_width and image_height must be greater than zero",
            );
        }
        let expected = match expected_len(params.format, params.image_width, params.image_height) {
            Ok(expected) => expected,
            Err(()) => return encode_error(id, "invalid_image", "image dimensions are too large"),
        };
        let streamed = params.data.is_some();
        let mut data = match params.data {
            Some(data) => data,
            None => match base64::engine::general_purpose::STANDARD.decode(params.data_base64) {
                Ok(data) => data,
                Err(_) => {
                    return encode_error(id, "invalid_image", "data_base64 is not valid base64")
                }
            },
        };
        let limit = if streamed {
            PANE_GRAPHICS_STREAM_MAX_BYTES
        } else {
            PANE_GRAPHICS_SET_MAX_BYTES
        };
        if data.is_empty() {
            return encode_error(id, "invalid_image", "image data must not be empty");
        }
        if data.len() > limit {
            return encode_error(id, "image_too_large", "image data is too large");
        }
        if expected.is_some_and(|size| size != data.len()) {
            return encode_error(
                id,
                "invalid_image",
                "image data does not match the frame contract",
            );
        }
        let format = canonicalize_bgra(params.format, &mut data);
        if !self.pane_graphics.can_store_inline(&key, data.len()) {
            return encode_error(
                id,
                "graphics_budget_exceeded",
                "pane graphics inline memory limit reached",
            );
        }
        let Some(host_image_id) = self.pane_graphics.reserve_image_id(&key) else {
            return encode_error(id, "layer_limit", "pane graphics image ids are exhausted");
        };
        // Move the liveness handle out before replacing the slot so Slot::drop cannot
        // deactivate the stream that the replacement still owns.
        let (stream_owner, stream_active) = self
            .pane_graphics
            .slots
            .get_mut(&key)
            .map(|slot| (slot.stream_owner.take(), slot.stream_active.take()))
            .unwrap_or_default();
        self.pane_graphics.slots.insert(
            key,
            Slot {
                host_image_id,
                layer: Some(Layer::inline(
                    format,
                    params.image_width,
                    params.image_height,
                    data,
                    params.placement,
                    params.z_index,
                )),
                stream_owner,
                stream_active,
                direct_gate: None,
            },
        );
        self.pane_graphics.mark_changed();
        encode_success(id, ResponseResult::Ok {})
    }

    fn reclaim_inactive_stream(&mut self, key: &PaneGraphicsKey) {
        let stale = self
            .pane_graphics
            .slots
            .get(key)
            .is_some_and(|slot| slot.stream_owner.is_some() && !slot.stream_is_active());
        if stale {
            self.pane_graphics.slots.remove(key);
            self.pane_graphics.mark_changed();
        }
    }
}

// fork(+127): `require_pane_graphics_enabled` was deleted here, and deleting it
// is NOT a loosening of the security boundary — read this before restoring it.
//
// It refused all five `pane.graphics.*` handlers unless `experimental.kitty_graphics`
// was on. But `kitty_graphics` is a RENDERER SELECTOR: it chooses between the
// kitty escape protocol and the half-block painter (`app/mod.rs`). Gating an API
// surface on it put two unrelated switches in series, so a plugin's own
// off-switch meant nothing while the flag was off, and turning the flag on was a
// precondition for using plugins rather than an alternative. That coupling was
// an accident of history and it is what +127 exists to remove.
//
// The real gate is untouched and is the only one that ever protected anything:
// a UI plugin reaching these methods through the bridge must hold the
// `pane_graphics` capability (`ui_plugin_api_bridge::PANE_GRAPHICS_METHODS`),
// declared in its manifest and visible in `plugin list` before anyone enables
// it, and a `source_path` must still canonicalise under a declared
// `graphics_roots` entry. Socket callers were never gated by capability at all
// and could already paint whenever kitty happened to be on — for them this was
// a coin-flip, not a permission.
//
// What made the deletion safe rather than merely tidy is `pane_graphics_paint`:
// layers now render on BOTH backends, so an accepted call is an honest promise
// of paint. Before +127, dropping this check would have accepted writes that
// silently never reached the screen. (fork(+127): `stream_open`/`stream_set`/
// `stream_close` below still dual-write `state.pane_graphics_layers` and
// `state.pane_graphics_streams`, re-anchored onto upstream's Runtime `slots`
// for the stream-liveness bookkeeping itself. `stream_direct` is the one
// exception — its BGRA/direct-file frames are Runtime-only, since that
// transport has no fork-era half-block equivalent.)

fn canonicalize_bgra(
    format: crate::api::schema::PaneGraphicsFormat,
    data: &mut [u8],
) -> crate::api::schema::PaneGraphicsFormat {
    if format != crate::api::schema::PaneGraphicsFormat::Bgra {
        return format;
    }
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    crate::api::schema::PaneGraphicsFormat::Rgba
}

fn graphics_key(pane_id: PaneId, layer_id: Option<&str>) -> Result<PaneGraphicsKey, &'static str> {
    let layer_id = layer_id.unwrap_or(PANE_GRAPHICS_PRIMARY_LAYER_ID);
    if layer_id.is_empty() || layer_id.len() > 64 {
        return Err("layer_id must contain between 1 and 64 characters");
    }
    if !layer_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err("layer_id contains unsupported characters");
    }
    Ok((pane_id, layer_id.to_owned()))
}

fn expected_len(
    format: crate::api::schema::PaneGraphicsFormat,
    width: u32,
    height: u32,
) -> Result<Option<usize>, ()> {
    let bytes = match format {
        crate::api::schema::PaneGraphicsFormat::Png => return Ok(None),
        crate::api::schema::PaneGraphicsFormat::Rgb => 3_u64,
        crate::api::schema::PaneGraphicsFormat::Rgba
        | crate::api::schema::PaneGraphicsFormat::Bgra => 4_u64,
    };
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(bytes))
        .and_then(|size| usize::try_from(size).ok())
        .map(Some)
        .ok_or(())
}

/// The layer owner carried by a `set`/`clear` request, or `None` when the call
/// came in over the socket API rather than the UI-plugin bridge.
///
/// `owner` is `#[serde(skip)]` on both params structs, so it can never be
/// supplied by a caller — not by a CLI, not by a wasm guest's JSON. The only
/// writer is `App::execute_ui_plugin_api_call`, which stamps the calling
/// module's plugin id after deserialization. That is what makes the ownership
/// check below a real boundary rather than an honour system.
fn layer_owner(owner: &str) -> Option<String> {
    let owner = owner.trim();
    if owner.is_empty() {
        None
    } else {
        Some(owner.to_string())
    }
}

/// fork(+127): default ceiling on layers one owner may hold on one pane.
///
/// 8 covers the real case comfortably — a Claude pane showing more than a
/// handful of `[Image #N]` placeholders at once is already unreadable — while
/// keeping the worst-case retained bytes per plugin bounded and predictable,
/// which is what makes per-plugin RAM measurable rather than a guess.
const PANE_GRAPHICS_DEFAULT_LAYER_LIMIT: usize = 8;

/// fork(+127): hard ceiling no manifest may exceed.
const PANE_GRAPHICS_MAX_LAYER_LIMIT: usize = 32;

fn pane_not_found(id: String, pane_id: &str) -> String {
    encode_error(id, "pane_not_found", format!("pane {pane_id} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ErrorResponse, SuccessResponse};
    use crate::config::Config;
    use crate::workspace::Workspace;

    // fork: silently dropped by the auto-merge (no conflict markers around
    // it) even though ~19 fork tests below still call it. Restored verbatim
    // so those tests compile again.
    fn app_with_test_workspace() -> (App, String) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("pane-graphics")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        (app, public_pane_id)
    }

    fn app() -> (App, String) {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("graphics")];
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let public = app.public_pane_id(0, pane).unwrap();
        app.state.kitty_graphics_enabled = true;
        (app, public)
    }

    fn frame(
        pane_id: String,
        layer_id: Option<&str>,
        owner: &str,
        value: u8,
    ) -> PaneGraphicsSetParams {
        PaneGraphicsSetParams {
            pane_id,
            layer_id: layer_id.map(str::to_owned),
            z_index: 2,
            owner: owner.to_owned(),
            format: crate::api::schema::PaneGraphicsFormat::Rgba,
            image_width: 1,
            image_height: 1,
            data: Some(vec![value; 4]),
            data_base64: String::new(),
            placement: Default::default(),
        }
    }

    fn pane_visible(response: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(response).unwrap()["result"]["pane_visible"]
            .as_bool()
            .unwrap()
    }

    #[test]
    fn api_pane_graphics_info_is_explicit_and_side_effect_free() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        let request = |pane_id: String| crate::api::schema::Request {
            id: "graphics-info".into(),
            method: crate::api::schema::Method::PaneGraphicsInfo(crate::api::schema::PaneTarget {
                pane_id,
            }),
        };

        // fork(+127): with the kitty renderer off, `.info` no longer refuses with
        // `feature_disabled` — it refuses with `cell_size_unavailable`, which is
        // the honest reason. Cell metrics are a property of the HOST TERMINAL,
        // not of which painter herdr chose, and conflating the two was the same
        // renderer/API coupling removed everywhere else in +127.
        let response = app.handle_api_request(request(public_pane_id.clone()));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "cell_size_unavailable");

        app.state.host_cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 11,
            height_px: 22,
        };
        let response = app.handle_api_request(request(public_pane_id));
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::PaneGraphicsInfo {
                cell_width_px: 11,
                cell_height_px: 22,
                ..
            }
        ));
        assert!(app.state.pane_graphics_layers.is_empty());
        assert!(app.state.pane_graphics_streams.is_empty());
    }

    #[test]
    fn info_reports_visibility_for_terminal_surface_workspace_tab_and_zoom() {
        let (mut app, pane_id) = app();
        app.state.mode = crate::app::Mode::Terminal;
        app.state.active = Some(0);
        app.state.host_cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        let visible = app.handle_pane_graphics_info(
            "visible".into(),
            crate::api::schema::PaneTarget {
                pane_id: pane_id.clone(),
            },
        );
        assert!(pane_visible(&visible));

        let hidden_workspace = Workspace::test_new("hidden");
        let hidden_workspace_pane = hidden_workspace.tabs[0].root_pane;
        app.state.workspaces.push(hidden_workspace);
        let hidden_workspace_id = app.public_pane_id(1, hidden_workspace_pane).unwrap();
        let hidden = app.handle_pane_graphics_info(
            "hidden-workspace".into(),
            crate::api::schema::PaneTarget {
                pane_id: hidden_workspace_id,
            },
        );
        assert!(!pane_visible(&hidden));

        let inactive_tab = app.state.workspaces[0].test_add_tab(Some("inactive"));
        let inactive_pane = app.state.workspaces[0].tabs[inactive_tab].root_pane;
        let inactive_id = app.public_pane_id(0, inactive_pane).unwrap();
        let hidden_tab = app.handle_pane_graphics_info(
            "hidden-tab".into(),
            crate::api::schema::PaneTarget {
                pane_id: inactive_id,
            },
        );
        assert!(!pane_visible(&hidden_tab));

        app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces[0].tabs[0].zoomed = true;
        let zoomed_away = app.handle_pane_graphics_info(
            "zoomed-away".into(),
            crate::api::schema::PaneTarget {
                pane_id: pane_id.clone(),
            },
        );
        assert!(!pane_visible(&zoomed_away));

        app.state.workspaces[0].tabs[0].zoomed = false;
        app.state.mode = crate::app::Mode::Navigate;
        let short_lived_mode = app.handle_pane_graphics_info(
            "navigate".into(),
            crate::api::schema::PaneTarget { pane_id },
        );
        assert!(pane_visible(&short_lived_mode));
    }

    #[test]
    fn monolithic_info_keeps_fast_transport_and_exact_pixels_disabled() {
        let (mut app, pane_id) = app();
        app.state.host_cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        assert!(!app.pane_graphics_files.is_initialized());
        let response = app
            .handle_pane_graphics_info("info".into(), crate::api::schema::PaneTarget { pane_id });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::PaneGraphicsInfo {
                file_frame_damage: true,
                max_layers_per_pane: 16,
                pixel_mouse: false,
                ..
            }
        ));
        assert!(app.pane_graphics.slots.is_empty());
        assert!(!app.pane_graphics_files.is_initialized());
    }

    #[cfg(unix)]
    #[test]
    fn info_advertises_rgba_direct_and_bgra_fallback_file_formats_on_demand() {
        let (mut app, pane_id) = app();
        app.state.host_cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        app.direct_graphics_available = true;
        let response = app
            .handle_pane_graphics_info("info".into(), crate::api::schema::PaneTarget { pane_id });
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(app.pane_graphics_files.is_initialized());
        assert_eq!(
            value["result"]["file_frame_formats"],
            serde_json::json!(["rgba", "bgra"])
        );
        assert_eq!(
            value["result"]["file_frame_max_bytes"],
            PANE_GRAPHICS_STREAM_MAX_BYTES
        );
        assert_eq!(
            value["result"]["file_frame_direct_max_bytes"],
            PANE_GRAPHICS_DIRECT_FILE_MAX_BYTES
        );
        assert_eq!(value["result"]["file_frame_damage"], true);
        assert_eq!(value["result"]["file_frame_transport"], "direct-kitty");
    }

    /// fork(+130): `.info` reports the pane's CELL extents alongside the cell
    /// pixel size, because pixel-per-cell alone never told a caller how many
    /// cells there are. A pane that is not laid out this frame OMITS them
    /// rather than reporting `0` — `0` is the value the placement API overloads
    /// to mean "the whole pane" (#40).
    #[test]
    fn pane_graphics_info_reports_cell_extents_and_omits_them_when_unlaid() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.host_cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 11,
            height_px: 22,
        };
        let request = || crate::api::schema::Request {
            id: "graphics-info".into(),
            method: crate::api::schema::Method::PaneGraphicsInfo(crate::api::schema::PaneTarget {
                pane_id: public_pane_id.clone(),
            }),
        };

        // No layout pass has run, so there is no rect to report.
        let response = app.handle_api_request(request());
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::PaneGraphicsInfo {
                cols: None,
                rows: None,
                ..
            }
        ));

        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(26, 1, 99, 49),
            inner_rect: ratatui::layout::Rect::new(27, 2, 97, 47),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];

        let response = app.handle_api_request(request());
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::PaneGraphicsInfo {
                cols: Some(97),
                rows: Some(47),
                ..
            }
        ));
    }

    #[test]
    fn layers_are_isolated_and_stream_owned() {
        let (mut app, pane_id) = app();
        for (layer, z, value) in [(None, 0, 1), (Some("chrome"), 10, 2)] {
            let mut request = frame(pane_id.clone(), layer, "", value);
            request.z_index = z;
            let response = app.handle_pane_graphics_set(format!("set-{value}"), request);
            assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        }
        let (_, pane) = app.parse_pane_id(&pane_id).unwrap();
        assert_eq!(
            app.pane_graphics
                .slots
                .get(&(pane, PANE_GRAPHICS_PRIMARY_LAYER_ID.into()))
                .and_then(|slot| slot.layer.as_ref())
                .unwrap()
                .z_index,
            0
        );
        assert_eq!(
            app.pane_graphics
                .slots
                .get(&(pane, "chrome".into()))
                .and_then(|slot| slot.layer.as_ref())
                .unwrap()
                .z_index,
            10
        );
        let response = app.handle_pane_graphics_stream_open(
            "open".into(),
            PaneGraphicsStreamParams {
                pane_id: pane_id.clone(),
                layer_id: Some("chrome".into()),
                z_index: 10,
                owner: "stream".into(),
            },
        );
        assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        let response =
            app.handle_pane_graphics_set("conflict".into(), frame(pane_id, Some("chrome"), "", 3));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "stream_conflict");
    }

    /// fork(+127): `handle_pane_graphics_set`/`clear` dual-write — the
    /// Runtime `slots` engine below AND `state.pane_graphics_layers`, which is
    /// what the half-block fallback painter reads (`pane_graphics_paint.rs`).
    #[test]
    fn api_pane_graphics_set_and_clear_updates_runtime_layers() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3, 4]);

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-set".into(),
            method: crate::api::schema::Method::PaneGraphicsSet(PaneGraphicsSetParams {
                layer_id: None,
                z_index: 0,
                pane_id: public_pane_id.clone(),
                owner: String::new(),
                format: crate::api::schema::PaneGraphicsFormat::Rgba,
                image_width: 1,
                image_height: 1,
                data: None,
                data_base64: data,
                source_path: String::new(),
                placement: crate::api::schema::PaneGraphicsPlacementParams {
                    grid_cols: Some(10),
                    grid_rows: Some(4),
                    ..Default::default()
                },
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));

        let layers = app
            .state
            .pane_graphics_layers
            .get(&pane_id)
            .expect("graphics layer stored");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].image_width, 1);
        assert_eq!(layers[0].image_height, 1);
        assert_eq!(layers[0].render.grid_cols, Some(10));
        assert_eq!(layers[0].render.grid_rows, Some(4));
        assert!(!app.state.session_dirty);
        assert_eq!(app.pane_graphics.slots.len(), 1);

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-clear".into(),
            method: crate::api::schema::Method::PaneGraphicsClear(PaneGraphicsClearParams {
                layer_id: None,
                pane_id: public_pane_id,
                owner: String::new(),
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert!(!app.state.pane_graphics_layers.contains_key(&pane_id));
        assert!(app.pane_graphics.slots.is_empty());
    }

    fn error_code(response: &str) -> String {
        serde_json::from_str::<ErrorResponse>(response)
            .unwrap()
            .error
            .code
    }

    #[test]
    fn validates_layer_and_frame_contracts() {
        let cases = [
            (Some("bad space"), 1, 1, vec![1; 4], "invalid_layer_id"),
            (None, 0, 1, vec![1; 4], "invalid_image"),
            (None, 1, 1, Vec::new(), "invalid_image"),
            (None, 1, 1, vec![1; 3], "invalid_image"),
        ];
        for (layer, width, height, data, expected) in cases {
            let (mut app, pane_id) = app();
            let mut params = frame(pane_id, layer, "", 1);
            params.image_width = width;
            params.image_height = height;
            params.data = Some(data);
            let response = app.handle_pane_graphics_set("invalid".into(), params);
            assert_eq!(error_code(&response), expected);
            assert!(app.pane_graphics.slots.is_empty());
        }
    }

    #[test]
    fn stream_owner_controls_only_its_named_layer() {
        let (mut app, pane_id) = app();
        let params = |owner: &str| PaneGraphicsStreamParams {
            pane_id: pane_id.clone(),
            layer_id: Some("chrome".into()),
            z_index: 7,
            owner: owner.into(),
        };
        assert!(serde_json::from_str::<SuccessResponse>(
            &app.handle_pane_graphics_stream_open("open".into(), params("owner"))
        )
        .is_ok());
        assert_eq!(
            error_code(&app.handle_pane_graphics_stream_set(
                "wrong".into(),
                frame(pane_id.clone(), Some("chrome"), "other", 2),
            )),
            "stream_conflict"
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&app.handle_pane_graphics_stream_set(
                "frame".into(),
                frame(pane_id.clone(), Some("chrome"), "owner", 3),
            ))
            .is_ok()
        );
        app.handle_pane_graphics_stream_close("stale".into(), params("other"));
        assert_eq!(app.pane_graphics.slots.len(), 1);
        app.handle_pane_graphics_stream_close("close".into(), params("owner"));
        assert!(app.pane_graphics.slots.is_empty());
    }

    #[test]
    fn static_set_and_clear_reclaim_inactive_stream_layers() {
        let (mut app, pane_id) = app();
        let open = |layer: &str| PaneGraphicsStreamParams {
            pane_id: pane_id.clone(),
            layer_id: Some(layer.into()),
            z_index: 0,
            owner: format!("owner-{layer}"),
        };
        for layer in ["set", "clear"] {
            assert!(serde_json::from_str::<SuccessResponse>(
                &app.handle_pane_graphics_stream_open(layer.into(), open(layer))
            )
            .is_ok());
            let (_, pane) = app.parse_pane_id(&pane_id).unwrap();
            app.pane_graphics.slots[&(pane, layer.into())]
                .stream_active
                .as_ref()
                .unwrap()
                .store(false, std::sync::atomic::Ordering::Release);
        }

        assert!(
            serde_json::from_str::<SuccessResponse>(&app.handle_pane_graphics_set(
                "set-static".into(),
                frame(pane_id.clone(), Some("set"), "", 4),
            ))
            .is_ok()
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&app.handle_pane_graphics_clear(
                "clear-static".into(),
                PaneGraphicsClearParams {
                    pane_id,
                    layer_id: Some("clear".into()),
                    owner: String::new(),
                },
            ))
            .is_ok()
        );
        assert_eq!(app.pane_graphics.slots.len(), 1);
        assert!(app
            .pane_graphics
            .slots
            .values()
            .all(|slot| slot.stream_owner.is_none()));
    }

    #[test]
    fn layer_limit_counts_empty_and_populated_stream_slots() {
        let (mut app, pane_id) = app();
        for index in 0..PANE_GRAPHICS_MAX_LAYERS_PER_PANE {
            let layer_id = format!("layer-{index}");
            let response = app.handle_pane_graphics_stream_open(
                layer_id.clone(),
                PaneGraphicsStreamParams {
                    pane_id: pane_id.clone(),
                    layer_id: Some(layer_id),
                    z_index: index as i32,
                    owner: format!("owner-{index}"),
                },
            );
            assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        }
        let response = app.handle_pane_graphics_set(
            "set-overflow".into(),
            frame(pane_id.clone(), Some("set-overflow"), "", 1),
        );
        assert_eq!(error_code(&response), "layer_limit");
        let response = app.handle_pane_graphics_stream_open(
            "overflow".into(),
            PaneGraphicsStreamParams {
                pane_id,
                layer_id: Some("overflow".into()),
                z_index: 0,
                owner: "overflow".into(),
            },
        );
        assert_eq!(error_code(&response), "layer_limit");
        assert_eq!(app.pane_graphics.slots.len(), 16);
    }

    #[test]
    fn set_and_clear_preserve_runtime_only_layer_details() {
        let (mut app, pane_id) = app();
        let (_, pane) = app.parse_pane_id(&pane_id).unwrap();
        let mut params = frame(pane_id.clone(), None, "", 9);
        params.placement.grid_cols = 10;
        params.placement.grid_rows = 4;
        assert!(serde_json::from_str::<SuccessResponse>(
            &app.handle_pane_graphics_set("set".into(), params)
        )
        .is_ok());
        let layer = app
            .pane_graphics
            .slots
            .get(&(pane, PANE_GRAPHICS_PRIMARY_LAYER_ID.into()))
            .and_then(|slot| slot.layer.as_ref())
            .unwrap();
        assert_eq!(layer.inline_data(), Some([9; 4].as_slice()));
        assert_eq!((layer.render.grid_cols, layer.render.grid_rows), (10, 4));
        assert!(!app.state.session_dirty);

        assert!(
            serde_json::from_str::<SuccessResponse>(&app.handle_pane_graphics_clear(
                "clear".into(),
                PaneGraphicsClearParams {
                    pane_id,
                    layer_id: None,
                    owner: String::new(),
                },
            ))
            .is_ok()
        );
        assert!(app.pane_graphics.slots.is_empty());
    }

    fn set_request(id: &str, pane_id: &str, owner: &str) -> crate::api::schema::Request {
        crate::api::schema::Request {
            id: id.into(),
            method: crate::api::schema::Method::PaneGraphicsSet(PaneGraphicsSetParams {
                layer_id: None,
                z_index: 0,
                pane_id: pane_id.to_string(),
                owner: owner.to_string(),
                format: crate::api::schema::PaneGraphicsFormat::Rgba,
                image_width: 1,
                image_height: 1,
                data: None,
                data_base64: base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3, 4]),
                source_path: String::new(),
                placement: Default::default(),
            }),
        }
    }

    /// fork: build a `UiPluginManager` holding one module of `plugin_id` that
    /// declares `roots`, so the allowlist has something real to consult.
    fn manager_with_graphics_roots(
        plugin_id: &str,
        roots: Vec<String>,
    ) -> crate::ui_plugins::UiPluginManager {
        let plugin = crate::api::schema::InstalledPluginInfo {
            plugin_id: plugin_id.into(),
            name: plugin_id.into(),
            version: "0.1.0".into(),
            min_herdr_version: crate::build_info::BASE_VERSION.into(),
            description: None,
            manifest_path: format!("/tmp/{plugin_id}/herdr-plugin.toml"),
            plugin_root: format!("/tmp/{plugin_id}"),
            enabled: true,
            platforms: None,
            build: vec![],
            startup: Vec::new(),
            actions: vec![],
            events: vec![],
            panes: vec![],
            ui_modules: vec![crate::api::schema::PluginManifestUiModule {
                rail_icon: None,
                max_layers_per_pane: None,
                id: "images".into(),
                title: "Images".into(),
                description: None,
                platforms: None,
                kind: Default::default(),
                api_version: 2,
                module: "dist/images.wasm".into(),
                hot_reload: Default::default(),
                surfaces: vec![crate::api::schema::PluginUiSurface::Header],
                placement: None,
                capabilities: vec![crate::api::schema::PluginUiCapability::PaneGraphics],
                ui_input_file: None,
                image_file: None,
                graphics_roots: roots,
                collector: None,
                host_actions: Vec::new(),
                host_action_mode: None,
            }],
            link_handlers: vec![],
            source: Default::default(),
            warnings: vec![],
        };
        crate::ui_plugins::UiPluginManager::from_installed_plugins(
            &std::collections::HashMap::from([(plugin.plugin_id.clone(), plugin)]),
        )
    }

    fn source_set_request(
        id: &str,
        pane_id: &str,
        owner: &str,
        source: &str,
    ) -> crate::api::schema::Request {
        crate::api::schema::Request {
            id: id.into(),
            method: crate::api::schema::Method::PaneGraphicsSet(PaneGraphicsSetParams {
                layer_id: None,
                z_index: 0,
                pane_id: pane_id.to_string(),
                owner: owner.to_string(),
                format: crate::api::schema::PaneGraphicsFormat::Rgba,
                image_width: 1,
                image_height: 1,
                data: None,
                data_base64: String::new(),
                source_path: source.to_string(),
                placement: Default::default(),
            }),
        }
    }

    fn source_test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr_pane_graphics_source_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The point of the allowlist. A plugin that declared no `graphics_roots`
    /// must not be able to make the host read a file for it — the wasm guest has
    /// no WASI, so without this check `source_path` would hand a sandboxed module
    /// an arbitrary-file-read primitive using the host as its deputy.
    #[test]
    fn plugin_source_path_is_refused_without_a_declared_root() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let dir = source_test_dir("undeclared");
        let file = dir.join("a.rgba");
        std::fs::write(&file, [1_u8, 2, 3, 4]).unwrap();

        let response = app.handle_api_request(source_set_request(
            "set",
            &public_pane_id,
            "plugin:images",
            file.to_str().unwrap(),
        ));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "source_not_allowed");
        assert!(app.state.pane_graphics_layers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A declared root works, and the bytes come off disk rather than the wire.
    #[test]
    fn plugin_source_path_is_allowed_under_a_declared_root() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
        let dir = source_test_dir("declared");
        let file = dir.join("a.rgba");
        std::fs::write(&file, [9_u8, 8, 7, 6]).unwrap();
        app.ui_plugins =
            manager_with_graphics_roots("images", vec![dir.to_string_lossy().to_string()]);

        let response = app.handle_api_request(source_set_request(
            "set",
            &public_pane_id,
            "plugin:images",
            file.to_str().unwrap(),
        ));
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        let layers = app.state.pane_graphics_layers.get(&pane_id).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].data, vec![9_u8, 8, 7, 6]);
        assert_eq!(layers[0].owner.as_deref(), Some("plugin:images"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The check that a string-prefix comparison would fail. A symlink planted
    /// INSIDE an allowed root pointing outside it must not smuggle the target
    /// through, which is why both sides are canonicalised before comparing.
    #[test]
    #[cfg(unix)]
    fn plugin_source_path_cannot_escape_a_declared_root_via_symlink() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let dir = source_test_dir("symlink");
        let allowed = dir.join("allowed");
        let secret_dir = dir.join("secret");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&secret_dir).unwrap();
        let secret = secret_dir.join("secret.rgba");
        std::fs::write(&secret, [1_u8, 2, 3, 4]).unwrap();
        let bait = allowed.join("bait.rgba");
        std::os::unix::fs::symlink(&secret, &bait).unwrap();
        app.ui_plugins =
            manager_with_graphics_roots("images", vec![allowed.to_string_lossy().to_string()]);

        let response = app.handle_api_request(source_set_request(
            "symlink",
            &public_pane_id,
            "plugin:images",
            bait.to_str().unwrap(),
        ));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "source_not_allowed");
        assert!(app.state.pane_graphics_layers.is_empty());

        // ..and `..` inside the request is equally dead, for the same reason.
        let traversal = allowed.join("../secret/secret.rgba");
        let response = app.handle_api_request(source_set_request(
            "traversal",
            &public_pane_id,
            "plugin:images",
            traversal.to_str().unwrap(),
        ));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "source_not_allowed");
        assert!(app.state.pane_graphics_layers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sibling directory whose name merely STARTS WITH an allowed root is not
    /// under it. `/x/img` must not admit `/x/img-secrets`.
    #[test]
    fn plugin_source_path_rejects_a_sibling_root_sharing_a_name_prefix() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let dir = source_test_dir("prefix");
        let allowed = dir.join("img");
        let sibling = dir.join("img-secrets");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let file = sibling.join("a.rgba");
        std::fs::write(&file, [1_u8, 2, 3, 4]).unwrap();
        app.ui_plugins =
            manager_with_graphics_roots("images", vec![allowed.to_string_lossy().to_string()]);

        let response = app.handle_api_request(source_set_request(
            "prefix",
            &public_pane_id,
            "plugin:images",
            file.to_str().unwrap(),
        ));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "source_not_allowed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The socket caller is deliberately unrestricted: it can already read the
    /// file itself and send it as `data_base64`, so an allowlist would cost it
    /// the efficient path while buying no safety.
    #[test]
    fn socket_caller_source_path_is_unrestricted() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
        let dir = source_test_dir("socket");
        let file = dir.join("a.rgba");
        std::fs::write(&file, [4_u8, 3, 2, 1]).unwrap();

        let response = app.handle_api_request(source_set_request(
            "set",
            &public_pane_id,
            "",
            file.to_str().unwrap(),
        ));
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            app.state.pane_graphics_layers.get(&pane_id).unwrap()[0].data,
            vec![4_u8, 3, 2, 1]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relative path, a directory, and a missing file are all refused before
    /// any allowlist question is asked.
    #[test]
    fn source_path_rejects_relative_directory_and_missing_targets() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let dir = source_test_dir("shape");

        let response =
            app.handle_api_request(source_set_request("rel", &public_pane_id, "", "a/b.rgba"));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_source_path");

        let response = app.handle_api_request(source_set_request(
            "dir",
            &public_pane_id,
            "",
            dir.to_str().unwrap(),
        ));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_source_path");

        let response = app.handle_api_request(source_set_request(
            "missing",
            &public_pane_id,
            "",
            dir.join("nope.rgba").to_str().unwrap(),
        ));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "source_not_found");
        assert!(app.state.pane_graphics_layers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// fork: two plugins COEXIST on one pane, one layer each, in a deterministic
    /// composite order. This replaces the earlier `owner_conflict` refusal — that
    /// rule existed only because a pane held a single layer, and with a list per
    /// pane it would block the ordinary case (an image plugin and something else
    /// painting the same pane) to prevent a collision that can no longer happen.
    #[test]
    fn api_pane_graphics_layers_from_two_plugins_coexist_on_one_pane() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();

        let response = app.handle_api_request(set_request("first", &public_pane_id, "plugin:b"));
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        let response = app.handle_api_request(set_request("second", &public_pane_id, "plugin:a"));
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();

        let layers = app.state.pane_graphics_layers.get(&pane_id).unwrap();
        assert_eq!(layers.len(), 2, "both plugins must hold a layer");
        // Composite order is by owner, not by arrival, so the paint order is
        // stable across frames instead of following insertion or hash order.
        assert_eq!(
            layers
                .iter()
                .map(|layer| layer.owner.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["plugin:a".to_string(), "plugin:b".to_string()]
        );

        // A second `set` from one plugin replaces ITS layer only.
        let response = app.handle_api_request(set_request("again", &public_pane_id, "plugin:a"));
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            app.state.pane_graphics_layers.get(&pane_id).unwrap().len(),
            2
        );
    }

    /// fork: a plugin's `clear` must not erase another plugin's image, while the
    /// socket caller's `clear` wipes the pane. Same asymmetry as `owner`.
    #[test]
    fn api_pane_graphics_clear_is_scoped_to_the_calling_plugin() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();

        for owner in ["plugin:a", "plugin:b", ""] {
            let response = app.handle_api_request(set_request("set", &public_pane_id, owner));
            let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        }
        assert_eq!(
            app.state.pane_graphics_layers.get(&pane_id).unwrap().len(),
            3
        );

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "clear-a".into(),
            method: crate::api::schema::Method::PaneGraphicsClear(PaneGraphicsClearParams {
                layer_id: None,
                pane_id: public_pane_id.clone(),
                owner: "plugin:a".into(),
            }),
        });
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        let layers = app.state.pane_graphics_layers.get(&pane_id).unwrap();
        assert_eq!(layers.len(), 2, "only the calling plugin's layer goes away");
        assert!(layers
            .iter()
            .all(|layer| layer.owner.as_deref() != Some("plugin:a")));

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "clear-cli".into(),
            method: crate::api::schema::Method::PaneGraphicsClear(PaneGraphicsClearParams {
                layer_id: None,
                pane_id: public_pane_id,
                owner: String::new(),
            }),
        });
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(
            app.state.pane_graphics_layers.is_empty(),
            "the socket caller clears the whole pane"
        );
    }

    /// The socket API outranks any plugin: the user at the CLI must always be
    /// able to take a pane back.
    #[test]
    fn api_pane_graphics_socket_caller_may_replace_and_clear_a_plugin_layer() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;

        let response = app.handle_api_request(set_request("plugin", &public_pane_id, "plugin:a"));
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();

        let response = app.handle_api_request(set_request("cli", &public_pane_id, ""));
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "cli-clear".into(),
            method: crate::api::schema::Method::PaneGraphicsClear(PaneGraphicsClearParams {
                layer_id: None,
                pane_id: public_pane_id,
                owner: String::new(),
            }),
        });
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(app.state.pane_graphics_layers.is_empty());
    }

    /// A plugin gets no unload event, so the host must drop its images when it
    /// is disabled. Without this `plugin disable` succeeds while the plugin's
    /// last image stays painted on the pane.
    #[test]
    fn disabling_a_plugin_clears_only_its_own_pane_graphics_layers() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();

        let response = app.handle_api_request(set_request("set", &public_pane_id, "plugin:images"));
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        let revision_before = app.state.pane_graphics_revision;

        assert_eq!(
            app.state
                .clear_pane_graphics_layers_for_owner("plugin:other"),
            0,
            "an unrelated plugin must not clear this layer"
        );
        assert_eq!(app.state.pane_graphics_revision, revision_before);
        assert!(app.state.pane_graphics_layers.contains_key(&pane_id));

        assert_eq!(
            app.state
                .clear_pane_graphics_layers_for_owner("plugin:images"),
            1
        );
        assert!(app.state.pane_graphics_layers.is_empty());
        assert_ne!(app.state.pane_graphics_revision, revision_before);
    }

    /// fork(+127): the inverse of the test this replaces.
    ///
    /// It used to assert `feature_disabled` when `experimental.kitty_graphics`
    /// was off. That gate is gone: it coupled an API surface to a RENDERER
    /// SELECTOR, so a plugin's own off-switch was meaningless whenever the flag
    /// happened to be off. What replaces it is this — with kitty off, a `set`
    /// SUCCEEDS and the layer is retained, because `pane_graphics_paint` renders
    /// it as half-blocks. The old behaviour and the new one are both about
    /// honesty: an accepted call must be a promise the host can keep, and now it
    /// is on both backends.
    ///
    /// The real gate is unchanged and lives elsewhere: a UI plugin needs the
    /// `pane_graphics` capability (`ui_plugin_api_bridge`), and `source_path`
    /// still has to canonicalise under a declared `graphics_roots` entry.
    #[test]
    fn api_pane_graphics_works_with_the_kitty_renderer_disabled() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
        assert!(
            !app.state.kitty_graphics_enabled,
            "precondition: the kitty renderer is off"
        );
        let data = base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3, 4]);

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-set".into(),
            method: crate::api::schema::Method::PaneGraphicsSet(PaneGraphicsSetParams {
                layer_id: None,
                z_index: 0,
                pane_id: public_pane_id.clone(),
                owner: String::new(),
                format: crate::api::schema::PaneGraphicsFormat::Rgba,
                image_width: 1,
                image_height: 1,
                data: None,
                data_base64: data,
                source_path: String::new(),
                placement: crate::api::schema::PaneGraphicsPlacementParams::default(),
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "graphics-set");
        assert_eq!(
            app.state
                .pane_graphics_layers
                .get(&pane_id)
                .map(Vec::len)
                .unwrap_or(0),
            1,
            "the layer is retained so the half-block compositor can paint it"
        );

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-clear".into(),
            method: crate::api::schema::Method::PaneGraphicsClear(PaneGraphicsClearParams {
                layer_id: None,
                pane_id: public_pane_id,
                owner: String::new(),
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "graphics-clear");
        assert!(!app.state.pane_graphics_layers.contains_key(&pane_id));
    }

    #[test]
    fn api_pane_graphics_stream_claim_uses_resolved_pane_identity() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
        let alias = format!("{public_pane_id}:alias");
        app.state
            .public_pane_id_aliases
            .insert(alias.clone(), pane_id);

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-stream-open".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamOpen(
                crate::api::schema::PaneGraphicsStreamParams {
                    pane_id: public_pane_id,
                    layer_id: None,
                    z_index: 0,
                    owner: "stream-1".into(),
                },
            ),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(
            app.state.pane_graphics_streams.get(&pane_id),
            Some(&"stream-1".to_string())
        );

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-stream-open-alias".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamOpen(
                crate::api::schema::PaneGraphicsStreamParams {
                    pane_id: alias.clone(),
                    layer_id: None,
                    z_index: 0,
                    owner: "stream-2".into(),
                },
            ),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "stream_conflict");
        assert_eq!(
            app.state.pane_graphics_streams.get(&pane_id),
            Some(&"stream-1".to_string())
        );

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-stream-close-wrong-owner".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamClose(
                crate::api::schema::PaneGraphicsStreamParams {
                    pane_id: alias.clone(),
                    layer_id: None,
                    z_index: 0,
                    owner: "stream-2".into(),
                },
            ),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(
            app.state.pane_graphics_streams.get(&pane_id),
            Some(&"stream-1".to_string())
        );

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-stream-close-alias".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamClose(
                crate::api::schema::PaneGraphicsStreamParams {
                    pane_id: alias,
                    layer_id: None,
                    z_index: 0,
                    owner: "stream-1".into(),
                },
            ),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert!(!app.state.pane_graphics_streams.contains_key(&pane_id));
    }

    #[test]
    fn api_pane_graphics_stream_owns_layer_while_active() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
        let public_data = base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3, 4]);

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-stream-open".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamOpen(
                crate::api::schema::PaneGraphicsStreamParams {
                    pane_id: public_pane_id.clone(),
                    layer_id: None,
                    z_index: 0,
                    owner: "stream-1".into(),
                },
            ),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-set-public".into(),
            method: crate::api::schema::Method::PaneGraphicsSet(PaneGraphicsSetParams {
                layer_id: None,
                z_index: 0,
                pane_id: public_pane_id.clone(),
                owner: String::new(),
                format: crate::api::schema::PaneGraphicsFormat::Rgba,
                image_width: 1,
                image_height: 1,
                data: None,
                data_base64: public_data.clone(),
                source_path: String::new(),
                placement: crate::api::schema::PaneGraphicsPlacementParams::default(),
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "stream_conflict");
        assert!(!app.state.pane_graphics_layers.contains_key(&pane_id));

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-set-stream".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamSet(PaneGraphicsSetParams {
                layer_id: None,
                z_index: 0,
                pane_id: public_pane_id.clone(),
                owner: "stream-1".into(),
                format: crate::api::schema::PaneGraphicsFormat::Rgba,
                image_width: 1,
                image_height: 1,
                data: None,
                data_base64: public_data,
                source_path: String::new(),
                placement: crate::api::schema::PaneGraphicsPlacementParams::default(),
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert!(app.state.pane_graphics_layers.contains_key(&pane_id));

        let wrong_owner_data = base64::engine::general_purpose::STANDARD.encode([5_u8, 6, 7, 8]);
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-set-stream-wrong-owner".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamSet(PaneGraphicsSetParams {
                layer_id: None,
                z_index: 0,
                pane_id: public_pane_id.clone(),
                owner: "stream-2".into(),
                format: crate::api::schema::PaneGraphicsFormat::Rgba,
                image_width: 1,
                image_height: 1,
                data: None,
                data_base64: wrong_owner_data,
                source_path: String::new(),
                placement: crate::api::schema::PaneGraphicsPlacementParams::default(),
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "stream_conflict");
        assert_eq!(
            app.state.pane_graphics_layers.get(&pane_id).unwrap()[0].data,
            vec![1_u8, 2, 3, 4]
        );

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-clear-public".into(),
            method: crate::api::schema::Method::PaneGraphicsClear(PaneGraphicsClearParams {
                layer_id: None,
                pane_id: public_pane_id.clone(),
                owner: String::new(),
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "stream_conflict");
        assert!(app.state.pane_graphics_layers.contains_key(&pane_id));

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-stream-close".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamClose(
                crate::api::schema::PaneGraphicsStreamParams {
                    pane_id: public_pane_id,
                    layer_id: None,
                    z_index: 0,
                    owner: "stream-1".into(),
                },
            ),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert!(!app.state.pane_graphics_layers.contains_key(&pane_id));
    }

    #[test]
    fn api_pane_graphics_rejects_overflowing_raw_dimensions() {
        for format in [
            crate::api::schema::PaneGraphicsFormat::Rgb,
            crate::api::schema::PaneGraphicsFormat::Rgba,
        ] {
            let (mut app, public_pane_id) = app_with_test_workspace();
            app.state.kitty_graphics_enabled = true;
            let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();

            let response = app.handle_api_request(crate::api::schema::Request {
                id: "graphics-overflow".into(),
                method: crate::api::schema::Method::PaneGraphicsSet(PaneGraphicsSetParams {
                    layer_id: None,
                    z_index: 0,
                    pane_id: public_pane_id,
                    owner: String::new(),
                    format,
                    image_width: u32::MAX,
                    image_height: u32::MAX,
                    data: None,
                    data_base64: base64::engine::general_purpose::STANDARD.encode([1_u8]),
                    source_path: String::new(),
                    placement: crate::api::schema::PaneGraphicsPlacementParams::default(),
                }),
            });

            let error: ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(error.error.code, "invalid_image");
            assert!(!app.state.pane_graphics_layers.contains_key(&pane_id));
        }
    }

    #[test]
    fn api_pane_graphics_validates_exact_raw_lengths() {
        for (format, exact_len) in [
            (crate::api::schema::PaneGraphicsFormat::Rgb, 6_usize),
            (crate::api::schema::PaneGraphicsFormat::Rgba, 8_usize),
        ] {
            for data_len in [exact_len, exact_len - 1] {
                let (mut app, public_pane_id) = app_with_test_workspace();
                app.state.kitty_graphics_enabled = true;
                let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
                let response = app.handle_api_request(crate::api::schema::Request {
                    id: "graphics-raw-length".into(),
                    method: crate::api::schema::Method::PaneGraphicsSet(PaneGraphicsSetParams {
                        layer_id: None,
                        z_index: 0,
                        pane_id: public_pane_id,
                        owner: String::new(),
                        format,
                        image_width: 2,
                        image_height: 1,
                        data: None,
                        data_base64: base64::engine::general_purpose::STANDARD
                            .encode(vec![1_u8; data_len]),
                        source_path: String::new(),
                        placement: crate::api::schema::PaneGraphicsPlacementParams::default(),
                    }),
                });

                if data_len == exact_len {
                    let success: SuccessResponse = serde_json::from_str(&response).unwrap();
                    assert!(matches!(success.result, ResponseResult::Ok {}));
                    assert!(app.state.pane_graphics_layers.contains_key(&pane_id));
                }
            }
        }
    }

    #[test]
    fn raw_formats_require_exact_non_overflowing_lengths() {
        for (format, exact) in [
            (crate::api::schema::PaneGraphicsFormat::Rgb, 6),
            (crate::api::schema::PaneGraphicsFormat::Rgba, 8),
        ] {
            for (length, expected) in [(exact, "ok"), (exact - 1, "invalid_image")] {
                let (mut app, pane_id) = app();
                let mut params = frame(pane_id, None, "", 1);
                params.format = format;
                params.image_width = 2;
                params.data = Some(vec![1; length]);
                let response = app.handle_pane_graphics_set("length".into(), params);
                if expected == "ok" {
                    assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
                } else {
                    assert_eq!(error_code(&response), expected);
                }
            }
            let (mut app, pane_id) = app();
            let mut params = frame(pane_id, None, "", 1);
            params.format = format;
            params.image_width = u32::MAX;
            params.image_height = u32::MAX;
            assert_eq!(
                error_code(&app.handle_pane_graphics_set("overflow".into(), params)),
                "invalid_image"
            );
        }
    }

    #[test]
    fn api_pane_graphics_uses_transport_specific_limits() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        app.state.kitty_graphics_enabled = true;
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-public-too-large".into(),
            method: crate::api::schema::Method::PaneGraphicsSet(PaneGraphicsSetParams {
                layer_id: None,
                z_index: 0,
                pane_id: public_pane_id,
                owner: String::new(),
                format: crate::api::schema::PaneGraphicsFormat::Png,
                image_width: 1,
                image_height: 1,
                data: None,
                data_base64: base64::engine::general_purpose::STANDARD.encode(vec![
                    1_u8;
                    PANE_GRAPHICS_SET_MAX_BYTES
                        + 1
                ]),
                source_path: String::new(),
                placement: crate::api::schema::PaneGraphicsPlacementParams::default(),
            }),
        });

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "image_too_large");
        assert!(!app.state.pane_graphics_layers.contains_key(&pane_id));
    }

    #[test]
    fn public_frames_validate_base64_and_public_size_limit() {
        let (mut app, pane_id) = app();
        let mut params = frame(pane_id.clone(), None, "", 1);
        params.format = crate::api::schema::PaneGraphicsFormat::Png;
        params.data = None;
        params.data_base64 = "not base64".into();
        assert_eq!(
            error_code(&app.handle_pane_graphics_set("base64".into(), params)),
            "invalid_image"
        );

        let mut params = frame(pane_id, None, "", 1);
        params.format = crate::api::schema::PaneGraphicsFormat::Png;
        params.data = None;
        params.data_base64 =
            base64::engine::general_purpose::STANDARD
                .encode(vec![1; PANE_GRAPHICS_SET_MAX_BYTES + 1]);
        assert_eq!(
            error_code(&app.handle_pane_graphics_set("large".into(), params)),
            "image_too_large"
        );
    }

    /// fork(+127): streaming, like the one-shot methods, no longer depends on
    /// the kitty renderer selector. The stream is still an EXCLUSIVE claim on
    /// the pane and is still never reachable from a wasm guest (it sits in
    /// `ui_plugin_api_bridge::BLOCKING_METHODS`); only the renderer coupling was
    /// removed.
    #[test]
    fn api_pane_graphics_stream_works_with_the_kitty_renderer_disabled() {
        let (mut app, public_pane_id) = app_with_test_workspace();
        let (_, pane_id) = app.parse_pane_id(&public_pane_id).unwrap();
        assert!(
            !app.state.kitty_graphics_enabled,
            "precondition: the kitty renderer is off"
        );

        let open = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-stream-open".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamOpen(
                crate::api::schema::PaneGraphicsStreamParams {
                    pane_id: public_pane_id.clone(),
                    layer_id: None,
                    z_index: 0,
                    owner: "stream-1".into(),
                },
            ),
        });
        let success: SuccessResponse = serde_json::from_str(&open).unwrap();
        assert_eq!(success.id, "graphics-stream-open");
        assert!(app.state.pane_graphics_streams.contains_key(&pane_id));

        let close = app.handle_api_request(crate::api::schema::Request {
            id: "graphics-stream-close".into(),
            method: crate::api::schema::Method::PaneGraphicsStreamClose(
                crate::api::schema::PaneGraphicsStreamParams {
                    pane_id: public_pane_id,
                    layer_id: None,
                    z_index: 0,
                    owner: "stream-1".into(),
                },
            ),
        });
        let success: SuccessResponse = serde_json::from_str(&close).unwrap();
        assert_eq!(success.id, "graphics-stream-close");
        assert!(!app.state.pane_graphics_streams.contains_key(&pane_id));
    }

    #[test]
    fn aliases_resolve_to_the_same_layer_identity() {
        let (mut app, pane_id) = app();
        let (_, pane) = app.parse_pane_id(&pane_id).unwrap();
        let alias = format!("{pane_id}:alias");
        app.state.public_pane_id_aliases.insert(alias.clone(), pane);
        let params = |target: String, owner: &str| PaneGraphicsStreamParams {
            pane_id: target,
            layer_id: None,
            z_index: 0,
            owner: owner.into(),
        };
        assert!(serde_json::from_str::<SuccessResponse>(
            &app.handle_pane_graphics_stream_open("open".into(), params(pane_id.clone(), "owner"),)
        )
        .is_ok());
        assert_eq!(
            error_code(&app.handle_pane_graphics_stream_open(
                "conflict".into(),
                params(alias.clone(), "other"),
            )),
            "stream_conflict"
        );
        app.handle_pane_graphics_stream_close("stale".into(), params(alias.clone(), "other"));
        assert_eq!(app.pane_graphics.slots.len(), 1);
        app.handle_pane_graphics_stream_close("close".into(), params(alias, "owner"));
        assert!(app.pane_graphics.slots.is_empty());
    }

    #[cfg(unix)]
    fn direct_file(app: &App, name: &str, data: &[u8]) -> String {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = app
            .pane_graphics_files
            .source_directory()
            .unwrap()
            .join(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(data).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    fn sparse_direct_file(app: &App, name: &str, len: usize) -> String {
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = app
            .pane_graphics_files
            .source_directory()
            .unwrap()
            .join(name);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.set_len(len as u64).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    fn direct_params(
        pane_id: String,
        owner: &str,
        path: String,
    ) -> crate::api::schema::PaneGraphicsDirectParams {
        crate::api::schema::PaneGraphicsDirectParams {
            pane_id,
            layer_id: None,
            z_index: 0,
            owner: owner.into(),
            image_width: 1,
            image_height: 1,
            format: crate::api::schema::PaneGraphicsFormat::Rgba,
            path,
            sequence: 1,
            revision: 1,
            placement: Default::default(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_file_is_leased_or_safely_copied_for_inline_fallback() {
        let (mut app, pane_id) = app();
        let stream = PaneGraphicsStreamParams {
            pane_id: pane_id.clone(),
            layer_id: None,
            z_index: 0,
            owner: "owner".into(),
        };
        app.handle_pane_graphics_stream_open("open".into(), stream.clone());
        for (width, height) in [(0, 1), (1, 0)] {
            let mut params = direct_params(pane_id.clone(), "owner", "unused".into());
            (params.image_width, params.image_height) = (width, height);
            assert_eq!(
                error_code(&app.handle_pane_graphics_stream_direct("invalid".into(), params)),
                "invalid_image"
            );
        }
        for format in [
            crate::api::schema::PaneGraphicsFormat::Rgb,
            crate::api::schema::PaneGraphicsFormat::Png,
        ] {
            let mut params = direct_params(pane_id.clone(), "owner", "unused".into());
            params.format = format;
            assert_eq!(
                error_code(&app.handle_pane_graphics_stream_direct("invalid".into(), params)),
                "invalid_image"
            );
        }
        app.direct_graphics_available = true;
        let path = direct_file(&app, "direct", &[1, 2, 3, 4]);
        let response = app.handle_pane_graphics_stream_direct(
            "direct".into(),
            crate::api::schema::PaneGraphicsDirectParams {
                sequence: 7,
                revision: 8,
                ..direct_params(pane_id.clone(), "owner", path)
            },
        );
        let ack: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            ack.result,
            ResponseResult::PaneGraphicsFrameAck {
                sequence: 7,
                revision: 8
            }
        ));
        assert!(app
            .pane_graphics
            .slots
            .values()
            .next()
            .unwrap()
            .layer
            .as_ref()
            .unwrap()
            .direct_lease()
            .is_some());

        app.handle_pane_graphics_stream_close("close".into(), stream.clone());
        app.handle_pane_graphics_stream_open("reopen".into(), stream);
        app.direct_graphics_available = false;
        let path = direct_file(&app, "fallback", &[5, 6, 7, 8]);
        app.handle_pane_graphics_stream_direct(
            "fallback".into(),
            crate::api::schema::PaneGraphicsDirectParams {
                sequence: 9,
                revision: 10,
                ..direct_params(pane_id, "owner", path)
            },
        );
        assert_eq!(
            app.pane_graphics
                .slots
                .values()
                .next()
                .unwrap()
                .layer
                .as_ref()
                .unwrap()
                .inline_data(),
            Some([5, 6, 7, 8].as_slice())
        );
    }

    #[cfg(unix)]
    #[test]
    fn direct_primary_rgba_accepts_fullscreen_retina_frame() {
        let (mut app, pane_id) = app();
        app.direct_graphics_available = true;
        app.handle_pane_graphics_stream_open(
            "open".into(),
            PaneGraphicsStreamParams {
                pane_id: pane_id.clone(),
                layer_id: None,
                z_index: 0,
                owner: "owner".into(),
            },
        );
        let (image_width, image_height) = (3456, 2234);
        let len = image_width * image_height * 4;
        let path = sparse_direct_file(&app, "retina-frame", len as usize);
        let response = app.handle_pane_graphics_stream_direct(
            "frame".into(),
            crate::api::schema::PaneGraphicsDirectParams {
                image_width,
                image_height,
                ..direct_params(pane_id, "owner", path)
            },
        );

        assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        assert!(app
            .pane_graphics
            .slots
            .values()
            .next()
            .unwrap()
            .layer
            .as_ref()
            .unwrap()
            .direct_lease()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn bgra_and_secondary_file_frames_are_canonical_owned_rgba() {
        let (mut app, pane_id) = app();
        app.direct_graphics_available = true;
        for (layer, format, source) in [
            (
                "browser-chrome",
                crate::api::schema::PaneGraphicsFormat::Rgba,
                [1, 2, 3, 4],
            ),
            (
                "primary",
                crate::api::schema::PaneGraphicsFormat::Bgra,
                [3, 2, 1, 4],
            ),
        ] {
            app.handle_pane_graphics_stream_open(
                format!("open-{layer}"),
                PaneGraphicsStreamParams {
                    pane_id: pane_id.clone(),
                    layer_id: Some(layer.into()),
                    z_index: 0,
                    owner: layer.into(),
                },
            );
            let path = direct_file(&app, layer, &source);
            app.handle_pane_graphics_stream_direct(
                format!("frame-{layer}"),
                crate::api::schema::PaneGraphicsDirectParams {
                    layer_id: Some(layer.into()),
                    format,
                    ..direct_params(pane_id.clone(), layer, path)
                },
            );
            let key = (app.parse_pane_id(&pane_id).unwrap().1, layer.into());
            assert_eq!(
                app.pane_graphics.slots[&key]
                    .layer
                    .as_ref()
                    .unwrap()
                    .inline_data(),
                Some([1, 2, 3, 4].as_slice())
            );
        }
    }

}
