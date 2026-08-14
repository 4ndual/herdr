//! Tidus subagent bar: a persistent bottom row showing the lineage of the
//! focused orchestrator and its direct children, where each child is a chip you
//! can click to swap the terminal area to that agent.
//!
//! Why it exists: tidus can already spawn a tree of agents, but herdr's tab bar
//! is a flat list in creation order — it cannot say who spawned whom, and once
//! children launch hidden (the host's create-hidden work) it will not show them
//! at all. This bar is the doorway: the only surface that renders the fleet's
//! *spawn* graph, rooted at whatever you are currently looking at.
//!
//! Data path (the generic `ui_input_file` transport — no host code): the
//! out-of-process `tidus-bar-collector` resolves the snapshot each dash tick and
//! writes `tidus_bar_input.json` into this module's state dir; the host reads
//! that file per frame and injects it as `HERDR_PLUGIN_UI_INPUT_JSON`.
//!
//! Interaction:
//!   * LEFT-click a chip → the terminal area swaps to that agent (`tab.focus`,
//!     or `tab.show {focus:true}` once children launch hidden). Clicking the
//!     pinned parent swaps back the same way.
//!   * LEFT-click a chip with no pane in this instance (a `light` child that
//!     runs inside `conductor`, or one reaped away) → report instead of
//!     navigating, because there is no PTY to show.
//!   * RIGHT-click → report. A bar you are looking at rather than driving
//!     should not carry a destructive second gesture.
//!
//! The bar never steals focus on its own: it navigates only on a click, which
//! is the brief's first-class UX rule (children spawn `focus:false`).

mod render;

use herdr_ui_plugin_api::{
    HostEffect, UiActionKind, UiEvent, UiHitRect, UiLayer, UiSurface, UiUpdate,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::Mutex;
use tidus_bar_model::BarInput;

/// Last snapshot rendered, so a click resolves against exactly what is on
/// screen rather than whatever the collector wrote in the meantime.
static LAST_INPUT: Mutex<Option<BarInput>> = Mutex::new(None);

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

// ── ABI version ────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_api_version() -> i32 {
    3
}

// ── Allocator (host writes event JSON here) ────────────────────────────────

/// Reused input buffer — deliberately NOT a fresh allocation per call. The v3
/// guest ABI has no `dealloc` export, so a per-event `Vec` leak would ratchet
/// wasm linear memory up forever. The host's sequence is strictly
/// alloc-then-deliver on one thread, so reuse is safe; the response buffer is
/// separate, so an outbound update cannot be clobbered by the next payload.
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
    let input: BarInput = match serde_json::from_str(json) {
        Ok(input) => input,
        Err(_) => return UiUpdate::default(),
    };
    // A snapshot from a newer collector may mean something different by the
    // same field names; render nothing rather than a confident wrong answer.
    if input.schema != tidus_bar_model::SCHEMA {
        return UiUpdate::default();
    }

    // Focusing the bar itself is not a pane change. Repaint the previous
    // snapshot instead of blanking, so clicking a chip does not erase the bar
    // you just clicked.
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

    // One hit rect PER CHIP. `UiHitRect` is ABSOLUTE (screen cells), not
    // relative to the panel: a rect at 0,0 silently points at the terminal's
    // top-left corner, so the click does nothing and some unrelated cell
    // answers instead — a failure with no error anywhere, which is why every
    // rect is anchored on `ctx.rect` and covered by a test.
    let hits = if ctx.rect.width == 0 || ctx.rect.height == 0 {
        Vec::new()
    } else {
        render::layout(&input)
            .map(|(_, placed)| {
                placed
                    .into_iter()
                    .filter(|p| p.x < ctx.rect.width)
                    .map(|p| UiHitRect {
                        action_id: format!("{}{}", render::CHIP_ACTION, p.index),
                        x: ctx.rect.x + p.x,
                        y: ctx.rect.y,
                        // Clip at the panel edge so a chip scrolled off the
                        // right cannot claim cells outside the bar.
                        width: p.width.min(ctx.rect.width - p.x),
                        height: 1,
                    })
                    .collect()
            })
            .unwrap_or_default()
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

/// Left-click navigates, right-click reports.
fn handle_action(action_id: &str, kind: UiActionKind) -> UiUpdate {
    let Some(index) = action_id
        .strip_prefix(render::CHIP_ACTION)
        .and_then(|raw| raw.parse::<usize>().ok())
    else {
        return UiUpdate::default();
    };
    let Some(input) = LAST_INPUT.lock().expect("input mutex poisoned").clone() else {
        return UiUpdate::default();
    };
    let Some(chip) = input.chips.get(index) else {
        // The bar changed under the click (a child finished between the frame
        // and the press). Doing nothing is correct; guessing at a neighbour
        // would open an agent the user did not aim at.
        return UiUpdate::default();
    };

    let report = || UiUpdate {
        effects: vec![HostEffect::ShowToast {
            message: render::detail(&input, chip),
        }],
        ..Default::default()
    };

    if !matches!(kind, UiActionKind::LeftDown) {
        return report();
    }
    // No pane in this instance — a `light` child living inside conductor, or
    // one the reaper took. There is nothing to swap the terminal area to, so
    // the chip reports what it knows instead of failing silently.
    let Some(tab_id) = chip.tab_id.clone() else {
        return report();
    };

    // Swap the terminal area to this agent. `tab.show` un-hides AND focuses in
    // one call, which is exactly what a hidden child needs; `tab.focus` is the
    // plain path for a tab already on the bar. Both are `api_write` methods
    // (`ui_plugin_api_bridge.rs`), and both are synchronous, so the swap has
    // happened by the time the next frame is drawn.
    let (method, params) = if chip.hidden {
        (
            "tab.show",
            serde_json::json!({ "tab_id": tab_id, "focus": true }),
        )
    } else {
        ("tab.focus", serde_json::json!({ "tab_id": tab_id }))
    };
    UiUpdate {
        effects: vec![HostEffect::Api {
            request_id: format!("tidus-bar:focus:{index}"),
            method: method.to_string(),
            params,
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidus_bar_model::{Chip, ChipRole, ChipStatus, EdgeKind};

    /// The guest keeps process-wide state, so tests that touch it must not run
    /// concurrently.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn chip(name: &str, role: ChipRole) -> Chip {
        Chip {
            name: name.to_string(),
            role,
            status: ChipStatus::Idle,
            edge: (role == ChipRole::Child).then_some(EdgeKind::Dispatch),
            tab_id: Some(format!("w1:t-{name}")),
            pane_id: Some(format!("w1:p-{name}")),
            hidden: false,
            task: None,
            job_tail: None,
            exec: None,
            runtime: Some("claude".into()),
        }
    }

    fn seed(input: BarInput) {
        *LAST_INPUT.lock().expect("input mutex poisoned") = Some(input);
    }

    fn sample() -> BarInput {
        BarInput {
            focus_harness: Some("squad".into()),
            evidence: Some("HARNESS_NAME of pid 1".into()),
            chips: vec![
                chip("main", ChipRole::Ancestor),
                chip("squad", ChipRole::Current),
                chip("kid", ChipRole::Child),
            ],
            empty_reason: None,
            ..Default::default()
        }
    }

    /// Built from the real host types rather than hand-written JSON, so a
    /// change to the frame shape is a compile error here instead of a runtime
    /// "missing field" that would make every guest test lie.
    fn frame_event(input: &BarInput, rect: Rect) -> UiEvent {
        frame_event_from_json(&serde_json::to_string(input).expect("encode"), rect)
    }

    fn frame_event_from_json(json: &str, rect: Rect) -> UiEvent {
        let mut env = std::collections::HashMap::new();
        env.insert("HERDR_PLUGIN_UI_INPUT_JSON".to_string(), json.to_string());
        UiEvent::Frame(herdr_ui_plugin_api::UiFrameContext {
            surface: UiSurface::Panel,
            rect: herdr_ui_plugin_api::UiLayoutRect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            },
            focused: false,
            host: herdr_ui_plugin_api::HostSnapshot {
                focus: Default::default(),
                env,
            },
        })
    }

    #[test]
    fn a_left_click_focuses_that_chips_tab() {
        let _guard = test_guard();
        seed(sample());
        let update = handle_action("tidus-bar:chip:2", UiActionKind::LeftDown);
        match update.effects.as_slice() {
            [HostEffect::Api { method, params, .. }] => {
                assert_eq!(method, "tab.focus");
                assert_eq!(params["tab_id"], "w1:t-kid");
            }
            other => panic!("expected one api effect, got {other:?}"),
        }
    }

    #[test]
    fn clicking_the_pinned_parent_swaps_back_the_same_way() {
        let _guard = test_guard();
        seed(sample());
        let update = handle_action("tidus-bar:chip:0", UiActionKind::LeftDown);
        match update.effects.as_slice() {
            [HostEffect::Api { method, params, .. }] => {
                assert_eq!(method, "tab.focus");
                assert_eq!(params["tab_id"], "w1:t-main");
            }
            other => panic!("expected the ancestor's tab, got {other:?}"),
        }
    }

    #[test]
    fn a_hidden_child_is_shown_rather_than_focused() {
        let _guard = test_guard();
        let mut input = sample();
        input.chips[2].hidden = true;
        seed(input);
        let update = handle_action("tidus-bar:chip:2", UiActionKind::LeftDown);
        match update.effects.as_slice() {
            [HostEffect::Api { method, params, .. }] => {
                // tab.focus alone cannot reveal a hidden tab; this is the one
                // call that both un-hides and focuses.
                assert_eq!(method, "tab.show");
                assert_eq!(params["focus"], true);
                assert_eq!(params["tab_id"], "w1:t-kid");
            }
            other => panic!("expected tab.show, got {other:?}"),
        }
    }

    #[test]
    fn a_chip_with_no_pane_reports_instead_of_navigating() {
        let _guard = test_guard();
        let mut input = sample();
        input.chips[2].tab_id = None;
        input.chips[2].pane_id = None;
        input.chips[2].exec = Some("light".into());
        seed(input);
        let update = handle_action("tidus-bar:chip:2", UiActionKind::LeftDown);
        match update.effects.as_slice() {
            [HostEffect::ShowToast { message }] => {
                assert!(message.contains("no pane in this instance"), "{message}");
            }
            other => panic!("expected a report, got {other:?}"),
        }
    }

    #[test]
    fn a_right_click_reports_and_never_navigates() {
        let _guard = test_guard();
        seed(sample());
        let update = handle_action("tidus-bar:chip:2", UiActionKind::RightDown);
        assert!(
            update
                .effects
                .iter()
                .all(|e| matches!(e, HostEffect::ShowToast { .. })),
            "a reporting gesture must not move the user: {:?}",
            update.effects
        );
    }

    #[test]
    fn a_click_on_a_chip_that_vanished_does_nothing() {
        let _guard = test_guard();
        seed(sample());
        // The bar changed between the frame and the press. Opening a
        // neighbouring agent would be worse than doing nothing.
        let update = handle_action("tidus-bar:chip:9", UiActionKind::LeftDown);
        assert!(update.effects.is_empty());
        let update = handle_action("tidus-bar:not-a-chip", UiActionKind::LeftDown);
        assert!(update.effects.is_empty());
    }

    #[test]
    fn hit_rects_are_absolute_and_one_per_chip() {
        let _guard = test_guard();
        let input = sample();
        let rect = Rect::new(7, 42, 80, 1);
        let update = handle_event(&frame_event(&input, rect));
        let layer = update.layers.first().expect("a layer");
        assert_eq!(layer.hits.len(), 3, "one hit rect per chip");
        for hit in &layer.hits {
            // Anchored on the panel, not on 0,0 — the bug that made an earlier
            // strip's click land on the tab bar with no error anywhere.
            assert!(hit.x >= rect.x, "hit {hit:?} left of the panel");
            assert_eq!(hit.y, rect.y);
            assert!(
                hit.x + hit.width <= rect.x + rect.width,
                "{hit:?} overflows"
            );
        }
        // Action ids must address the chips in order.
        let ids: Vec<&str> = layer.hits.iter().map(|h| h.action_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["tidus-bar:chip:0", "tidus-bar:chip:1", "tidus-bar:chip:2"]
        );
    }

    #[test]
    fn the_first_frame_has_no_rect_yet_and_publishes_no_dead_hit_rects() {
        let _guard = test_guard();
        let input = sample();
        let update = handle_event(&frame_event(&input, Rect::new(0, 0, 0, 0)));
        let layer = update.layers.first().expect("a layer");
        assert!(
            layer.hits.is_empty(),
            "a zero rect cannot host a real click target"
        );
        assert!(!layer.nodes.is_empty(), "content is still measured");
    }

    #[test]
    fn every_token_the_collector_writes_survives_into_the_row() {
        let _guard = test_guard();
        // Pins the LITERAL bytes: a model field rename that rebuilds the
        // collector but not the guest drops a token while still looking
        // plausible, which a hand-built sample input cannot catch.
        let json = r#"{"schema":1,"home":"/home/a","self_focus":false,
            "focus_harness":"squad",
            "chips":[
              {"name":"rootone","role":"ancestor","status":"idle","tab_id":"w1:t1"},
              {"name":"squad","role":"current","status":"busy","tab_id":"w1:t2"},
              {"name":"kidalpha","role":"child","status":"attention","edge":"dispatch","tab_id":"w1:t3"}
            ]}"#;
        let update = handle_event(&frame_event_from_json(json, Rect::new(0, 30, 120, 1)));
        let text = format!("{:?}", update.layers.first().expect("a layer").nodes);
        for token in ["rootone", "squad", "kidalpha"] {
            assert!(text.contains(token), "{token} missing from the row");
        }
        assert!(
            text.contains(ChipStatus::Attention.dot()),
            "the attention dot must reach the screen"
        );
    }

    #[test]
    fn an_unknown_schema_renders_nothing_rather_than_guessing() {
        let _guard = test_guard();
        let mut input = sample();
        input.schema = tidus_bar_model::SCHEMA + 1;
        let update = handle_event(&frame_event(&input, Rect::new(0, 0, 80, 1)));
        assert!(update.layers.is_empty());
    }

    #[test]
    fn a_self_focus_frame_repaints_instead_of_blanking() {
        let _guard = test_guard();
        seed(sample());
        let blank = BarInput {
            self_focus: true,
            ..Default::default()
        };
        let update = handle_event(&frame_event(&blank, Rect::new(0, 0, 80, 1)));
        let layer = update
            .layers
            .first()
            .expect("the previous bar is repainted");
        assert_eq!(layer.hits.len(), 3, "and stays clickable");
    }

    #[test]
    fn an_empty_bar_emits_no_layer_so_the_host_carves_no_band() {
        let _guard = test_guard();
        let update = handle_event(&frame_event(&BarInput::default(), Rect::new(0, 0, 80, 1)));
        assert!(update.layers.is_empty());
    }
}
