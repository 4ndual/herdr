//! Harness-indicator guest: one bottom-row line naming the fleet harness the
//! FOCUSED pane is running inside, the agent herdr detects there, and the
//! folder it was launched from.
//!
//! Why it exists: a fleet pane and a plain pane look identical once the runtime
//! has painted over the screen. Nothing in herdr says which isolated config a
//! given agent is reading, so "am I in the harness I think I am" is answered by
//! guessing from a tab label. This strip answers it from the running process's
//! own environment, and says so when the harness's declared runtime and the
//! pane's actual runtime disagree.
//!
//! Data path (the generic `ui_input_file` transport — no host code): the
//! out-of-process `harness-indicator-collector` resolves the snapshot each dash
//! tick and writes `harness_input.json` into this module's state dir; the host
//! reads that file per frame and injects it as `HERDR_PLUGIN_UI_INPUT_JSON`.
//!
//! Interaction: LEFT-click the strip → toast the full attribution, including
//! the config dir and which evidence produced it.
//!
//! Terminology: "harness" here is a **fleet harness** (`~/harnesses/<name>`),
//! not herdr's unrelated internal use of the word for an agent CLI kind.

mod render;

use harness_indicator_model::IndicatorInput;
use herdr_ui_plugin_api::{
    HostEffect, UiActionKind, UiEvent, UiHitRect, UiLayer, UiSurface, UiUpdate,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::Mutex;

/// Last snapshot rendered, so a click can build its toast from exactly what the
/// strip is showing without re-reading any file.
static LAST_INPUT: Mutex<Option<IndicatorInput>> = Mutex::new(None);

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

// ── ABI version ────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_api_version() -> i32 {
    3
}

// ── Allocator (host writes event JSON here) ────────────────────────────────

/// Reused input buffer — deliberately NOT a fresh allocation per call. The v3
/// guest ABI has no `dealloc` export, so a per-event `Vec` leak would ratchet
/// wasm linear memory up forever (measured at 3.1 MB/min across the modules
/// that had this bug). The host's sequence is strictly alloc-then-deliver on
/// one thread, so reuse is safe; the response buffer is separate, so an
/// outbound update cannot be clobbered by the next inbound payload.
static HERDR_UI_INPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

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
        // A poisoned lock must not hand back a dangling pointer.
        Err(_) => 0,
    }
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

    // Generic transport: the host injects this module's ui_input_file contents
    // here. No key ⇒ nothing to render (the collector has not produced a
    // snapshot yet), and an unparseable one is treated the same way.
    let json = match ctx.host.env.get("HERDR_PLUGIN_UI_INPUT_JSON") {
        Some(json) if !json.is_empty() => json,
        _ => return UiUpdate::default(),
    };
    let input: IndicatorInput = match serde_json::from_str(json) {
        Ok(input) => input,
        Err(_) => return UiUpdate::default(),
    };

    // Focusing the strip itself is not a pane change. Repaint the previous
    // snapshot instead of blanking, so clicking the row does not erase the
    // thing you clicked (the same stickiness usage-dash uses).
    let input = if input.self_focus {
        match LAST_INPUT.lock().expect("input mutex poisoned").clone() {
            Some(previous) => previous,
            None => input,
        }
    } else {
        *LAST_INPUT.lock().expect("input mutex poisoned") = Some(input.clone());
        input
    };

    // Render into a CONTENT-sized buffer, independent of the incoming rect, so
    // the host can measure the natural width and carve the slot on the very
    // first content frame (before which `ctx.rect` is still zero).
    let width = render::content_width(&input);
    if width == 0 {
        return UiUpdate::default();
    }
    let content = Rect::new(0, 0, width, 1);
    let mut buf = Buffer::empty(content);
    render::render(&input, content, &mut buf);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);

    // One hit rect over the whole carved row: the strip has a single meaning,
    // so every cell of it should answer the same question.
    //
    // UiHitRect is ABSOLUTE (screen cells), not relative to the panel. A
    // rect at 0,0 silently points at the top-left corner of the terminal, so
    // clicking the strip does nothing and clicking the tab bar toasts — a
    // failure with no error anywhere, which is why it is anchored on the
    // panel's own rect and covered by a test.
    let hits = if ctx.rect.width == 0 || ctx.rect.height == 0 {
        Vec::new()
    } else {
        vec![UiHitRect {
            action_id: render::DETAIL_ACTION.to_string(),
            x: ctx.rect.x,
            y: ctx.rect.y,
            width: width.min(ctx.rect.width),
            height: 1,
        }]
    };

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

/// A click asks "on what evidence?" — answer with the full attribution rather
/// than repeating the line the user just clicked.
fn handle_action(action_id: &str, kind: UiActionKind) -> UiUpdate {
    if action_id != render::DETAIL_ACTION {
        return UiUpdate::default();
    }
    // Right-click is deliberately inert: this plugin only reports, and a
    // reporting surface with a destructive second gesture is how a glance
    // becomes an accident.
    if !matches!(kind, UiActionKind::LeftDown) {
        return UiUpdate::default();
    }
    let Some(input) = LAST_INPUT.lock().expect("input mutex poisoned").clone() else {
        return UiUpdate::default();
    };
    UiUpdate {
        effects: vec![HostEffect::ShowToast {
            message: render::detail(&input),
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_indicator_model::{HarnessRef, HarnessSource};

    /// The guest keeps process-wide state, so tests that touch it must not run
    /// concurrently.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn frame_json(env: &str) -> String {
        format!(
            "{{\"type\":\"frame\",\"surface\":\"panel\",\
             \"rect\":{{\"x\":3,\"y\":47,\"width\":80,\"height\":1}},\
             \"focused\":false,\
             \"host\":{{\"focus\":{{\"session_id\":null,\"session_ref\":null,\
             \"cwd\":null,\"agent\":\"None\",\"is_self_focus\":false}},\
             \"env\":{env}}}}}"
        )
    }

    fn env_with(raw: &str) -> String {
        format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(raw).expect("encode")
        )
    }

    fn sample() -> IndicatorInput {
        IndicatorInput {
            pane_id: Some("w1:p1".into()),
            agent: Some("claude".into()),
            harness: Some(HarnessRef {
                name: "e2e-claude".into(),
                runtime: Some("claude".into()),
                profile: false,
                source: HarnessSource::Env,
            }),
            agent_cwd: Some("/home/x/harnesses/e2e-claude/workspace".into()),
            shell_cwd: Some("/home/x/harnesses/e2e-claude/workspace".into()),
            home: Some("/home/x".into()),
            config_dir: Some("/home/x/harnesses/e2e-claude/config".into()),
            ..Default::default()
        }
    }

    fn frame_with(input: &IndicatorInput) -> UiUpdate {
        let raw = serde_json::to_string(input).expect("encode");
        let event: UiEvent =
            serde_json::from_str(&frame_json(&env_with(&raw))).expect("frame parses");
        handle_event(&event)
    }

    #[test]
    fn renders_a_panel_layer_with_one_hit_rect() {
        let _guard = test_guard();
        let update = frame_with(&sample());
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::Panel));
        assert!(!update.layers[0].nodes.is_empty());
        assert_eq!(update.layers[0].hits.len(), 1);
        let hit = &update.layers[0].hits[0];
        assert_eq!(hit.action_id, render::DETAIL_ACTION);
        // Absolute, anchored on the panel rect the host handed us — not 0,0.
        assert_eq!((hit.x, hit.y), (3, 47));
        assert!(hit.width > 0 && hit.width <= 80);
        assert_eq!(hit.height, 1);
    }

    #[test]
    fn a_zero_sized_panel_publishes_no_hit_rect() {
        let _guard = test_guard();
        let raw = serde_json::to_string(&sample()).expect("encode");
        let frame = frame_json(&env_with(&raw)).replace(
            "\"rect\":{\"x\":3,\"y\":47,\"width\":80,\"height\":1}",
            "\"rect\":{\"x\":0,\"y\":0,\"width\":0,\"height\":0}",
        );
        let event: UiEvent = serde_json::from_str(&frame).expect("frame parses");
        let update = handle_event(&event);
        // A click target with no panel behind it would sit over whatever else
        // occupies the top-left corner.
        assert!(update.layers[0].hits.is_empty());
    }

    #[test]
    fn missing_or_broken_input_renders_nothing() {
        let _guard = test_guard();
        let empty: UiEvent = serde_json::from_str(&frame_json("{}")).expect("frame parses");
        assert!(handle_event(&empty).layers.is_empty());
        let broken: UiEvent =
            serde_json::from_str(&frame_json(&env_with("{not json"))).expect("frame parses");
        assert!(handle_event(&broken).layers.is_empty());
    }

    #[test]
    fn an_empty_snapshot_carves_no_row() {
        let _guard = test_guard();
        let update = frame_with(&IndicatorInput::default());
        assert!(
            update.layers.is_empty(),
            "no focused pane must not reserve a row"
        );
    }

    #[test]
    fn self_focus_repaints_the_previous_pane_instead_of_blanking() {
        let _guard = test_guard();
        let first = frame_with(&sample());
        assert_eq!(first.layers.len(), 1);
        let sticky = frame_with(&IndicatorInput {
            self_focus: true,
            ..Default::default()
        });
        assert_eq!(
            sticky.layers.len(),
            1,
            "clicking the strip must not erase what it was showing"
        );
        assert_eq!(sticky.layers[0].nodes, first.layers[0].nodes);
    }

    #[test]
    fn left_click_toasts_the_evidence_and_right_click_does_nothing() {
        let _guard = test_guard();
        frame_with(&sample());
        let update = handle_action(render::DETAIL_ACTION, UiActionKind::LeftDown);
        match update.effects.first() {
            Some(HostEffect::ShowToast { message }) => {
                assert!(message.contains("e2e-claude"));
                assert!(message.contains("config dir"));
            }
            other => panic!("expected a toast, got {other:?}"),
        }
        assert!(
            handle_action(render::DETAIL_ACTION, UiActionKind::RightDown)
                .effects
                .is_empty()
        );
        assert!(
            handle_action("someone-elses-action", UiActionKind::LeftDown)
                .effects
                .is_empty()
        );
    }

    /// The bytes the shipped collector writes must render. `sample()` cannot
    /// catch collector↔model drift — it is re-serialized from the guest's own
    /// model — so this pins the literal snapshot shape instead.
    #[test]
    fn renders_from_the_shipped_collector_snapshot() {
        let _guard = test_guard();
        let raw = r#"{"pane_id":"w1:p3Q","agent":"claude","harness":{"name":"e2e-codex","runtime":"codex","profile":false,"source":"env"},"agent_cwd":"/home/tester/harnesses/e2e-codex/workspace","shell_cwd":"/home/tester/Projects/herdr","self_focus":false,"home":"/home/tester","config_dir":"/home/tester/harnesses/e2e-codex/config"}"#;
        let event: UiEvent =
            serde_json::from_str(&frame_json(&env_with(raw))).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        // Every field the collector sends must survive into the rendered text;
        // a rename on one side would otherwise silently drop a token and leave
        // a shorter, still-plausible strip.
        let text: String = update.layers[0]
            .nodes
            .iter()
            .filter_map(|node| match node {
                herdr_ui_plugin_api::UiNode::Line { spans } => Some(spans),
                _ => None,
            })
            .flatten()
            .map(|span| span.text.clone())
            .collect();
        assert!(text.contains("e2e-codex"), "harness name missing: {text}");
        assert!(
            text.contains("codex\u{2260}claude"),
            "mismatch missing: {text}"
        );
        assert!(
            text.contains("~/harnesses/e2e-codex/workspace"),
            "agent folder missing: {text}"
        );
        assert!(
            text.contains("~/Projects/herdr"),
            "shell folder missing: {text}"
        );
    }
}
