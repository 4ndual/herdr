//! Out-of-process version-dash collector.
//!
//! Run on a cadence (a 10-minute LaunchAgent — see ../scripts/), it:
//!   1. fetches the release-channel manifest (`herdr.dev/latest.json`) with
//!      `curl`, the same URL the in-app updater uses, to learn the latest stable
//!      version + release-notes URL;
//!   2. resolves the RUNNING version of each tracked instance via its own binary
//!      `--version` — `hr` (the daily driver) and `test` (herdrtest);
//!   3. counts own-fork commits ahead of the latest stable tag (git-source
//!      approximation — see the model's `InstanceState` caveat);
//!   4. health-checks herdrtest (socket connectable + binary answers);
//!   5. classifies each instance and writes `version_input.json` into the
//!      version-dash plugin's state dir (atomically), where the host's generic
//!      `ui_input_file` reader injects it into the guest each frame.
//!
//! Every run rewrites the file. On a failed manifest fetch it carries the
//! last-known `latest` forward and marks the snapshot stale with the age of the
//! last success, so the strip shows "last-known + age" and never blanks/blocks.
//!
//! All the network / process / filesystem lives here; the shared
//! `version-dash-model` crate stays wasm-safe for the guest.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use version_dash_model::{InstanceStatus, VersionRenderInput};

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
/// age survives across runs without polluting the render model.
const LAST_FETCH_EXT: &str = "last-fetch";

/// Curl wall-clock budget. A short timeout keeps a hung network from stacking
/// collector runs; on timeout we fall back to the cached snapshot.
const CURL_MAX_SECS: &str = "8";
/// Per-binary `--version` budget so a wedged binary can't stall the run.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Socket connect budget for the herdrtest liveness probe.
const SOCK_TIMEOUT: Duration = Duration::from_millis(750);
/// Freshness budget (seconds): older than this ⇒ mark stale. Defaults to 3× the
/// 10-minute cadence. Overridable with `HERDR_VERSION_STALE_SECS`.
const DEFAULT_STALE_SECS: u64 = 1800;

/// Default fork source checkout used for the local-changes git count.
const DEFAULT_GIT_REPO: &str = "Projects/herdr";
/// Default per-instance binary + socket layout on Andres's machine. Every path is
/// env-overridable so the sandbox E2E can point at throwaway clones.
const DEFAULT_HR_BIN: &str = ".herdr-hr/bin/herdr";
const DEFAULT_TEST_BIN: &str = ".local/bin/herdrtest";
/// hr's server socket. Used to resolve hr's LIVE executable dynamically: the
/// running hr server does not necessarily exec the `~/.herdr-hr/bin` snapshot —
/// an old launcher loop can keep it exec'ing `target/release/herdr` directly — so
/// the reported version must come from the actual running process, not an assumed
/// path. Resolve socket → pid → exe.
const DEFAULT_HR_SOCK: &str = ".herdr-hr/config/herdr/sessions/hr/herdr.sock";
// The connectable herdrtest SERVER socket is the top-level `herdr.sock` under the
// instance's config dir (verified live 2026-07-24). The per-session dir only
// holds `herdr-client.sock`, which refuses connections, so it is NOT the liveness
// target despite the session being named `test`.
const DEFAULT_TEST_SOCK: &str = ".herdr-test/config/herdr/herdr.sock";

/// Minimal view of the release manifest we need.
#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    assets: std::collections::HashMap<String, String>,
}

/// One instance's fixed identity/config, resolved from env-with-default.
struct InstanceCfg {
    /// Display label, e.g. `"hr"` / `"test"`.
    label: &'static str,
    /// Binary to run `--version` on.
    bin: PathBuf,
    /// Server socket to liveness-probe. `None` ⇒ not health-checked (hr is never
    /// marked broken — if hr were down you couldn't see the strip at all).
    sock: Option<PathBuf>,
}

fn main() {
    let out_path = output_path();
    if let Some(parent) = out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let last_fetch_path = out_path.with_extension(LAST_FETCH_EXT);

    let previous = read_previous(&out_path);
    let now = unix_now();

    // Latest stable (network). On failure we carry the previous value forward so
    // classification still has a reference point.
    let manifest = fetch_manifest();
    let (latest, notes_url, error) = match &manifest {
        Ok(m) => {
            let _ = write_atomic(&last_fetch_path, now.to_string().as_bytes());
            (Some(m.version.clone()), notes_url_from_manifest(m), None)
        }
        Err(err) => (
            previous.as_ref().and_then(|p| p.latest.clone()),
            previous.as_ref().and_then(|p| p.notes_url.clone()),
            Some(err.clone()),
        ),
    };

    let instances = instance_configs()
        .into_iter()
        .map(|cfg| resolve_instance(&cfg, latest.as_deref()))
        .collect::<Vec<_>>();

    let (age_label, stale) = if manifest.is_ok() {
        ("just now".to_string(), false)
    } else {
        let last_fetch = read_last_fetch(&last_fetch_path);
        let age_secs = last_fetch.map(|t| now.saturating_sub(t));
        (
            age_secs.map(age_label).unwrap_or_default(),
            age_secs.map(|a| a > stale_budget()).unwrap_or(true),
        )
    };

    let input = VersionRenderInput {
        instances,
        latest,
        notes_url,
        latest_date: previous.as_ref().and_then(|p| p.latest_date.clone()),
        age_label,
        stale,
        error,
    };

    match serde_json::to_vec(&input) {
        Ok(bytes) => {
            if let Err(err) = write_atomic(&out_path, &bytes) {
                eprintln!(
                    "version-dash-collector: write {} failed: {err}",
                    out_path.display()
                );
                std::process::exit(1);
            }
        }
        Err(err) => {
            eprintln!("version-dash-collector: encode failed: {err}");
            std::process::exit(1);
        }
    }
}

/// The tracked instances, in display order (hr first, then herdrtest). Every path
/// is env-overridable so a sandbox run can target throwaway clones without
/// touching the live instances.
fn instance_configs() -> Vec<InstanceCfg> {
    vec![
        InstanceCfg {
            label: "hr",
            bin: resolve_hr_bin(),
            // hr is never health-checked / never marked broken.
            sock: None,
        },
        InstanceCfg {
            label: "test",
            bin: env_path("HERDR_VERSION_TEST_BIN", DEFAULT_TEST_BIN),
            sock: Some(env_path("HERDR_VERSION_TEST_SOCK", DEFAULT_TEST_SOCK)),
        },
    ]
}

/// Resolve hr's binary for the version read. Precedence:
///   1. `HERDR_VERSION_HR_BIN` override (sandbox / explicit);
///   2. the LIVE hr server's actual executable (socket → pid → exe) — the running
///      server may exec `target/release/herdr` directly rather than the
///      `~/.herdr-hr/bin` snapshot, so the reported version must come from the
///      real process, never an assumed path;
///   3. the snapshot default.
fn resolve_hr_bin() -> PathBuf {
    if let Some(explicit) = env_nonempty("HERDR_VERSION_HR_BIN") {
        return PathBuf::from(explicit);
    }
    let sock = env_path("HERDR_VERSION_HR_SOCK", DEFAULT_HR_SOCK);
    if let Some(exe) = live_exe_for_socket(&sock) {
        return exe;
    }
    env_path("HERDR_VERSION_HR_BIN", DEFAULT_HR_BIN)
}

/// Resolve the executable of the process holding `sock` open: `lsof -t <sock>` →
/// first pid → its exe path (`ps -p <pid> -o comm=`). Returns `None` when nothing
/// listens or the path doesn't resolve to an existing herdr binary.
fn live_exe_for_socket(sock: &Path) -> Option<PathBuf> {
    let pids = run_with_timeout(Command::new("lsof").arg("-t").arg(sock), VERSION_TIMEOUT)?;
    if !pids.status.success() {
        return None;
    }
    let pid = String::from_utf8_lossy(&pids.stdout)
        .split_whitespace()
        .next()?
        .to_string();
    let out = run_with_timeout(
        Command::new("ps").args(["-p", &pid, "-o", "comm="]),
        VERSION_TIMEOUT,
    )?;
    if !out.status.success() {
        return None;
    }
    let exe = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    // Only trust an existing herdr binary; otherwise fall back to the default.
    if exe.is_file() && exe.file_name().and_then(|n| n.to_str()) == Some("herdr") {
        Some(exe)
    } else {
        None
    }
}

/// Probe one instance: read its version, run any liveness check, classify.
fn resolve_instance(cfg: &InstanceCfg, latest: Option<&str>) -> InstanceStatus {
    let version = binary_version(&cfg.bin);
    // herdrtest is "broken" when its socket won't connect OR its binary won't
    // report a version. Instances without a socket (hr) are never broken.
    let broken = match &cfg.sock {
        Some(sock) => version.is_none() || !socket_connectable(sock),
        None => false,
    };
    let state = InstanceStatus::classify(version.as_deref(), latest, broken);
    // +N = own-fork commits ahead of the instance's OWN released version tag
    // (`git rev-list --count v<version>..HEAD`), the git-source approximation.
    // Based on the instance version (not the latest stable) because the fork
    // does not carry future upstream tags — the latest stable tag may not exist
    // locally, whereas each instance's version is a real released tag it forked
    // from. Only computed when a version resolved.
    let local_changes = version.as_deref().and_then(git_local_changes);
    InstanceStatus {
        label: cfg.label.to_string(),
        version,
        local_changes,
        state,
    }
}

/// Run `<bin> --version` (prints `herdr X.Y.Z`) with a timeout; return the
/// trailing version token, or `None` if the binary is missing / errors / times
/// out.
fn binary_version(bin: &Path) -> Option<String> {
    let output = run_with_timeout(Command::new(bin).arg("--version"), VERSION_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().last().map(str::to_string)
}

/// Spawn a command with a wall-clock timeout, killing it if it overruns. Returns
/// `None` on spawn failure or timeout.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    use std::process::Stdio;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let start = SystemTime::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed().map(|e| e > timeout).unwrap_or(true) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

/// True when a Unix socket accepts a connection within the probe budget.
#[cfg(unix)]
fn socket_connectable(path: &Path) -> bool {
    use std::os::unix::net::UnixStream;
    // A plain connect is enough for liveness: the server `accept()`s or the OS
    // refuses/times out. We don't speak the protocol — just prove someone listens.
    match UnixStream::connect(path) {
        Ok(stream) => {
            let _ = stream.set_read_timeout(Some(SOCK_TIMEOUT));
            true
        }
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn socket_connectable(_path: &Path) -> bool {
    // No Unix-socket liveness on non-unix; fall back to the binary version check
    // alone (broken == version unresolved).
    true
}

/// Count own-fork commits ahead of `version`'s release tag: `git -C <repo>
/// rev-list --count v<version>..HEAD`. If the tag is missing locally, fetch tags
/// once and retry; if it is still absent, fall back to the ahead-count from the
/// newest tag reachable from HEAD (`git describe`), so a valid but unfetched tag
/// never silently drops the count. Returns `None` when the repo can't resolve.
fn git_local_changes(version: &str) -> Option<u32> {
    let repo = env_path("HERDR_VERSION_GIT_REPO", DEFAULT_GIT_REPO);
    let tag = format!("v{}", version.trim_start_matches('v'));
    let range = format!("{tag}..HEAD");

    if let Some(n) = rev_list_count(&repo, &range) {
        return Some(n);
    }
    // Tag not present locally? Fetch just the tags once, then retry.
    let _ = run_with_timeout(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["fetch", "--tags", "--quiet"]),
        VERSION_TIMEOUT,
    );
    if let Some(n) = rev_list_count(&repo, &range) {
        return Some(n);
    }
    // Fall back to the count since the newest reachable tag.
    describe_ahead_count(&repo)
}

/// Ahead-count from `git describe --tags --long` (e.g. `v0.7.3-107-g2b99356` ⇒
/// 107). The git-source approximation when the exact version tag is unavailable.
fn describe_ahead_count(repo: &Path) -> Option<u32> {
    let output = run_with_timeout(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["describe", "--tags", "--long"]),
        VERSION_TIMEOUT,
    )?;
    if !output.status.success() {
        return None;
    }
    // `<tag>-<ahead>-g<sha>` — the ahead count is the second-to-last dash field.
    let text = String::from_utf8_lossy(&output.stdout);
    let desc = text.trim();
    let mut parts = desc.rsplitn(3, '-');
    let _sha = parts.next()?;
    parts.next()?.parse().ok()
}

fn rev_list_count(repo: &Path, range: &str) -> Option<u32> {
    let output = run_with_timeout(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-list", "--count", range]),
        VERSION_TIMEOUT,
    )?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
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
/// else the platform default. The version-dash LaunchAgent exports the hr
/// instance's `XDG_STATE_HOME`, so the file lands where the hr host reads it.
fn state_dir() -> PathBuf {
    if let Some(xdg) = env_nonempty("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("herdr");
    }
    if let Some(home) = env_nonempty("HOME") {
        return PathBuf::from(home).join(".local/state/herdr");
    }
    std::env::temp_dir().join("herdr")
}

/// Sanitize a plugin id into a path component the SAME way herdr does for the
/// common case (lowercase ascii letters, digits, and `. _ -` pass through).
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

/// Fetch + parse the channel manifest via curl. Returns the parsed manifest or a
/// short error string suitable for the offline toast.
fn fetch_manifest() -> Result<Manifest, String> {
    let url =
        env_nonempty("HERDR_VERSION_MANIFEST_URL").unwrap_or_else(|| match channel().as_str() {
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

/// Which update channel to report. `HERDR_VERSION_CHANNEL` overrides; defaults to
/// stable (Homebrew/Nix and most users are stable-only).
fn channel() -> String {
    env_nonempty("HERDR_VERSION_CHANNEL").unwrap_or_else(|| "stable".to_string())
}

/// Derive the GitHub release-tag page from any asset download URL in the
/// manifest, e.g.
/// `…/releases/download/v0.7.2/herdr-macos-aarch64` → `…/releases/tag/v0.7.2`.
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

/// Resolve a path from `key`, else `HOME/<default>` (defaults are HOME-relative).
fn env_path(key: &str, default_rel: &str) -> PathBuf {
    if let Some(explicit) = env_nonempty(key) {
        return PathBuf::from(explicit);
    }
    let home = env_nonempty("HOME").unwrap_or_else(|| ".".to_string());
    PathBuf::from(home).join(default_rel)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use version_dash_model::InstanceState;

    #[test]
    fn path_component_matches_herdr_for_our_id() {
        assert_eq!(
            path_component("builtin.version-dash"),
            "builtin.version-dash"
        );
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

    #[test]
    fn env_path_defaults_are_home_relative() {
        std::env::set_var("HOME", "/home/x");
        std::env::remove_var("HERDR_VERSION_HR_BIN");
        assert_eq!(
            env_path("HERDR_VERSION_HR_BIN", DEFAULT_HR_BIN),
            PathBuf::from("/home/x/.herdr-hr/bin/herdr")
        );
        std::env::set_var("HERDR_VERSION_HR_BIN", "/opt/herdr");
        assert_eq!(
            env_path("HERDR_VERSION_HR_BIN", DEFAULT_HR_BIN),
            PathBuf::from("/opt/herdr")
        );
        std::env::remove_var("HERDR_VERSION_HR_BIN");
    }

    #[test]
    fn missing_binary_resolves_unknown_not_broken_for_hr() {
        // hr has no socket ⇒ never broken; a missing binary is Unknown.
        let cfg = InstanceCfg {
            label: "hr",
            bin: PathBuf::from("/nonexistent/herdr-bin-xyz"),
            sock: None,
        };
        let status = resolve_instance(&cfg, Some("0.7.5"));
        assert_eq!(status.state, InstanceState::Unknown);
        assert_eq!(status.version, None);
        assert_eq!(status.local_changes, None);
    }

    #[test]
    fn describe_long_parses_ahead_count() {
        // Direct parse check of the `<tag>-<ahead>-g<sha>` shape (no git needed):
        // reuse the same rsplit logic the fn uses.
        let desc = "v0.7.3-107-g2b99356";
        let mut parts = desc.rsplitn(3, '-');
        let _sha = parts.next().unwrap();
        let ahead: u32 = parts.next().unwrap().parse().unwrap();
        assert_eq!(ahead, 107);
    }

    #[test]
    fn missing_binary_with_socket_is_broken() {
        // test has a socket ⇒ a missing binary (or dead socket) is Broken.
        let cfg = InstanceCfg {
            label: "test",
            bin: PathBuf::from("/nonexistent/herdrtest-xyz"),
            sock: Some(PathBuf::from("/nonexistent/herdr.sock")),
        };
        let status = resolve_instance(&cfg, Some("0.7.5"));
        assert_eq!(status.state, InstanceState::Broken);
    }
}
