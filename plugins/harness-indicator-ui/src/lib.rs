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
//! Interaction: HOVER the strip → the row itself swaps to the full attribution
//! (config dir, and which evidence produced it) and swaps back when the cursor
//! leaves. Nothing is clickable: this strip only reports.
//!
//! It used to LEFT-CLICK to toast that attribution. The UX canon puts detail on
//! hover and reserves clicks for actions, because "click to find out what this
//! is" is indistinguishable from "click to do something" on a strip full of
//! other plugins' clickable chips. A one-row carve has no slack row for a
//! tooltip and the guest API has no positioned node to float one, so the detail
//! takes over the row it elaborates — see render::render_hovered.
//!
//! Terminology: "harness" here is a **fleet harness** (`~/harnesses/<name>`),
//! not herdr's unrelated internal use of the word for an agent CLI kind.

mod render;

use harness_indicator_model::IndicatorInput;
use herdr_ui_plugin_api::{MouseEvent, UiEvent, UiHitRect, UiLayer, UiSurface, UiUpdate};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::Mutex;

/// Last snapshot rendered, so a click can build its toast from exactly what the
/// strip is showing without re-reading any file.
static LAST_INPUT: Mutex<Option<IndicatorInput>> = Mutex::new(None);

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// The carved row's absolute rect (x, y, width) from the last frame, and whether
/// the cursor is currently inside it. Hover is resolved from the RAW mouse
/// stream (capability `mouse_all`) because the host suppresses raw `Move` over a
/// module's own declared hit-rects — the same reason button-dash and
/// archive-dash declare none.
static STRIP_RECT: Mutex<Option<(u16, u16, u16)>> = Mutex::new(None);
static HOVERED: Mutex<bool> = Mutex::new(false);

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
        UiEvent::Mouse(mouse) => return handle_mouse(mouse),
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

    // Remember where the row landed so the RAW mouse stream can be resolved
    // against it: mouse coordinates are absolute, and so is this rect.
    *STRIP_RECT.lock().expect("strip rect mutex poisoned") =
        if ctx.rect.width == 0 || ctx.rect.height == 0 {
            None
        } else {
            Some((ctx.rect.x, ctx.rect.y, ctx.rect.width))
        };

    render_from(&input)
}

/// Build the layer for `input` in its current hover state. Shared by the frame
/// path and the mouse path, so a hover repaint is the same rendering as a frame
/// repaint rather than a second, drifting copy of it.
fn render_from(input: &IndicatorInput) -> UiUpdate {
    // Render into a CONTENT-sized buffer, independent of the incoming rect, so
    // the host can measure the natural width and carve the slot on the very
    // first content frame (before which `ctx.rect` is still zero).
    let width = render::content_width(input);
    if width == 0 {
        return UiUpdate::default();
    }
    let hovered = *HOVERED.lock().expect("hover mutex poisoned");
    // The hovered form is wider than the summary; render it into its own
    // content-sized buffer so the host measures what it will actually draw.
    let width = if hovered {
        render::hovered_width(input).max(width)
    } else {
        width
    };
    let content = Rect::new(0, 0, width, 1);
    let mut buf = Buffer::empty(content);
    if hovered {
        render::render_hovered(input, content, &mut buf);
    } else {
        render::render(input, content, &mut buf);
    }
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);

    // NO declarative hit-rects: they would make the host suppress the raw
    // `Move` events this strip needs to know it is hovered (the F5 dedupe), and
    // there is nothing here to click anyway.
    let hits: Vec<UiHitRect> = Vec::new();

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

/// Hover in, hover out. Clicks are ignored on purpose: this strip reports, and
/// a reporting surface that also acts is how a glance becomes an accident.
fn handle_mouse(mouse: &MouseEvent) -> UiUpdate {
    let inside = match *STRIP_RECT.lock().expect("strip rect mutex poisoned") {
        Some((x, y, width)) => {
            mouse.row == y && mouse.column >= x && mouse.column < x.saturating_add(width)
        }
        None => false,
    };
    let mut hovered = HOVERED.lock().expect("hover mutex poisoned");
    if *hovered == inside {
        // No change ⇒ no repaint. A strip that re-rendered on every mouse move
        // would repaint the bottom row continuously while the user works.
        return UiUpdate::default();
    }
    *hovered = inside;
    drop(hovered);
    let Some(input) = LAST_INPUT.lock().expect("input mutex poisoned").clone() else {
        return UiUpdate::default();
    };
    render_from(&input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_indicator_model::{HarnessRef, HarnessSource};

    /// The guest keeps process-wide state, so tests that touch it must not run
    /// concurrently.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Hover state is process-global; a test that left the strip hovered
        // would otherwise hand the next one the detail row instead of the
        // summary.
        *HOVERED.lock().expect("hover mutex poisoned") = false;
        *STRIP_RECT.lock().expect("strip rect mutex poisoned") = None;
        *LAST_INPUT.lock().expect("input mutex poisoned") = None;
        guard
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

    /// Flatten an update's rendered text, for assertions about what the row says.
    fn text_of(update: &UiUpdate) -> String {
        update
            .layers
            .iter()
            .flat_map(|layer| layer.nodes.iter())
            .filter_map(|node| match node {
                herdr_ui_plugin_api::UiNode::Line { spans } => Some(spans),
                _ => None,
            })
            .flatten()
            .map(|span| span.text.clone())
            .collect()
    }

    fn frame_with(input: &IndicatorInput) -> UiUpdate {
        let raw = serde_json::to_string(input).expect("encode");
        let event: UiEvent =
            serde_json::from_str(&frame_json(&env_with(&raw))).expect("frame parses");
        handle_event(&event)
    }

    /// No hit-rects, on purpose: declaring one makes the host suppress the raw
    /// `Move` events this strip needs in order to know it is hovered, and there
    /// is nothing here to click.
    #[test]
    fn renders_a_panel_layer_with_no_hit_rects() {
        let _guard = test_guard();
        let update = frame_with(&sample());
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::Panel));
        assert!(!update.layers[0].nodes.is_empty());
        assert!(update.layers[0].hits.is_empty());
    }

    /// A zero-sized carve leaves nothing to hover: the remembered rect is
    /// cleared so a stray mouse move cannot "hover" a strip that is not drawn.
    #[test]
    fn a_zero_sized_panel_is_not_hoverable() {
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

    fn mouse(kind: herdr_ui_plugin_api::MouseEventKind, column: u16, row: u16) -> UiEvent {
        UiEvent::Mouse(MouseEvent { kind, column, row })
    }

    /// The canon trigger: hovering swaps the row to the full attribution, and
    /// leaving swaps it back. Nothing is emitted as a toast, and no click acts.
    #[test]
    fn hovering_the_strip_shows_the_evidence_in_place() {
        use herdr_ui_plugin_api::MouseEventKind;
        let _guard = test_guard();
        let summary = frame_with(&sample());
        let summary_text = text_of(&summary);

        // frame_json() carves the row at x=3, y=47, width=80.
        let hovered = handle_event(&mouse(MouseEventKind::Move, 10, 47));
        let hovered_text = text_of(&hovered);
        assert!(
            hovered_text.contains("config dir") || hovered_text.contains("evidence"),
            "hover must reveal the attribution, got: {hovered_text}"
        );
        assert_ne!(hovered_text, summary_text, "hover must change the row");
        assert!(
            hovered.effects.is_empty(),
            "detail is rendered, never toasted"
        );

        // Leaving the row restores the summary.
        let left = handle_event(&mouse(MouseEventKind::Move, 10, 20));
        assert_eq!(text_of(&left), summary_text);

        // A click on the strip does nothing at all: this surface only reports.
        assert!(
            handle_event(&mouse(MouseEventKind::LeftDown, 10, 47))
                .effects
                .is_empty(),
            "a click on a reporting strip must do nothing"
        );
    }

    /// A move that does not cross the boundary must not repaint: the bottom row
    /// would otherwise re-render on every mouse motion while the user works.
    #[test]
    fn moving_inside_the_row_does_not_repaint() {
        use herdr_ui_plugin_api::MouseEventKind;
        let _guard = test_guard();
        frame_with(&sample());
        assert!(!handle_event(&mouse(MouseEventKind::Move, 10, 47))
            .layers
            .is_empty());
        assert!(handle_event(&mouse(MouseEventKind::Move, 12, 47))
            .layers
            .is_empty());
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
