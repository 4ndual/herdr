//! Harness-manager guest: a pure right-panel renderer of the host-provided
//! harness inspection model (mirrors the historial-dash wave-1 pattern — the
//! host projects a fully-precomputed model into `harness_input.json` each tick
//! and the guest just draws it).
//!
//! Interaction split:
//! - The header collapse toggle, hover tooltip, and right-click disable menu are
//!   guest-owned.
//! - The open / disable-option hit-rects are declared here but RESOLVED
//!   host-side (the host owns pane spawning and the Claude settings files), so
//!   the guest never handles those left-clicks.

mod render;

use harness_manager_model::HarnessRenderModel;
use herdr_ui_plugin_api::{
    HostEffect, MouseEvent, MouseEventKind, UiActionKind, UiEvent, UiFrameContext, UiLayer, UiNode,
    UiSurface, UiUpdate, UiUpdateScope,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Guest-owned collapse state, re-synced from the host's carved rect width in
/// `handle_frame` so host state always wins.
static COLLAPSED: AtomicBool = AtomicBool::new(false);

/// Per-section collapsed state, keyed by section TITLE so it survives row
/// churn and model refreshes (R14). A title present here renders that section
/// collapsed (header + aggregate only). Toggled by clicking a section header;
/// cleared only on target change.
static COLLAPSED_SECTIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Section titles whose `collapsed_default` has already been applied once, so a
/// user expand is not undone by the default on the next refresh.
static SEEDED_SECTIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Most recent render model, cached so a raw mouse `Move` (which carries no
/// input) can re-render the hover tooltip against the same rows.
static MODEL: Mutex<Option<HarnessRenderModel>> = Mutex::new(None);
/// Absolute carved panel rect (x, y, width, height) from the last frame.
static AREA: Mutex<Option<(u16, u16, u16, u16)>> = Mutex::new(None);
/// Index (into `render::body_lines`) of the hovered line, or `None`.
static HOVERED: Mutex<Option<usize>> = Mutex::new(None);
/// Open right-click disable menu, if any. Cleared on cancel, on a new
/// right-click, and whenever the model changes (a disable removed the row).
static MENU: Mutex<Option<render::MenuState>> = Mutex::new(None);
/// Body scroll offset (first visible body line index). Adjusted by the mouse
/// wheel over the panel, clamped to the clipped range, and reset to 0 whenever
/// the focused target changes.
static SCROLL: AtomicUsize = AtomicUsize::new(0);
/// Body lines scrolled per wheel notch.
const SCROLL_STEP: usize = 3;

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
        UiEvent::Mouse(mouse) => handle_mouse(mouse),
        UiEvent::Action {
            action_id,
            kind,
            row,
            ..
        } => handle_action(action_id, *kind, *row),
        _ => UiUpdate::default(),
    }
}

/// Resolve the absolute body-line index a screen `(col, row)` is over, given the
/// cached panel rect and current scroll offset. Body lines start at `y + 2`
/// (header + target lines); the on-screen row maps to `scroll + (row - first)`.
fn line_at(
    area: (u16, u16, u16, u16),
    lines: usize,
    scroll: usize,
    col: u16,
    row: u16,
) -> Option<usize> {
    let (x, y, width, height) = area;
    if width <= 3 || height <= 2 {
        return None;
    }
    if col < x || col >= x.saturating_add(width) {
        return None;
    }
    let first = y + 2;
    if row < first {
        return None;
    }
    let on_screen = (row - first) as usize;
    let max_lines = (height - 2) as usize;
    if on_screen >= max_lines {
        return None;
    }
    let idx = scroll + on_screen;
    (idx < lines).then_some(idx)
}

/// Snapshot of the collapsed section titles for passing into the pure renderer.
fn collapsed_snapshot() -> Vec<String> {
    COLLAPSED_SECTIONS
        .lock()
        .expect("collapsed mutex poisoned")
        .clone()
}

/// Clamp the raw scroll offset to the clipped range for the current model/area.
fn clamped_scroll(model: &HarnessRenderModel, height: u16) -> usize {
    let visible = (height as usize).saturating_sub(2);
    let collapsed = collapsed_snapshot();
    let max_scroll = render::body_lines(model, &collapsed)
        .len()
        .saturating_sub(visible);
    SCROLL.load(Ordering::SeqCst).min(max_scroll)
}

/// Re-render the right-panel layer from the cached model/area/hover/menu.
fn render_from_cache() -> Option<UiUpdate> {
    let model_guard = MODEL.lock().expect("model mutex poisoned");
    let model = model_guard.as_ref()?;
    let (x, y, width, height) = (*AREA.lock().expect("area mutex poisoned"))?;
    if width <= 3 || height == 0 {
        return None;
    }
    let area = Rect::new(x, y, width, height);
    let hovered = *HOVERED.lock().expect("hover mutex poisoned");
    let menu = MENU.lock().expect("menu mutex poisoned").clone();
    let scroll = clamped_scroll(model, height);
    let collapsed = collapsed_snapshot();
    let mut buf = Buffer::empty(area);
    let hits = render::render(model, area, &mut buf, hovered, menu.as_ref(), scroll, &collapsed);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);
    Some(UiUpdate {
        layers: vec![right_panel_layer(nodes, hits)],
        effects: Vec::new(),
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    })
}

/// Raw mouse handling (`mouse_all`): only `Move` matters — it updates the
/// hovered line so the tooltip follows the cursor. Every branch re-renders from
/// cache; returning an empty update would blank the panel and drop its declared
/// hit-rects.
fn handle_mouse(mouse: &MouseEvent) -> UiUpdate {
    match mouse.kind {
        MouseEventKind::Move => {
            let area = *AREA.lock().expect("area mutex poisoned");
            let (lines, scroll) = MODEL
                .lock()
                .expect("model mutex poisoned")
                .as_ref()
                .map(|m| {
                    let height = area.map(|(_, _, _, h)| h).unwrap_or(0);
                    let collapsed = collapsed_snapshot();
                    (
                        render::body_lines(m, &collapsed).len(),
                        clamped_scroll(m, height),
                    )
                })
                .unwrap_or((0, 0));
            let next = area.and_then(|a| line_at(a, lines, scroll, mouse.column, mouse.row));
            let mut guard = HOVERED.lock().expect("hover mutex poisoned");
            *guard = next;
        }
        MouseEventKind::ScrollDown => adjust_scroll(SCROLL_STEP as isize),
        MouseEventKind::ScrollUp => adjust_scroll(-(SCROLL_STEP as isize)),
        _ => {}
    }
    render_from_cache().unwrap_or_default()
}

/// Shift the scroll offset by `delta` lines, clamped to the clipped range for
/// the current model/area. No-op when there is nothing cached to scroll over.
fn adjust_scroll(delta: isize) {
    let height = AREA
        .lock()
        .expect("area mutex poisoned")
        .map(|(_, _, _, h)| h)
        .unwrap_or(0);
    let max_scroll = MODEL
        .lock()
        .expect("model mutex poisoned")
        .as_ref()
        .map(|m| {
            let visible = (height as usize).saturating_sub(2);
            let collapsed = collapsed_snapshot();
            render::body_lines(m, &collapsed)
                .len()
                .saturating_sub(visible)
        })
        .unwrap_or(0);
    let cur = SCROLL.load(Ordering::SeqCst) as isize;
    let next = cur.saturating_add(delta).clamp(0, max_scroll as isize) as usize;
    SCROLL.store(next, Ordering::SeqCst);
}

fn parse_model(ctx: &UiFrameContext) -> Option<HarnessRenderModel> {
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

    // Cache the model + carved rect; a model change closes the menu (the row
    // may be gone after a disable) and clamps a stale hover index.
    {
        let mut cached = MODEL.lock().expect("model mutex poisoned");
        let target_changed = cached
            .as_ref()
            .map(|m| m.target != model.target)
            .unwrap_or(true);
        if cached.as_ref() != Some(&model) {
            *MENU.lock().expect("menu mutex poisoned") = None;
        }
        // A new focused target resets the scroll to the top AND clears the
        // per-section collapse/seed state — that state belongs to the previous
        // target's sections. Refreshes of the SAME target preserve it (R14).
        if target_changed {
            SCROLL.store(0, Ordering::SeqCst);
            COLLAPSED_SECTIONS
                .lock()
                .expect("collapsed mutex poisoned")
                .clear();
            SEEDED_SECTIONS
                .lock()
                .expect("seeded mutex poisoned")
                .clear();
        }
        *cached = Some(model.clone());
    }
    // Apply each section's `collapsed_default` exactly once (first time its
    // title is seen for this target); a later user toggle is never re-defaulted.
    seed_collapsed_defaults(&model);
    *AREA.lock().expect("area mutex poisoned") = Some((area.x, area.y, area.width, area.height));
    {
        let mut hov = HOVERED.lock().expect("hover mutex poisoned");
        if let Some(i) = *hov {
            // Hover holds an absolute body-line index; drop it if it no longer
            // exists in the (possibly shrunk) view.
            let collapsed = collapsed_snapshot();
            if i >= render::body_lines(&model, &collapsed).len() {
                *hov = None;
            }
        }
    }

    // Zero rect: emit an invisible placeholder line (so the rail keeps this
    // plugin's icon and the carve stays) ONLY when there is something to show;
    // an empty model requests no carve at all.
    if area.width == 0 || area.height == 0 {
        if model.is_empty() {
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

    // A carved-down 3-col handle rect always means collapsed. When the panel
    // re-expands from the collapsed handle (width ≤3 → >3), reset the scroll to
    // the top: a stale offset left over from before the collapse would otherwise
    // persist and strand the body a few rows below the top after re-opening.
    let now_collapsed = area.width <= 3;
    let was_collapsed = COLLAPSED.swap(now_collapsed, Ordering::SeqCst);
    if was_collapsed && !now_collapsed {
        SCROLL.store(0, Ordering::SeqCst);
    }

    render_from_cache().unwrap_or_default()
}

/// Apply each section's `collapsed_default` the first time its title is seen
/// (per target). Idempotent: a title already in `SEEDED_SECTIONS` is skipped, so
/// a user's later expand/collapse is never overridden by the default (R14).
fn seed_collapsed_defaults(model: &HarnessRenderModel) {
    let mut seeded = SEEDED_SECTIONS.lock().expect("seeded mutex poisoned");
    let mut collapsed = COLLAPSED_SECTIONS.lock().expect("collapsed mutex poisoned");
    for section in &model.sections {
        if seeded.iter().any(|t| t == &section.title) {
            continue;
        }
        seeded.push(section.title.clone());
        if section.collapsed_default && !collapsed.iter().any(|t| t == &section.title) {
            collapsed.push(section.title.clone());
        }
    }
}

/// Toggle the collapsed state of the section titled `title`.
fn toggle_section(title: &str) {
    let mut collapsed = COLLAPSED_SECTIONS.lock().expect("collapsed mutex poisoned");
    if let Some(pos) = collapsed.iter().position(|t| t == title) {
        collapsed.remove(pos);
    } else {
        collapsed.push(title.to_string());
    }
}

/// Extract the row key from a row-region action id (either the openable or the
/// menu-only prefix), or `None` for other ids.
fn row_key(action_id: &str) -> Option<&str> {
    action_id
        .strip_prefix(render::ACTION_OPEN_PREFIX)
        .or_else(|| action_id.strip_prefix(render::ACTION_ROW_PREFIX))
}

/// Whether the model row with `key` offers the disable menu.
fn key_can_disable(key: &str) -> bool {
    row_flag(key, |r| r.can_disable)
}

/// Whether the model row with `key` resolves to a file. Reuses `openable`,
/// which is exactly "the host can turn this key into a path" — the same
/// precondition "copy path" needs, since the guest never sees a path itself.
fn key_can_copy(key: &str) -> bool {
    row_flag(key, |r| r.openable)
}

fn row_flag(key: &str, pick: impl Fn(&harness_manager_model::HarnessRow) -> bool) -> bool {
    MODEL
        .lock()
        .expect("model mutex poisoned")
        .as_ref()
        .map(|m| {
            m.sections
                .iter()
                .flat_map(|s| s.rows.iter())
                .any(|r| r.key == key && pick(r))
        })
        .unwrap_or(false)
}

fn handle_action(action_id: &str, kind: UiActionKind, row: u16) -> UiUpdate {
    // Right-click on a disableable row opens the guest disable menu. The host
    // does not intercept RightDown, so it routes here as a v3 Action.
    if kind == UiActionKind::RightDown {
        if let Some(key) = row_key(action_id) {
            let can_disable = key_can_disable(key);
            let can_copy = key_can_copy(key);
            // A row earns a menu if it offers anything beyond cancel. Openable
            // rows now qualify on their own, so a file row that cannot be
            // disabled still gets "copy path".
            if can_disable || can_copy {
                *MENU.lock().expect("menu mutex poisoned") = Some(render::MenuState {
                    key: key.to_string(),
                    anchor_y: row,
                    can_copy,
                    can_disable,
                });
                return render_from_cache().unwrap_or_default();
            }
        }
        return UiUpdate::default();
    }
    // Act on press only; the host routes both left_down and left_up for one
    // physical click, and reacting to both would double-fire.
    if kind != UiActionKind::LeftDown {
        return UiUpdate::default();
    }
    if action_id == render::ACTION_MENU_CANCEL {
        *MENU.lock().expect("menu mutex poisoned") = None;
        return render_from_cache().unwrap_or_default();
    }
    // Clicking a section header toggles that section collapsed/expanded. The
    // host does not resolve `harness:section:` ids (see `parse_harness_action`),
    // so they route here as guest-owned v3 Actions.
    if let Some(title) = action_id.strip_prefix(render::ACTION_SECTION_PREFIX) {
        toggle_section(title);
        return render_from_cache().unwrap_or_default();
    }
    // Open / disable-option left-clicks are resolved host-side and never reach
    // the guest; the only remaining guest-owned left-click is the collapse
    // toggle.
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
    use harness_manager_model::{HarnessRow, HarnessSection, RowState};

    /// Tests share the module's global view-state statics (SCROLL, COLLAPSED,
    /// MODEL, …); serialize every test that drives `handle_event` so parallel
    /// runs can't interleave frames and flake each other's assertions.
    static TEST_STATE_GUARD: Mutex<()> = Mutex::new(());

    fn state_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_STATE_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Hermetic start: clear every view-state static so a test never
        // observes state leaked by whichever test happened to run before it.
        *MODEL.lock().expect("model mutex poisoned") = None;
        *MENU.lock().expect("menu mutex poisoned") = None;
        *HOVERED.lock().expect("hover mutex poisoned") = None;
        *AREA.lock().expect("area mutex poisoned") = None;
        COLLAPSED_SECTIONS
            .lock()
            .expect("collapsed mutex poisoned")
            .clear();
        SEEDED_SECTIONS.lock().expect("seeded mutex poisoned").clear();
        COLLAPSED.store(false, Ordering::SeqCst);
        SCROLL.store(0, Ordering::SeqCst);
        guard
    }

    fn frame_json(env: &str, width: u16) -> String {
        format!(
            "{{\"type\":\"frame\",\"surface\":\"right_panel\",\
             \"rect\":{{\"x\":0,\"y\":0,\"width\":{width},\"height\":16}},\
             \"focused\":false,\
             \"host\":{{\"focus\":{{\"session_id\":null,\"session_ref\":null,\
             \"cwd\":null,\"agent\":\"None\",\"is_self_focus\":false}},\
             \"env\":{env}}}}}"
        )
    }

    fn action_json(action_id: &str, kind: &str, row: u16) -> String {
        format!(
            "{{\"type\":\"action\",\"action_id\":\"{action_id}\",\
             \"surface\":\"right_panel\",\"column\":1,\"row\":{row},\"kind\":\"{kind}\"}}"
        )
    }

    fn test_model() -> HarnessRenderModel {
        HarnessRenderModel {
            target: "claude · abcd1234 · herdr".to_string(),
            sections: vec![HarnessSection {
                title: "HOOKS".to_string(),
                summary: String::new(),
                agg: None,
                collapsed_default: false,
                rows: vec![HarnessRow {
                    key: "hook:0".to_string(),
                    label: "PreToolUse pkg-guard".to_string(),
                    meta: "x2".to_string(),
                    state: RowState::Idle,
                    tooltip: "tip".to_string(),
                    openable: true,
                    can_disable: true,
                    indent: 0,
                    ..Default::default()
                }],
            }],
        }
    }

    fn model_env(model: &HarnessRenderModel) -> String {
        let model_json = serde_json::to_string(model).expect("model serializes");
        format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&model_json).expect("env value serializes")
        )
    }

    #[test]
    fn renders_right_panel_from_input() {
        let _state = state_guard();
        let event: UiEvent =
            serde_json::from_str(&frame_json(&model_env(&test_model()), 44)).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::RightPanel));
        assert!(update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id == "harness:open:hook:0"));
    }

    #[test]
    fn right_click_on_row_opens_menu_with_disable_hits() {
        let _state = state_guard();
        // Seed the model cache with a frame first.
        let frame: UiEvent =
            serde_json::from_str(&frame_json(&model_env(&test_model()), 44)).expect("frame parses");
        let _ = handle_event(&frame);
        let event: UiEvent =
            serde_json::from_str(&action_json("harness:open:hook:0", "right_down", 3))
                .expect("action parses");
        let update = handle_event(&event);
        assert!(update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id == "harness:disable_session:hook:0"));
        assert!(update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id == "harness:disable_profile:hook:0"));
        // Cancel closes it again.
        let cancel: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_MENU_CANCEL, "left_down", 4))
                .expect("action parses");
        let update = handle_event(&cancel);
        assert!(!update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id.starts_with("harness:disable_")));
    }

    #[test]
    fn missing_input_renders_nothing() {
        let _state = state_guard();
        let event: UiEvent = serde_json::from_str(&frame_json("{}", 44)).expect("frame parses");
        assert!(handle_event(&event).layers.is_empty());
    }

    #[test]
    fn toggle_left_down_emits_set_surface_collapsed() {
        let _state = state_guard();
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_PANEL, "left_down", 0))
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
    fn line_at_maps_screen_row_to_scrolled_body_index() {
        // area (x=0,y=0,w=44,h=6): body starts at screen row 2, 4 visible rows.
        let area = (0u16, 0u16, 44u16, 6u16);
        // No scroll: screen row 2 is body line 0.
        assert_eq!(line_at(area, 10, 0, 1, 2), Some(0));
        assert_eq!(line_at(area, 10, 0, 1, 5), Some(3));
        // Scrolled by 2: screen row 2 now maps to body line 2.
        assert_eq!(line_at(area, 10, 2, 1, 2), Some(2));
        assert_eq!(line_at(area, 10, 2, 1, 5), Some(5));
        // Past the last real body line resolves to None even if on-screen.
        assert_eq!(line_at(area, 4, 2, 1, 5), None);
        // Above the body and past the visible window are both None.
        assert_eq!(line_at(area, 10, 0, 1, 1), None);
        assert_eq!(line_at(area, 10, 0, 1, 6), None);
    }

    #[test]
    fn scroll_down_then_reset_on_target_change() {
        let _state = state_guard();
        let frame: UiEvent =
            serde_json::from_str(&frame_json(&model_env(&test_model()), 44)).expect("frame parses");
        let _ = handle_event(&frame);
        // Wheel down bumps the offset (clamped to the model's range; the tiny
        // test model may clamp to 0, so assert the reset path directly instead).
        adjust_scroll(5);
        SCROLL.store(3, Ordering::SeqCst);
        // A frame whose target changed resets scroll to 0.
        let mut changed = test_model();
        changed.target = "claude · ffffffff · other".to_string();
        let frame2: UiEvent =
            serde_json::from_str(&frame_json(&model_env(&changed), 44)).expect("frame parses");
        let _ = handle_event(&frame2);
        assert_eq!(SCROLL.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reexpand_from_collapsed_handle_resets_scroll_to_top() {
        let _state = state_guard();
        // Expanded frame seeds the model (target set → SCROLL already 0).
        let expanded =
            serde_json::from_str::<UiEvent>(&frame_json(&model_env(&test_model()), 44))
                .expect("frame parses");
        let _ = handle_event(&expanded);
        // Collapse to the 3-col handle, then scroll the (still-cached) body.
        let collapsed =
            serde_json::from_str::<UiEvent>(&frame_json(&model_env(&test_model()), 3))
                .expect("frame parses");
        let _ = handle_event(&collapsed);
        SCROLL.store(4, Ordering::SeqCst);
        // Re-expand: the width ≤3 → >3 transition resets the offset to the top.
        let reexpanded =
            serde_json::from_str::<UiEvent>(&frame_json(&model_env(&test_model()), 44))
                .expect("frame parses");
        let _ = handle_event(&reexpanded);
        assert_eq!(SCROLL.load(Ordering::SeqCst), 0, "re-expand resets to top");
    }

    #[test]
    fn open_left_down_is_not_handled_by_guest() {
        let _state = state_guard();
        let event: UiEvent =
            serde_json::from_str(&action_json("harness:open:ctx:0", "left_down", 3))
                .expect("action parses");
        let update = handle_event(&event);
        assert!(update.effects.is_empty());
    }
}
