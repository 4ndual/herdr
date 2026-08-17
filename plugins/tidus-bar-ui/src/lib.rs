//! Tidus subagent bar: a persistent bottom band showing the lineage of the
//! selected agent, the selected agent itself, its brothers, and — on a second
//! row underneath it — its own children. Every chip is clickable and swaps the
//! terminal area to that agent.
//!
//! Selecting a brother re-roots the band on that brother WITHOUT removing the
//! brothers: the brother set is a property of the parent, so navigating
//! sideways keeps the row you navigated from. The band is one row tall at rest
//! and two only while the selected agent has children to list.
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
//!   * RIGHT-click a chip → opens a small context menu in the bar's own row
//!     (`Copy metadata`, `Close`, `Cancel`), styled and behaving like the
//!     host's built-in tab/pane menus. `Copy metadata` reports the chip's
//!     harness, runtime, pane id, tab id, workspace and launch folder.
//!     `Close` closes that chip's tab; if the closed chip was the CURRENT
//!     (focused) one, focus is pointed back at its parent in the lineage
//!     rather than whatever the host would otherwise pick.
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
use tidus_bar_model::{BarInput, Chip, ChipRole};

/// Last snapshot rendered, so a click resolves against exactly what is on
/// screen rather than whatever the collector wrote in the meantime.
static LAST_INPUT: Mutex<Option<BarInput>> = Mutex::new(None);

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// A context menu opened by right-click, addressing the same `BarInput::chips`
/// index a chip click would resolve against. Kept separate from `LAST_INPUT`
/// so a menu survives an unrelated snapshot refresh (it re-resolves the chip
/// by index on every repaint) but is dropped the moment that index no longer
/// names a chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenMenu {
    chip_index: usize,
}

static MENU: Mutex<Option<OpenMenu>> = Mutex::new(None);

/// The carved band's absolute rect (x, y, width, height) from the last frame.
/// Resolved from the RAW mouse stream (capability `mouse_all`): the host
/// suppresses raw `Move` over a module's own declared hit-rects, so a bar that
/// does its own hit testing must declare none and do its own arithmetic — the
/// button-dash / archive-dash model.
///
/// The HEIGHT is carried because the band is two rows tall whenever the
/// selected node has children, and a press then has to be attributed to the
/// right row: the child row is indented to sit UNDER the selected chip, so the
/// same column names a different agent on each row.
static ROW_RECT: Mutex<Option<(u16, u16, u16, u16)>> = Mutex::new(None);
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
            Some((ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height))
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
///
/// Checks the open menu first: while one is up, the row shows the menu
/// instead of the chips, on every repaint (the next natural frame tick,
/// not just the click that opened it) until it is dismissed or acted on.
fn render_from(input: &BarInput) -> UiUpdate {
    if let Some(menu) = *MENU.lock().expect("menu mutex poisoned") {
        return render_menu(input, menu);
    }
    let width = render::content_width(input);
    let height = render::content_height(input);
    if width == 0 || height == 0 {
        return clear_bar();
    }
    // Content-sized in BOTH axes. The manifest sizes this band `auto`, and the
    // host measures an auto band's height as the number of `Line` nodes the
    // module emits (`ui.rs::panel_content_dims`), so the buffer's height is
    // literally how many rows of pane the bar takes: one at rest, two while the
    // selected node has children to show underneath it.
    let content = Rect::new(0, 0, width, height);
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

/// Render the open menu in place of the chip row. Falls back to a normal
/// repaint (and drops the menu) if `menu.chip_index` no longer names a chip —
/// the bar refreshed out from under an open menu, and describing a chip that
/// vanished would be worse than closing the menu that was pointed at it.
fn render_menu(input: &BarInput, menu: OpenMenu) -> UiUpdate {
    let Some(chip) = input.chips.get(menu.chip_index) else {
        *MENU.lock().expect("menu mutex poisoned") = None;
        return render_from(input);
    };
    let width = render::menu_content_width(&chip.name);
    let content = Rect::new(0, 0, width, 1);
    let mut buf = Buffer::empty(content);
    render::render_menu(&chip.name, content, &mut buf);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);
    UiUpdate {
        layers: vec![UiLayer {
            surface: UiSurface::Panel,
            z_index: 0,
            consumes_mouse: false,
            nodes,
            hits: Vec::new(),
            data: None,
        }],
        ..Default::default()
    }
}

/// Which chip covers an absolute cell, from the SAME layout the rows were drawn
/// from — so the cell that answers a click is by construction the cell that was
/// drawn there, with no second copy of the geometry to drift.
///
/// Both axes are matched: the child row is indented to sit under the selected
/// chip, so column alone would resolve a press on a child to whichever brother
/// happens to be above it.
fn chip_at(input: &BarInput, column: u16, row: u16) -> Option<usize> {
    let (ox, oy, width, height) = (*ROW_RECT.lock().expect("row rect mutex poisoned"))?;
    let rel_row = row.checked_sub(oy)?;
    if rel_row >= height {
        return None;
    }
    let rel = column.checked_sub(ox)?;
    if rel >= width {
        return None;
    }
    let (_, placed) = render::layout(input)?;
    placed
        .iter()
        .find(|p| p.row == rel_row && rel >= p.x && rel < p.x + p.width)
        .map(|p| p.index)
}

/// Which menu item (by index into [`render::MENU_ITEMS`]) covers an absolute
/// cell, from the SAME layout the menu row was drawn from.
fn menu_item_at(chip_name: &str, column: u16, row: u16) -> Option<usize> {
    let (ox, oy, _width, _height) = (*ROW_RECT.lock().expect("row rect mutex poisoned"))?;
    // The menu is always exactly one row, on the band's first line, even when
    // the chip grid it replaced was two rows tall.
    if row != oy {
        return None;
    }
    let rel = column.checked_sub(ox)?;
    let (_, placed) = render::menu_layout(chip_name);
    placed
        .iter()
        .find(|p| rel >= p.x && rel < p.x + p.width)
        .map(|p| p.index)
}

/// The chip immediately upstream of `chip_index` in fleet lineage.
///
/// `BarInput::chips` is ordered root-most ancestor first, then Current, then
/// the brothers, then the children (see the model's own doc comment), so:
///
///   * a CHILD hangs off the selected node itself (`Current`);
///   * a BROTHER hangs off the same parent the selected node does — the LAST
///     ancestor, not the slot before it in the vector, which for a brother is
///     another brother rather than a parent;
///   * an ancestor or the selected node hangs off the previous slot, because
///     within the lineage prefix the vector order IS the descent.
fn parent_tab_id(chips: &[Chip], chip_index: usize) -> Option<String> {
    let chip = chips.get(chip_index)?;
    match chip.role {
        ChipRole::Child => chips
            .iter()
            .find(|c| c.role == ChipRole::Current)
            .and_then(|c| c.tab_id.clone()),
        ChipRole::Sibling => chips
            .iter()
            .filter(|c| c.role == ChipRole::Ancestor)
            .next_back()
            .and_then(|c| c.tab_id.clone()),
        ChipRole::Ancestor | ChipRole::Current => chip_index
            .checked_sub(1)
            .and_then(|i| chips.get(i))
            .and_then(|c| c.tab_id.clone()),
    }
}

/// The workspace id a `tab_id` belongs to. Tab ids are `<workspace>:<tab>`
/// (see the live pane roster and every other id on the chip), so the
/// workspace is simply the id's own prefix — no extra round trip needed for
/// the metadata report.
fn workspace_of(tab_id: &str) -> Option<&str> {
    tab_id.split_once(':').map(|(ws, _)| ws)
}

/// Build the "Copy metadata" report for `chip`, in the same key/shape family
/// as the host's own pane "Copy metadata" (harness, runtime, pane id, tab id,
/// workspace, folder) — an unknown fact is omitted rather than printed as
/// "unknown", matching that action's own convention.
fn metadata_text(chip: &Chip) -> String {
    let mut out = String::from("herdr tab metadata\n");
    let mut fields: Vec<(&str, String)> = vec![("harness", chip.name.clone())];
    if let Some(runtime) = &chip.runtime {
        fields.push(("runtime", runtime.clone()));
    }
    if let Some(pane_id) = &chip.pane_id {
        fields.push(("pane", pane_id.clone()));
    }
    if let Some(tab_id) = &chip.tab_id {
        fields.push(("tab", tab_id.clone()));
        if let Some(ws) = workspace_of(tab_id) {
            fields.push(("workspace", ws.to_string()));
        }
    }
    if let Some(folder) = &chip.folder {
        fields.push(("folder", folder.clone()));
    }
    let width = fields.iter().map(|(k, _)| k.len() + 1).max().unwrap_or(0);
    for (key, value) in fields {
        out.push_str(&format!("  {:<width$}  {value}\n", format!("{key}:"), width = width));
    }
    out
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

    if let Some(menu) = *MENU.lock().expect("menu mutex poisoned") {
        return handle_menu_mouse(&input, menu, mouse);
    }

    let hit = chip_at(&input, mouse.column, mouse.row);

    if mouse.kind == MouseEventKind::RightDown {
        let Some(index) = hit else {
            return UiUpdate::default();
        };
        if input.chips.get(index).is_none() {
            return UiUpdate::default();
        }
        *MENU.lock().expect("menu mutex poisoned") = Some(OpenMenu { chip_index: index });
        return render_from(&input);
    }

    if !matches!(mouse.kind, MouseEventKind::LeftDown) {
        // Move, wheel: nothing. The bar is hover-inert, so the mouse crossing
        // it must not repaint the row, must not change its width, and must
        // not move the chips the user is aiming at.
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

/// Mouse handling while the right-click menu is open: left-click on an item
/// runs it, any other left-click dismisses (clicking away is how the host's
/// own context menus close too), and right-click always dismisses rather than
/// re-targeting — a second right-click while a menu is up reads as "never
/// mind", not "open a different one out from under it".
fn handle_menu_mouse(input: &BarInput, menu: OpenMenu, mouse: &MouseEvent) -> UiUpdate {
    if mouse.kind == MouseEventKind::RightDown {
        *MENU.lock().expect("menu mutex poisoned") = None;
        return render_from(input);
    }
    if mouse.kind != MouseEventKind::LeftDown {
        return UiUpdate::default();
    }

    // The chip may have vanished from under the menu (bar refreshed between
    // the frame and the press); close rather than act on a stale target.
    let Some(chip) = input.chips.get(menu.chip_index).cloned() else {
        *MENU.lock().expect("menu mutex poisoned") = None;
        return render_from(input);
    };

    let item = menu_item_at(&chip.name, mouse.column, mouse.row);
    *MENU.lock().expect("menu mutex poisoned") = None;

    match item.map(|i| render::MENU_ITEMS[i]) {
        Some("Copy metadata") => UiUpdate {
            effects: vec![HostEffect::CopyText {
                text: metadata_text(&chip),
            }],
            ..render_from(input)
        },
        Some("Close") => close_chip(input, &chip, menu.chip_index),
        // "Cancel", a miss outside every item, or a click past a chip the bar
        // no longer draws — all the same outcome: close the menu and do
        // nothing else, exactly like clicking away from a host context menu.
        _ => render_from(input),
    }
}

/// Close `chip`'s tab. If it was the CURRENT (focused) chip, also point focus
/// at its parent in the lineage — overriding whatever tab the host would
/// otherwise pick — falling back to the host's own next-tab choice when no
/// parent is known (an ancestor-less root, or a lineage the collector could
/// not resolve).
fn close_chip(input: &BarInput, chip: &Chip, chip_index: usize) -> UiUpdate {
    let Some(tab_id) = chip.tab_id.clone() else {
        return UiUpdate {
            effects: vec![HostEffect::ShowToast {
                message: format!("{} has no tab to close", chip.name),
            }],
            ..render_from(input)
        };
    };

    let mut effects = vec![HostEffect::Api {
        request_id: format!("tidus-bar:close:{chip_index}"),
        method: "tab.close".to_string(),
        params: serde_json::json!({ "tab_id": tab_id }),
    }];
    if chip.role == ChipRole::Current {
        if let Some(parent) = parent_tab_id(&input.chips, chip_index) {
            effects.push(HostEffect::Api {
                request_id: format!("tidus-bar:refocus:{chip_index}"),
                method: "tab.focus".to_string(),
                params: serde_json::json!({ "tab_id": parent }),
            });
        }
    }
    UiUpdate {
        effects,
        ..render_from(input)
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
            display_name: name.to_string(),
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
            folder: None,
        }
    }

    fn seed(input: BarInput) {
        // The band as the host would carve it: origin (0, 0), 200 columns, and
        // exactly as many rows as the bar asked for — an `auto` band is sized
        // from the module's own node count, so the test must not hand the click
        // path more rows than the renderer drew.
        let height = render::content_height(&input).max(1);
        *LAST_INPUT.lock().expect("input mutex poisoned") = Some(input);
        *ROW_RECT.lock().expect("row rect mutex poisoned") = Some((0, 0, 200, height));
    }

    /// Press a chip at the CENTRE of the cells the bar actually drew for it, on
    /// the row it actually drew it on. The click path under test IS the
    /// geometry, so a test that called the handler with a chip id would prove
    /// nothing about a real press.
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
                row: p.row,
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
        let row = p.row;
        let press = |column| {
            handle_mouse(&MouseEvent {
                kind: MouseEventKind::LeftDown,
                column,
                row,
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

    /// The selected node with brothers beside it and children under it — the
    /// shape the redesign is about.
    fn family() -> BarInput {
        BarInput {
            focus_harness: Some("squad".into()),
            chips: vec![
                chip("main", ChipRole::Ancestor),
                chip("squad", ChipRole::Current),
                chip("bro", ChipRole::Sibling),
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

    /// Spec (5): a brother is a click target that re-roots the bar on itself,
    /// exactly like a child. Nothing about the click path may treat "the other
    /// children of my parent" as decoration.
    #[test]
    fn a_left_click_on_a_brother_focuses_it_like_any_other_chip() {
        let _guard = test_guard();
        seed(family());
        let update = click_chip(2, MouseEventKind::LeftDown);
        match update.effects.as_slice() {
            [HostEffect::Api { method, params, .. }] => {
                assert_eq!(method, "tab.focus");
                assert_eq!(params["tab_id"], "w1:t-bro");
            }
            other => panic!("a brother must be clickable, got {other:?}"),
        }
    }

    /// The child row is indented to sit UNDER the selected chip, so a press
    /// must be attributed by ROW as well as column — otherwise a click on a
    /// child opens whichever brother happens to be drawn above it, which is the
    /// worst kind of bug: it navigates somewhere plausible.
    #[test]
    fn a_press_on_the_child_row_never_resolves_to_the_row_above_it() {
        let _guard = test_guard();
        let input = family();
        seed(input.clone());
        let (_, placed) = render::layout(&input).expect("rows");
        let kid = placed.iter().find(|p| p.index == 3).expect("the child");
        assert_eq!(kid.row, 2, "precondition: the child is on the child row");

        // Same column, both rows: two different agents must answer.
        let column = kid.x + kid.width / 2;
        let press = |row| {
            handle_mouse(&MouseEvent {
                kind: MouseEventKind::LeftDown,
                column,
                row,
            })
        };
        let tab_of = |update: &UiUpdate| match update.effects.as_slice() {
            [HostEffect::Api { params, .. }] => Some(params["tab_id"].clone()),
            _ => None,
        };
        assert_eq!(
            tab_of(&press(2)),
            Some(serde_json::json!("w1:t-kid")),
            "the child row must open the child"
        );
        for above in [0u16, 1] {
            assert_ne!(
                tab_of(&press(above)),
                Some(serde_json::json!("w1:t-kid")),
                "the same column on row {above} is a different agent"
            );
        }
    }

    /// A press below the band belongs to the pane, not to the bar. With a
    /// one-row bar this was implied by `row != oy`; with a growable band it has
    /// to survive the height check.
    #[test]
    fn a_press_past_the_bands_last_row_is_not_a_chip() {
        let _guard = test_guard();
        seed(sample());
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: 2,
            row: 9,
        });
        assert!(update.effects.is_empty(), "{:?}", update.effects);
    }

    /// The band costs one row per LEVEL it actually has something to show on,
    /// and no more — the host sizes an `auto` band from the node count, so the
    /// number of emitted `Line` nodes IS the number of pane rows taken.
    #[test]
    fn the_band_costs_one_row_per_level_it_has_something_to_show() {
        let _guard = test_guard();

        // A lone root: nothing to hang from, nothing hanging off it.
        let lone = BarInput {
            chips: vec![chip("main", ChipRole::Current)],
            empty_reason: None,
            ..Default::default()
        };
        let update = handle_event(&frame_event(&lone, Rect::new(0, 30, 120, 1)));
        assert_eq!(line_nodes(&update), 1, "a lone orchestrator costs one row");

        // Lineage + family, but the selection has no children.
        let childless = BarInput {
            chips: vec![
                chip("main", ChipRole::Ancestor),
                chip("squad", ChipRole::Current),
                chip("bro", ChipRole::Sibling),
            ],
            empty_reason: None,
            ..Default::default()
        };
        let update = handle_event(&frame_event(&childless, Rect::new(0, 30, 120, 2)));
        assert_eq!(
            line_nodes(&update),
            2,
            "an empty child row must not be carved"
        );

        // ...and the child row appears only once there is a child on it.
        let update = handle_event(&frame_event(&family(), Rect::new(0, 30, 120, 3)));
        assert_eq!(line_nodes(&update), 3, "children earn the third row");
    }

    /// Closing a brother points focus back at the parent it shares with the
    /// selected node — not at the chip that happens to precede it in the
    /// vector, which for a brother is another brother.
    #[test]
    fn a_brothers_parent_is_the_shared_parent_not_the_previous_chip() {
        let _guard = test_guard();
        let input = family();
        assert_eq!(
            parent_tab_id(&input.chips, 2).as_deref(),
            Some("w1:t-main"),
            "a brother hangs off the last ancestor"
        );
        assert_eq!(
            parent_tab_id(&input.chips, 3).as_deref(),
            Some("w1:t-squad"),
            "a child hangs off the selected node"
        );
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

    /// Canon §1: right-click opens a contextual MENU rather than acting
    /// immediately — no effect fires until an item is picked.
    #[test]
    fn a_right_click_opens_the_menu_without_acting_immediately() {
        let _guard = test_guard();
        seed(sample());
        let update = click_chip(2, MouseEventKind::RightDown);
        assert!(
            update.effects.is_empty(),
            "opening the menu must not act by itself, got {:?}",
            update.effects
        );
        assert!(
            !update.layers.is_empty(),
            "opening the menu must repaint the row to show it"
        );
        assert_eq!(
            *MENU.lock().unwrap(),
            Some(OpenMenu { chip_index: 2 }),
            "the menu must remember which chip it was opened for"
        );
    }

    /// Right-clicking empty band (no chip under the cursor) opens nothing.
    #[test]
    fn a_right_click_off_every_chip_opens_no_menu() {
        let _guard = test_guard();
        seed(sample());
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::RightDown,
            column: 199,
            row: 0,
        });
        assert!(update.effects.is_empty());
        assert!(MENU.lock().unwrap().is_none());
    }

    /// "Copy metadata" reports harness, runtime, pane id, tab id, workspace and
    /// folder — the same field family as the host's own pane "Copy metadata" —
    /// and closes the menu afterward.
    #[test]
    fn copy_metadata_reports_the_chips_own_fields() {
        let _guard = test_guard();
        let mut input = sample();
        input.chips[2].folder = Some("/home/andual/harnesses/kid/workspace".into());
        seed(input);
        let update = click_chip(2, MouseEventKind::RightDown);
        assert!(update.layers.len() <= 1);
        let item = menu_item_column(2, "Copy metadata");
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: item,
            row: 0,
        });
        match update.effects.as_slice() {
            [HostEffect::CopyText { text }] => {
                assert!(text.contains("harness:") && text.contains("kid"), "{text}");
                assert!(text.contains("runtime:") && text.contains("claude"), "{text}");
                assert!(text.contains("pane:") && text.contains("w1:p-kid"), "{text}");
                assert!(text.contains("tab:") && text.contains("w1:t-kid"), "{text}");
                assert!(text.contains("workspace:") && text.contains("w1"), "{text}");
                assert!(text.contains("folder:") && text.contains("kid/workspace"), "{text}");
            }
            other => panic!("expected a CopyText effect, got {other:?}"),
        }
        assert!(MENU.lock().unwrap().is_none(), "the menu closes after acting");
    }

    /// Closing the CURRENT (focused) chip also refocuses its parent — the last
    /// ancestor in the lineage — overriding whatever the host would otherwise
    /// pick next.
    #[test]
    fn closing_the_current_chip_refocuses_its_parent() {
        let _guard = test_guard();
        seed(sample()); // main (ancestor) ▸ squad (current) ▸ kid (child)
        click_chip(1, MouseEventKind::RightDown);
        let item = menu_item_column(1, "Close");
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: item,
            row: 0,
        });
        match update.effects.as_slice() {
            [HostEffect::Api { method: m1, params: p1, .. }, HostEffect::Api { method: m2, params: p2, .. }] =>
            {
                assert_eq!(m1, "tab.close");
                assert_eq!(p1["tab_id"], "w1:t-squad");
                assert_eq!(m2, "tab.focus");
                assert_eq!(p2["tab_id"], "w1:t-main", "focus returns to the parent");
            }
            other => panic!("expected close then refocus, got {other:?}"),
        }
    }

    /// Closing a CHILD (not the focused chip) closes it without touching focus
    /// — nothing on the bar claims it was the tab on screen.
    #[test]
    fn closing_a_child_chip_does_not_refocus() {
        let _guard = test_guard();
        seed(sample());
        click_chip(2, MouseEventKind::RightDown); // "kid", a Child
        let item = menu_item_column(2, "Close");
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: item,
            row: 0,
        });
        match update.effects.as_slice() {
            [HostEffect::Api { method, params, .. }] => {
                assert_eq!(method, "tab.close");
                assert_eq!(params["tab_id"], "w1:t-kid");
            }
            other => panic!("expected exactly one close effect, got {other:?}"),
        }
    }

    /// A root chip with no ancestor at all (nothing upstream) closes with no
    /// parent to refocus — the host's own next-tab choice applies.
    #[test]
    fn closing_a_rootless_current_chip_has_nothing_to_refocus() {
        let _guard = test_guard();
        seed(BarInput {
            chips: vec![chip("main", ChipRole::Current)],
            ..Default::default()
        });
        click_chip(0, MouseEventKind::RightDown);
        let item = menu_item_column(0, "Close");
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: item,
            row: 0,
        });
        match update.effects.as_slice() {
            [HostEffect::Api { method, .. }] => assert_eq!(method, "tab.close"),
            other => panic!("expected exactly one close effect, got {other:?}"),
        }
    }

    /// A chip with no tab (a `light` child, or one the reaper took) has nothing
    /// to close, so "Close" refuses out loud instead of sending a doomed
    /// `tab.close`.
    #[test]
    fn closing_a_chip_with_no_tab_reports_instead() {
        let _guard = test_guard();
        let mut input = sample();
        input.chips[2].tab_id = None;
        seed(input);
        click_chip(2, MouseEventKind::RightDown);
        let item = menu_item_column(2, "Close");
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: item,
            row: 0,
        });
        match update.effects.as_slice() {
            [HostEffect::ShowToast { message }] => {
                assert!(message.contains("no tab to close"), "{message}");
            }
            other => panic!("expected a report, got {other:?}"),
        }
    }

    /// "Cancel", and a click outside every item, both just close the menu.
    #[test]
    fn cancel_and_a_miss_both_just_close_the_menu() {
        let _guard = test_guard();
        seed(sample());

        click_chip(2, MouseEventKind::RightDown);
        let cancel = menu_item_column(2, "Cancel");
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: cancel,
            row: 0,
        });
        assert!(update.effects.is_empty());
        assert!(MENU.lock().unwrap().is_none());

        click_chip(2, MouseEventKind::RightDown);
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: 199,
            row: 0,
        });
        assert!(update.effects.is_empty());
        assert!(MENU.lock().unwrap().is_none());
    }

    /// Right-clicking again while a menu is open dismisses it rather than
    /// re-targeting a different chip.
    #[test]
    fn a_second_right_click_dismisses_the_open_menu() {
        let _guard = test_guard();
        seed(sample());
        click_chip(2, MouseEventKind::RightDown);
        assert!(MENU.lock().unwrap().is_some());
        let update = click_chip(0, MouseEventKind::RightDown);
        assert!(update.effects.is_empty());
        assert!(MENU.lock().unwrap().is_none());
    }

    /// The column of a menu item, for `chip_index`'s menu — mirrors
    /// `render::menu_layout` exactly, so a click at this column is what a real
    /// press on that label looks like.
    fn menu_item_column(chip_index: usize, label: &str) -> u16 {
        let input = LAST_INPUT
            .lock()
            .expect("input mutex poisoned")
            .clone()
            .expect("seeded");
        let chip = &input.chips[chip_index];
        let (_, placed) = render::menu_layout(&chip.name);
        let item_index = render::MENU_ITEMS
            .iter()
            .position(|l| *l == label)
            .expect("label is a real menu item");
        placed
            .iter()
            .find(|p| p.index == item_index)
            .expect("item placed")
            .x
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

    /// How many `Line` nodes the update carries — which is exactly how the host
    /// measures an `auto` band's height (`ui.rs::panel_content_dims`), so this
    /// is the bar's real cost in pane rows.
    fn line_nodes(update: &UiUpdate) -> usize {
        update
            .layers
            .iter()
            .flat_map(|layer| layer.nodes.iter())
            .filter(|node| matches!(node, herdr_ui_plugin_api::UiNode::Line { .. }))
            .count()
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
        *ROW_RECT.lock().expect("row rect mutex poisoned") =
            Some((rect.x, rect.y, rect.width, render::content_height(&input)));
        let (_, placed) = render::layout(&input).expect("row");
        let p = placed.iter().find(|p| p.index == 2).expect("chip");
        let update = handle_mouse(&MouseEvent {
            kind: MouseEventKind::LeftDown,
            column: rect.x + p.x + p.width / 2,
            row: rect.y + p.row,
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
