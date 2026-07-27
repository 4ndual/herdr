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
    /// Focused pane is a plain shell / non-agent (or nothing focused).
    #[default]
    None,
}

/// What herdr knows about the focused pane, delivered to plugin surfaces through
/// the WASM UI bridge: the `FocusedSession` capability, the `HERDR_FOCUS_*` frame
/// env, and `UiEvent::FocusChanged`. The host fills it from its own `AppState`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FocusInput {
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
    CopyText(String),
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
