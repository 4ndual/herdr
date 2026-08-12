//! Claude `[Image #N]` previews, as a plugin.
//!
//! This module is the replacement for `src/host_image.rs`'s
//! `resolve_pane_placeholders` path — the last piece of Claude-specific image
//! policy that lived inside herdr and could only be removed by rebuilding it.
//! Everything the host keeps is generic: a `pane_graphics` capability, an
//! owner-tagged layer list keyed by `(owner, layer_id)`, a `source_path` whose
//! directory must appear in this plugin's own `graphics_roots`, and a half-block
//! compositor that knows nothing about Claude.
//!
//! Division of labour, forced by the fact that a wasm32 guest here has no WASI
//! and no host imports (it cannot open a file, list a directory, or read a PNG
//! header):
//!
//!   * the collector answers "which `N` exist on disk, where, how big" and
//!     writes `image_input.json`;
//!   * this guest answers "which `N` are on screen, at which cell" — it reads
//!     the pane text through `pane.read` and parses the placeholders itself —
//!     then joins the two by `N` and issues one `pane.graphics.set` per image
//!     with `layer_id = "img-<N>"`;
//!   * the host reads the bytes.
//!
//! Image BYTES never enter this module.
//!
//! ## Known limitation: `col_exact`
//!
//! Placeholder columns are computed as a **character index** into the row text
//! returned by `pane.read`, not as a terminal display column. Those differ for
//! any row containing a wide (CJK, emoji) or combining character to the LEFT of
//! the `[Image #N]`. When that happens the image is placed a few cells off
//! horizontally; it is never placed in the wrong pane or the wrong row.
//!
//! This is shipped deliberately rather than fixed, because fixing it properly
//! means a new host read surface (`pane.read { format: "cells" }`) and +127's
//! entire point is to stop growing host API. The guest reports the assumption
//! per placeholder via `col_exact`, and the status line says `~col` while any
//! visible placeholder is inexact, so the limitation is visible where a user
//! will actually meet it rather than only in a commit message.

use herdr_ui_plugin_api::{HostEffect, UiEvent, UiLayer, UiSurface, UiTextSpan, UiUpdate};
use serde::Deserialize;
use std::sync::Mutex;

/// Request id for the pane listing (which pane is focused, and its session).
const REQ_PANES: &str = "claude-image:panes";
/// Prefix for the per-pane text read. The pane id is appended so a late result
/// for a pane we have since left is recognised and dropped.
const REQ_READ_PREFIX: &str = "claude-image:read:";
/// Prefix for paint calls; the `N` is appended so a refusal can be attributed to
/// one image rather than invalidating every layer.
const REQ_SET_PREFIX: &str = "claude-image:set:";
/// Prefix for clear calls.
const REQ_CLEAR_PREFIX: &str = "claude-image:clear:";
/// Request id for the one-shot cell-geometry probe. Asked once per module load:
/// cell pixel size is a font property, not something that changes per frame.
const REQ_INFO: &str = "claude-image:info";

/// Ceiling on layers this guest will ask for, matching the manifest's
/// `max_layers_per_pane`. Enforced here as well as in the host so the guest is
/// never the thing that trips the host's limit and generates error traffic.
const MAX_LAYERS: usize = 8;

/// The BOX a placeholder preview is fitted into, in cells. Deliberately a GUEST
/// policy, not a host one: "how big should a Claude image preview be" is exactly
/// the sort of decision that used to require editing herdr.
///
/// These are a CEILING, not a size. Emitting them as the size — which is what
/// this plugin used to do — draws every image at the same 40x12 no matter its
/// shape, so a tall screenshot and a wide banner come out equally squashed. The
/// actual box comes from `fit_preview`.
const MAX_PREVIEW_COLS: u32 = 40;
const MAX_PREVIEW_ROWS: u32 = 12;

/// Cell size assumed until `pane.graphics.info` answers. Roughly a 12pt
/// monospace cell; only the RATIO matters here, and 1:2 is close enough that a
/// preview drawn before the probe returns is not visibly wrong.
const ASSUMED_CELL_PX: (u32, u32) = (8, 16);

// ── input from the collector ───────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ImageEntry {
    number: u32,
    path: String,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct ImageInput {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    images: Vec<ImageEntry>,
}

// ── placeholders parsed out of pane text ───────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placeholder {
    number: u32,
    /// Character index of the `[` within its row. See the `col_exact` note in
    /// the module docs.
    col: u16,
    row: u16,
    /// False when the row contains a non-ASCII character before the `[`, i.e.
    /// when the character index is not guaranteed to equal the display column.
    col_exact: bool,
}

/// What we asked the host to paint, so an unchanged screen costs nothing.
///
/// The BOX is part of this key, not just the position. Keying only on
/// `(path, col, row)` means a preview whose fitted size changed — because the
/// cell probe answered, or the collector reported different image dimensions —
/// is judged "already painted" and never re-sent, so the image keeps whatever
/// box it was first given.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Painted {
    pane_id: String,
    number: u32,
    path: String,
    col: u16,
    row: u16,
    grid_cols: u32,
    grid_rows: u32,
}

#[derive(Default)]
struct State {
    pane_id: Option<String>,
    /// Session of the focused pane, from `pane.list`. Compared against the
    /// collector's `session_id`; a mismatch paints nothing, so a focus change
    /// racing a collector run can never paint one session's images onto
    /// another's pane.
    pane_session: Option<String>,
    panes_in_flight: bool,
    read_in_flight: bool,
    /// Cell size in pixels, from `pane.graphics.info`. `None` until the probe
    /// answers; `fit_preview` falls back rather than blocking on it, because a
    /// preview that never paints is worse than one fitted with an assumed cell
    /// aspect for a frame.
    cell_px: Option<(u32, u32)>,
    info_in_flight: bool,
    /// Set once the probe has answered either way. A refusal must not leave the
    /// guest asking forever, so this is set on failure too.
    info_settled: bool,
    /// Placeholders from the most recent successful `pane.read`.
    placeholders: Vec<Placeholder>,
    /// Layers we believe are currently set, in `layer_id` order.
    painted: Vec<Painted>,
    /// Last refusal, surfaced on the status line rather than swallowed.
    last_error: Option<String>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

// ── ABI ────────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_api_version() -> i32 {
    3
}

/// Reused input buffer — deliberately NOT a fresh allocation per call.
///
/// The obvious implementation (`Vec::with_capacity` + `mem::forget`) leaks the
/// buffer on every single event: the v3 guest ABI has no `dealloc` export, so
/// nothing ever gives it back, and a wasm linear memory only ever grows. The
/// host calls `herdr_ui_alloc` before every `herdr_ui_on_event`, so the guest
/// ratchets up by one payload per event forever and the host process RSS climbs
/// with it. Measured on hr before this fix: 3.1 MB/min across the still-leaking
/// modules, 5.8 GB of linear memory over a 1d20h uptime.
///
/// Reuse is safe because the host's sequence is strictly alloc-then-deliver on
/// one thread: the payload is written here, `herdr_ui_on_event` parses it, and
/// nothing in the event path calls `herdr_ui_alloc` again before the next event.
/// The response buffer is separate, so the outbound update cannot be clobbered
/// by the next inbound payload.
static HERDR_UI_INPUT: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_alloc(len: i32) -> i32 {
    let wanted = len.max(0) as usize;
    match HERDR_UI_INPUT.lock() {
        Ok(mut buf) => {
            // Resize before taking the pointer: growing may move the allocation.
            buf.clear();
            buf.resize(wanted, 0);
            buf.as_mut_ptr() as i32
        }
        // A poisoned lock must not hand back a dangling pointer; 0 tells the
        // host this guest cannot take the payload.
        Err(_) => 0,
    }
}

static STATIC_UPDATE: &str = r#"{"layers":[]}"#;

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_ptr() -> i32 {
    STATIC_UPDATE.as_ptr() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_len() -> i32 {
    STATIC_UPDATE.len() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_on_event(ptr: i32, len: i32) -> i32 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize) };
    let update = match serde_json::from_slice::<UiEvent>(bytes) {
        Ok(event) => handle_event(&event),
        Err(_) => UiUpdate::default(),
    };
    let encoded = serde_json::to_vec(&update).unwrap_or_default();
    let mut guard = LAST_EVENT_RESPONSE.lock().expect("response mutex poisoned");
    *guard = encoded;
    guard.as_ptr() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_on_event_len() -> i32 {
    LAST_EVENT_RESPONSE
        .lock()
        .expect("response mutex poisoned")
        .len() as i32
}

// ── event handling ─────────────────────────────────────────────────────────

fn handle_event(event: &UiEvent) -> UiUpdate {
    let mut guard = STATE.lock().expect("state mutex poisoned");
    let state = guard.get_or_insert_with(State::default);

    match event {
        UiEvent::Frame(ctx) => {
            let input = ctx
                .host
                .env
                .get("HERDR_PLUGIN_UI_INPUT_JSON")
                .filter(|json| !json.is_empty())
                .and_then(|json| serde_json::from_str::<ImageInput>(json).ok())
                .unwrap_or_default();
            on_frame(state, &input)
        }
        UiEvent::ApiResult {
            request_id,
            ok,
            response,
            ..
        } if request_id == REQ_PANES => {
            state.panes_in_flight = false;
            if *ok {
                if let Some(response) = response.as_ref() {
                    handle_pane_list(state, response);
                }
            }
            status_only(state)
        }
        UiEvent::ApiResult {
            request_id,
            ok,
            response,
            ..
        } if request_id == REQ_INFO => {
            state.info_in_flight = false;
            // Settled either way. A host that refuses the probe (older binary,
            // capability not granted) must cost one call, not one per frame.
            state.info_settled = true;
            if *ok {
                if let Some(response) = response.as_ref() {
                    state.cell_px = parse_cell_px(response);
                }
            }
            status_only(state)
        }
        UiEvent::ApiResult {
            request_id,
            ok,
            response,
            error,
        } if request_id.starts_with(REQ_READ_PREFIX) => {
            state.read_in_flight = false;
            let pane = &request_id[REQ_READ_PREFIX.len()..];
            // A late result for a pane we have left describes the wrong screen.
            if state.pane_id.as_deref() != Some(pane) {
                return status_only(state);
            }
            if *ok {
                state.placeholders = response
                    .as_ref()
                    .map(|value| parse_placeholders(&pane_rows(value)))
                    .unwrap_or_default();
                state.last_error = None;
            } else {
                // A failed read means no placeholders, which is visually
                // identical to "no images on screen". Reporting it is the
                // difference between "off" and "broken" — and a swallowed
                // `pane.read` failure is exactly what made this plugin look
                // like a working feature painting nothing.
                state.placeholders.clear();
                state.last_error = Some(error.clone().unwrap_or_else(|| "pane.read failed".into()));
            }
            status_only(state)
        }
        UiEvent::ApiResult {
            request_id,
            ok,
            error,
            ..
        } if request_id.starts_with(REQ_SET_PREFIX) => {
            if !*ok {
                // Forget the layer this refusal belongs to so a corrected input
                // is retried rather than suppressed by the no-repaint guard, and
                // say so on the strip: a silently-dropped image is exactly the
                // failure mode this lane exists to remove.
                let number = request_id[REQ_SET_PREFIX.len()..].parse::<u32>().ok();
                state.painted.retain(|entry| Some(entry.number) != number);
                state.last_error = Some(error.clone().unwrap_or_else(|| "set refused".into()));
            }
            status_only(state)
        }
        UiEvent::ApiResult { request_id, .. } if request_id.starts_with(REQ_CLEAR_PREFIX) => {
            status_only(state)
        }
        _ => UiUpdate::default(),
    }
}

/// Apply a `pane.list` answer: adopt the focused pane and its session, and drop
/// placeholders that describe a pane we have just left. Split out of the event
/// arm so the focus-change behaviour is testable without the ABI.
fn handle_pane_list(state: &mut State, response: &serde_json::Value) {
    let previous = state.pane_id.clone();
    let (pane_id, session) = match focused_pane(response) {
        Some((pane_id, session)) => (Some(pane_id), session),
        None => (None, None),
    };
    // Focus moved: our placeholders describe a pane we are no longer looking
    // at. Drop them rather than paint stale geometry. The reconcile in
    // `on_frame` then clears the old pane's layers by `(pane_id, layer_id)`.
    if pane_id != previous {
        state.placeholders.clear();
    }
    state.pane_id = pane_id;
    state.pane_session = session;
}

fn on_frame(state: &mut State, input: &ImageInput) -> UiUpdate {
    let mut update = status_only(state);

    // Re-resolve the focused pane EVERY frame we are not already waiting on a
    // listing — not only the first time. Asking once and caching the answer
    // latches the plugin to whichever pane happened to be focused when the
    // module loaded: every later focus change is invisible, the guest keeps
    // reading a pane the user has left, finds no placeholders, and reports
    // `none on screen` with no error. That is indistinguishable from "this
    // feature is broken" and it is invisible in a single-pane sandbox, which is
    // exactly why it shipped. Focus is not initial state; it is state that
    // changes constantly, so it belongs on the same per-frame cadence as the
    // pane read. The `REQ_PANES` arm already drops stale placeholders when the
    // id changes, and the reconcile below clears the old pane's layers by
    // `(pane_id, layer_id)`, so a change costs one clear and one set.
    if !state.panes_in_flight {
        state.panes_in_flight = true;
        update.effects.push(HostEffect::Api {
            request_id: REQ_PANES.to_string(),
            method: "pane.list".to_string(),
            params: serde_json::json!({}),
        });
    }

    let Some(pane_id) = state.pane_id.clone() else {
        return update;
    };

    // Cell geometry is a font fact, so this is asked ONCE, not per frame. It is
    // what turns the box ceiling into a shape: without it there is no way to
    // know how many rows are as tall as N columns are wide.
    if !state.info_settled && !state.info_in_flight {
        state.info_in_flight = true;
        update.effects.push(HostEffect::Api {
            request_id: REQ_INFO.to_string(),
            method: "pane.graphics.info".to_string(),
            params: serde_json::json!({ "pane_id": pane_id }),
        });
    }

    // Re-read the pane text every frame we are not already waiting on one: the
    // placeholder can scroll, and its ROW is what tells the host where to paint.
    // The read is what makes the preview follow the text instead of pinning to
    // wherever it first appeared.
    if !state.read_in_flight {
        state.read_in_flight = true;
        update.effects.push(HostEffect::Api {
            request_id: format!("{REQ_READ_PREFIX}{pane_id}"),
            method: "pane.read".to_string(),
            // `source: "visible"` is load-bearing, not a default worth omitting:
            // row indices in a placement are relative to the pane's VIEWPORT, so
            // anything but the visible grid would place images by scrollback
            // offsets. (`source` is also a required field — omitting it makes the
            // whole call fail, which is how this was found.)
            params: serde_json::json!({
                "pane_id": pane_id,
                "source": "visible",
                "format": "text",
            }),
        });
    }

    // Refuse to paint when the collector's snapshot is for another session. It
    // is better to show nothing for a frame than the previous agent's images.
    let session_matches = state.pane_session.is_none()
        || input.session_id.is_empty()
        || state.pane_session.as_deref() == Some(input.session_id.as_str());

    let desired: Vec<Painted> = if session_matches {
        state
            .placeholders
            .iter()
            .filter_map(|placeholder| {
                let entry = input
                    .images
                    .iter()
                    .find(|entry| entry.number == placeholder.number)?;
                let (grid_cols, grid_rows) = fit_preview(entry.width, entry.height, state.cell_px)?;
                Some(Painted {
                    pane_id: pane_id.clone(),
                    number: placeholder.number,
                    path: entry.path.clone(),
                    col: placeholder.col,
                    row: placeholder.row,
                    grid_cols,
                    grid_rows,
                })
            })
            .take(MAX_LAYERS)
            .collect()
    } else {
        Vec::new()
    };

    if desired == state.painted {
        return update;
    }

    // Clear what is no longer wanted BEFORE setting, so a screen that lost a
    // placeholder does not keep its image for a frame. Per-`layer_id`, never a
    // blanket clear: another plugin may own layers on this pane, and the whole
    // reason layers are keyed by `(owner, layer_id)` is that neither plugin can
    // erase the other's.
    for previous in &state.painted {
        if !desired
            .iter()
            .any(|entry| entry.number == previous.number && entry.pane_id == previous.pane_id)
        {
            update.effects.push(HostEffect::Api {
                request_id: format!("{REQ_CLEAR_PREFIX}{}", previous.number),
                method: "pane.graphics.clear".to_string(),
                params: serde_json::json!({
                    "pane_id": previous.pane_id,
                    "layer_id": layer_id(previous.number),
                }),
            });
        }
    }

    for entry in &desired {
        if state.painted.contains(entry) {
            continue;
        }
        update.effects.push(HostEffect::Api {
            request_id: format!("{REQ_SET_PREFIX}{}", entry.number),
            method: "pane.graphics.set".to_string(),
            params: serde_json::json!({
                "pane_id": entry.pane_id,
                // Stable per placeholder: image 2 moving down the screen updates
                // its own layer instead of stacking a new one, and image 1
                // disappearing does not disturb it.
                "layer_id": layer_id(entry.number),
                "format": "png",
                "image_width": input
                    .images
                    .iter()
                    .find(|image| image.number == entry.number)
                    .map(|image| image.width)
                    .unwrap_or(0),
                "image_height": input
                    .images
                    .iter()
                    .find(|image| image.number == entry.number)
                    .map(|image| image.height)
                    .unwrap_or(0),
                // The host reads the bytes, checked against `graphics_roots`.
                "source_path": entry.path,
                // FIELD NAMES ARE LOAD-BEARING. `PaneGraphicsPlacementParams`
                // is `viewport_col`/`viewport_row`, every field is
                // `#[serde(default)]`, and the struct does not deny unknown
                // fields — so `col`/`row` (what this sent until 2026-07-28) are
                // accepted, discarded, and defaulted to 0. Every preview then
                // painted at the pane's top-left, stacked on top of each other,
                // which is the bug the user reported as "images block text".
                // There is no error to see: the call succeeds.
                "placement": {
                    "viewport_col": entry.col,
                    // One row below the placeholder text, so the preview sits
                    // under its own `[Image #N]` label rather than covering it.
                    "viewport_row": entry.row.saturating_add(1),
                    "grid_cols": entry.grid_cols,
                    "grid_rows": entry.grid_rows,
                },
            }),
        });
    }

    state.painted = desired;
    state.last_error = None;
    update.layers = status_only(state).layers;
    update
}

fn layer_id(number: u32) -> String {
    format!("img-{number}")
}

/// Read `cell_width_px`/`cell_height_px` out of a `pane.graphics.info` answer.
/// Both must be present and non-zero: a half-known cell is not better than the
/// assumption, it is just a differently-wrong aspect.
fn parse_cell_px(response: &serde_json::Value) -> Option<(u32, u32)> {
    let width = response.get("cell_width_px")?.as_u64()? as u32;
    let height = response.get("cell_height_px")?.as_u64()? as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// Fit an image into the preview box while keeping its shape.
///
/// The box is measured in CELLS but the image is measured in PIXELS, and a cell
/// is about twice as tall as it is wide. Ignoring that is what made the old
/// fixed 40x12 wrong for everything: 40x12 cells is roughly 320x192 pixels, a
/// 5:3 landscape frame, so a portrait screenshot was squashed into it.
///
/// Returns `None` for a zero-sized image — a degenerate box is a host error
/// waiting to happen, and there is nothing sensible to draw.
fn fit_preview(
    image_width: u32,
    image_height: u32,
    cell_px: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    if image_width == 0 || image_height == 0 {
        return None;
    }
    let (cell_w, cell_h) = cell_px.unwrap_or(ASSUMED_CELL_PX);
    let (cell_w, cell_h) = (cell_w.max(1), cell_h.max(1));

    // Widest box whose HEIGHT is within the ceiling, then clamp to the width
    // ceiling. Doing it in this order means the binding constraint wins
    // whichever it is, so the result is inside the box on both axes.
    // u64 throughout: the products are pixel counts times cell counts.
    let cols_at_max_rows =
        (u64::from(MAX_PREVIEW_ROWS) * u64::from(image_width) * u64::from(cell_h))
            / (u64::from(image_height) * u64::from(cell_w));
    let grid_cols = MAX_PREVIEW_COLS.min(cols_at_max_rows.max(1) as u32);

    let rows_at_that_width = (u64::from(grid_cols) * u64::from(image_height) * u64::from(cell_w))
        / (u64::from(image_width) * u64::from(cell_h));
    let grid_rows = MAX_PREVIEW_ROWS.min(rows_at_that_width.max(1) as u32);

    Some((grid_cols, grid_rows))
}

/// Our own surface is one status line. It reports the count, whether any visible
/// placeholder has an inexact column, and the last refusal — the three things a
/// user needs to tell "off" from "broken".
fn status_only(state: &State) -> UiUpdate {
    let text = if let Some(error) = &state.last_error {
        format!("images: {error}")
    } else if state.pane_id.is_none() {
        "images: resolving pane".to_string()
    } else if state.painted.is_empty() {
        "images: none on screen".to_string()
    } else {
        let inexact = state
            .placeholders
            .iter()
            .any(|placeholder| !placeholder.col_exact);
        format!(
            "images: {}{}",
            state.painted.len(),
            if inexact { " (~col)" } else { "" }
        )
    };
    UiUpdate {
        layers: vec![UiLayer {
            surface: UiSurface::Panel,
            z_index: 0,
            consumes_mouse: false,
            nodes: vec![herdr_ui_plugin_api::UiNode::Line {
                spans: vec![UiTextSpan {
                    text,
                    ..Default::default()
                }],
            }],
            hits: Vec::new(),
            data: None,
        }],
        ..Default::default()
    }
}

// ── parsing ────────────────────────────────────────────────────────────────

/// Pull the visible rows out of a `pane.read` response. Shape-tolerant: this is
/// a response body the guest does not own, and a schema tweak should degrade to
/// "no placeholders", not to a panic in a module the host runs every frame.
fn pane_rows(response: &serde_json::Value) -> Vec<String> {
    // `pane.read` nests its payload under `read`; accept the bare shape too so a
    // future flattening does not silently blank the previews.
    let response = response.get("read").unwrap_or(response);
    if let Some(rows) = response.get("rows").and_then(serde_json::Value::as_array) {
        return rows
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
    }
    for key in ["text", "content", "screen"] {
        if let Some(text) = response.get(key).and_then(serde_json::Value::as_str) {
            return text.lines().map(str::to_string).collect();
        }
    }
    Vec::new()
}

/// Parse `[Image #N]` occurrences. Same grammar the host used before +127 — a
/// user must not see different placeholders recognised for the same screen.
fn parse_placeholders(rows: &[String]) -> Vec<Placeholder> {
    let mut out = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let chars: Vec<char> = row.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            if let Some((number, consumed)) = match_placeholder(&chars[i..]) {
                out.push(Placeholder {
                    number,
                    col: i as u16,
                    row: row_idx as u16,
                    // Character index == display column only when everything to
                    // the left is single-width ASCII.
                    col_exact: chars[..i].iter().all(char::is_ascii),
                });
                i += consumed;
            } else {
                i += 1;
            }
        }
    }
    out
}

fn match_placeholder(chars: &[char]) -> Option<(u32, usize)> {
    const PREFIX: &[char] = &['[', 'I', 'm', 'a', 'g', 'e', ' ', '#'];
    if chars.len() < PREFIX.len() + 2 {
        return None;
    }
    if &chars[..PREFIX.len()] != PREFIX {
        return None;
    }
    let mut idx = PREFIX.len();
    let mut digits = String::new();
    while idx < chars.len() && chars[idx].is_ascii_digit() {
        digits.push(chars[idx]);
        idx += 1;
    }
    if digits.is_empty() {
        return None;
    }
    if idx >= chars.len() || chars[idx] != ']' {
        return None;
    }
    idx += 1;
    Some((digits.parse::<u32>().ok()?, idx))
}

/// Focused pane id and its agent session, from `pane.list`.
fn focused_pane(response: &serde_json::Value) -> Option<(String, Option<String>)> {
    let panes = response.get("panes")?.as_array()?;
    let focused = panes
        .iter()
        .find(|pane| pane.get("focused").and_then(serde_json::Value::as_bool) == Some(true))
        .or_else(|| panes.first())?;
    let pane_id = focused
        .get("pane_id")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    // `pane.list` returns `agent_session` as an OBJECT
    // (`{agent, kind, source, value}`), not a bare string. Reading it as a
    // string left `pane_session` permanently `None`, which made the
    // cross-session guard below inert: it falls back to "no opinion" and paints
    // whatever the collector last produced. Harmless while focus never moved —
    // and now that focus IS re-resolved every frame, a focus change to another
    // agent's pane is precisely the race the guard exists to stop. Both shapes
    // are accepted so a host that ever flattens the field keeps working.
    let session = focused.get("agent_session").and_then(|value| {
        value
            .as_str()
            .or_else(|| value.get("value").and_then(serde_json::Value::as_str))
            .map(str::to_string)
    });
    Some((pane_id, session))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    fn entry(number: u32, path: &str) -> ImageEntry {
        ImageEntry {
            number,
            path: path.to_string(),
            width: 40,
            height: 20,
        }
    }

    fn ready_state() -> State {
        State {
            pane_id: Some("w_1_p_1".into()),
            pane_session: Some("sess".into()),
            // Suppress the two per-frame query effects so the paint tests below
            // count paints only. `a_resolved_pane_still_re_resolves_focus_every_frame`
            // is the test that asserts those queries are actually issued.
            read_in_flight: true,
            panes_in_flight: true,
            // Same reason: the cell probe is a query, not a paint. Tests that
            // care about it set this back to false.
            info_settled: true,
            ..State::default()
        }
    }

    fn calls(update: &UiUpdate) -> Vec<(String, String)> {
        update
            .effects
            .iter()
            .map(|effect| match effect {
                HostEffect::Api {
                    request_id, method, ..
                } => (request_id.clone(), method.clone()),
                _ => (String::new(), String::new()),
            })
            .collect()
    }

    fn params_of(update: &UiUpdate, request_id: &str) -> serde_json::Value {
        update
            .effects
            .iter()
            .find_map(|effect| match effect {
                HostEffect::Api {
                    request_id: id,
                    params,
                    ..
                } if id == request_id => Some(params.clone()),
                _ => None,
            })
            .expect("request not issued")
    }

    #[test]
    fn placeholders_are_parsed_with_row_and_column() {
        let found = parse_placeholders(&rows(&["hello", "  [Image #3] here", "[Image #12]"]));
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].number, found[0].col, found[0].row), (3, 2, 1));
        assert_eq!((found[1].number, found[1].col, found[1].row), (12, 0, 2));
        assert!(found[0].col_exact && found[1].col_exact);
    }

    /// The documented limitation, pinned as a test so it cannot be "fixed"
    /// silently or regress into a claim of exactness it does not have.
    #[test]
    fn a_wide_character_to_the_left_marks_the_column_inexact() {
        let found = parse_placeholders(&rows(&["日本語 [Image #1]"]));
        assert_eq!(found.len(), 1);
        assert!(
            !found[0].col_exact,
            "char index cannot equal display column after wide characters"
        );
    }

    #[test]
    fn malformed_placeholders_are_ignored() {
        assert!(parse_placeholders(&rows(&["[Image #]", "[Image 3]", "[image #3]"])).is_empty());
    }

    /// Each placeholder gets its own stable `layer_id`, which is what lets image
    /// 2 move without disturbing image 1.
    #[test]
    fn each_placeholder_gets_its_own_stable_layer_id() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["[Image #1]", "[Image #2]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/c/1.png"), entry(2, "/c/2.png")],
        };
        let update = on_frame(&mut state, &input);
        assert_eq!(
            params_of(&update, "claude-image:set:1")["layer_id"],
            "img-1"
        );
        assert_eq!(
            params_of(&update, "claude-image:set:2")["layer_id"],
            "img-2"
        );
    }

    /// The no-repaint guard: `pane.graphics.set` is a state write, and reissuing
    /// an identical layer every frame is the easiest way to make this plugin
    /// look expensive in the exact CPU measurement it has to pass.
    #[test]
    fn an_unchanged_screen_paints_once_not_once_per_frame() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["[Image #1]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/c/1.png")],
        };
        assert_eq!(calls(&on_frame(&mut state, &input)).len(), 1);
        for _ in 0..10 {
            assert!(calls(&on_frame(&mut state, &input)).is_empty());
        }
    }

    /// A placeholder that scrolls away must take its image with it, and by
    /// `layer_id` — a blanket clear would erase another plugin's layers on the
    /// same pane.
    #[test]
    fn a_vanished_placeholder_clears_only_its_own_layer() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["[Image #1]", "[Image #2]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/c/1.png"), entry(2, "/c/2.png")],
        };
        let _ = on_frame(&mut state, &input);

        // Image 2 stays on the same row: a repaint of 2 here would be correct
        // (its geometry changed), and would hide whether the clear is targeted.
        state.placeholders = parse_placeholders(&rows(&["", "[Image #2]"]));
        let update = on_frame(&mut state, &input);
        let issued = calls(&update);
        assert_eq!(
            issued,
            vec![(
                "claude-image:clear:1".to_string(),
                "pane.graphics.clear".to_string()
            )],
            "only the vanished layer is cleared, and image 2 is not repainted"
        );
        assert_eq!(
            params_of(&update, "claude-image:clear:1")["layer_id"],
            "img-1"
        );
    }

    /// A placeholder with no file on disk paints nothing rather than asking the
    /// host for a path that does not exist.
    #[test]
    fn a_placeholder_with_no_cached_file_paints_nothing() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["[Image #7]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/c/1.png")],
        };
        assert!(calls(&on_frame(&mut state, &input)).is_empty());
    }

    /// The race this guards: the collector describes session A while focus has
    /// already moved to session B. Painting A's images into B's pane would be
    /// wrong in a way the user cannot diagnose.
    #[test]
    fn a_session_mismatch_paints_nothing() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["[Image #1]"]));
        let input = ImageInput {
            session_id: "other-session".into(),
            images: vec![entry(1, "/c/1.png")],
        };
        assert!(calls(&on_frame(&mut state, &input)).is_empty());
    }

    #[test]
    fn the_paint_call_sends_a_path_and_never_image_bytes() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["  [Image #1]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/c/1.png")],
        };
        let update = on_frame(&mut state, &input);
        let params = params_of(&update, "claude-image:set:1");
        assert_eq!(params["source_path"], "/c/1.png");
        assert!(params.get("data").is_none());
        assert!(params.get("data_base64").is_none());
        assert_eq!(params["placement"]["viewport_col"], 2);
        // One row below its own label.
        assert_eq!(params["placement"]["viewport_row"], 1);
    }

    /// The regression this pins is the reason every preview used to land at the
    /// pane's top-left. `PaneGraphicsPlacementParams` names its fields
    /// `viewport_col`/`viewport_row`, defaults every one of them, and does not
    /// deny unknown fields — so `col`/`row` were accepted, dropped, and read as
    /// 0, with a successful response and nothing in any log.
    ///
    /// Asserting the ABSENCE of the old names is the point: the previous version
    /// of this suite asserted `placement["col"] == 2` and passed, which is how a
    /// green test suite shipped a feature that never worked. A test that only
    /// checks the value it wrote cannot see a name the host ignores.
    #[test]
    fn the_placement_uses_the_hosts_field_names_and_not_the_ignored_ones() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["  [Image #1]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/c/1.png")],
        };
        let update = on_frame(&mut state, &input);
        let placement = params_of(&update, "claude-image:set:1")["placement"].clone();

        assert!(
            placement.get("col").is_none() && placement.get("row").is_none(),
            "the host silently ignores `col`/`row`; sending them is the bug: {placement}"
        );
        assert!(placement.get("viewport_col").is_some());
        assert!(placement.get("viewport_row").is_some());
    }

    /// A structural guard, not another value assertion.
    ///
    /// The host's `PaneGraphicsPlacementParams` lives in the herdr BINARY crate
    /// (`src/api/schema/panes.rs`). The contract this guest vendors,
    /// `herdr-ui-plugin-api`, contains no placement type at all — so there is no
    /// shared type to round-trip through, and a misspelled field cannot fail at
    /// compile time. THAT is the structural reason this bug was possible, and
    /// the reason it survived: both sides agree only on a JSON object shape that
    /// neither side checks.
    ///
    /// The nearest available guard is a local mirror with the one attribute the
    /// host struct is missing. Deserializing the ACTUAL emitted payload into it
    /// fails on any field the host would silently drop, so a future rename
    /// breaks this test instead of the feature. The real fix is to move the
    /// placement type into the shared crate, or add `deny_unknown_fields` to the
    /// host struct; until then this mirror must be updated in lockstep, which is
    /// exactly why it names its source file above.
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct HostPlacementMirror {
        #[serde(default)]
        viewport_col: i32,
        #[serde(default)]
        viewport_row: i32,
        #[serde(default)]
        grid_cols: u32,
        #[serde(default)]
        grid_rows: u32,
    }

    #[test]
    fn the_emitted_placement_deserializes_into_the_hosts_shape() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["  [Image #1]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/c/1.png")],
        };
        let update = on_frame(&mut state, &input);
        let placement = params_of(&update, "claude-image:set:1")["placement"].clone();

        let parsed: HostPlacementMirror =
            serde_json::from_value(placement.clone()).unwrap_or_else(|error| {
                panic!("host would reject/ignore part of {placement}: {error}")
            });
        assert_eq!(parsed.viewport_col, 2);
        assert_eq!(parsed.viewport_row, 1);
        assert!(parsed.grid_cols > 0 && parsed.grid_rows > 0);

        // And prove the guard actually guards: the payload this plugin used to
        // send is rejected by the mirror even though the real host accepts it.
        assert!(
            serde_json::from_value::<HostPlacementMirror>(
                serde_json::json!({"col": 2, "row": 1, "grid_cols": 40, "grid_rows": 12})
            )
            .is_err(),
            "the mirror must reject the field names the host silently ignores"
        );
    }

    /// A non-zero column is what proves the placement is per-placeholder rather
    /// than defaulted. With the old names every image resolved to 0,0 and they
    /// stacked on each other at the corner.
    #[test]
    fn two_placeholders_get_two_distinct_positions() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["  [Image #1]", "        [Image #2]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/c/1.png"), entry(2, "/c/2.png")],
        };
        let update = on_frame(&mut state, &input);
        let first = params_of(&update, "claude-image:set:1")["placement"].clone();
        let second = params_of(&update, "claude-image:set:2")["placement"].clone();
        assert_eq!((&first["viewport_col"], &first["viewport_row"]).0, 2);
        assert_ne!(first["viewport_col"], second["viewport_col"]);
        assert_ne!(first["viewport_row"], second["viewport_row"]);
    }

    /// The other half of the user's complaint: every preview was the same
    /// 40x12 regardless of the image, so nothing kept its shape.
    #[test]
    fn the_box_keeps_the_images_shape_through_the_cell_aspect() {
        // 2:1 landscape. A cell is 8x16, so 40 cols is 320px wide and the box
        // that matches is 10 rows (160px) — not the old flat 12.
        assert_eq!(fit_preview(400, 200, Some((8, 16))), Some((40, 10)));

        // Portrait: height binds first, so the box narrows instead of
        // stretching the image across the full 40 columns.
        let (cols, rows) = fit_preview(200, 400, Some((8, 16))).expect("fits");
        assert_eq!(rows, MAX_PREVIEW_ROWS);
        assert!(
            cols < MAX_PREVIEW_COLS,
            "a tall image must not fill the width"
        );
        assert_eq!((cols, rows), (12, 12));
    }

    #[test]
    fn every_fitted_box_stays_inside_the_ceiling() {
        for (width, height) in [(1, 4000), (4000, 1), (7, 11), (1920, 1080), (1, 1)] {
            let (cols, rows) = fit_preview(width, height, Some((8, 16)))
                .unwrap_or_else(|| panic!("{width}x{height} must fit"));
            assert!(cols >= 1 && rows >= 1, "{width}x{height} -> {cols}x{rows}");
            assert!(
                cols <= MAX_PREVIEW_COLS && rows <= MAX_PREVIEW_ROWS,
                "{width}x{height} -> {cols}x{rows} escapes the box"
            );
        }
    }

    /// A zero-sized image has no shape to preserve and a zero-sized box means
    /// "the whole pane" to the host — precisely the overpaint being fixed.
    #[test]
    fn a_zero_sized_image_is_not_painted() {
        assert_eq!(fit_preview(0, 100, Some((8, 16))), None);
        assert_eq!(fit_preview(100, 0, Some((8, 16))), None);
    }

    /// The probe is a one-shot, and a host that refuses it must not be asked
    /// again every frame.
    #[test]
    fn the_cell_probe_is_asked_once_and_settles_even_when_refused() {
        let mut state = ready_state();
        state.info_settled = false;
        let input = ImageInput::default();

        let update = on_frame(&mut state, &input);
        assert!(api_methods(&update).contains(&"pane.graphics.info".to_string()));
        // In flight: no duplicate on the next frame.
        let update = on_frame(&mut state, &input);
        assert!(!api_methods(&update).contains(&"pane.graphics.info".to_string()));

        handle_event(&UiEvent::ApiResult {
            request_id: REQ_INFO.to_string(),
            ok: false,
            response: None,
            error: Some("unsupported".into()),
        });

        // A refusal settles it too, so the guest falls back to the assumed cell
        // aspect rather than re-asking forever.
        let mut state = ready_state();
        state.info_settled = true;
        state.placeholders = parse_placeholders(&rows(&["[Image #1]"]));
        let update = on_frame(
            &mut state,
            &ImageInput {
                session_id: "sess".into(),
                images: vec![entry(1, "/c/1.png")],
            },
        );
        assert!(!api_methods(&update).contains(&"pane.graphics.info".to_string()));
        let placement = params_of(&update, "claude-image:set:1")["placement"].clone();
        assert_eq!(placement["grid_cols"], 40);
        assert_eq!(placement["grid_rows"], 10);
    }

    #[test]
    fn a_cell_size_answer_is_adopted_and_changes_the_fit() {
        assert_eq!(
            parse_cell_px(&serde_json::json!({"cell_width_px": 10, "cell_height_px": 21})),
            Some((10, 21))
        );
        // A degenerate answer is worse than the assumption, not better.
        assert_eq!(
            parse_cell_px(&serde_json::json!({"cell_width_px": 0, "cell_height_px": 21})),
            None
        );
        // The cell aspect changes the box, which is the whole reason to ask.
        // Square cells are SHORTER than 8x16 ones, so the same 2:1 image needs
        // more rows — and hits the row ceiling first, narrowing the width.
        let square = fit_preview(400, 200, Some((10, 10))).expect("fits");
        let tall = fit_preview(400, 200, Some((8, 16))).expect("fits");
        assert_eq!((square, tall), ((24, 12), (40, 10)));
    }

    /// The no-repaint guard keys on the BOX as well as the position. Learning
    /// the real cell size after the first paint must re-send, or the preview
    /// keeps the box it was fitted with under the assumption.
    #[test]
    fn a_changed_fit_repaints_even_though_the_image_did_not_change() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["[Image #1]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/c/1.png")],
        };
        assert!(!calls(&on_frame(&mut state, &input)).is_empty());
        // Unchanged screen: nothing re-sent.
        assert!(calls(&on_frame(&mut state, &input)).is_empty());

        state.cell_px = Some((10, 10));
        let update = on_frame(&mut state, &input);
        let placement = params_of(&update, "claude-image:set:1")["placement"].clone();
        // Square cells refit the same 2:1 image from 40x10 to 24x12.
        assert_eq!(placement["grid_cols"], 24, "square cells, 2:1 image");
        assert_eq!(placement["grid_rows"], 12);
    }

    /// Never more than the manifest's declared ceiling: the guest must not be
    /// the thing that trips the host's limit and generates error traffic.
    #[test]
    fn the_declared_layer_ceiling_is_respected() {
        let mut state = ready_state();
        let lines: Vec<String> = (1..=20).map(|n| format!("[Image #{n}]")).collect();
        state.placeholders = parse_placeholders(&lines);
        let input = ImageInput {
            session_id: "sess".into(),
            images: (1..=20).map(|n| entry(n, "/c/x.png")).collect(),
        };
        assert_eq!(calls(&on_frame(&mut state, &input)).len(), MAX_LAYERS);
    }

    /// A refused paint must not be latched: a corrected input has to be retried,
    /// and the no-repaint guard would otherwise swallow it forever.
    #[test]
    fn a_refused_paint_is_retried_rather_than_suppressed() {
        let mut state = ready_state();
        state.placeholders = parse_placeholders(&rows(&["[Image #1]"]));
        let input = ImageInput {
            session_id: "sess".into(),
            images: vec![entry(1, "/outside/1.png")],
        };
        let _ = on_frame(&mut state, &input);
        assert_eq!(state.painted.len(), 1);

        let update = handle_event(&UiEvent::ApiResult {
            request_id: "claude-image:set:1".into(),
            ok: false,
            response: None,
            error: Some("source_not_allowed".into()),
        });
        // The refusal is reported, not swallowed.
        assert!(format!("{:?}", update.layers).contains("source_not_allowed"));

        let mut state = STATE.lock().expect("state mutex poisoned");
        let state = state.get_or_insert_with(State::default);
        assert!(state.painted.is_empty());
    }

    #[test]
    fn focused_pane_and_its_session_are_read_from_pane_list() {
        let response = serde_json::json!({
            "panes": [
                {"pane_id": "w_1_p_1", "focused": false, "agent_session": "a"},
                {"pane_id": "w_1_p_2", "focused": true, "agent_session": "b"},
            ]
        });
        assert_eq!(
            focused_pane(&response),
            Some(("w_1_p_2".to_string(), Some("b".to_string())))
        );
    }

    /// `pane.list` really answers with `agent_session` as an object; the bare
    /// string is accepted too so a flattened host field keeps working.
    #[test]
    fn agent_session_object_shape_is_read_not_ignored() {
        let response = serde_json::json!({
            "panes": [{
                "pane_id": "w2_p5",
                "focused": true,
                "agent_session": {
                    "agent": "claude",
                    "kind": "id",
                    "source": "herdr:claude",
                    "value": "f17a7ab8"
                }
            }]
        });
        assert_eq!(
            focused_pane(&response),
            Some(("w2_p5".to_string(), Some("f17a7ab8".to_string())))
        );
    }

    fn api_methods(update: &UiUpdate) -> Vec<String> {
        update
            .effects
            .iter()
            .filter_map(|effect| match effect {
                HostEffect::Api { method, .. } => Some(method.clone()),
                _ => None,
            })
            .collect()
    }

    /// THE regression test for the focus latch. Asking `pane.list` only while
    /// `pane_id` is `None` pinned the plugin to the pane that was focused when
    /// the module loaded: every later focus change was invisible and the guest
    /// read a pane the user had left. A single-pane sandbox cannot see this, so
    /// it has to be a test — a resolved pane must NOT stop the re-resolution.
    #[test]
    fn a_resolved_pane_still_re_resolves_focus_every_frame() {
        let mut state = State {
            pane_id: Some("w2_p5".to_string()),
            pane_session: Some("sess".to_string()),
            ..State::default()
        };
        let input = ImageInput::default();

        let update = on_frame(&mut state, &input);
        assert!(
            api_methods(&update).contains(&"pane.list".to_string()),
            "a frame with a known pane must still ask which pane is focused"
        );
        // …and it still reads the pane it knows about in the same frame, so
        // re-resolving costs no latency on the common no-focus-change path.
        assert!(api_methods(&update).contains(&"pane.read".to_string()));

        // One listing at a time: the second frame must not stack a duplicate.
        let update = on_frame(&mut state, &input);
        assert!(!api_methods(&update).contains(&"pane.list".to_string()));

        // The answer arrives naming a DIFFERENT pane: stale placeholders go.
        state.placeholders = vec![Placeholder {
            number: 1,
            col: 0,
            row: 0,
            col_exact: true,
        }];
        handle_pane_list(
            &mut state,
            &serde_json::json!({
                "panes": [{"pane_id": "w1_pK", "focused": true,
                           "agent_session": {"value": "other"}}]
            }),
        );
        assert_eq!(state.pane_id.as_deref(), Some("w1_pK"));
        assert_eq!(state.pane_session.as_deref(), Some("other"));
        assert!(
            state.placeholders.is_empty(),
            "placeholders describe the pane we left"
        );
    }

    #[test]
    fn an_unexpected_response_shape_degrades_rather_than_panicking() {
        assert_eq!(focused_pane(&serde_json::json!({})), None);
        assert_eq!(focused_pane(&serde_json::json!({"panes": "nope"})), None);
        assert!(pane_rows(&serde_json::json!({})).is_empty());
        assert_eq!(
            pane_rows(&serde_json::json!({"text": "a\nb"})),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    /// `pane.read` nests its payload under `read` and requires a `source`. Both
    /// were got wrong first time and both fail the same silent way: no
    /// placeholders, which looks exactly like "no images on screen".
    #[test]
    fn pane_read_payload_is_unwrapped_from_its_envelope() {
        let response = serde_json::json!({
            "read": {"pane_id": "w1:p1", "format": "text", "text": "x\n[Image #4]"}
        });
        let found = parse_placeholders(&pane_rows(&response));
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].number, found[0].row), (4, 1));
    }

    #[test]
    fn the_read_asks_for_the_visible_viewport() {
        let mut state = State {
            pane_id: Some("w1:p1".into()),
            ..State::default()
        };
        let update = on_frame(&mut state, &ImageInput::default());
        let params = params_of(&update, "claude-image:read:w1:p1");
        // Row indices in a placement are viewport-relative; any other source
        // would place images by scrollback offsets.
        assert_eq!(params["source"], "visible");
    }

    /// A refused read must be reported, not swallowed. Silence here is
    /// indistinguishable from "no images", which is what hid the bug.
    #[test]
    fn a_failed_read_is_reported_rather_than_looking_like_no_images() {
        {
            let mut guard = STATE.lock().expect("state mutex poisoned");
            *guard = Some(State {
                pane_id: Some("w1:p1".into()),
                read_in_flight: true,
                ..State::default()
            });
        }
        let update = handle_event(&UiEvent::ApiResult {
            request_id: "claude-image:read:w1:p1".into(),
            ok: false,
            response: None,
            error: Some("pane_not_found".into()),
        });
        assert!(format!("{:?}", update.layers).contains("pane_not_found"));
    }
}
