use dash_common::{AgentKind, FocusInput, HeadlessMode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub const API_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSurface {
    Header,
    Footer,
    TabBar,
    Sidebar,
    RightPanel,
    Overlay,
    ContextMenu,
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
    HeadlessActions,
    Toasts,
    OpenPluginPanes,
    PluginActions,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiTextSpan {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiLayer {
    pub surface: UiSurface,
    #[serde(default)]
    pub z_index: i16,
    #[serde(default)]
    pub consumes_mouse: bool,
    #[serde(default)]
    pub nodes: Vec<UiNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostEffect {
    CopyText(String),
    SendPaneKeys {
        lines: Vec<String>,
    },
    SpawnHeadless {
        agent: AgentKind,
        mode: HeadlessMode,
        sid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiUpdate {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<UiLayer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<HostEffect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumed_action_ids: Vec<String>,
}
