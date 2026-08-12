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

/// Manifest action that opens the cached release notes in a pane.
const ACTION_SHOW_NOTES: &str = "show-latest-notes";

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
    // The `latest` token is not an instance and owns no binary, so neither the
    // toast nor the promotion path applies: EITHER button opens the release
    // notes. Discoverability beats button purity here — a user who right-clicks
    // it is asking the same question as one who left-clicks it, and the answer
    // is never destructive.
    if label == render::LATEST_LABEL {
        return match kind {
            UiActionKind::LeftDown | UiActionKind::RightDown => notes_effect(),
            _ => UiUpdate::default(),
        };
    }

    match kind {
        // Fire once per physical click (press only).
        UiActionKind::LeftDown => toast_effect(label),
        UiActionKind::RightDown => update_effect(label),
        _ => UiUpdate::default(),
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

    #[test]
    fn renders_panel_layer_with_per_instance_hits() {
        let _guard = test_guard();
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
        let _guard = test_guard();
        let event: UiEvent = serde_json::from_str(&frame_json("{}")).expect("frame parses");
        assert!(handle_event(&event).layers.is_empty());
    }

    #[test]
    fn instance_hits_are_disjoint_and_clamped() {
        let (_, ranges) = render::build(&sample_input());
        let panel = Rect::new(3, 9, 40, 1);
        let hits = instance_hits(panel, &ranges);
        // Two instances + `latest`.
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].x, 3); // first token at panel.x
        for pair in hits.windows(2) {
            // disjoint, in order — a token that overlaps its neighbour steals
            // its clicks, which is exactly how a click lands on the wrong
            // instance and promotes the wrong binary.
            assert!(
                pair[0].x + pair[0].width <= pair[1].x,
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
        for h in &hits {
            assert!(h.x + h.width <= panel.x + panel.width, "clamped to panel");
            assert_eq!(h.y, 9);
            assert_eq!(h.height, 1);
        }
    }

    #[test]
    fn left_click_hr_toasts_hr_detail() {
        let _guard = test_guard();
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

    fn right_click(label: &str) -> UiUpdate {
        let action: UiEvent = serde_json::from_str(&action_json(
            &format!("version:instance:{label}"),
            "right_down",
        ))
        .expect("action");
        handle_event(&action)
    }

    fn invoked_action(update: &UiUpdate) -> Option<&str> {
        update.effects.iter().find_map(|e| match e {
            HostEffect::InvokePluginAction { action_id, .. } => Some(action_id.as_str()),
            _ => None,
        })
    }

    // ── the `latest` token: notes, never a binary ──────────────────────────

    fn click_latest(kind: &str) -> UiUpdate {
        let action: UiEvent = serde_json::from_str(&action_json(
            &format!("version:instance:{}", render::LATEST_LABEL),
            kind,
        ))
        .expect("action");
        handle_event(&action)
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
        assert_eq!(invoked_action(&right_click("test")), Some(ACTION_UPDATE_TEST));
    }

    #[test]
    fn right_click_test_invokes_update_from_latest() {
        let _guard = test_guard();
        seed(input_with_test_state(InstanceState::Verified));
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
        let _guard = test_guard();
        // Only reachable with a VERIFIED test build — see the inert-button tests.
        seed(input_with_test_state(InstanceState::Verified));
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
