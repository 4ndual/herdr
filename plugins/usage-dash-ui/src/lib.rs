mod render;

use herdr_ui_plugin_api::{UiEvent, UiLayer, UiSurface, UiUpdate};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::Mutex;
use usage_dash_model::UsageRenderInput;

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

// ── Static update (returned on init; we have nothing to declare statically) ─

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
        _ => return UiUpdate::default(),
    };

    // v3 generic transport (spec §7.1): the host reads the plugin's declared
    // `ui_input_file` (usage_input.json) from the module's state dir and injects
    // its contents here. Replaces the wave-0 host-specific HERDR_USAGE_RENDER_JSON.
    let json = match ctx.host.env.get("HERDR_PLUGIN_UI_INPUT_JSON") {
        Some(j) if !j.is_empty() => j,
        _ => return UiUpdate::default(),
    };

    let input: UsageRenderInput = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return UiUpdate::default(),
    };

    let area = Rect::new(ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height);
    let mut buf = Buffer::empty(area);
    let hits = render::render(&input, area, &mut buf);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);

    UiUpdate {
        layers: vec![UiLayer {
            surface: UiSurface::UsageStrip,
            z_index: 0,
            consumes_mouse: false,
            nodes,
            hits,
            data: None,
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_dash_model::{Cli, UsageSnapshot};

    /// Build a `frame` event JSON with `env` as the host env map. Exercises the
    /// real transport: the guest only ever sees a deserialized `UiEvent`.
    fn frame_json(env: &str) -> String {
        format!(
            "{{\"type\":\"frame\",\"surface\":\"usage_strip\",\
             \"rect\":{{\"x\":0,\"y\":0,\"width\":80,\"height\":7}},\
             \"focused\":false,\
             \"host\":{{\"focus\":{{\"session_id\":null,\"session_ref\":null,\
             \"cwd\":null,\"agent\":\"None\",\"is_self_focus\":false}},\
             \"env\":{env}}}}}"
        )
    }

    fn sample_input_json() -> String {
        let mut snap = UsageSnapshot::empty(Cli::Claude);
        snap.model = Some("Opus 4.8".into());
        snap.ctx_used_pct = Some(29.0);
        let input = UsageRenderInput {
            snapshot: Some(snap),
            expanded: false,
            collapsed: false,
            stats_shown: true,
            scroll: 0,
            now: 1_000_000,
            five_hour_abs: String::new(),
            weekly_abs: String::new(),
        };
        serde_json::to_string(&input).expect("input serializes")
    }

    #[test]
    fn renders_usage_strip_from_ui_input_env_key() {
        // Host injects the plugin's ui_input_file contents under the generic
        // §7.1 key; the guest must render a usage_strip layer from it.
        let env = format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&sample_input_json()).expect("env value serializes")
        );
        let event: UiEvent = serde_json::from_str(&frame_json(&env)).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::UsageStrip));
        assert!(
            !update.layers[0].nodes.is_empty(),
            "usage strip should render nodes from the injected input"
        );
    }

    #[test]
    fn missing_ui_input_env_key_renders_nothing() {
        let event: UiEvent = serde_json::from_str(&frame_json("{}")).expect("frame parses");
        let update = handle_event(&event);
        assert!(
            update.layers.is_empty(),
            "no input ⇒ empty update (loss-free under partial-default merge)"
        );
    }
}
