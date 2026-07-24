//! wasm-safe data model for the version dashboard.
//!
//! Two consumers share this crate:
//!   - the out-of-process `version-dash-collector` binary FILLS a
//!     [`VersionRenderInput`], serializes it, and writes it to the plugin's
//!     state dir (`version_input.json`);
//!   - the `version-dash-ui` guest wasm module READS the same struct back from
//!     the host-injected `HERDR_PLUGIN_UI_INPUT_JSON` and renders it.
//!
//! It contains NO I/O and NO rendering so it compiles cleanly to
//! `wasm32-unknown-unknown` for the guest and to a native binary for the
//! collector. The only logic here is version parsing / comparison, which both
//! sides need and which is unit-tested below.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// Everything the strip needs to render, produced by the collector each run.
///
/// The collector rewrites this file on every cadence tick (success OR failure),
/// so `age_label` is recomputed relative to the last SUCCESSFUL fetch and is
/// never more than one cadence interval stale. The guest stays clock-free
/// (wasm has no wall clock), so all time formatting happens collector-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VersionRenderInput {
    /// The running fork's version (from `herdr --version`), e.g. `"0.7.3"`.
    /// `None` when the collector could not resolve it.
    #[serde(default)]
    pub current: Option<String>,
    /// The latest STABLE version from the release channel manifest
    /// (`herdr.dev/latest.json`), e.g. `"0.7.5"`. `None` when the last fetch
    /// failed and there is no cached value.
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
    /// True when the last successful fetch is older than the freshness budget
    /// (the collector's cadence times a slack factor): the strip dims + shows
    /// the age so a dead collector / offline host is visible, never a crash.
    #[serde(default)]
    pub stale: bool,
    /// Last error string when the most recent run failed (offline, bad JSON,
    /// …). The previous good `latest`/`notes_url` are still carried so the
    /// strip shows last-known + age instead of going blank.
    #[serde(default)]
    pub error: Option<String>,
}

impl VersionRenderInput {
    /// True when a strictly newer stable release than the running fork exists.
    /// Missing/unparseable versions are never treated as an update (fail quiet).
    pub fn update_available(&self) -> bool {
        match (self.current.as_deref(), self.latest.as_deref()) {
            (Some(current), Some(latest)) => compare_versions(latest, current) == Ordering::Greater,
            _ => false,
        }
    }

    /// True when the running fork is at or ahead of the latest stable release
    /// (the fork can lead upstream). Only meaningful when both versions parse.
    pub fn up_to_date(&self) -> bool {
        match (self.current.as_deref(), self.latest.as_deref()) {
            (Some(current), Some(latest)) => compare_versions(latest, current) != Ordering::Greater,
            _ => false,
        }
    }
}

/// Parse a dotted version like `"0.7.3"` or `"0.7.3-preview.4"` into its numeric
/// release components (`[0, 7, 3]`), ignoring any `-suffix`/`+build` metadata.
/// Returns `None` when there is no leading numeric component to compare.
pub fn parse_version(raw: &str) -> Option<Vec<u64>> {
    // Drop a leading `v`, then everything from the first `-` (pre-release) or
    // `+` (build metadata): we only order the numeric release train.
    let core = raw.trim().trim_start_matches('v');
    let core = core
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .trim();
    if core.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for segment in core.split('.') {
        // A non-numeric segment (e.g. a git hash) ends the comparable prefix.
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
/// shorter version as zero-padded (`0.7` == `0.7.0`). Unparseable inputs sort
/// as "not greater" so they never masquerade as an available update.
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
        // If either side is unparseable we cannot claim one is newer.
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
        // The bug a naive string compare would hit: "0.7.10" < "0.7.9" lexically.
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
    fn update_available_only_when_latest_is_strictly_newer() {
        let update = VersionRenderInput {
            current: Some("0.7.3".into()),
            latest: Some("0.7.5".into()),
            ..Default::default()
        };
        assert!(update.update_available());
        assert!(!update.up_to_date());

        // Fork ahead of upstream stable (real case: fork 0.7.3 vs stable 0.7.2).
        let ahead = VersionRenderInput {
            current: Some("0.7.3".into()),
            latest: Some("0.7.2".into()),
            ..Default::default()
        };
        assert!(!ahead.update_available());
        assert!(ahead.up_to_date());

        // Equal.
        let equal = VersionRenderInput {
            current: Some("0.7.3".into()),
            latest: Some("0.7.3".into()),
            ..Default::default()
        };
        assert!(!equal.update_available());
        assert!(equal.up_to_date());
    }

    #[test]
    fn missing_versions_are_not_an_update() {
        let no_latest = VersionRenderInput {
            current: Some("0.7.3".into()),
            latest: None,
            ..Default::default()
        };
        assert!(!no_latest.update_available());
        assert!(!no_latest.up_to_date());
    }

    #[test]
    fn round_trips_through_json() {
        let input = VersionRenderInput {
            current: Some("0.7.3".into()),
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
