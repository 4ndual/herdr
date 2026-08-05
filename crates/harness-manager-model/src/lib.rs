//! wasm-safe render model shared by the harness-manager host collector (which
//! inspects the focused Claude Code pane's session transcript, settings, and
//! process) and the harness-manager WASM plugin (which renders it). No I/O, no
//! clock, no ratatui — every display string is precomputed host-side, mirroring
//! the historial-dash model contract.

use serde::{Deserialize, Serialize};

/// Live-state color of a row's leading dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RowState {
    /// Enabled and idle (green).
    Idle,
    /// Fired within the last ~2s (red flash so the user sees WHEN it runs).
    Active,
    /// Disabled / deferred / archived (dim).
    Off,
    /// No live-state concept for this row (no dot).
    #[default]
    None,
}

/// One row in a harness-manager section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HarnessRow {
    /// Stable routing key (`"ctx:<n>"`, `"hook:<n>"`, `"mcp:<n>"`, `"plugin:<n>"`,
    /// `"skill:<n>"`) the host parses back out of a clicked `action_id` to find
    /// the source entry in its collector snapshot.
    pub key: String,
    /// Left-hand display text, already trimmed/truncatable by the renderer.
    pub label: String,
    /// Right-hand metadata text (sizes, counts, timings). May be empty.
    #[serde(default)]
    pub meta: String,
    /// Leading state dot.
    pub state: RowState,
    /// Hover tooltip text; empty hides the tooltip. Newlines split lines.
    #[serde(default)]
    pub tooltip: String,
    /// Whether a left-click opens this entry (host-resolved `harness:open:`).
    #[serde(default)]
    pub openable: bool,
    /// Whether a right-click offers the disable menu (session / profile),
    /// host-resolved via `harness:disable_session:` / `harness:disable_profile:`.
    #[serde(default)]
    pub can_disable: bool,
    /// Whether a right-click offers to turn this entry back ON, host-resolved via
    /// `harness:enable_session:` / `harness:enable_profile:`.
    ///
    /// This is a separate flag rather than `!can_disable` because the two are not
    /// complements: a row can be off and NOT re-enableable (nothing recorded the
    /// original definition), and a row can be on and not disableable. Only the
    /// host knows whether a restorable record exists, so only the host may set it.
    #[serde(default)]
    pub can_enable: bool,
    /// When a toggle on this row takes effect: `"now"` for entries herdr itself
    /// owns (its own plugins), `"next session"` for anything Claude Code reads at
    /// session start — hooks, MCPs, Claude plugins, skills, agents.
    ///
    /// DESIGN §1.1: a toggle that looks instant but is not is a silent failure, so
    /// the renderer states this on the row rather than letting the user infer it.
    /// `None` means the row has no toggle and nothing to say.
    #[serde(default)]
    pub applies_when: Option<String>,
    /// Set by the host when this row's toggle has been written to config but has
    /// NOT yet taken effect (i.e. `applies_when` is not `"now"` and the session
    /// has not restarted). Rendered as pending, never as done.
    #[serde(default)]
    pub toggle_pending: bool,
    /// Extra indentation level (context tree children render indented).
    #[serde(default)]
    pub indent: u8,
    /// Optional group this row belongs to (e.g. a hook event name like
    /// "PreToolUse"). Rows sharing a `group` are emitted contiguously so the UI
    /// can render one group header with the rows nested beneath it. `None` = the
    /// row stands alone. Backward-compatible: absent in old inputs.
    #[serde(default)]
    pub group: Option<String>,
    /// Tokens this row currently occupies in the live context window (loaded at
    /// conversation start). `None` when the row costs nothing now (e.g. a skill
    /// body that only loads on invoke) or when the cost is not determinable.
    #[serde(default)]
    pub tok_loaded: Option<u32>,
    /// Tokens this row would cost if invoked / recalled / fully loaded (e.g. a
    /// skill's SKILL.md body, a memory file recalled on demand). `None` when not
    /// applicable or not determinable.
    #[serde(default)]
    pub tok_full: Option<u32>,
    /// Short provenance for the token numbers — a measured real source, e.g.
    /// "desc 412B", "SKILL.md 8.1KB", "transcript usage", "calibrated est", or
    /// "not determinable". Empty hides it. Never a fabricated number's excuse.
    #[serde(default)]
    pub tok_src: Option<String>,
    /// When this row's context cost is incurred: "start" | "on-invoke" |
    /// "on-recall" | "on-spawn" | "deferred" | "evicted". `None` when it has no
    /// load event.
    #[serde(default)]
    pub load_when: Option<String>,
    /// For on-demand items (skill invoked, memory recalled, file read): a short
    /// human string for WHEN it last loaded this session (transcript timestamp,
    /// `HH:MM`). `None` for start-loaded or never-loaded rows.
    #[serde(default)]
    pub loaded_at: Option<String>,
    /// Config scope the item comes from: "global" (loads in every session —
    /// user-level settings/skills/agents/global CLAUDE.md) vs "session"
    /// (project files, this session's recalled memories / read files). `None`
    /// when scope has no meaning for the row.
    #[serde(default)]
    pub scope: Option<String>,
}

/// A titled section (Context, Session, Hooks, MCPs, Plugins, Skills).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSection {
    pub title: String,
    /// Right-aligned section summary (e.g. totals). May be empty.
    #[serde(default)]
    pub summary: String,
    /// Header aggregate string computed host-side (e.g. "x214 · 8.6s" for hooks,
    /// "12.4KB ~3.4k tok" for context, "1.2GB · 34% cpu" for the process tree).
    /// Distinct from `summary`: `agg` is the dense computed rollup the UI shows
    /// next to the section title; `summary` stays the simple count. Empty when
    /// there is nothing to aggregate. Backward-compatible.
    #[serde(default)]
    pub agg: Option<String>,
    /// Whether the UI should render this section collapsed by default (e.g. a
    /// long low-signal catalog). `false` = expanded. Backward-compatible.
    #[serde(default)]
    pub collapsed_default: bool,
    pub rows: Vec<HarnessRow>,
}

/// One selectable harness profile for the tab-3 profile picker: the plain
/// "daily" `~/.claude`, each `~/.claude-code-router/profiles/<name>`, and each
/// ccp-managed `~/claude-hub/profiles/<name>`. Backward-compatible (absent in
/// old inputs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProfileRef {
    /// Profile name (dir name, or "daily" for `~/.claude`).
    pub name: String,
    /// Display root (e.g. "~/.claude" or "~/claude-hub/profiles/v1-mac").
    #[serde(default)]
    pub root: String,
    /// Whether this is the profile driving the focused session.
    #[serde(default)]
    pub active: bool,
    /// Short origin tag: "daily" | "ccr" | "ccp".
    #[serde(default)]
    pub kind: String,
}

/// A profile's GLOBAL harness view (tabs 2 and 3): what a profile configures
/// profile-wide, independent of any live session — hooks, skills, agents,
/// plugins, and the profile-level context files, with if-loaded costs rather
/// than per-session "loaded now" numbers. No SESSION section, no session-scoped
/// rows. Backward-compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProfileView {
    /// Which profile this view describes.
    pub name: String,
    /// Display root of the profile's config.
    #[serde(default)]
    pub root: String,
    pub sections: Vec<HarnessSection>,
}

/// A catalog resource with provenance and independently reported desired/effective state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResourceCatalogEntry {
    pub kind: String,
    pub id: String,
    #[serde(default)] pub label: String,
    #[serde(default)] pub source: String,
    #[serde(default)] pub version: String,
    #[serde(default)] pub updated: String,
    #[serde(default)] pub license: Option<String>,
    #[serde(default)] pub trust: String,
    #[serde(default)] pub installed: bool,
    #[serde(default)] pub desired: String,
    #[serde(default)] pub effective: String,
    #[serde(default)] pub reason: String,
    #[serde(default)] pub can_change: bool,
}

/// Full render model the host publishes each tick to `harness_input.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HarnessRenderModel {
    /// One-line target identity ("claude · <sid8> · <cwd basename>"). Empty when
    /// no Claude pane has been focused yet (the panel then shows a hint).
    #[serde(default)]
    pub target: String,
    pub sections: Vec<HarnessSection>,
    /// Plugin-owned deterministic Resource Hub catalog.
    #[serde(default)]
    pub catalog: Vec<ResourceCatalogEntry>,
    /// Tab-3 profile picker list (daily + ccr + ccp profiles). Empty in old
    /// inputs; the guest then shows only the session tab.
    #[serde(default)]
    pub profiles: Vec<ProfileRef>,
    /// Tab-2 global view of the ACTIVE profile (always present once a target is
    /// focused; built from the profile driving the session).
    #[serde(default)]
    pub profile_view: Option<ProfileView>,
    /// Tab-3 current selection echoed by the host (defaults to the active
    /// profile). The guest marks this row selected in the picker.
    #[serde(default)]
    pub selected_profile: Option<String>,
    /// Tab-3 selected profile's global view. `None` when the selection is the
    /// active profile (the guest then reuses [`Self::profile_view`]).
    #[serde(default)]
    pub selected_view: Option<ProfileView>,
    /// Unix seconds when the collector built this model.
    ///
    /// Lets the guest say how long ago its data was true. `None` in models
    /// written by a host that predates the field, and the guest must degrade to
    /// an age-less staleness marker rather than assume the epoch — a panel
    /// claiming "stale since 1970" would be worse than one that just says
    /// "stale".
    ///
    /// This alone is NOT enough to compute an age: the guest also needs the
    /// current time, which it cannot get from a model that has stopped
    /// arriving. The host supplies that separately, per frame, as
    /// `HERDR_NOW_UNIX`.
    #[serde(default)]
    pub generated_at: Option<i64>,
}

impl HarnessRenderModel {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_roundtrips_through_json() {
        let model = HarnessRenderModel {
            target: "claude · 0ccc55a3 · herdr".to_string(),
            sections: vec![HarnessSection {
                title: "HOOKS".to_string(),
                summary: "7".to_string(),
                agg: Some("x214 · 8.6s".to_string()),
                collapsed_default: false,
                rows: vec![HarnessRow {
                    key: "hook:0".to_string(),
                    label: "pkg-guard".to_string(),
                    meta: "x41 avg 180ms".to_string(),
                    state: RowState::Idle,
                    tooltip: "runs before every Bash call".to_string(),
                    openable: true,
                    can_disable: true,
                    indent: 0,
                    group: Some("PreToolUse".to_string()),
                    tok_loaded: None,
                    tok_full: None,
                    tok_src: Some("not determinable".to_string()),
                    load_when: None,
                    loaded_at: None,
                    scope: Some("global".to_string()),
                }],
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&model).expect("serialize");
        let back: HarnessRenderModel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(model, back);
    }

    #[test]
    fn default_model_is_empty() {
        assert!(HarnessRenderModel::default().is_empty());
    }
}
