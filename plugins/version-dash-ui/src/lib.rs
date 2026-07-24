//! Version-dash guest: a compact "running herdr version vs latest stable"
//! indicator rendered in a generic plugin `Panel` docked on the LEFT of the
//! bottom dash band (position is pure manifest DATA — see `[ui_modules.placement]`).
//!
//! Data path (spec §7.1, the generic `ui_input_file` transport — NO host code):
//! the out-of-process `version-dash-collector` fetches the release channel
//! manifest + the running fork's version on a 10-minute cadence and writes
//! `version_input.json` into this module's state dir. The host reads that file
//! each frame and injects it as `HERDR_PLUGIN_UI_INPUT_JSON`, which the guest
//! deserializes and renders. Clicking the strip toasts the latest release date
//! and notes URL.

mod render;

use herdr_ui_plugin_api::{
    HostEffect, UiActionKind, UiEvent, UiLayer, UiSurface, UiUpdate, UiUpdateScope,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::Mutex;
use version_dash_model::VersionRenderInput;

/// Cache of the last input we rendered, so an `Action` click can build its toast
/// from the same data the strip is showing without re-reading any file.
static LAST_INPUT: Mutex<Option<VersionRenderInput>> = Mutex::new(None);

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
    let ctx = match event {
        UiEvent::Frame(ctx) => ctx,
        UiEvent::Action {
            action_id, kind, ..
        } => return handle_action(action_id, *kind),
        _ => return UiUpdate::default(),
    };

    // Generic §7.1 transport: the host injects the plugin's ui_input_file
    // (version_input.json) contents here. No key ⇒ nothing to render (loss-free
    // under partial-default merge, same as usage-dash).
    let json = match ctx.host.env.get("HERDR_PLUGIN_UI_INPUT_JSON") {
        Some(j) if !j.is_empty() => j,
        _ => return UiUpdate::default(),
    };

    let input: VersionRenderInput = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return UiUpdate::default(),
    };

    *LAST_INPUT.lock().expect("input mutex poisoned") = Some(input.clone());

    // Render into a CONTENT-sized buffer, independent of the incoming panel rect,
    // so the host can measure the natural width and carve the slot even on the
    // first content frame (before which `ctx.rect` is still zero). The hit rect
    // comes from the real (carved) panel rect so clicks map to screen cells.
    let content = Rect::new(0, 0, render::content_width(&input).max(1), 1);
    let mut buf = Buffer::empty(content);
    render::render(&input, content, &mut buf);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);
    let panel = Rect::new(ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height);
    let hits = render::hit_rect(panel);

    UiUpdate {
        layers: vec![UiLayer {
            surface: UiSurface::Panel,
            z_index: 0,
            consumes_mouse: false,
            nodes,
            hits,
            data: None,
        }],
        ..Default::default()
    }
}

fn handle_action(action_id: &str, kind: UiActionKind) -> UiUpdate {
    // Fire once per physical click (press only), and only for our own action.
    if kind != UiActionKind::LeftDown || action_id != render::ACTION_OPEN {
        return UiUpdate::default();
    }
    let message = LAST_INPUT
        .lock()
        .expect("input mutex poisoned")
        .as_ref()
        .map(toast_message)
        .unwrap_or_else(|| "version: no data yet".to_string());

    UiUpdate {
        layers: Vec::new(),
        effects: vec![HostEffect::ShowToast { message }],
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    }
}

/// Build the click toast from the last rendered input: state line + latest
/// release date + notes URL, plus any collector error for offline visibility.
fn toast_message(input: &VersionRenderInput) -> String {
    let current = input.current.as_deref().unwrap_or("unknown");
    let latest = input.latest.as_deref().unwrap_or("unknown");
    // The host renders toasts on a single line, so join parts with " · "
    // separators rather than newlines (which collapse into a run-on string).
    let mut parts = vec![if input.update_available() {
        format!("herdr {current} — update {latest} available")
    } else {
        format!("herdr {current} — up to date (latest {latest})")
    }];
    if let Some(date) = input.latest_date.as_deref() {
        parts.push(format!("released {date}"));
    }
    if let Some(url) = input.notes_url.as_deref() {
        parts.push(url.to_string());
    }
    if !input.age_label.is_empty() {
        // "just now" already reads as a time, so don't append "ago".
        if input.age_label == "just now" {
            parts.push("checked just now".to_string());
        } else {
            parts.push(format!("checked {} ago", input.age_label));
        }
    }
    if let Some(err) = input.error.as_deref() {
        parts.push(format!("last check failed: {err}"));
    }
    parts.join(" \u{00B7} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_json(env: &str) -> String {
        format!(
            "{{\"type\":\"frame\",\"surface\":\"panel\",\
             \"rect\":{{\"x\":0,\"y\":0,\"width\":28,\"height\":3}},\
             \"focused\":false,\
             \"host\":{{\"focus\":{{\"session_id\":null,\"session_ref\":null,\
             \"cwd\":null,\"agent\":\"None\",\"is_self_focus\":false}},\
             \"env\":{env}}}}}"
        )
    }

    fn env_with_input(input: &VersionRenderInput) -> String {
        let inner = serde_json::to_string(input).unwrap();
        format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&inner).unwrap()
        )
    }

    fn action_json(action_id: &str, kind: &str) -> String {
        format!(
            "{{\"type\":\"action\",\"action_id\":\"{action_id}\",\
             \"surface\":\"panel\",\"column\":1,\"row\":0,\"kind\":\"{kind}\"}}"
        )
    }

    #[test]
    fn renders_panel_layer_from_injected_input() {
        let input = VersionRenderInput {
            current: Some("0.7.3".into()),
            latest: Some("0.7.5".into()),
            ..Default::default()
        };
        let event: UiEvent =
            serde_json::from_str(&frame_json(&env_with_input(&input))).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::Panel));
        assert!(!update.layers[0].nodes.is_empty());
        assert!(update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id == render::ACTION_OPEN));
    }

    #[test]
    fn missing_input_env_renders_nothing() {
        let event: UiEvent = serde_json::from_str(&frame_json("{}")).expect("frame parses");
        let update = handle_event(&event);
        assert!(update.layers.is_empty());
    }

    #[test]
    fn toast_message_names_update_date_and_url() {
        // Pure builder — no shared static, so it's isolation-safe under the
        // parallel test runner (LAST_INPUT is exercised via the action path below).
        let input = VersionRenderInput {
            current: Some("0.7.3".into()),
            latest: Some("0.7.5".into()),
            notes_url: Some("https://github.com/ogulcancelik/herdr/releases/tag/v0.7.5".into()),
            latest_date: Some("2026-07-20".into()),
            age_label: "3h".into(),
            ..Default::default()
        };
        let msg = toast_message(&input);
        assert!(msg.contains("update 0.7.5 available"), "{msg}");
        assert!(msg.contains("releases/tag/v0.7.5"), "{msg}");
        assert!(msg.contains("released 2026-07-20"), "{msg}");
        assert!(msg.contains("checked 3h ago"), "{msg}");
    }

    #[test]
    fn toast_message_up_to_date_variant() {
        let input = VersionRenderInput {
            current: Some("0.7.5".into()),
            latest: Some("0.7.5".into()),
            ..Default::default()
        };
        assert!(toast_message(&input).contains("up to date"));
    }

    #[test]
    fn click_left_down_emits_exactly_one_toast() {
        // Content-agnostic (LAST_INPUT is a shared static across parallel tests):
        // assert only that a left_down on our action yields one ShowToast effect.
        let action: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_OPEN, "left_down")).expect("action");
        let update = handle_event(&action);
        assert_eq!(update.effects.len(), 1);
        assert!(matches!(update.effects[0], HostEffect::ShowToast { .. }));
    }

    #[test]
    fn click_left_up_is_ignored() {
        let action: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_OPEN, "left_up")).expect("action");
        let update = handle_event(&action);
        assert!(update.effects.is_empty(), "left_up must not toast");
    }
}
