//! Version-dash guest: a compact "running herdr version vs latest stable"
//! indicator rendered in a generic plugin `Panel` docked as a single full-width
//! line on the bottom row (position is pure manifest DATA — see
//! `[ui_modules.placement]`). It tracks two instances: `hr` (the daily driver)
//! and `test` (herdrtest).
//!
//! Data path (spec §7.1, the generic `ui_input_file` transport — NO host code):
//! the out-of-process `version-dash-collector` resolves each instance's version,
//! the latest stable release, and per-instance health on a 10-minute cadence,
//! then writes `version_input.json` into this module's state dir. The host reads
//! that file each frame and injects it as `HERDR_PLUGIN_UI_INPUT_JSON`, which the
//! guest deserializes and renders.
//!
//! Interaction:
//!   - LEFT-click an instance → toast its versions + latest release date/notes.
//!   - RIGHT-click an instance → invoke its staged-update plugin action
//!     (`test` pulls the fork integration-tip build; `hr` pulls the exact build
//!     currently running in `test`). The actual update work runs in the plugin's
//!     declared action scripts; the guest only fires the intent.

mod render;

use herdr_ui_plugin_api::{
    HostEffect, UiActionKind, UiEvent, UiHitRect, UiLayer, UiSurface, UiUpdate, UiUpdateScope,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::Mutex;
use version_dash_model::VersionRenderInput;

/// This plugin's manifest id, used to route right-click update intents back to
/// our own declared action scripts via `InvokePluginAction`.
const PLUGIN_ID: &str = "builtin.version-dash";

/// Action id prefix for a per-instance hit rect: `version:instance:<label>`.
/// The `kind` (LeftDown/RightDown) on the resulting `UiEvent::Action` decides
/// toast vs update.
const INSTANCE_ACTION_PREFIX: &str = "version:instance:";

/// Declared plugin actions (see herdr-plugin.toml `[[actions]]`). `test` pulls
/// the fork integration-tip build; `hr` may ONLY pull the exact build running in
/// `test` (staged promotion — enforced in the action script, not just hidden).
const ACTION_UPDATE_TEST: &str = "update-herdrtest-from-latest";
const ACTION_UPDATE_HR: &str = "update-hr-from-herdrtest";

/// Cache of the last input we rendered, so a click can build its toast from the
/// same data the strip is showing without re-reading any file.
static LAST_INPUT: Mutex<Option<VersionRenderInput>> = Mutex::new(None);

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

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

    // Generic §7.1 transport: the host injects the plugin's ui_input_file
    // (version_input.json) contents here. No key ⇒ nothing to render (loss-free
    // under partial-default merge, same as usage-dash).
    let json = match ctx.host.env.get("HERDR_PLUGIN_UI_INPUT_JSON") {
        Some(j) if !j.is_empty() => j,
        _ => return UiUpdate::default(),
    };

    let input: VersionRenderInput = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return UiUpdate::default(),
    };

    *LAST_INPUT.lock().expect("input mutex poisoned") = Some(input.clone());

    // Render into a CONTENT-sized buffer, independent of the incoming panel rect,
    // so the host can measure the natural width and carve the slot even on the
    // first content frame (before which `ctx.rect` is still zero). The per-instance
    // hit rects come from the real (carved) panel rect so clicks map to cells.
    let content = Rect::new(0, 0, render::content_width(&input).max(1), 1);
    let mut buf = Buffer::empty(content);
    let ranges = render::render(&input, content, &mut buf);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);
    let panel = Rect::new(ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height);
    let hits = instance_hits(panel, &ranges);

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

/// Map each instance's line-relative column range to an absolute hit rect on the
/// carved panel row. Clamped to the panel width so a rect never spills past the
/// slot. Empty until the panel has been carved (rect still zero at bootstrap).
fn instance_hits(panel: Rect, ranges: &[render::InstanceSpan]) -> Vec<UiHitRect> {
    if panel.width == 0 || panel.height == 0 {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for span in ranges {
        if span.start >= panel.width {
            break;
        }
        let width = span.width.min(panel.width - span.start);
        if width == 0 {
            continue;
        }
        hits.push(UiHitRect {
            x: panel.x + span.start,
            y: panel.y,
            width,
            height: 1,
            action_id: format!("{INSTANCE_ACTION_PREFIX}{}", span.label),
        });
    }
    hits
}

fn handle_action(action_id: &str, kind: UiActionKind) -> UiUpdate {
    let Some(label) = action_id.strip_prefix(INSTANCE_ACTION_PREFIX) else {
        return UiUpdate::default();
    };
    match kind {
        // Fire once per physical click (press only).
        UiActionKind::LeftDown => toast_effect(label),
        UiActionKind::RightDown => update_effect(label),
        _ => UiUpdate::default(),
    }
}

/// Left-click: toast the clicked instance's version detail (or latest info).
fn toast_effect(label: &str) -> UiUpdate {
    let message = LAST_INPUT
        .lock()
        .expect("input mutex poisoned")
        .as_ref()
        .map(|input| toast_message(input, label))
        .unwrap_or_else(|| "version: no data yet".to_string());

    UiUpdate {
        layers: Vec::new(),
        effects: vec![HostEffect::ShowToast { message }],
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    }
}

/// Right-click: invoke the staged-update action for the clicked instance. The
/// enforcement (hr may only take the exact test build) lives in the action
/// script; the guest just fires the intent for the matching instance.
fn update_effect(label: &str) -> UiUpdate {
    let action_id = match label {
        "hr" => ACTION_UPDATE_HR,
        "test" => ACTION_UPDATE_TEST,
        _ => return UiUpdate::default(),
    };
    UiUpdate {
        layers: Vec::new(),
        effects: vec![HostEffect::InvokePluginAction {
            plugin_id: PLUGIN_ID.to_string(),
            action_id: action_id.to_string(),
            payload_json: None,
        }],
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    }
}

/// Find an instance by label in the last input.
fn find_instance<'a>(
    input: &'a VersionRenderInput,
    label: &str,
) -> Option<&'a version_dash_model::InstanceStatus> {
    input.instances.iter().find(|i| i.label == label)
}

/// Build the click toast for `label`: its version + change count, plus the shared
/// latest release date / notes URL, plus any collector error for offline
/// visibility. The host renders toasts on a single line, so parts join with
/// " · " rather than newlines (which collapse into a run-on string).
fn toast_message(input: &VersionRenderInput, label: &str) -> String {
    let latest = input.latest.as_deref().unwrap_or("unknown");
    let mut parts = Vec::new();

    match find_instance(input, label) {
        Some(inst) => {
            let version = inst.version.as_deref().unwrap_or("unknown");
            let changes = match inst.local_changes {
                Some(n) if n > 0 => format!(" (+{n} local)"),
                _ => String::new(),
            };
            let state = match inst.state {
                version_dash_model::InstanceState::UpToDate => "up to date",
                version_dash_model::InstanceState::Behind => "update available",
                version_dash_model::InstanceState::Broken => "BROKEN",
                version_dash_model::InstanceState::Unknown => "unknown",
            };
            parts.push(format!(
                "{label} {version}{changes} — {state} (latest {latest})"
            ));
        }
        None => parts.push(format!("{label}: no data")),
    }

    if let Some(date) = input.latest_date.as_deref() {
        parts.push(format!("released {date}"));
    }
    if let Some(url) = input.notes_url.as_deref() {
        parts.push(url.to_string());
    }
    if !input.age_label.is_empty() {
        if input.age_label == "just now" {
            parts.push("checked just now".to_string());
        } else {
            parts.push(format!("checked {} ago", input.age_label));
        }
    }
    if let Some(err) = input.error.as_deref() {
        parts.push(format!("last check failed: {err}"));
    }
    parts.join(" \u{00B7} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use version_dash_model::{InstanceState, InstanceStatus};

    fn inst(label: &str, ver: &str, changes: Option<u32>, state: InstanceState) -> InstanceStatus {
        InstanceStatus {
            label: label.into(),
            version: Some(ver.into()),
            local_changes: changes,
            state,
        }
    }

    fn sample_input() -> VersionRenderInput {
        VersionRenderInput {
            instances: vec![
                inst("hr", "0.7.3", Some(107), InstanceState::Behind),
                inst("test", "0.7.5", Some(0), InstanceState::UpToDate),
            ],
            latest: Some("0.7.5".into()),
            ..Default::default()
        }
    }

    fn frame_json(env: &str) -> String {
        format!(
            "{{\"type\":\"frame\",\"surface\":\"panel\",\
             \"rect\":{{\"x\":0,\"y\":0,\"width\":60,\"height\":1}},\
             \"focused\":false,\
             \"host\":{{\"focus\":{{\"session_id\":null,\"session_ref\":null,\
             \"cwd\":null,\"agent\":\"None\",\"is_self_focus\":false}},\
             \"env\":{env}}}}}"
        )
    }

    fn env_with_input(input: &VersionRenderInput) -> String {
        let inner = serde_json::to_string(input).unwrap();
        format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&inner).unwrap()
        )
    }

    fn action_json(action_id: &str, kind: &str) -> String {
        format!(
            "{{\"type\":\"action\",\"action_id\":\"{action_id}\",\
             \"surface\":\"panel\",\"column\":1,\"row\":0,\"kind\":\"{kind}\"}}"
        )
    }

    #[test]
    fn renders_panel_layer_with_per_instance_hits() {
        let event: UiEvent = serde_json::from_str(&frame_json(&env_with_input(&sample_input())))
            .expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::Panel));
        assert!(!update.layers[0].nodes.is_empty());
        let hits = &update.layers[0].hits;
        assert!(hits.iter().any(|h| h.action_id == "version:instance:hr"));
        assert!(hits.iter().any(|h| h.action_id == "version:instance:test"));
    }

    #[test]
    fn missing_input_env_renders_nothing() {
        let event: UiEvent = serde_json::from_str(&frame_json("{}")).expect("frame parses");
        assert!(handle_event(&event).layers.is_empty());
    }

    #[test]
    fn instance_hits_are_disjoint_and_clamped() {
        let (_, ranges) = render::build(&sample_input());
        let panel = Rect::new(3, 9, 40, 1);
        let hits = instance_hits(panel, &ranges);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].x, 3); // first token at panel.x
        assert!(hits[0].x + hits[0].width <= hits[1].x); // disjoint, in order
        for h in &hits {
            assert!(h.x + h.width <= panel.x + panel.width, "clamped to panel");
            assert_eq!(h.y, 9);
            assert_eq!(h.height, 1);
        }
    }

    #[test]
    fn left_click_hr_toasts_hr_detail() {
        *LAST_INPUT.lock().unwrap() = Some(sample_input());
        let action: UiEvent =
            serde_json::from_str(&action_json("version:instance:hr", "left_down")).expect("action");
        let update = handle_event(&action);
        assert_eq!(update.effects.len(), 1);
        match &update.effects[0] {
            HostEffect::ShowToast { message } => {
                assert!(message.contains("hr 0.7.3"), "{message}");
                assert!(message.contains("+107 local"), "{message}");
                assert!(message.contains("update available"), "{message}");
            }
            other => panic!("expected toast, got {other:?}"),
        }
    }

    #[test]
    fn right_click_test_invokes_update_from_latest() {
        let action: UiEvent =
            serde_json::from_str(&action_json("version:instance:test", "right_down"))
                .expect("action");
        let update = handle_event(&action);
        assert_eq!(update.effects.len(), 1);
        match &update.effects[0] {
            HostEffect::InvokePluginAction {
                plugin_id,
                action_id,
                ..
            } => {
                assert_eq!(plugin_id, PLUGIN_ID);
                assert_eq!(action_id, ACTION_UPDATE_TEST);
            }
            other => panic!("expected invoke, got {other:?}"),
        }
    }

    #[test]
    fn right_click_hr_invokes_update_from_herdrtest() {
        let action: UiEvent =
            serde_json::from_str(&action_json("version:instance:hr", "right_down"))
                .expect("action");
        let update = handle_event(&action);
        match &update.effects[0] {
            HostEffect::InvokePluginAction { action_id, .. } => {
                assert_eq!(action_id, ACTION_UPDATE_HR);
            }
            other => panic!("expected invoke, got {other:?}"),
        }
    }

    #[test]
    fn left_up_is_ignored() {
        let action: UiEvent =
            serde_json::from_str(&action_json("version:instance:hr", "left_up")).expect("action");
        assert!(handle_event(&action).effects.is_empty());
    }

    #[test]
    fn unknown_action_id_ignored() {
        let action: UiEvent =
            serde_json::from_str(&action_json("something:else", "left_down")).expect("action");
        assert!(handle_event(&action).effects.is_empty());
    }
}
