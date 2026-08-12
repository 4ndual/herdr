use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub const API_VERSION: u32 = 3;

/// Which agent CLI occupies the focused pane. Part of the focused-session
/// context the bridge delivers to plugins (the `FocusedSession` capability and
/// the `HERDR_FOCUS_*` frame env).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AgentKind {
    Claude,
    Codex,
    Pi,
    Jcode,
    /// Focused pane is a plain shell / non-agent (or nothing focused).
    #[default]
    None,
}

/// What herdr knows about the focused pane, delivered to plugin surfaces through
/// the WASM UI bridge: the `FocusedSession` capability, the `HERDR_FOCUS_*` frame
/// env, and `UiEvent::FocusChanged`. The host fills it from its own `AppState`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FocusInput {
    /// Public pane identifier matching the pane API roster (for example `w1:p1`).
    pub pane_id: Option<String>,
    /// Agent session id keyed into the usage store (often a transcript stem).
    /// For path-backed sessions like pi, herdr derives this from the session
    /// file stem so the reader can still resolve transcript files.
    pub session_id: Option<String>,
    /// Raw native session reference used to resume the focused agent backend.
    /// Examples: Claude/Codex UUID, Pi session path.
    pub session_ref: Option<String>,
    /// Working directory of the focused pane, used for cwd-keyed sources.
    pub cwd: Option<PathBuf>,
    /// Which agent CLI is focused.
    pub agent: AgentKind,
    /// True when the focused pane is a plugin surface/self — the host asks the
    /// plugin to keep its previous snapshot (sticky, no flicker).
    pub is_self_focus: bool,
    /// fork(wave-1): name of the subagent the focused Claude pane is currently
    /// VIEWING (Claude Code's "Viewing @<name>" / "Message @<name>…" subview),
    /// with the leading `@` stripped. `Some` only when the focused agent is
    /// Claude and its bottom-screen chrome shows the subagent-view marker; the
    /// usage source then reports that subagent's model + context instead of the
    /// main session's (plan-quota rows stay the account's — same plan).
    pub subagent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSurface {
    Header,
    Footer,
    TabBar,
    Sidebar,
    RightPanel,
    Overlay,
    ContextMenu,
    /// The multi-row usage dashboard strip (maps to the host's `usage_dash_area`).
    /// Added in API v2 for the WASM usage-dash module.
    UsageStrip,
    /// A generic, plugin-placed panel. Its rect is NOT a hardcoded per-variant
    /// match: the host lays it out from the DATA in the module's manifest
    /// `placement` spec (region/align/size/min_peer/z), so adding, moving, or
    /// resizing a panel is pure manifest hot-reload with no host rebuild. The
    /// host publishes each resolved panel rect (keyed by `plugin_id:module_id`)
    /// in the layout blob's `panel_areas`, and delivers it as the surface's
    /// frame `rect`, so the guest offsets its absolute hit-rects from it.
    Panel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiCapability {
    FocusedSession,
    ThemeRead,
    LayoutRead,
    MouseAll,
    KeyboardInput,
    ContextMenuAugment,
    ClipboardWrite,
    PaneInput,
    Toasts,
    OpenPluginPanes,
    PluginActions,
    /// API v3 (§5.3): read-only socket API methods via `HostEffect::Api`.
    ApiRead,
    /// API v3 (§5.3): structural write socket API methods via `HostEffect::Api`.
    ApiWrite,
    /// API v3 (§5.3): text/keystroke-injecting socket API methods via
    /// `HostEffect::Api`. Split out from `ApiWrite` because injecting text into a
    /// pane/agent is arbitrary command execution in the user's shells.
    ApiInput,
    /// fork: `pane.graphics.set`/`.clear`/`.info` via `HostEffect::Api`. Split
    /// out from `ApiWrite` for the same reason `ApiInput` is: it is a distinct
    /// power — painting into a pane the module does not own — and it should be
    /// grantable, refusable and visible on its own.
    PaneGraphics,
    /// fork: `plugin.enable`/`plugin.disable` via `HostEffect::Api`. Split out
    /// from `ApiWrite` because a module holding it can turn OTHER modules off,
    /// including ones the user relies on — so it must be declared, visible in
    /// the manifest, and refusable on its own rather than riding along with
    /// ordinary structural writes. `plugin.link`/`plugin.unlink` stay
    /// host-reserved: installing code from a path is a strictly larger power
    /// than flipping an already-installed plugin's enabled bit.
    PluginAdmin,
    /// fork(host-actions): observe or replace a host-drawn control named in the
    /// module's manifest `host_actions`. Its own capability rather than a fold
    /// into `MouseAll` because the powers differ in kind: `MouseAll` is a
    /// read-only firehose of coordinates, while this one can SUPPRESS a native
    /// button. A user granting "see my clicks" has not granted "turn off my
    /// new-tab button", and `plugin list` should show which is which.
    HostActions,
    /// fork(host-actions): request exclusive keyboard routing via
    /// `HostEffect::SetKeyboardCapture`. Split from `KeyboardInput` because the
    /// powers differ in kind: `KeyboardInput` is a broadcast COPY of keys (the
    /// pane still receives them — the documented non-consumable semantics that
    /// doctor-dash's safety argument depends on), while capture REDIRECTS keys
    /// away from the pane and the host's own mode handlers. A module holding
    /// only `KeyboardInput` must never be able to starve the user's terminal
    /// of keystrokes.
    KeyboardCapture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPluginManifestRef {
    pub plugin_id: String,
    pub module_id: String,
    pub api_version: u32,
    pub surfaces: Vec<UiSurface>,
    pub requested_capabilities: Vec<UiCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub focus: FocusInput,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiLayoutRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiFrameContext {
    pub surface: UiSurface,
    pub rect: UiLayoutRect,
    pub focused: bool,
    pub host: HostSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub column: u16,
    pub row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseEventKind {
    Move,
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
    MiddleDown,
    MiddleUp,
    ScrollUp,
    ScrollDown,
    Drag,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub key: String,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub meta: bool,
}

/// Kind of click that resolved to a v3 `UiEvent::Action` (spec §4). A closed set
/// distinct from [`MouseEventKind`]: actions never fire for move/drag/scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiActionKind {
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
    MiddleDown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiEvent {
    Frame(UiFrameContext),
    Mouse(MouseEvent),
    Key(KeyEvent),
    FocusChanged(FocusInput),
    Tick,
    ThemeChanged,
    ReloadRequested,
    /// API v3 (§4): dynamic click routing. Delivered only to the v3 module that
    /// declared the `UiHitRect` the click resolved to; `column`/`row` are
    /// surface-local (relative to the surface rect origin from the module's most
    /// recent `frame` event for that surface).
    Action {
        action_id: String,
        surface: UiSurface,
        column: u16,
        row: u16,
        kind: UiActionKind,
    },
    /// fork(host-actions): a host-drawn control this module claimed was
    /// activated. `action_id` is one of the host's stable `host.*` ids, never a
    /// module-private one, which is what separates this from [`Self::Action`].
    ///
    /// `mode` echoes the manifest claim so a module holding both kinds can tell
    /// "the host already did this, you are being told" from "the host did
    /// nothing, this is yours to handle" without tracking its own manifest.
    ///
    /// Deliberately NOT gated on a new `API_VERSION`: delivery is gated on the
    /// module having NAMED the action in its manifest, which is a stronger and
    /// more honest opt-in than a version number, and it leaves the negotiation
    /// table (`2 | 3`) untouched for upstream.
    HostAction {
        action_id: String,
        mode: UiHostActionMode,
    },
    /// API v3 (§5.2): the synchronous result of a `HostEffect::Api` request,
    /// delivered only to v3 modules that are still registered at dispatch time.
    ApiResult {
        request_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// fork(host-actions): whether the host still ran its own default body for a
/// claimed control. Mirrors the manifest `host_action_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiHostActionMode {
    /// The host ran its default; this is a notification.
    #[default]
    Observe,
    /// The host skipped its default; the module owns the outcome.
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiTextSpan {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Text modifiers (API v2). Default false so v1 spans deserialize unchanged.
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiActionRef {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiNode {
    Line {
        spans: Vec<UiTextSpan>,
    },
    Menu {
        title: String,
        items: Vec<UiActionRef>,
        selected: usize,
    },
    Tabs {
        items: Vec<UiActionRef>,
        selected: usize,
    },
    Status {
        left: Vec<UiTextSpan>,
        right: Vec<UiTextSpan>,
    },
}

/// An absolute-coordinate clickable region a layer exposes (API v2). The guest
/// computes these while rendering (e.g. the usage STATE/STATS toggles, or goal
/// node/chip rows); the host resolves a click inside one to its `action_id` and
/// dispatches it back to the owning module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiHitRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub action_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiLayer {
    pub surface: UiSurface,
    #[serde(default)]
    pub z_index: i16,
    #[serde(default)]
    pub consumes_mouse: bool,
    #[serde(default)]
    pub nodes: Vec<UiNode>,
    /// Absolute-coordinate clickable regions (API v2). Empty for v1 plugins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hits: Vec<UiHitRect>,
    /// Free-form module-owned payload (API v2). The goal module ships its full
    /// `GoalHits` JSON here (richer than `hits`); the usage path leaves it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostEffect {
    /// Copy text through the host clipboard path. This is a struct variant, not
    /// a newtype: `HostEffect` is internally tagged, and serde cannot serialize
    /// a tagged newtype variant wrapping a string — it fails at run time with
    /// "cannot serialize tagged newtype variant", which silently truncates the
    /// emitting module's whole update to zero bytes and blanks its layer.
    CopyText {
        text: String,
    },
    SendPaneKeys {
        lines: Vec<String>,
    },
    ShowToast {
        message: String,
    },
    OpenPluginPane {
        plugin_id: String,
        entrypoint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload_json: Option<String>,
    },
    InvokePluginAction {
        plugin_id: String,
        action_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload_json: Option<String>,
    },
    /// API v3 (§5.1): guest → host bridge call. `request_id` is guest-chosen and
    /// opaque; the host echoes it verbatim in the matching `UiEvent::ApiResult`.
    /// `method`/`params` mirror the existing socket API surface exactly
    /// (src/api/schema.rs `Method` serde tag/content) and are executed
    /// synchronously, in-process, in effect-declaration order.
    Api {
        request_id: String,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    /// API v3 (§7.2): collapse/expand one of the module's own declared
    /// surfaces. The host reclaims the surface's reserved rows for panes while
    /// collapsed (usage strip: full reserve ↔ 1 row) and restores them on
    /// expand. Filtered against the module's manifest `surfaces` — a module
    /// can only collapse surfaces it declares.
    SetSurfaceCollapsed {
        surface: UiSurface,
        collapsed: bool,
    },
    /// fork(host-actions): claim (or release) exclusive keyboard routing.
    /// While a module holds capture, the host delivers every key press to THAT
    /// module only — the focused pane and the host's own mode handlers see
    /// nothing. This is what makes a plugin-drawn form with text fields
    /// possible at all: without it keys are broadcast, so typing into an
    /// overlay would also type into the terminal underneath.
    ///
    /// Gated on [`UiCapability::KeyboardCapture`]. The host force-releases on
    /// ctrl+escape (a failsafe the guest cannot override), when the module
    /// unloads, and when its instance traps — a buggy guest must never be able
    /// to hold the user's keyboard hostage.
    SetKeyboardCapture {
        captured: bool,
    },
}

/// API v3 (§6) update merge scope. Only meaningful for v3 modules — v2 modules
/// keep implicit full-replace regardless of what this field parses to, decided
/// host-side from the module's negotiated version, so the `Default` below (used
/// when the field is entirely absent from a v2 payload) is inert for v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiUpdateScope {
    /// Merge keyed by surface: replaces all z-levels of each surface named in
    /// `layers`, leaving unnamed surfaces untouched. Default for v3.
    #[default]
    Partial,
    /// v2 semantics: replaces the module's entire cached layer set.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiUpdate {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<UiLayer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<HostEffect>,
    /// API v3 (§6). Ignored (forced to `Full`) for v2 modules by the host.
    #[serde(default)]
    pub scope: UiUpdateScope,
    /// Dead since v2 (F3); v3 parsing no longer reads it, but the field stays so
    /// old v2 payloads that still emit it keep parsing (accept-and-ignore).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumed_action_ids: Vec<String>,
}

/// Guest-side rendering support: render with ratatui into an off-screen
/// [`ratatui::buffer::Buffer`], then flatten it back into [`UiNode::Line`] rows
/// that the host's existing Line renderer paints verbatim. This lets a WASM
/// module reuse the exact native `render.rs` layout code (compiled to wasm) while
/// speaking only the host's text+role vocabulary. The host never enables this.
#[cfg(feature = "guest")]
pub mod guest {
    use super::{UiNode, UiTextSpan};
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};

    /// Map a semantic ratatui named color to the host role string. Truecolor /
    /// indexed / reset cells map to `None` (host draws them as default text).
    /// Mirrors the host's old `recolor_to_palette` mapping so themed colors match.
    pub fn color_role(c: Color) -> Option<&'static str> {
        Some(match c {
            Color::Green | Color::LightGreen => "success",
            Color::Yellow | Color::LightYellow => "warning",
            Color::Red | Color::LightRed => "danger",
            Color::Blue | Color::LightBlue => "blue",
            Color::Cyan | Color::LightCyan => "teal",
            Color::Magenta | Color::LightMagenta => "mauve",
            Color::Gray => "subtext",
            Color::DarkGray => "muted",
            Color::White => "default",
            _ => return None,
        })
    }

    #[derive(PartialEq, Eq, Clone, Copy)]
    struct CellStyle {
        role: Option<&'static str>,
        bold: bool,
        underline: bool,
        italic: bool,
    }

    /// Flatten every row of `buf` into one [`UiNode::Line`], run-length-merging
    /// adjacent cells that share a style into a single span. Trailing default
    /// whitespace on each row is dropped so the host doesn't paint empty bg.
    pub fn flatten_buffer(buf: &Buffer) -> Vec<UiNode> {
        let area = buf.area;
        let mut nodes = Vec::with_capacity(area.height as usize);
        for y in area.top()..area.bottom() {
            let mut spans: Vec<UiTextSpan> = Vec::new();
            let mut cur_text = String::new();
            let mut cur_style: Option<CellStyle> = None;
            for x in area.left()..area.right() {
                let cell = &buf[(x, y)];
                let m = cell.modifier;
                let style = CellStyle {
                    role: color_role(cell.fg),
                    bold: m.contains(Modifier::BOLD),
                    underline: m.contains(Modifier::UNDERLINED),
                    italic: m.contains(Modifier::ITALIC),
                };
                let sym = cell.symbol();
                match cur_style {
                    Some(s) if s == style => cur_text.push_str(sym),
                    _ => {
                        if let Some(s) = cur_style.take() {
                            push_span(&mut spans, &cur_text, s);
                        }
                        cur_text.clear();
                        cur_text.push_str(sym);
                        cur_style = Some(style);
                    }
                }
            }
            if let Some(s) = cur_style.take() {
                push_span(&mut spans, &cur_text, s);
            }
            trim_trailing_default(&mut spans);
            nodes.push(UiNode::Line { spans });
        }
        nodes
    }

    fn push_span(spans: &mut Vec<UiTextSpan>, text: &str, s: CellStyle) {
        spans.push(UiTextSpan {
            text: text.to_string(),
            role: s.role.map(str::to_string),
            bold: s.bold,
            underline: s.underline,
            italic: s.italic,
        });
    }

    /// Drop trailing spans that are pure whitespace with no role/modifiers, so a
    /// padded row doesn't carry a long empty tail into the host paint.
    fn trim_trailing_default(spans: &mut Vec<UiTextSpan>) {
        while let Some(last) = spans.last() {
            let plain = last.role.is_none() && !last.bold && !last.underline && !last.italic;
            if plain && last.text.trim().is_empty() {
                spans.pop();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod host_effect_tests {
    use super::*;

    /// Every `HostEffect` must survive a JSON round trip.
    ///
    /// `HostEffect` is internally tagged (`#[serde(tag = "kind")]`), and serde
    /// cannot serialize an internally-tagged NEWTYPE variant that wraps a
    /// primitive: it fails at run time, not compile time. That failure is
    /// silent and catastrophic in practice — the guest's whole update is
    /// truncated to zero bytes, so the emitting module's entire layer blanks
    /// rather than just dropping the one effect. `CopyText` shipped that way.
    ///
    /// This test exists so a future primitive-wrapping variant fails here
    /// instead of in a user's blanked panel.
    #[test]
    fn every_host_effect_round_trips_through_json() {
        let effects = vec![
            HostEffect::CopyText {
                text: "hello".into(),
            },
            HostEffect::SendPaneKeys {
                lines: vec!["echo hi".into()],
            },
            HostEffect::ShowToast {
                message: "done".into(),
            },
            HostEffect::OpenPluginPane {
                plugin_id: "p".into(),
                entrypoint: "e".into(),
                payload_json: Some("{}".into()),
            },
            HostEffect::InvokePluginAction {
                plugin_id: "p".into(),
                action_id: "a".into(),
                payload_json: None,
            },
            HostEffect::Api {
                request_id: "r1".into(),
                method: "pane.info".into(),
                params: serde_json::json!({"pane": "w1:p1"}),
            },
            HostEffect::SetSurfaceCollapsed {
                surface: UiSurface::UsageStrip,
                collapsed: true,
            },
            HostEffect::SetKeyboardCapture { captured: true },
        ];

        for effect in effects {
            let encoded = serde_json::to_string(&effect)
                .unwrap_or_else(|err| panic!("failed to serialize {effect:?}: {err}"));
            let decoded: HostEffect = serde_json::from_str(&encoded)
                .unwrap_or_else(|err| panic!("failed to deserialize {encoded}: {err}"));
            assert_eq!(decoded, effect, "round trip changed the effect");
        }
    }

    /// Pin the wire shape the guests actually emit, so the field name cannot
    /// drift without a deliberate edit here.
    #[test]
    fn copy_text_encodes_as_a_tagged_object_with_a_text_field() {
        let encoded = serde_json::to_string(&HostEffect::CopyText {
            text: "/tmp/x".into(),
        })
        .expect("copy_text must serialize");
        assert_eq!(encoded, r#"{"kind":"copy_text","text":"/tmp/x"}"#);
    }
}

// ── Pane graphics placement ────────────────────────────────────────────────

/// Where a pane-graphics (or pane-text) layer sits, in terminal CELLS.
///
/// # Why this lives HERE and not in the host
///
/// It used to live only in the herdr binary crate, as
/// `api::schema::PaneGraphicsPlacementParams`. Guests cannot reach that, so
/// every guest hand-rolled the JSON object — and got no compile-time check on
/// the field names. `claude-image-preview` shipped `col`/`row` instead of
/// `viewport_col`/`viewport_row` for its entire life: the host accepted the
/// call, discarded the unknown keys, defaulted the real fields, and painted
/// every image at the pane's top-left, stacked over the text it was annotating.
/// The call returned success and logged nothing, and the plugin's own test
/// suite was green because it asserted on the same wrong names.
///
/// Sharing the type is the only fix that makes that class of mistake a COMPILE
/// error rather than a silent misrender. `deny_unknown_fields` on the host
/// makes it a loud runtime error for callers that still build JSON by hand;
/// this makes it impossible for callers that do not.
///
/// # Extents are `Option`, and `Some(0)` is invalid
///
/// A missing extent means "the whole pane in that axis" — the long-standing
/// default, and the reason `Default` is still meaningful. An extent that is
/// present but zero is REJECTED rather than silently meaning whole-pane,
/// because those two are worlds apart on screen and the difference used to be
/// one typo wide: any misspelled key defaulted its real field to 0, and 0 read
/// as "fill the pane", so a typo became a full-pane opaque overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PaneGraphicsPlacement {
    /// Column of the layer's left edge, relative to the pane's text viewport.
    /// May be negative: a placement starting off the left edge paints its
    /// visible remainder, matching kitty.
    #[serde(default)]
    pub viewport_col: i32,
    /// Row of the layer's top edge. Negative is legal, as above.
    #[serde(default)]
    pub viewport_row: i32,
    /// Width in cells. `None` = the full pane width. `Some(0)` is an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_cols: Option<u32>,
    /// Height in cells. `None` = the full pane height. `Some(0)` is an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_rows: Option<u32>,
}

impl PaneGraphicsPlacement {
    /// The whole pane, in both axes.
    ///
    /// Spelled out rather than left to `Default::default()` so that "I meant the
    /// entire pane" is visible at the call site. It is also the answer to the
    /// question `box_at` panics on: a zero-sized box is never the way to ask for
    /// everything.
    pub const fn whole_pane() -> Self {
        Self {
            viewport_col: 0,
            viewport_row: 0,
            grid_cols: None,
            grid_rows: None,
        }
    }

    /// A box of `cols` x `rows` cells with its top-left at `col`/`row`.
    ///
    /// Takes bare `u32` on purpose: a caller writing `Some(cols)` is reasoning
    /// about the representation again, which is what these constructors exist to
    /// stop.
    ///
    /// # Panics
    ///
    /// If either extent is zero. Zero is not "the whole pane" — that is
    /// [`whole_pane`](Self::whole_pane) — and it is not a size either, so there
    /// is no honest placement to build.
    ///
    /// **This is a compile error only when the call is actually const-evaluated**
    /// (a `const`/`static` initialiser, or a const-generic position). In ordinary
    /// code — which is nearly every real call site — `box_at(x, y, 0, 0)` is a
    /// normal runtime call and panics at runtime like any other assert. Do not
    /// rely on the build catching a literal zero for you. For extents computed at
    /// run time that may legitimately clamp to zero, use
    /// [`try_box_at`](Self::try_box_at) and handle the `None`.
    pub const fn box_at(col: i32, row: i32, cols: u32, rows: u32) -> Self {
        assert!(
            cols > 0,
            "placement width must be greater than 0; use PaneGraphicsPlacement::whole_pane() for the full pane"
        );
        assert!(
            rows > 0,
            "placement height must be greater than 0; use PaneGraphicsPlacement::whole_pane() for the full pane"
        );
        Self {
            viewport_col: col,
            viewport_row: row,
            grid_cols: Some(cols),
            grid_rows: Some(rows),
        }
    }

    /// [`box_at`](Self::box_at) for extents that may legitimately be zero,
    /// returning `None` instead of panicking.
    ///
    /// This exists so that a caller whose `cols`/`rows` come out of a layout
    /// calculation has somewhere to go other than the panicking constructor.
    /// Without it the ergonomic path is also the fragile one, and a guest that
    /// clamps to zero takes the whole plugin down instead of skipping a frame.
    pub fn try_box_at(col: i32, row: i32, cols: u32, rows: u32) -> Option<Self> {
        if cols == 0 || rows == 0 {
            return None;
        }
        Some(Self::box_at(col, row, cols, rows))
    }

    /// `Err` with a caller-facing reason when an extent is present but zero.
    ///
    /// Deliberately NOT a clamp-to-1 or a fallback to whole-pane: both would put
    /// something plausible on screen and leave the caller believing its numbers
    /// were used, which is the failure mode this whole type exists to end.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.grid_cols == Some(0) {
            return Err(
                "placement.grid_cols must be greater than 0; omit it for the full pane width",
            );
        }
        if self.grid_rows == Some(0) {
            return Err(
                "placement.grid_rows must be greater than 0; omit it for the full pane height",
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod placement_tests {
    use super::PaneGraphicsPlacement;

    #[test]
    fn whole_pane_omits_both_extents_rather_than_zeroing_them() {
        let placement = PaneGraphicsPlacement::whole_pane();
        assert_eq!(placement.grid_cols, None);
        assert_eq!(placement.grid_rows, None);
        assert!(placement.validate().is_ok());
        // The distinction the whole type turns on: "whole pane" must not be
        // expressible as a present zero, or we are back to the sentinel.
        assert_ne!(
            placement,
            PaneGraphicsPlacement {
                grid_cols: Some(0),
                grid_rows: Some(0),
                ..Default::default()
            }
        );
    }

    #[test]
    fn box_at_wraps_the_extents_so_callers_never_write_some() {
        let placement = PaneGraphicsPlacement::box_at(2, 1, 40, 12);
        assert_eq!(placement.viewport_col, 2);
        assert_eq!(placement.viewport_row, 1);
        assert_eq!(placement.grid_cols, Some(40));
        assert_eq!(placement.grid_rows, Some(12));
        assert!(placement.validate().is_ok());
    }

    #[test]
    #[should_panic(expected = "placement width must be greater than 0")]
    fn box_at_panics_rather_than_building_a_zero_extent() {
        // `validate` only runs on the wire path, so without this assert the
        // ergonomic constructor could build in-process exactly the state every
        // other layer rejects.
        let _ = PaneGraphicsPlacement::box_at(0, 0, 0, 4);
    }

    #[test]
    #[should_panic(expected = "placement height must be greater than 0")]
    fn box_at_panics_on_a_zero_height_too() {
        let _ = PaneGraphicsPlacement::box_at(0, 0, 4, 0);
    }

    #[test]
    fn try_box_at_is_none_for_a_degenerate_box_and_never_upgrades_it() {
        assert_eq!(PaneGraphicsPlacement::try_box_at(0, 0, 0, 4), None);
        assert_eq!(PaneGraphicsPlacement::try_box_at(0, 0, 4, 0), None);
        // Specifically NOT whole_pane(): mapping a zero extent to "everything"
        // is how a nothing-sized box becomes a full-pane overlay, which is the
        // original bug wearing a different hat.
        assert_ne!(
            PaneGraphicsPlacement::try_box_at(0, 0, 0, 0),
            Some(PaneGraphicsPlacement::whole_pane())
        );
        assert_eq!(
            PaneGraphicsPlacement::try_box_at(3, 4, 10, 5),
            Some(PaneGraphicsPlacement::box_at(3, 4, 10, 5))
        );
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_defaulted() {
        // This is the exact payload claude-image-preview shipped. Before
        // deny_unknown_fields it deserialized happily into an all-zero
        // placement, which the painter read as "the whole pane".
        let err = serde_json::from_str::<PaneGraphicsPlacement>(
            r#"{"col":2,"row":1,"grid_cols":40,"grid_rows":12}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("col"), "{err}");
    }

    #[test]
    fn omitted_extents_mean_whole_pane_and_stay_omitted_on_the_wire() {
        let placement = serde_json::from_str::<PaneGraphicsPlacement>(r#"{}"#).unwrap();
        assert_eq!(placement, PaneGraphicsPlacement::default());
        assert_eq!(placement.grid_cols, None);
        assert!(placement.validate().is_ok());
        assert_eq!(
            serde_json::to_string(&placement).unwrap(),
            r#"{"viewport_col":0,"viewport_row":0}"#
        );
    }

    #[test]
    fn a_present_but_zero_extent_is_an_error_not_a_full_pane_overlay() {
        let placement = PaneGraphicsPlacement {
            grid_cols: Some(0),
            ..Default::default()
        };
        assert!(placement.validate().unwrap_err().contains("grid_cols"));
        let placement = PaneGraphicsPlacement {
            grid_rows: Some(0),
            ..Default::default()
        };
        assert!(placement.validate().unwrap_err().contains("grid_rows"));
    }

    #[test]
    fn a_negative_origin_is_legal() {
        let placement =
            serde_json::from_str::<PaneGraphicsPlacement>(r#"{"viewport_col":-3,"grid_cols":10}"#)
                .unwrap();
        assert_eq!(placement.viewport_col, -3);
        assert_eq!(placement.grid_cols, Some(10));
        assert!(placement.validate().is_ok());
    }
}
