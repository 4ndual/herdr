//! Detached-sessions dash (guest wasm, API v3).
//!
//! Renders a compact list of LIVE external tmux sessions that hold detached
//! herdr conversations (the `herdr-detached-*` sessions created by the Detach
//! button / ⌃⌥D). The list is produced out-of-process by `scripts/collect-detached.sh`
//! (declared as the module `collector`); the host writes its snapshot into this
//! module's `ui_input_file` and injects it here as `HERDR_PLUGIN_UI_INPUT_JSON`,
//! so a tmux session that dies disappears from the list within one poll.
//!
//! Interaction (mouse-first): left- OR right-clicking an entry opens a 2-item
//! menu — "restore into herdr" and "open in new window" — each of which runs the
//! matching plugin action with the clicked entry delivered as
//! `HERDR_PLUGIN_PAYLOAD_JSON` via `HostEffect::InvokePluginAction`.

use std::sync::Mutex;

use herdr_ui_plugin_api::{
    HostEffect, UiActionKind, UiEvent, UiFrameContext, UiHitRect, UiLayer, UiNode, UiSurface,
    UiTextSpan, UiUpdate,
};
use serde::{Deserialize, Serialize};

const PLUGIN_ID: &str = "builtin.detached-dash";

/// One detached tmux session, as produced by the collector and rendered here.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SessionEntry {
    /// tmux session name (e.g. `herdr-detached-foo-ab12cd34`) — the restore/open
    /// scripts target it, and it is the stable identity.
    name: String,
    /// Human label derived from the source tab/cwd at detach time.
    #[serde(default)]
    label: String,
    /// Agent kind (`claude` / `codex` / …) resumed inside the session.
    #[serde(default)]
    agent: String,
    /// Agent session id resumed inside the tmux session.
    #[serde(default)]
    session: String,
    /// Working directory the conversation runs in.
    #[serde(default)]
    cwd: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct DetachedInput {
    #[serde(default)]
    sessions: Vec<SessionEntry>,
}

/// The last rendered session list, so a mouse `Action` (which is not a `Frame`
/// and carries no input) can resolve the clicked index to its entry + payload.
static LAST_SESSIONS: Mutex<Vec<SessionEntry>> = Mutex::new(Vec::new());
/// The last panel rect (x, y, w, h), so an `Action` can re-render immediately
/// instead of waiting for the next ~1 Hz frame.
static LAST_RECT: Mutex<Option<(u16, u16, u16, u16)>> = Mutex::new(None);
/// Index into the session list whose 2-item menu is currently open, or `None`.
static MENU_OPEN: Mutex<Option<usize>> = Mutex::new(None);

// ── ABI ──────────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_api_version() -> i32 {
    3
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_alloc(len: i32) -> i32 {
    let mut bytes = Vec::<u8>::with_capacity(len.max(0) as usize);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr as i32
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

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

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

// ── Event handling ────────────────────────────────────────────────────────────

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
    let sessions = ctx
        .host
        .env
        .get("HERDR_PLUGIN_UI_INPUT_JSON")
        .filter(|j| !j.is_empty())
        .and_then(|j| serde_json::from_str::<DetachedInput>(j).ok())
        .map(|input| input.sessions)
        .unwrap_or_default();

    {
        let mut guard = LAST_SESSIONS.lock().expect("sessions mutex poisoned");
        *guard = sessions.clone();
    }
    {
        let mut guard = LAST_RECT.lock().expect("rect mutex poisoned");
        *guard = Some((ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height));
    }
    // If an open menu points past the (now shorter) list, close it.
    {
        let mut menu = MENU_OPEN.lock().expect("menu mutex poisoned");
        if let Some(i) = *menu {
            if i >= sessions.len() {
                *menu = None;
            }
        }
    }

    render(&sessions, ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height)
}

fn handle_action(action_id: &str, kind: UiActionKind) -> UiUpdate {
    // Act on press only; the host routes both *_down and *_up for one physical
    // click, and reacting to both would double-fire.
    if !matches!(kind, UiActionKind::LeftDown | UiActionKind::RightDown) {
        return UiUpdate::default();
    }

    let sessions = LAST_SESSIONS
        .lock()
        .expect("sessions mutex poisoned")
        .clone();

    let mut effects: Vec<HostEffect> = Vec::new();

    if let Some(rest) = action_id.strip_prefix("row:") {
        // Toggle the menu for this row.
        if let Ok(i) = rest.parse::<usize>() {
            let mut menu = MENU_OPEN.lock().expect("menu mutex poisoned");
            *menu = if *menu == Some(i) { None } else { Some(i) };
        }
    } else if let Some(rest) = action_id.strip_prefix("restore:") {
        if let Ok(i) = rest.parse::<usize>() {
            if let Some(entry) = sessions.get(i) {
                effects.push(invoke("restore", entry));
            }
        }
        *MENU_OPEN.lock().expect("menu mutex poisoned") = None;
    } else if let Some(rest) = action_id.strip_prefix("window:") {
        if let Ok(i) = rest.parse::<usize>() {
            if let Some(entry) = sessions.get(i) {
                effects.push(invoke("open-window", entry));
            }
        }
        *MENU_OPEN.lock().expect("menu mutex poisoned") = None;
    } else if action_id == "close" {
        *MENU_OPEN.lock().expect("menu mutex poisoned") = None;
    }

    // Re-render immediately from the cached rect so the menu opens/closes without
    // waiting for the next frame. If we have no rect yet, the next Frame renders.
    let rect = *LAST_RECT.lock().expect("rect mutex poisoned");
    let mut update = match rect {
        Some((x, y, w, h)) => render(&sessions, x, y, w, h),
        None => UiUpdate::default(),
    };
    update.effects.extend(effects);
    update
}

/// Build the `InvokePluginAction` effect that runs `action_id` with `entry`
/// serialized as the payload (delivered to the script as HERDR_PLUGIN_PAYLOAD_JSON).
fn invoke(action_id: &str, entry: &SessionEntry) -> HostEffect {
    HostEffect::InvokePluginAction {
        plugin_id: PLUGIN_ID.to_string(),
        action_id: action_id.to_string(),
        payload_json: serde_json::to_string(entry).ok(),
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn span(text: impl Into<String>, role: Option<&str>) -> UiTextSpan {
    UiTextSpan {
        text: text.into(),
        role: role.map(str::to_string),
        ..Default::default()
    }
}

fn line(spans: Vec<UiTextSpan>) -> UiNode {
    UiNode::Line { spans }
}

/// Render the panel. Nodes are painted top-down from `y`; hit-rects use absolute
/// coordinates offset from (`x`, `y`).
fn render(sessions: &[SessionEntry], x: u16, y: u16, width: u16, height: u16) -> UiUpdate {
    if width == 0 || height == 0 {
        return UiUpdate::default();
    }
    let menu_open = *MENU_OPEN.lock().expect("menu mutex poisoned");

    match menu_open {
        Some(i) if i < sessions.len() => render_menu(&sessions[i], i, x, y, width, height),
        _ => render_list(sessions, x, y, width, height),
    }
}

fn render_list(sessions: &[SessionEntry], x: u16, y: u16, width: u16, height: u16) -> UiUpdate {
    let mut nodes: Vec<UiNode> = Vec::new();
    let mut hits: Vec<UiHitRect> = Vec::new();

    let header = format!("\u{23CF} Detached ({})", sessions.len()); // ⏏ Detached (N)
    nodes.push(line(vec![span(header, Some("lavender"))]));

    if sessions.is_empty() {
        nodes.push(line(vec![span("  none — nothing detached", Some("muted"))]));
        return layers(nodes, hits);
    }

    // One row per session, capped to the available height (minus the header).
    let max_rows = height.saturating_sub(1) as usize;
    for (i, entry) in sessions.iter().take(max_rows).enumerate() {
        let row_y = y + 1 + i as u16;
        let label = if entry.label.is_empty() {
            entry.name.clone()
        } else {
            entry.label.clone()
        };
        let agent = if entry.agent.is_empty() {
            "?".to_string()
        } else {
            entry.agent.clone()
        };
        let text = format!("{} \u{00B7} {}", label, agent); // "<label> · <agent>"
        let text = truncate(&text, width as usize);
        nodes.push(line(vec![span(text, Some("blue"))]));
        hits.push(UiHitRect {
            x,
            y: row_y,
            width,
            height: 1,
            action_id: format!("row:{i}"),
        });
    }

    layers(nodes, hits)
}

fn render_menu(
    entry: &SessionEntry,
    index: usize,
    x: u16,
    y: u16,
    width: u16,
    _height: u16,
) -> UiUpdate {
    let mut nodes: Vec<UiNode> = Vec::new();
    let mut hits: Vec<UiHitRect> = Vec::new();

    let label = if entry.label.is_empty() {
        entry.name.clone()
    } else {
        entry.label.clone()
    };
    nodes.push(line(vec![
        span("\u{23CF} ", Some("lavender")),
        span(truncate(&label, width.saturating_sub(2) as usize), Some("blue")),
    ]));

    // Row 1: restore into herdr.
    nodes.push(line(vec![span(
        truncate("  \u{21A9} restore into herdr", width as usize),
        Some("green"),
    )]));
    hits.push(UiHitRect {
        x,
        y: y + 1,
        width,
        height: 1,
        action_id: format!("restore:{index}"),
    });

    // Row 2: open in new window.
    nodes.push(line(vec![span(
        truncate("  \u{2921} open in new window", width as usize),
        Some("teal"),
    )]));
    hits.push(UiHitRect {
        x,
        y: y + 2,
        width,
        height: 1,
        action_id: format!("window:{index}"),
    });

    // Row 3: close.
    nodes.push(line(vec![span(
        truncate("  \u{2715} close", width as usize),
        Some("muted"),
    )]));
    hits.push(UiHitRect {
        x,
        y: y + 3,
        width,
        height: 1,
        action_id: "close".to_string(),
    });

    layers(nodes, hits)
}

fn layers(nodes: Vec<UiNode>, hits: Vec<UiHitRect>) -> UiUpdate {
    UiUpdate {
        layers: vec![UiLayer {
            surface: UiSurface::Panel,
            z_index: 0,
            consumes_mouse: true,
            nodes,
            hits,
            data: None,
        }],
        ..Default::default()
    }
}

/// Truncate `s` to at most `max` columns (char count is a good enough proxy for
/// the short ASCII/label text here), appending nothing — the host clips anyway.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> SessionEntry {
        SessionEntry {
            name: name.to_string(),
            label: "myproj".to_string(),
            agent: "claude".to_string(),
            session: "sid-123".to_string(),
            cwd: "/tmp/x".to_string(),
        }
    }

    fn frame_json(input_json: &str) -> String {
        let env = format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(input_json).unwrap()
        );
        format!(
            "{{\"type\":\"frame\",\"surface\":\"panel\",\
             \"rect\":{{\"x\":0,\"y\":0,\"width\":34,\"height\":6}},\
             \"focused\":false,\
             \"host\":{{\"focus\":{{\"session_id\":null,\"session_ref\":null,\
             \"cwd\":null,\"agent\":\"None\",\"is_self_focus\":false}},\
             \"env\":{env}}}}}"
        )
    }

    fn action_json(action_id: &str, kind: &str) -> String {
        format!(
            "{{\"type\":\"action\",\"action_id\":\"{action_id}\",\
             \"surface\":\"panel\",\"column\":1,\"row\":1,\"kind\":\"{kind}\"}}"
        )
    }

    #[test]
    fn empty_list_renders_none_line() {
        *MENU_OPEN.lock().unwrap() = None;
        let ev: UiEvent = serde_json::from_str(&frame_json(r#"{"sessions":[]}"#)).unwrap();
        let update = handle_event(&ev);
        assert_eq!(update.layers.len(), 1);
        assert!(update.layers[0].hits.is_empty());
    }

    #[test]
    fn list_declares_one_hit_per_session() {
        *MENU_OPEN.lock().unwrap() = None;
        let _ = entry("herdr-detached-a"); // helper kept for other tests
        let ev: UiEvent = serde_json::from_str(&frame_json(
            r#"{"sessions":[{"name":"herdr-detached-a","label":"a","agent":"claude","session":"s","cwd":"/x"},{"name":"herdr-detached-b","label":"b","agent":"claude","session":"s2","cwd":"/y"}]}"#,
        ))
        .unwrap();
        let update = handle_event(&ev);
        let hits = &update.layers[0].hits;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].action_id, "row:0");
        assert_eq!(hits[1].action_id, "row:1");
    }

    #[test]
    fn right_click_row_opens_menu_with_restore_and_window() {
        *MENU_OPEN.lock().unwrap() = None;
        // Seed LAST_SESSIONS + LAST_RECT via a frame first.
        let ev: UiEvent = serde_json::from_str(&frame_json(
            r#"{"sessions":[{"name":"herdr-detached-a","label":"a","agent":"claude","session":"s","cwd":"/x"}]}"#,
        ))
        .unwrap();
        let _ = handle_event(&ev);

        let ev: UiEvent = serde_json::from_str(&action_json("row:0", "right_down")).unwrap();
        let update = handle_event(&ev);
        let ids: Vec<&str> = update.layers[0]
            .hits
            .iter()
            .map(|h| h.action_id.as_str())
            .collect();
        assert!(ids.contains(&"restore:0"), "menu should offer restore: {ids:?}");
        assert!(ids.contains(&"window:0"), "menu should offer window: {ids:?}");
        assert!(ids.contains(&"close"));
    }

    #[test]
    fn clicking_restore_emits_invoke_with_payload() {
        *MENU_OPEN.lock().unwrap() = None;
        let ev: UiEvent = serde_json::from_str(&frame_json(
            r#"{"sessions":[{"name":"herdr-detached-a","label":"a","agent":"claude","session":"sid-1","cwd":"/x"}]}"#,
        ))
        .unwrap();
        let _ = handle_event(&ev);
        // Open menu.
        let _ = handle_event(&serde_json::from_str::<UiEvent>(&action_json("row:0", "left_down")).unwrap());
        // Click restore.
        let ev: UiEvent = serde_json::from_str(&action_json("restore:0", "left_down")).unwrap();
        let update = handle_event(&ev);
        assert_eq!(update.effects.len(), 1);
        match &update.effects[0] {
            HostEffect::InvokePluginAction {
                plugin_id,
                action_id,
                payload_json,
            } => {
                assert_eq!(plugin_id, PLUGIN_ID);
                assert_eq!(action_id, "restore");
                let payload = payload_json.as_ref().expect("payload present");
                assert!(payload.contains("sid-1"), "payload carries session id: {payload}");
            }
            other => panic!("expected InvokePluginAction, got {other:?}"),
        }
        // Menu closed after acting.
        assert_eq!(*MENU_OPEN.lock().unwrap(), None);
    }

    #[test]
    fn action_up_is_ignored() {
        *MENU_OPEN.lock().unwrap() = None;
        let ev: UiEvent = serde_json::from_str(&action_json("row:0", "left_up")).unwrap();
        let update = handle_event(&ev);
        assert!(update.effects.is_empty());
        assert_eq!(*MENU_OPEN.lock().unwrap(), None);
    }
}
