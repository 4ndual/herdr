//! doctor-dash guest: a `🩺` shortcut button in the bottom dash that opens an
//! overlay subpanel of one-click healers.
//!
//! ```text
//!  bottom dash:  … [🍴 ⌃⌥F] [▶]  [🩺 ✗1]
//!                                   │ click
//!                                   ▼
//!   ╭──────────────────────────────────────────╮
//!   │ doctor    ✓1 ✗1 ?1                       │
//!   │ ─────────────────────────────────────────│
//!   │  ▸ ✗ cx / CCR              broken        │
//!   │      gateway not answering · click to fix│
//!   │    ⟳ hr / herdr server     working       │
//!   │      restarting · escalated to an agent  │
//!   │    ? disk errors           unknown       │
//!   │      smartctl unavailable                │
//!   │                                          │
//!   │  ⟳ recheck   ⚙ fix   esc close           │
//!   │  ↑↓ select  click a broken row to fix    │
//!   ╰──────────────────────────────────────────╯
//! ```
//!
//! ## Where the data comes from — the guest runs nothing
//!
//! A zero-import wasm guest has no filesystem and no process spawn, so it can
//! neither read `~/.config/herdr/fixers.json` nor run a `check`. The split is:
//!
//! * `scripts/doctor.sh` owns the manifest path, runs every fixer's
//!   `check` / `detail` / `fix` / `escalate`, and writes a snapshot into the
//!   module's `HERDR_PLUGIN_STATE_DIR`.
//! * The host reads that file every frame (manifest `ui_input_file`, spec §7.1)
//!   and injects it as `HERDR_PLUGIN_UI_INPUT_JSON`.
//! * This guest parses the blob ([`model::parse`]) and renders it.
//!
//! The fixer list is therefore pure DATA end to end. A fourth entry appended to
//! `fixers.json` shows up with no edit to this crate, no rebuild and no
//! manifest change.
//!
//! ## ASYNC — how a slow fix stays off the render loop
//!
//! The plugin layer already has an async story and this uses it rather than
//! inventing one. `App::start_plugin_command`
//! (`src/app/api/plugins/runtime.rs`) spawns every plugin action on its own
//! `std::thread`, returns immediately, and reports completion later as an
//! `AppEvent::PluginCommandFinished`. So the guest's entire involvement in a fix
//! is emitting one `plugin.action.invoke` effect and returning — it never waits,
//! and the render loop is never blocked no matter how long the repair takes.
//!
//! Progress comes back the same way the data does: `doctor.sh` rewrites the
//! snapshot when a fix starts (row → `working`), when it escalates, and when it
//! finishes, and the next frame picks it up. To cover the sub-second gap before
//! the first rewrite lands, a clicked row is held in [`PENDING`] and rendered as
//! `working` locally until the snapshot generation changes.
//!
//! Note what is deliberately NOT used: `HostEffect::Api` runs synchronously
//! in-process, so blocking methods are refused by the host's own bridge policy
//! (`src/ui_plugin_api_bridge.rs`). `plugin.action.invoke` is safe precisely
//! because it only *starts* a thread.
//!
//! ## KEYBOARD — a real limitation, stated rather than papered over
//!
//! `App::handle_key` (`src/app/input/mod.rs:79`) forwards every key to plugins
//! and then, unconditionally, continues to herdr's own handling — the return
//! value means "layers changed", not "consumed". There is no way for a guest to
//! swallow a key. So while this overlay is open, every keystroke ALSO reaches
//! the focused pane.
//!
//! Consequence: `↑ ↓` (history recall in a shell) and `escape` are bound,
//! because their leak is benign and they are recoverable. `enter` is NOT bound
//! to anything, even though the settings overlay uses `⏎ apply` — binding it
//! would invite pressing return over a live Claude pane, which submits whatever
//! is in its prompt box. Running a fix is mouse-only for that reason, and the
//! footer advertises exactly what works. Fixing this properly needs a host-side
//! key-consume protocol; see the report.

mod model;
mod render;

use herdr_ui_plugin_api::guest::flatten_buffer;
use herdr_ui_plugin_api::{UiFrameContext, UiHitRect, UiLayer, UiNode, UiSurface};
use model::Snapshot;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

/// This plugin's manifest id; sent with every `plugin.action.invoke` so the host
/// resolves the action against THIS plugin.
const PLUGIN_ID: &str = "builtin.doctor-dash";

/// Manifest action ids (see `herdr-plugin.toml`).
const ACTION_CHECK_ALL: &str = "check-all";
const ACTION_RUN_FIX: &str = "run-fix";

/// Hit-rect id of the bottom-dash button.
const ACTION_OPEN: &str = "doctor:open";

/// Frame-env key carrying the runner's snapshot (host-injected, spec §7.1).
const ENV_UI_INPUT: &str = "HERDR_PLUGIN_UI_INPUT_JSON";

/// z-order. The button sits BELOW button-dash's 50 so `layout_plugin_panels`
/// carves button-dash's slot first and this one lands to its left, inside the
/// same shortcut-button area. The overlay sits above everything.
const Z_BUTTON: i16 = 40;
const Z_OVERLAY: i16 = 200;

// ── guest-persistent state ───────────────────────────────────────────────────

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static OPEN: AtomicBool = AtomicBool::new(false);
static SELECTED: AtomicUsize = AtomicUsize::new(0);
static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);
/// Absolute rect of the `Overlay` surface (the pane area) from the most recent
/// overlay frame. Hit-rects are absolute, so they are these plus the renderer's
/// relative geometry.
static OVERLAY_RECT: Mutex<Option<(u16, u16, u16, u16)>> = Mutex::new(None);
/// Absolute rect of the `Panel` surface (the button slot).
static PANEL_RECT: Mutex<Option<(u16, u16, u16, u16)>> = Mutex::new(None);
/// Rows whose fix was just requested, with the snapshot generation current at
/// click time. They render as `working` until the runner's snapshot moves on —
/// this is the only thing bridging the gap between the click and the first
/// rewrite, and it self-expires so a dead runner cannot leave a row spinning
/// forever (the generation changes on the next `check-all`).
static PENDING: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

// ── v3 wire types ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum V3Event {
    Frame(UiFrameContext),
    #[allow(dead_code)]
    Mouse(Value),
    Key(KeyInput),
    #[allow(dead_code)]
    FocusChanged(Value),
    Tick,
    ThemeChanged,
    ReloadRequested,
    Action(ActionInput),
    #[allow(dead_code)]
    ApiResult(Value),
}

#[derive(Debug, Deserialize)]
struct KeyInput {
    key: String,
}

#[derive(Debug, Deserialize)]
struct ActionInput {
    action_id: String,
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
}

#[derive(Debug, Serialize)]
struct UiUpdateV3 {
    scope: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    layers: Vec<UiLayer>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    effects: Vec<HostEffectV3>,
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
    on_event_bytes(bytes)
}

/// The pointer-free half of [`herdr_ui_on_event`]. Split out so the ABI's
/// error path is testable on the host: `ptr as i32` truncates a 64-bit host
/// pointer, so a native test can never call the extern fn directly.
fn on_event_bytes(bytes: &[u8]) -> i32 {
    // An unparseable event must still produce a valid update: rendering the
    // current state is always safe, and never leaves the panel blank.
    let update = match serde_json::from_slice::<V3Event>(bytes) {
        Ok(event) => handle_event(&event),
        Err(_) => render_update(Vec::new()),
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

// ── event routing ────────────────────────────────────────────────────────────

fn handle_event(event: &V3Event) -> UiUpdateV3 {
    match event {
        V3Event::Frame(frame) => handle_frame(frame),
        V3Event::Action(action) => handle_action(&action.action_id),
        V3Event::Key(key) => handle_key(&key.key),
        V3Event::Tick
        | V3Event::ThemeChanged
        | V3Event::ReloadRequested
        | V3Event::Mouse(_)
        | V3Event::FocusChanged(_)
        | V3Event::ApiResult(_) => render_update(Vec::new()),
    }
}

fn handle_frame(frame: &UiFrameContext) -> UiUpdateV3 {
    let rect = (
        frame.rect.x,
        frame.rect.y,
        frame.rect.width,
        frame.rect.height,
    );
    match frame.surface {
        UiSurface::Overlay => *OVERLAY_RECT.lock().expect("overlay rect poisoned") = Some(rect),
        UiSurface::Panel => *PANEL_RECT.lock().expect("panel rect poisoned") = Some(rect),
        _ => {}
    }
    ingest_snapshot(frame.host.env.get(ENV_UI_INPUT).map(String::as_str));
    render_update(Vec::new())
}

/// Adopt the host-injected snapshot and reconcile local state against it:
/// expire optimistic `working` flags whose generation has moved on, and clamp
/// the selection if the fixer list shrank.
fn ingest_snapshot(raw: Option<&str>) {
    let snapshot = model::parse(raw);
    {
        let mut pending = PENDING.lock().expect("pending poisoned");
        pending.retain(|(_, generation)| *generation == snapshot.generation);
    }
    let len = snapshot.rows.len();
    if len == 0 {
        SELECTED.store(0, Ordering::SeqCst);
    } else if SELECTED.load(Ordering::SeqCst) >= len {
        SELECTED.store(len - 1, Ordering::SeqCst);
    }
    *SNAPSHOT.lock().expect("snapshot poisoned") = Some(snapshot);
}

fn handle_action(action_id: &str) -> UiUpdateV3 {
    if action_id == ACTION_OPEN {
        // Opening runs every check, per the contract: the panel never shows a
        // stale verdict.
        OPEN.store(true, Ordering::SeqCst);
        return render_update(vec![invoke(ACTION_CHECK_ALL, None)]);
    }
    if !OPEN.load(Ordering::SeqCst) {
        return render_update(Vec::new());
    }
    match action_id {
        render::ACTION_CLOSE => {
            OPEN.store(false, Ordering::SeqCst);
            render_update(Vec::new())
        }
        render::ACTION_RECHECK => render_update(vec![invoke(ACTION_CHECK_ALL, None)]),
        render::ACTION_FIX_SELECTED => {
            let selected = SELECTED.load(Ordering::SeqCst);
            match selected_id(selected) {
                Some(id) => render_update(start_fix(&id)),
                None => render_update(Vec::new()),
            }
        }
        other => match other.strip_prefix(render::ACTION_ROW_PREFIX) {
            Some(id) => {
                let id = id.to_string();
                let snapshot = current_snapshot();
                let index = snapshot.rows.iter().position(|row| row.id == id);
                if let Some(index) = index {
                    SELECTED.store(index, Ordering::SeqCst);
                }
                // Selecting a broken row runs its fix — that is the whole
                // interaction. Healthy / unknown / working rows only select.
                let fixable = index
                    .and_then(|i| snapshot.rows.get(i))
                    .map(|row| row.state.is_fixable())
                    .unwrap_or(false);
                if fixable {
                    render_update(start_fix(&id))
                } else {
                    render_update(Vec::new())
                }
            }
            None => render_update(Vec::new()),
        },
    }
}

/// See the KEYBOARD note at the top of this file: keys are observed, never
/// consumed, so only non-destructive bindings exist.
fn handle_key(key: &str) -> UiUpdateV3 {
    if !OPEN.load(Ordering::SeqCst) {
        return render_update(Vec::new());
    }
    let len = current_snapshot().rows.len();
    match key {
        "escape" => {
            OPEN.store(false, Ordering::SeqCst);
        }
        "up" if len > 0 => {
            let selected = SELECTED.load(Ordering::SeqCst);
            SELECTED.store(selected.saturating_sub(1), Ordering::SeqCst);
        }
        "down" if len > 0 => {
            let selected = SELECTED.load(Ordering::SeqCst);
            SELECTED.store((selected + 1).min(len - 1), Ordering::SeqCst);
        }
        _ => {}
    }
    render_update(Vec::new())
}

fn selected_id(index: usize) -> Option<String> {
    current_snapshot().rows.get(index).map(|row| row.id.clone())
}

/// Ask the host to run one repair. Returns the effects to emit; the row is
/// marked locally so it flips to `working` on this very frame instead of after
/// the runner's first rewrite.
fn start_fix(id: &str) -> Vec<HostEffectV3> {
    let generation = current_snapshot().generation;
    {
        let mut pending = PENDING.lock().expect("pending poisoned");
        if !pending.iter().any(|(pending_id, _)| pending_id == id) {
            pending.push((id.to_string(), generation));
        }
    }
    vec![
        invoke(ACTION_RUN_FIX, Some(json!({ "id": id }).to_string())),
        HostEffectV3::ShowToast {
            message: format!("doctor: fixing {id}\u{2026}"),
        },
    ]
}

/// `plugin.action.invoke` is in the host bridge's WRITE allow-list, so this
/// plugin needs `api_write` and NOT `api_input`. The payload rides in the
/// invocation context, which the host hands the command as
/// `HERDR_PLUGIN_PAYLOAD_JSON`.
fn invoke(action_id: &str, payload_json: Option<String>) -> HostEffectV3 {
    let mut params = json!({
        "plugin_id": PLUGIN_ID,
        "action_id": action_id,
    });
    if let Some(payload) = payload_json {
        params["context"] = json!({ "plugin_payload_json": payload });
    }
    HostEffectV3::Api {
        request_id: format!("doctor:invoke:{action_id}"),
        method: "plugin.action.invoke".to_string(),
        params,
    }
}

fn current_snapshot() -> Snapshot {
    SNAPSHOT
        .lock()
        .expect("snapshot poisoned")
        .clone()
        .unwrap_or_default()
}

/// The snapshot as the user should SEE it: rows whose fix was just requested
/// are forced to `working` even though the runner has not rewritten the file
/// yet.
fn effective_snapshot() -> Snapshot {
    let mut snapshot = current_snapshot();
    let pending = PENDING.lock().expect("pending poisoned").clone();
    if pending.is_empty() {
        return snapshot;
    }
    for row in &mut snapshot.rows {
        if pending.iter().any(|(id, _)| *id == row.id) {
            row.state = model::FixerState::Working;
            if row.note.is_none() {
                row.note = Some("starting\u{2026}".to_string());
            }
        }
    }
    snapshot
}

// ── rendering ────────────────────────────────────────────────────────────────

fn render_update(effects: Vec<HostEffectV3>) -> UiUpdateV3 {
    let snapshot = effective_snapshot();
    let mut layers = vec![button_layer(&snapshot)];
    layers.push(overlay_layer(&snapshot));
    UiUpdateV3 {
        scope: "partial",
        layers,
        effects,
    }
}

/// The bottom-dash button. Rendered into a content-sized buffer (not the
/// incoming panel rect) so the host can measure the natural width and carve the
/// slot — a panel that only drew once it already had a rect would deadlock.
fn button_layer(snapshot: &Snapshot) -> UiLayer {
    let line = render::button_line(snapshot);
    let width = line.width().max(1) as u16;
    let area = Rect::new(0, 0, width, 1);
    let mut buf = Buffer::empty(area);
    Paragraph::new(line).render(area, &mut buf);
    let nodes = flatten_buffer(&buf);

    // The button's hit-rect is absolute, offset from the panel rect the host
    // reported. Before the first panel frame there is no rect, so no hit-rect —
    // the button is drawn but inert for one tick.
    let hits = match *PANEL_RECT.lock().expect("panel rect poisoned") {
        Some((x, y, w, h)) if w > 0 && h > 0 => vec![UiHitRect {
            x,
            y,
            width: w.min(width),
            height: 1,
            action_id: ACTION_OPEN.to_string(),
        }],
        _ => Vec::new(),
    };

    UiLayer {
        surface: UiSurface::Panel,
        z_index: Z_BUTTON,
        consumes_mouse: false,
        nodes,
        hits,
        data: None,
    }
}

/// The overlay subpanel. Closed ⇒ an empty, non-consuming layer: it draws
/// nothing, owns no hit-rects, and clicks fall straight through to the panes.
fn overlay_layer(snapshot: &Snapshot) -> UiLayer {
    let rect = *OVERLAY_RECT.lock().expect("overlay rect poisoned");
    let (rx, ry, rw, rh) = match rect {
        Some(rect) if OPEN.load(Ordering::SeqCst) => rect,
        _ => {
            return UiLayer {
                surface: UiSurface::Overlay,
                z_index: Z_OVERLAY,
                consumes_mouse: false,
                nodes: Vec::new(),
                hits: Vec::new(),
                data: None,
            }
        }
    };
    if rw == 0 || rh == 0 {
        return UiLayer {
            surface: UiSurface::Overlay,
            z_index: Z_OVERLAY,
            consumes_mouse: false,
            nodes: Vec::new(),
            hits: Vec::new(),
            data: None,
        };
    }

    let area = Rect::new(0, 0, rw, rh);
    let mut buf = Buffer::empty(area);
    let boxes = render::overlay(snapshot, SELECTED.load(Ordering::SeqCst), area, &mut buf);
    let nodes: Vec<UiNode> = flatten_buffer(&buf);
    let hits = boxes
        .into_iter()
        .map(|hit| UiHitRect {
            x: rx.saturating_add(hit.x),
            y: ry.saturating_add(hit.y),
            width: hit.width,
            height: hit.height,
            action_id: hit.action_id,
        })
        .collect();

    UiLayer {
        surface: UiSurface::Overlay,
        z_index: Z_OVERLAY,
        // Modal: while open, clicks belong to the panel and must not also land
        // in the pane underneath.
        consumes_mouse: true,
        nodes,
        hits,
        data: None,
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate process-global state; serialize them so a parallel
    // `cargo test` cannot race, and so a panic cannot poison a peer.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        OPEN.store(false, Ordering::SeqCst);
        SELECTED.store(0, Ordering::SeqCst);
        *SNAPSHOT.lock().unwrap() = None;
        *PENDING.lock().unwrap() = Vec::new();
        *OVERLAY_RECT.lock().unwrap() = Some((0, 2, 100, 30));
        *PANEL_RECT.lock().unwrap() = Some((120, 40, 10, 1));
    }

    const FIXTURE: &str = r#"{
        "manifest_path": "/tmp/fixers.json",
        "generation": 100,
        "rows": [
            {"id":"cx","label":"cx / CCR","state":"healthy","detail":"gateway up on :3456"},
            {"id":"hr","label":"hr","state":"broken","detail":"socket missing"},
            {"id":"disk","label":"disk errors","state":"unknown","detail":"smartctl unavailable"}
        ]
    }"#;

    fn feed(raw: &str) {
        ingest_snapshot(Some(raw));
    }

    fn layer(update: &UiUpdateV3, surface: UiSurface) -> &UiLayer {
        update
            .layers
            .iter()
            .find(|layer| layer.surface == surface)
            .expect("layer present")
    }

    fn overlay_text(update: &UiUpdateV3) -> String {
        layer(update, UiSurface::Overlay)
            .nodes
            .iter()
            .map(|node| match node {
                UiNode::Line { spans } => {
                    spans.iter().map(|s| s.text.clone()).collect::<String>() + "\n"
                }
                _ => String::new(),
            })
            .collect()
    }

    fn api_invocations(update: &UiUpdateV3) -> Vec<(String, Value)> {
        update
            .effects
            .iter()
            .filter_map(|effect| match effect {
                HostEffectV3::Api { method, params, .. } => Some((method.clone(), params.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn closed_overlay_owns_no_hit_rects_and_consumes_nothing() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        let update = render_update(Vec::new());
        let overlay = layer(&update, UiSurface::Overlay);
        assert!(overlay.nodes.is_empty());
        assert!(overlay.hits.is_empty());
        assert!(
            !overlay.consumes_mouse,
            "a closed overlay must not swallow pane clicks"
        );
    }

    #[test]
    fn clicking_the_button_opens_and_runs_every_check() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        let update = handle_action(ACTION_OPEN);
        assert!(OPEN.load(Ordering::SeqCst));
        let calls = api_invocations(&update);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "plugin.action.invoke");
        assert_eq!(calls[0].1["action_id"], json!(ACTION_CHECK_ALL));
        assert_eq!(calls[0].1["plugin_id"], json!(PLUGIN_ID));
        // ... and the modal now consumes mouse.
        assert!(layer(&update, UiSurface::Overlay).consumes_mouse);
        reset();
    }

    #[test]
    fn the_button_carries_an_absolute_hit_rect_at_the_panel_origin() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        let update = render_update(Vec::new());
        let button = layer(&update, UiSurface::Panel);
        let hit = button.hits.first().expect("button hit-rect");
        assert_eq!(hit.action_id, ACTION_OPEN);
        assert_eq!((hit.x, hit.y), (120, 40));
        reset();
    }

    #[test]
    fn open_panel_renders_all_three_fixture_rows_with_their_details() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        let update = handle_action(ACTION_OPEN);
        let text = overlay_text(&update);
        for expected in [
            "cx / CCR",
            "gateway up on :3456",
            "hr",
            "socket missing",
            "disk errors",
            "smartctl unavailable",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
        reset();
    }

    #[test]
    fn clicking_a_broken_row_runs_its_fix_with_the_id_in_the_payload() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        handle_action(ACTION_OPEN);
        let update = handle_action("doctor:row:hr");
        let calls = api_invocations(&update);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["action_id"], json!(ACTION_RUN_FIX));
        assert_eq!(
            calls[0].1["context"]["plugin_payload_json"],
            json!(r#"{"id":"hr"}"#)
        );
        reset();
    }

    #[test]
    fn clicking_a_healthy_row_only_selects_it() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        handle_action(ACTION_OPEN);
        let update = handle_action("doctor:row:cx");
        assert!(
            api_invocations(&update).is_empty(),
            "no fix must be started"
        );
        assert_eq!(SELECTED.load(Ordering::SeqCst), 0);
        reset();
    }

    #[test]
    fn a_clicked_row_flips_to_working_before_the_runner_rewrites_anything() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        handle_action(ACTION_OPEN);
        let update = handle_action("doctor:row:hr");
        let text = overlay_text(&update);
        assert!(
            text.contains('\u{27F3}'),
            "working glyph ⟳ missing:\n{text}"
        );
        assert!(text.contains("working"), "{text}");
        reset();
    }

    #[test]
    fn the_optimistic_working_flag_expires_when_the_snapshot_moves_on() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        handle_action(ACTION_OPEN);
        handle_action("doctor:row:hr");
        assert_eq!(PENDING.lock().unwrap().len(), 1);
        // The runner rewrites with a new generation: hr is healthy again.
        feed(
            r#"{"manifest_path":"/tmp/fixers.json","generation":101,
                "rows":[{"id":"hr","label":"hr","state":"healthy","detail":"socket up"}]}"#,
        );
        assert!(PENDING.lock().unwrap().is_empty());
        let text = overlay_text(&render_update(Vec::new()));
        assert!(text.contains("healthy"), "{text}");
        reset();
    }

    #[test]
    fn escalation_note_from_the_runner_is_what_the_user_sees_after_a_failed_fix() {
        let _g = guard();
        reset();
        feed(
            r#"{"manifest_path":"/tmp/f.json","generation":5,
                "rows":[{"id":"cx","label":"cx / CCR","state":"working",
                         "detail":"fix failed (exit 1)","note":"escalated to an agent via cx"}]}"#,
        );
        OPEN.store(true, Ordering::SeqCst);
        let text = overlay_text(&render_update(Vec::new()));
        assert!(text.contains("escalated to an agent via cx"), "{text}");
        assert!(text.contains("fix failed (exit 1)"), "{text}");
        reset();
    }

    #[test]
    fn missing_manifest_degrades_to_an_informative_state_not_a_blank_panel() {
        let _g = guard();
        reset();
        feed(
            r#"{"manifest_path":"/Users/x/.config/herdr/fixers.json",
                "error":"fixers.json not found","generation":3,"rows":[]}"#,
        );
        OPEN.store(true, Ordering::SeqCst);
        let text = overlay_text(&render_update(Vec::new()));
        assert!(text.contains("cannot read fixers"), "{text}");
        assert!(text.contains("fixers.json not found"), "{text}");
        assert!(
            text.contains("/Users/x/.config/herdr/fixers.json"),
            "{text}"
        );
        reset();
    }

    #[test]
    fn malformed_snapshot_degrades_and_the_ui_keeps_rendering() {
        let _g = guard();
        reset();
        feed("{ not json at all");
        OPEN.store(true, Ordering::SeqCst);
        let update = render_update(Vec::new());
        let text = overlay_text(&update);
        assert!(text.contains("cannot read fixers"), "{text}");
        // Still closable — never a trap.
        assert!(layer(&update, UiSurface::Overlay)
            .hits
            .iter()
            .any(|hit| hit.action_id == render::ACTION_CLOSE));
        reset();
    }

    #[test]
    fn a_garbage_event_payload_still_produces_a_valid_update() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        let ptr = on_event_bytes(b"not an event");
        let len = herdr_ui_on_event_len();
        assert!(ptr != 0 && len > 0);
        let bytes = LAST_EVENT_RESPONSE.lock().unwrap().clone();
        let parsed: Value = serde_json::from_slice(&bytes).expect("valid json update");
        assert_eq!(parsed["scope"], json!("partial"));
        reset();
    }

    #[test]
    fn backdrop_click_and_escape_both_close() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        handle_action(ACTION_OPEN);
        handle_action(render::ACTION_CLOSE);
        assert!(!OPEN.load(Ordering::SeqCst));

        handle_action(ACTION_OPEN);
        handle_key("escape");
        assert!(!OPEN.load(Ordering::SeqCst));
        reset();
    }

    #[test]
    fn arrow_keys_move_the_selection_within_bounds() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        handle_action(ACTION_OPEN);
        handle_key("down");
        assert_eq!(SELECTED.load(Ordering::SeqCst), 1);
        handle_key("down");
        handle_key("down");
        assert_eq!(
            SELECTED.load(Ordering::SeqCst),
            2,
            "clamped at the last row"
        );
        handle_key("up");
        handle_key("up");
        handle_key("up");
        assert_eq!(
            SELECTED.load(Ordering::SeqCst),
            0,
            "clamped at the first row"
        );
        reset();
    }

    #[test]
    fn enter_is_deliberately_unbound() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        handle_action(ACTION_OPEN);
        handle_key("down"); // select the broken row
        let update = handle_key("enter");
        assert!(
            api_invocations(&update).is_empty(),
            "enter must not run a fix: keys leak to the focused pane (see KEYBOARD note)"
        );
        reset();
    }

    #[test]
    fn keys_are_ignored_while_the_panel_is_closed() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        handle_key("down");
        assert_eq!(SELECTED.load(Ordering::SeqCst), 0);
        reset();
    }

    #[test]
    fn selection_is_clamped_when_the_fixer_list_shrinks() {
        let _g = guard();
        reset();
        feed(FIXTURE);
        SELECTED.store(2, Ordering::SeqCst);
        feed(r#"{"generation":2,"rows":[{"id":"cx","label":"cx","state":"healthy"}]}"#);
        assert_eq!(SELECTED.load(Ordering::SeqCst), 0);
        reset();
    }

    #[test]
    fn a_fourth_fixer_appears_with_no_code_change() {
        // The whole point of the data-driven contract: append a row to the
        // runner's output and it renders, with no edit to this crate.
        let _g = guard();
        reset();
        feed(
            r#"{"generation":9,"rows":[
                {"id":"cx","label":"cx / CCR","state":"healthy","detail":"up"},
                {"id":"hr","label":"hr","state":"healthy","detail":"up"},
                {"id":"disk","label":"disk errors","state":"healthy","detail":"clean"},
                {"id":"ollama","label":"ollama daemon","state":"broken","detail":"not listening"}
            ]}"#,
        );
        OPEN.store(true, Ordering::SeqCst);
        let update = render_update(Vec::new());
        let text = overlay_text(&update);
        assert!(text.contains("ollama daemon"), "{text}");
        assert!(layer(&update, UiSurface::Overlay)
            .hits
            .iter()
            .any(|hit| hit.action_id == "doctor:row:ollama"));
        reset();
    }

    #[test]
    fn frame_events_record_each_surface_rect_separately() {
        let _g = guard();
        reset();
        *OVERLAY_RECT.lock().unwrap() = None;
        *PANEL_RECT.lock().unwrap() = None;
        // Built from the real API types rather than hand-written JSON, so this
        // exercises the exact wire shape the host emits (several `FocusInput`
        // fields are not `serde(default)`).
        let mut env = std::collections::HashMap::new();
        env.insert(ENV_UI_INPUT.to_string(), FIXTURE.to_string());
        let frame = UiFrameContext {
            surface: UiSurface::Overlay,
            rect: herdr_ui_plugin_api::UiLayoutRect {
                x: 3,
                y: 4,
                width: 90,
                height: 25,
            },
            focused: false,
            host: herdr_ui_plugin_api::HostSnapshot {
                focus: herdr_ui_plugin_api::FocusInput::default(),
                env,
            },
        };
        let wire = json!({ "type": "frame" })
            .as_object()
            .cloned()
            .map(|mut map| {
                if let Value::Object(frame) = serde_json::to_value(&frame).expect("frame encodes") {
                    map.extend(frame);
                }
                Value::Object(map)
            })
            .expect("event object");
        let parsed: V3Event = serde_json::from_value(wire).expect("frame parses");
        handle_event(&parsed);
        assert_eq!(*OVERLAY_RECT.lock().unwrap(), Some((3, 4, 90, 25)));
        assert!(PANEL_RECT.lock().unwrap().is_none());
        assert_eq!(current_snapshot().rows.len(), 3);
        reset();
    }

    #[test]
    fn overlay_hit_rects_are_absolute_offsets_of_the_surface_rect() {
        let _g = guard();
        reset();
        *OVERLAY_RECT.lock().unwrap() = Some((10, 5, 100, 30));
        feed(FIXTURE);
        handle_action(ACTION_OPEN);
        let update = render_update(Vec::new());
        let hits = &layer(&update, UiSurface::Overlay).hits;
        let backdrop = hits.last().expect("backdrop");
        assert_eq!((backdrop.x, backdrop.y), (10, 5));
        assert_eq!((backdrop.width, backdrop.height), (100, 30));
        for hit in hits {
            assert!(hit.x >= 10 && hit.y >= 5, "hit {hit:?} is not absolute");
        }
        reset();
    }
}
