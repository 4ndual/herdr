//! Button-dash guest: a data-driven, clickable quick-reference of
//! keyboard-shortcut actions rendered in a generic plugin `Panel` whose position
//! is pure manifest DATA (`[ui_modules.placement]`), docked beside the usage
//! strip by default.
//!
//! Each entry in [`BUTTONS`] is one row of data — no rendering code changes to
//! add a button: append a [`ButtonDef`] here (emoji, color, shortcut, label,
//! description) and declare a matching `[[actions]]` entry in `herdr-plugin.toml`.
//!
//! Compact strip: each button shows only its **emoji + key combo** in its own
//! color (no full label). Hovering a button reveals its full label + description
//! as an in-panel tooltip row; clicking it invokes the declared host action.
//!
//! Mouse model (why we DON'T use declarative hit-rects): the host suppresses raw
//! mouse events — including hover `Move` — over a module's own declared hit-rects
//! (`should_deliver_raw_mouse`, the F5 click-dedupe). To get hover we instead
//! request the `mouse_all` capability, declare NO hit-rects, and resolve clicks
//! AND hovers ourselves from the raw `Move`/`LeftDown` stream against the button
//! rects we compute from the panel origin. This keeps everything a pure guest /
//! manifest change: adding a button, recoloring, or the whole hover-tooltip is a
//! hot-reload with NO host rebuild.

use herdr_ui_plugin_api::{UiFrameContext, UiHitRect, UiLayer, UiNode, UiSurface, UiTextSpan};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Mutex;

/// This plugin's manifest id; sent with every `plugin.action.invoke` so the
/// host resolves the action against THIS plugin (not another with a same id).
const PLUGIN_ID: &str = "builtin.button-dash";

// ── Button registry (data-driven) ────────────────────────────────────────────
// Add a button = add one row here + one `[[actions]]` entry in the manifest with
// a matching id. No rendering code changes required.

struct ButtonDef {
    /// Stable identity for the row (also the manifest `[[actions]]` id the click
    /// invokes via `plugin.action.invoke`). Mirrors `action_id`; asserted equal
    /// in tests, not read at render time.
    #[allow(dead_code)]
    id: &'static str,
    /// Full label, shown in the hover tooltip / expanded list (NOT the strip).
    label: &'static str,
    /// Emoji glyph shown in the compact strip (double-width aware).
    emoji: &'static str,
    /// Display string for the key combo, e.g. "⌃⌥X".
    shortcut: &'static str,
    /// Palette role → distinct color per button (see plugin_span_style host-side:
    /// blue|red|green|mauve|yellow|teal|peach|accent…).
    color: &'static str,
    /// What the button does. Documented per-button and asserted non-empty in
    /// tests; not rendered (the tooltip shows label + shortcut only, to keep the
    /// panel a fixed width — see [`detail_spans`]).
    #[allow(dead_code)]
    description: &'static str,
    /// Manifest `[[actions]]` id the click invokes via `plugin.action.invoke`.
    action_id: &'static str,
    /// Plugin the action belongs to. Most buttons target THIS plugin
    /// ([`PLUGIN_ID`]); the archive button targets `builtin.archive-dash`.
    plugin: &'static str,
}

/// ORDER IS THE UI. The grid wraps at [`BUTTONS_PER_ROW`], so this array is
/// read left-to-right, top-to-bottom, and it is grouped on purpose:
///
///   rows 1-2 — act on the CONVERSATION in the focused pane (fork, resume,
///              relocate, archive, background agent, remote). Non-destructive:
///              the worst case is an extra tab.
///   row 3   — act on HERDR ITSELF, ending with the two that kill processes.
///              They sit last so the pointer never crosses `Full restart` on
///              the way to `Fork`, and so the destructive pair reads as one
///              group rather than as three unrelated lifecycle buttons
///              scattered through the strip.
///
/// Per-agent support is stated in each `description` because it genuinely
/// differs — `Remote` and `Background agent` are Claude Code features, `fork`
/// covers three of the four runtimes, and jcode has no fork primitive at all.
/// The backing scripts refuse (and toast) rather than guess; see
/// scripts/dash-common.sh `agent_launch_cmd`.
static BUTTONS: &[ButtonDef] = &[
    ButtonDef {
        id: "fork",
        label: "Fork",
        emoji: "\u{1F374}",            // 🍴
        shortcut: "\u{2303}\u{2325}F", // ⌃⌥F
        color: "green",
        description: "Fork this conversation into a new TAB — claude, codex and pi; jcode has no fork flag and is refused",
        action_id: "fork",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "fork-workspace",
        label: "Fork \u{2192} workspace",
        emoji: "\u{1F531}",                    // 🔱
        shortcut: "\u{2303}\u{2325}\u{21E7}F", // ⌃⌥⇧F
        color: "mauve",
        description: "Fork this conversation into a new WORKSPACE — claude, codex and pi; jcode has no fork flag and is refused",
        action_id: "fork-workspace",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "resume-search",
        label: "Resume search",
        emoji: "\u{1F50E}", // 🔎
        shortcut: "\u{2303}\u{2325}S", // ⌃⌥S (display only; spec proposed ⌃⌥R but that already binds Remote)
        color: "muted",
        description: "Search every claude, codex, pi and fleet-harness transcript and resume the picked session in skip mode",
        action_id: "resume-search",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "relaunch-skip",
        label: "Relaunch skip",
        emoji: "\u{26A1}",             // ⚡
        shortcut: "\u{2303}\u{2325}K", // ⌃⌥K
        // `peach` was the last genuinely distinct role left in the host palette
        // (see canonical_color in the tests), so this button needs no entry in
        // ALLOWED_COLOR_COLLISIONS — and the next one added will.
        color: "peach",
        description: "Relaunch THIS session in place with permissions bypassed — claude and codex; pi and jcode have no such flag",
        action_id: "relaunch-skip",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "relocate",
        label: "Relocate",
        emoji: "\u{1F4E6}",            // 📦
        shortcut: "\u{2303}\u{2325}L", // ⌃⌥L
        // Deliberate, allow-listed collision with `fork` (see
        // ALLOWED_COLOR_COLLISIONS): `success` is an alias of `green`. Every
        // distinct colour was already taken, so the choice was WHICH pair
        // collides — fork and relocate are the two "send this conversation
        // somewhere else" buttons, so sharing a hue reads as a family rather
        // than as a mistake. Their glyphs (🍴 vs 📦) carry the distinction.
        color: "success",
        description: "Move this conversation to the project it is actually about and continue it there (claude only)",
        action_id: "relocate",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "archive",
        label: "Archive pane",
        emoji: "\u{1F5C4}\u{FE0F}",    // 🗄️
        shortcut: "\u{2303}\u{2325}A", // ⌃⌥A
        color: "teal",
        description: "Close the current pane and save it to the archive widget, restorable for claude, codex and pi",
        action_id: "archive",
        plugin: "builtin.archive-dash",
    },
    ButtonDef {
        id: "bg-dispatch",
        label: "Background agent",
        emoji: "\u{1F680}",            // 🚀
        shortcut: "\u{2303}\u{2325}B", // ⌃⌥B
        color: "magenta",
        description: "Open a pane to type a prompt, then dispatch it as a detached background agent (claude --bg; claude only)",
        action_id: "bg-dispatch",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "remote",
        label: "Remote",
        emoji: "\u{1F4F1}",            // 📱
        shortcut: "\u{2303}\u{2325}R", // ⌃⌥R
        color: "subtext",
        description: "Claude only: enable Remote Control (/remote-control) to continue this session from the phone or claude.ai",
        action_id: "remote",
        plugin: PLUGIN_ID,
    },
    // ── herdr lifecycle; the two that kill processes come last ──────────────
    ButtonDef {
        id: "handoff-hr",
        label: "Handoff hr",
        emoji: "\u{1F91D}",            // 🤝
        shortcut: "\u{2303}\u{2325}H", // ⌃⌥H
        // `warning` resolves to palette.yellow. It used to collide with
        // draw-screenshot's `yellow`; that button is gone, so this is now an
        // unshared swatch. Kept as `warning` rather than promoted to a "free"
        // role because red would group it with full-restart, which reads as
        // "destructive" — the opposite of what it is. It leads the lifecycle
        // group for the same reason: safest first.
        color: "warning",
        description:
            "Swap in the new binary WITHOUT killing panes — agents and subagents keep running",
        action_id: "handoff-hr",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "restart-hr",
        label: "Restart hr",
        emoji: "\u{1F504}",            // 🔄
        shortcut: "\u{2303}\u{2325}X", // ⌃⌥X
        color: "blue",
        description: "Fully kill & relaunch this herdr instance, restoring the session",
        action_id: "restart-hr",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "full-restart",
        label: "Full restart",
        emoji: "\u{267B}\u{FE0F}",             // ♻️
        shortcut: "\u{2303}\u{2325}\u{21E7}X", // ⌃⌥⇧X
        color: "red",
        description:
            "Hard kill tmux + agents + server (reclaim all RAM), relaunch fresh & resume",
        action_id: "full-restart",
        plugin: PLUGIN_ID,
    },
];

/// Manifest action opened by the strip's `[?]` cell: the full reference list,
/// as a floating OVERLAY pane.
///
/// It used to be a guest-local expand/collapse that drew one row per button
/// BELOW the grid, inside the bottom band. The band has ~4 slack rows and the
/// registry has 11 buttons, so 7 entries were clipped off-screen with no scroll
/// affordance (defect D4). Per the UX canon a list that does not fit the band is
/// a secondary surface, and a secondary surface is an overlay — which also gets
/// the Esc-to-close contract for free. The hover tooltip is unchanged: it is one
/// row and it always fits.
const ACTION_REFERENCE_LIST: &str = "reference-list";

// ── Guest-persistent state ────────────────────────────────────────────────────

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// Index into [`BUTTONS`] the cursor is currently hovering (for the tooltip),
/// or `None`. [`HOVER_TOGGLE`] is the sentinel for "hovering the list toggle".
static HOVERED: Mutex<Option<usize>> = Mutex::new(None);
/// Absolute (x, row) anchor of the compact button GRID — the panel's TOP-LEFT
/// cell (`rect.x`, `rect.y`; the host renders panel nodes top-down). The grid
/// rows render FIRST so they stay pinned there, and the hover tooltip /
/// expanded list draws BELOW them: the grid never moves and the tooltip never
/// covers a button. Raw mouse coordinates are absolute, so button rects are the
/// grid-relative geometry offset by this anchor.
static STRIP_ORIGIN: Mutex<Option<(u16, u16)>> = Mutex::new(None);

const HOVER_TOGGLE: usize = usize::MAX;

// ── v3 wire types (local; see module doc comment) ────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum V3Event {
    Frame(UiFrameContext),
    Mouse(MouseInput),
    #[allow(dead_code)]
    Key(Value),
    #[allow(dead_code)]
    FocusChanged(Value),
    Tick,
    ThemeChanged,
    ReloadRequested,
    #[allow(dead_code)]
    Action(Value),
    #[allow(dead_code)]
    ApiResult(Value),
}

#[derive(Debug, Deserialize)]
struct MouseInput {
    kind: String,
    column: u16,
    row: u16,
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

// ── Display-width (double-width emoji aware) ──────────────────────────────────
// The compact strip mixes ASCII, the modifier glyphs (⌃⌥⇧, width 1) and emoji
// (width 2). Terminal cell math must match the host renderer's advance so the
// hit-rects line up under the emoji. This covers the controlled glyph set we
// render; it is not a general unicode-width implementation.
fn cell_width(s: &str) -> u16 {
    let mut w: u16 = 0;
    for ch in s.chars() {
        let c = ch as u32;
        w += match c {
            0xFE0F => 0,            // variation selector-16 (renders on preceding glyph)
            0x1F000..=0x1FAFF => 2, // emoji & pictographs
            0x2600..=0x27BF => 2,   // misc symbols / dingbats (♻ etc.)
            0x2B00..=0x2BFF => 2,   // misc symbols & arrows
            _ => 1,
        };
    }
    w
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
        V3Event::Mouse(mouse) => handle_mouse(mouse),
        V3Event::Key(_)
        | V3Event::FocusChanged(_)
        | V3Event::Tick
        | V3Event::ThemeChanged
        | V3Event::ReloadRequested
        | V3Event::Action(_)
        | V3Event::ApiResult(_) => render_update(Vec::new()),
    }
}

fn handle_frame(frame: &UiFrameContext) -> UiUpdateV3 {
    *STRIP_ORIGIN.lock().expect("strip origin mutex poisoned") = panel_origin(frame);
    render_update(Vec::new())
}

/// Resolve clicks and hovers ourselves from the raw mouse stream (see module
/// doc): `Move` updates the hovered button (tooltip); `LeftDown` on a button
/// invokes it; the toggle flips the expanded list.
fn handle_mouse(mouse: &MouseInput) -> UiUpdateV3 {
    let hit = hit_at(mouse.column, mouse.row);
    match mouse.kind.as_str() {
        "move" => {
            // Scope the lock so it is released BEFORE render_update, which locks
            // HOVERED again — std Mutex is not reentrant (would deadlock).
            {
                let mut guard = HOVERED.lock().expect("hover mutex poisoned");
                *guard = hit;
            }
            render_update(Vec::new())
        }
        "left_down" => match hit {
            Some(HOVER_TOGGLE) => {
                // The reference list is an overlay pane now, not an in-panel
                // expansion — see ACTION_REFERENCE_LIST.
                render_update(vec![invoke_effect(PLUGIN_ID, ACTION_REFERENCE_LIST)])
            }
            Some(index) => {
                let button = &BUTTONS[index];
                render_update(vec![invoke_effect(button.plugin, button.action_id)])
            }
            None => render_update(Vec::new()),
        },
        _ => render_update(Vec::new()),
    }
}

fn invoke_effect(plugin_id: &str, action_id: &str) -> HostEffectV3 {
    HostEffectV3::Api {
        request_id: format!("button-dash:invoke:{action_id}"),
        method: "plugin.action.invoke".to_string(),
        params: json!({
            "plugin_id": plugin_id,
            "action_id": action_id,
        }),
    }
}

/// Absolute (x, top-row) anchor of the button strip from this module's `Panel`
/// rect (see manifest `[ui_modules.placement]`). The host renders panel nodes
/// TOP-DOWN from `rect.y` (empirically: a lone strip node in a 7-row band rect
/// draws at `rect.y`, not `rect.y + height - 1`). So the strip — rendered as the
/// FIRST node in [`button_dash_layer`] — is pinned to `rect.y`, and detail rows
/// stack BELOW it in the band's slack rows. A zero-sized rect (no slot carved
/// yet) yields `None`, disabling hit-testing until the first real frame.
fn panel_origin(frame: &UiFrameContext) -> Option<(u16, u16)> {
    origin_from_rect(frame.rect.x, frame.rect.y, frame.rect.width, frame.rect.height)
}

/// Pure core of [`panel_origin`] (testable without a full `UiFrameContext`): the
/// strip's absolute `(x, top-row)`, or `None` for an un-carved (zero-sized) rect.
fn origin_from_rect(x: u16, y: u16, width: u16, height: u16) -> Option<(u16, u16)> {
    if width == 0 || height == 0 {
        return None;
    }
    Some((x, y))
}

// ── Strip layout (single source of truth for render + hit-testing) ────────────

/// Buttons per grid row: the strip wraps into a compact multi-row BLOCK instead
/// of one long line, so the panel stays narrow and the usage dash keeps its
/// width. 10 buttons + the toggle = 3 rows (4 / 4 / 2 + toggle).
///
/// The manifest's `[ui_modules.placement].size` must match the widest row this
/// produces — see [`widest_grid_row`], which the test suite pins to that value
/// so adding a button can't silently start clipping the strip.
const BUTTONS_PER_ROW: usize = 4;

/// One clickable segment on the compact grid: its action target and its
/// strip-relative cell extent `[x, x+width)` on grid row `row`.
struct Segment {
    /// `Some(index)` into BUTTONS, or [`HOVER_TOGGLE`] for the list toggle.
    target: usize,
    row: u16,
    x: u16,
    width: u16,
}

/// Text of one compact button: `[<emoji> <shortcut>]`.
fn compact_label(button: &ButtonDef) -> String {
    format!("[{} {}]", button.emoji, button.shortcut)
}

/// The reference cell. A question mark, not a disclosure triangle: it opens an
/// OVERLAY rather than expanding this panel, and the canon reserves the
/// open/close glyph pair for surfaces that really do collapse back in place.
fn toggle_label() -> &'static str {
    "[?]"
}

/// Lay out the compact GRID once (rows of [`BUTTONS_PER_ROW`] cells, the toggle
/// as the final cell); return the drawn rows and the grid-relative segments so
/// render and hit-testing never disagree.
fn strip_grid() -> (Vec<Vec<UiTextSpan>>, Vec<Segment>) {
    let mut rows: Vec<Vec<UiTextSpan>> = Vec::new();
    let mut segments = Vec::new();
    let mut spans: Vec<UiTextSpan> = Vec::new();
    let mut col = 0u16;
    let mut row = 0u16;

    let mut cell = |spans: &mut Vec<UiTextSpan>,
                    col: &mut u16,
                    row: &mut u16,
                    rows: &mut Vec<Vec<UiTextSpan>>,
                    count: usize| {
        if count > 0 && count % BUTTONS_PER_ROW == 0 {
            rows.push(std::mem::take(spans));
            *col = 0;
            *row += 1;
        }
    };

    let mut count = 0usize;
    push_gap(&mut spans, &mut col, " ");
    for (index, button) in BUTTONS.iter().enumerate() {
        cell(&mut spans, &mut col, &mut row, &mut rows, count);
        if col == 0 {
            push_gap(&mut spans, &mut col, " ");
        }
        let label = compact_label(button);
        let width = cell_width(&label);
        segments.push(Segment {
            target: index,
            row,
            x: col,
            width,
        });
        spans.push(span(&label, button.color));
        col += width;
        push_gap(&mut spans, &mut col, "  ");
        count += 1;
    }
    // List toggle (▶/▼) as the last grid cell.
    cell(&mut spans, &mut col, &mut row, &mut rows, count);
    if col == 0 {
        push_gap(&mut spans, &mut col, " ");
    }
    let toggle = toggle_label();
    let twidth = cell_width(toggle);
    segments.push(Segment {
        target: HOVER_TOGGLE,
        row,
        x: col,
        width: twidth,
    });
    spans.push(span(toggle, "teal"));
    rows.push(spans);

    (rows, segments)
}

/// Number of grid rows the compact block occupies (detail rows render below).
// Test-only helper: the render path derives the same count from `strip_grid()`
// directly, so the wasm build legitimately never calls it.
#[allow(dead_code)]
fn grid_rows() -> u16 {
    let (rows, _) = strip_grid();
    rows.len() as u16
}

/// Cell width of the widest rendered grid row — i.e. exactly how many columns
/// the panel slot needs to show the strip without clipping.
///
/// The manifest pins a FIXED `[ui_modules.placement].size` rather than `auto`,
/// because `auto` regrows the panel to the widest row every frame and that
/// moved the buttons under the cursor whenever a hover tooltip appeared. A
/// fixed size means this number and the manifest's have to agree by hand — the
/// slot was left at 45 after four buttons were deleted, quietly reserving dead
/// columns that the usage dash could have used. `manifest_panel_size_matches_grid`
/// is what stops that from happening again.
// Test-only helper: the guest never needs its own width at runtime (the host
// hands it a rect), so only `manifest_panel_size_matches_grid` calls this.
#[allow(dead_code)]
fn widest_grid_row() -> u16 {
    let (rows, _) = strip_grid();
    rows.iter()
        .map(|spans| spans.iter().map(|s| cell_width(&s.text)).sum::<u16>())
        .max()
        .unwrap_or(0)
}

/// Resolve which target (button index / toggle) covers an absolute cell, or
/// `None`. Any grid row is clickable/hoverable.
fn hit_at(abs_col: u16, abs_row: u16) -> Option<usize> {
    let (ox, oy) = (*STRIP_ORIGIN.lock().expect("strip origin mutex poisoned"))?;
    let rel_row = abs_row.checked_sub(oy)?;
    let rel = abs_col.checked_sub(ox)?;
    let (_, segments) = strip_grid();
    segments
        .iter()
        .find(|seg| {
            seg.row == rel_row && rel >= seg.x && rel < seg.x.saturating_add(seg.width)
        })
        .map(|seg| seg.target)
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render_update(effects: Vec<HostEffectV3>) -> UiUpdateV3 {
    UiUpdateV3 {
        scope: "partial",
        layers: vec![button_dash_layer()],
        effects,
    }
}

/// The compact strip is always the FIRST node so, in the top-aligned band, it
/// stays pinned to the panel's top row (`rect.y`, see [`panel_origin`]) and
/// never moves when a tooltip appears. Detail rows (hover tooltip or expanded
/// list) render BELOW it, into the band's slack rows.
fn button_dash_layer() -> UiLayer {
    let mut nodes: Vec<UiNode> = Vec::new();

    // Grid rows FIRST → pinned to the panel's top rows (fixed; see STRIP_ORIGIN).
    let (rows, _segments) = strip_grid();
    for spans in rows {
        nodes.push(UiNode::Line { spans });
    }

    let hovered = *HOVERED.lock().expect("hover mutex poisoned");

    if let Some(index) = hovered {
        // Hover tooltip below the strip: the hovered button's full label.
        if index != HOVER_TOGGLE {
            if let Some(button) = BUTTONS.get(index) {
                nodes.push(UiNode::Line {
                    spans: detail_spans(button),
                });
            }
        }
    }

    // No declarative hit-rects: clicks AND hovers are resolved from the raw
    // `mouse_all` stream in `handle_mouse` (declaring hits would make the host
    // suppress hover `Move` over our own cells — the F5 dedupe).
    UiLayer {
        surface: UiSurface::Panel,
        z_index: 50,
        consumes_mouse: false,
        nodes,
        hits: Vec::<UiHitRect>::new(),
        data: None,
    }
}

/// A tooltip / expanded-list detail row: `  <emoji> <label>  <shortcut>`. Kept
/// narrower than the compact strip so the fixed-width panel (manifest
/// `[ui_modules.placement].size`) never resizes when a tooltip appears — that
/// resize is what used to shift the buttons horizontally on hover. The longer
/// `description` is intentionally not rendered here for that reason.
fn detail_spans(button: &ButtonDef) -> Vec<UiTextSpan> {
    vec![
        plain("  "),
        span(&format!("{} ", button.emoji), button.color),
        span(button.label, button.color),
        plain("  "),
        span(button.shortcut, "muted"),
    ]
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

fn push_gap(spans: &mut Vec<UiTextSpan>, col: &mut u16, gap: &str) {
    spans.push(plain(gap));
    *col += cell_width(gap);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate process-global state (STRIP_ORIGIN / HOVERED).
    // Serialize them so parallel `cargo test` can't race (and so
    // a panic mid-test can't poison a shared mutex for the others).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        *STRIP_ORIGIN.lock().unwrap() = None;
        *HOVERED.lock().unwrap() = None;
    }

    fn mouse(kind: &str, column: u16, row: u16) -> MouseInput {
        MouseInput {
            kind: kind.to_string(),
            column,
            row,
        }
    }

    #[test]
    fn every_registry_button_has_nonempty_fields() {
        for button in BUTTONS {
            assert!(!button.label.is_empty());
            assert!(!button.emoji.is_empty());
            assert!(!button.shortcut.is_empty());
            assert!(!button.color.is_empty());
            assert!(!button.description.is_empty());
            assert!(!button.action_id.is_empty());
            assert_eq!(button.id, button.action_id);
        }
    }

    /// Resolve a palette ROLE to the canonical palette field it actually paints,
    /// mirroring `plugin_span_style` in the host (`herdr/src/ui.rs:993-1012`).
    ///
    /// Several roles are ALIASES for the same colour. Comparing the role strings
    /// therefore proves nothing: two buttons can carry different role names and
    /// render byte-identical. Verified against a real frame with
    /// `tmux capture-pane -pe` — `magenta` and `mauve` both emit
    /// `38;2;203;166;247`.
    ///
    /// Note this resolves the FOREGROUND only. `accent` is `blue` plus BOLD; the
    /// fg is identical, so it is treated as a collision here and the weight is
    /// recorded as a mitigation, not as a distinct colour. That is the honest
    /// reading of §1.4 of DESIGN.md: weight is a weaker channel than hue and is
    /// lost entirely to a bold-averse theme, so it cannot carry distinctness on
    /// its own.
    fn canonical_color(role: &str) -> &'static str {
        match role {
            "accent" | "blue" => "palette.blue",
            "muted" => "palette.overlay0",
            "success" | "green" => "palette.green",
            "warning" | "yellow" => "palette.yellow",
            "danger" | "red" => "palette.red",
            "teal" | "cyan" => "palette.teal",
            "mauve" | "magenta" => "palette.mauve",
            "subtext" | "gray" => "palette.subtext0",
            "peach" => "palette.peach",
            "panel" => "palette.panel_bg",
            _ => "palette.text",
        }
    }

    /// Collisions that are ACCEPTED, each as a sorted role pair.
    ///
    /// The host exposes 11 distinct foreground colours and the registry now has
    /// more buttons than that, so perfect distinctness is arithmetically
    /// impossible — the honest end state is "documented collisions, no accidental
    /// ones". Each pair below is deliberate, and every pair is separated by some
    /// OTHER channel (all buttons carry unrelated glyphs, per §1.4):
    ///
    /// - `("magenta", "mauve")` — pure aliases; `bg-dispatch` was specced as
    ///   `magenta` on the belief that it was a free colour. It is not, and no
    ///   free colour remains. Giving it a genuinely distinct hue needs a new
    ///   palette arm in the host, which is a host change and out of scope here.
    ///
    /// Note every entry is a pair of DIFFERENT role names. Two buttons sharing
    /// one role (`"green"` twice) is never allowed and still fails.
    /// - `("green", "success")` — pure aliases. Chosen deliberately for
    ///   `relocate` vs `fork`: past the 11-colour ceiling the question is not
    ///   *whether* to collide but *which* pair should, and these are the two
    ///   "send this conversation elsewhere" buttons.
    ///
    /// Trimmed when the cx/CCR and draw-overlay buttons were removed: dropping
    /// `resume-in-cx` (accent), `refresh-cx-token` (cyan) and `draw-screenshot`
    /// (yellow) left `("accent","blue")`, `("cyan","teal")` and
    /// `("warning","yellow")` with no colliding pair to excuse. They are deleted
    /// rather than kept "just in case": a stale allow-list entry silently
    /// pre-approves a future collision nobody argued for, which is exactly the
    /// accidental-collision case this test exists to catch. `handoff-hr` keeps
    /// `warning` and is now unpaired.
    const ALLOWED_COLOR_COLLISIONS: &[(&str, &str)] =
        &[("green", "success"), ("magenta", "mauve")];

    #[test]
    fn button_colors_are_distinct() {
        for (i, a) in BUTTONS.iter().enumerate() {
            for b in BUTTONS.iter().skip(i + 1) {
                if canonical_color(a.color) != canonical_color(b.color) {
                    continue;
                }
                let mut pair = [a.color, b.color];
                pair.sort_unstable();
                assert!(
                    ALLOWED_COLOR_COLLISIONS.contains(&(pair[0], pair[1])),
                    "buttons '{}' ({}) and '{}' ({}) both render as {} — \
                     a colour is only distinct if it RESOLVES distinctly. \
                     Pick a role with a different canonical colour, or add the \
                     pair to ALLOWED_COLOR_COLLISIONS with a reason.",
                    a.id,
                    a.color,
                    b.id,
                    b.color,
                    canonical_color(a.color),
                );
            }
        }
    }

    #[test]
    fn cell_width_counts_emoji_as_double() {
        assert_eq!(cell_width("X"), 1);
        assert_eq!(cell_width("\u{2303}\u{2325}X"), 3); // ⌃⌥X modifiers are width 1
        assert_eq!(cell_width("\u{1F504}"), 2); // 🔄
        assert_eq!(cell_width("\u{267B}\u{FE0F}"), 2); // ♻️ (symbol 2 + VS16 0)
        assert_eq!(cell_width("\u{25B6}"), 1); // ▶ toggle glyph
    }

    #[test]
    fn segments_are_contiguous_and_cover_every_target() {
        let _g = guard();
        reset();
        let (_, segments) = strip_grid();
        // one segment per button + the toggle
        assert_eq!(segments.len(), BUTTONS.len() + 1);
        assert!(segments.iter().any(|s| s.target == HOVER_TOGGLE));
        for (i, _) in BUTTONS.iter().enumerate() {
            assert!(segments.iter().any(|s| s.target == i));
        }
    }

    #[test]
    fn hit_at_resolves_button_under_origin_row() {
        let _g = guard();
        reset();
        *STRIP_ORIGIN.lock().unwrap() = Some((10, 5));
        let (_, segments) = strip_grid();
        // middle of the first button's rect, offset by origin
        let first = &segments[0];
        let col = 10 + first.x + first.width / 2;
        assert_eq!(hit_at(col, 5 + first.row), Some(0));
        // a row past the whole grid → no hit
        assert_eq!(hit_at(col, 5 + grid_rows()), None);
        *STRIP_ORIGIN.lock().unwrap() = None;
        assert_eq!(hit_at(col, 5), None);
    }

    #[test]
    fn left_down_on_button_emits_plugin_action_invoke() {
        let _g = guard();
        reset();
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        let (_, segments) = strip_grid();
        let seg = &segments[0];
        let col = seg.x + seg.width / 2;
        let update = handle_mouse(&mouse("left_down", col, seg.row));
        match update.effects.first().expect("one effect") {
            HostEffectV3::Api { method, params, .. } => {
                assert_eq!(method, "plugin.action.invoke");
                assert_eq!(params["plugin_id"], json!(PLUGIN_ID));
                assert_eq!(params["action_id"], json!(BUTTONS[0].action_id));
            }
            other => panic!("expected Api effect, got {other:?}"),
        }
        *STRIP_ORIGIN.lock().unwrap() = None;
    }

    #[test]
    fn panel_origin_is_top_row_not_bottom() {
        // Regression: the strip renders TOP-aligned at rect.y. A 7-row band rect
        // at y=39 must anchor the strip to row 39, NOT 39+7-1=45. The earlier
        // bottom-anchored math made hit_at's row check reject every click
        // (observed live: click row=39 vs stored origin row=45 → hit=None).
        assert_eq!(origin_from_rect(128, 39, 48, 7), Some((128, 39)));
        // un-carved rect → no hit-testing yet
        assert_eq!(origin_from_rect(128, 39, 0, 7), None);
        assert_eq!(origin_from_rect(128, 39, 48, 0), None);
    }

    #[test]
    fn click_at_observed_coords_resolves_fork() {
        // Regression for the live-observed miss: with origin=(128,39) [after the
        // top-row fix], a click on the top row must resolve to the button under
        // it instead of missing entirely (the bug was hit=None for every click).
        //
        // The column is DERIVED from fork's own segment rather than hardcoded.
        // It used to be the observed col=152, but that pins the assertion to the
        // button ORDER, not to the hit-testing math this test exists to guard —
        // inserting any button ahead of fork slid a different button under 152
        // and failed the test for a reason it does not name.
        let _g = guard();
        reset();
        *STRIP_ORIGIN.lock().unwrap() = Some((128, 39));
        let fork_idx = BUTTONS
            .iter()
            .position(|b| b.action_id == "fork")
            .expect("fork button exists");
        let (_, segments) = strip_grid();
        let seg = segments
            .iter()
            .find(|s| s.target == fork_idx)
            .expect("fork segment");
        let update = handle_mouse(&mouse("left_down", 128 + seg.x + seg.width / 2, 39 + seg.row));
        match update.effects.first().expect("fork invoke effect") {
            HostEffectV3::Api { params, .. } => {
                assert_eq!(params["action_id"], json!("fork"));
            }
            other => panic!("expected Api effect, got {other:?}"),
        }
        reset();
    }

    #[test]
    fn strip_is_first_node_so_it_never_moves() {
        // The strip must be node 0 in every state (plain, hover, expanded) so the
        // top-aligned band pins it to rect.y regardless of detail rows below.
        let _g = guard();
        reset();
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        // plain: exactly the grid rows
        let base = grid_rows() as usize;
        assert_eq!(button_dash_layer().nodes.len(), base);
        // hover: grid rows first, tooltip after
        let (_, segments) = strip_grid();
        let seg = &segments[0];
        handle_mouse(&mouse("move", seg.x + seg.width / 2, seg.row));
        let nodes = button_dash_layer().nodes;
        assert_eq!(nodes.len(), base + 1);
        // node 0 is the strip (contains the toggle glyph); node 1 is the tooltip
        // (starts with leading padding, no toggle)
        reset();
    }

    /// The panel is a FIXED width, so the manifest carries a number that has to
    /// equal what the guest actually draws. Nothing else enforces that: a wrong
    /// value is not a compile error, not a runtime error, and not visible
    /// unless you count columns — too small silently clips the strip, too large
    /// silently steals width from the usage dash it shares the band with.
    ///
    /// This reads the real manifest so the two cannot drift: add a button and
    /// this test tells you the new number instead of the next person finding it
    /// with a ruler.
    #[test]
    fn manifest_panel_size_matches_grid() {
        let _g = guard();
        reset();
        let manifest = include_str!("../herdr-plugin.toml");
        // Scan for the table HEADER line rather than splitting on the name:
        // the string `[ui_modules.placement]` also appears inside a comment
        // further up, and splitting there reads a chunk with no keys in it.
        let raw = manifest
            .lines()
            .map(str::trim)
            .skip_while(|line| *line != "[ui_modules.placement]")
            .skip(1)
            .take_while(|line| !line.starts_with('['))
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| line.strip_prefix("size"))
            .and_then(|rest| rest.trim().strip_prefix('='))
            .map(|rest| rest.trim().trim_matches('"').to_string())
            .expect("[ui_modules.placement] declares a size");
        let declared: u16 = raw
            .parse()
            .unwrap_or_else(|_| panic!("size = \"{raw}\" is not a fixed cell count"));
        assert_eq!(
            declared,
            widest_grid_row(),
            "herdr-plugin.toml pins the panel at {declared} cells but the button grid \
             renders {} — set size = \"{}\"",
            widest_grid_row(),
            widest_grid_row(),
        );
    }

    /// The lifecycle buttons are the ones that kill processes, so they live at
    /// the END of the strip: the pointer must never cross `Full restart` to
    /// reach a conversation button. Pins the grouping, not the exact order
    /// within it.
    #[test]
    fn destructive_lifecycle_buttons_are_last() {
        let lifecycle = ["handoff-hr", "restart-hr", "full-restart"];
        let first_lifecycle = BUTTONS
            .iter()
            .position(|b| lifecycle.contains(&b.id))
            .expect("lifecycle buttons exist");
        assert!(
            BUTTONS[first_lifecycle..]
                .iter()
                .all(|b| lifecycle.contains(&b.id)),
            "a conversation button sits after a herdr-lifecycle button; \
             the restart group must stay at the end of the strip"
        );
        assert_eq!(
            BUTTONS.last().map(|b| b.id),
            Some("full-restart"),
            "the most destructive button must be last"
        );
    }

    #[test]
    fn fork_buttons_are_registered() {
        assert!(BUTTONS.iter().any(|b| b.action_id == "fork"));
        assert!(BUTTONS.iter().any(|b| b.action_id == "fork-workspace"));
    }

    /// The relaunch button is the only one that CLOSES the pane it was clicked
    /// from, so a registry entry pointing at a missing action would strand the
    /// user's session. Pinned like the fork pair.
    #[test]
    fn relaunch_skip_button_is_registered() {
        assert!(BUTTONS.iter().any(|b| b.action_id == "relaunch-skip"));
    }

    #[test]
    fn remote_button_is_registered() {
        assert!(BUTTONS.iter().any(|b| b.action_id == "remote"));
    }

    /// The cx/CCR buttons and the macOS-only draw-overlay buttons are gone for
    /// good. Asserting their ABSENCE is what stops a future "restore the old
    /// registry" edit from silently reintroducing a button whose backing script
    /// no longer exists — every id here would dispatch to a deleted action.
    #[test]
    fn removed_buttons_stay_removed() {
        for gone in [
            "resume-in-cx",
            "refresh-cx-token",
            "draw-screenshot",
            "draw-screenshot-region",
        ] {
            assert!(
                !BUTTONS.iter().any(|b| b.action_id == gone),
                "button '{gone}' was removed (cx/CCR or macOS-only) and must not come back"
            );
        }
    }

    #[test]
    fn move_sets_hover_and_renders_tooltip_row() {
        let _g = guard();
        reset();
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        let (_, segments) = strip_grid();
        let seg = &segments[1]; // second button
        let col = seg.x + seg.width / 2;
        let update = handle_mouse(&mouse("move", col, seg.row));
        // grid rows + one tooltip row for the hovered button
        let layer = &update.layers[0];
        let base = grid_rows() as usize;
        assert_eq!(layer.nodes.len(), base + 1);
        assert_eq!(*HOVERED.lock().unwrap(), Some(1));
        // moving off the grid clears the tooltip
        let update = handle_mouse(&mouse("move", 999, 99));
        assert_eq!(update.layers[0].nodes.len(), base);
        assert_eq!(*HOVERED.lock().unwrap(), None);
        *STRIP_ORIGIN.lock().unwrap() = None;
    }

    /// D4: the reference cell opens the OVERLAY action and adds no rows to the
    /// band. The old behaviour (expand in place) clipped 7 of 11 entries.
    #[test]
    fn reference_cell_opens_the_overlay_and_never_grows_the_panel() {
        let _g = guard();
        reset();
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        let (_, segments) = strip_grid();
        let toggle = segments.iter().find(|s| s.target == HOVER_TOGGLE).unwrap();
        let col = toggle.x + toggle.width / 2;
        let update = handle_mouse(&mouse("left_down", col, toggle.row));
        match update.effects.first().expect("one effect") {
            HostEffectV3::Api { method, params, .. } => {
                assert_eq!(method, "plugin.action.invoke");
                assert_eq!(params["plugin_id"], json!(PLUGIN_ID));
                assert_eq!(params["action_id"], json!(ACTION_REFERENCE_LIST));
            }
            other => panic!("expected Api effect, got {other:?}"),
        }
        // the grid, and nothing else: no rows were added to the bottom band
        assert_eq!(update.layers[0].nodes.len(), grid_rows() as usize);
        *STRIP_ORIGIN.lock().unwrap() = None;
    }

    /// The overlay renders from `reference.json`; the strip renders from
    /// [`BUTTONS`]. They are two copies of one list, so pin them together —
    /// otherwise the reference silently documents a strip that has moved on.
    #[test]
    fn reference_json_matches_buttons() {
        #[derive(serde::Deserialize)]
        struct Entry {
            id: String,
            label: String,
            emoji: String,
            shortcut: String,
            color: String,
            description: String,
        }
        #[derive(serde::Deserialize)]
        struct File {
            buttons: Vec<Entry>,
        }
        let raw = include_str!("../reference.json");
        let file: File = serde_json::from_str(raw).expect("reference.json parses");
        assert_eq!(
            file.buttons.len(),
            BUTTONS.len(),
            "reference.json has {} entries but the strip renders {} buttons — \
             regenerate it when the registry changes",
            file.buttons.len(),
            BUTTONS.len(),
        );
        for (entry, button) in file.buttons.iter().zip(BUTTONS) {
            assert_eq!(entry.id, button.id, "reference.json order must match BUTTONS");
            assert_eq!(entry.label, button.label, "label drift on '{}'", button.id);
            assert_eq!(entry.emoji, button.emoji, "emoji drift on '{}'", button.id);
            assert_eq!(
                entry.shortcut, button.shortcut,
                "shortcut drift on '{}'",
                button.id
            );
            assert_eq!(entry.color, button.color, "colour drift on '{}'", button.id);
            assert_eq!(
                entry.description, button.description,
                "description drift on '{}'",
                button.id
            );
        }
    }

    #[test]
    fn no_declarative_hit_rects_declared() {
        let _g = guard();
        reset();
        // Hover requires raw mouse over our cells, which the host only delivers
        // when we declare NO hit-rects (F5 dedupe). Guard that invariant.
        let layer = button_dash_layer();
        assert!(layer.hits.is_empty());
    }

    #[test]
    fn left_up_does_not_invoke() {
        let _g = guard();
        reset();
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        let (_, segments) = strip_grid();
        let seg = &segments[0];
        let col = seg.x + seg.width / 2;
        let update = handle_mouse(&mouse("left_up", col, seg.row));
        assert!(update.effects.is_empty());
        *STRIP_ORIGIN.lock().unwrap() = None;
    }
}
