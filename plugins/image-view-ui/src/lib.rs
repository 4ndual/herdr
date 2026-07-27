//! Image-view guest: paints an image into a pane WITHOUT any host image code.
//!
//! This is the plugin half of "an image renderer you can switch off at runtime".
//! Everything the host gained for it is generic bridge surface — the
//! `pane_graphics` capability, three allow rows, and `source_path` — none of it
//! knows what an image cache is. This module does.
//!
//! The shape is forced by one constraint: **a wasm32 guest here has no WASI and
//! no host imports, so it cannot read a file.** Image bytes therefore never
//! enter, leave, or pass through this module. The division is:
//!
//!   * the out-of-process collector picks a file and measures it, writing a path
//!     plus pixel dimensions into `image_input.json` (the generic §7.1
//!     `ui_input_file` transport — no host code);
//!   * this guest decides WHICH image goes into WHICH pane and says so;
//!   * the host reads the bytes, and only from a directory this plugin's own
//!     manifest declared in `graphics_roots`.
//!
//! Turning it off is `plugin disable`: the capability goes, the host drops this
//! owner's layers, and the image leaves the pane. No rebuild.

use herdr_ui_plugin_api::{HostEffect, UiEvent, UiLayer, UiSurface, UiTextSpan, UiUpdate};
use serde::Deserialize;
use std::sync::Mutex;

/// Request id for our one read call. The host echoes it verbatim on the
/// matching `UiEvent::ApiResult`, which is how we tell it from anything else.
const REQ_PANES: &str = "image-view:panes";
/// Request id for the paint call. We do not act on its result beyond noting
/// failure, but it has to be distinguishable from the pane list.
const REQ_SET: &str = "image-view:set";

/// What the collector writes. Deliberately just a path and its dimensions: the
/// guest cannot open the file to learn either, and the host will not guess.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ImageInput {
    /// Absolute path. Must canonicalise under a declared `graphics_roots` entry
    /// or the host refuses it with `source_not_allowed` — that refusal is the
    /// point of the field, not a failure mode to work around.
    path: String,
    width: u32,
    height: u32,
}

#[derive(Default)]
struct State {
    /// Focused pane, learned from `pane.list`. `None` until the first result.
    pane_id: Option<String>,
    /// True between issuing `pane.list` and its result, so a 60-per-second frame
    /// stream cannot turn one unanswered question into a request storm.
    panes_in_flight: bool,
    /// The (pane, image) pair currently painted. A repaint is issued only when
    /// this differs from what the input and focus now say, because
    /// `pane.graphics.set` is a state write and re-sending an identical layer
    /// every frame would be the single easiest way to make this plugin look
    /// expensive in exactly the CPU measurement it has to pass.
    painted: Option<(String, ImageInput)>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

// ── ABI version ────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_api_version() -> i32 {
    3
}

// ── Allocator (host writes event JSON here) ────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_alloc(len: i32) -> i32 {
    let mut bytes = Vec::<u8>::with_capacity(len.max(0) as usize);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr as i32
}

// ── Static update (nothing to declare statically) ──────────────────────────

static STATIC_UPDATE: &str = r#"{"layers":[]}"#;

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_ptr() -> i32 {
    STATIC_UPDATE.as_ptr() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_len() -> i32 {
    STATIC_UPDATE.len() as i32
}

// ── Event handler ──────────────────────────────────────────────────────────

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

// ── Internal ───────────────────────────────────────────────────────────────

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
                .and_then(|json| serde_json::from_str::<ImageInput>(json).ok());
            on_frame(state, input)
        }
        UiEvent::ApiResult {
            request_id,
            ok,
            response,
            ..
        } if request_id == REQ_PANES => {
            state.panes_in_flight = false;
            if *ok {
                state.pane_id = response.as_ref().and_then(focused_pane_id);
            }
            // Nothing to paint yet: the next frame carries the input and will
            // issue the set now that a pane is known.
            status_only(state)
        }
        UiEvent::ApiResult { request_id, ok, .. } if request_id == REQ_SET => {
            if !*ok {
                // The paint was refused — most likely `source_not_allowed` for a
                // path outside graphics_roots. Forget what we thought we painted
                // so a corrected input is retried rather than suppressed by the
                // no-repaint guard.
                state.painted = None;
            }
            status_only(state)
        }
        _ => UiUpdate::default(),
    }
}

fn on_frame(state: &mut State, input: Option<ImageInput>) -> UiUpdate {
    let Some(input) = input else {
        return status_only(state);
    };

    let Some(pane_id) = state.pane_id.clone() else {
        // Ask once, not once per frame.
        if state.panes_in_flight {
            return status_only(state);
        }
        state.panes_in_flight = true;
        let mut update = status_only(state);
        update.effects.push(HostEffect::Api {
            request_id: REQ_PANES.to_string(),
            method: "pane.list".to_string(),
            params: serde_json::json!({}),
        });
        return update;
    };

    if state.painted.as_ref() == Some(&(pane_id.clone(), input.clone())) {
        return status_only(state);
    }

    let effect = HostEffect::Api {
        request_id: REQ_SET.to_string(),
        method: "pane.graphics.set".to_string(),
        params: serde_json::json!({
            "pane_id": pane_id,
            "format": "png",
            "image_width": input.width,
            "image_height": input.height,
            // The host reads these bytes. `data_base64` stays empty: sending the
            // image through the guest is the thing this field exists to avoid,
            // and the guest could not produce it anyway.
            "source_path": input.path,
        }),
    };
    state.painted = Some((pane_id, input));

    let mut update = status_only(state);
    update.effects.push(effect);
    update
}

/// Our own surface is one line of status text, not the image. Keeping it honest
/// about what is painted is what makes the feature legible when it is off.
fn status_only(state: &State) -> UiUpdate {
    let text = match (&state.pane_id, &state.painted) {
        (_, Some((_pane, image))) => format!(
            "image {}x{} → {}",
            image.width,
            image.height,
            short_name(&image.path)
        ),
        (Some(_), None) => "image: waiting for input".to_string(),
        (None, None) => "image: resolving pane".to_string(),
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

fn short_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Pull the focused pane id out of a `pane.list` response. Shape-tolerant on
/// purpose: this is the one place the guest depends on a response body it does
/// not own, and a schema tweak upstream should degrade to "no pane yet" rather
/// than to a panic in a module the host runs every frame.
fn focused_pane_id(response: &serde_json::Value) -> Option<String> {
    let panes = response.get("panes")?.as_array()?;
    let focused = panes
        .iter()
        .find(|pane| pane.get("focused").and_then(serde_json::Value::as_bool) == Some(true))
        .or_else(|| panes.first())?;
    focused
        .get("pane_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str) -> ImageInput {
        ImageInput {
            path: path.to_string(),
            width: 4,
            height: 2,
        }
    }

    fn api_calls(update: &UiUpdate) -> Vec<(&str, &str)> {
        update
            .effects
            .iter()
            .filter_map(|effect| match effect {
                HostEffect::Api {
                    request_id, method, ..
                } => Some((request_id.as_str(), method.as_str())),
                _ => None,
            })
            .collect()
    }

    /// A frame stream is continuous; an unanswered question must not become a
    /// request storm. Without the in-flight guard this issues one `pane.list`
    /// per frame forever, which is exactly the shape that would show up as this
    /// plugin's CPU cost.
    #[test]
    fn pane_list_is_asked_once_while_the_answer_is_outstanding() {
        let mut state = State::default();
        let first = on_frame(&mut state, Some(input("/img/a.png")));
        assert_eq!(api_calls(&first), vec![(REQ_PANES, "pane.list")]);
        for _ in 0..10 {
            let next = on_frame(&mut state, Some(input("/img/a.png")));
            assert!(api_calls(&next).is_empty(), "asked again while in flight");
        }
    }

    /// The no-repaint guard. `pane.graphics.set` is a state write; re-sending an
    /// identical layer every frame would make an idle plugin look expensive.
    #[test]
    fn an_unchanged_image_is_painted_once_not_once_per_frame() {
        let mut state = State {
            pane_id: Some("w_1_p_1".into()),
            ..State::default()
        };
        let first = on_frame(&mut state, Some(input("/img/a.png")));
        assert_eq!(api_calls(&first), vec![(REQ_SET, "pane.graphics.set")]);
        for _ in 0..10 {
            let next = on_frame(&mut state, Some(input("/img/a.png")));
            assert!(api_calls(&next).is_empty(), "repainted an unchanged image");
        }
    }

    #[test]
    fn a_changed_image_repaints() {
        let mut state = State {
            pane_id: Some("w_1_p_1".into()),
            ..State::default()
        };
        let _ = on_frame(&mut state, Some(input("/img/a.png")));
        let next = on_frame(&mut state, Some(input("/img/b.png")));
        assert_eq!(api_calls(&next), vec![(REQ_SET, "pane.graphics.set")]);
    }

    /// A refused paint must not be latched. If the host says no — a path outside
    /// `graphics_roots` gives `source_not_allowed` — a corrected input has to be
    /// retried, and the no-repaint guard would otherwise swallow it.
    #[test]
    fn a_refused_paint_is_retried_rather_than_suppressed() {
        let mut state = State {
            pane_id: Some("w_1_p_1".into()),
            ..State::default()
        };
        let _ = on_frame(&mut state, Some(input("/outside/a.png")));
        assert!(state.painted.is_some());

        // Host refuses.
        state.painted = None;

        let retry = on_frame(&mut state, Some(input("/outside/a.png")));
        assert_eq!(api_calls(&retry), vec![(REQ_SET, "pane.graphics.set")]);
    }

    /// The guest never sees bytes and must never send any: `source_path` carries
    /// the reference, `data_base64` stays absent.
    #[test]
    fn the_paint_call_sends_a_path_and_never_image_bytes() {
        let mut state = State {
            pane_id: Some("w_1_p_1".into()),
            ..State::default()
        };
        let update = on_frame(&mut state, Some(input("/img/a.png")));
        let HostEffect::Api { params, .. } = &update.effects[0] else {
            panic!("expected an api effect");
        };
        assert_eq!(params["source_path"], "/img/a.png");
        assert!(params.get("data").is_none());
        assert!(params.get("data_base64").is_none());
    }

    #[test]
    fn focused_pane_is_preferred_over_the_first_listed() {
        let response = serde_json::json!({
            "panes": [
                {"pane_id": "w_1_p_1", "focused": false},
                {"pane_id": "w_1_p_2", "focused": true},
            ]
        });
        assert_eq!(focused_pane_id(&response).as_deref(), Some("w_1_p_2"));
    }

    /// Shape tolerance: an unrecognised body yields "no pane yet", not a panic
    /// in a module the host runs every frame.
    #[test]
    fn an_unexpected_response_shape_yields_no_pane_rather_than_panicking() {
        assert_eq!(focused_pane_id(&serde_json::json!({})), None);
        assert_eq!(focused_pane_id(&serde_json::json!({"panes": "nope"})), None);
        assert_eq!(focused_pane_id(&serde_json::json!({"panes": []})), None);
    }
}
