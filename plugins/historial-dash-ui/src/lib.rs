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
    HostEffect, MouseEvent, MouseEventKind, UiActionKind, UiEvent, UiFrameContext, UiLayer, UiNode,
    UiSurface, UiUpdate, UiUpdateScope,
};
use historial_dash_model::HistorialRenderModel;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use render::{AgentFilter, HeaderButton, RowFilters};

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// The header button the cursor is currently over (drives the hover-hint line),
/// or `None`. Mutually exclusive with [`HOVERED`]: a move over a header button
/// clears the row hover and vice-versa.
static HEADER_HOVER: Mutex<Option<HeaderButton>> = Mutex::new(None);

/// Guest-owned collapse state, toggled by clicks on `render::ACTION_TOGGLE_PANEL`.
/// Re-synced from the host's carved rect width in `handle_frame` so host state
/// (a restart, or a rail collapse) always wins.
static COLLAPSED: AtomicBool = AtomicBool::new(false);

/// Most recent render model, cached so a raw mouse `Move` (which carries no
/// input) can re-render the hover tooltip against the same rows the last frame
/// drew.
static MODEL: Mutex<Option<HistorialRenderModel>> = Mutex::new(None);
/// Absolute carved panel rect (x, y, width, height) from the last frame — the
/// anchor for resolving which row an absolute mouse coordinate is over.
static AREA: Mutex<Option<(u16, u16, u16, u16)>> = Mutex::new(None);
/// Index of the row whose tooltip is currently shown, or `None`.
static HOVERED: Mutex<Option<usize>> = Mutex::new(None);
/// Whether plain terminal/shell rows are listed. Default off (agent rows only);
/// toggled by clicks on `render::ACTION_TOGGLE_TERMINALS`. Guest-persistent in
/// memory (resets to the default on reload, which is acceptable).
static SHOW_TERMINALS: AtomicBool = AtomicBool::new(false);
/// Whether the legend/help overlay is shown. Default off; toggled by clicks on
/// `render::ACTION_TOGGLE_LEGEND`. Guest-persistent in memory.
static SHOW_LEGEND: AtomicBool = AtomicBool::new(false);
/// Monotonic per-agent filter click counter; `AgentFilter::from_index` maps it to
/// the current selection (all/claude/cx/codex/pi). Advanced by clicks on
/// `render::ACTION_CYCLE_AGENT`.
static AGENT_FILTER_INDEX: AtomicUsize = AtomicUsize::new(0);

/// The composed header filter state read from the guest's static flags.
fn current_filters() -> RowFilters {
    RowFilters {
        show_terminals: SHOW_TERMINALS.load(Ordering::SeqCst),
        agent: AgentFilter::from_index(AGENT_FILTER_INDEX.load(Ordering::SeqCst)),
        show_legend: SHOW_LEGEND.load(Ordering::SeqCst),
    }
}

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
        UiEvent::Mouse(mouse) => handle_mouse(mouse),
        UiEvent::Action {
            action_id, kind, ..
        } => handle_action(action_id, *kind),
        _ => UiUpdate::default(),
    }
}

/// Resolve the row index an absolute `(col, row)` is over, given the cached panel
/// rect and the visible row count, or `None` when off the list.
fn hovered_row_at(area: (u16, u16, u16, u16), rows: usize, col: u16, row: u16) -> Option<usize> {
    let (x, y, width, height) = area;
    if width <= 3 || height <= 1 {
        return None;
    }
    if col < x || col >= x.saturating_add(width) {
        return None;
    }
    let first_row_y = y + 1;
    if row < first_row_y {
        return None;
    }
    let idx = (row - first_row_y) as usize;
    let max_rows = (height - 1) as usize;
    let visible = rows.min(max_rows);
    (idx < visible).then_some(idx)
}

/// Re-render the right-panel layer from the cached model/area/hover. Used by the
/// raw mouse path, which has no frame rect of its own. Returns `None` when there
/// is nothing carved to draw into (so the caller falls back to an empty update).
fn render_from_cache() -> Option<UiUpdate> {
    let model_guard = MODEL.lock().expect("model mutex poisoned");
    let model = model_guard.as_ref()?;
    let (x, y, width, height) = (*AREA.lock().expect("area mutex poisoned"))?;
    if width <= 3 || height == 0 {
        return None;
    }
    let area = Rect::new(x, y, width, height);
    let hovered = *HOVERED.lock().expect("hover mutex poisoned");
    let header_hover = *HEADER_HOVER.lock().expect("header hover mutex poisoned");
    let filters = current_filters();
    let mut buf = Buffer::empty(area);
    let hits = render::render(model, area, &mut buf, hovered, filters, header_hover);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);
    Some(UiUpdate {
        layers: vec![right_panel_layer(nodes, hits)],
        effects: Vec::new(),
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    })
}

/// Raw mouse handling (requires the `mouse_all` capability, which delivers every
/// mouse event outside this module's own hit-rects, plus `Move` over them). Only
/// `Move` matters: it updates the hovered row so the tooltip follows the cursor.
/// Clicks are owned by the host-resolved hit-rects (restore/fork/delete) and are
/// ignored here. Every branch re-renders the current layer from cache — returning
/// an empty update would blank the panel (and, in the click case, replace its
/// declared restore/fork/delete hit-rects), so we never do that.
fn handle_mouse(mouse: &MouseEvent) -> UiUpdate {
    if mouse.kind == MouseEventKind::Move {
        let area = *AREA.lock().expect("area mutex poisoned");
        // A move over a header button shows that button's hint and suppresses any
        // row tooltip; otherwise resolve the hovered row as before.
        let header = area.and_then(|(x, y, w, h)| {
            render::header_button_at(Rect::new(x, y, w, h), mouse.column, mouse.row)
        });
        if let Some(btn) = header {
            *HEADER_HOVER.lock().expect("header hover mutex poisoned") = Some(btn);
            *HOVERED.lock().expect("hover mutex poisoned") = None;
        } else {
            *HEADER_HOVER.lock().expect("header hover mutex poisoned") = None;
            // Hover maps against the VISIBLE (filtered) rows, matching what the
            // renderer draws under the show-terminals toggle.
            let filters = current_filters();
            let rows = MODEL
                .lock()
                .expect("model mutex poisoned")
                .as_ref()
                .map(|m| render::visible_rows(m, filters).len())
                .unwrap_or(0);
            let next = area.and_then(|a| hovered_row_at(a, rows, mouse.column, mouse.row));
            *HOVERED.lock().expect("hover mutex poisoned") = next;
        }
    }
    // Re-render the current panel (same content when hover is unchanged, so the
    // host sees no diff and nothing is clobbered). Falls back to an empty update
    // only when there is no carved panel to draw (nothing to blank).
    render_from_cache().unwrap_or_default()
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

    // Cache the model + carved rect so a later raw `Move` (no input, no rect) can
    // re-render the tooltip. Clamp any stale hover against the new row count.
    *MODEL.lock().expect("model mutex poisoned") = Some(model.clone());
    *AREA.lock().expect("area mutex poisoned") =
        Some((area.x, area.y, area.width, area.height));
    {
        let mut hov = HOVERED.lock().expect("hover mutex poisoned");
        if let Some(i) = *hov {
            let filters = current_filters();
            let max_rows = area.height.saturating_sub(1) as usize;
            let visible = render::visible_rows(&model, filters).len().min(max_rows);
            if i >= visible {
                *hov = None;
            }
        }
    }

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

    let hovered = *HOVERED.lock().expect("hover mutex poisoned");
    let header_hover = *HEADER_HOVER.lock().expect("header hover mutex poisoned");
    let filters = current_filters();
    let mut buf = Buffer::empty(area);
    let hits = render::render(&model, area, &mut buf, hovered, filters, header_hover);
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
    // The collapse toggle and the terminals toggle are guest-owned;
    // restore/fork/delete are resolved host-side and never reach the guest.
    if action_id == render::ACTION_TOGGLE_TERMINALS {
        let show = !SHOW_TERMINALS.load(Ordering::SeqCst);
        SHOW_TERMINALS.store(show, Ordering::SeqCst);
        // A stale hover index may now point past the newly filtered list; drop it
        // and let the next frame/move re-resolve it.
        *HOVERED.lock().expect("hover mutex poisoned") = None;
        // Re-render immediately with the new filter (falls back to empty only when
        // there is no carved panel, which never happens on a header click).
        return render_from_cache().unwrap_or_default();
    }
    if action_id == render::ACTION_TOGGLE_LEGEND {
        let show = !SHOW_LEGEND.load(Ordering::SeqCst);
        SHOW_LEGEND.store(show, Ordering::SeqCst);
        *HOVERED.lock().expect("hover mutex poisoned") = None;
        return render_from_cache().unwrap_or_default();
    }
    if action_id == render::ACTION_CYCLE_AGENT {
        // Advance the per-agent filter one step (all -> claude -> cx -> ... -> all).
        AGENT_FILTER_INDEX.fetch_add(1, Ordering::SeqCst);
        *HOVERED.lock().expect("hover mutex poisoned") = None;
        return render_from_cache().unwrap_or_default();
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

    fn model_env(rows: usize) -> String {
        let mut model = HistorialRenderModel::default();
        for i in 0..rows {
            model.rows.push(historial_dash_model::HistorialRow {
                key: format!("{i}:w_1_p_{i}"),
                agent: "claude".to_string(),
                label: "herdr".to_string(),
                sid4: "c123".to_string(),
                reltime: "5m".to_string(),
                cwd: "/tmp".to_string(),
                restorable: true,
                workspace_name: "myws".to_string(),
                first_prompt: "first".to_string(),
                last_prompt: "last".to_string(),
            is_subagent: false,
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
    fn toggle_terminals_action_flips_state() {
        let before = SHOW_TERMINALS.load(Ordering::SeqCst);
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_TERMINALS, "left_down"))
                .expect("action parses");
        let _ = handle_event(&event);
        assert_ne!(SHOW_TERMINALS.load(Ordering::SeqCst), before);
        // Flip back so the process-global default is left as found.
        let _ = handle_event(&event);
        assert_eq!(SHOW_TERMINALS.load(Ordering::SeqCst), before);
    }

    #[test]
    fn toggle_legend_action_flips_state() {
        let before = SHOW_LEGEND.load(Ordering::SeqCst);
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_LEGEND, "left_down"))
                .expect("action parses");
        let _ = handle_event(&event);
        assert_ne!(SHOW_LEGEND.load(Ordering::SeqCst), before);
        let _ = handle_event(&event);
        assert_eq!(SHOW_LEGEND.load(Ordering::SeqCst), before);
    }

    #[test]
    fn cycle_agent_action_advances_index() {
        let before = AGENT_FILTER_INDEX.load(Ordering::SeqCst);
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_CYCLE_AGENT, "left_down"))
                .expect("action parses");
        let _ = handle_event(&event);
        assert_eq!(AGENT_FILTER_INDEX.load(Ordering::SeqCst), before + 1);
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
