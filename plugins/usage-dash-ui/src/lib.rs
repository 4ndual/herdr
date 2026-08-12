mod render;

use herdr_ui_plugin_api::{
    HostEffect, MouseEventKind, UiActionKind, UiEvent, UiLayer, UiSurface, UiUpdate, UiUpdateScope,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Mutex;
use usage_dash_model::{Freshness, UsageRenderInput, VersionToken};

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Guest-owned strip collapse state, toggled by clicks on
/// [`render::ACTION_TOGGLE_COLLAPSE`]. Re-synced from the host's carved rect
/// height every frame (a 1-row rect always means collapsed), so host state
/// wins after restarts and hot-reloads — same pattern as goal-dash's panel.
static COLLAPSED: AtomicBool = AtomicBool::new(false);

/// Guest-owned expand / stats / scroll state.
///
/// These used to live in the host's `App::usage_view` and arrive in the render
/// input. Nothing could actually change them: the strip declares
/// `usage:toggle_expand` and `usage:toggle_stats` as v3 hit-rects, and a
/// declared v3 hit makes the cell guest-owned, so the host's own click path
/// (`usage_host::route_mouse`, reached only `if !v3_owned_cell`) was skipped and
/// the click arrived here — where nothing handled it. Both affordances painted,
/// invited a click, and did nothing.
///
/// They are owned here now, the same way [`COLLAPSED`] already was. The host's
/// copies of these fields remain in the input envelope and are ignored; they go
/// away with `usage_host.rs` in the wave-2 host commit.
static EXPANDED: AtomicBool = AtomicBool::new(false);
/// Stats rows start shown, matching the host default this replaces.
static STATS_SHOWN: AtomicBool = AtomicBool::new(true);
/// Scroll offset within the expanded state detail. Only meaningful while
/// [`EXPANDED`]; reset whenever the detail is collapsed or goes away, so a stale
/// offset can never leave the panel scrolled past content the user cannot see.
static SCROLL: AtomicU16 = AtomicU16::new(0);

/// Interaction state for the caption's version tokens, owned by the guest.
///
/// `hovered` drives the on-hover reveal of the latest version; `confirming`
/// holds the token that has been clicked once while outdated and is waiting for
/// a confirming second click before anything restarts the pane.
static VERSION_UI: Mutex<render::VersionUi> = Mutex::new(render::VersionUi {
    hovered: None,
    confirming: None,
});

/// The most recent render input and rect, kept so a hover can repaint
/// immediately instead of waiting for the next ~1 Hz frame. Without this the
/// caption would lag a full second behind the cursor and read as broken.
static LAST_FRAME: Mutex<Option<(String, (u16, u16, u16, u16))>> = Mutex::new(None);

/// The plugin id used when asking the host to run our own update actions.
const PLUGIN_ID: &str = "builtin.usage-dash";

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

// ── Static update (returned on init; we have nothing to declare statically) ─

static STATIC_UPDATE: &str = r#"{"layers":[]}"#;

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_ptr() -> i32 {
    STATIC_UPDATE.as_ptr() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_len() -> i32 {
    STATIC_UPDATE.len() as i32
}

// ── Optional handoff state-carry exposure ──────────────────────────────────
// A live handoff instantiates the guest fresh, which reset every toggle above
// — the exact affordances #11 made work. These optional exports carry the
// user's intent across the swap; a guest that omits them keeps resetting.

static SAVED_STATE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Park the serialized toggles so `_len` measures the same buffer, mirroring the
/// `static_update_ptr`/`_len` contract. Only the four guest-owned atomics travel:
/// the render input, version-token hover and the response scratch buffer are all
/// re-derived on the next frame, so carrying them would risk staleness for
/// nothing.
#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_save_state_ptr() -> i32 {
    let state = format!(
        r#"{{"v":1,"collapsed":{},"expanded":{},"stats_shown":{},"scroll":{}}}"#,
        COLLAPSED.load(Ordering::SeqCst),
        EXPANDED.load(Ordering::SeqCst),
        STATS_SHOWN.load(Ordering::SeqCst),
        SCROLL.load(Ordering::SeqCst)
    );
    let mut guard = SAVED_STATE.lock().expect("saved state mutex poisoned");
    *guard = state.into_bytes();
    guard.as_ptr() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_save_state_len() -> i32 {
    SAVED_STATE
        .lock()
        .expect("saved state mutex poisoned")
        .len() as i32
}

/// Adopt a blob from the previous server. Returns 0 for anything unrecognised so
/// the host starts this guest fresh — the pre-existing behaviour — rather than
/// failing the handoff over a toggle.
#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_restore_state(ptr: i32, len: i32) -> i32 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize) };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return 0;
    };
    if value.get("v").and_then(serde_json::Value::as_u64) != Some(1) {
        return 0;
    }
    for (key, target) in [
        ("collapsed", &COLLAPSED),
        ("expanded", &EXPANDED),
        ("stats_shown", &STATS_SHOWN),
    ] {
        if let Some(flag) = value.get(key).and_then(serde_json::Value::as_bool) {
            target.store(flag, Ordering::SeqCst);
        }
    }
    if let Some(scroll) = value.get("scroll").and_then(serde_json::Value::as_u64) {
        SCROLL.store(scroll.min(u16::MAX as u64) as u16, Ordering::SeqCst);
    }
    1
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
        UiEvent::Mouse(m) if m.kind == MouseEventKind::Move => {
            return handle_hover(m.column, m.row)
        }
        // MouseAll broadcasts every wheel event in the terminal, so the rect
        // check is what keeps a scroll inside a pane from moving the strip.
        UiEvent::Mouse(m)
            if matches!(
                m.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) =>
        {
            if !in_last_frame(m.column, m.row) {
                return UiUpdate::default();
            }
            return handle_scroll(m.kind == MouseEventKind::ScrollDown);
        }
        _ => return UiUpdate::default(),
    };

    // v3 generic transport (spec §7.1): the host reads the plugin's declared
    // `ui_input_file` (usage_input.json) from the module's state dir and injects
    // its contents here. Replaces the wave-0 host-specific HERDR_USAGE_RENDER_JSON.
    let json = match ctx.host.env.get("HERDR_PLUGIN_UI_INPUT_JSON") {
        Some(j) if !j.is_empty() => j,
        _ => return UiUpdate::default(),
    };

    let mut input: UsageRenderInput = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return UiUpdate::default(),
    };
    apply_guest_view(&mut input);

    let area = Rect::new(ctx.rect.x, ctx.rect.y, ctx.rect.width, ctx.rect.height);
    // Host state wins: a 1-row carve is the collapsed strip, anything taller
    // is expanded. (A zero rect means "not carved" — leave the latch alone.)
    if area.height == 1 {
        COLLAPSED.store(true, Ordering::SeqCst);
    } else if area.height > 1 {
        COLLAPSED.store(false, Ordering::SeqCst);
    }
    // Remember this frame so a hover can repaint without waiting for the next
    // tick. Stored as the raw JSON to keep the static free of borrowed data.
    if let Ok(mut last) = LAST_FRAME.lock() {
        *last = Some((json.clone(), (area.x, area.y, area.width, area.height)));
    }

    let version_ui = VERSION_UI.lock().map(|u| *u).unwrap_or_default();
    let mut buf = Buffer::empty(area);
    let hits = render::render(&input, area, &mut buf, version_ui);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);

    UiUpdate {
        layers: vec![UiLayer {
            surface: UiSurface::UsageStrip,
            z_index: 0,
            consumes_mouse: false,
            nodes,
            hits,
            data: None,
        }],
        ..Default::default()
    }
}

/// Repaint from the cached frame, applying the current version-UI state.
///
/// Returns an empty update when there is nothing cached yet (no frame seen) —
/// never a blank layer, which would erase the strip.
fn repaint_from_cache() -> UiUpdate {
    let Some((json, (x, y, w, h))) = LAST_FRAME.lock().ok().and_then(|g| g.clone()) else {
        return UiUpdate::default();
    };
    let Ok(mut input) = serde_json::from_str::<UsageRenderInput>(&json) else {
        return UiUpdate::default();
    };
    apply_guest_view(&mut input);
    let area = Rect::new(x, y, w, h);
    let version_ui = VERSION_UI.lock().map(|u| *u).unwrap_or_default();
    let mut buf = Buffer::empty(area);
    let hits = render::render(&input, area, &mut buf, version_ui);
    let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);
    UiUpdate {
        layers: vec![UiLayer {
            surface: UiSurface::UsageStrip,
            z_index: 0,
            consumes_mouse: false,
            nodes,
            hits,
            data: None,
        }],
        scope: UiUpdateScope::Partial,
        ..Default::default()
    }
}

/// Track which version token the cursor is over, repainting only when the
/// answer actually changes.
///
/// The cursor generates a move event per cell, so repainting unconditionally
/// would push a full strip redraw on every mouse twitch.
fn handle_hover(column: u16, row: u16) -> UiUpdate {
    // MouseAll broadcasts EVERY move in the terminal, not just moves over this
    // surface. Reject anything outside the last frame's rect before doing any
    // work: `hovered_token` parses JSON and re-renders, which must not run on
    // every mouse twitch anywhere on screen.
    if !in_last_frame(column, row) {
        // Leaving the strip must still clear a stale hover, or the caption would
        // keep showing the expanded "→ latest" text after the cursor left.
        let changed = match VERSION_UI.lock() {
            Ok(mut ui) => {
                let changed = ui.hovered.is_some();
                ui.hovered = None;
                changed
            }
            Err(_) => false,
        };
        return if changed {
            repaint_from_cache()
        } else {
            UiUpdate::default()
        };
    }

    let token = hovered_token(column, row);
    let changed = match VERSION_UI.lock() {
        Ok(mut ui) => {
            let changed = ui.hovered != token;
            ui.hovered = token;
            changed
        }
        Err(_) => false,
    };
    if changed {
        repaint_from_cache()
    } else {
        UiUpdate::default()
    }
}

/// Resolve a cursor position against the version hit rects from the last frame.
fn hovered_token(column: u16, row: u16) -> Option<VersionToken> {
    let (json, (x, y, w, h)) = LAST_FRAME.lock().ok().and_then(|g| g.clone())?;
    let mut input = serde_json::from_str::<UsageRenderInput>(&json).ok()?;
    apply_guest_view(&mut input);
    let area = Rect::new(x, y, w, h);
    // Render into a throwaway buffer purely to recover this frame's hit rects.
    // Cheap (a few hundred cells) and keeps one definition of where tokens sit,
    // rather than duplicating the caption layout maths here.
    let version_ui = VERSION_UI.lock().map(|u| *u).unwrap_or_default();
    let mut buf = Buffer::empty(area);
    let hits = render::render(&input, area, &mut buf, version_ui);
    hits.iter()
        .find(|h| column >= h.x && column < h.x + h.width && row >= h.y && row < h.y + h.height)
        .and_then(|h| match h.action_id.as_str() {
            render::ACTION_VERSION_CLAUDE => Some(VersionToken::Claude),
            render::ACTION_VERSION_CODEX => Some(VersionToken::Codex),
            _ => None,
        })
}

/// Freshness of a token in the last rendered frame.
fn token_freshness(token: VersionToken) -> Freshness {
    let Some((json, _)) = LAST_FRAME.lock().ok().and_then(|g| g.clone()) else {
        return Freshness::Unknown;
    };
    let Ok(input) = serde_json::from_str::<UsageRenderInput>(&json) else {
        return Freshness::Unknown;
    };
    let Some(snap) = input.snapshot else {
        return Freshness::Unknown;
    };
    match token {
        VersionToken::Claude => snap.versions.claude.freshness(),
        VersionToken::Codex => snap
            .versions
            .codex
            .as_ref()
            .map(|c| c.freshness())
            .unwrap_or(Freshness::Unknown),
    }
}

/// Handle a click on a version token.
///
/// Outdated → first click arms an inline confirmation, second click on the same
/// token runs the update action (which restarts the pane). Current → a forced
/// re-check. Unknown → nothing, because we do not know an update exists.
fn handle_version_click(token: VersionToken) -> UiUpdate {
    match token_freshness(token) {
        Freshness::Outdated => {
            let confirmed = match VERSION_UI.lock() {
                Ok(mut ui) => {
                    if ui.confirming == Some(token) {
                        ui.confirming = None;
                        true
                    } else {
                        // Arming one token disarms any other.
                        ui.confirming = Some(token);
                        false
                    }
                }
                Err(_) => false,
            };
            if !confirmed {
                // Just show the inline "click to update?" prompt.
                return repaint_from_cache();
            }
            let action_id = match token {
                VersionToken::Claude => "update-claude-code",
                VersionToken::Codex => "update-codex",
            };
            let mut update = repaint_from_cache();
            update.effects.push(HostEffect::InvokePluginAction {
                plugin_id: PLUGIN_ID.to_string(),
                action_id: action_id.to_string(),
                payload_json: None,
            });
            update
        }
        Freshness::Current => {
            if let Ok(mut ui) = VERSION_UI.lock() {
                ui.confirming = None;
            }
            let mut update = repaint_from_cache();
            update.effects.push(HostEffect::InvokePluginAction {
                plugin_id: PLUGIN_ID.to_string(),
                action_id: "verify-cli-versions".to_string(),
                payload_json: None,
            });
            update
        }
        Freshness::Unknown => {
            if let Ok(mut ui) = VERSION_UI.lock() {
                ui.confirming = None;
            }
            UiUpdate::default()
        }
    }
}

/// Overwrite the host's (now inert) view fields with the guest-owned ones, so
/// every render path — live frame, cached repaint, hit-rect recovery — draws one
/// consistent state.
///
/// Also re-latches `expanded`/`scroll` off when this frame has no expandable
/// detail. That mirrors the cleanup the host used to do when the strip reported
/// no expand hit, and it is what stops a latch set on a Claude pane from leaking
/// onto the next pane, which may have nothing to expand.
fn apply_guest_view(input: &mut UsageRenderInput) {
    if !has_expandable_detail(input) {
        EXPANDED.store(false, Ordering::SeqCst);
        SCROLL.store(0, Ordering::SeqCst);
    }
    input.expanded = EXPANDED.load(Ordering::SeqCst);
    input.stats_shown = STATS_SHOWN.load(Ordering::SeqCst);
    input.scroll = SCROLL.load(Ordering::SeqCst);
}

/// Whether this frame has state detail worth expanding — the same condition
/// `render` uses to decide whether to declare the expand hit at all.
fn has_expandable_detail(input: &UsageRenderInput) -> bool {
    input
        .snapshot
        .as_ref()
        .and_then(|snap| snap.state_full.as_ref())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Scroll the expanded state detail. Ignored unless the detail is open, so the
/// wheel keeps its normal meaning over a collapsed strip.
fn handle_scroll(delta_down: bool) -> UiUpdate {
    if !EXPANDED.load(Ordering::SeqCst) {
        return UiUpdate::default();
    }
    let current = SCROLL.load(Ordering::SeqCst);
    // Saturating both ways: a wheel at the top must not wrap to u16::MAX, which
    // would scroll the detail entirely out of view with no way back.
    let next = if delta_down {
        current.saturating_add(1)
    } else {
        current.saturating_sub(1)
    };
    if next == current {
        return UiUpdate::default();
    }
    SCROLL.store(next, Ordering::SeqCst);
    repaint_from_cache()
}

/// Is `(column, row)` inside the last frame's rect?
fn in_last_frame(column: u16, row: u16) -> bool {
    LAST_FRAME
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|(_, (x, y, w, h))| {
            column >= x && column < x.saturating_add(w) && row >= y && row < y.saturating_add(h)
        })
        .unwrap_or(false)
}

fn handle_action(action_id: &str, kind: UiActionKind) -> UiUpdate {
    // Act on press only; the host routes both left_down and left_up for one
    // physical click, and reacting to both would double-fire the toggle.
    if kind != UiActionKind::LeftDown {
        return UiUpdate::default();
    }
    match action_id {
        render::ACTION_VERSION_CLAUDE => return handle_version_click(VersionToken::Claude),
        render::ACTION_VERSION_CODEX => return handle_version_click(VersionToken::Codex),
        _ => {}
    }
    // Any other click abandons a pending confirmation: clicking elsewhere is the
    // natural "no", and a confirm left armed across unrelated clicks could later
    // restart the pane from a stray click.
    if let Ok(mut ui) = VERSION_UI.lock() {
        ui.confirming = None;
    }
    // Guest-owned view toggles. Repaint from the cached frame rather than
    // waiting for the next ~1 Hz tick — a toggle that takes a second to appear
    // reads as a missed click.
    match action_id {
        "usage:toggle_expand" => {
            let expanded = !EXPANDED.load(Ordering::SeqCst);
            EXPANDED.store(expanded, Ordering::SeqCst);
            // Closing the detail drops the offset with it; reopening starts at
            // the top rather than wherever the last read left off.
            if !expanded {
                SCROLL.store(0, Ordering::SeqCst);
            }
            return repaint_from_cache();
        }
        "usage:toggle_stats" => {
            STATS_SHOWN.fetch_xor(true, Ordering::SeqCst);
            return repaint_from_cache();
        }
        _ => {}
    }
    if action_id != render::ACTION_TOGGLE_COLLAPSE {
        return UiUpdate::default();
    }
    let collapsed = !COLLAPSED.load(Ordering::SeqCst);
    COLLAPSED.store(collapsed, Ordering::SeqCst);
    UiUpdate {
        layers: Vec::new(),
        effects: vec![HostEffect::SetSurfaceCollapsed {
            surface: UiSurface::UsageStrip,
            collapsed,
        }],
        scope: UiUpdateScope::Partial,
        consumed_action_ids: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_dash_model::{Cli, UsageSnapshot};

    /// The view latches are process-wide statics and `cargo test` runs tests in
    /// parallel, so every test that asserts on them must hold this first.
    /// Poisoning is recovered from rather than propagated: one failing test
    /// should not cascade into false failures in the others.
    static TOGGLE_LOCK: Mutex<()> = Mutex::new(());

    /// Build a `frame` event JSON with `env` as the host env map. Exercises the
    /// real transport: the guest only ever sees a deserialized `UiEvent`.
    fn frame_json(env: &str) -> String {
        frame_json_with_height(env, 7)
    }

    fn frame_json_with_height(env: &str, height: u16) -> String {
        format!(
            "{{\"type\":\"frame\",\"surface\":\"usage_strip\",\
             \"rect\":{{\"x\":0,\"y\":0,\"width\":80,\"height\":{height}}},\
             \"focused\":false,\
             \"host\":{{\"focus\":{{\"session_id\":null,\"session_ref\":null,\
             \"cwd\":null,\"agent\":\"None\",\"is_self_focus\":false}},\
             \"env\":{env}}}}}"
        )
    }

    fn sample_input_json() -> String {
        let mut snap = UsageSnapshot::empty(Cli::Claude);
        snap.model = Some("Opus 4.8".into());
        snap.ctx_used_pct = Some(29.0);
        let input = UsageRenderInput {
            snapshot: Some(snap),
            expanded: false,
            collapsed: false,
            stats_shown: true,
            scroll: 0,
            now: 1_000_000,
            five_hour_abs: String::new(),
            weekly_abs: String::new(),
        };
        serde_json::to_string(&input).expect("input serializes")
    }

    #[test]
    fn renders_usage_strip_from_ui_input_env_key() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Host injects the plugin's ui_input_file contents under the generic
        // §7.1 key; the guest must render a usage_strip layer from it.
        let env = format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&sample_input_json()).expect("env value serializes")
        );
        let event: UiEvent = serde_json::from_str(&frame_json(&env)).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(matches!(update.layers[0].surface, UiSurface::UsageStrip));
        assert!(
            !update.layers[0].nodes.is_empty(),
            "usage strip should render nodes from the injected input"
        );
    }

    fn env_json() -> String {
        format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&sample_input_json()).expect("env value serializes")
        )
    }

    fn action_json(action_id: &str, kind: &str) -> String {
        format!(
            "{{\"type\":\"action\",\"action_id\":\"{action_id}\",\
             \"surface\":\"usage_strip\",\"column\":1,\"row\":0,\"kind\":\"{kind}\"}}"
        )
    }

    #[test]
    fn full_render_declares_collapse_hit() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let event: UiEvent = serde_json::from_str(&frame_json(&env_json())).expect("frame parses");
        let update = handle_event(&event);
        assert!(update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id == render::ACTION_TOGGLE_COLLAPSE));
    }

    #[test]
    fn one_row_rect_renders_expand_affordance() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let event: UiEvent =
            serde_json::from_str(&frame_json_with_height(&env_json(), 1)).expect("frame parses");
        let update = handle_event(&event);
        assert_eq!(update.layers.len(), 1);
        assert!(!update.layers[0].nodes.is_empty());
        assert!(update.layers[0]
            .hits
            .iter()
            .any(|h| h.action_id == render::ACTION_TOGGLE_COLLAPSE));
        // NOTE: no assertion on the COLLAPSED static here — tests run in
        // parallel and other frames (height 7) race on the same latch.
    }

    #[test]
    fn collapse_action_left_down_emits_set_surface_collapsed() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        COLLAPSED.store(false, Ordering::SeqCst);
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_COLLAPSE, "left_down"))
                .expect("action parses");
        let update = handle_event(&event);
        assert_eq!(update.effects.len(), 1);
        match &update.effects[0] {
            HostEffect::SetSurfaceCollapsed { surface, collapsed } => {
                assert!(matches!(surface, UiSurface::UsageStrip));
                assert!(*collapsed);
            }
            other => panic!("expected SetSurfaceCollapsed, got {other:?}"),
        }
    }

    #[test]
    fn collapse_action_ignores_left_up() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let event: UiEvent =
            serde_json::from_str(&action_json(render::ACTION_TOGGLE_COLLAPSE, "left_up"))
                .expect("action parses");
        let update = handle_event(&event);
        assert!(
            update.effects.is_empty(),
            "left_up must not toggle collapse"
        );
    }

    /// Input whose snapshot has expandable detail, so the strip declares the
    /// `usage:toggle_expand` hit.
    fn expandable_input_json() -> String {
        let mut snap = UsageSnapshot::empty(Cli::Claude);
        snap.model = Some("Opus 4.8".into());
        snap.ctx_used_pct = Some(29.0);
        snap.state_full = Some("some long state detail worth expanding".into());
        let input = UsageRenderInput {
            snapshot: Some(snap),
            expanded: false,
            collapsed: false,
            stats_shown: true,
            scroll: 0,
            now: 1_000_000,
            five_hour_abs: String::new(),
            weekly_abs: String::new(),
        };
        format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&serde_json::to_string(&input).expect("input serializes"))
                .expect("env value serializes")
        )
    }

    /// Input carrying an explicit Claude version pair, so the caption declares
    /// the `usage:version_claude` token in a known freshness state.
    fn versioned_input_json(current: Option<&str>, latest: Option<&str>) -> String {
        let mut snap = UsageSnapshot::empty(Cli::Claude);
        snap.model = Some("Opus 4.8".into());
        snap.ctx_used_pct = Some(29.0);
        snap.versions.claude.current = current.map(str::to_string);
        snap.versions.claude.latest = latest.map(str::to_string);
        let input = UsageRenderInput {
            snapshot: Some(snap),
            expanded: false,
            collapsed: false,
            stats_shown: true,
            scroll: 0,
            now: 1_000_000,
            five_hour_abs: String::new(),
            weekly_abs: String::new(),
        };
        format!(
            "{{\"HERDR_PLUGIN_UI_INPUT_JSON\":{}}}",
            serde_json::to_string(&serde_json::to_string(&input).expect("input serializes"))
                .expect("env value serializes")
        )
    }

    /// Render one frame of `versioned_input_json`.
    fn version_frame(current: Option<&str>, latest: Option<&str>) -> UiUpdate {
        let event: UiEvent =
            serde_json::from_str(&frame_json(&versioned_input_json(current, latest)))
                .expect("frame parses");
        handle_event(&event)
    }

    /// Centre of the Claude version token's hit rect in the given update.
    fn claude_token_xy(update: &UiUpdate) -> (u16, u16) {
        let hit = update.layers[0]
            .hits
            .iter()
            .find(|h| h.action_id == render::ACTION_VERSION_CLAUDE)
            .expect("the caption must declare the claude version hit");
        (hit.x + hit.width / 2, hit.y)
    }

    /// Drive one frame so `LAST_FRAME` is populated, which every repaint path
    /// depends on, then return the hit ids that frame declared.
    fn seed_frame() -> Vec<String> {
        let event: UiEvent =
            serde_json::from_str(&frame_json(&expandable_input_json())).expect("frame parses");
        let update = handle_event(&event);
        update.layers[0]
            .hits
            .iter()
            .map(|h| h.action_id.clone())
            .collect()
    }

    fn click(action_id: &str) -> UiUpdate {
        let event: UiEvent =
            serde_json::from_str(&action_json(action_id, "left_down")).expect("action parses");
        handle_event(&event)
    }

    /// The expand toggle was DEAD before this: the strip declared the hit-rect,
    /// the guest ignored the id, and the host skipped its own click path because
    /// a declared v3 hit makes the cell guest-owned
    /// (`app/input/mod.rs:304`, `if !v3_owned_cell`). It painted, invited a
    /// click, and did nothing.
    #[test]
    fn expand_toggle_flips_state_and_repaints() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        EXPANDED.store(false, Ordering::SeqCst);
        SCROLL.store(0, Ordering::SeqCst);

        let ids = seed_frame();
        assert!(
            ids.iter().any(|id| id == "usage:toggle_expand"),
            "the strip must still declare the expand hit; got {ids:?}"
        );

        let update = click("usage:toggle_expand");
        assert!(EXPANDED.load(Ordering::SeqCst), "the click must open it");
        assert!(
            !update.layers.is_empty(),
            "it must repaint immediately, not wait for the next ~1 Hz frame"
        );

        // Scroll is only live while expanded, and it must not wrap at the top.
        assert!(!handle_scroll(false).layers.is_empty() || SCROLL.load(Ordering::SeqCst) == 0);
        assert_eq!(SCROLL.load(Ordering::SeqCst), 0, "no wrap to u16::MAX");
        handle_scroll(true);
        assert_eq!(SCROLL.load(Ordering::SeqCst), 1);

        // Closing drops the offset with it.
        click("usage:toggle_expand");
        assert!(!EXPANDED.load(Ordering::SeqCst));
        assert_eq!(SCROLL.load(Ordering::SeqCst), 0, "closing resets scroll");
    }

    #[test]
    fn stats_toggle_flips_state_and_repaints() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        STATS_SHOWN.store(true, Ordering::SeqCst);
        seed_frame();

        let update = click("usage:toggle_stats");
        assert!(!STATS_SHOWN.load(Ordering::SeqCst), "the click must hide stats");
        assert!(!update.layers.is_empty(), "and repaint immediately");

        click("usage:toggle_stats");
        assert!(STATS_SHOWN.load(Ordering::SeqCst), "and toggle back");
    }

    #[test]
    fn scroll_is_inert_while_collapsed() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        EXPANDED.store(false, Ordering::SeqCst);
        SCROLL.store(0, Ordering::SeqCst);
        let update = handle_scroll(true);
        assert!(update.layers.is_empty(), "the wheel keeps its normal meaning");
        assert_eq!(SCROLL.load(Ordering::SeqCst), 0);
    }

    /// A latch set on a pane with expandable detail must not leak onto the next
    /// pane, which may have none — otherwise the strip renders as "expanded"
    /// with nothing to show and no visible way back.
    #[test]
    fn a_frame_without_detail_relatches_expand_off() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        EXPANDED.store(true, Ordering::SeqCst);
        SCROLL.store(4, Ordering::SeqCst);

        // sample_input_json's snapshot has no state_full.
        let event: UiEvent =
            serde_json::from_str(&frame_json(&env_json())).expect("frame parses");
        handle_event(&event);

        assert!(!EXPANDED.load(Ordering::SeqCst));
        assert_eq!(SCROLL.load(Ordering::SeqCst), 0);
    }

    /// All painted text of an update, as one string.
    fn painted(update: &UiUpdate) -> String {
        serde_json::to_string(&update.layers).expect("layers serialize")
    }

    /// The expanded branch must actually be REACHABLE from a click: opening it
    /// has to paint the detail region, and the wheel has to move that region.
    #[test]
    fn expanded_branch_paints_detail_and_scroll_moves_it() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        EXPANDED.store(false, Ordering::SeqCst);
        SCROLL.store(0, Ordering::SeqCst);

        // Render through the frame path each time: `repaint_from_cache` reads a
        // process-wide LAST_FRAME that the other tests also write, so only a
        // frame we hand in ourselves is deterministic under a parallel run.
        let frame = || {
            let event: UiEvent =
                serde_json::from_str(&frame_json(&expandable_input_json())).expect("frame parses");
            painted(&handle_event(&event))
        };

        let collapsed = frame();
        assert!(
            !collapsed.contains("expanding"),
            "detail must be hidden while collapsed"
        );

        click("usage:toggle_expand");
        assert!(EXPANDED.load(Ordering::SeqCst));
        let opened = frame();
        assert!(
            opened.contains("expanding"),
            "the click must paint the detail region; got {opened}"
        );

        handle_scroll(true);
        assert_eq!(SCROLL.load(Ordering::SeqCst), 1);
        assert_ne!(opened, frame(), "the wheel must move the detail region");
    }

    /// The whole point of the feature: an outdated CLI paints RED, the first
    /// click only ARMS an inline confirmation (nothing restarts the pane yet),
    /// and only the confirming second click invokes the update action.
    #[test]
    fn outdated_token_is_red_and_needs_two_clicks_to_update() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut ui) = VERSION_UI.lock() {
            *ui = render::VersionUi::default();
        }

        let update = version_frame(Some("2.1.0"), Some("2.2.0"));
        let red_frame = painted(&update);
        assert!(
            red_frame.contains("\"role\":\"danger\""),
            "outdated must paint red; got {red_frame}"
        );

        let first = click(render::ACTION_VERSION_CLAUDE);
        let armed = painted(&first);
        assert!(
            armed.contains("click to update?"),
            "the first click must only arm the confirmation; got {armed}"
        );
        assert!(
            first.effects.is_empty(),
            "arming must not invoke anything; got {:?}",
            first.effects
        );

        let confirmed = click(render::ACTION_VERSION_CLAUDE);
        assert!(
            confirmed.effects.iter().any(|e| matches!(
                e,
                HostEffect::InvokePluginAction { action_id, .. } if action_id == "update-claude-code"
            )),
            "the confirming click must invoke update-claude-code; got {:?}",
            confirmed.effects
        );
    }

    /// Hover is the only way to see what "latest" actually is, so it must reveal
    /// it in every freshness state — including the ones that are not red.
    #[test]
    fn hover_reveals_latest_for_current_and_unknown() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut ui) = VERSION_UI.lock() {
            *ui = render::VersionUi::default();
        }

        let update = version_frame(Some("2.2.0"), Some("2.2.0"));
        let (x, y) = claude_token_xy(&update);
        let hovered = painted(&handle_hover(x, y));
        assert!(
            hovered.contains("(latest 2.2.0)"),
            "hovering a current token must still name the latest; got {hovered}"
        );

        // A version we could never check must not read as up to date. Reset the
        // hover first: hovering the same token twice is a no-op repaint.
        if let Ok(mut ui) = VERSION_UI.lock() {
            *ui = render::VersionUi::default();
        }
        let update = version_frame(Some("2.2.0"), None);
        let (x, y) = claude_token_xy(&update);
        let hovered = painted(&handle_hover(x, y));
        assert!(
            hovered.contains("(latest unknown)"),
            "an unchecked token must say so; got {hovered}"
        );
    }

    /// A dead network leaves `latest` unknown. That must never paint red and a
    /// click must do nothing: we do not know an update exists.
    #[test]
    fn unknown_version_is_never_red_and_click_is_inert() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut ui) = VERSION_UI.lock() {
            *ui = render::VersionUi::default();
        }

        let update = version_frame(Some("2.2.0"), None);
        let unknown_frame = painted(&update);
        assert!(
            !unknown_frame.contains("\"role\":\"danger\""),
            "unknown must not be painted as outdated; got {unknown_frame}"
        );

        let clicked = click(render::ACTION_VERSION_CLAUDE);
        assert!(
            clicked.effects.is_empty(),
            "an unknown token has nothing to update; got {:?}",
            clicked.effects
        );
    }

    #[test]
    fn missing_ui_input_env_key_renders_nothing() {
        let _guard = TOGGLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let event: UiEvent = serde_json::from_str(&frame_json("{}")).expect("frame parses");
        let update = handle_event(&event);
        assert!(
            update.layers.is_empty(),
            "no input ⇒ empty update (loss-free under partial-default merge)"
        );
    }
}
