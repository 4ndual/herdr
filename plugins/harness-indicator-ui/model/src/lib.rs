//! wasm-safe data model shared by the harness-indicator collector and guest.
//!
//! Two consumers:
//!   - `harness-indicator-collector` (native) RESOLVES the focused pane's fleet
//!     harness, agent and folders, and writes an [`IndicatorInput`] to the
//!     plugin's state dir as `harness_input.json`;
//!   - `harness-indicator-ui` (wasm guest) READS it back out of the
//!     host-injected `HERDR_PLUGIN_UI_INPUT_JSON` and renders one line.
//!
//! No I/O and no rendering live here, so it compiles for both
//! `wasm32-unknown-unknown` and the host target. The only logic is the
//! classification both sides must agree on — which is exactly the thing that
//! must not be duplicated in two places and allowed to drift.
//!
//! Terminology warning: "harness" in THIS crate means a **fleet harness** — a
//! directory under `$HARNESS_ROOT` (`~/harnesses/<name>`) holding an isolated
//! agent config. herdr uses the same word elsewhere for "which agent CLI"
//! (`harness_host.rs`, `crates/harness-manager-model`). They are unrelated.

use serde::{Deserialize, Serialize};

/// Where the harness attribution came from. Rendered as a suffix marker so a
/// weaker signal never looks like a stronger one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HarnessSource {
    /// The live process's own environment (`CLAUDE_CONFIG_DIR` / `CODEX_HOME` /
    /// `JCODE_HOME`) resolved to a harness config dir. This is proof: it is what
    /// the running runtime is actually reading.
    #[default]
    Env,
    /// The focused session's transcript file was found under a harness root.
    /// Weaker: it says where this conversation's history lives, which is
    /// normally the same answer, but is inferred rather than observed.
    Transcript,
}

/// The fleet harness a pane belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessRef {
    pub name: String,
    /// `runtime` from `harness.json` (absent in the file ⇒ `claude`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// `profile: true` — a pointer at a pre-existing config dir (e.g.
    /// `~/.claude`) rather than a spawnable harness.
    #[serde(default)]
    pub profile: bool,
    #[serde(default)]
    pub source: HarnessSource,
}

/// What the strip should say about the focused pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    /// An agent running inside a fleet harness.
    InHarness,
    /// An agent running on the default roots (`~/.claude`, `~/.codex`, …).
    NoHarness,
    /// A pane with no detected agent.
    Shell,
    /// Nothing focused, or the focused surface is the plugin itself.
    Empty,
}

/// One resolved snapshot of the focused pane. Every field is optional because
/// every field can genuinely be unknown, and a strip that renders "unknown"
/// honestly is worth more than one that guesses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IndicatorInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// Agent kind as herdr detects it: `claude` | `codex` | `pi` | `jcode`.
    /// `None` ⇒ herdr sees no agent in this pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessRef>,
    /// Where the AGENT is working: the runtime process's own cwd, read from
    /// `/proc/<pid>/cwd`. A runtime does not chdir after launch, so this is the
    /// folder it was started in and it does not move when a shell does.
    ///
    /// herdr keeps no launch-cwd record of its own — both `PaneInfo.cwd` and
    /// `PaneInfo.foreground_cwd` are LIVE process cwds (`src/pane.rs`), so the
    /// runtime's own process is the only place the launch folder still exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_cwd: Option<String>,
    /// Where the pane's SHELL is now — herdr's `PaneInfo.cwd`. Differs from
    /// [`Self::agent_cwd`] when the runtime was started in a subshell
    /// (`(cd x && claude)`) or the shell moved after launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_cwd: Option<String>,
    /// True when the focused surface is the plugin itself; the guest keeps the
    /// previous frame rather than blanking (matches usage-dash stickiness).
    #[serde(default)]
    pub self_focus: bool,
    /// A resolution problem worth showing rather than swallowing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `$HOME` as the collector sees it, so the guest — which has no
    /// environment of its own — can shorten paths to `~` without the collector
    /// having to send pre-formatted display strings. Paths stay raw and truthy
    /// in the snapshot; formatting stays in the renderer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// The config dir the harness attribution resolved to. Not rendered on the
    /// strip; it is what a click reveals, and what makes an attribution
    /// auditable rather than something the strip merely asserts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

impl IndicatorInput {
    pub fn state(&self) -> PaneState {
        if self.pane_id.is_none() {
            return PaneState::Empty;
        }
        match (&self.agent, &self.harness) {
            (None, _) => PaneState::Shell,
            (Some(_), Some(_)) => PaneState::InHarness,
            (Some(_), None) => PaneState::NoHarness,
        }
    }

    /// The harness declares one runtime and herdr detected another.
    ///
    /// This is the check the whole plugin earns its place with: a pane holding
    /// `CLAUDE_CONFIG_DIR=~/harnesses/e2e-codex/config` while running `codex`
    /// is reading one harness's config with another's runtime, and today
    /// nothing anywhere says so.
    ///
    /// Only a harness that declares a runtime AND a pane with a detected agent
    /// can disagree; anything unknown is NOT a mismatch, because "I don't know"
    /// must never render as "these disagree".
    pub fn mismatch(&self) -> Option<(String, String)> {
        let harness = self.harness.as_ref()?;
        let declared = harness.runtime.as_ref()?;
        let detected = self.agent.as_ref()?;
        (declared != detected).then(|| (declared.clone(), detected.clone()))
    }

    /// The folder pair to render: `(where the work happens, Some(where the
    /// shell sits))`, the second only when it differs. The agent's folder leads
    /// because it is the one that describes the session; equal paths collapse
    /// to one so the common case stays short.
    pub fn folders(&self) -> (Option<&str>, Option<&str>) {
        let agent = self.agent_cwd.as_deref();
        let shell = self.shell_cwd.as_deref();
        match (agent, shell) {
            (Some(a), Some(s)) if a != s => (Some(a), Some(s)),
            (Some(a), _) => (Some(a), None),
            (None, Some(s)) => (Some(s), None),
            (None, None) => (None, None),
        }
    }
}

/// Replace a leading `$HOME` with `~` for display. Pure string work so both
/// sides can call it; the guest has no `$HOME` of its own, so the home prefix
/// is passed in.
pub fn tilde(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_pane(agent: &str, harness: Option<HarnessRef>) -> IndicatorInput {
        IndicatorInput {
            pane_id: Some("w1:p1".into()),
            agent: Some(agent.into()),
            harness,
            ..Default::default()
        }
    }

    fn harness(name: &str, runtime: Option<&str>) -> HarnessRef {
        HarnessRef {
            name: name.into(),
            runtime: runtime.map(str::to_string),
            profile: false,
            source: HarnessSource::Env,
        }
    }

    #[test]
    fn states_cover_every_pane_shape() {
        assert_eq!(
            agent_pane("claude", Some(harness("e2e-claude", Some("claude")))).state(),
            PaneState::InHarness
        );
        assert_eq!(agent_pane("claude", None).state(), PaneState::NoHarness);
        assert_eq!(
            IndicatorInput {
                pane_id: Some("w1:p1".into()),
                ..Default::default()
            }
            .state(),
            PaneState::Shell
        );
        assert_eq!(IndicatorInput::default().state(), PaneState::Empty);
    }

    #[test]
    fn mismatch_needs_two_known_sides() {
        // Both known and different ⇒ the finding.
        assert_eq!(
            agent_pane("claude", Some(harness("e2e-codex", Some("codex")))).mismatch(),
            Some(("codex".into(), "claude".into()))
        );
        // Agreement is not a mismatch.
        assert!(
            agent_pane("codex", Some(harness("e2e-codex", Some("codex"))))
                .mismatch()
                .is_none()
        );
        // An undeclared runtime is unknown, not disagreement.
        assert!(agent_pane("codex", Some(harness("e2e-codex", None)))
            .mismatch()
            .is_none());
        // A shell pane inside a harness dir cannot disagree with anything.
        assert!(IndicatorInput {
            pane_id: Some("w1:p1".into()),
            harness: Some(harness("e2e-codex", Some("codex"))),
            ..Default::default()
        }
        .mismatch()
        .is_none());
    }

    #[test]
    fn folders_collapse_when_equal_and_split_when_not() {
        let same = IndicatorInput {
            agent_cwd: Some("/a".into()),
            shell_cwd: Some("/a".into()),
            ..Default::default()
        };
        assert_eq!(same.folders(), (Some("/a"), None));
        let moved = IndicatorInput {
            agent_cwd: Some("/a".into()),
            shell_cwd: Some("/b".into()),
            ..Default::default()
        };
        assert_eq!(moved.folders(), (Some("/a"), Some("/b")));
        let only_current = IndicatorInput {
            shell_cwd: Some("/b".into()),
            ..Default::default()
        };
        assert_eq!(only_current.folders(), (Some("/b"), None));
    }

    #[test]
    fn tilde_only_replaces_a_path_boundary() {
        assert_eq!(tilde("/home/x/p", "/home/x"), "~/p");
        assert_eq!(tilde("/home/x", "/home/x"), "~");
        // Not a boundary: /home/xyz must not become ~yz.
        assert_eq!(tilde("/home/xyz", "/home/x"), "/home/xyz");
        assert_eq!(tilde("/other", "/home/x"), "/other");
        assert_eq!(tilde("/home/x/p", ""), "/home/x/p");
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let input = agent_pane("claude", Some(harness("e2e-claude", Some("claude"))));
        let json = serde_json::to_string(&input).expect("serialize");
        let back: IndicatorInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(input, back);
    }

    #[test]
    fn unknown_fields_and_missing_fields_are_tolerated() {
        // Forward compatible: a newer collector may add fields, and an older
        // guest must keep rendering rather than blanking the strip.
        let raw = r#"{"pane_id":"w1:p1","agent":"claude","future_field":42}"#;
        let parsed: IndicatorInput = serde_json::from_str(raw).expect("tolerant parse");
        assert_eq!(parsed.state(), PaneState::NoHarness);
    }
}
