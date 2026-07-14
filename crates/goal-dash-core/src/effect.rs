//! Host-effect contract for the goal core, recovered verbatim from the
//! dissolved `dash-common` crate (purged wave 1). `FocusInput`/`AgentKind`
//! now come from `herdr-ui-plugin-api`; these two types were goal-only.

use herdr_ui_plugin_api::AgentKind;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

/// Returned by the core's `handle()` for the host to execute.
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
