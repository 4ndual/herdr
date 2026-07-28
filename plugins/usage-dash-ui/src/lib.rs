mod render;

use herdr_ui_plugin_api::{
    HostEffect, UiActionKind, UiEvent, UiLayer, UiSurface, UiUpdate, UiUpdateScope,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use usage_dash_model::UsageRenderInput;

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Guest-owned strip collapse state, toggled by clicks on
/// [`render::ACTION_TOGGLE_COLLAPSE`]. Re-synced from the host's carved rect
/// height every frame (a 1-row rect always means collapsed), so host state
/// wins after restarts and hot-reloads — same pattern as goal-dash's panel.
static COLLAPSED: AtomicBool = AtomicBool::new(false);

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
        UiEvent::Action {
            action_id, kind, ..
        } => return handle_action(action_id, *kind),
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
    // Host state wins: a 1-row carve is the collapsed strip, anything taller
    // is expanded. (A zero rect means "not carved" — leave the latch alone.)
    if area.height == 1 {
        COLLAPSED.store(true, Ordering::SeqCst);
    } else if area.height > 1 {
        COLLAPSED.store(false, Ordering::SeqCst);
    }
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

fn handle_action(action_id: &str, kind: UiActionKind) -> UiUpdate {
    // Act on press only; the host routes both left_down and left_up for one
    // physical click, and reacting to both would double-fire the toggle.
    if kind != UiActionKind::LeftDown || action_id != render::ACTION_TOGGLE_COLLAPSE {
        return UiUpdate::default();
    }
    let collapsed = !COLLAPSED.load(Ordering::SeqCst);
    COLLAPSED.store(collapsed, Ordering::SeqCst);
    UiUpdate {
        layers: Vec::new(),
        effects: vec![HostEffect::SetSurfaceCollapsed {
            surface: UiSurface::UsageStrip,
            collapsed,
        }],
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_dash_model::{Cli, UsageSnapshot};

    /// Build a `frame` event JSON with `env` as the host env map. Exercises the
    /// real transport: the guest only ever sees a deserialized `UiEvent`.
    fn frame_json(env: &str) -> String {
        frame_json_with_height(env, 7)
    }

    fn frame_json_with_height(env: &str, height: u16) -> String {
        format!(
            "{{\"type\":\"frame\",\"surface\":\"usage_strip\",\
             \"rect\":{{\"x\":0,\"y\":0,\"width\":80,\"height\":{height}}},\
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

    fn env_json() -> String {
        format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&sample_input_json()).expect("env value serializes")
        )
    }

    fn action_json(action_id: &str, kind: &str) -> String {
        format!(
            "{{\"type\":\"action\",\"action_id\":\"{action_id}\",\
             \"surface\":\"usage_strip\",\"column\":1,\"row\":0,\"kind\":\"{kind}\"}}"
        )
    }

    #[test]
    fn full_render_declares_collapse_hit() {
        let event: UiEvent =
            serde_json::from_str(&frame_json(&env_json())).expect("frame parses");
        let update = handle_event(&event);
        assert!(update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id == render::ACTION_TOGGLE_COLLAPSE));
    }

    #[test]
    fn one_row_rect_renders_expand_affordance() {
        let event: UiEvent =
            serde_json::from_str(&frame_json_with_height(&env_json(), 1)).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(!update.layers[0].nodes.is_empty());
        assert!(update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id == render::ACTION_TOGGLE_COLLAPSE));
        // NOTE: no assertion on the COLLAPSED static here — tests run in
        // parallel and other frames (height 7) race on the same latch.
    }

    #[test]
    fn collapse_action_left_down_emits_set_surface_collapsed() {
        COLLAPSED.store(false, Ordering::SeqCst);
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_COLLAPSE, "left_down"))
                .expect("action parses");
        let update = handle_event(&event);
        assert_eq!(update.effects.len(), 1);
        match &update.effects[0] {
            HostEffect::SetSurfaceCollapsed { surface, collapsed } => {
                assert!(matches!(surface, UiSurface::UsageStrip));
                assert!(*collapsed);
            }
            other => panic!("expected SetSurfaceCollapsed, got {other:?}"),
        }
    }

    #[test]
    fn collapse_action_ignores_left_up() {
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_COLLAPSE, "left_up"))
                .expect("action parses");
        let update = handle_event(&event);
        assert!(update.effects.is_empty(), "left_up must not toggle collapse");
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
