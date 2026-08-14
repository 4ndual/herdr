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
    HostEffect, MouseEvent, MouseEventKind, UiEvent, UiHitRect, UiLayer, UiSurface, UiUpdate,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::Mutex;
use tidus_bar_model::BarInput;

/// Last snapshot rendered, so a click resolves against exactly what is on
/// screen rather than whatever the collector wrote in the meantime.
static LAST_INPUT: Mutex<Option<BarInput>> = Mutex::new(None);

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// The carved row's absolute rect (x, y, width) from the last frame, and the
/// chip the cursor is over. Both are resolved from the RAW mouse stream
/// (capability `mouse_all`): the host suppresses raw `Move` over a module's own
/// declared hit-rects, so a bar that wants hover must declare none and do its
/// own arithmetic — the button-dash / archive-dash model.
static ROW_RECT: Mutex<Option<(u16, u16, u16)>> = Mutex::new(None);
// The bar is HOVER-INERT. There is deliberately no hover state here.
//
// It used to swap the whole row for a detail line about the chip under the
// cursor. Three things were wrong with that, and they compound:
//
//   * the row is navigation-dense — chips sit shoulder to shoulder, so moving
//     the mouse ACROSS the bar to reach one rewrote the row under the cursor
//     several times on the way;
//   * the swap changed the row's WIDTH, so the host re-carved the slot and the
//     chips moved while being aimed at;
//   * a parked cursor kept a stale detail line on screen after the bar changed
//     beneath it — no Move event is coming to correct a cursor that has not
//     moved, which cost an independent video gate two failures.
//
// The summary row now renders statically and hover changes nothing. Detail is
// not lost: a chip's own text carries its status dot, and left-click still
// navigates to the agent itself, which is the answer the detail line was
// standing in for.

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
    *ROW_RECT.lock().expect("row rect mutex poisoned") =
        if ctx.rect.width == 0 || ctx.rect.height == 0 {
            None
        } else {
            Some((ctx.rect.x, ctx.rect.y, ctx.rect.width))
        };

    render_from(&input)
}

/// Say "I have nothing to show" in the one way the host understands.
///
/// `UiUpdate::default()` is NOT that: v3 updates default to
/// [`UiUpdateScope::Partial`], and `merge_layers` only replaces the surfaces an
/// update actually names. An update with no layers therefore names no surface,
/// replaces nothing, and leaves the previously painted row on screen — forever.
///
/// That is how the bar kept showing a full chip row over a plain shell with no
/// harness: the collector correctly emitted an empty snapshot within ~60 ms,
/// the guest correctly decided there was nothing to draw, and the host
/// correctly kept the last thing it had been given. An independent video gate
/// failed this twice before the meaning of "empty" was pinned down.
///
/// `Full` scope with no layers replaces this module's entire contribution with
/// nothing, which is exactly the claim. It is used ONLY where the bar has
/// genuinely nothing to show — every other `UiUpdate::default()` in this guest
/// still means "no change", which is what a no-op mouse move must send.
fn clear_bar() -> UiUpdate {
    UiUpdate {
        layers: Vec::new(),
        scope: herdr_ui_plugin_api::UiUpdateScope::Full,
        ..Default::default()
    }
}

/// Build the layer for `input`. One rendering, used by the frame path and by
/// the click path, so what a click lands on is by construction what was drawn.
fn render_from(input: &BarInput) -> UiUpdate {
    let width = render::content_width(input);
    if width == 0 {
        return clear_bar();
    }
    let content = Rect::new(0, 0, width, 1);
    let mut buf = Buffer::empty(content);
    render::render(input, content, &mut buf);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);

    // NO declarative hit-rects. Clicks are resolved from the same layout the
    // row was drawn from, so the cell that was clicked is by construction the
    // cell that was drawn. (The rects were originally omitted so raw `Move`
    // would reach the guest for hover; hover is gone, and resolving clicks from
    // the layout is worth keeping on its own — one geometry, not two.)
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

/// Which chip covers an absolute cell, from the SAME layout the row was drawn
/// from — so the cell that answers a click is by construction the cell that was
/// drawn there, with no second copy of the geometry to drift.
fn chip_at(input: &BarInput, column: u16, row: u16) -> Option<usize> {
    let (ox, oy, width) = (*ROW_RECT.lock().expect("row rect mutex poisoned"))?;
    if row != oy {
        return None;
    }
    let rel = column.checked_sub(ox)?;
    if rel >= width {
        return None;
    }
    let (_, placed) = render::layout(input)?;
    placed
        .iter()
        .find(|p| rel >= p.x && rel < p.x + p.width)
        .map(|p| p.index)
}

/// Hover reveals, left-click navigates, right-click does nothing.
///
/// Right-click used to report the chip's detail as a toast. The canon reserves
/// right-click for a contextual MENU and puts detail on hover; this bar has no
/// secondary actions, so right-click is inert rather than being given a second,
/// undiscoverable meaning.
fn handle_mouse(mouse: &MouseEvent) -> UiUpdate {
    let Some(input) = LAST_INPUT.lock().expect("input mutex poisoned").clone() else {
        return UiUpdate::default();
    };
    let hit = chip_at(&input, mouse.column, mouse.row);

    if !matches!(mouse.kind, MouseEventKind::LeftDown) {
        // Move, right-click, wheel: nothing. The bar is hover-inert, so the
        // mouse crossing it must not repaint the row, must not change its
        // width, and must not move the chips the user is aiming at.
        return UiUpdate::default();
    }

    let Some(index) = hit else {
        return UiUpdate::default();
    };
    let Some(chip) = input.chips.get(index) else {
        // The bar changed under the click (a child finished between the frame
        // and the press). Doing nothing is correct; guessing at a neighbour
        // would open an agent the user did not aim at.
        return UiUpdate::default();
    };

    // No pane in this instance — a `light` child living inside conductor, or one
    // the reaper took. There is nothing to swap the terminal area to, so the
    // click REFUSES OUT LOUD (canon §7: an action that cannot be performed says
    // so) rather than doing nothing. This is the only toast the bar emits, and
    // it is an outcome, not a description.
    let Some(tab_id) = chip.tab_id.clone() else {
        return UiUpdate {
            effects: vec![HostEffect::ShowToast {
                message: format!("{} has no pane to open", chip.name),
            }],
            ..Default::default()
        };
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
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The remembered row rect is process-global.
        *ROW_RECT.lock().expect("row rect mutex poisoned") = None;
        *LAST_INPUT.lock().expect("input mutex poisoned") = None;
        guard
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
            finished: false,
        }
    }

    fn seed(input: BarInput) {
        *LAST_INPUT.lock().expect("input mutex poisoned") = Some(input);
        // The row as the host would carve it: origin (0, 0), 200 columns.
        *ROW_RECT.lock().expect("row rect mutex poisoned") = Some((0, 0, 200));
    }

    /// Press a chip at the CENTRE of the cells the row actually drew for it.
    /// The click path under test IS the geometry, so a test that called the
    /// handler with a chip id would prove nothing about a real press.
    fn click_chip(index: usize, kind: MouseEventKind) -> UiUpdate {
        let input = LAST_INPUT
            .lock()
            .expect("input mutex poisoned")
            .clone()
            .expect("seeded");
        let (_, placed) = render::layout(&input).expect("a row to click");
        match placed.iter().find(|p| p.index == index) {
            Some(p) => handle_mouse(&MouseEvent {
                kind,
                column: p.x + p.width / 2,
                row: 0,
            }),
            // No such chip drawn — press past the end of the row, which is what
            // clicking a chip that vanished actually looks like.
            None => handle_mouse(&MouseEvent {
                kind,
                column: 199,
                row: 0,
            }),
        }
    }

    /// Press the FIRST and LAST cell of a chip, not just its middle: an
    /// off-by-one in the width only shows at the edges (canon §10).
    fn click_chip_edges(index: usize) -> (UiUpdate, UiUpdate) {
        let input = LAST_INPUT
            .lock()
            .expect("input mutex poisoned")
            .clone()
            .expect("seeded");
        let (_, placed) = render::layout(&input).expect("a row to click");
        let p = placed
            .iter()
            .find(|p| p.index == index)
            .expect("chip is drawn");
        let press = |column| {
            handle_mouse(&MouseEvent {
                kind: MouseEventKind::LeftDown,
                column,
                row: 0,
            })
        };
        (press(p.x), press(p.x + p.width - 1))
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
        let update = click_chip(2, MouseEventKind::LeftDown);
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
        let update = click_chip(0, MouseEventKind::LeftDown);
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
        let update = click_chip(2, MouseEventKind::LeftDown);
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
        let update = click_chip(2, MouseEventKind::LeftDown);
        match update.effects.as_slice() {
            [HostEffect::ShowToast { message }] => {
                assert!(message.contains("no pane to open"), "{message}");
            }
            other => panic!("expected a report, got {other:?}"),
        }
    }

    /// Canon §1: right-click opens a contextual MENU or does nothing. This bar
    /// has no secondary actions, so right-click is inert — it must not navigate
    /// and must not describe (hover already does that).
    #[test]
    fn a_right_click_neither_navigates_nor_reports() {
        let _guard = test_guard();
        seed(sample());
        let update = click_chip(2, MouseEventKind::RightDown);
        assert!(
            update.effects.is_empty(),
            "right-click must be inert, got {:?}",
            update.effects
        );
    }

    /// Canon §2: hovering a chip reveals that chip's detail in place, and
    /// leaving the row restores the summary. No toast is involved.
    /// The bar is hover-inert: crossing it changes nothing.
    ///
    /// The row is navigation-dense, so a hover that rewrote it also changed its
    /// width, re-carved the host slot, and moved the chips the user was aiming
    /// at — while a parked cursor kept describing a bar that had since changed
    /// underneath it.
    #[test]
    fn hovering_a_chip_changes_nothing() {
        let _guard = test_guard();
        seed(sample());
        let summary = text_of(&render_from(&sample()));

        for index in 0..3 {
            let moved = click_chip(index, MouseEventKind::Move);
            assert!(
                moved.layers.is_empty() && moved.effects.is_empty(),
                "a move over chip {index} must produce no update at all"
            );
        }

        // And the row itself is unchanged after all that crossing.
        assert_eq!(text_of(&render_from(&sample())), summary);
    }

    /// Canon §10: the edges of a chip are as clickable as its middle.
    #[test]
    fn the_first_and_last_cell_of_a_chip_both_click() {
        let _guard = test_guard();
        seed(sample());
        let (first, last) = click_chip_edges(2);
        for (which, update) in [("first", first), ("last", last)] {
            match update.effects.as_slice() {
                [HostEffect::Api { method, params, .. }] => {
                    assert_eq!(method, "tab.focus", "{which} cell");
                    assert_eq!(params["tab_id"], "w1:t-kid", "{which} cell");
                }
                other => panic!("{which} cell of the chip is dead: {other:?}"),
            }
        }
    }

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

    #[test]
    fn a_click_on_a_chip_that_vanished_does_nothing() {
        let _guard = test_guard();
        seed(sample());
        // The bar changed between the frame and the press. Opening a
        // neighbouring agent would be worse than doing nothing.
        let update = click_chip(9, MouseEventKind::LeftDown);
        assert!(update.effects.is_empty());
        let update = click_chip(42, MouseEventKind::LeftDown);
        assert!(update.effects.is_empty());
    }

    /// The bar publishes NO hit-rects: declaring one makes the host suppress
    /// the raw Move events hover needs (the F5 dedupe). Clicks are resolved
    /// from the same layout the row was drawn from — see click_chip.
    #[test]
    fn no_hit_rects_are_published_so_hover_keeps_working() {
        let _guard = test_guard();
        let input = sample();
        let rect = Rect::new(7, 42, 80, 1);
        let update = handle_event(&frame_event(&input, rect));
        let layer = update.layers.first().expect("a layer");
        assert!(layer.hits.is_empty(), "hit rects would kill hover");
        // and the row is still clickable, from its own geometry
        seed(input.clone());
        *ROW_RECT.lock().expect("row rect mutex poisoned") = Some((rect.x, rect.y, rect.width));
        let (_, placed) = render::layout(&input).expect("row");
        let p = placed.iter().find(|p| p.index == 2).expect("chip");
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: rect.x + p.x + p.width / 2,
            row: rect.y,
        });
        assert!(!update.effects.is_empty(), "the drawn cell must click");
    }

    #[test]
    fn the_first_frame_has_no_rect_yet_and_is_not_clickable() {
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
        // The repaint keeps the previous row; clickability comes from the
        // geometry now, not from published rects.
        assert!(!layer.nodes.is_empty(), "and stays drawn");
    }

    /// A cursor parked on the bar must not keep a stale row on screen when the
    /// bar changes underneath it.
    ///
    /// This was the hover pattern's sharpest failure — no Move event is coming
    /// to correct a cursor that has not moved, so the guest kept describing an
    /// agent that was no longer there, and an independent video gate caught the
    /// bar reading "tb-root [idle] · claude · focused" over a plain shell.
    /// Being hover-inert removes the failure at the source: there is no
    /// cursor-dependent state left to go stale.
    #[test]
    fn a_parked_cursor_cannot_describe_a_bar_that_changed_under_it() {
        let _guard = test_guard();
        seed(sample());
        let _ = click_chip(0, MouseEventKind::Move);

        // Focus moves to a pane with no harness: the bar has nothing to show.
        let update = handle_event(&frame_event(&BarInput::default(), Rect::new(0, 0, 80, 1)));
        assert!(update.layers.is_empty(), "the band goes neutral");
        assert_eq!(
            update.scope,
            herdr_ui_plugin_api::UiUpdateScope::Full,
            "an empty PARTIAL update names no surface and so clears nothing"
        );

        // The cursor has not moved. A move event at the same cell still says
        // nothing, and in particular does not resurrect a row for the vanished
        // lineage. Sent raw rather than through `click_chip`, which needs a
        // drawn row to aim at and there is deliberately no longer one.
        let after = handle_mouse(&MouseEvent {
            kind: MouseEventKind::Move,
            column: 2,
            row: 0,
        });
        assert!(after.layers.is_empty(), "no layer comes back from a hover");
    }

    /// Index 0 naming a different agent used to be a hover-invalidation
    /// problem. With no hover there is nothing to invalidate — the frame simply
    /// draws whoever is there now.
    #[test]
    fn a_changed_lineage_is_simply_redrawn() {
        let _guard = test_guard();
        seed(sample());
        let _ = click_chip(0, MouseEventKind::Move);

        let mut other = sample();
        other.chips[0].name = "other-root".into();
        let update = handle_event(&frame_event(&other, Rect::new(0, 0, 80, 1)));
        let text = text_of(&update);
        assert!(
            text.contains("other-root"),
            "the new lineage is drawn: {text}"
        );
        assert!(
            !text.contains("tb-root"),
            "and the previous one is gone: {text}"
        );
    }

    /// The bar with nothing to show must CLEAR the surface, not merely decline
    /// to redraw it.
    ///
    /// v3 updates default to `Partial`, where `merge_layers` replaces only the
    /// surfaces an update names. An empty Partial update names none, so the
    /// host keeps the last row it was given: on a plain shell the bar kept
    /// showing a full chip row for a harness that pane has nothing to do with.
    /// Two independent video gates failed on that before the contract was
    /// pinned here.
    #[test]
    fn an_empty_bar_clears_the_surface_rather_than_leaving_the_old_row() {
        let _guard = test_guard();
        // Paint a real bar first, so there IS something to leave behind.
        let painted = handle_event(&frame_event(&sample(), Rect::new(0, 30, 120, 1)));
        assert!(
            !painted.layers.is_empty(),
            "precondition: a row was painted"
        );

        let update = handle_event(&frame_event(&BarInput::default(), Rect::new(0, 30, 120, 1)));
        assert!(update.layers.is_empty(), "nothing to draw");
        assert_eq!(
            update.scope,
            herdr_ui_plugin_api::UiUpdateScope::Full,
            "and it must say so with Full scope, or the old row survives"
        );
    }

    /// The inverse: a no-op event must NOT clear the bar. If "no change" and
    /// "nothing to show" both cleared, a mouse move that changes no hover would
    /// blank the row the user is looking at.
    /// Mouse motion across the bar is a no-op, and a no-op must stay
    /// `Partial`: an empty `Full` update would CLEAR the row, so a cursor
    /// crossing the bar would erase it.
    #[test]
    fn a_move_event_leaves_the_bar_alone() {
        let _guard = test_guard();
        // seed() installs the row rect the click helper aims at; sending a
        // frame with a different rect here would move the row out from under it.
        seed(sample());
        for index in [0, 1, 2] {
            let noop = click_chip(index, MouseEventKind::Move);
            assert!(
                noop.layers.is_empty(),
                "a move over chip {index} is a no-op"
            );
            assert_eq!(
                noop.scope,
                herdr_ui_plugin_api::UiUpdateScope::Partial,
                "a no-op must stay Partial so it changes nothing"
            );
        }
    }
}
