//! Shared contract between the herdr host and the two dashboard cores
//! (`usage-dash-core`, `goal-dash-core`). Defined FIRST so both cores import
//! the same `FocusInput`/`Effect` types — the multi-worker integration contract.
//!
//! herdr fills a [`FocusInput`] from its own `AppState` (the focused pane's
//! session identity / cwd / agent kind) and hands it to each core's `refresh()`.
//! A core's `handle()` returns [`Effect`]s the host executes.

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
    /// True when the focused pane is the dashboard region/self — host asks the
    /// core to keep its previous snapshot (sticky, no flicker).
    pub is_self_focus: bool,
}

/// A headless goal action the host should run on the focused agent backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeadlessMode {
    /// Infer the MAIN goal text.
    Infer,
    /// Infer CURRENT + sub-goals (merge).
    InferTree,
    /// Goal Observer: decide sub-goals → proposals.
    Observe,
    /// AI project-organizer: move goal files into the matching project folder.
    Sort,
}

/// Returned by a core's `handle()` for the host to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    /// Nothing further; state already updated in-core (host re-renders).
    Refresh,
    /// Run the matching headless action on the focused backend without blocking
    /// the UI. `sid` stays the canonical goal/transcript lookup key; `session_ref`
    /// carries the raw backend-native resume token/path when required.
    SpawnHeadless {
        agent: AgentKind,
        mode: HeadlessMode,
        sid: String,
        session_ref: Option<String>,
        cwd: Option<PathBuf>,
    },
    /// Send a key/text sequence to the focused agent's pane (clear+rebrief).
    SendPaneKeys { lines: Vec<String> },
    /// Copy text through Herdr's host clipboard path.
    CopyText(String),
    /// Surface a transient status line in the panel.
    Status(String),
}
