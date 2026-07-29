//! Archive-dash guest: a compact chip widget of the most-recently ARCHIVED
//! sessions, rendered in a generic plugin `Panel` docked at the start of the
//! bottom band (position is pure manifest DATA, `[ui_modules.placement]`).
//!
//! Data flow: the archive scripts (`scripts/archive-pane.sh`,
//! `scripts/archive-op.sh`) own the store at `~/.herdr-archive/archive.json` and
//! write a last-10 (newest first) view into `<state_dir>/archive_input.json`.
//! The host reads that declared `ui_input_file` each frame and injects its bytes
//! as `HERDR_PLUGIN_UI_INPUT_JSON` (spec §7.1), which we parse here. The guest
//! has NO filesystem of its own, so all mutations (archive/restore/fork/delete)
//! run host-side through param-less `[[actions]]`.
//!
//! Passing "which entry": the widget renders each item as a chip in a fixed slot
//! order (slot 1 = newest, matching the input file), and interaction invokes one
//! of three actions with the slot as a PAYLOAD — `InvokePluginAction` carrying
//! `{"slot":N}`, which the host exports to the script as
//! `HERDR_PLUGIN_PAYLOAD_JSON`:
//!   left-click a chip   -> `restore`
//!   right-click a chip  -> open an inline `[fork] [del]` menu below that chip
//!     click `[fork]`     -> `fork`
//!     click `[del]`      -> `delete`
//! hovering a chip shows its résumé as a tooltip row below the strip.
//!
//! This used to be thirty actions (`<verb>-slot-1..10`) because host actions
//! were param-less. They no longer are — but note the fix is NOT the socket
//! method `plugin.action.invoke`, which still carries no payload; it is the
//! guest-side `HostEffect::InvokePluginAction` variant, gated on the
//! `plugin_actions` capability the manifest now declares.
//!
//! Mouse model (why NO declarative hit-rects): the host suppresses raw mouse —
//! including hover `Move` — over a module's own hit-rects (the F5 click-dedupe).
//! Hover + the sticky right-click menu REQUIRE the raw stream, so we request
//! `mouse_all`, declare NO hit-rects, and resolve clicks/hovers ourselves from
//! the absolute `Move`/`LeftDown`/`RightDown` coordinates against the chip rects
//! we compute from the panel origin.

use herdr_ui_plugin_api::{UiFrameContext, UiHitRect, UiLayer, UiNode, UiSurface, UiTextSpan};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Mutex;

/// This plugin's manifest id; sent with every `plugin.action.invoke`.
const PLUGIN_ID: &str = "builtin.archive-dash";

/// Max chips rendered (matches the last-10 the scripts write).
const MAX_CHIPS: usize = 10;
/// Chip label is truncated to this many chars to keep chips compact.
const LABEL_MAX_CHARS: usize = 10;
/// Résumé tooltip is truncated to this many chars (wrapped over up to
/// `RESUME_MAX_ROWS` rows of the panel width).
const RESUME_MAX_CHARS: usize = 360;
/// Max wrapped tooltip rows below the chip strip.
const RESUME_MAX_ROWS: usize = 4;

// ── Input model (parsed from HERDR_PLUGIN_UI_INPUT_JSON) ───────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
struct ArchiveInput {
    #[serde(default)]
    items: Vec<ArchiveItem>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct ArchiveItem {
    /// 1-based slot (newest = 1). Drives the `<verb>-slot-<slot>` action id.
    slot: u32,
    #[serde(default)]
    label: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    resume: String,
}

// ── Guest-persistent state ────────────────────────────────────────────────────

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// Items parsed from the most recent frame's input JSON. Kept so mouse events
/// (which carry no input) can still map a chip index → its slot action.
static ITEMS: Mutex<Vec<ArchiveItem>> = Mutex::new(Vec::new());
/// Chip index the cursor is hovering (for the résumé tooltip), or `None`.
static HOVERED: Mutex<Option<usize>> = Mutex::new(None);
/// Chip index whose inline `[fork] [del]` menu is open, or `None`. Sticky: only
/// a click (on a menu button, a chip, or empty space) changes it.
static MENU_OPEN: Mutex<Option<usize>> = Mutex::new(None);
/// Absolute (x, top-row) anchor of the chip strip — the panel's TOP row (the
/// host renders panel nodes top-down from `rect.y`). Detail rows (tooltip/menu)
/// stack BELOW it. `None` until the first carved (non-zero) frame.
static STRIP_ORIGIN: Mutex<Option<(u16, u16)>> = Mutex::new(None);
/// Carved panel width from the most recent frame; bounds tooltip wrapping.
static PANEL_WIDTH: Mutex<u16> = Mutex::new(0);

// ── v3 wire types (local; mirror button-dash) ────────────────────────────────

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
    /// Run one of THIS plugin's declared actions, with a payload. The host
    /// exports `payload_json` to the action's script as
    /// `HERDR_PLUGIN_PAYLOAD_JSON` (`app/api/plugins/runtime.rs:82-87`).
    ///
    /// This is the ONLY way a guest passes a parameter to its own script. The
    /// socket method `plugin.action.invoke` — the [`HostEffectV3::Api`] arm
    /// above — carries no payload at all, which is why this widget used to
    /// declare thirty `<verb>-slot-N` actions to encode one integer.
    ///
    /// Gated on the `plugin_actions` capability: without it the host silently
    /// discards the effect (`ui_plugins.rs:1309-1311`), so the manifest must
    /// declare it or every chip click becomes a no-op.
    InvokePluginAction {
        plugin_id: String,
        action_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        payload_json: Option<String>,
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

// ── Display-width (double-width emoji aware; mirrors button-dash) ──────────────
fn cell_width(s: &str) -> u16 {
    let mut w: u16 = 0;
    for ch in s.chars() {
        let c = ch as u32;
        w += match c {
            0xFE0F => 0,            // variation selector-16
            0x1F000..=0x1FAFF => 2, // emoji & pictographs
            0x2600..=0x27BF => 2,   // misc symbols / dingbats
            0x2B00..=0x2BFF => 2,   // misc symbols & arrows
            _ => 1,
        };
    }
    w
}

/// Truncate `s` to at most `max` chars (char-safe), appending "…" when clipped.
fn truncate_chars(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            out.push('\u{2026}'); // …
            break;
        }
        out.push(ch);
    }
    out
}

/// Agent glyph for the chip (double-width; keeps the strip aligned).
fn agent_emoji(agent: &str) -> &'static str {
    match agent {
        "claude" => "\u{1F7E3}", // 🟣
        "codex" => "\u{1F7E2}",  // 🟢
        "pi" | "omp" => "\u{1F535}", // 🔵
        _ => "\u{26AA}",         // ⚪
    }
}

fn agent_color(agent: &str) -> &'static str {
    match agent {
        "claude" => "mauve",
        "codex" => "green",
        "pi" | "omp" => "blue",
        _ => "muted",
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
    // §7.1 generic transport: parse the injected input file contents.
    let items = frame
        .host
        .env
        .get("HERDR_PLUGIN_UI_INPUT_JSON")
        .filter(|j| !j.is_empty())
        .and_then(|j| serde_json::from_str::<ArchiveInput>(j).ok())
        .map(|input| {
            let mut v = input.items;
            v.truncate(MAX_CHIPS);
            v
        })
        .unwrap_or_default();

    {
        let mut guard = ITEMS.lock().expect("items mutex poisoned");
        *guard = items;
    }
    *STRIP_ORIGIN.lock().expect("strip origin mutex poisoned") = panel_origin(frame);
    *PANEL_WIDTH.lock().expect("panel width mutex poisoned") = frame.rect.width;
    // Clamp any stale hover/menu index against the new item count.
    let count = ITEMS.lock().expect("items mutex poisoned").len();
    clamp_index(&HOVERED, count);
    clamp_index(&MENU_OPEN, count);
    render_update(Vec::new())
}

fn clamp_index(cell: &Mutex<Option<usize>>, count: usize) {
    let mut guard = cell.lock().expect("index mutex poisoned");
    if let Some(i) = *guard {
        if i >= count {
            *guard = None;
        }
    }
}

/// Resolve clicks and hovers from the raw mouse stream (see module doc).
fn handle_mouse(mouse: &MouseInput) -> UiUpdateV3 {
    match mouse.kind.as_str() {
        "move" => {
            let hit = chip_at(mouse.column, mouse.row);
            {
                let mut guard = HOVERED.lock().expect("hover mutex poisoned");
                *guard = hit;
            }
            render_update(Vec::new())
        }
        "left_down" => {
            // A menu button click wins over a chip click.
            if let Some((chip, which)) = menu_button_at(mouse.column, mouse.row) {
                let slot = slot_of(chip);
                *MENU_OPEN.lock().expect("menu mutex poisoned") = None;
                if let Some(slot) = slot {
                    let action = match which {
                        MenuButton::Fork => "fork",
                        MenuButton::Delete => "delete",
                    };
                    return render_update(vec![invoke_effect(action, slot)]);
                }
                return render_update(Vec::new());
            }
            // Otherwise: a chip left-click restores; anything else closes the menu.
            let menu_was_open = MENU_OPEN.lock().expect("menu mutex poisoned").is_some();
            *MENU_OPEN.lock().expect("menu mutex poisoned") = None;
            match chip_at(mouse.column, mouse.row) {
                Some(chip) if !menu_was_open => match slot_of(chip) {
                    Some(slot) => render_update(vec![invoke_effect("restore", slot)]),
                    None => render_update(Vec::new()),
                },
                // If a menu was open, the click just dismisses it (no restore).
                _ => render_update(Vec::new()),
            }
        }
        "right_down" => {
            let hit = chip_at(mouse.column, mouse.row);
            *MENU_OPEN.lock().expect("menu mutex poisoned") = hit;
            render_update(Vec::new())
        }
        _ => render_update(Vec::new()),
    }
}

fn slot_of(chip: usize) -> Option<u32> {
    ITEMS
        .lock()
        .expect("items mutex poisoned")
        .get(chip)
        .map(|i| i.slot)
}

/// Run `<action_id>` against the record at `slot` (1-based, newest first).
///
/// The slot travels as a payload rather than being baked into the action id.
/// `payload_json` is a JSON *string*, not an object — the host hands it to the
/// script verbatim as `HERDR_PLUGIN_PAYLOAD_JSON`.
fn invoke_effect(action_id: &str, slot: u32) -> HostEffectV3 {
    HostEffectV3::InvokePluginAction {
        plugin_id: PLUGIN_ID.to_string(),
        action_id: action_id.to_string(),
        payload_json: Some(json!({ "slot": slot }).to_string()),
    }
}

fn panel_origin(frame: &UiFrameContext) -> Option<(u16, u16)> {
    origin_from_rect(frame.rect.x, frame.rect.y, frame.rect.width, frame.rect.height)
}

fn origin_from_rect(x: u16, y: u16, width: u16, height: u16) -> Option<(u16, u16)> {
    if width == 0 || height == 0 {
        return None;
    }
    Some((x, y))
}

// ── Strip layout (single source of truth for render + hit-testing) ────────────

/// One clickable chip on the strip: its item index and strip-relative extent.
/// `row` is the strip-relative row the chip wrapped onto (0 = first row).
struct Segment {
    index: usize,
    row: u16,
    x: u16,
    width: u16,
}

fn chip_label(item: &ArchiveItem) -> String {
    format!(
        "[{} {}]",
        agent_emoji(&item.agent),
        truncate_chars(&item.label, LABEL_MAX_CHARS)
    )
}

/// Lay out the chip strip once, WRAPPED to the panel width (a chip that would
/// overflow the carved panel starts a new row instead of bleeding past it);
/// return the drawn rows and the strip-relative segments so render and
/// hit-testing never disagree.
fn strip_rows(items: &[ArchiveItem]) -> (Vec<Vec<UiTextSpan>>, Vec<Segment>) {
    let panel_width = (*PANEL_WIDTH.lock().expect("panel width mutex poisoned")).max(20);
    let mut rows: Vec<Vec<UiTextSpan>> = Vec::new();
    let mut segments = Vec::new();
    let mut spans: Vec<UiTextSpan> = Vec::new();
    let mut row = 0u16;
    let mut col = 0u16;

    push_gap(&mut spans, &mut col, " ");
    for (index, item) in items.iter().enumerate() {
        let label = chip_label(item);
        let width = cell_width(&label);
        if col > 1 && col.saturating_add(width) > panel_width {
            rows.push(std::mem::take(&mut spans));
            row += 1;
            col = 0;
            push_gap(&mut spans, &mut col, " ");
        }
        segments.push(Segment {
            index,
            row,
            x: col,
            width,
        });
        spans.push(span(&label, agent_color(&item.agent)));
        col += width;
        push_gap(&mut spans, &mut col, " ");
    }
    rows.push(spans);
    (rows, segments)
}

/// Number of rows the wrapped chip strip currently occupies.
fn strip_row_count() -> u16 {
    let items = ITEMS.lock().expect("items mutex poisoned");
    let (rows, _) = strip_rows(&items);
    rows.len() as u16
}

/// Resolve which chip covers an absolute cell on the (wrapped) strip, or `None`.
fn chip_at(abs_col: u16, abs_row: u16) -> Option<usize> {
    let (ox, oy) = (*STRIP_ORIGIN.lock().expect("strip origin mutex poisoned"))?;
    let rel_row = abs_row.checked_sub(oy)?;
    let rel = abs_col.checked_sub(ox)?;
    let items = ITEMS.lock().expect("items mutex poisoned");
    let (_, segments) = strip_rows(&items);
    segments
        .iter()
        .find(|seg| {
            seg.row == rel_row && rel >= seg.x && rel < seg.x.saturating_add(seg.width)
        })
        .map(|seg| seg.index)
}

// ── Inline right-click menu ([fork] [del]) ────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuButton {
    Fork,
    Delete,
}

const MENU_INDENT: u16 = 2;
const FORK_TEXT: &str = "[fork]";
const DEL_TEXT: &str = "[del]";
const MENU_GAP: &str = " ";

/// The menu row is rendered directly BELOW the (wrapped) strip, indented under
/// the chip's start `x`. Returns the two button rects' strip-relative x extents.
fn menu_segments() -> (u16, u16, u16, u16) {
    let fork_x = MENU_INDENT;
    let fork_w = cell_width(FORK_TEXT);
    let del_x = fork_x + fork_w + cell_width(MENU_GAP);
    let del_w = cell_width(DEL_TEXT);
    (fork_x, fork_w, del_x, del_w)
}

/// Resolve a click on the open menu's `[fork]`/`[del]` button, if any.
fn menu_button_at(abs_col: u16, abs_row: u16) -> Option<(usize, MenuButton)> {
    let chip = (*MENU_OPEN.lock().expect("menu mutex poisoned"))?;
    let (ox, oy) = (*STRIP_ORIGIN.lock().expect("strip origin mutex poisoned"))?;
    if abs_row != oy + strip_row_count() {
        return None;
    }
    let rel = abs_col.checked_sub(ox)?;
    let (fx, fw, dx, dw) = menu_segments();
    if rel >= fx && rel < fx + fw {
        Some((chip, MenuButton::Fork))
    } else if rel >= dx && rel < dx + dw {
        Some((chip, MenuButton::Delete))
    } else {
        None
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render_update(effects: Vec<HostEffectV3>) -> UiUpdateV3 {
    UiUpdateV3 {
        scope: "partial",
        layers: vec![archive_layer()],
        effects,
    }
}

/// The chip strip is always the FIRST node so, in the top-aligned band, it stays
/// pinned to `rect.y` and never moves. A detail row (menu OR tooltip) renders
/// BELOW it. When both would apply, the sticky right-click menu wins.
fn archive_layer() -> UiLayer {
    let items = ITEMS.lock().expect("items mutex poisoned").clone();
    let mut nodes: Vec<UiNode> = Vec::new();

    let (strip, _segments) = strip_rows(&items);
    for spans in strip {
        nodes.push(UiNode::Line { spans });
    }

    let menu = *MENU_OPEN.lock().expect("menu mutex poisoned");
    let hovered = *HOVERED.lock().expect("hover mutex poisoned");

    if let Some(chip) = menu {
        if items.get(chip).is_some() {
            nodes.push(UiNode::Line { spans: menu_spans() });
        }
    } else if let Some(chip) = hovered {
        if let Some(item) = items.get(chip) {
            let width = *PANEL_WIDTH.lock().expect("panel width mutex poisoned");
            for spans in tooltip_rows(item, width) {
                nodes.push(UiNode::Line { spans });
            }
        }
    }

    // No declarative hit-rects: clicks AND hovers come from the raw mouse stream.
    UiLayer {
        surface: UiSurface::Panel,
        z_index: 50,
        consumes_mouse: false,
        nodes,
        hits: Vec::<UiHitRect>::new(),
        data: None,
    }
}

fn menu_spans() -> Vec<UiTextSpan> {
    vec![
        plain("  "),
        span(FORK_TEXT, "green"),
        plain(MENU_GAP),
        span(DEL_TEXT, "red"),
    ]
}

/// Wrap the résumé over up to `RESUME_MAX_ROWS` rows of the panel width so a
/// long conversation summary (~300+ chars) stays fully readable on hover.
fn tooltip_rows(item: &ArchiveItem, panel_width: u16) -> Vec<Vec<UiTextSpan>> {
    let text = if item.resume.is_empty() {
        item.label.clone()
    } else {
        truncate_chars(&item.resume, RESUME_MAX_CHARS)
    };
    // 2-col indent; keep a sane minimum when the carve is very narrow.
    let row_chars = usize::from(panel_width.saturating_sub(2)).max(20);
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(row_chars)
        .take(RESUME_MAX_ROWS)
        .map(|chunk| {
            let line: String = chunk.iter().collect();
            vec![plain("  "), span(&line, "muted")]
        })
        .collect()
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

    // Serialize tests: they mutate process-global state (ITEMS / HOVERED /
    // MENU_OPEN / STRIP_ORIGIN).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        *STRIP_ORIGIN.lock().unwrap() = None;
        *HOVERED.lock().unwrap() = None;
        *MENU_OPEN.lock().unwrap() = None;
        *ITEMS.lock().unwrap() = Vec::new();
    }

    fn item(slot: u32, label: &str, agent: &str, resume: &str) -> ArchiveItem {
        ArchiveItem {
            slot,
            label: label.to_string(),
            agent: agent.to_string(),
            resume: resume.to_string(),
        }
    }

    fn set_items(items: Vec<ArchiveItem>) {
        *ITEMS.lock().unwrap() = items;
    }

    fn mouse(kind: &str, column: u16, row: u16) -> MouseInput {
        MouseInput {
            kind: kind.to_string(),
            column,
            row,
        }
    }

    #[test]
    fn cell_width_counts_emoji_as_double() {
        assert_eq!(cell_width("X"), 1);
        assert_eq!(cell_width("\u{1F7E3}"), 2); // 🟣 claude glyph
        assert_eq!(cell_width("\u{26AA}"), 2); // ⚪ default glyph
        assert_eq!(cell_width("[del]"), 5);
    }

    #[test]
    fn truncate_appends_ellipsis_only_when_clipped() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("0123456789abc", 10), "0123456789\u{2026}");
    }

    #[test]
    fn agent_glyphs_and_colors_are_distinct_per_known_agent() {
        assert_ne!(agent_emoji("claude"), agent_emoji("codex"));
        assert_ne!(agent_emoji("codex"), agent_emoji("pi"));
        assert_eq!(agent_emoji("unknown"), "\u{26AA}");
        assert_eq!(agent_color("claude"), "mauve");
    }

    #[test]
    fn parses_input_json_into_items() {
        let json = r#"{"items":[{"slot":1,"label":"herdr","agent":"claude","resume":"hi … bye"}]}"#;
        let parsed: ArchiveInput = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].slot, 1);
        assert_eq!(parsed.items[0].label, "herdr");
    }

    #[test]
    fn strip_segments_one_per_item() {
        let _g = guard();
        reset();
        let items = vec![item(1, "a", "claude", ""), item(2, "b", "codex", "")];
        let (_, segs) = strip_rows(&items);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].index, 0);
        assert_eq!(segs[1].index, 1);
        // contiguous, non-overlapping, increasing
        assert!(segs[1].x >= segs[0].x + segs[0].width);
    }

    #[test]
    fn chip_at_resolves_under_origin_row() {
        let _g = guard();
        reset();
        set_items(vec![item(1, "alpha", "claude", ""), item(2, "beta", "codex", "")]);
        // Both chips must land on ONE row for the "wrong row" assertion below to
        // mean what it says. `reset()` leaves PANEL_WIDTH at 0, which clamps to
        // the 20-cell floor and wraps `beta` onto row 1 directly beneath
        // `alpha` — so (col, oy + 1) resolved to chip 1, not to None, and the
        // test read as a hit-testing bug when it was really an unset width.
        *PANEL_WIDTH.lock().unwrap() = 80;
        *STRIP_ORIGIN.lock().unwrap() = Some((10, 5));
        let items = ITEMS.lock().unwrap().clone();
        let (_, segs) = strip_rows(&items);
        let s0 = &segs[0];
        let s1 = &segs[1];
        assert_eq!((s0.row, s1.row), (0, 0), "both chips fit on one row at w=80");
        let col = 10 + s0.x + s0.width / 2;
        assert_eq!(chip_at(col, 5), Some(0));
        assert_eq!(chip_at(col, 6), None); // wrong row
        assert_eq!(chip_at(10 + s1.x + s1.width / 2, 5), Some(1));
        reset();
    }

    /// The companion case the single-row test above cannot cover: once the strip
    /// wraps, a chip on row 1 is reachable ONLY at `oy + 1`, and the cell
    /// directly above it belongs to whichever chip owns row 0.
    #[test]
    fn chip_at_follows_a_wrapped_chip_onto_its_own_row() {
        let _g = guard();
        reset();
        set_items(vec![item(1, "alpha", "claude", ""), item(2, "beta", "codex", "")]);
        *PANEL_WIDTH.lock().unwrap() = 20; // narrow enough to force a wrap
        *STRIP_ORIGIN.lock().unwrap() = Some((10, 5));
        let items = ITEMS.lock().unwrap().clone();
        let (_, segs) = strip_rows(&items);
        assert_eq!((segs[0].row, segs[1].row), (0, 1), "narrow strip wraps");
        let col = 10 + segs[1].x + segs[1].width / 2;
        assert_eq!(chip_at(col, 6), Some(1));
        assert_eq!(chip_at(col, 5), Some(0)); // the row above is still chip 0
        assert_eq!(chip_at(col, 7), None); // nothing below the last row
        reset();
    }

    #[test]
    fn left_click_chip_emits_restore_with_slot_payload() {
        let _g = guard();
        reset();
        set_items(vec![item(3, "gamma", "claude", "resume text")]);
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        let items = ITEMS.lock().unwrap().clone();
        let (_, segs) = strip_rows(&items);
        let s = &segs[0];
        let col = s.x + s.width / 2;
        let update = handle_mouse(&mouse("left_down", col, 0));
        match update.effects.first().expect("one effect") {
            HostEffectV3::InvokePluginAction {
                plugin_id,
                action_id,
                payload_json,
            } => {
                assert_eq!(plugin_id, PLUGIN_ID);
                // The slot is in the PAYLOAD now, not baked into the action id.
                assert_eq!(action_id, "restore");
                assert_eq!(payload_json.as_deref(), Some(r#"{"slot":3}"#));
            }
            other => panic!("expected InvokePluginAction, got {other:?}"),
        }
        reset();
    }

    /// The wire form is the actual contract with the host: the tag must be
    /// `invoke_plugin_action` and `payload_json` must be a JSON *string*, not a
    /// nested object. Asserting on the enum alone would not catch a serde
    /// rename, and a wrong tag fails silently — the host drops the effect.
    #[test]
    fn invoke_effect_serializes_to_the_payload_wire_shape() {
        let wire = serde_json::to_value(invoke_effect("delete", 7)).expect("serialize");
        assert_eq!(wire["kind"], json!("invoke_plugin_action"));
        assert_eq!(wire["plugin_id"], json!(PLUGIN_ID));
        assert_eq!(wire["action_id"], json!("delete"));
        assert_eq!(wire["payload_json"], json!(r#"{"slot":7}"#));
    }

    /// No action id may encode a slot any more — that explosion is what this
    /// widget was collapsing. Catches a partial revert.
    #[test]
    fn no_effect_encodes_the_slot_in_its_action_id() {
        for (verb, slot) in [("restore", 1u32), ("fork", 10), ("delete", 4)] {
            match invoke_effect(verb, slot) {
                HostEffectV3::InvokePluginAction { action_id, .. } => {
                    assert_eq!(action_id, verb);
                    assert!(
                        !action_id.contains("slot"),
                        "action id '{action_id}' still encodes a slot"
                    );
                }
                other => panic!("expected InvokePluginAction, got {other:?}"),
            }
        }
    }

    #[test]
    fn right_click_opens_menu_and_renders_menu_row() {
        let _g = guard();
        reset();
        set_items(vec![item(1, "x", "codex", "r")]);
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        let items = ITEMS.lock().unwrap().clone();
        let (_, segs) = strip_rows(&items);
        let s = &segs[0];
        let col = s.x + s.width / 2;
        let update = handle_mouse(&mouse("right_down", col, 0));
        assert_eq!(*MENU_OPEN.lock().unwrap(), Some(0));
        assert!(update.effects.is_empty());
        // strip row + menu row
        assert_eq!(update.layers[0].nodes.len(), 2);
        reset();
    }

    #[test]
    fn fork_and_delete_menu_buttons_carry_the_slot_payload() {
        let _g = guard();
        reset();
        set_items(vec![item(7, "y", "claude", "r")]);
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        *MENU_OPEN.lock().unwrap() = Some(0);
        let (fx, fw, dx, dw) = menu_segments();
        // fork button (menu row is at oy+1 == row 1)
        let update = handle_mouse(&mouse("left_down", fx + fw / 2, 1));
        match update.effects.first().expect("fork effect") {
            HostEffectV3::InvokePluginAction {
                action_id,
                payload_json,
                ..
            } => {
                assert_eq!(action_id, "fork");
                assert_eq!(payload_json.as_deref(), Some(r#"{"slot":7}"#));
            }
            other => panic!("expected InvokePluginAction, got {other:?}"),
        }
        // menu closed after acting
        assert_eq!(*MENU_OPEN.lock().unwrap(), None);

        // reopen, click delete
        *MENU_OPEN.lock().unwrap() = Some(0);
        let update = handle_mouse(&mouse("left_down", dx + dw / 2, 1));
        match update.effects.first().expect("delete effect") {
            HostEffectV3::InvokePluginAction {
                action_id,
                payload_json,
                ..
            } => {
                assert_eq!(action_id, "delete");
                assert_eq!(payload_json.as_deref(), Some(r#"{"slot":7}"#));
            }
            other => panic!("expected InvokePluginAction, got {other:?}"),
        }
        reset();
    }

    #[test]
    fn left_click_elsewhere_dismisses_open_menu_without_restore() {
        let _g = guard();
        reset();
        set_items(vec![item(1, "z", "claude", "r")]);
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        *MENU_OPEN.lock().unwrap() = Some(0);
        // click far away (empty space): closes menu, no effect
        let update = handle_mouse(&mouse("left_down", 999, 0));
        assert_eq!(*MENU_OPEN.lock().unwrap(), None);
        assert!(update.effects.is_empty());
        reset();
    }

    #[test]
    fn left_click_chip_while_menu_open_only_dismisses() {
        let _g = guard();
        reset();
        set_items(vec![item(1, "z", "claude", "r")]);
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        *MENU_OPEN.lock().unwrap() = Some(0);
        let items = ITEMS.lock().unwrap().clone();
        let (_, segs) = strip_rows(&items);
        let s = &segs[0];
        // clicking the chip (not a menu button) while menu open dismisses, no restore
        let update = handle_mouse(&mouse("left_down", s.x + s.width / 2, 0));
        assert_eq!(*MENU_OPEN.lock().unwrap(), None);
        assert!(update.effects.is_empty());
        reset();
    }

    #[test]
    fn move_sets_hover_and_renders_tooltip() {
        let _g = guard();
        reset();
        set_items(vec![item(1, "alpha", "claude", "first … last")]);
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        let items = ITEMS.lock().unwrap().clone();
        let (_, segs) = strip_rows(&items);
        let s = &segs[0];
        let update = handle_mouse(&mouse("move", s.x + s.width / 2, 0));
        assert_eq!(*HOVERED.lock().unwrap(), Some(0));
        assert_eq!(update.layers[0].nodes.len(), 2); // strip + tooltip
        // move off strip clears hover
        let update = handle_mouse(&mouse("move", 999, 0));
        assert_eq!(*HOVERED.lock().unwrap(), None);
        assert_eq!(update.layers[0].nodes.len(), 1);
        reset();
    }

    #[test]
    fn strip_is_first_node_in_all_states() {
        let _g = guard();
        reset();
        set_items(vec![item(1, "a", "claude", "r")]);
        *STRIP_ORIGIN.lock().unwrap() = Some((0, 0));
        assert_eq!(archive_layer().nodes.len(), 1);
        *HOVERED.lock().unwrap() = Some(0);
        assert_eq!(archive_layer().nodes.len(), 2);
        *HOVERED.lock().unwrap() = None;
        *MENU_OPEN.lock().unwrap() = Some(0);
        assert_eq!(archive_layer().nodes.len(), 2);
        reset();
    }

    #[test]
    fn no_declarative_hit_rects_declared() {
        let _g = guard();
        reset();
        set_items(vec![item(1, "a", "claude", "r")]);
        let layer = archive_layer();
        assert!(layer.hits.is_empty());
    }

    #[test]
    fn frame_parses_input_and_clamps_stale_indices() {
        let _g = guard();
        reset();
        // stale hover pointing past the new (shorter) list must be cleared
        *HOVERED.lock().unwrap() = Some(5);
        let mut env = std::collections::HashMap::new();
        env.insert(
            "HERDR_PLUGIN_UI_INPUT_JSON".to_string(),
            r#"{"items":[{"slot":1,"label":"only","agent":"claude","resume":"r"}]}"#.to_string(),
        );
        let frame_json = serde_json::to_string(&json!({
            "type": "frame",
            "surface": "panel",
            "rect": {"x": 4, "y": 40, "width": 60, "height": 3},
            "focused": false,
            "host": {"focus": {"session_id": null, "session_ref": null, "cwd": null, "agent": "None", "is_self_focus": false, "subagent": null}, "env": env}
        }))
        .unwrap();
        let event: V3Event = serde_json::from_str(&frame_json).unwrap();
        let _ = handle_event(&event);
        assert_eq!(ITEMS.lock().unwrap().len(), 1);
        assert_eq!(*STRIP_ORIGIN.lock().unwrap(), Some((4, 40)));
        assert_eq!(*HOVERED.lock().unwrap(), None); // clamped
        reset();
    }

    #[test]
    fn max_chips_truncates_to_ten() {
        let _g = guard();
        reset();
        let items: Vec<Value> = (1..=15)
            .map(|n| json!({"slot": n, "label": format!("s{n}"), "agent": "claude", "resume": ""}))
            .collect();
        let mut env = std::collections::HashMap::new();
        env.insert(
            "HERDR_PLUGIN_UI_INPUT_JSON".to_string(),
            serde_json::to_string(&json!({"items": items})).unwrap(),
        );
        let frame_json = serde_json::to_string(&json!({
            "type": "frame",
            "surface": "panel",
            "rect": {"x": 0, "y": 0, "width": 200, "height": 3},
            "focused": false,
            "host": {"focus": {"session_id": null, "session_ref": null, "cwd": null, "agent": "None", "is_self_focus": false, "subagent": null}, "env": env}
        }))
        .unwrap();
        let event: V3Event = serde_json::from_str(&frame_json).unwrap();
        let _ = handle_event(&event);
        assert_eq!(ITEMS.lock().unwrap().len(), MAX_CHIPS);
        reset();
    }
}
