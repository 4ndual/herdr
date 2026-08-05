//! fork(wave-2): generic, module-agnostic out-of-process collector runner for
//! UI-plugin dashboard modules.
//!
//! A UI module may declare a `collector` argv in its manifest (see
//! [`crate::api::schema::PluginManifestUiModule::collector`]). On the dashboard
//! tick the host launches that command to PRODUCE the module's snapshot in a
//! child process, so collection logic hot-reloads by rebuilding the small
//! collector binary — no host rebuild or server restart. The host still owns the
//! `ui_input_file` envelope (view toggles, countdown clock); only the snapshot
//! inside it comes from the collector.
//!
//! This module knows nothing about any specific dashboard: [`launch_if_idle`]
//! takes a plugin id, the module, and the current [`FocusInput`], spawns the
//! command (reusing the same [`crate::plugin_command`] builder the plugin
//! event/action executor uses), enforces one-in-flight-per-module plus a hard
//! timeout, and delivers the parsed [`CollectorOutcome`] back on the app event
//! channel. The consumer (e.g. `usage_host`) decides how to apply it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use herdr_ui_plugin_api::FocusInput;

use crate::api::schema::PluginManifestUiModule;
use crate::app::App;
use crate::events::AppEvent;

/// Hard wall-clock budget for one collector run before the host kills it.
const COLLECTOR_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on bytes read from the collector's stdout (its answer is a single small
/// JSON object; anything larger is a bug and is truncated before parsing).
const COLLECTOR_STDOUT_MAX_BYTES: usize = 256 * 1024;

/// What the collector's stdout told the host to do with the stored snapshot,
/// mirroring `usage_dash_core::RefreshOutcome` sticky semantics. The snapshot is
/// kept as a raw JSON value here so this runner stays module-agnostic; the
/// consumer deserializes it into its own snapshot type.
#[derive(Debug, Clone, PartialEq)]
pub enum CollectorOutcome {
    /// Replace the stored snapshot with this JSON object.
    Replace(serde_json::Value),
    /// Leave the stored snapshot as-is (no flicker).
    Keep,
    /// Clear the stored snapshot (no active session).
    Clear,
}

/// Composite in-flight / last-error key for one module of one plugin. A `\0`
/// separator can never appear in a plugin or module id.
pub fn module_key(plugin_id: &str, module_id: &str) -> String {
    format!("{plugin_id}\u{0}{module_id}")
}

/// Parse the collector's stdout protocol into a [`CollectorOutcome`].
///
/// Protocol (a single JSON object):
///   `{"outcome":"replace","snapshot":{…}}` | `{"outcome":"keep"}` | `{"outcome":"clear"}`
pub fn parse_collector_stdout(stdout: &str) -> Result<CollectorOutcome, String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|err| format!("collector stdout is not valid JSON: {err}"))?;
    match value.get("outcome").and_then(serde_json::Value::as_str) {
        Some("replace") => value
            .get("snapshot")
            .cloned()
            .map(CollectorOutcome::Replace)
            .ok_or_else(|| "collector 'replace' outcome is missing 'snapshot'".to_string()),
        Some("keep") => Ok(CollectorOutcome::Keep),
        Some("clear") => Ok(CollectorOutcome::Clear),
        Some(other) => Err(format!("collector reported unknown outcome '{other}'")),
        None => Err("collector stdout is missing string field 'outcome'".to_string()),
    }
}

/// Resolve a collector `argv[0]` against `plugin_root` when it is relative, so
/// the manifest can point at a workspace-built binary with a plugin-relative
/// path. Absolute programs and bare command names are left untouched.
fn resolve_program(program: &str, plugin_root: &Path) -> String {
    let path = Path::new(program);
    let looks_relative_path =
        (program.contains('/') || program.contains('\\')) && !path.is_absolute();
    if looks_relative_path {
        plugin_root.join(path).to_string_lossy().into_owned()
    } else {
        program.to_string()
    }
}

/// Launch the module's collector for this tick unless one is already in flight
/// for the module. Non-blocking: spawns a worker thread that runs the command
/// with a hard [`COLLECTOR_TIMEOUT`], captures stdout, and delivers an
/// [`AppEvent::CollectorFinished`]. The caller must have already resolved that
/// `module` belongs to `plugin_id` and declares a collector.
pub fn launch_if_idle(
    app: &mut App,
    plugin_id: &str,
    module: &PluginManifestUiModule,
    focus: &FocusInput,
) {
    let Some(argv) = module.collector.as_ref().filter(|argv| !argv.is_empty()) else {
        return;
    };
    let key = module_key(plugin_id, &module.id);
    if !app.collectors_in_flight.insert(key.clone()) {
        // A previous run is still going; skip this tick (1 Hz cadence).
        return;
    }

    let plugin_root = match app.state.installed_plugins.get(plugin_id) {
        Some(plugin) => PathBuf::from(&plugin.plugin_root),
        None => {
            app.collectors_in_flight.remove(&key);
            return;
        }
    };
    let state_dir = crate::plugin_paths::plugin_state_dir(plugin_id);
    let focus_json = match serde_json::to_string(focus) {
        Ok(json) => json,
        Err(err) => {
            app.collectors_in_flight.remove(&key);
            tracing::warn!(error = %err, plugin_id, "failed to serialize collector FocusInput");
            return;
        }
    };

    let program = resolve_program(&argv[0], &plugin_root);
    let args = argv[1..].to_vec();
    let plugin_id_owned = plugin_id.to_string();
    let module_id = module.id.clone();
    let event_tx = app.event_tx.clone();

    std::thread::spawn(move || {
        let result = run_collector(&program, &args, &plugin_root, &state_dir, &focus_json);
        let _ = event_tx.blocking_send(AppEvent::CollectorFinished {
            plugin_id: plugin_id_owned,
            module_id,
            result,
        });
    });
}

/// Run the collector command to completion (or [`COLLECTOR_TIMEOUT`]), returning
/// the parsed outcome or a human-readable failure reason.
fn run_collector(
    program: &str,
    args: &[String],
    plugin_root: &Path,
    state_dir: &Path,
    focus_json: &str,
) -> Result<CollectorOutcome, String> {
    // 0.8.0: upstream's plugin v1 replaced `command_for_argv` with
    // `command_for_argv_in_dir`, which sets the cwd itself and resolves a
    // relative `./collector`-style program against it. That is exactly the
    // plugin-root execution environment ADR-003 requires, so the collector
    // adopts upstream's helper instead of keeping a fork-local spawn path.
    let mut command = crate::plugin_command::command_for_argv_in_dir(program, args, plugin_root);
    command
        .env("HERDR_PLUGIN_STATE_DIR", state_dir)
        .env("HERDR_FOCUS_JSON", focus_json)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to spawn collector: {err}"))?;

    // Drain stdout on a helper thread so a full pipe can never deadlock the wait.
    let stdout_reader = child
        .stdout
        .take()
        .map(|stdout| std::thread::spawn(move || read_capped(stdout, COLLECTOR_STDOUT_MAX_BYTES)));

    let deadline = Instant::now() + COLLECTOR_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) => return Err(format!("failed to wait on collector: {err}")),
        }
    };

    if timed_out {
        // Do NOT join the stdout reader here: killing the child does not reap any
        // grandchild it spawned, and that orphan can keep the stdout pipe open,
        // which would block the join for the orphan's full lifetime. The reader
        // thread is left to finish on its own (it unblocks at pipe EOF). We don't
        // need the output on the timeout path anyway.
        return Err(format!(
            "collector timed out after {}s",
            COLLECTOR_TIMEOUT.as_secs()
        ));
    }

    let stdout = stdout_reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    match status {
        Some(status) if status.success() => parse_collector_stdout(&stdout),
        Some(status) => Err(format!("collector exited with status {status}")),
        None => Err("collector produced no exit status".to_string()),
    }
}

/// Read up to `cap` bytes from `reader` into a lossy UTF-8 string.
fn read_capped(mut reader: impl Read, cap: usize) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if kept.len() < cap {
                    let take = n.min(cap - kept.len());
                    kept.extend_from_slice(&buf[..take]);
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replace_with_snapshot() {
        let outcome = parse_collector_stdout(r#"{"outcome":"replace","snapshot":{"a":1}}"#)
            .expect("replace parses");
        match outcome {
            CollectorOutcome::Replace(value) => {
                assert_eq!(value.get("a").and_then(serde_json::Value::as_i64), Some(1));
            }
            other => panic!("expected replace, got {other:?}"),
        }
    }

    #[test]
    fn parses_keep_and_clear() {
        assert_eq!(
            parse_collector_stdout(r#"{"outcome":"keep"}"#).unwrap(),
            CollectorOutcome::Keep
        );
        assert_eq!(
            parse_collector_stdout("  {\"outcome\":\"clear\"}\n").unwrap(),
            CollectorOutcome::Clear
        );
    }

    #[test]
    fn rejects_replace_without_snapshot() {
        let err = parse_collector_stdout(r#"{"outcome":"replace"}"#).unwrap_err();
        assert!(err.contains("missing 'snapshot'"), "{err}");
    }

    #[test]
    fn rejects_unknown_and_unparseable() {
        assert!(parse_collector_stdout(r#"{"outcome":"bogus"}"#).is_err());
        assert!(parse_collector_stdout("not json").is_err());
        assert!(parse_collector_stdout(r#"{"no":"outcome"}"#).is_err());
    }

    #[test]
    fn resolves_relative_program_against_plugin_root() {
        let root = Path::new("/plugins/usage-dash-ui");
        assert_eq!(
            resolve_program("../../target/release/usage-dash-collector", root),
            "/plugins/usage-dash-ui/../../target/release/usage-dash-collector"
        );
        // Bare names and absolute paths pass through untouched.
        assert_eq!(
            resolve_program("usage-dash-collector", root),
            "usage-dash-collector"
        );
        assert_eq!(
            resolve_program("/usr/bin/collector", root),
            "/usr/bin/collector"
        );
    }

    #[test]
    fn read_capped_truncates() {
        let data = vec![b'x'; 1000];
        let out = read_capped(&data[..], 10);
        assert_eq!(out.len(), 10);
    }

    // fork(wave-2): host-runner integration over a real script fixture, proving
    // the spawn/env/protocol/timeout path end-to-end without the wasm plugin.
    #[cfg(unix)]
    mod runner {
        use super::*;
        use std::io::Write;

        fn write_script(body: &str) -> (PathBuf, PathBuf) {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!(
                "herdr-collector-test-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("collector.sh");
            let mut f = std::fs::File::create(&script).unwrap();
            f.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
                .unwrap();
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
            (dir, script)
        }

        fn run(script: &Path, dir: &Path) -> Result<CollectorOutcome, String> {
            run_collector(
                &script.to_string_lossy(),
                &[],
                dir,
                dir,
                r#"{"agent":"none"}"#,
            )
        }

        #[test]
        fn replace_keep_clear_scripts_map_to_outcomes() {
            let (dir, script) = write_script(r#"echo '{"outcome":"replace","snapshot":{"k":7}}'"#);
            assert!(matches!(
                run(&script, &dir),
                Ok(CollectorOutcome::Replace(_))
            ));
            std::fs::remove_dir_all(&dir).ok();

            let (dir, script) = write_script(r#"echo '{"outcome":"keep"}'"#);
            assert_eq!(run(&script, &dir).unwrap(), CollectorOutcome::Keep);
            std::fs::remove_dir_all(&dir).ok();

            let (dir, script) = write_script(r#"echo '{"outcome":"clear"}'"#);
            assert_eq!(run(&script, &dir).unwrap(), CollectorOutcome::Clear);
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn env_focus_json_reaches_child() {
            let (dir, script) = write_script(
                r#"printf '{"outcome":"replace","snapshot":{"focus":"%s"}}' "$HERDR_FOCUS_JSON""#,
            );
            let out = run_collector(&script.to_string_lossy(), &[], &dir, &dir, "sentinel-focus");
            match out {
                Ok(CollectorOutcome::Replace(value)) => {
                    assert_eq!(
                        value.get("focus").and_then(serde_json::Value::as_str),
                        Some("sentinel-focus")
                    );
                }
                other => panic!("expected replace, got {other:?}"),
            }
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn nonzero_exit_is_a_failure() {
            let (dir, script) = write_script("echo '{\"outcome\":\"keep\"}'\nexit 3");
            let err = run(&script, &dir).unwrap_err();
            assert!(err.contains("status"), "{err}");
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn timeout_kills_and_fails() {
            let (dir, script) = write_script("sleep 30");
            let start = Instant::now();
            let err = run(&script, &dir).unwrap_err();
            assert!(err.contains("timed out"), "{err}");
            assert!(
                start.elapsed() < COLLECTOR_TIMEOUT + Duration::from_secs(2),
                "timeout did not fire promptly"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
