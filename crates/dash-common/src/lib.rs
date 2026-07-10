//! Shared focused-session context the herdr host hands to UI-plugin surfaces.
//!
//! herdr fills a [`FocusInput`] from its own `AppState` (the focused pane's
//! session identity / cwd / agent kind) and delivers it to plugins through the
//! WASM UI bridge (`herdr-ui-plugin-api`): the `FocusedSession` capability, the
//! `HERDR_FOCUS_*` frame env, and `UiEvent::FocusChanged`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which agent CLI occupies the focused pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AgentKind {
    Claude,
    Codex,
    Pi,
    /// Focused pane is a plain shell / non-agent (or nothing focused).
    #[default]
    None,
}

/// What herdr knows about the focused pane, fed to a core's `refresh()`.
/// Replaces the old cmux `focus.rs`/`events.rs` discovery entirely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FocusInput {
    /// Agent session id keyed into the goal/usage stores (often a transcript
    /// stem). For path-backed sessions like pi, herdr derives this from the
    /// session file stem so the core can still resolve transcript + goal files.
    pub session_id: Option<String>,
    /// Raw native session reference used to resume or send a headless action on
    /// the focused agent backend. Examples: Claude/Codex UUID, Pi session path.
    pub session_ref: Option<String>,
    /// Working directory of the focused pane, used for cwd-keyed sources.
    pub cwd: Option<PathBuf>,
    /// Which agent CLI is focused.
    pub agent: AgentKind,
    /// True when the focused pane is a plugin surface/self — the host asks the
    /// plugin to keep its previous snapshot (sticky, no flicker).
    pub is_self_focus: bool,
}
