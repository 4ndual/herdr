//! CLI version freshness: what this pane is running vs what is published.
//!
//! Feeds the clickable version token in the usage caption. Two facts:
//!
//!   * `current` — from the transcript envelope: the version the pane is
//!     genuinely RUNNING, which may be older than what is installed on disk;
//!     that gap is the whole point.
//!   * `latest` — the npm registry's `latest` dist-tag.
//!
//! ## Why this is cached
//!
//! The collector runs once per dashboard tick (~1 Hz). Fetching the registry at
//! that rate would be abusive and would stall every tick on the network, so the
//! `latest` values live in a small JSON cache with a [`FETCH_TTL_SECS`] TTL and
//! only one process-lifetime fetch is attempted past expiry.
//!
//! ## Failure policy
//!
//! A failed fetch NEVER clears a previously known `latest`; the last value is
//! carried forward and its age reported, so the caller can distinguish fresh
//! from stale. A never-successful fetch leaves `latest` as `None`, which
//! [`CliVersion::freshness`] maps to `Unknown` — deliberately not `Outdated`, so
//! a dead network can never paint a version red.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{CliVersion, CliVersions};

/// npm package that publishes the Claude Code CLI.
const CLAUDE_PKG: &str = "@anthropic-ai/claude-code";

/// How long a fetched `latest` stays authoritative. 30 minutes: releases are not
/// more frequent than that, and it keeps the registry hit rate negligible even
/// with several panes.
const FETCH_TTL_SECS: u64 = 30 * 60;

/// Wall-clock budget for a single registry request. Short on purpose — this runs
/// inside a dashboard tick, and a hung network must never stall the strip.
const CURL_MAX_SECS: &str = "4";

/// Cache file name inside the plugin state dir.
const CACHE_FILE: &str = "cli_versions_cache.json";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the cache lives. `HERDR_PLUGIN_STATE_DIR` is set by the host when it
/// runs the collector; the temp-dir fallback keeps tests and manual runs working.
fn cache_path() -> PathBuf {
    std::env::var("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(CACHE_FILE)
}

/// One cached `latest`, with the time it was fetched.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedLatest {
    version: String,
    fetched_at: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct Cache {
    #[serde(default)]
    claude: Option<CachedLatest>,
}

fn read_cache() -> Cache {
    std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_cache(cache: &Cache) {
    let Ok(encoded) = serde_json::to_string(cache) else {
        return;
    };
    let path = cache_path();
    // Same temp-then-rename discipline the rest of the dash uses, so a reader
    // never observes a half-written cache.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, encoded).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Ask the npm registry for a package's `latest` dist-tag.
///
/// Hits the `/latest` endpoint, which returns just that version's manifest
/// (~3 KB) rather than the full packument (megabytes for these packages).
///
/// Deliberately sends NO `Accept` header: the abbreviated-metadata type
/// (`application/vnd.npm.install-v1+json`) is only valid on the packument
/// endpoint, and `/latest` answers **406 Not Acceptable** when it is sent —
/// which fails silently into "latest unknown" if you do not check.
///
/// Returns `None` on any failure — the caller carries the previous value forward.
fn fetch_latest(pkg: &str) -> Option<String> {
    let url = format!("https://registry.npmjs.org/{pkg}/latest");
    let out = Command::new("curl")
        .args(["-fsS", "--max-time", CURL_MAX_SECS, &url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let version = value.get("version")?.as_str()?.trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

/// Resolve a package's `latest`, refreshing the cache entry only when it is
/// missing or older than [`FETCH_TTL_SECS`].
///
/// Returns `(latest, age_secs)`. `age_secs` is the age of the returned value,
/// so a caller can tell a value fetched seconds ago from one carried forward
/// across a long network outage.
fn latest_with_age(
    pkg: &str,
    slot: &mut Option<CachedLatest>,
    force: bool,
) -> (Option<String>, Option<u64>) {
    let now = now_secs();
    let expired = slot
        .as_ref()
        .map(|c| now.saturating_sub(c.fetched_at) >= FETCH_TTL_SECS)
        .unwrap_or(true);

    if force || expired {
        if let Some(version) = fetch_latest(pkg) {
            *slot = Some(CachedLatest {
                version,
                fetched_at: now,
            });
        }
        // On failure: fall through and keep whatever was cached. The age below
        // then reports how stale it is rather than pretending it is fresh.
    }

    match slot {
        Some(c) => (
            Some(c.version.clone()),
            Some(now.saturating_sub(c.fetched_at)),
        ),
        None => (None, None),
    }
}

/// Build the version block for a snapshot.
///
/// `running_claude` is the version from the transcript envelope.
///
/// `force` bypasses the TTL — used by the "verify now" click on an
/// already-current token.
pub fn versions_for(running_claude: Option<&str>, force: bool) -> CliVersions {
    let mut cache = read_cache();
    let before = cache.claude.clone();

    let (claude_latest, claude_age) = latest_with_age(CLAUDE_PKG, &mut cache.claude, force);
    let claude = CliVersion {
        current: running_claude.map(|v| v.to_string()),
        latest: claude_latest,
        checked_age_secs: claude_age,
    };

    // Only rewrite the cache when something actually changed — this runs once
    // per tick and the dash already had a disk-write problem.
    let changed = before.as_ref().map(|c| (&c.version, c.fetched_at))
        != cache.claude.as_ref().map(|c| (&c.version, c.fetched_at));
    if changed {
        write_cache(&cache);
    }

    CliVersions { claude }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Freshness;

    /// Each test gets its own state dir so the cache file cannot leak between
    /// them (nextest runs each test in its own process, so setting env is safe).
    fn temp_state_dir(tag: &str) -> PathBuf {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("herdr-cliver-{tag}-{uniq}"));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);
        dir
    }

    #[test]
    fn a_fresh_cache_entry_is_used_without_fetching() {
        let dir = temp_state_dir("fresh");
        // Deliberately implausible version: if this comes back, it came from the
        // cache and no network call replaced it.
        let mut slot = Some(CachedLatest {
            version: "9.9.9".to_string(),
            fetched_at: now_secs(),
        });
        let (latest, age) = latest_with_age(CLAUDE_PKG, &mut slot, false);
        assert_eq!(latest.as_deref(), Some("9.9.9"));
        assert!(age.unwrap_or(u64::MAX) < 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The failure policy, stated as a test: an expired entry whose refetch
    /// fails must be carried forward with its true age, not dropped.
    #[test]
    fn an_expired_entry_survives_a_failed_refetch() {
        let dir = temp_state_dir("expired");
        let old = now_secs().saturating_sub(FETCH_TTL_SECS * 4);
        let mut slot = Some(CachedLatest {
            version: "1.2.3".to_string(),
            fetched_at: old,
        });
        // An unresolvable package name makes the fetch fail without depending on
        // the network being down.
        let (latest, age) = latest_with_age(
            "@herdr-test/definitely-not-a-real-package-9f3c1",
            &mut slot,
            false,
        );
        assert_eq!(
            latest.as_deref(),
            Some("1.2.3"),
            "a failed refetch must not discard the last known value"
        );
        assert!(
            age.unwrap_or(0) >= FETCH_TTL_SECS,
            "the carried-forward value must report its real age, not look fresh"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_never_fetched_entry_is_unknown_not_outdated() {
        let dir = temp_state_dir("never");
        let mut slot: Option<CachedLatest> = None;
        let (latest, age) = latest_with_age(
            "@herdr-test/definitely-not-a-real-package-9f3c1",
            &mut slot,
            false,
        );
        assert_eq!(latest, None);
        assert_eq!(age, None);
        let v = CliVersion {
            current: Some("2.1.220".to_string()),
            latest,
            checked_age_secs: age,
        };
        assert_eq!(v.freshness(), Freshness::Unknown);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cached_latest_is_used_without_a_network_call() {
        let dir = temp_state_dir("cached");
        // Seed a fresh claude entry so this test performs no network call.
        write_cache(&Cache {
            claude: Some(CachedLatest {
                version: "2.1.220".to_string(),
                fetched_at: now_secs(),
            }),
        });
        let v = versions_for(Some("2.1.220"), false);
        assert_eq!(v.claude.latest.as_deref(), Some("2.1.220"));
        assert_eq!(v.claude.freshness(), Freshness::Current);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_running_version_drives_freshness_not_the_installed_one() {
        let dir = temp_state_dir("running");
        write_cache(&Cache {
            claude: Some(CachedLatest {
                version: "2.1.220".to_string(),
                fetched_at: now_secs(),
            }),
        });
        // A pane still running an older build is outdated even though the
        // machine may well have 2.1.220 installed.
        let v = versions_for(Some("2.1.219"), false);
        assert_eq!(v.claude.current.as_deref(), Some("2.1.219"));
        assert_eq!(v.claude.freshness(), Freshness::Outdated);
        let _ = std::fs::remove_dir_all(dir);
    }
}
