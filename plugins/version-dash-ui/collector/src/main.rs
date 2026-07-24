//! Out-of-process version-dash collector.
//!
//! Run on a cadence (a 10-minute LaunchAgent — see ../scripts/), it:
//!   1. resolves the RUNNING fork version via `herdr --version`;
//!   2. fetches the release-channel manifest (`herdr.dev/latest.json`) with
//!      `curl`, the same URL the in-app updater uses;
//!   3. derives the latest stable version + a release-notes URL;
//!   4. writes `version_input.json` into the version-dash plugin's state dir
//!      (atomically), where the host's generic `ui_input_file` reader injects it
//!      into the guest each frame.
//!
//! Every run rewrites the file. On a failed fetch it carries the last-known good
//! values forward and marks the snapshot stale with the age of the last success,
//! so the strip shows "last-known + age" and never blanks or blocks.
//!
//! All the network / process / filesystem lives here; the shared
//! `version-dash-model` crate stays wasm-safe for the guest.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use version_dash_model::VersionRenderInput;

/// Canonical stable release manifest URL. Mirrors
/// `src/update.rs::STABLE_UPDATE_MANIFEST_URL` (verified 2026-07-24); kept in
/// sync so the strip reports exactly what `herdr update` would install.
const STABLE_MANIFEST_URL: &str = "https://herdr.dev/latest.json";
/// Preview channel manifest, mirroring `src/update.rs::PREVIEW_UPDATE_MANIFEST_URL`.
const PREVIEW_MANIFEST_URL: &str = "https://herdr.dev/preview.json";

/// This plugin's manifest id; the state dir path component is derived from it,
/// matching herdr's `plugin_state_dir` (lowercase/digits/`._-` kept verbatim).
const PLUGIN_ID: &str = "builtin.version-dash";
/// The manifest's `ui_input_file` name — the file the host reads each frame.
const INPUT_FILE: &str = "version_input.json";
/// Sidecar extension holding the unix seconds of the last SUCCESSFUL fetch, so
/// age survives across runs without polluting the render model. Derived from the
/// output file's own name so distinct output files never share a sidecar.
const LAST_FETCH_EXT: &str = "last-fetch";

/// Curl wall-clock budget. A short timeout keeps a hung network from stacking
/// collector runs; on timeout we fall back to the cached snapshot.
const CURL_MAX_SECS: &str = "8";
/// Freshness budget (seconds): older than this ⇒ mark stale. Defaults to 3× the
/// 10-minute cadence, so a couple of missed/failed runs are tolerated before the
/// strip dims. Overridable with `HERDR_VERSION_STALE_SECS`.
const DEFAULT_STALE_SECS: u64 = 1800;

/// Minimal view of the release manifest we need.
#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    assets: std::collections::HashMap<String, String>,
}

fn main() {
    let out_path = output_path();
    if let Some(parent) = out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let last_fetch_path = out_path.with_extension(LAST_FETCH_EXT);

    let previous = read_previous(&out_path);
    let now = unix_now();

    let current = current_version();
    let manifest = fetch_manifest();

    let input = match manifest {
        Ok(m) => {
            // Success: refresh everything and stamp the fetch time.
            let _ = write_atomic(&last_fetch_path, now.to_string().as_bytes());
            VersionRenderInput {
                current,
                latest: Some(m.version.clone()),
                notes_url: notes_url_from_manifest(&m),
                latest_date: None,
                age_label: "just now".to_string(),
                stale: false,
                error: None,
            }
        }
        Err(err) => {
            // Failure: carry last-known good forward, compute age + staleness.
            let last_fetch = read_last_fetch(&last_fetch_path);
            let age_secs = last_fetch.map(|t| now.saturating_sub(t));
            VersionRenderInput {
                current,
                latest: previous.as_ref().and_then(|p| p.latest.clone()),
                notes_url: previous.as_ref().and_then(|p| p.notes_url.clone()),
                latest_date: previous.as_ref().and_then(|p| p.latest_date.clone()),
                age_label: age_secs.map(age_label).unwrap_or_default(),
                stale: age_secs.map(|a| a > stale_budget()).unwrap_or(true),
                error: Some(err),
            }
        }
    };

    match serde_json::to_vec(&input) {
        Ok(bytes) => {
            if let Err(err) = write_atomic(&out_path, &bytes) {
                eprintln!("version-dash-collector: write {} failed: {err}", out_path.display());
                std::process::exit(1);
            }
        }
        Err(err) => {
            eprintln!("version-dash-collector: encode failed: {err}");
            std::process::exit(1);
        }
    }
}

/// Resolve the file the host reads. `HERDR_VERSION_DASH_OUTPUT` wins (tests /
/// non-standard layouts); otherwise `<state_dir>/plugins/<id>/version_input.json`
/// replicating herdr's `plugin_state_dir`.
fn output_path() -> PathBuf {
    if let Some(explicit) = env_nonempty("HERDR_VERSION_DASH_OUTPUT") {
        return PathBuf::from(explicit);
    }
    state_dir()
        .join("plugins")
        .join(path_component(PLUGIN_ID))
        .join(INPUT_FILE)
}

/// Replicate herdr's `config::state_dir()`: `$XDG_STATE_HOME/herdr` when set,
/// else the platform default. hr/herdrtest both export `XDG_STATE_HOME`, so a
/// LaunchAgent that inherits the same env lands in the right instance's dir.
fn state_dir() -> PathBuf {
    if let Some(xdg) = env_nonempty("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("herdr");
    }
    if let Some(home) = env_nonempty("HOME") {
        // macOS/Linux herdr default state root.
        return PathBuf::from(home).join(".local/state/herdr");
    }
    std::env::temp_dir().join("herdr")
}

/// Sanitize a plugin id into a path component the SAME way herdr does for the
/// common case (lowercase ascii letters, digits, and `. _ -` pass through).
/// `builtin.version-dash` contains only such bytes, so this is exact for us.
fn path_component(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Resolve the running fork's version by executing the herdr binary with
/// `--version` (prints `herdr X.Y.Z`). `HERDR_VERSION_BIN` overrides the binary
/// (hr points it at its release binary); otherwise `herdr` on PATH.
fn current_version() -> Option<String> {
    let bin = env_nonempty("HERDR_VERSION_BIN").unwrap_or_else(|| "herdr".to_string());
    let output = Command::new(&bin).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Format: "herdr 0.7.3" — take the last whitespace token.
    text.split_whitespace().last().map(str::to_string)
}

/// Fetch + parse the channel manifest via curl. Returns the parsed manifest or a
/// short error string suitable for the offline toast.
fn fetch_manifest() -> Result<Manifest, String> {
    let url = env_nonempty("HERDR_VERSION_MANIFEST_URL").unwrap_or_else(|| match channel().as_str() {
        "preview" => PREVIEW_MANIFEST_URL.to_string(),
        _ => STABLE_MANIFEST_URL.to_string(),
    });

    // `file://` is honored by curl too, which lets the sandbox point at a local
    // fixture without special-casing here.
    let output = Command::new("curl")
        .args(["-fsS", "--max-time", CURL_MAX_SECS, &url])
        .output()
        .map_err(|err| format!("curl spawn failed: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "fetch failed ({}): {}",
            output.status,
            stderr.trim().lines().last().unwrap_or("").trim()
        ));
    }
    serde_json::from_slice::<Manifest>(&output.stdout)
        .map_err(|err| format!("manifest parse failed: {err}"))
}

/// Which update channel to report. `HERDR_VERSION_CHANNEL` overrides; defaults
/// to stable (Homebrew/Nix and most users are stable-only).
fn channel() -> String {
    env_nonempty("HERDR_VERSION_CHANNEL").unwrap_or_else(|| "stable".to_string())
}

/// Derive the GitHub release-tag page from any asset download URL in the
/// manifest, e.g.
/// `…/releases/download/v0.7.2/herdr-macos-aarch64` → `…/releases/tag/v0.7.2`.
/// Returns `None` when no asset URL follows that shape (no guessed fallback).
fn notes_url_from_manifest(m: &Manifest) -> Option<String> {
    let asset = m.assets.values().next()?;
    let (base, rest) = asset.split_once("/releases/download/")?;
    let tag = rest.split('/').next()?;
    if tag.is_empty() {
        return None;
    }
    Some(format!("{base}/releases/tag/{tag}"))
}

fn read_previous(path: &Path) -> Option<VersionRenderInput> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_last_fetch(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn stale_budget() -> u64 {
    env_nonempty("HERDR_VERSION_STALE_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_STALE_SECS)
}

/// Atomic write: temp file in the same dir, then rename, so the host's
/// mtime-cached reader never sees a torn file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("out"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    std::fs::rename(&tmp, path)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compact human age: "just now" / "12m" / "3h" / "2d".
fn age_label(secs: u64) -> String {
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_component_matches_herdr_for_our_id() {
        assert_eq!(path_component("builtin.version-dash"), "builtin.version-dash");
    }

    #[test]
    fn notes_url_derived_from_asset() {
        let mut assets = std::collections::HashMap::new();
        assets.insert(
            "macos-aarch64".to_string(),
            "https://github.com/ogulcancelik/herdr/releases/download/v0.7.2/herdr-macos-aarch64"
                .to_string(),
        );
        let m = Manifest {
            version: "0.7.2".into(),
            assets,
        };
        assert_eq!(
            notes_url_from_manifest(&m).as_deref(),
            Some("https://github.com/ogulcancelik/herdr/releases/tag/v0.7.2")
        );
    }

    #[test]
    fn notes_url_none_when_no_download_asset() {
        let mut assets = std::collections::HashMap::new();
        assets.insert("x".to_string(), "https://example.com/herdr".to_string());
        let m = Manifest {
            version: "0.7.2".into(),
            assets,
        };
        assert_eq!(notes_url_from_manifest(&m), None);
    }

    #[test]
    fn age_label_buckets() {
        assert_eq!(age_label(5), "just now");
        assert_eq!(age_label(120), "2m");
        assert_eq!(age_label(3 * 3600 + 10), "3h");
        assert_eq!(age_label(2 * 86_400 + 5), "2d");
    }

    #[test]
    fn output_path_honors_explicit_override() {
        std::env::set_var("HERDR_VERSION_DASH_OUTPUT", "/tmp/version_input.json");
        assert_eq!(output_path(), PathBuf::from("/tmp/version_input.json"));
        std::env::remove_var("HERDR_VERSION_DASH_OUTPUT");
    }

    #[test]
    fn state_dir_uses_xdg_state_home() {
        std::env::set_var("XDG_STATE_HOME", "/tmp/hr/state");
        assert_eq!(state_dir(), PathBuf::from("/tmp/hr/state/herdr"));
        std::env::remove_var("XDG_STATE_HOME");
    }
}
