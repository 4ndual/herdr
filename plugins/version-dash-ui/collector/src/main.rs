//! Out-of-process version-dash collector.
//!
//! Run on a cadence (every 10 minutes, from a launchd agent on macOS or a
//! systemd user timer on Linux — see ../scripts/), it:
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
use version_dash_model::{
    InstanceState, InstanceStatus, PipelinePhase, PipelineStage, PipelineStatus, StagedBuild,
    Verification, VersionRenderInput,
};

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

/// Offline patch-notes cache for the latest release — what a click on the
/// `latest` token opens.
const NOTES_FILE: &str = "latest_notes.md";

/// Every release's notes, newest first.
const ARCHIVE_FILE: &str = "releases_notes.md";

/// Curl wall-clock budget. A short timeout keeps a hung network from stacking
/// collector runs; on timeout we fall back to the cached snapshot. It must stay
/// well under the host's 5s `COLLECTOR_TIMEOUT` (`src/plugin_collector.rs`),
/// which kills the run outright — two fetches (stable + preview) plus the
/// per-instance `--version` probes have to fit inside that budget.
const CURL_MAX_SECS: &str = "2";
/// Per-binary `--version` budget so a wedged binary can't stall the run.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Socket connect budget for the herdrtest liveness probe.
const SOCK_TIMEOUT: Duration = Duration::from_millis(750);
/// Freshness budget (seconds): older than this ⇒ mark stale. Defaults to 3× the
/// 10-minute cadence. Overridable with `HERDR_VERSION_STALE_SECS`.
const DEFAULT_STALE_SECS: u64 = 1800;

/// Default fork source checkout used for the local-changes git count, relative
/// to `$HOME`. Point `HERDR_VERSION_GIT_REPO` at the real checkout on machines
/// that keep it elsewhere; when neither resolves to a git repo the count is
/// simply absent, never fatal.
const DEFAULT_GIT_REPO: &str = "Projects/herdr";
/// Default per-instance binary + socket layout. Every path is env-overridable so
/// the sandbox E2E can point at throwaway clones.
const DEFAULT_HR_BIN: &str = ".herdr-hr/bin/herdr";
/// XDG roots, used to ask each instance's running server what it executes.
const DEFAULT_HR_ROOT: &str = ".herdr-hr";
const DEFAULT_TEST_ROOT: &str = ".herdr-test";
const DEFAULT_TEST_BIN: &str = ".local/bin/herdrtest";
/// hr's server socket. Used to resolve hr's LIVE executable dynamically: the
/// running hr server does not necessarily exec the `~/.herdr-hr/bin` snapshot —
/// an old launcher loop can keep it exec'ing `target/release/herdr` directly — so
/// the reported version must come from the actual running process, not an assumed
/// path. Resolve socket → pid → exe.
const DEFAULT_HR_SOCK: &str = ".herdr-hr/config/herdr/sessions/hr/herdr.sock";
// herdrtest's connectable server socket is resolved at runtime — see
// `resolve_test_sock`.

/// Minimal view of the release manifest we need.
#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    assets: std::collections::HashMap<String, String>,
    /// Release-notes markdown for `version`. The channel manifest carries the
    /// full body, so patch notes never need a second network round trip — and
    /// crucially never need one at CLICK time, which is the only moment the
    /// user is actually waiting.
    #[serde(default)]
    notes: Option<String>,
    /// Per-version notes for every published release. Present in the same
    /// manifest, so the cache written below is a complete offline archive.
    #[serde(default)]
    releases: std::collections::HashMap<String, ManifestRelease>,
}

#[derive(Debug, Clone, Deserialize)]
struct ManifestRelease {
    #[serde(default)]
    notes: Option<String>,
}

/// Notes for `version`, preferring the per-release entry and falling back to the
/// manifest's top-level body (which describes the latest release).
fn manifest_notes(m: &Manifest, version: &str) -> Option<String> {
    m.releases
        .get(version)
        .and_then(|r| r.notes.clone())
        .or_else(|| m.notes.clone())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    /// The instance's XDG root, used to ask its RUNNING server what version it
    /// is actually executing (`herdr status server`). Without this the collector
    /// can only read the binary on disk, which is a statement about a file.
    root: Option<PathBuf>,
    /// The instance's session name. Required alongside `root`: without
    /// HERDR_SESSION the CLI resolves the DEFAULT session socket
    /// (`config/herdr/herdr.sock`), not `config/herdr/sessions/<name>/herdr.sock`,
    /// and reports `status: not running` for an instance that is running fine.
    session: Option<String>,
}

fn main() {
    let out_path = output_path();
    if let Some(parent) = out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let last_fetch_path = out_path.with_extension(LAST_FETCH_EXT);

    let previous = read_previous(&out_path);
    let now = unix_now();

    // Self-throttle. The host's generic collector runner fires on the DASH TICK
    // — measured at roughly once per second — while this collector's real
    // cadence is ten minutes. Without this guard, adopting the host tick would
    // mean two requests to herdr.dev plus `lsof`, `herdr status` and two
    // `--version` probes EVERY SECOND, forever. The throttle lives here rather
    // than in the manifest because it is a property of this collector's cost,
    // not of the host's scheduling.
    //
    // The in-flight update pipeline is the deliberate exception: while
    // pipeline.json / staged-build.json are moving, the strip is a progress
    // indicator and ten-minute-old truth is useless, so a snapshot older than
    // either of those files always refreshes.
    if !refresh_due(&out_path, now) {
        println!("{{\"outcome\":\"keep\"}}");
        return;
    }

    // Latest stable (network). On failure we carry the previous value forward so
    // classification still has a reference point.
    let manifest = fetch_manifest();
    let (latest, notes_url, error) = match &manifest {
        Ok(m) => {
            let _ = write_atomic(&last_fetch_path, now.to_string().as_bytes());
            // Cache the notes next to the input file. Written only on a
            // SUCCESSFUL fetch, so a failed refresh leaves the last good notes
            // in place rather than blanking them — the click must still work
            // offline, which is the whole point of caching them at all.
            if let Some(dir) = out_path.parent() {
                write_notes_cache(dir, m);
            }
            (Some(m.version.clone()), notes_url_from_manifest(m), None)
        }
        Err(err) => (
            previous.as_ref().and_then(|p| p.latest.clone()),
            previous.as_ref().and_then(|p| p.notes_url.clone()),
            Some(err.clone()),
        ),
    };

    let state_dir_for_inputs = out_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let pipeline = read_pipeline(&state_dir_for_inputs, now);
    let staged_build = read_staged_build(&state_dir_for_inputs, now);

    let instances = instance_configs()
        .into_iter()
        .map(|cfg| {
            resolve_instance(
                &cfg,
                latest.as_deref(),
                &state_dir_for_inputs,
                pipeline.as_ref(),
            )
        })
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

    let mut input = VersionRenderInput {
        instances,
        latest,
        notes_url,
        latest_date: previous.as_ref().and_then(|p| p.latest_date.clone()),
        age_label,
        stale,
        error,
        pipeline,
        staged_build,
    };
    downgrade_states_the_guest_cannot_read(&mut input);

    match serde_json::to_vec(&input) {
        Ok(bytes) => {
            if let Err(err) = write_atomic(&out_path, &bytes) {
                eprintln!(
                    "version-dash-collector: write {} failed: {err}",
                    out_path.display()
                );
                std::process::exit(1);
            }
            // Stdout is the host's generic collector protocol
            // (`src/plugin_collector.rs::parse_collector_stdout`). This
            // collector owns its `ui_input_file` outright and has nothing for
            // the host to apply to its own state, so the honest answer is
            // `keep`. Printing nothing would make every tick log a parse
            // failure for a run that actually succeeded.
            println!("{{\"outcome\":\"keep\"}}");
        }
        Err(err) => {
            eprintln!("version-dash-collector: encode failed: {err}");
            std::process::exit(1);
        }
    }
}

/// Never hand the guest a state name it does not know.
///
/// ── MEASURED, NOT ASSUMED ────────────────────────────────────────────────────
/// The collector and the guest `.wasm` are separate artefacts on separate
/// release cycles, and the host loads whichever wasm the plugin registry points
/// at. Feeding a wasm built on 2026-07-24 (vocabulary: `up_to_date`, `behind`,
/// `broken`, `unknown`) a newer variant was tested in a sandbox instance, and the
/// failure mode is the worst available one: `serde_json::from_str` fails, the
/// guest returns early, and the host KEEPS THE LAST GOOD FRAME. The strip does
/// not blank and does not error — it silently freezes on hours-old truth while
/// looking exactly as confident as a fresh one. That is the same class of defect
/// as the green `✓` over a pending restart, arrived at from the other side.
///
/// So capability is detected from the artefact that will actually parse the
/// JSON, and anything it cannot read is mapped DOWN to the nearest state it can,
/// always in the safe direction:
///
///   verified         → up_to_date  (green ✓ — a checked build really is fine)
///   awaiting_restart → broken      (✗ — pending, and must not look calm)
///   unsafe_restart   → broken      (✗ — ungated binary staged; the loudest
///                                   glyph the old vocabulary has)
///   escalated        → broken      (✗ — needs a human)
///   working          → unknown     (? — in flight, no verdict yet)
///
/// Downgrades never invent confidence: the only one that lands on green is
/// `verified`, which is strictly stronger than the `up_to_date` it becomes.
/// `detail` is left untouched, so the precise sentence still reaches the toast.
///
/// This is a bridge, not a destination. Ship the guest and the collector together
/// and it becomes a no-op; it exists so that the ORDER of those two installs can
/// never break the strip.
fn downgrade_states_the_guest_cannot_read(input: &mut VersionRenderInput) {
    let Some(vocabulary) = guest_vocabulary() else {
        return; // Guest not locatable: assume current, change nothing.
    };
    let known = |state: InstanceState| vocabulary.iter().any(|v| v == state.wire_name());
    for inst in &mut input.instances {
        if known(inst.state) {
            continue;
        }
        let fallback = match inst.state {
            InstanceState::Verified => InstanceState::UpToDate,
            InstanceState::Working => InstanceState::Unknown,
            _ => InstanceState::Broken,
        };
        // If even the fallback is unknown to this guest, Unknown is in the
        // original four and is always safe.
        inst.state = if known(fallback) {
            fallback
        } else {
            InstanceState::Unknown
        };
    }
}

/// The state names the guest wasm can actually deserialize, recovered by
/// scanning the module for its serde variant literals. `None` when the wasm
/// cannot be found, in which case the caller must not downgrade anything.
fn guest_vocabulary() -> Option<Vec<String>> {
    const REL_WASM: &str = "target/wasm32-unknown-unknown/release/version_dash_ui.wasm";
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = env_nonempty("VD_GUEST_WASM") {
        candidates.push(PathBuf::from(explicit));
    }
    if let Some(root) = env_nonempty("VD_GUEST_ROOT") {
        candidates.push(PathBuf::from(root).join(REL_WASM));
    }
    // Derived from the collector's OWN location, so the common case needs no
    // configuration at all. The manifest declares the collector as
    // `../../target/release/version-dash-collector` relative to the plugin root,
    // so a collector at `<tree>/target/release/…` belongs to the plugin at
    // `<tree>/plugins/version-dash-ui`. This must work unconfigured: the host
    // supplies no env of its own, and a collector that cannot find the guest
    // silently disables the downgrade — which re-arms the exact freeze it exists
    // to prevent, with no visible symptom.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(tree) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            candidates.push(tree.join("plugins/version-dash-ui").join(REL_WASM));
        }
    }
    // The host runs plugin commands with the plugin root as CWD.
    candidates.push(PathBuf::from(REL_WASM));
    let wasm = candidates.into_iter().find(|p| p.exists())?;
    // Read the module and scan it here rather than shelling out to `strings`:
    // that is binutils, not guaranteed present, and BSD and GNU differ on
    // defaults — a missing tool made the wasm version silently unknown. The
    // markers are ASCII, so a lossy decode of the whole file finds them.
    let bytes = std::fs::read(&wasm).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let found: Vec<String> = InstanceState::ALL
        .iter()
        .filter(|state| text.contains(state.wire_name()))
        .map(|state| state.wire_name().to_string())
        .collect();
    // A wasm that does not even contain the original four is not a version-dash
    // guest — refuse to draw conclusions from it.
    if found.len() < 4 {
        return None;
    }
    Some(found)
}

/// The tracked instances, in display order (hr first, then herdrtest). Every path
/// is env-overridable so a sandbox run can target throwaway clones without
/// touching the live instances.
fn instance_configs() -> Vec<InstanceCfg> {
    // The session name drives BOTH the socket path and the `HERDR_SESSION` the
    // status probe runs with, so it is resolved once. Default `herdrtest`: that
    // is the session this machine's throwaway instance actually runs under.
    let test_session =
        env_nonempty("HERDR_VERSION_TEST_SESSION").unwrap_or_else(|| "herdrtest".into());
    vec![
        InstanceCfg {
            label: "hr",
            bin: resolve_hr_bin(),
            // hr is never health-checked / never marked broken.
            sock: None,
            root: Some(env_path("HERDR_VERSION_HR_ROOT", DEFAULT_HR_ROOT)),
            session: Some(env_nonempty("HERDR_VERSION_HR_SESSION").unwrap_or_else(|| "hr".into())),
        },
        InstanceCfg {
            label: "test",
            bin: env_path("HERDR_VERSION_TEST_BIN", DEFAULT_TEST_BIN),
            sock: Some(resolve_test_sock(&test_session)),
            root: Some(env_path("HERDR_VERSION_TEST_ROOT", DEFAULT_TEST_ROOT)),
            session: Some(test_session),
        },
    ]
}

/// Whether this run should do real work, or leave the existing snapshot alone.
///
/// `false` when the snapshot is younger than the refresh interval AND no
/// in-flight update file is newer than it. A missing or unreadable snapshot
/// always refreshes — the first run must produce one.
fn refresh_due(out_path: &Path, now: u64) -> bool {
    let Some(age) = file_age_secs(out_path, now) else {
        return true;
    };
    if age >= refresh_interval() {
        return true;
    }
    // Progress files move faster than the interval; if one is newer than the
    // snapshot, the snapshot is behind the thing it is reporting.
    let Some(dir) = out_path.parent() else {
        return false;
    };
    ["pipeline.json", "staged-build.json"]
        .iter()
        .filter_map(|name| file_age_secs(&dir.join(name), now))
        .any(|progress_age| progress_age < age)
}

/// Seconds since `path` was last written, or `None` when it does not exist.
fn file_age_secs(path: &Path, now: u64) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now.saturating_sub(secs))
}

/// Minimum seconds between real collector runs. Ten minutes matches the cadence
/// this collector was designed for; `HERDR_VERSION_REFRESH_SECS` overrides it
/// (`0` disables the throttle entirely, which is what the tests and a manual
/// one-shot run want).
fn refresh_interval() -> u64 {
    env_nonempty("HERDR_VERSION_REFRESH_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(600)
}

/// Resolve herdrtest's connectable SERVER socket.
///
/// MEASURED 2026-08-13: the socket lives at
/// `<root>/config/herdr/sessions/<session>/herdr.sock`, and the top-level
/// `<root>/config/herdr/herdr.sock` this collector used to assume DOES NOT
/// EXIST on this layout. That single wrong path made every run classify a
/// perfectly healthy herdrtest as `server not running` — the surface's loudest
/// possible lie about the thing it exists to report.
///
/// Precedence: explicit override → the session socket → the legacy top-level
/// socket. The first candidate that exists wins; if none exists the session
/// socket is returned so the liveness probe fails against the path a human
/// would actually go looking for.
fn resolve_test_sock(session: &str) -> PathBuf {
    if let Some(explicit) = env_nonempty("HERDR_VERSION_TEST_SOCK") {
        return PathBuf::from(explicit);
    }
    let root = env_path("HERDR_VERSION_TEST_ROOT", DEFAULT_TEST_ROOT);
    let herdr_dir = root.join("config").join("herdr");
    let session_sock = herdr_dir.join("sessions").join(session).join("herdr.sock");
    let legacy_sock = herdr_dir.join("herdr.sock");
    for candidate in [&session_sock, &legacy_sock] {
        if candidate.exists() {
            return candidate.clone();
        }
    }
    session_sock
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

/// Resolve the executable of the process holding `sock` open. Returns `None`
/// when nothing listens or the path doesn't resolve to an existing herdr binary.
///
/// Two platform routes, because neither half of the old macOS-shaped
/// implementation worked on Linux:
///
///   * `lsof` is not installed by default on many distros, and its absence was
///     silent — the live exe never resolved, so `update-hr.sh` promotion only
///     ever saw the snapshot.
///   * `ps -o comm=` prints the full executable PATH only on macOS; on Linux it
///     prints the bare name `herdr`, which then fails the "is an existing file"
///     test below. `dash-common.sh:326` and `handoff-herdr.sh:62` already say
///     exactly this and use `readlink /proc/<pid>/exe` — the shell sweep fixed
///     it, this Rust twin was missed.
///
/// On Linux both problems disappear by reading `/proc` directly: no external
/// tool, and `/proc/<pid>/exe` is the real path by construction.
fn live_exe_for_socket(sock: &Path) -> Option<PathBuf> {
    let exe = live_exe_for_socket_impl(sock)?;
    // Only trust an existing herdr binary; otherwise fall back to the default.
    if exe.is_file() && exe.file_name().and_then(|n| n.to_str()) == Some("herdr") {
        Some(exe)
    } else {
        None
    }
}

/// Socket-namespace inode of the LISTENING unix socket bound to `path`, parsed
/// out of `/proc/net/unix`.
///
/// The filesystem inode of the socket file is a different number from the one
/// `/proc/<pid>/fd/*` reports as `socket:[N]`, so the path's `stat` inode cannot
/// be used to find the holder. Selecting on the listening state also fixes what
/// the `lsof -t` version got wrong: that returns every process with the socket
/// open — server AND connected clients, all of them named `herdr` — and the old
/// code just took the first, so the "live" exe was whichever client happened to
/// sort first.
///
/// Line format: `Num RefCount Protocol Flags Type St Inode Path`, where `St` is
/// `01` for a bound, listening socket and `03` for a connected peer.
#[cfg(target_os = "linux")]
fn listening_socket_inode(table: &str, path: &str) -> Option<u64> {
    for line in table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        if fields[7] != path || fields[5] != "01" {
            continue;
        }
        if let Ok(inode) = fields[6].parse::<u64>() {
            return Some(inode);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn live_exe_for_socket_impl(sock: &Path) -> Option<PathBuf> {
    let table = std::fs::read_to_string("/proc/net/unix").ok()?;
    let inode = listening_socket_inode(&table, sock.to_str()?)?;
    let want = format!("socket:[{inode}]");

    for proc in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = proc.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(proc.path().join("fd")) else {
            // Not ours to inspect — every other user's process lands here.
            continue;
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path())
                .map(|target| target.as_os_str() == want.as_str())
                .unwrap_or(false)
            {
                return std::fs::read_link(proc.path().join("exe")).ok();
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn live_exe_for_socket_impl(sock: &Path) -> Option<PathBuf> {
    let pids = run_with_timeout(Command::new("lsof").arg("-t").arg(sock), VERSION_TIMEOUT)?;
    if !pids.status.success() {
        return None;
    }
    let pid = String::from_utf8_lossy(&pids.stdout)
        .split_whitespace()
        .next()?
        .to_string();
    // macOS `ps -o comm=` prints the full executable path.
    let out = run_with_timeout(
        Command::new("ps").args(["-p", &pid, "-o", "comm="]),
        VERSION_TIMEOUT,
    )?;
    if !out.status.success() {
        return None;
    }
    Some(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

/// A verification receipt written by `verify-instance.sh`.
#[derive(Debug, Deserialize)]
struct Receipt {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    checks: Vec<ReceiptCheck>,
}

#[derive(Debug, Deserialize)]
struct ReceiptCheck {
    #[serde(default)]
    name: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    detail: String,
}

/// Read the receipt for `label` and decide whether it still applies.
///
/// The receipt is honoured ONLY while its `sha256` matches the binary currently
/// in place. That binding is what stops a stale blessing from surviving a binary
/// swap: change the binary by any route — this pipeline, a hand `mv`, a stray
/// `cargo` relink into the live path — and the receipt silently stops counting,
/// so the instance falls back to the weak version-string claim instead of
/// carrying forward a "verified" it no longer earns.
///
/// Returns the verdict plus a short detail line for the click toast.
/// The one receipt check that means "everything passed, a human action is
/// outstanding" rather than "something failed". `update-hr.sh` writes it after a
/// successful, fully gated promotion, because the binary is installed but the
/// instance is still executing the old one.
const AWAITING_RESTART_CHECK: &str = "awaiting_user_restart";

fn read_verification(state_dir: &Path, label: &str, bin: &Path) -> (Verification, Option<String>) {
    let path = state_dir.join(format!("verify-{label}.json"));
    let Ok(bytes) = std::fs::read(&path) else {
        return (Verification::None, None);
    };
    let Ok(receipt) = serde_json::from_slice::<Receipt>(&bytes) else {
        return (Verification::None, None);
    };
    let Some(current) = sha256_of(bin) else {
        return (Verification::None, None);
    };
    if receipt.sha256.is_empty() || receipt.sha256 != current {
        // Receipt is for a different build than the one installed.
        return (
            Verification::None,
            Some("binary changed since it was verified".to_string()),
        );
    }
    // A PENDING HUMAN ACTION IS NOT A FAILED CHECK.
    //
    // `update-hr.sh` writes `ok: false` with exactly one failing check —
    // `awaiting_user_restart` — after a promotion in which every gate PASSED.
    // Read naively that is `Verification::Failed`, which classifies as `Broken`,
    // so a correctly gated update renders with the same red ✗ as a binary that
    // failed verification outright. That is the mirror image of the green-tick
    // bug: it spends the user's trust in the loud direction, and a strip that
    // cries wolf on success stops being read at all.
    //
    // The distinction is safety-relevant, not cosmetic: `AwaitingRestart` means
    // "restarting completes a verified update"; `Broken` means "do not". They
    // must not share a glyph.
    let failed: Vec<&ReceiptCheck> = receipt.checks.iter().filter(|c| !c.ok).collect();
    if !receipt.ok && failed.len() == 1 && failed[0].name == AWAITING_RESTART_CHECK {
        return (
            Verification::Passed,
            Some(if failed[0].detail.is_empty() {
                "verified; restart to finish".to_string()
            } else {
                failed[0].detail.clone()
            }),
        );
    }
    if receipt.ok {
        (Verification::Passed, None)
    } else {
        let failed = receipt
            .checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| {
                if c.detail.is_empty() {
                    c.name.clone()
                } else {
                    format!("{}: {}", c.name, c.detail)
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        (
            Verification::Failed,
            Some(if failed.is_empty() {
                "verification failed".to_string()
            } else {
                failed
            }),
        )
    }
}

/// sha256 of a file, via whichever hashing CLI this machine has (no crypto
/// dependency needed for a build-identity check that already trusts the local
/// filesystem).
///
/// `shasum` is a Perl script that ships with macOS but comes from `perl` rather
/// than coreutils on Linux, so a minimal distro or container has `sha256sum` and
/// no `shasum` — and the failure was silent (`None`), leaving the strip unable to
/// tell a staged build from the running one. Both spellings print the digest as
/// the first whitespace-separated field.
fn sha256_of(path: &Path) -> Option<String> {
    let attempts: [(&str, &[&str]); 2] = [("sha256sum", &[]), ("shasum", &["-a", "256"])];
    for (bin, args) in attempts {
        let Some(out) = run_with_timeout(Command::new(bin).args(args).arg(path), VERSION_TIMEOUT)
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        if let Some(digest) = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .map(str::to_string)
        {
            return Some(digest);
        }
    }
    None
}

/// Read the live pipeline status written by the update scripts, recomputing the
/// elapsed clock against the wall clock here (the guest has no clock).
fn read_pipeline(state_dir: &Path, now: u64) -> Option<PipelineStatus> {
    #[derive(Debug, Deserialize)]
    struct RawPipeline {
        #[serde(default)]
        stage: Option<PipelineStage>,
        #[serde(default)]
        phase: PipelinePhase,
        #[serde(default)]
        step: String,
        #[serde(default)]
        started_unix: Option<u64>,
        #[serde(default)]
        updated_unix: Option<u64>,
        #[serde(default)]
        log_path: Option<String>,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        agent_ref: Option<String>,
    }

    let bytes = std::fs::read(state_dir.join("pipeline.json")).ok()?;
    let raw: RawPipeline = serde_json::from_slice(&bytes).ok()?;

    let mut phase = raw.phase;
    let elapsed_secs = raw.started_unix.map(|s| now.saturating_sub(s));

    // Watchdog: a "running" pipeline whose status file has not been touched for
    // longer than the stall budget is not running, it is dead — the action
    // process was killed, the machine slept, or a step wedged. Reporting it as
    // Running forever would leave the strip spinning and both buttons locked,
    // which is exactly the silent-hang failure the Working state exists to
    // prevent. Demote it so the user can act.
    let stall_budget = env_nonempty("HERDR_VERSION_PIPELINE_STALL_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(600u64);
    let mut error = raw.error.clone();
    if phase == PipelinePhase::Running {
        let staleness = raw
            .updated_unix
            .or(raw.started_unix)
            .map(|t| now.saturating_sub(t))
            .unwrap_or(u64::MAX);
        if staleness > stall_budget {
            phase = PipelinePhase::Failed;
            error = Some(format!(
                "no progress for {}; the update process is gone",
                age_label(staleness)
            ));
        }
    }

    Some(PipelineStatus {
        stage: raw.stage,
        phase,
        step: raw.step,
        elapsed_label: elapsed_secs.map(clock_label).unwrap_or_default(),
        log_path: raw.log_path,
        exit_code: raw.exit_code,
        error,
        agent_ref: raw.agent_ref,
    })
}

/// Read the background prebuild's staged-binary descriptor.
fn read_staged_build(state_dir: &Path, now: u64) -> Option<StagedBuild> {
    #[derive(Debug, Deserialize)]
    struct RawStaged {
        path: String,
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        local_changes: Option<u32>,
        #[serde(default)]
        sha256: Option<String>,
        #[serde(default)]
        built_unix: Option<u64>,
        #[serde(default)]
        bridge_ok: bool,
    }
    let bytes = std::fs::read(state_dir.join("staged-build.json")).ok()?;
    let raw: RawStaged = serde_json::from_slice(&bytes).ok()?;
    // A staged binary that has since been deleted is not staged.
    if !Path::new(&raw.path).is_file() {
        return None;
    }
    Some(StagedBuild {
        path: raw.path,
        version: raw.version,
        local_changes: raw.local_changes,
        sha256: raw.sha256,
        age_label: raw
            .built_unix
            .map(|t| age_label(now.saturating_sub(t)))
            .unwrap_or_default(),
        bridge_ok: raw.bridge_ok,
    })
}

/// `M:SS` elapsed clock for a run in progress.
fn clock_label(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Probe one instance: read its version, run any liveness check, classify.
fn resolve_instance(
    cfg: &InstanceCfg,
    latest: Option<&str>,
    state_dir: &Path,
    pipeline: Option<&PipelineStatus>,
) -> InstanceStatus {
    let version = binary_version(&cfg.bin);
    // BROKEN IS A PROPERTY OF THE BINARY, NOT OF A PROCESS.
    //
    // This used to be `version.is_none() || !socket_connectable(sock)`, so
    // `herdrtest` rendered a red `✗` whenever its server was not running — which
    // is its NORMAL state; it is a sandbox that is started to be tested and then
    // stopped. Andres: "i don't care if the server is running or not, i care if
    // the main binary of herdrtest works and was tested and everything is okay."
    // The token was answering the wrong question, and the answer it gave was the
    // one that blocks promotion, so a perfectly good verified build looked
    // failed. Liveness now only reaches `detail`, never the glyph and never the
    // gate. `broken` means the file will not tell us what it is; verification
    // failure is carried separately by `Verification::Failed`, which
    // `classify_running` still maps to Broken — a real failed check, not a
    // stopped process.
    let broken = version.is_none();
    let offline = cfg
        .sock
        .as_ref()
        .map(|sock| !socket_connectable(sock))
        .unwrap_or(false);
    // The sha-bound receipt for THIS instance, and any pipeline phase currently
    // acting on it. Both outrank the version-string comparison — see
    // InstanceStatus::classify_full for the precedence and why.
    let (verification, verify_detail) = read_verification(state_dir, cfg.label, &cfg.bin);
    let own_phase = pipeline.and_then(|p| {
        if p.stage.map(|s| s.label()) == Some(cfg.label) {
            Some(p.phase)
        } else {
            None
        }
    });
    // What the instance is ACTUALLY executing, asked of its server. When this
    // disagrees with the binary on disk, a restart changes the running code, and
    // classify_running decides whether that is a pending update or a hazard.
    let running = running_version(&cfg.bin, cfg.root.as_deref(), cfg.session.as_deref());
    let state = InstanceStatus::classify_running(
        version.as_deref(),
        latest,
        broken,
        own_phase,
        verification,
        running.as_deref(),
    );
    // +N = the local-changes marker Andres reads to know how far the thing in
    // front of him is from the released tag. It is BAKED INTO EACH BINARY and
    // read back out of the file — never computed from a working tree.
    //
    // It used to be `git_local_changes(version)`, which asked `~/Projects/herdr`
    // for a count and stamped the answer on every token. That tree is one fork
    // checkout among many; it knows nothing about the binary being described. It
    // is how an UPSTREAM 0.7.5 — a build containing zero fork commits — came to
    // render `+113`, and how the merged 0.7.5 (really `+114`, built from a
    // different worktree) would have rendered `+113` too. A number that does not
    // come from the binary is decoration.
    //
    // No stamp (upstream builds, or fork builds predating the stamp) yields None,
    // which the guest renders as unknown. Borrowing the tree's number to fill the
    // gap is the exact defect being removed, so there is no fallback here.
    // ...but ONLY when the version being displayed is the one this binary
    // reports. When disk and running disagree the token shows the RUNNING
    // version, and a running process cannot be asked for its provenance — the
    // stamp is a property of the file, and the file is now a different file.
    // Pairing the two produced `hr 0.7.3+114`: the count of a staged 0.7.5
    // welded to the version of the 0.7.3 that is actually executing. That is the
    // borrowed-number bug again, one scope down, so it gets the same answer —
    // no marker rather than a wrong one.
    let disk_is_shown = match (&version, &running) {
        (Some(disk), Some(live)) => disk == live,
        _ => true,
    };
    let local_changes = if disk_is_shown {
        binary_fork_commits(&cfg.bin)
    } else {
        None
    };
    // Toast detail: prefer the live pipeline step, else why verification did not
    // apply / what failed.
    // When disk and running disagree, the token shows what is RUNNING and the
    // detail names what is staged. Showing the disk version would repeat the
    // original lie in a smaller font: the user is on 0.7.3, and a strip that
    // prints 0.7.5 next to his instance is telling him he already restarted.
    let staged_detail = match (&version, &running) {
        (Some(disk), Some(live)) if disk != live => Some(format!(
            "running {live}; {disk} is staged on disk but has NOT been verified \u{2014}              restarting would boot it"
        )),
        _ => None,
    };
    let shown_version = match (&version, &running) {
        (Some(disk), Some(live)) if disk != live => Some(live.clone()),
        _ => version.clone(),
    };
    // Liveness is demoted to a footnote. It is real information — it explains why
    // a click may need to start the instance first — but it is the LAST thing
    // consulted, so it can never overwrite a verification result or a staged
    // hazard, and it never reaches the glyph.
    let offline_detail = if offline {
        Some(match verification {
            Verification::Passed => {
                "binary verified; server not running (normal for a sandbox)".to_string()
            }
            _ => "server not running".to_string(),
        })
    } else {
        None
    };
    let detail = own_phase
        .and_then(|_| pipeline.map(|p| p.step.clone()))
        .filter(|s| !s.is_empty())
        .or(staged_detail)
        .or(verify_detail)
        .or(offline_detail);

    InstanceStatus {
        label: cfg.label.to_string(),
        version: shown_version,
        local_changes,
        state,
        detail,
    }
}

/// Run `<bin> --version` (prints `herdr X.Y.Z`) with a timeout; return the
/// trailing version token, or `None` if the binary is missing / errors / times
/// out.
/// The version the instance's LIVE SERVER reports it is executing.
///
/// This exists because `binary_version` reads a FILE, and on macOS an atomic
/// `mv` swaps the directory entry while the running process keeps the old inode
/// — so the path still resolves, `--version` still answers, and the answer
/// describes a binary nobody is running. Even socket→pid→exe does not help:
/// `ps -o comm=` reports the same PATH after the swap.
///
/// On 2026-07-27 that gap let the strip paint `hr 0.7.5 ✓ up_to_date` while the
/// server was executing 0.7.3, hiding a pending restart into an ungated build.
/// Only the server can answer "what am I running", so ask it.
fn running_version(bin: &Path, root: Option<&Path>, session: Option<&str>) -> Option<String> {
    let root = root?;
    let session = session?;
    let mut cmd = Command::new(bin);
    cmd.arg("status").arg("server");
    // Same env discipline the pipeline scripts use: scrub inherited HERDR_*
    // nesting vars (the collector runs as a child of a herdr instance) and pin
    // the XDG roots, or the CLI resolves the WRONG instance's socket.
    for k in [
        "HERDR_ENV",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_SESSION",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_PANE_ID",
    ] {
        cmd.env_remove(k);
    }
    cmd.env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("HERDR_SESSION", session);
    let output = run_with_timeout(&mut cmd, VERSION_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    // `status: running` / `version: 0.7.3` / `protocol: 17` / ...
    let text = String::from_utf8_lossy(&output.stdout);
    let mut running = false;
    let mut version = None;
    for line in text.lines() {
        let (k, v) = line.split_once(':').map(|(k, v)| (k.trim(), v.trim()))?;
        match k {
            "status" => running = v == "running",
            "version" if !v.is_empty() => version = Some(v.to_string()),
            _ => {}
        }
    }
    // A stopped server is running nothing; returning its last version would
    // manufacture a mismatch that does not exist.
    if running {
        version
    } else {
        None
    }
}

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
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    // THE PIPES MUST BE DRAINED WHILE THE CHILD RUNS, NOT AFTER IT EXITS.
    //
    // This function used to poll `try_wait()` and only call `wait_with_output()`
    // once the child had already exited. That works for a command that prints a
    // few bytes and is why it survived so long — every caller was `--version` or
    // `status server`. It DEADLOCKS for anything that produces more than one
    // pipe buffer (64 KiB on macOS): the child fills the pipe, blocks in
    // `write`, never exits, `try_wait` never reports exit, and the timeout fires
    // and kills it. The caller then sees a timeout and concludes the data is
    // absent — a false negative that looks exactly like a missing feature.
    //
    // Observed as `+N` reading `None` from a binary that demonstrably contained
    // its provenance marker. The bug was not in the parser; the parser never got
    // any input. Reader threads make output size irrelevant.
    let mut stdout_pipe = child.stdout.take()?;
    let mut stderr_pipe = child.stderr.take()?;
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let start = SystemTime::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
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
    };
    // Both pipes are closed by the child's exit, so these joins cannot hang.
    let stdout = stdout_reader.join().ok()?;
    let stderr = stderr_reader.join().ok()?;
    Some(std::process::Output {
        status,
        stdout,
        stderr,
    })
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

/// Read the `+N` local-changes marker OUT OF THE BINARY ITSELF.
///
/// `build.rs` bakes `HERDR_FORK_PROVENANCE=<git describe --tags --long>` as a
/// single self-describing literal precisely so it survives into a file that can
/// be inspected while stopped. That matters: `herdrtest` spends most of its life
/// installed-but-not-running, and asking a dead server for its provenance is not
/// an option. `strings` is the same technique `herdr-bridge-check.sh` already
/// uses to gate these binaries, so the dependency is not a new one.
///
/// `None` means this binary carries no stamp — an upstream build, or a fork
/// build older than the stamp. The caller must render that as unknown. Do not
/// add a working-tree fallback; that is the bug this function exists to remove.
fn binary_fork_commits(bin: &Path) -> Option<u32> {
    // `grep -a -o -m1` rather than `strings`: it returns ONE short line instead
    // of every literal in a 30 MB binary. `run_with_timeout` no longer deadlocks
    // on volume, but there is no reason to move megabytes to read 26 bytes.
    let out = run_with_timeout(
        Command::new("grep")
            .args(["-a", "-o", "-m1", "HERDR_FORK_PROVENANCE=[^;]*;"])
            .arg(bin),
        VERSION_TIMEOUT,
    )?;
    // Exit 1 simply means "no match": an upstream binary, or one predating the
    // stamp. That is a legitimate answer, not an error, and it must yield None.
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // NOT line-anchored, and that is deliberate. The Rust string table packs
    // unrelated literals end to end with no separators, so `strings` emits the
    // marker in the MIDDLE of a multi-kilobyte line — the first version of this
    // parser matched on `^HERDR_FORK_PROVENANCE=` and found nothing in a binary
    // that demonstrably contained it. Scan for the needle anywhere, then stop at
    // the first character that cannot belong to a `git describe` output, since
    // whatever literal was linked next begins there.
    const NEEDLE: &str = "HERDR_FORK_PROVENANCE=";
    let start = text.find(NEEDLE)? + NEEDLE.len();
    // The build bakes a `;` terminator for exactly this reason — see
    // `build_info::FORK_PROVENANCE`. Stopping on "first character that looks
    // wrong" is not sufficient, because every character `git describe` emits is
    // also legal in the identifier that may be linked immediately after it.
    let desc = text[start..].split(';').next()?.trim();
    if desc.is_empty() || desc == "unknown" {
        return None;
    }
    // `<tag>-<ahead>-g<sha>[-dirty]`
    let desc = desc.strip_suffix("-dirty").unwrap_or(desc);
    let mut parts = desc.rsplitn(3, '-');
    let sha = parts.next()?;
    if !sha.starts_with('g') {
        return None; // `--always` degraded to a bare sha: no tag, no count.
    }
    parts.next()?.parse().ok()
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

/// Resolve the file the host reads, in precedence order:
///   1. `HERDR_VERSION_DASH_OUTPUT` — tests and non-standard layouts;
///   2. `HERDR_PLUGIN_STATE_DIR` — set by the host when IT launches this
///      collector (the generic per-tick runner). This is the module's own state
///      dir, so the snapshot always lands in the instance that asked for it, no
///      XDG guessing and no per-instance scheduled job;
///   3. `<state_dir>/plugins/<id>/version_input.json`, replicating herdr's
///      `plugin_state_dir` — the standalone/scheduled-job path.
fn output_path() -> PathBuf {
    if let Some(explicit) = env_nonempty("HERDR_VERSION_DASH_OUTPUT") {
        return PathBuf::from(explicit);
    }
    if let Some(state) = env_nonempty("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(state).join(INPUT_FILE);
    }
    state_dir()
        .join("plugins")
        .join(path_component(PLUGIN_ID))
        .join(INPUT_FILE)
}

/// Replicate herdr's `config::state_dir()`: `$XDG_STATE_HOME/herdr` when set,
/// else the platform default. The version-dash scheduled job exports the hr
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
/// Write the offline patch-notes cache: `latest_notes.md` (what the click
/// opens) plus `releases_notes.md` (every published release, newest first, so
/// the archive survives even when a version disappears from a later manifest).
///
/// Failures are deliberately silent: the notes are a convenience surface, and a
/// collector that exited non-zero over them would take the whole version strip
/// down with it.
fn write_notes_cache(dir: &Path, m: &Manifest) {
    if let Some(notes) = manifest_notes(m, &m.version) {
        let url = notes_url_from_manifest(m).unwrap_or_default();
        let body = format!(
            "# herdr {}\n\n{}{}\n",
            m.version,
            if url.is_empty() {
                String::new()
            } else {
                format!("{url}\n\n")
            },
            notes
        );
        let _ = write_atomic(&dir.join(NOTES_FILE), body.as_bytes());
    }

    let mut versions: Vec<&String> = m.releases.keys().collect();
    versions.sort_by(|a, b| version_key(b).cmp(&version_key(a)));
    let mut archive = String::new();
    for v in versions {
        if let Some(notes) = m.releases.get(v).and_then(|r| r.notes.clone()) {
            archive.push_str(&format!("# herdr {v}\n\n{}\n\n", notes.trim()));
        }
    }
    if !archive.is_empty() {
        let _ = write_atomic(&dir.join(ARCHIVE_FILE), archive.as_bytes());
    }
}

/// Numeric sort key for a `major.minor.patch` string, so `0.7.10` sorts after
/// `0.7.9` instead of before it as a lexical compare would.
fn version_key(v: &str) -> (u32, u32, u32) {
    let mut it = v
        .split('.')
        .map(|p| p.trim_matches(|c: char| !c.is_ascii_digit()))
        .map(|p| p.parse::<u32>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

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

    /// Real `/proc/net/unix` excerpt for one herdr session socket: one listening
    /// entry (`St` 01) and several connected peers (`St` 03) on the same path.
    #[cfg(target_os = "linux")]
    const PROC_NET_UNIX: &str = "\
Num       RefCount Protocol Flags    Type St Inode Path
0000000000000000: 00000003 00000000 00000000 0001 03 162009167 /home/u/.herdr-hr/sessions/hr/herdr.sock
0000000000000000: 00000002 00000000 00010000 0001 01 465226 /home/u/.herdr-hr/sessions/hr/herdr.sock
0000000000000000: 00000003 00000000 00000000 0001 03 481887 /home/u/.herdr-hr/sessions/hr/herdr.sock
0000000000000000: 00000002 00000000 00010000 0001 01 999999 /home/u/.herdr-test/sessions/t/herdr.sock";

    #[cfg(target_os = "linux")]
    #[test]
    fn listening_socket_inode_picks_the_listener_not_a_peer() {
        let inode =
            listening_socket_inode(PROC_NET_UNIX, "/home/u/.herdr-hr/sessions/hr/herdr.sock");
        assert_eq!(inode, Some(465226), "must skip the St=03 connected peers");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn listening_socket_inode_is_scoped_to_the_requested_path() {
        assert_eq!(
            listening_socket_inode(PROC_NET_UNIX, "/home/u/.herdr-test/sessions/t/herdr.sock"),
            Some(999999)
        );
        assert_eq!(listening_socket_inode(PROC_NET_UNIX, "/nope.sock"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn listening_socket_inode_tolerates_a_garbage_table() {
        assert_eq!(listening_socket_inode("", "/x.sock"), None);
        assert_eq!(
            listening_socket_inode("header only\nshort line", "/x.sock"),
            None
        );
    }

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
            notes: None,
            releases: std::collections::HashMap::new(),
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
            notes: None,
            releases: std::collections::HashMap::new(),
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

    // One test owns both output-path env vars: they are process-global, so
    // splitting these assertions across threaded tests would race.
    #[test]
    fn output_path_precedence_is_explicit_then_host_state_dir() {
        std::env::set_var("HERDR_VERSION_DASH_OUTPUT", "/tmp/version_input.json");
        std::env::set_var(
            "HERDR_PLUGIN_STATE_DIR",
            "/tmp/hr/plugins/builtin.version-dash",
        );
        assert_eq!(output_path(), PathBuf::from("/tmp/version_input.json"));
        std::env::remove_var("HERDR_VERSION_DASH_OUTPUT");
        assert_eq!(
            output_path(),
            PathBuf::from("/tmp/hr/plugins/builtin.version-dash").join(INPUT_FILE)
        );
        std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
    }

    #[test]
    fn refresh_is_throttled_unless_the_snapshot_is_stale_or_a_pipeline_moved() {
        let dir = std::env::temp_dir().join(format!("vd-refresh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let snapshot = dir.join(INPUT_FILE);
        std::fs::write(&snapshot, b"{}").expect("write snapshot");

        // No snapshot at all ⇒ always due (the first run must produce one).
        assert!(refresh_due(&dir.join("absent.json"), unix_now()));

        // A snapshot written "now" is inside the default 600s interval.
        let now = unix_now();
        assert!(!refresh_due(&snapshot, now));

        // Same file, evaluated as if 20 minutes had passed.
        assert!(refresh_due(&snapshot, now + 1_200));

        // A pipeline file NEWER than the snapshot beats the throttle: while an
        // update is in flight the strip is a progress indicator.
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        std::fs::write(dir.join("pipeline.json"), b"{}").expect("write pipeline");
        assert!(refresh_due(&snapshot, unix_now()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_sock_falls_back_to_the_session_socket_when_nothing_exists() {
        std::env::remove_var("HERDR_VERSION_TEST_SOCK");
        std::env::set_var("HERDR_VERSION_TEST_ROOT", "/nonexistent/.herdr-test");
        // Neither candidate exists, so the session path is what a human is sent
        // to — never the legacy top-level socket this collector used to assume.
        assert_eq!(
            resolve_test_sock("herdrtest"),
            PathBuf::from("/nonexistent/.herdr-test/config/herdr/sessions/herdrtest/herdr.sock")
        );
        std::env::set_var("HERDR_VERSION_TEST_SOCK", "/tmp/explicit.sock");
        assert_eq!(
            resolve_test_sock("herdrtest"),
            PathBuf::from("/tmp/explicit.sock")
        );
        std::env::remove_var("HERDR_VERSION_TEST_SOCK");
        std::env::remove_var("HERDR_VERSION_TEST_ROOT");
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
    fn missing_binary_is_broken_even_without_socket() {
        // Broken is a property of the binary, not of a process (see
        // resolve_instance): a file that will not report its version is Broken
        // for hr too — liveness only ever reaches `detail`, never the glyph.
        let cfg = InstanceCfg {
            label: "hr",
            bin: PathBuf::from("/nonexistent/herdr-bin-xyz"),
            sock: None,
            root: None,
            session: None,
        };
        let status = resolve_instance(&cfg, Some("0.7.5"), &std::env::temp_dir(), None);
        assert_eq!(status.state, InstanceState::Broken);
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
            root: None,
            session: None,
        };
        let status = resolve_instance(&cfg, Some("0.7.5"), &std::env::temp_dir(), None);
        assert_eq!(status.state, InstanceState::Broken);
    }
}
