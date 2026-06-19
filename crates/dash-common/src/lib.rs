//! Shared contract between the herdr host and the two dashboard cores
//! (`usage-dash-core`, `goal-dash-core`). Defined FIRST so both cores import
//! the same `FocusInput`/`Effect` types — the multi-worker integration contract.
//!
//! herdr fills a [`FocusInput`] from its own `AppState` (the focused pane's
//! session id / cwd / agent kind) and hands it to each core's `refresh()`.
//! A core's `handle()` returns [`Effect`]s the host executes — file writes the
//! core already performed (host just re-renders) and, crucially,
//! [`Effect::SpawnClaude`] so the host runs `claude -p` without blocking the UI.

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
    /// Agent session id (e.g. Claude `--session-id` / `--resume` uuid), if any.
    pub session_id: Option<String>,
    /// Working directory of the focused pane, used for cwd-keyed sources
    /// (Codex/pi rollouts, goal-file project resolution).
    pub cwd: Option<PathBuf>,
    /// Which agent CLI is focused.
    pub agent: AgentKind,
    /// True when the focused pane is the dashboard region/self — host asks the
    /// core to keep its previous snapshot (sticky, no flicker).
    pub is_self_focus: bool,
}

/// A headless Claude oneshot a goal action wants the host to run.
/// Mirrors goal-widget's `--infer` / `--infer-tree` / `--observe` / `--sort`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaudeMode {
    /// Infer the MAIN goal text.
    Infer,
    /// Infer CURRENT + sub-goals (merge).
    InferTree,
    /// Goal Observer: decide sub-goals → proposals.
    Observe,
    /// AI project-organizer: move goal files into the matching project folder.
    Sort,
}

/// Returned by a core's `handle()` for the host to execute. File mutations are
/// performed inside the core (it owns the `.goal.json`/history store); the host
/// only re-renders for those. `SpawnClaude`/`SendPaneKeys` need host machinery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    /// Nothing further; state already updated in-core (host re-renders).
    Refresh,
    /// Run `claude -p` for `sid` in `mode`; host streams the result back to the
    /// core via `apply_claude_result()` so the UI never blocks on the call.
    SpawnClaude { mode: ClaudeMode, sid: String },
    /// Send a key/text sequence to the focused agent's pane (clear+rebrief).
    SendPaneKeys { lines: Vec<String> },
    /// Surface a transient status line in the panel.
    Status(String),
}
