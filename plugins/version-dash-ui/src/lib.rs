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
    HostEffect, MouseEvent, MouseEventKind, UiEvent, UiHitRect, UiLayer, UiSurface, UiUpdate,
    UiUpdateScope,
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
// The per-instance action-id prefix is gone with the hit-rects: presses are
// resolved against the drawn token ranges now, so there is no id to dispatch.

/// Declared plugin actions (see herdr-plugin.toml `[[actions]]`). `test` pulls
/// the fork integration-tip build; `hr` may ONLY pull the exact build running in
/// `test` (staged promotion — enforced in the action script, not just hidden).
const ACTION_UPDATE_TEST: &str = "update-herdrtest-from-latest";
const ACTION_UPDATE_HR: &str = "update-hr-from-herdrtest";

/// Manifest action that opens the cached release notes in a pane.
const ACTION_SHOW_NOTES: &str = "show-latest-notes";

/// Cache of the last input we rendered, so a click can build its toast from the
/// same data the strip is showing without re-reading any file.
static LAST_INPUT: Mutex<Option<VersionRenderInput>> = Mutex::new(None);

/// The carved row's absolute rect (x, y, width), the token ranges the row was
/// drawn with, and the token the cursor is over. All three are needed because
/// hover and clicks are resolved from the RAW mouse stream (capability
/// `mouse_all`): the host suppresses raw `Move` over a module's own hit-rects.
static PANEL_RECT: Mutex<Option<(u16, u16, u16)>> = Mutex::new(None);
static RANGES: Mutex<Vec<(String, u16, u16)>> = Mutex::new(Vec::new());
static HOVERED: Mutex<Option<String>> = Mutex::new(None);

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

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
    *PANEL_RECT.lock().expect("panel rect mutex poisoned") =
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
fn render_from(input: &VersionRenderInput) -> UiUpdate {
    let hovered = HOVERED.lock().expect("hover mutex poisoned").clone();
    let (content_w, detail) = match &hovered {
        Some(label) => {
            let detail = toast_message(input, label);
            (render::hovered_width(&detail).max(1), Some(detail))
        }
        None => (render::content_width(input).max(1), None),
    };
    let content = Rect::new(0, 0, content_w, 1);
    let mut buf = Buffer::empty(content);
    // The token ranges always come from the SUMMARY layout: they are what the
    // next press is resolved against, and a press lands on the row as it was
    // before the hover swapped it (the swap is what the cursor is sitting on).
    let ranges = render::render(
        input,
        Rect::new(0, 0, render::content_width(input).max(1), 1),
        &mut Buffer::empty(Rect::new(0, 0, render::content_width(input).max(1), 1)),
    );
    *RANGES.lock().expect("ranges mutex poisoned") = ranges
        .iter()
        .map(|span| (span.label.clone(), span.start, span.width))
        .collect();
    match &detail {
        Some(detail) => render::render_hovered(detail, content, &mut buf),
        None => {
            let _ = render::render(input, content, &mut buf);
        }
    }
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);

    UiUpdate {
        layers: vec![UiLayer {
            surface: UiSurface::Panel,
            z_index: 0,
            consumes_mouse: false,
            nodes,
            // NO hit-rects: the host suppresses the raw `Move` events hover
            // needs over a module's own rects (the F5 dedupe). Presses are
            // resolved against the same ranges the row was drawn from.
            hits: Vec::<UiHitRect>::new(),
            data: None,
        }],
        ..Default::default()
    }
}

/// Which instance token covers an absolute cell, from the SAME ranges the row
/// was drawn from.
fn token_at(column: u16, row: u16) -> Option<String> {
    let (ox, oy, width) = (*PANEL_RECT.lock().expect("panel rect mutex poisoned"))?;
    if row != oy {
        return None;
    }
    let rel = column.checked_sub(ox)?;
    if rel >= width {
        return None;
    }
    RANGES
        .lock()
        .expect("ranges mutex poisoned")
        .iter()
        .find(|(_, start, w)| rel >= *start && rel < start + w)
        .map(|(label, _, _)| label.clone())
}

/// Hover reveals, clicks act.
///
/// Left-click used to TOAST the hovered instance's detail — "click to find out
/// what this is", the pattern the canon removes: on a band whose other chips all
/// do something when clicked, a describing click is indistinguishable from an
/// acting one. Detail is on hover now, and left-click is reserved: it opens the
/// release notes on the `latest` token and is inert on an instance token, which
/// owns no non-destructive primary action.
///
/// Right-click still invokes the guarded promotion. That is a deliberate,
/// documented exception to the canon's "right-click opens a menu" rule: a
/// one-row carve cannot host a menu, and the promotion is already gated on a
/// sha-bound verification receipt rather than on a dialog. The canon-shaped
/// replacement — right-click ARMS, a second right-click confirms — is specified
/// in plugin-ux-standard.md and left to the worker who owns the promotion path.
fn handle_mouse(mouse: &MouseEvent) -> UiUpdate {
    let hit = token_at(mouse.column, mouse.row);

    match mouse.kind {
        MouseEventKind::LeftDown => match hit.as_deref() {
            Some(render::LATEST_LABEL) => notes_effect(),
            // An instance token has no non-destructive primary action: its
            // detail is on hover and its promotion is on right-click.
            _ => UiUpdate::default(),
        },
        MouseEventKind::RightDown => match hit.as_deref() {
            Some(render::LATEST_LABEL) => notes_effect(),
            Some(label) => update_effect(label),
            None => UiUpdate::default(),
        },
        _ => {
            let mut hovered = HOVERED.lock().expect("hover mutex poisoned");
            if *hovered == hit {
                // No boundary crossed ⇒ no repaint.
                return UiUpdate::default();
            }
            *hovered = hit;
            drop(hovered);
            let Some(input) = LAST_INPUT.lock().expect("input mutex poisoned").clone() else {
                return UiUpdate::default();
            };
            render_from(&input)
        }
    }
}

/// Click on `latest`: open the release notes for it.
///
/// The notes are served from the collector's offline cache (`latest_notes.md`),
/// written on every successful fetch, so the click never waits on the network
/// and still works with no connectivity at all. When no notes have ever been
/// cached the action script says so in a pane rather than failing silently.
fn notes_effect() -> UiUpdate {
    UiUpdate {
        layers: Vec::new(),
        effects: vec![HostEffect::InvokePluginAction {
            plugin_id: PLUGIN_ID.to_string(),
            action_id: ACTION_SHOW_NOTES.to_string(),
            payload_json: None,
        }],
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    }
}

/// Right-click: invoke the staged-update action for the clicked instance —
/// but only when that promotion is currently permitted.
///
/// # The inert-button rule
///
/// The hazard this closes is a right-click on `hr` succeeding when it never
/// should have been offered. Promoting `test` → `hr` is allowed ONLY while
/// `test` is `Verified`: a sha-bound receipt proving the verification gate ran
/// against the exact binary now installed in herdrtest. Anything weaker — in
/// particular `UpToDate`, which an unbridged upstream binary earns merely by
/// reporting a current version number — leaves the path CLOSED.
///
/// Closed, not warned and not confirm-dialogued. "Are you sure?" is not a safety
/// mechanism; it asks the user to adjudicate a question they clicked precisely
/// because they did not want to think about it. The action is simply not
/// invoked, and a toast says why.
///
/// This is one of TWO enforcement points, and deliberately the weaker one. The
/// promotion script re-derives the same rule from the same shared model before
/// touching a byte, so hiding the action is not what makes it safe — it is what
/// makes the UI honest about what it will do.
fn update_effect(label: &str) -> UiUpdate {
    let action_id = match label {
        "hr" => ACTION_UPDATE_HR,
        "test" => ACTION_UPDATE_TEST,
        _ => return UiUpdate::default(),
    };

    let block = {
        let guard = LAST_INPUT.lock().expect("input mutex poisoned");
        match guard.as_ref() {
            // No snapshot yet ⇒ nothing can be proven ⇒ refuse. Fail closed.
            None => Some("no version data yet".to_string()),
            Some(input) => match label {
                "hr" => input.hr_promotion_block(),
                "test" => input.test_promotion_block(),
                _ => Some("unknown instance".to_string()),
            },
        }
    };

    if let Some(reason) = block {
        return UiUpdate {
            layers: Vec::new(),
            effects: vec![HostEffect::ShowToast {
                message: format!("update {label}: blocked \u{2014} {reason}"),
            }],
            scope: UiUpdateScope::Partial,
            consumed_action_ids: Vec::new(),
        };
    }

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
            // Exhaustive on purpose: a new state must be given words here
            // rather than silently inheriting someone else's label.
            let state = match inst.state {
                version_dash_model::InstanceState::Verified => "VERIFIED — safe to promote",
                version_dash_model::InstanceState::UpToDate => "version current, NOT verified",
                version_dash_model::InstanceState::Behind => "update available",
                version_dash_model::InstanceState::Broken => "BROKEN",
                version_dash_model::InstanceState::Working => "update running now",
                version_dash_model::InstanceState::AwaitingRestart => {
                    "new build installed — press \u{2303}\u{2325}X to finish"
                }
                version_dash_model::InstanceState::UnsafeRestart => {
                    "UNVERIFIED build staged — do NOT restart"
                }
                version_dash_model::InstanceState::Escalated => "handed to an agent",
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
            detail: None,
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

    /// Drive the strip the way the host does: a frame that carves the row at
    /// (0, 0), then a raw mouse event on the CELLS THAT WERE DRAWN. Returns the
    /// update the press produced.
    fn press(label: &str, kind: herdr_ui_plugin_api::MouseEventKind, edge: Edge) -> UiUpdate {
        let event: UiEvent = serde_json::from_str(&frame_json(&env_with_input(&sample_input())))
            .expect("frame parses");
        let _ = handle_event(&event);
        let (_, ranges) = render::build(&sample_input());
        let span = ranges
            .iter()
            .find(|s| s.label == label)
            .expect("token is drawn");
        let column = match edge {
            Edge::First => span.start,
            Edge::Middle => span.start + span.width / 2,
            Edge::Last => span.start + span.width - 1,
        };
        handle_event(&UiEvent::Mouse(herdr_ui_plugin_api::MouseEvent {
            kind,
            column,
            row: 0,
        }))
    }

    #[derive(Clone, Copy)]
    enum Edge {
        First,
        Middle,
        Last,
    }

    /// No hit-rects: declaring one would make the host suppress the raw `Move`
    /// events hover needs. The row is still driven — see the press tests below.
    #[test]
    fn renders_panel_layer_with_no_hit_rects() {
        let _guard = test_guard();
        let event: UiEvent = serde_json::from_str(&frame_json(&env_with_input(&sample_input())))
            .expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::Panel));
        assert!(!update.layers[0].nodes.is_empty());
        assert!(
            update.layers[0].hits.is_empty(),
            "hit rects would kill the hover the canon requires"
        );
    }

    /// The contract that actually breaks in the field is collector-output →
    /// guest-model, and it breaks SILENTLY: a field the guest cannot parse makes
    /// `handle_event` return early and the host keeps the last good frame, so a
    /// strip that renders nothing looks identical to one that has no data yet.
    /// `sample_input()` cannot catch that — it is re-serialized from the guest's
    /// own model. This is the literal bytes `version-dash-collector` wrote on
    /// 2026-08-13, so a model change that outruns the collector fails HERE.
    #[test]
    fn renders_from_the_shipped_collector_snapshot() {
        let _guard = test_guard();
        let raw = r#"{"instances":[{"label":"hr","version":"0.8.0","local_changes":193,"state":"up_to_date","detail":null},{"label":"test","version":"0.8.0","local_changes":193,"state":"up_to_date","detail":null}],"latest":"0.8.0","notes_url":"https://github.com/herdrdev/herdr/releases/tag/v0.8.0","latest_date":null,"age_label":"just now","stale":false,"error":null,"pipeline":null,"staged_build":null}"#;
        let env = format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(raw).unwrap()
        );
        let event: UiEvent = serde_json::from_str(&frame_json(&env)).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(
            update.layers.len(),
            1,
            "collector snapshot must render a layer"
        );
        assert!(!update.layers[0].nodes.is_empty());
    }

    #[test]
    fn missing_input_env_renders_nothing() {
        let _guard = test_guard();
        let event: UiEvent = serde_json::from_str(&frame_json("{}")).expect("frame parses");
        assert!(handle_event(&event).layers.is_empty());
    }

    /// The token ranges a press is resolved against must be disjoint and in
    /// order: a token that overlaps its neighbour steals its clicks, which is
    /// exactly how a press lands on the wrong instance and promotes the wrong
    /// binary.
    #[test]
    fn token_ranges_are_disjoint_and_in_order() {
        let (_, ranges) = render::build(&sample_input());
        assert_eq!(ranges.len(), 3, "two instances + latest");
        for pair in ranges.windows(2) {
            assert!(
                pair[0].start + pair[0].width <= pair[1].start,
                "{:?} overlaps {:?}",
                (pair[0].start, pair[0].width),
                (pair[1].start, pair[1].width),
            );
        }
    }

    /// Canon §2: HOVERING an instance token reveals its detail in place. It
    /// used to require a left-click, which toasted — "click to find out what
    /// this is" on a band whose other chips all act when clicked.
    #[test]
    fn hovering_hr_shows_hr_detail_in_place() {
        use herdr_ui_plugin_api::MouseEventKind;
        let _guard = test_guard();
        let update = press("hr", MouseEventKind::Move, Edge::Middle);
        let text = text_of(&update);
        assert!(update.effects.is_empty(), "detail is drawn, never toasted");
        assert!(text.contains("hr 0.7.3"), "{text}");
        assert!(text.contains("+107 local"), "{text}");
        assert!(text.contains("update available"), "{text}");
    }

    /// Canon §1: left-click is reserved for a primary action. An instance token
    /// has none that is safe, so it is inert — and inert must be provably inert,
    /// not accidentally dead.
    #[test]
    fn left_click_on_an_instance_is_inert() {
        use herdr_ui_plugin_api::MouseEventKind;
        let _guard = test_guard();
        for edge in [Edge::First, Edge::Middle, Edge::Last] {
            let update = press("hr", MouseEventKind::LeftDown, edge);
            assert!(
                update.effects.is_empty(),
                "an instance token must not act on left-click"
            );
        }
    }

    /// Canon §10: the first and last cell of a token are as clickable as its
    /// middle. `latest` is the token with a real left-click action.
    #[test]
    fn the_edges_of_the_latest_token_open_the_notes() {
        use herdr_ui_plugin_api::MouseEventKind;
        let _guard = test_guard();
        for edge in [Edge::First, Edge::Middle, Edge::Last] {
            let update = press(render::LATEST_LABEL, MouseEventKind::LeftDown, edge);
            assert_eq!(
                invoked_action(&update),
                Some(ACTION_SHOW_NOTES),
                "an edge cell of the latest token is dead"
            );
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

    /// `LAST_INPUT` is process-global guest state, so tests that seed it must
    /// not run concurrently — otherwise one test's pipeline status leaks into
    /// another's click and the failure looks like a logic bug. Every test that
    /// seeds or clicks takes this lock first.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Take the serialization lock, tolerating a previous test's panic.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Seed the guest's cached input, as a frame would.
    fn seed(input: VersionRenderInput) {
        *LAST_INPUT.lock().unwrap() = Some(input);
    }

    fn input_with_test_state(state: InstanceState) -> VersionRenderInput {
        VersionRenderInput {
            instances: vec![
                inst("hr", "0.7.3", Some(107), InstanceState::UpToDate),
                inst("test", "0.7.5", Some(0), state),
            ],
            latest: Some("0.7.5".into()),
            ..Default::default()
        }
    }

    /// Right-press a token on the cells it was drawn at. Note this seeds a
    /// frame from `sample_input()`, then presses; tests that need a DIFFERENT
    /// input seed it after the frame via LAST_INPUT, which is what the guest
    /// itself reads when it acts.
    fn right_click(label: &str) -> UiUpdate {
        press_seeded(
            label,
            herdr_ui_plugin_api::MouseEventKind::RightDown,
            Edge::Middle,
        )
    }

    /// Like `press` but keeps whatever LAST_INPUT the test has already seeded:
    /// the geometry comes from the sample layout, the DECISION comes from the
    /// seeded input.
    fn press_seeded(
        label: &str,
        kind: herdr_ui_plugin_api::MouseEventKind,
        edge: Edge,
    ) -> UiUpdate {
        let seeded = LAST_INPUT.lock().expect("input mutex poisoned").clone();
        let event: UiEvent = serde_json::from_str(&frame_json(&env_with_input(&sample_input())))
            .expect("frame parses");
        let _ = handle_event(&event);
        // Restore unconditionally, INCLUDING None: "no snapshot at all" is a
        // case the fail-closed tests depend on, and a frame that seeded the
        // sample would quietly hand them data they asked not to have.
        *LAST_INPUT.lock().expect("input mutex poisoned") = seeded;
        let (_, ranges) = render::build(&sample_input());
        let span = ranges
            .iter()
            .find(|s| s.label == label)
            .expect("token is drawn");
        let column = match edge {
            Edge::First => span.start,
            Edge::Middle => span.start + span.width / 2,
            Edge::Last => span.start + span.width - 1,
        };
        handle_event(&UiEvent::Mouse(herdr_ui_plugin_api::MouseEvent {
            kind,
            column,
            row: 0,
        }))
    }

    fn invoked_action(update: &UiUpdate) -> Option<&str> {
        update.effects.iter().find_map(|e| match e {
            HostEffect::InvokePluginAction { action_id, .. } => Some(action_id.as_str()),
            _ => None,
        })
    }

    // ── the `latest` token: notes, never a binary ──────────────────────────

    fn click_latest(kind: &str) -> UiUpdate {
        let kind = match kind {
            "left_down" => herdr_ui_plugin_api::MouseEventKind::LeftDown,
            _ => herdr_ui_plugin_api::MouseEventKind::RightDown,
        };
        press_seeded(render::LATEST_LABEL, kind, Edge::Middle)
    }

    #[test]
    fn either_click_on_latest_opens_the_release_notes() {
        let _guard = test_guard();
        seed(input_with_test_state(InstanceState::Verified));
        for kind in ["left_down", "right_down"] {
            assert_eq!(
                invoked_action(&click_latest(kind)),
                Some(ACTION_SHOW_NOTES),
                "clicking latest ({kind}) must open the notes"
            );
        }
    }

    /// The dangerous confusion would be `latest` inheriting instance semantics —
    /// a right-click there must never reach a promotion action, whatever state
    /// the instances are in.
    #[test]
    fn clicking_latest_never_invokes_a_promotion() {
        let _guard = test_guard();
        for state in InstanceState::ALL {
            seed(input_with_test_state(state));
            for kind in ["left_down", "right_down"] {
                let update = click_latest(kind);
                let invoked = invoked_action(&update);
                assert_ne!(invoked, Some(ACTION_UPDATE_HR), "state {state:?}");
                assert_ne!(invoked, Some(ACTION_UPDATE_TEST), "state {state:?}");
            }
        }
    }

    /// The notes click must not depend on the collector having produced a
    /// snapshot yet: it reads a file cache, not the guest's in-memory input.
    #[test]
    fn latest_notes_open_even_with_no_input_seeded() {
        let _guard = test_guard();
        *LAST_INPUT.lock().unwrap() = None;
        assert_eq!(
            invoked_action(&click_latest("left_down")),
            Some(ACTION_SHOW_NOTES)
        );
    }

    // ── the inert-button rule, at the click boundary ───────────────────────

    #[test]
    fn right_click_hr_is_inert_while_test_is_unverified() {
        let _guard = test_guard();
        // UpToDate is the dangerous case: a bridge-less upstream 0.7.5 reports a
        // current version and lands here. Clicking hr must NOT invoke anything.
        for state in [
            InstanceState::UpToDate,
            InstanceState::Behind,
            InstanceState::Broken,
            InstanceState::Unknown,
            InstanceState::Working,
            InstanceState::Escalated,
            InstanceState::AwaitingRestart,
        ] {
            seed(input_with_test_state(state));
            let update = right_click("hr");
            assert!(
                invoked_action(&update).is_none(),
                "hr promotion must not be invoked while test is {state:?}"
            );
            // …and the refusal is explained rather than silent.
            assert!(
                matches!(update.effects.first(), Some(HostEffect::ShowToast { message }) if message.contains("blocked")),
                "{state:?} must explain the refusal, got {:?}",
                update.effects
            );
        }
    }

    #[test]
    fn right_click_hr_works_once_test_is_verified() {
        let _guard = test_guard();
        seed(input_with_test_state(InstanceState::Verified));
        let update = right_click("hr");
        assert_eq!(
            invoked_action(&update),
            Some(ACTION_UPDATE_HR),
            "effects were {:?}",
            update.effects
        );
    }

    #[test]
    fn right_click_with_no_data_at_all_fails_closed() {
        let _guard = test_guard();
        *LAST_INPUT.lock().unwrap() = None;
        for label in ["hr", "test"] {
            let update = right_click(label);
            assert!(
                invoked_action(&update).is_none(),
                "{label} must fail closed with no snapshot"
            );
        }
    }

    #[test]
    fn right_click_test_is_blocked_while_a_run_is_in_flight() {
        let _guard = test_guard();
        let mut input = input_with_test_state(InstanceState::Verified);
        input.pipeline = Some(version_dash_model::PipelineStatus {
            stage: Some(version_dash_model::PipelineStage::Test),
            phase: version_dash_model::PipelinePhase::Running,
            ..Default::default()
        });
        seed(input);
        assert!(invoked_action(&right_click("test")).is_none());
        assert!(invoked_action(&right_click("hr")).is_none());
    }

    #[test]
    fn right_click_test_still_works_when_test_is_broken() {
        let _guard = test_guard();
        // Rebuilding herdrtest is the recovery path; a broken test must not lock
        // the user out of the button that fixes it.
        seed(input_with_test_state(InstanceState::Broken));
        assert_eq!(
            invoked_action(&right_click("test")),
            Some(ACTION_UPDATE_TEST)
        );
    }

    #[test]
    fn right_click_test_invokes_update_from_latest() {
        let _guard = test_guard();
        seed(input_with_test_state(InstanceState::Verified));
        let update = right_click("test");
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
        let _guard = test_guard();
        // Only reachable with a VERIFIED test build — see the inert-button tests.
        seed(input_with_test_state(InstanceState::Verified));
        let update = right_click("hr");
        match &update.effects[0] {
            HostEffect::InvokePluginAction { action_id, .. } => {
                assert_eq!(action_id, ACTION_UPDATE_HR);
            }
            other => panic!("expected invoke, got {other:?}"),
        }
    }

    #[test]
    fn left_up_is_ignored() {
        let _guard = test_guard();
        let action: UiEvent =
            serde_json::from_str(&action_json("version:instance:hr", "left_up")).expect("action");
        assert!(handle_event(&action).effects.is_empty());
    }

    #[test]
    fn unknown_action_id_ignored() {
        let _guard = test_guard();
        let action: UiEvent =
            serde_json::from_str(&action_json("something:else", "left_down")).expect("action");
        assert!(handle_event(&action).effects.is_empty());
    }
}
