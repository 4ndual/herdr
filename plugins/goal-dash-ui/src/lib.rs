mod render;

use goal_dash_model::GoalRenderModel;
use herdr_ui_plugin_api::{
    HostEffect, UiActionKind, UiEvent, UiFrameContext, UiLayer, UiNode, UiSurface, UiUpdate,
    UiUpdateScope,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Guest-owned collapse state, toggled by clicks on `render::ACTION_TOGGLE_PANEL`.
/// Persists across frames for the wasm instance's lifetime (reset on hot-reload /
/// trap, same as any other guest static). Re-synced from the host's carved rect
/// width in `handle_frame` so a restart (host state) always wins.
static COLLAPSED: AtomicBool = AtomicBool::new(false);

// ── ABI version ────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_api_version() -> i32 {
    3
}

// ── Allocator (host writes event JSON here) ────────────────────────────────

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
    match event {
        UiEvent::Frame(ctx) => handle_frame(ctx),
        UiEvent::Action {
            action_id, kind, ..
        } => handle_action(action_id, *kind),
        _ => UiUpdate::default(),
    }
}

fn handle_frame(ctx: &UiFrameContext) -> UiUpdate {
    // v3 generic transport (spec §7.1): the host reads the plugin's declared
    // `ui_input_file` (goal_input.json) from the module's state dir and injects
    // its contents here.
    let json = match ctx.host.env.get("HERDR_PLUGIN_UI_INPUT_JSON") {
        Some(j) if !j.is_empty() => j,
        _ => return UiUpdate::default(),
    };

    let model: GoalRenderModel = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return UiUpdate::default(),
    };

    let area = Rect::new(ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height);
    // Zero rect = the host hasn't carved the right panel yet, and it only
    // carves when this layer already has nodes. Emit an invisible placeholder
    // line to request the carve; the next frame arrives with a real rect.
    // (A zero rect is NOT the collapsed handle — don't latch COLLAPSED here.)
    // Only request the carve when a session is focused: an idle layout keeps
    // every column for panes (and startup diagnostics stay fully visible on
    // small clients).
    if (area.width == 0 || area.height == 0) && !model.has_session {
        return UiUpdate::default();
    }
    if area.width == 0 || area.height == 0 {
        return UiUpdate {
            layers: vec![UiLayer {
                surface: UiSurface::RightPanel,
                z_index: 0,
                consumes_mouse: false,
                nodes: vec![UiNode::Line { spans: Vec::new() }],
                hits: Vec::new(),
                data: None,
            }],
            effects: Vec::new(),
            scope: UiUpdateScope::Partial,
            consumed_action_ids: Vec::new(),
        };
    }
    // Host state wins after restarts: a carved-down 3-col handle rect always
    // means collapsed, regardless of what our static currently holds.
    if area.width <= 3 {
        COLLAPSED.store(true, Ordering::SeqCst);
    }

    let mut buf = Buffer::empty(area);
    let hits = render::render(&model, area, &mut buf);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);

    UiUpdate {
        layers: vec![UiLayer {
            surface: UiSurface::RightPanel,
            z_index: 0,
            consumes_mouse: false,
            nodes,
            hits,
            data: None,
        }],
        effects: Vec::new(),
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    }
}

fn handle_action(action_id: &str, kind: UiActionKind) -> UiUpdate {
    // Act on press only; the host routes both left_down and left_up for one
    // physical click, and reacting to both would double-fire the toggle.
    if kind != UiActionKind::LeftDown {
        return UiUpdate::default();
    }
    if action_id != render::ACTION_TOGGLE_PANEL {
        return UiUpdate::default();
    }
    let collapsed = !COLLAPSED.load(Ordering::SeqCst);
    COLLAPSED.store(collapsed, Ordering::SeqCst);
    UiUpdate {
        layers: Vec::new(),
        effects: vec![HostEffect::SetSurfaceCollapsed {
            surface: UiSurface::RightPanel,
            collapsed,
        }],
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_json(env: &str, width: u16) -> String {
        format!(
            "{{\"type\":\"frame\",\"surface\":\"right_panel\",\
             \"rect\":{{\"x\":0,\"y\":0,\"width\":{width},\"height\":12}},\
             \"focused\":false,\
             \"host\":{{\"focus\":{{\"session_id\":null,\"session_ref\":null,\
             \"cwd\":null,\"agent\":\"None\",\"is_self_focus\":false}},\
             \"env\":{env}}}}}"
        )
    }

    fn action_json(action_id: &str, kind: &str) -> String {
        format!(
            "{{\"type\":\"action\",\"action_id\":\"{action_id}\",\
             \"surface\":\"right_panel\",\"column\":1,\"row\":0,\"kind\":\"{kind}\"}}"
        )
    }

    fn sample_model_json() -> String {
        let mut model = GoalRenderModel::default();
        model.has_session = false;
        serde_json::to_string(&model).expect("model serializes")
    }

    #[test]
    fn renders_right_panel_from_ui_input_env_key() {
        let env = format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&sample_model_json()).expect("env value serializes")
        );
        let event: UiEvent = serde_json::from_str(&frame_json(&env, 40)).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::RightPanel));
        assert!(!update.layers[0].nodes.is_empty());
        assert!(!update.layers[0].hits.is_empty(), "header collapse hit should be declared");
    }

    #[test]
    fn missing_ui_input_env_key_renders_nothing() {
        let event: UiEvent = serde_json::from_str(&frame_json("{}", 40)).expect("frame parses");
        let update = handle_event(&event);
        assert!(update.layers.is_empty());
    }

    #[test]
    fn action_guard_ignores_left_up() {
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_PANEL, "left_up"))
                .expect("action parses");
        let update = handle_event(&event);
        assert!(update.layers.is_empty());
        assert!(update.effects.is_empty(), "left_up must not toggle collapse state");
    }

    #[test]
    fn action_left_down_emits_set_surface_collapsed() {
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_PANEL, "left_down"))
                .expect("action parses");
        let update = handle_event(&event);
        assert_eq!(update.effects.len(), 1);
        match &update.effects[0] {
            HostEffect::SetSurfaceCollapsed { surface, .. } => {
                assert!(matches!(surface, UiSurface::RightPanel));
            }
            other => panic!("expected SetSurfaceCollapsed, got {other:?}"),
        }
    }
}
