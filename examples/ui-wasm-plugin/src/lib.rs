//! v3 demo bridge guest: exercises the herdr WASM UI Plugin API v3 surface
//! (persistent instance state, dynamic action routing, the in-process API
//! bridge, and partial-scope updates) for wave-1 E2E coverage.
//!
//! `crates/herdr-ui-plugin-api` is still v2 and does not yet define the v3
//! wire types (`scope`, `action`, `api_result`, the `Api` effect), so this
//! crate defines them locally against `.local/prd/wasm-ui-plugin-api-v3.md`.
//! v2 types that are unchanged in v3 (`UiSurface`, `UiNode`, `UiTextSpan`,
//! `UiHitRect`, `UiLayer`, `UiFrameContext`) are reused from the shared crate.

use herdr_ui_plugin_api::{UiFrameContext, UiHitRect, UiLayer, UiNode, UiSurface, UiTextSpan};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

// ── Guest-persistent state ──────────────────────────────────────────────────
// Persists across events for the wasm instance's lifetime; reset only on
// hot-reload or a trap (spec §3), which is exactly what TC1 and TC3 exercise.

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static COUNTER: AtomicU32 = AtomicU32::new(0);
static REQUESTED_INITIAL: AtomicBool = AtomicBool::new(false);
static STRIP_COLLAPSED: AtomicBool = AtomicBool::new(false);
static ACTIVE_WORKSPACE_HINT: Mutex<Option<String>> = Mutex::new(None);
static WORKSPACES_LINE: Mutex<Option<String>> = Mutex::new(None);
static PANES_LINE: Mutex<Option<String>> = Mutex::new(None);
static SPLIT_ERROR: Mutex<Option<String>> = Mutex::new(None);

const REQ_WORKSPACES: &str = "demo:workspaces";
const REQ_PANES: &str = "demo:panes";
const REQ_SPLIT: &str = "demo:split";

// ── v3 wire types (local; see module doc comment) ───────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum V3Event {
    Frame(UiFrameContext),
    // Payloads must stay typed as `Value` (not `()`) so serde's internally
    // tagged deserialization still accepts the object body; the demo has no
    // use for the contents, so the field itself is intentionally unread.
    #[allow(dead_code)]
    Mouse(Value),
    #[allow(dead_code)]
    Key(Value),
    #[allow(dead_code)]
    FocusChanged(Value),
    Tick,
    ThemeChanged,
    ReloadRequested,
    Action(ActionEvent),
    ApiResult(ApiResultEvent),
}

/// Only `action_id` is read: routing in this guest is entirely by id, and
/// `surface`/`column`/`row`/`kind` (present on the wire per spec §4) are
/// unused, so they're left off rather than carried as dead fields.
#[derive(Debug, Deserialize)]
struct ActionEvent {
    action_id: String,
}

#[derive(Debug, Deserialize)]
struct ApiResultEvent {
    request_id: String,
    ok: bool,
    #[serde(default)]
    response: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HostEffectV3 {
    ShowToast {
        message: String,
    },
    Api {
        request_id: String,
        method: String,
        params: Value,
    },
    SetSurfaceCollapsed {
        surface: UiSurface,
        collapsed: bool,
    },
}

#[derive(Debug, Serialize)]
struct UiUpdateV3 {
    scope: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    layers: Vec<UiLayer>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    effects: Vec<HostEffectV3>,
}

fn empty_update() -> UiUpdateV3 {
    UiUpdateV3 {
        scope: "partial",
        layers: Vec::new(),
        effects: Vec::new(),
    }
}

// ── ABI exports ──────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_api_version() -> i32 {
    3
}

static STATIC_UPDATE: &str = r#"{"scope":"partial","layers":[]}"#;

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_ptr() -> i32 {
    STATIC_UPDATE.as_ptr() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_len() -> i32 {
    STATIC_UPDATE.len() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_alloc(len: i32) -> i32 {
    let mut bytes = Vec::<u8>::with_capacity(len.max(0) as usize);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_on_event(ptr: i32, len: i32) -> i32 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize) };
    let update = match serde_json::from_slice::<V3Event>(bytes) {
        Ok(event) => handle_event(&event),
        Err(_) => empty_update(),
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

// ── Event routing ────────────────────────────────────────────────────────────

fn handle_event(event: &V3Event) -> UiUpdateV3 {
    match event {
        V3Event::Frame(frame) => handle_frame(frame),
        V3Event::Action(action) => handle_action(action),
        V3Event::ApiResult(result) => handle_api_result(result),
        V3Event::Mouse(_)
        | V3Event::Key(_)
        | V3Event::FocusChanged(_)
        | V3Event::Tick
        | V3Event::ThemeChanged
        | V3Event::ReloadRequested => empty_update(),
    }
}

fn handle_frame(frame: &UiFrameContext) -> UiUpdateV3 {
    if let Some(hint) = active_workspace_hint(frame) {
        *ACTIVE_WORKSPACE_HINT
            .lock()
            .expect("layout hint mutex poisoned") = Some(hint);
    }

    let mut effects = Vec::new();
    if !REQUESTED_INITIAL.swap(true, Ordering::SeqCst) {
        effects.extend(refresh_effects());
    }
    render_update(effects)
}

fn handle_action(action: &ActionEvent) -> UiUpdateV3 {
    match action.action_id.as_str() {
        "demo:increment" => {
            COUNTER.fetch_add(1, Ordering::SeqCst);
            render_update(Vec::new())
        }
        "demo:refresh" => render_update(refresh_effects()),
        // Item H (§7.2): collapse/expand the bottom usage strip. Interim home
        // for the toggle until usage-dash is a linked v3 plugin owning it.
        "demo:toggle_strip" => {
            let collapsed = !STRIP_COLLAPSED.load(Ordering::SeqCst);
            STRIP_COLLAPSED.store(collapsed, Ordering::SeqCst);
            render_update(vec![HostEffectV3::SetSurfaceCollapsed {
                surface: UiSurface::UsageStrip,
                collapsed,
            }])
        }
        "demo:split" => render_update(vec![HostEffectV3::Api {
            request_id: REQ_SPLIT.to_string(),
            method: "pane.split".to_string(),
            params: json!({ "direction": "right" }),
        }]),
        // TC6: a deliberately non-whitelisted action id. The host never
        // interprets `action_id` (spec §4), so this proves arbitrary ids
        // route back to the owning module rather than being rejected.
        "demo:party" => render_update(vec![HostEffectV3::ShowToast {
            message: "v3 action routing works: demo:party".to_string(),
        }]),
        // TC3 runaway self-test: exercises the host's epoch-interruption
        // trap-and-recover path (spec §3). Deliberately never returns.
        "demo:hang" => loop {
            std::hint::spin_loop();
        },
        _ => render_update(Vec::new()),
    }
}

fn handle_api_result(result: &ApiResultEvent) -> UiUpdateV3 {
    match result.request_id.as_str() {
        REQ_WORKSPACES => {
            let line = if result.ok {
                summarize_workspaces(result.response.as_ref())
            } else {
                format!(
                    "workspaces: error: {}",
                    result.error.as_deref().unwrap_or("unknown")
                )
            };
            *WORKSPACES_LINE.lock().expect("workspaces mutex poisoned") = Some(line);
            render_update(Vec::new())
        }
        REQ_PANES => {
            let line = if result.ok {
                summarize_panes(result.response.as_ref())
            } else {
                format!(
                    "panes: error: {}",
                    result.error.as_deref().unwrap_or("unknown")
                )
            };
            *PANES_LINE.lock().expect("panes mutex poisoned") = Some(line);
            render_update(Vec::new())
        }
        REQ_SPLIT => {
            let err = if result.ok {
                None
            } else {
                Some(
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| "unknown error".to_string()),
                )
            };
            *SPLIT_ERROR.lock().expect("split error mutex poisoned") = err;
            render_update(Vec::new())
        }
        // Stale or unrecognized request id (e.g. surviving a hot-reload
        // instance reset): tolerate silently per spec §5.4.
        _ => empty_update(),
    }
}

fn refresh_effects() -> Vec<HostEffectV3> {
    vec![
        HostEffectV3::Api {
            request_id: REQ_WORKSPACES.to_string(),
            method: "workspace.list".to_string(),
            params: json!({}),
        },
        HostEffectV3::Api {
            request_id: REQ_PANES.to_string(),
            method: "pane.list".to_string(),
            params: json!({}),
        },
    ]
}

fn active_workspace_hint(frame: &UiFrameContext) -> Option<String> {
    let raw = frame.host.env.get("HERDR_LAYOUT_JSON")?;
    let value: Value = serde_json::from_str(raw).ok()?;
    value
        .get("active_workspace_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn summarize_workspaces(response: Option<&Value>) -> String {
    let items = response
        .and_then(|v| v.get("workspaces"))
        .and_then(Value::as_array);
    match items {
        Some(items) if !items.is_empty() => {
            let names: Vec<String> = items
                .iter()
                .map(|w| {
                    let label = w.get("label").and_then(Value::as_str).unwrap_or("?");
                    let id = w.get("workspace_id").and_then(Value::as_str).unwrap_or("?");
                    format!("{label}({id})")
                })
                .collect();
            format!("workspaces: {}", names.join(", "))
        }
        _ => "workspaces: (none)".to_string(),
    }
}

fn summarize_panes(response: Option<&Value>) -> String {
    let items = response
        .and_then(|v| v.get("panes"))
        .and_then(Value::as_array);
    match items {
        Some(items) if !items.is_empty() => {
            let names: Vec<String> = items
                .iter()
                .map(|p| {
                    let display = p
                        .get("label")
                        .and_then(Value::as_str)
                        .or_else(|| p.get("title").and_then(Value::as_str))
                        .or_else(|| p.get("agent").and_then(Value::as_str))
                        .unwrap_or("pane");
                    let id = p.get("pane_id").and_then(Value::as_str).unwrap_or("?");
                    format!("{display}({id})")
                })
                .collect();
            format!("panes: {}", names.join(", "))
        }
        _ => "panes: (none)".to_string(),
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn render_update(effects: Vec<HostEffectV3>) -> UiUpdateV3 {
    UiUpdateV3 {
        scope: "partial",
        layers: vec![header_layer(), right_panel_layer()],
        effects,
    }
}

fn header_layer() -> UiLayer {
    let count = COUNTER.load(Ordering::SeqCst);
    let hint = ACTIVE_WORKSPACE_HINT
        .lock()
        .expect("layout hint mutex poisoned")
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    UiLayer {
        surface: UiSurface::Header,
        z_index: 10,
        consumes_mouse: false,
        nodes: vec![UiNode::Status {
            left: vec![
                span(" WASM UI v3 ", "accent"),
                span("demo bridge guest", "muted"),
            ],
            right: vec![
                span(&format!("ws:{hint}"), "subtext"),
                span(&format!("clicks:{count}"), "success"),
            ],
        }],
        hits: Vec::new(),
        data: None,
    }
}

fn right_panel_layer() -> UiLayer {
    let mut nodes: Vec<UiNode> = Vec::new();
    let mut hits = Vec::new();

    nodes.push(UiNode::Line {
        spans: vec![span("API bridge (v3)", "accent")],
    });

    let workspaces = WORKSPACES_LINE
        .lock()
        .expect("workspaces mutex poisoned")
        .clone()
        .unwrap_or_else(|| "workspaces: (not loaded)".to_string());
    nodes.push(UiNode::Line {
        spans: vec![plain(&workspaces)],
    });

    let panes = PANES_LINE
        .lock()
        .expect("panes mutex poisoned")
        .clone()
        .unwrap_or_else(|| "panes: (not loaded)".to_string());
    nodes.push(UiNode::Line {
        spans: vec![plain(&panes)],
    });

    let refresh_row = nodes.len() as u16;
    let mut refresh_spans = Vec::new();
    let mut refresh_col = 0u16;
    push_button(
        &mut refresh_spans,
        &mut hits,
        &mut refresh_col,
        refresh_row,
        "[refresh]",
        "demo:refresh",
        "teal",
    );
    nodes.push(UiNode::Line {
        spans: refresh_spans,
    });

    let split_row = nodes.len() as u16;
    let mut split_spans = Vec::new();
    let mut split_col = 0u16;
    push_button(
        &mut split_spans,
        &mut hits,
        &mut split_col,
        split_row,
        "[split pane]",
        "demo:split",
        "teal",
    );
    nodes.push(UiNode::Line { spans: split_spans });

    let count = COUNTER.load(Ordering::SeqCst);
    let demo_row = nodes.len() as u16;
    let mut demo_spans = Vec::new();
    let mut demo_col = 0u16;
    push_button(
        &mut demo_spans,
        &mut hits,
        &mut demo_col,
        demo_row,
        &format!("count: {count} [+]"),
        "demo:increment",
        "accent",
    );
    push_gap(&mut demo_spans, &mut demo_col, "  ");
    push_button(
        &mut demo_spans,
        &mut hits,
        &mut demo_col,
        demo_row,
        "party",
        "demo:party",
        "mauve",
    );
    push_gap(&mut demo_spans, &mut demo_col, "  ");
    push_button(
        &mut demo_spans,
        &mut hits,
        &mut demo_col,
        demo_row,
        "\u{26A0} hang",
        "demo:hang",
        "danger",
    );
    push_gap(&mut demo_spans, &mut demo_col, "  ");
    let strip_label = if STRIP_COLLAPSED.load(Ordering::SeqCst) {
        "\u{00BB} strip"
    } else {
        "\u{00AB} strip"
    };
    push_button(
        &mut demo_spans,
        &mut hits,
        &mut demo_col,
        demo_row,
        strip_label,
        "demo:toggle_strip",
        "teal",
    );
    nodes.push(UiNode::Line { spans: demo_spans });

    if let Some(err) = SPLIT_ERROR
        .lock()
        .expect("split error mutex poisoned")
        .clone()
    {
        nodes.push(UiNode::Line {
            spans: vec![span(&format!("split refused: {err}"), "danger")],
        });
    }

    UiLayer {
        surface: UiSurface::RightPanel,
        z_index: 12,
        consumes_mouse: false,
        nodes,
        hits,
        data: None,
    }
}

fn span(text: &str, role: &str) -> UiTextSpan {
    UiTextSpan {
        text: text.to_string(),
        role: Some(role.to_string()),
        ..Default::default()
    }
}

fn plain(text: &str) -> UiTextSpan {
    UiTextSpan {
        text: text.to_string(),
        ..Default::default()
    }
}

fn push_button(
    spans: &mut Vec<UiTextSpan>,
    hits: &mut Vec<UiHitRect>,
    col: &mut u16,
    row: u16,
    label: &str,
    action_id: &str,
    role: &str,
) {
    let width = label.chars().count() as u16;
    hits.push(UiHitRect {
        x: *col,
        y: row,
        width,
        height: 1,
        action_id: action_id.to_string(),
    });
    spans.push(span(label, role));
    *col += width;
}

fn push_gap(spans: &mut Vec<UiTextSpan>, col: &mut u16, gap: &str) {
    spans.push(plain(gap));
    *col += gap.chars().count() as u16;
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-exact against the spec §5.1 `Api` effect example.
    #[test]
    fn api_effect_matches_spec_example() {
        let effect = HostEffectV3::Api {
            request_id: "tabs-1".to_string(),
            method: "pane.list".to_string(),
            params: json!({}),
        };
        let encoded = serde_json::to_string(&effect).expect("serialize");
        assert_eq!(
            encoded,
            r#"{"kind":"api","request_id":"tabs-1","method":"pane.list","params":{}}"#
        );
    }

    /// Round-trips the two spec §5.2 `api_result` examples (ok and error).
    #[test]
    fn api_result_parses_spec_examples() {
        let ok_json =
            r#"{"type":"api_result","request_id":"tabs-1","ok":true,"response":{"workspaces":[]}}"#;
        match serde_json::from_str::<V3Event>(ok_json).expect("parse ok result") {
            V3Event::ApiResult(result) => {
                assert_eq!(result.request_id, "tabs-1");
                assert!(result.ok);
                assert_eq!(result.response, Some(json!({"workspaces": []})));
                assert_eq!(result.error, None);
            }
            other => panic!("expected ApiResult, got {other:?}"),
        }

        let err_json = r#"{"type":"api_result","request_id":"x","ok":false,"error":"capability_denied: pane.split requires ApiWrite"}"#;
        match serde_json::from_str::<V3Event>(err_json).expect("parse err result") {
            V3Event::ApiResult(result) => {
                assert_eq!(result.request_id, "x");
                assert!(!result.ok);
                assert_eq!(
                    result.error.as_deref(),
                    Some("capability_denied: pane.split requires ApiWrite")
                );
            }
            other => panic!("expected ApiResult, got {other:?}"),
        }
    }

    /// An empty update under partial scope is a documented no-op (spec §6).
    #[test]
    fn empty_update_is_a_partial_no_op() {
        let update = empty_update();
        let encoded = serde_json::to_string(&update).expect("serialize");
        assert_eq!(encoded, r#"{"scope":"partial"}"#);
    }

    /// An unrecognized `action_id` parses fine and renders without a trap.
    #[test]
    fn action_event_parses_with_only_action_id() {
        let json = r#"{"type":"action","action_id":"demo:party","surface":"footer","column":5,"row":0,"kind":"left_down"}"#;
        match serde_json::from_str::<V3Event>(json).expect("parse action") {
            V3Event::Action(action) => assert_eq!(action.action_id, "demo:party"),
            other => panic!("expected Action, got {other:?}"),
        }
    }
}
