use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::AgentStatus;
use super::common::SplitDirection;
use super::panes::PaneInfo;
use super::workspaces::WorkspaceWorktreeInfo;
use crate::popup_size::PopupSize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginLinkParams {
    pub path: String,
    #[serde(default = "super::common::default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PluginSourceInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PluginListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
}

/// fork(+127): params for `plugin.ui.status`. Both filters are optional; omitting
/// both returns every registered UI module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginUiStatusParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_id: Option<String>,
}

/// fork(+127): read-only live status of one registered UI module.
///
/// This type is the answer to the question that cost hours: *what is actually
/// loaded in this running instance, and if my thing is not showing, why not?*
/// Before it, answering that required rebuilding a mirror sandbox of the whole
/// config, because nothing on the wire distinguished "module failed to load"
/// from "module loaded and produced nothing" from "module produced nodes that
/// did not fit". Each of those is a different field here.
///
/// Strictly read-only and side-effect free, so it is safe to poll and safe to
/// expose to plugins themselves under `api_read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginUiModuleStatus {
    pub plugin_id: String,
    pub module_id: String,
    /// Whether the owning plugin is enabled in the registry. A module can be
    /// registered-but-disabled; distinguishing that from "not registered" is the
    /// first fork in the diagnostic tree.
    pub enabled: bool,
    /// Whether a guest instance is live right now. `false` with `last_error` set
    /// is a load/hot-reload failure; `false` with no error means never loaded.
    pub loaded: bool,
    /// `api_version` as DECLARED in the manifest.
    pub api_version_declared: u32,
    /// The version actually negotiated with the guest's export. A mismatch with
    /// `api_version_declared` is silent at runtime and changes which events the
    /// module receives, which makes it worth showing side by side.
    pub negotiated_version: u32,
    pub hot_reload: String,
    pub surfaces: Vec<String>,
    /// Capabilities the manifest requested. Shown so "my API call is being
    /// denied" can be checked against what was actually granted without reading
    /// the manifest off disk.
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graphics_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_layers_per_pane: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_input_file: Option<String>,
    /// Whether this module declares an out-of-process collector.
    pub collector_declared: bool,
    /// Last load / hot-reload / guest-call error, or `None` if the last attempt
    /// succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// fork(+127): why this module's PANEL was dropped in the last laid-out
    /// frame, if it was. `None` means it was not dropped — either it rendered or
    /// it declares no panel surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_skip_reason: Option<String>,
    /// Content dims measured for the panel in the last laid-out frame. `(0, 0)`
    /// with `last_skip_reason == "no_content"` is the "module produced nothing"
    /// case; non-zero dims with a skip reason is the "did not fit" case.
    pub panel_content_w: u16,
    pub panel_content_h: u16,
    /// How many `pane.graphics` layers this module's PLUGIN currently holds,
    /// across every pane, and how many bytes they retain.
    ///
    /// Plugin-scoped, not module-scoped, because layer ownership is stamped with
    /// the plugin id — that is also the granularity `plugin disable` operates at.
    /// `layer_bytes` is the honest per-plugin RAM number for graphics: guest
    /// memory lives inside a wasm instance and is invisible to `footprint`, so
    /// the host accounting here is the only way to attribute it.
    pub layer_count: usize,
    pub layer_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginUnlinkParams {
    pub plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginSetEnabledParams {
    pub plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InstalledPluginInfo {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub min_herdr_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub manifest_path: String,
    pub plugin_root: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginPlatform>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build: Vec<PluginManifestBuild>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub startup: Vec<PluginManifestStartup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<PluginManifestAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<PluginManifestEventHook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<PluginManifestPane>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui_modules: Vec<PluginManifestUiModule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link_handlers: Vec<PluginManifestLinkHandler>,
    #[serde(default)]
    pub source: PluginSourceInfo,
    /// Warnings collected at link time or on registry load (e.g. unknown event names,
    /// missing manifest file). Non-fatal — the entry is kept and surfaced by plugin.list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginSourceInfo {
    #[serde(default)]
    pub kind: PluginSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_unix_ms: Option<u64>,
}

impl Default for PluginSourceInfo {
    fn default() -> Self {
        Self {
            kind: PluginSourceKind::Local,
            owner: None,
            repo: None,
            subdir: None,
            requested_ref: None,
            resolved_commit: None,
            managed_path: None,
            installed_unix_ms: None,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginSourceKind {
    #[default]
    Local,
    Github,
}

pub(crate) fn plugin_managed_path_component(value: &str) -> String {
    let slug = readable_plugin_path_slug(value);
    let hash = short_plugin_id_hash_for_path_component(value);
    format!("{slug}-{hash}")
}

fn readable_plugin_path_slug(value: &str) -> String {
    let mut slug = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug
        .trim_matches(|ch| matches!(ch, '-' | '_' | '.'))
        .to_string();
    let slug = if slug.is_empty() {
        "plugin".to_string()
    } else {
        slug.chars().take(80).collect()
    };
    if has_windows_reserved_stem_for_path_component(&slug) {
        slug.replace('.', "-")
    } else {
        slug
    }
}

pub(crate) fn short_plugin_id_hash_for_path_component(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(value.as_bytes());
    let mut hash = String::with_capacity(12);
    for byte in &digest[..6] {
        use std::fmt::Write as _;
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}

pub(crate) fn has_windows_reserved_stem_for_path_component(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_managed_path_component_is_windows_safe_and_collision_free() {
        let dotdot = plugin_managed_path_component("..");
        assert_ne!(dotdot, ".");
        assert_ne!(dotdot, "..");
        assert!(dotdot.starts_with("plugin-"));
        assert_ne!(
            plugin_managed_path_component("a:b"),
            plugin_managed_path_component("a_b")
        );
        assert_ne!(plugin_managed_path_component("con"), "con");
        assert!(!plugin_managed_path_component("example.").ends_with('.'));
        assert!(plugin_managed_path_component("con.example").starts_with("con-example-"));
        assert!(plugin_managed_path_component("aux.plugin").starts_with("aux-plugin-"));
        assert!(plugin_managed_path_component("nul.x").starts_with("nul-x-"));
        assert!(plugin_managed_path_component("com1.tool").starts_with("com1-tool-"));
    }

    #[test]
    fn plugin_managed_path_component_keeps_readable_slug() {
        let component = plugin_managed_path_component("example.worktree-bootstrap");
        assert!(component.starts_with("example.worktree-bootstrap-"));
        assert!(component.len() <= "example.worktree-bootstrap-".len() + 12);
    }

    #[test]
    fn plugin_managed_path_component_hash_distinguishes_same_slug_shape() {
        assert_ne!(
            plugin_managed_path_component("example:a"),
            plugin_managed_path_component("example/a")
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginManifestBuild {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginPlatform>>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginManifestStartup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginPlatform>>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginManifestAction {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<PluginActionContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginPlatform>>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginManifestEventHook {
    pub on: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginPlatform>>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginManifestPane {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginPlatform>>,
    #[serde(default)]
    pub placement: PluginPanePlacement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<PopupSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<PopupSize>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginManifestLinkHandler {
    pub id: String,
    pub title: String,
    pub pattern: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginPlatform>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginManifestUiModule {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginPlatform>>,
    #[serde(default)]
    pub kind: PluginUiModuleKind,
    pub api_version: u32,
    pub module: String,
    #[serde(default)]
    pub hot_reload: PluginHotReloadMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub surfaces: Vec<PluginUiSurface>,
    /// Data-driven placement for this module's `Panel` surface. Only consulted
    /// when the module declares the `panel` surface; ignored otherwise. `None`
    /// falls back to the `PluginPanelPlacement` default (free overlay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PluginPanelPlacement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<PluginUiCapability>,
    /// Optional single path segment (no separators, no `..`) naming a file under
    /// this module's `HERDR_PLUGIN_STATE_DIR`. When declared, the host reads the
    /// file each frame (size-capped, mtime-cached) and injects its contents as
    /// `HERDR_PLUGIN_UI_INPUT_JSON` in the module's frame env (spec §7.1). The
    /// plugin's own process side writes the file; the host never interprets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_input_file: Option<String>,
    /// fork: optional single path segment naming an image file under this
    /// module's plugin directory. When declared, the HOST loads it and paints
    /// it over the module's surface `Rect` (kitty transmit path, or half-block
    /// fallback). Purely host-consumed — it never crosses the wasm guest ABI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_file: Option<String>,
    /// fork: absolute directory roots this module may load images from at
    /// runtime via `pane.graphics.set { source_path }`. Only meaningful together
    /// with the `pane_graphics` capability.
    ///
    /// This is the dynamic generalisation of `image_file` above. `image_file` is
    /// fixed at manifest time, which cannot serve an image whose path is chosen
    /// per frame (a Claude session's `<session>/<N>.png`, say). `source_path`
    /// can — so the roots it may reach have to be declared up front rather than
    /// chosen by the guest, and they are visible in the manifest and in
    /// `plugin list` before anyone enables the plugin.
    ///
    /// Empty (the default) means a plugin-owned `set` may not use `source_path`
    /// at all. This is the whole security story of the field: a wasm guest has
    /// no WASI and no host imports, so it cannot read a file — `source_path` is
    /// the only way it can cause the host to read one on its behalf, and an
    /// undeclared root makes that a refusal rather than a filesystem oracle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graphics_roots: Vec<String>,
    /// fork(+127): ceiling on how many `pane.graphics` layers this plugin may
    /// hold on ONE pane. Omitted means the host default (8); the host clamps any
    /// value to a hard maximum (32).
    ///
    /// Declared rather than implicit because +127 changed layer identity from
    /// `owner` to `(owner, layer_id)`, which is what lets one plugin paint many
    /// images at once — and each retained layer holds its bytes in host memory
    /// for as long as it exists. A declared ceiling is visible in `plugin list`
    /// before anyone enables the plugin and makes the worst-case footprint of
    /// enabling it something you can read off the manifest instead of measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_layers_per_pane: Option<usize>,
    /// fork(wave-2): optional argv naming a command the host executes on each
    /// dashboard tick to PRODUCE this module's snapshot out-of-process, so
    /// collection logic hot-reloads (rebuild the bin) without a host rebuild.
    /// Relative `argv[0]` is resolved against `plugin_root` at exec time. The
    /// host gates and spawns it exactly like a plugin `event`/`action` command
    /// (same executor path); the child receives `HERDR_PLUGIN_STATE_DIR` and
    /// `HERDR_FOCUS_JSON`, and answers on stdout with the sticky snapshot
    /// protocol (`{"outcome":"replace","snapshot":{…}}` | `{"outcome":"keep"}`
    /// | `{"outcome":"clear"}`). Only meaningful together with `ui_input_file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collector: Option<Vec<String>>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiModuleKind {
    #[default]
    Wasm,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginHotReloadMode {
    Off,
    #[default]
    On,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiSurface {
    Header,
    Footer,
    TabBar,
    Sidebar,
    RightPanel,
    Overlay,
    ContextMenu,
    /// The multi-row usage dashboard strip (host `usage_strip_rect`). API v2.
    UsageStrip,
    /// A generic, plugin-placed panel whose rect the host lays out from the
    /// module manifest `placement` spec (region/align/size/min_peer/z) rather
    /// than a hardcoded per-variant match. Adding/moving/resizing a panel is a
    /// pure manifest edit (hot-reload), no host rebuild.
    Panel,
}

/// Data-driven placement for a `Panel` surface (schema mirror of the host's
/// generic panel layout). Every field is manifest DATA, so a plugin controls
/// where it renders with no host/enum change. Defaults reproduce a free overlay
/// with a snug (content-sized) box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PluginPanelPlacement {
    /// Which region the panel occupies / carves from.
    #[serde(default)]
    pub region: PluginPanelRegion,
    /// Where the panel sits along the carve axis of its region. Meaningful for
    /// `bottom_band` (which side of the usage band) and `free` (anchor within
    /// the pane area); edge regions (`left`/`right`/`top`/`bottom`) already
    /// encode their edge.
    #[serde(default)]
    pub align: PluginPanelAlign,
    /// Extent along the carved dimension: `"auto"` (snug to content, default),
    /// a cell count (`"30"`), or a percent of the available span (`"30%"`).
    /// Parsed host-side; unparseable values fall back to `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// Cells (columns for vertical bands, rows for horizontal bands) the peer
    /// region keeps: the panel is only carved when the peer keeps at least this
    /// much, so a secondary panel never squeezes the primary content unreadable.
    #[serde(default = "default_panel_min_peer")]
    pub min_peer: u16,
    /// Draw/hit z-order among panels and other layers (higher is on top).
    #[serde(default)]
    pub z: i16,
}

fn default_panel_min_peer() -> u16 {
    24
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginPanelRegion {
    /// Share the usage-strip band: carve a slot beside the usage dash (its own
    /// rect + hit-rects). This is what button-dash uses.
    BottomBand,
    /// Carve a column band from the left/right of the pane area (panes shrink).
    Left,
    Right,
    /// Carve a row band from the top/bottom of the pane area (panes shrink).
    Top,
    Bottom,
    /// Overlay the pane area without carving; anchored by `align`.
    #[default]
    Free,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginPanelAlign {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiCapability {
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
    /// `HostEffect::Api` (`pane.send_text`, `agent.send_keys`).
    ApiInput,
    /// fork: `pane.graphics.*` via `HostEffect::Api` — painting an image into a
    /// pane's cell grid. Deliberately NOT folded into `ApiWrite`: drawing over a
    /// pane is a different power from splitting or closing one, it is the only
    /// capability that puts pixels on screen outside the plugin's own surface,
    /// and separating it means `plugin list` shows exactly which plugin holds
    /// the right to draw into the user's panes.
    PaneGraphics,
    /// fork: `plugin.enable`/`plugin.disable` via `HostEffect::Api`. Kept out of
    /// `ApiWrite` for the same reason as the two above: a module holding this
    /// can switch OTHER modules off, so it has to be declared and visible in
    /// `plugin list` rather than arriving as a side effect of ordinary writes.
    /// `plugin.link`/`plugin.unlink` remain host-reserved — installing code from
    /// a path is a strictly larger power than flipping an installed plugin's
    /// enabled bit, and nothing in-guest needs it.
    PluginAdmin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PluginActionListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PluginLogListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginActionInvokeParams {
    pub action_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<PluginInvocationContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginCommandLogInfo {
    pub log_id: String,
    pub plugin_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    pub command: Vec<String>,
    pub status: PluginCommandStatus,
    pub started_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginCommandStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginPlatform {
    Linux,
    Macos,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginActionContext {
    Global,
    Workspace,
    Tab,
    Pane,
    Selection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PluginInvocationContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorkspaceWorktreeInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_status: Option<AgentStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clicked_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_handler_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_payload_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginActionInfo {
    pub plugin_id: String,
    pub action_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<PluginActionContext>,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<PluginPlatform>>,
}

impl PluginActionInfo {
    pub fn qualified_id(&self) -> String {
        format!("{}.{}", self.plugin_id, self.action_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginPaneOpenParams {
    pub plugin_id: String,
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PluginPanePlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<PopupSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<PopupSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<SplitDirection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginPanePlacement {
    #[default]
    Overlay,
    Popup,
    Split,
    Tab,
    Zoomed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginPaneFocusParams {
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginPaneCloseParams {
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginPaneInfo {
    pub plugin_id: String,
    pub entrypoint: String,
    pub pane: PaneInfo,
}
