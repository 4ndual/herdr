//! Harness Panel guest: the fleet daemon GUI, rendered in herdr's side panel.
//!
//! The guest owns presentation and input only. It has no sockets and no
//! filesystem, so every fact it draws arrives as `HERDR_PLUGIN_UI_INPUT_JSON`,
//! written by the Go collector, and every mutation leaves as an
//! `InvokePluginAction` effect that the collector's `op` mode performs.
//!
//! # Which surface it draws on
//!
//! It declares both `right_panel` and `panel` and emits its layer on whichever
//! one the host actually gives geometry for. On herdr 0.8.0+193 `right_panel`
//! is never carved (see the manifest), so in practice the `panel` placement
//! renders; when #32 lands, the right-panel rect becomes non-zero and the same
//! code moves to the real slot with no edit and without ever drawing twice.

pub mod model;
pub mod render;

use herdr_ui_plugin_api::{
    HostEffect, UiEvent, UiFrameContext, UiLayer, UiSurface, UiUpdate, UiUpdateScope,
};
use model::Snapshot;
use render::Ctx;
use std::sync::Mutex;

const PLUGIN_ID: &str = "builtin.harness-panel";
const OP_ACTION: &str = "panel-op";

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_api_version() -> i32 {
    3
}

/// Reused input buffer. The v3 ABI has no dealloc, so allocating per event
/// would ratchet linear memory up forever — the leak historial-dash documents.
static INPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_alloc(len: i32) -> i32 {
    match INPUT.lock() {
        Ok(mut buf) => {
            buf.clear();
            buf.resize(len.max(0) as usize, 0);
            buf.as_mut_ptr() as i32
        }
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

// ── guest-local view state ─────────────────────────────────────────────────
//
// Hover and scroll live here, not in the collector: they change on every mouse
// move, and round-tripping them through a file and a process would make the
// panel feel dead. Everything that outlives a frame — selection, view, tab —
// goes to the collector, because the collector has to fetch for it.

static HOVER: Mutex<Option<usize>> = Mutex::new(None);
static SCROLL: Mutex<usize> = Mutex::new(0);
/// Last rect the host gave us per surface, so a mouse event (which carries no
/// rect) can be resolved against the geometry the click was made on.
static LAST_RECT: Mutex<(u16, u16, u16, u16)> = Mutex::new((0, 0, 0, 0));
static LAST_SURFACE: Mutex<Option<UiSurface>> = Mutex::new(None);
static LAST_TOTAL_ROWS: Mutex<usize> = Mutex::new(0);

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_on_event(ptr: i32, len: i32) -> i32 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize) };
    let update = match serde_json::from_slice::<UiEvent>(bytes) {
        Ok(event) => handle(&event),
        Err(_) => UiUpdate::default(),
    };
    let encoded = serde_json::to_vec(&update).unwrap_or_default();
    match RESPONSE.lock() {
        Ok(mut guard) => {
            *guard = encoded;
            guard.as_ptr() as i32
        }
        Err(_) => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_on_event_len() -> i32 {
    RESPONSE.lock().map(|g| g.len() as i32).unwrap_or(0)
}

fn handle(event: &UiEvent) -> UiUpdate {
    match event {
        UiEvent::Frame(ctx) => on_frame(ctx),
        UiEvent::Mouse(mouse) => on_mouse(mouse),
        UiEvent::Action { action_id, .. } => on_action(action_id),
        _ => UiUpdate::default(),
    }
}

fn snapshot_from(ctx: &UiFrameContext) -> Snapshot {
    ctx.host
        .env
        .get("HERDR_PLUGIN_UI_INPUT_JSON")
        .and_then(|raw| serde_json::from_str::<Snapshot>(raw).ok())
        .unwrap_or_default()
}

fn on_frame(ctx: &UiFrameContext) -> UiUpdate {
    // Zero rect: the host carves a panel only for a module that ALREADY renders
    // content, so a guest that draws nothing without geometry never gets any —
    // it waits forever for a rect that its own silence prevents. Answer with a
    // one-line placeholder to claim the carve; the next frame arrives with real
    // geometry and renders properly.
    //
    // Only the `panel` surface gets that placeholder. Claiming the right panel
    // the same way would, once #32 makes that surface real, leave this plugin
    // rendering into both slots at once.
    if ctx.rect.width == 0 || ctx.rect.height == 0 {
        if ctx.surface != UiSurface::Panel {
            return UiUpdate::default();
        }
        return UiUpdate {
            layers: vec![UiLayer {
                surface: ctx.surface,
                z_index: 0,
                consumes_mouse: false,
                nodes: vec![herdr_ui_plugin_api::UiNode::Line {
                    spans: vec![herdr_ui_plugin_api::UiTextSpan {
                        text: "harness fleet".into(),
                        ..Default::default()
                    }],
                }],
                hits: Vec::new(),
                data: None,
            }],
            effects: Vec::new(),
            scope: UiUpdateScope::Partial,
            consumed_action_ids: Vec::new(),
        };
    }
    *LAST_RECT.lock().expect("rect") = (ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height);
    *LAST_SURFACE.lock().expect("surface") = Some(ctx.surface);

    let snap = snapshot_from(ctx);
    let hover = *HOVER.lock().expect("hover");
    let scroll = *SCROLL.lock().expect("scroll");
    let painted = render::paint(
        &snap,
        &Ctx {
            width: ctx.rect.width,
            height: ctx.rect.height,
            hover,
            scroll,
        },
    );
    *LAST_TOTAL_ROWS.lock().expect("rows") = painted.total_rows;

    UiUpdate {
        layers: vec![UiLayer {
            surface: ctx.surface,
            z_index: 0,
            consumes_mouse: true,
            nodes: painted.nodes,
            hits: painted.hits,
            data: None,
        }],
        effects: Vec::new(),
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    }
}

/// Mouse handling: hover tracking and wheel scrolling. Clicks arrive as
/// `UiEvent::Action` because they resolve against the hit rects, so this only
/// deals with movement and the wheel.
fn on_mouse(mouse: &herdr_ui_plugin_api::MouseEvent) -> UiUpdate {
    use herdr_ui_plugin_api::MouseEventKind;
    let (rx, ry, _rw, rh) = *LAST_RECT.lock().expect("rect");
    let local_row = mouse.row.saturating_sub(ry);
    match mouse.kind {
        MouseEventKind::ScrollDown => {
            let total = *LAST_TOTAL_ROWS.lock().expect("rows");
            let mut s = SCROLL.lock().expect("scroll");
            // Never scroll past the end: an empty panel below the last row reads
            // as broken, and the site's tables clamp the same way.
            let max = total.saturating_sub(1);
            if *s < max {
                *s += 1;
            }
        }
        MouseEventKind::ScrollUp => {
            let mut s = SCROLL.lock().expect("scroll");
            *s = s.saturating_sub(1);
        }
        MouseEventKind::Move | MouseEventKind::Drag => {
            // Rows start under the header (title + one tab row).
            const HEADER_ROWS: u16 = 2;
            let idx = local_row.checked_sub(HEADER_ROWS).map(|r| r as usize);
            let scroll = *SCROLL.lock().expect("scroll");
            let mut h = HOVER.lock().expect("hover");
            let next = idx.map(|i| i + scroll);
            if *h == next {
                return UiUpdate::default();
            }
            *h = next;
        }
        _ => return UiUpdate::default(),
    }
    let _ = (rx, rh, mouse.column);
    // Ask for a repaint; the next frame renders with the new hover/scroll.
    UiUpdate::default()
}

/// A click on a hit rect. `action_id` is `verb:argument`, built in render.rs.
fn on_action(action_id: &str) -> UiUpdate {
    let (verb, arg) = match action_id.split_once(':') {
        Some(pair) => pair,
        None => return UiUpdate::default(),
    };
    // A view or selection change resets scroll: keeping a scroll offset from a
    // 35-row fleet list on a 3-row templates list shows an empty panel.
    *SCROLL.lock().expect("scroll") = 0;

    let payload = match verb {
        "view" => serde_json::json!({ "action": "view", "view": arg }),
        "select" => serde_json::json!({ "action": "select", "target": arg }),
        "spawn" | "stop" | "refresh" => serde_json::json!({ "action": verb, "target": arg }),
        _ => return UiUpdate::default(),
    };

    UiUpdate {
        layers: Vec::new(),
        effects: vec![HostEffect::InvokePluginAction {
            plugin_id: PLUGIN_ID.to_string(),
            action_id: OP_ACTION.to_string(),
            payload_json: Some(payload.to_string()),
        }],
        scope: UiUpdateScope::Partial,
        consumed_action_ids: vec![action_id.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_on_a_tab_asks_the_collector_to_switch_view() {
        let update = on_action("view:orchestrator");
        match update.effects.first() {
            Some(HostEffect::InvokePluginAction {
                plugin_id,
                action_id,
                payload_json,
            }) => {
                assert_eq!(plugin_id, PLUGIN_ID);
                assert_eq!(action_id, OP_ACTION);
                let payload = payload_json.as_deref().unwrap_or_default();
                assert!(
                    payload.contains("\"view\":\"orchestrator\""),
                    "payload was {payload}"
                );
            }
            other => panic!("expected an action invocation, got {other:?}"),
        }
        assert_eq!(
            update.consumed_action_ids,
            vec!["view:orchestrator".to_string()]
        );
    }

    #[test]
    fn a_click_on_a_mutation_names_its_target() {
        let update = on_action("spawn:alpha");
        let Some(HostEffect::InvokePluginAction { payload_json, .. }) = update.effects.first()
        else {
            panic!("no effect");
        };
        let payload = payload_json.as_deref().unwrap_or_default();
        assert!(payload.contains("\"action\":\"spawn\""), "{payload}");
        assert!(payload.contains("\"target\":\"alpha\""), "{payload}");
    }

    #[test]
    fn an_unknown_action_does_nothing_rather_than_guessing() {
        assert!(on_action("bogus:x").effects.is_empty());
        assert!(on_action("nocolon").effects.is_empty());
    }

    #[test]
    fn switching_view_resets_scroll() {
        *SCROLL.lock().unwrap() = 9;
        let _ = on_action("view:templates");
        assert_eq!(*SCROLL.lock().unwrap(), 0);
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;
    use herdr_ui_plugin_api::{HostSnapshot, UiLayoutRect};
    use std::collections::HashMap;

    fn ctx(surface: UiSurface, w: u16, h: u16, input: Option<&str>) -> UiFrameContext {
        let mut env = HashMap::new();
        if let Some(raw) = input {
            env.insert("HERDR_PLUGIN_UI_INPUT_JSON".to_string(), raw.to_string());
        }
        UiFrameContext {
            surface,
            rect: UiLayoutRect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
            focused: false,
            host: HostSnapshot {
                focus: Default::default(),
                env,
            },
        }
    }

    #[test]
    fn a_zero_rect_panel_frame_claims_the_carve() {
        let u = on_frame(&ctx(UiSurface::Panel, 0, 0, None));
        assert_eq!(
            u.layers.len(),
            1,
            "must emit a placeholder to claim the carve"
        );
        assert!(!u.layers[0].nodes.is_empty());
    }

    #[test]
    fn a_zero_rect_right_panel_frame_stays_silent() {
        let u = on_frame(&ctx(UiSurface::RightPanel, 0, 0, None));
        assert!(
            u.layers.is_empty(),
            "claiming both slots would double-render post-#32"
        );
    }

    #[test]
    fn a_real_rect_renders_the_fleet_from_the_input_file() {
        let input = r#"{"connected":true,"view":"fleet","reason":"event",
            "fleet":[{"name":"alpha","status":"running","runtime":"claude","sessions":2}]}"#;
        let u = on_frame(&ctx(UiSurface::Panel, 40, 20, Some(input)));
        assert_eq!(u.layers.len(), 1);
        let text: String = u.layers[0]
            .nodes
            .iter()
            .map(|n| match n {
                herdr_ui_plugin_api::UiNode::Line { spans } => {
                    spans.iter().map(|s| s.text.clone()).collect::<String>() + "\n"
                }
                _ => String::new(),
            })
            .collect();
        assert!(text.contains("alpha"), "fleet not rendered: {text}");
        assert!(text.contains("live"), "connection state missing: {text}");
        assert!(!u.layers[0].hits.is_empty(), "no clickable hit rects");
    }
}

/// Test-only seam so the byte-level contract tests in `tests/contract.rs` can
/// drive the exact parse and render path the guest uses, on the exact bytes the
/// Go collector writes. Not part of the wasm ABI — the host never calls it.
pub mod testing {
    use crate::model::Snapshot;
    use crate::render::{self, Ctx};

    #[derive(Clone, Copy)]
    pub struct RenderSize {
        pub width: u16,
        pub height: u16,
    }

    /// Parse collector output exactly as `on_frame` does.
    pub fn parse_snapshot(raw: &str) -> Option<Snapshot> {
        serde_json::from_str::<Snapshot>(raw).ok()
    }

    /// Render to plain text, the way the terminal would flatten the layer.
    pub fn render_text(raw: &str, size: RenderSize) -> String {
        let snap = parse_snapshot(raw).unwrap_or_default();
        let painted = render::paint(
            &snap,
            &Ctx {
                width: size.width,
                height: size.height,
                hover: None,
                scroll: 0,
            },
        );
        painted
            .nodes
            .iter()
            .map(|node| match node {
                herdr_ui_plugin_api::UiNode::Line { spans } => {
                    spans.iter().map(|s| s.text.clone()).collect::<String>()
                }
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
