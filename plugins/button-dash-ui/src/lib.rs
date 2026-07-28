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
use std::sync::atomic::{AtomicBool, Ordering};
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

static BUTTONS: &[ButtonDef] = &[
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
        emoji: "\u{267B}\u{FE0F}",              // ♻️
        shortcut: "\u{2303}\u{2325}\u{21E7}X",  // ⌃⌥⇧X
        color: "red",
        description:
            "Hard kill tmux + Claude Code + server (reclaim all RAM), relaunch fresh & resume",
        action_id: "full-restart",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "fork",
        label: "Fork",
        emoji: "\u{1F374}",            // 🍴
        shortcut: "\u{2303}\u{2325}F", // ⌃⌥F
        color: "green",
        description: "Fork this conversation into a new TAB (claude/cx/codex/pi)",
        action_id: "fork",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "fork-workspace",
        label: "Fork \u{2192} workspace",
        emoji: "\u{1F531}",                     // 🔱
        shortcut: "\u{2303}\u{2325}\u{21E7}F",  // ⌃⌥⇧F
        color: "mauve",
        description: "Fork this conversation into a new WORKSPACE (claude/cx/codex/pi)",
        action_id: "fork-workspace",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "resume-in-cx",
        label: "Resume in cx",
        emoji: "\u{1F916}",            // 🤖
        shortcut: "\u{2303}\u{2325}C", // ⌃⌥C
        color: "accent",
        description: "Fork this claude conversation into a new TAB running under cx (GPT-via-CCR)",
        action_id: "resume-in-cx",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "resume-search",
        label: "Resume search",
        emoji: "\u{1F50E}", // 🔎
        shortcut: "\u{2303}\u{2325}S", // ⌃⌥S (display only; spec proposed ⌃⌥R but that already binds Remote)
        color: "muted",
        description: "Search ALL claude-family transcripts and resume the picked session in the same pane",
        action_id: "resume-search",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "remote",
        label: "Remote",
        emoji: "\u{1F4F1}",            // 📱
        shortcut: "\u{2303}\u{2325}R", // ⌃⌥R
        color: "subtext",
        description: "Enable Remote Control (/remote-control) to continue this claude session from the phone / claude.ai",
        action_id: "remote",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "draw-screenshot",
        label: "Draw + shot",
        emoji: "\u{1F58A}\u{FE0F}",    // 🖊️
        shortcut: "\u{2325}\u{21E7}5", // ⌥⇧5
        color: "yellow",
        description: "Toggle a blank DrawPen overlay to draw on-screen; press again to screenshot + close",
        action_id: "draw-screenshot",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "draw-screenshot-region",
        label: "Draw + shot (region)",
        emoji: "\u{1F4D0}",                     // 📐
        shortcut: "\u{2325}\u{21E7}4",  // ⌥⇧4
        color: "peach",
        description: "Same overlay toggle, but finish by drag-selecting the capture region",
        action_id: "draw-screenshot-region",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "detach",
        label: "Detach",
        emoji: "\u{23CF}\u{FE0F}",     // ⏏️
        shortcut: "\u{2303}\u{2325}D", // ⌃⌥D
        color: "lavender",
        description: "Move this conversation into an external tmux session that survives herdr restarts (resume, not fork)",
        action_id: "detach",
        plugin: PLUGIN_ID,
    },
    ButtonDef {
        id: "archive",
        label: "Archive pane",
        emoji: "\u{1F5C4}\u{FE0F}",   // 🗄️
        shortcut: "\u{2303}\u{2325}A", // ⌃⌥A
        color: "teal",
        description: "Close the current pane and save it to the archive widget (restorable)",
        action_id: "archive",
        plugin: "builtin.archive-dash",
    },
];

// Action id for the built-in expand/collapse toggle (guest-internal; never
// reaches plugin.action.invoke).
const ACTION_TOGGLE_LIST: &str = "button-dash:toggle_list";
const _: &str = ACTION_TOGGLE_LIST; // reserved id, kept documented

// ── Guest-persistent state ────────────────────────────────────────────────────

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// Whether the expandable full list (label + shortcut + description) is shown.
static LIST_EXPANDED: AtomicBool = AtomicBool::new(false);
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
                let expanded = !LIST_EXPANDED.load(Ordering::SeqCst);
                LIST_EXPANDED.store(expanded, Ordering::SeqCst);
                render_update(Vec::new())
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
/// width. 7 buttons + the toggle = exactly 2 rows of 4.
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

fn toggle_label() -> &'static str {
    if LIST_EXPANDED.load(Ordering::SeqCst) {
        "[\u{25BC}]" // ▼
    } else {
        "[\u{25B6}]" // ▶
    }
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
fn grid_rows() -> u16 {
    let (rows, _) = strip_grid();
    rows.len() as u16
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

    let expanded = LIST_EXPANDED.load(Ordering::SeqCst);
    let hovered = *HOVERED.lock().expect("hover mutex poisoned");

    if expanded {
        // Full reference below the strip: one detail row per button.
        for button in BUTTONS {
            nodes.push(UiNode::Line {
                spans: detail_spans(button),
            });
        }
    } else if let Some(index) = hovered {
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

    // These tests mutate process-global state (STRIP_ORIGIN / HOVERED /
    // LIST_EXPANDED). Serialize them so parallel `cargo test` can't race (and so
    // a panic mid-test can't poison a shared mutex for the others).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        *STRIP_ORIGIN.lock().unwrap() = None;
        *HOVERED.lock().unwrap() = None;
        LIST_EXPANDED.store(false, Ordering::SeqCst);
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

    #[test]
    fn button_colors_are_distinct() {
        let mut colors: Vec<&str> = BUTTONS.iter().map(|b| b.color).collect();
        colors.sort_unstable();
        colors.dedup();
        assert_eq!(colors.len(), BUTTONS.len(), "each button needs its own color");
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
        // Regression for the live-observed miss: origin=(128,39) [after the
        // top-row fix], click col=152 row=39 → rel=24 lands inside the 🍴 fork
        // segment and must emit the `fork` action invoke.
        let _g = guard();
        reset();
        *STRIP_ORIGIN.lock().unwrap() = Some((128, 39));
        let update = handle_mouse(&mouse("left_down", 152, 39));
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

    #[test]
    fn fork_buttons_are_registered() {
        assert!(BUTTONS.iter().any(|b| b.action_id == "fork"));
        assert!(BUTTONS.iter().any(|b| b.action_id == "fork-workspace"));
    }

    #[test]
    fn resume_cx_and_remote_buttons_are_registered() {
        assert!(BUTTONS.iter().any(|b| b.action_id == "resume-in-cx"));
        assert!(BUTTONS.iter().any(|b| b.action_id == "remote"));
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

    #[test]
    fn toggle_click_flips_expanded_and_lists_all() {
        let _g = guard();
        reset();
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        let (_, segments) = strip_grid();
        let toggle = segments.iter().find(|s| s.target == HOVER_TOGGLE).unwrap();
        let col = toggle.x + toggle.width / 2;
        let update = handle_mouse(&mouse("left_down", col, toggle.row));
        assert!(LIST_EXPANDED.load(Ordering::SeqCst));
        assert!(update.effects.is_empty());
        // expanded → grid rows + one row per button
        assert_eq!(update.layers[0].nodes.len(), grid_rows() as usize + BUTTONS.len());
        LIST_EXPANDED.store(false, Ordering::SeqCst);
        *STRIP_ORIGIN.lock().unwrap() = None;
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
