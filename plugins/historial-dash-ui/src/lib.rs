//! Historial-dash guest: a pure right-panel renderer of the host-provided
//! pane-close history model (mirrors the goal-dash wave-1 pattern — the host
//! projects a fully-precomputed model into `historial_input.json` each tick and
//! the guest just draws it).
//!
//! Interaction split:
//! - The header collapse toggle is guest-owned: it flips guest collapse state
//!   and emits `SetSurfaceCollapsed { RightPanel }`, which the host maps onto the
//!   right-panel rail selection.
//! - The per-row restore / fork / delete hit-rects are declared here but RESOLVED
//!   host-side (the host owns the pane-close log and the resume machinery), so
//!   the guest never receives or handles those actions.

mod render;

use herdr_ui_plugin_api::{
    HostEffect, UiActionKind, UiEvent, UiFrameContext, UiLayer, UiNode, UiSurface, UiUpdate,
    UiUpdateScope,
};
use historial_dash_model::HistorialRenderModel;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Guest-owned collapse state, toggled by clicks on `render::ACTION_TOGGLE_PANEL`.
/// Re-synced from the host's carved rect width in `handle_frame` so host state
/// (a restart, or a rail collapse) always wins.
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

// ── Static update (returned on init; nothing declared statically) ──────────

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

fn parse_model(ctx: &UiFrameContext) -> Option<HistorialRenderModel> {
    let json = match ctx.host.env.get("HERDR_PLUGIN_UI_INPUT_JSON") {
        Some(j) if !j.is_empty() => j,
        _ => return None,
    };
    serde_json::from_str(json).ok()
}

fn right_panel_layer(nodes: Vec<UiNode>, hits: Vec<herdr_ui_plugin_api::UiHitRect>) -> UiLayer {
    UiLayer {
        surface: UiSurface::RightPanel,
        z_index: 0,
        consumes_mouse: false,
        nodes,
        hits,
        data: None,
    }
}

fn handle_frame(ctx: &UiFrameContext) -> UiUpdate {
    let Some(model) = parse_model(ctx) else {
        return UiUpdate::default();
    };

    let area = Rect::new(ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height);

    // Zero rect: the host only carves the right panel when a right_panel layer
    // already has content, and collapses the content column to nothing when the
    // rail is collapsed. Emit an invisible placeholder line (so the rail keeps
    // this plugin's icon and the carve stays) ONLY when there is history to show;
    // an empty log requests no carve at all.
    if area.width == 0 || area.height == 0 {
        if model.rows.is_empty() {
            return UiUpdate::default();
        }
        return UiUpdate {
            layers: vec![right_panel_layer(
                vec![UiNode::Line { spans: Vec::new() }],
                Vec::new(),
            )],
            effects: Vec::new(),
            scope: UiUpdateScope::Partial,
            consumed_action_ids: Vec::new(),
        };
    }

    // A carved-down 3-col handle rect always means collapsed.
    if area.width <= 3 {
        COLLAPSED.store(true, Ordering::SeqCst);
    } else {
        COLLAPSED.store(false, Ordering::SeqCst);
    }

    let mut buf = Buffer::empty(area);
    let hits = render::render(&model, area, &mut buf);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);

    UiUpdate {
        layers: vec![right_panel_layer(nodes, hits)],
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
    // Only the collapse toggle is guest-owned; restore/fork/delete are resolved
    // host-side and never reach the guest.
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

    fn model_env(rows: usize) -> String {
        let mut model = HistorialRenderModel::default();
        for i in 0..rows {
            model.rows.push(historial_dash_model::HistorialRow {
                key: format!("{i}:w_1_p_{i}"),
                agent: "claude".to_string(),
                label: "herdr".to_string(),
                reltime: "5m".to_string(),
                cwd: "/tmp".to_string(),
                restorable: true,
            });
        }
        let model_json = serde_json::to_string(&model).expect("model serializes");
        format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&model_json).expect("env value serializes")
        )
    }

    #[test]
    fn renders_right_panel_rows_from_input() {
        let event: UiEvent =
            serde_json::from_str(&frame_json(&model_env(2), 40)).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::RightPanel));
        assert!(!update.layers[0].hits.is_empty());
        assert!(update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id.starts_with("historial:restore:")));
    }

    #[test]
    fn zero_rect_with_rows_emits_placeholder_to_keep_rail_icon() {
        let event: UiEvent =
            serde_json::from_str(&frame_json(&model_env(1), 0)).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(update.layers[0].hits.is_empty());
    }

    #[test]
    fn zero_rect_without_rows_requests_no_carve() {
        let event: UiEvent =
            serde_json::from_str(&frame_json(&model_env(0), 0)).expect("frame parses");
        let update = handle_event(&event);
        assert!(update.layers.is_empty());
    }

    #[test]
    fn missing_input_renders_nothing() {
        let event: UiEvent = serde_json::from_str(&frame_json("{}", 40)).expect("frame parses");
        assert!(handle_event(&event).layers.is_empty());
    }

    #[test]
    fn toggle_left_down_emits_set_surface_collapsed() {
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_PANEL, "left_down"))
                .expect("action parses");
        let update = handle_event(&event);
        assert_eq!(update.effects.len(), 1);
        assert!(matches!(
            update.effects[0],
            HostEffect::SetSurfaceCollapsed {
                surface: UiSurface::RightPanel,
                ..
            }
        ));
    }

    #[test]
    fn restore_action_is_not_handled_by_guest() {
        // Host resolves row actions; the guest ignores them entirely.
        let event: UiEvent =
            serde_json::from_str(&action_json("historial:restore:1:w_1_p_1", "left_down"))
                .expect("action parses");
        let update = handle_event(&event);
        assert!(update.layers.is_empty());
        assert!(update.effects.is_empty());
    }

    #[test]
    fn toggle_ignores_left_up() {
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_PANEL, "left_up"))
                .expect("action parses");
        assert!(handle_event(&event).effects.is_empty());
    }
}
