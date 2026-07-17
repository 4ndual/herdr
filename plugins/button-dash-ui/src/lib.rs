//! Button-dash guest: a data-driven, clickable quick-reference of
//! keyboard-shortcut actions rendered in a generic plugin `Panel` whose position
//! is pure manifest DATA (`[ui_modules.placement]`), docked beside the usage
//! strip by default.
//!
//! Each entry in [`BUTTONS`] is one row of data — no rendering code changes to
//! add a button: append a [`ButtonDef`] here and declare a matching
//! `[[actions]]` entry in `herdr-plugin.toml`. Clicking a button emits a
//! `plugin.action.invoke` API effect that runs the declared host action.
//!
//! This crate mirrors the v3 guest ABI of `examples/ui-wasm-plugin`
//! (`crates/herdr-ui-plugin-api` is still v2 on the wire, so the v3-only wire
//! types are defined locally here). Hit-testing is absolute (screen
//! coordinates), so panel-relative rects are offset by the panel origin the host
//! resolves from the manifest placement and delivers as the frame `rect` — the
//! guest never needs to know WHERE it is docked, so moving/resizing the panel is
//! a pure manifest edit with no guest OR host rebuild.

use herdr_ui_plugin_api::{UiFrameContext, UiHitRect, UiLayer, UiNode, UiSurface, UiTextSpan};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// This plugin's manifest id; sent with every `plugin.action.invoke` so the
/// host resolves the action against THIS plugin (not another with a same id).
const PLUGIN_ID: &str = "builtin.button-dash";

// ── Button registry (data-driven) ────────────────────────────────────────────
// Add a button = add one row here + one `[[actions]]` entry in the manifest with
// a matching id. No rendering code changes required.

struct ButtonDef {
    /// Stable identity for the row (currently unused for routing, but kept so a
    /// future keyed lookup / persisted layout has a handle). Same as action_id
    /// today.
    #[allow(dead_code)]
    id: &'static str,
    /// Compact label shown on the button.
    label: &'static str,
    /// Display string for the key combo, e.g. "⌃⌥X".
    shortcut: &'static str,
    /// What the button does, shown in the expanded list.
    description: &'static str,
    /// Manifest `[[actions]]` id the click invokes via `plugin.action.invoke`.
    action_id: &'static str,
}

static BUTTONS: &[ButtonDef] = &[
    ButtonDef {
        id: "restart-hr",
        label: "Restart hr",
        shortcut: "\u{2303}\u{2325}X", // ⌃⌥X
        description: "Fully kill & relaunch this herdr instance, restoring the session",
        action_id: "restart-hr",
    },
    ButtonDef {
        id: "full-restart",
        label: "Full restart",
        shortcut: "\u{2303}\u{2325}\u{21E7}X", // ⌃⌥⇧X
        description:
            "Hard kill tmux + Claude Code + server (reclaim all RAM), relaunch fresh & resume",
        action_id: "full-restart",
    },
];

// Action id for the built-in expand/collapse toggle (not a manifest action; the
// guest handles it internally, so it never reaches plugin.action.invoke).
const ACTION_TOGGLE_LIST: &str = "button-dash:toggle_list";

// ── Guest-persistent state ────────────────────────────────────────────────────

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// Whether the expandable full list (label + shortcut + description) is shown.
static LIST_EXPANDED: AtomicBool = AtomicBool::new(false);
/// Absolute (x, y) origin of this module's `Panel` rect, taken from the frame
/// `rect` the host resolves from the manifest placement. Declared hit-rects are
/// matched in absolute screen coordinates (host hit-testing), so panel hits must
/// carry this offset.
static STRIP_ORIGIN: Mutex<Option<(u16, u16)>> = Mutex::new(None);

// ── v3 wire types (local; see module doc comment) ────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum V3Event {
    Frame(UiFrameContext),
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
    #[allow(dead_code)]
    ApiResult(Value),
}

#[derive(Debug, Deserialize)]
struct ActionEvent {
    action_id: String,
    /// spec §4: the host routes an `Action` for BOTH left_down and left_up of one
    /// physical click — reacting to both runs everything twice, so we gate on
    /// left_down only.
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HostEffectV3 {
    #[allow(dead_code)]
    ShowToast { message: String },
    Api {
        request_id: String,
        method: String,
        params: Value,
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

// ── ABI exports ───────────────────────────────────────────────────────────────

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

// ── Event routing ─────────────────────────────────────────────────────────────

fn handle_event(event: &V3Event) -> UiUpdateV3 {
    match event {
        V3Event::Frame(frame) => handle_frame(frame),
        V3Event::Action(action) => handle_action(action),
        V3Event::Mouse(_)
        | V3Event::Key(_)
        | V3Event::FocusChanged(_)
        | V3Event::Tick
        | V3Event::ThemeChanged
        | V3Event::ReloadRequested
        | V3Event::ApiResult(_) => render_update(Vec::new()),
    }
}

fn handle_frame(frame: &UiFrameContext) -> UiUpdateV3 {
    *STRIP_ORIGIN.lock().expect("strip origin mutex poisoned") = panel_origin(frame);
    render_update(Vec::new())
}

fn handle_action(action: &ActionEvent) -> UiUpdateV3 {
    // Act on press only; the matching left_up would double-run every handler.
    if action.kind.as_deref() != Some("left_down") {
        return render_update(Vec::new());
    }

    if action.action_id == ACTION_TOGGLE_LIST {
        let expanded = !LIST_EXPANDED.load(Ordering::SeqCst);
        LIST_EXPANDED.store(expanded, Ordering::SeqCst);
        return render_update(Vec::new());
    }

    // A registry button: invoke its declared manifest action. The host never
    // interprets action_id (spec §4); it routes back here, and we ask the host
    // to run the matching `[[actions]]` entry via the in-process API bridge.
    if BUTTONS.iter().any(|b| b.action_id == action.action_id) {
        return render_update(vec![HostEffectV3::Api {
            request_id: format!("button-dash:invoke:{}", action.action_id),
            method: "plugin.action.invoke".to_string(),
            params: json!({
                "plugin_id": PLUGIN_ID,
                "action_id": action.action_id,
            }),
        }]);
    }

    render_update(Vec::new())
}

/// Absolute origin of this module's generic `Panel` rect. The host resolves the
/// panel's placement (from the manifest `[ui_modules.placement]` DATA) and hands
/// the resolved rect back as the frame's `rect`, so the guest is agnostic of
/// WHERE it renders — it just offsets its absolute hit-rects by `rect` origin.
/// A zero-sized rect (no slot carved yet) yields `None`.
fn panel_origin(frame: &UiFrameContext) -> Option<(u16, u16)> {
    if frame.rect.width == 0 || frame.rect.height == 0 {
        return None;
    }
    Some((frame.rect.x, frame.rect.y))
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render_update(effects: Vec<HostEffectV3>) -> UiUpdateV3 {
    UiUpdateV3 {
        scope: "partial",
        layers: vec![button_dash_layer()],
        effects,
    }
}

/// Renders the button dash in the bottom-dash slot: row 0 is the compact clickable
/// strip (each button shows label + shortcut, plus a list toggle); when
/// expanded, rows 1..N list every button's label, shortcut, and description.
fn button_dash_layer() -> UiLayer {
    let mut nodes: Vec<UiNode> = Vec::new();
    let mut hits: Vec<UiHitRect> = Vec::new();

    // Row 0: compact clickable button strip.
    let mut spans = Vec::new();
    let mut col = 0u16;
    push_gap(&mut spans, &mut col, " ");
    for button in BUTTONS {
        let label = format!("[{} {}]", button.label, button.shortcut);
        push_button(
            &mut spans,
            &mut hits,
            &mut col,
            0,
            &label,
            button.action_id,
            "accent",
        );
        push_gap(&mut spans, &mut col, "  ");
    }
    let expanded = LIST_EXPANDED.load(Ordering::SeqCst);
    let toggle_label = if expanded {
        "[\u{25BC} shortcuts]" // ▼
    } else {
        "[\u{25B6} shortcuts]" // ▶
    };
    push_button(
        &mut spans,
        &mut hits,
        &mut col,
        0,
        toggle_label,
        ACTION_TOGGLE_LIST,
        "teal",
    );
    nodes.push(UiNode::Line { spans });

    // Expanded rows: full reference (label + shortcut + description).
    if expanded {
        for (i, button) in BUTTONS.iter().enumerate() {
            let row = (i + 1) as u16;
            let label = format!("[{} {}]", button.label, button.shortcut);
            let mut spans = Vec::new();
            let mut col = 0u16;
            push_gap(&mut spans, &mut col, "  ");
            push_button(
                &mut spans,
                &mut hits,
                &mut col,
                row,
                &label,
                button.action_id,
                "accent",
            );
            push_gap(&mut spans, &mut col, "  ");
            spans.push(span(button.description, "muted"));
            nodes.push(UiNode::Line { spans });
        }
    }

    // Hit-testing is absolute (screen coordinates); nodes are strip-relative.
    // Without a known origin yet (no frame received), declare no hits rather than
    // clickable rects at the wrong position.
    match *STRIP_ORIGIN.lock().expect("strip origin mutex poisoned") {
        Some((ox, oy)) => {
            for hit in &mut hits {
                hit.x = hit.x.saturating_add(ox);
                hit.y = hit.y.saturating_add(oy);
            }
        }
        None => hits.clear(),
    }

    UiLayer {
        surface: UiSurface::Panel,
        z_index: 50,
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn action(action_id: &str, kind: &str) -> ActionEvent {
        ActionEvent {
            action_id: action_id.to_string(),
            kind: Some(kind.to_string()),
        }
    }

    #[test]
    fn every_registry_button_has_nonempty_fields() {
        for button in BUTTONS {
            assert!(!button.label.is_empty());
            assert!(!button.shortcut.is_empty());
            assert!(!button.description.is_empty());
            assert!(!button.action_id.is_empty());
        }
    }

    #[test]
    fn clicking_a_registry_button_emits_plugin_action_invoke() {
        let update = handle_action(&ActionEvent {
            action_id: "restart-hr".to_string(),
            kind: Some("left_down".to_string()),
        });
        let effect = update.effects.first().expect("one effect");
        match effect {
            HostEffectV3::Api { method, params, .. } => {
                assert_eq!(method, "plugin.action.invoke");
                assert_eq!(params["plugin_id"], json!(PLUGIN_ID));
                assert_eq!(params["action_id"], json!("restart-hr"));
            }
            other => panic!("expected Api effect, got {other:?}"),
        }
    }

    #[test]
    fn invoke_effect_serializes_to_expected_shape() {
        let update = handle_action(&ActionEvent {
            action_id: "restart-hr".to_string(),
            kind: Some("left_down".to_string()),
        });
        let encoded = serde_json::to_string(&update.effects[0]).expect("serialize");
        assert_eq!(
            encoded,
            r#"{"kind":"api","request_id":"button-dash:invoke:restart-hr","method":"plugin.action.invoke","params":{"action_id":"restart-hr","plugin_id":"builtin.button-dash"}}"#
        );
    }

    #[test]
    fn action_up_does_not_invoke() {
        let update = handle_action(&ActionEvent {
            action_id: "restart-hr".to_string(),
            kind: Some("left_up".to_string()),
        });
        assert!(update.effects.is_empty());
    }

    #[test]
    fn toggle_list_flips_state_without_effects() {
        LIST_EXPANDED.store(false, Ordering::SeqCst);
        let update = handle_action(&action(ACTION_TOGGLE_LIST, "left_down"));
        assert!(update.effects.is_empty());
        assert!(LIST_EXPANDED.load(Ordering::SeqCst));
        handle_action(&action(ACTION_TOGGLE_LIST, "left_down"));
        assert!(!LIST_EXPANDED.load(Ordering::SeqCst));
    }

    #[test]
    fn expanded_layer_has_a_row_per_button_plus_strip() {
        LIST_EXPANDED.store(true, Ordering::SeqCst);
        let layer = button_dash_layer();
        // strip row + one detail row per button
        assert_eq!(layer.nodes.len(), 1 + BUTTONS.len());
        LIST_EXPANDED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn hits_are_offset_by_strip_origin() {
        *STRIP_ORIGIN.lock().unwrap() = Some((10, 5));
        let layer = button_dash_layer();
        assert!(layer.hits.iter().all(|h| h.x >= 10 && h.y >= 5));
        *STRIP_ORIGIN.lock().unwrap() = None;
        let layer = button_dash_layer();
        assert!(layer.hits.is_empty());
    }

    #[test]
    fn action_event_parses_with_kind() {
        let json = r#"{"type":"action","action_id":"restart-hr","surface":"usage_strip","column":3,"row":0,"kind":"left_down"}"#;
        match serde_json::from_str::<V3Event>(json).expect("parse") {
            V3Event::Action(a) => {
                assert_eq!(a.action_id, "restart-hr");
                assert_eq!(a.kind.as_deref(), Some("left_down"));
            }
            other => panic!("expected Action, got {other:?}"),
        }
    }
}
