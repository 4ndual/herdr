//! wasm-safe data model for the version dashboard.
//!
//! Two consumers share this crate:
//!   - the out-of-process `version-dash-collector` binary FILLS a
//!     [`VersionRenderInput`] (one [`InstanceStatus`] per herdr instance it
//!     tracks), serializes it, and writes it to the plugin's state dir
//!     (`version_input.json`);
//!   - the `version-dash-ui` guest wasm module READS the same struct back from
//!     the host-injected `HERDR_PLUGIN_UI_INPUT_JSON` and renders it.
//!
//! It contains NO I/O and NO rendering so it compiles cleanly to
//! `wasm32-unknown-unknown` for the guest and to a native binary for the
//! collector. The only logic here is version parsing / comparison / state
//! classification, which both sides need and which is unit-tested below.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// Per-instance freshness/health, deciding the strip color.
///
/// v1 caveat: `UpToDate`/`Behind` are decided by comparing the instance's
/// VERSION STRING against the latest stable release. They do NOT distinguish
/// which fork BUILD an instance runs (both instances share a version string
/// until a bump), because the `+N` change count is git-source-derived, not
/// baked per-binary. Per-build freshness coloring is a later host-code follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    /// At or ahead of the latest stable release (green).
    UpToDate,
    /// Behind the latest stable release — an update exists (yellow).
    Behind,
    /// Instance is broken: server socket dead or binary won't report a version
    /// (red). Only ever set for herdrtest — see the collector's criterion.
    Broken,
    /// Version could not be resolved and the instance is not known-broken (dim).
    #[default]
    Unknown,
}

/// One tracked herdr instance (hr or herdrtest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InstanceStatus {
    /// Short display label, e.g. `"hr"` / `"test"`.
    pub label: String,
    /// The instance's running version (from its binary `--version`), e.g.
    /// `"0.7.3"`. `None` when unresolved (unknown/broken).
    #[serde(default)]
    pub version: Option<String>,
    /// Count of own-fork commits ahead of the latest upstream stable tag
    /// (git-source-derived; reflects source HEAD, not the exact per-binary
    /// build — see [`InstanceState`]). Rendered as `+N`. `None` when unknown.
    #[serde(default)]
    pub local_changes: Option<u32>,
    /// Freshness/health color state.
    #[serde(default)]
    pub state: InstanceState,
}

impl InstanceStatus {
    /// Classify state from the instance's version vs the latest stable release.
    /// `broken` (herdrtest liveness failure) always wins. A missing/unparseable
    /// version with no broken signal is `Unknown`.
    pub fn classify(version: Option<&str>, latest: Option<&str>, broken: bool) -> InstanceState {
        if broken {
            return InstanceState::Broken;
        }
        match (version, latest) {
            (Some(v), Some(l)) => match compare_versions(v, l) {
                // v >= latest ⇒ up to date; v < latest ⇒ behind.
                Ordering::Less => InstanceState::Behind,
                _ => InstanceState::UpToDate,
            },
            _ => InstanceState::Unknown,
        }
    }
}

/// Everything the strip needs to render, produced by the collector each run.
///
/// The collector rewrites this file on every cadence tick (success OR failure),
/// so `age_label` is recomputed relative to the last SUCCESSFUL fetch and is
/// never more than one cadence interval stale. The guest stays clock-free
/// (wasm has no wall clock), so all time formatting happens collector-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VersionRenderInput {
    /// Tracked instances, in display order (hr first, then herdrtest).
    #[serde(default)]
    pub instances: Vec<InstanceStatus>,
    /// The latest STABLE version from the release channel manifest
    /// (`herdr.dev/latest.json`), e.g. `"0.7.5"`. `None` on a failed fetch with
    /// no cached value.
    #[serde(default)]
    pub latest: Option<String>,
    /// URL to the latest release's notes / changelog (for the click toast).
    #[serde(default)]
    pub notes_url: Option<String>,
    /// Human date/label of the latest release, when the manifest carries one.
    #[serde(default)]
    pub latest_date: Option<String>,
    /// Human "freshness" of `latest` relative to the last successful fetch,
    /// e.g. `"just now"`, `"12m"`, `"3h"`. Empty when never fetched.
    #[serde(default)]
    pub age_label: String,
    /// True when the last successful fetch is older than the freshness budget:
    /// the strip dims + shows the age so a dead collector / offline host is
    /// visible, never a crash.
    #[serde(default)]
    pub stale: bool,
    /// Last error string when the most recent run failed (offline, bad JSON, …).
    /// The previous good values are still carried so the strip shows last-known.
    #[serde(default)]
    pub error: Option<String>,
}

/// Parse a dotted version like `"0.7.3"` or `"0.7.3-preview.4"` into its numeric
/// release components (`[0, 7, 3]`), ignoring any `-suffix`/`+build` metadata.
/// Returns `None` when there is no leading numeric component to compare.
pub fn parse_version(raw: &str) -> Option<Vec<u64>> {
    let core = raw.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or("").trim();
    if core.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for segment in core.split('.') {
        match segment.parse::<u64>() {
            Ok(n) => parts.push(n),
            Err(_) => break,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Compare two version strings by their numeric release components, treating a
/// shorter version as zero-padded (`0.7` == `0.7.0`). Unparseable inputs sort as
/// `Equal` so they never masquerade as an available update.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    match (parse_version(a), parse_version(b)) {
        (Some(a), Some(b)) => {
            let len = a.len().max(b.len());
            for i in 0..len {
                let ai = a.get(i).copied().unwrap_or(0);
                let bi = b.get(i).copied().unwrap_or(0);
                match ai.cmp(&bi) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
            Ordering::Equal
        }
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_decorated_versions() {
        assert_eq!(parse_version("0.7.3"), Some(vec![0, 7, 3]));
        assert_eq!(parse_version("v0.7.3"), Some(vec![0, 7, 3]));
        assert_eq!(parse_version(" 0.7.3 "), Some(vec![0, 7, 3]));
        assert_eq!(parse_version("0.7.3-preview.4"), Some(vec![0, 7, 3]));
        assert_eq!(parse_version("0.7.3+build.9"), Some(vec![0, 7, 3]));
        assert_eq!(parse_version("1.0"), Some(vec![1, 0]));
    }

    #[test]
    fn rejects_non_numeric_versions() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("herdr"), None);
        assert_eq!(parse_version("-"), None);
    }

    #[test]
    fn numeric_ordering_not_lexicographic() {
        assert_eq!(compare_versions("0.7.10", "0.7.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.7.2", "0.7.10"), Ordering::Less);
        assert_eq!(compare_versions("0.8.0", "0.7.99"), Ordering::Greater);
    }

    #[test]
    fn zero_pads_shorter_version() {
        assert_eq!(compare_versions("0.7", "0.7.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.7.1", "0.7"), Ordering::Greater);
    }

    #[test]
    fn unparseable_never_greater() {
        assert_eq!(compare_versions("garbage", "0.7.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.7.0", "garbage"), Ordering::Equal);
    }

    #[test]
    fn classify_broken_wins_over_version() {
        assert_eq!(
            InstanceStatus::classify(Some("0.7.5"), Some("0.7.5"), true),
            InstanceState::Broken
        );
    }

    #[test]
    fn classify_behind_and_uptodate() {
        // Fork 0.7.3 vs stable 0.7.5 ⇒ behind (yellow).
        assert_eq!(
            InstanceStatus::classify(Some("0.7.3"), Some("0.7.5"), false),
            InstanceState::Behind
        );
        // At latest ⇒ up to date (green).
        assert_eq!(
            InstanceStatus::classify(Some("0.7.5"), Some("0.7.5"), false),
            InstanceState::UpToDate
        );
        // Ahead of latest (fork can lead) ⇒ up to date (green).
        assert_eq!(
            InstanceStatus::classify(Some("0.8.0"), Some("0.7.5"), false),
            InstanceState::UpToDate
        );
    }

    #[test]
    fn classify_unknown_without_version() {
        assert_eq!(
            InstanceStatus::classify(None, Some("0.7.5"), false),
            InstanceState::Unknown
        );
        assert_eq!(
            InstanceStatus::classify(Some("0.7.5"), None, false),
            InstanceState::Unknown
        );
    }

    #[test]
    fn round_trips_through_json() {
        let input = VersionRenderInput {
            instances: vec![
                InstanceStatus {
                    label: "hr".into(),
                    version: Some("0.7.3".into()),
                    local_changes: Some(107),
                    state: InstanceState::Behind,
                },
                InstanceStatus {
                    label: "test".into(),
                    version: Some("0.7.5".into()),
                    local_changes: Some(0),
                    state: InstanceState::UpToDate,
                },
            ],
            latest: Some("0.7.5".into()),
            notes_url: Some("https://herdr.dev/changelog".into()),
            latest_date: Some("2026-07-20".into()),
            age_label: "3h".into(),
            stale: false,
            error: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        let back: VersionRenderInput = serde_json::from_str(&json).unwrap();
        assert_eq!(input, back);
    }
}
