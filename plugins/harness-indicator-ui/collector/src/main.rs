//! Out-of-process collector for the harness-indicator strip.
//!
//! Answers one question about the FOCUSED pane — *which fleet harness is this
//! agent actually running inside, and from where was it launched* — and writes
//! the answer to the plugin's `ui_input_file`, where the host's generic reader
//! injects it into the guest each frame.
//!
//! Run by the host's per-tick collector runner (`App::tick_plugin_collectors`),
//! so it must be CHEAP: no network, no unbounded walks, no sleeps. One or two
//! local socket round trips plus a handful of `/proc` and `harness.json` reads,
//! measured at a few milliseconds. There is deliberately NO throttle: unlike
//! version-dash this snapshot is about the pane you are looking at RIGHT NOW,
//! and a stale answer after a focus change is the one failure that would make
//! the strip worse than nothing.
//!
//! Resolution order, strongest evidence first:
//!   1. the live runtime process's own environment (`CLAUDE_CONFIG_DIR`,
//!      `CODEX_HOME`, `JCODE_HOME`) → the config dir it is really reading;
//!   2. the focused session's transcript file found under a harness root.
//! Anything unresolved stays `None` — the strip says "no harness" only when the
//! process demonstrably runs on the default roots, never because a lookup
//! failed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use harness_indicator_model::{HarnessRef, HarnessSource, IndicatorInput};

/// The manifest's `ui_input_file` — the file the host reads each frame.
const INPUT_FILE: &str = "harness_input.json";

/// Socket budget. The server is local and answers in microseconds; a short
/// deadline means a wedged server costs the dash tick nothing.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(400);

/// Per-runtime config-dir environment variables, in the order we trust them.
/// `CLAUDE_CONFIG_DIR` is always set by `harness open`/`supervise` (hooks and
/// persona read it) and `CODEX_HOME` additionally for codex, so a codex pane
/// carries both and either resolves to the same harness.
const CONFIG_ENV_VARS: [&str; 3] = ["CODEX_HOME", "CLAUDE_CONFIG_DIR", "JCODE_HOME"];

/// Process names that ARE an agent runtime. Used to pick which of a pane's
/// foreground processes owns the agent's working directory — the shell's cwd is
/// a different fact and is reported separately.
const RUNTIME_NAMES: [&str; 4] = ["claude", "codex", "pi", "jcode"];

fn main() {
    let out_path = output_path();
    if let Some(parent) = out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let focus = read_focus();
    let input = resolve(&focus);

    match serde_json::to_vec(&input) {
        Ok(bytes) => {
            if let Err(err) = write_atomic(&out_path, &bytes) {
                eprintln!(
                    "harness-indicator-collector: write {} failed: {err}",
                    out_path.display()
                );
                std::process::exit(1);
            }
            // Host collector protocol (`src/plugin_collector.rs`). This plugin
            // owns its `ui_input_file` outright and has nothing for the host to
            // apply to its own state, so `keep` is the honest answer. Printing
            // nothing would log a parse failure for a successful run.
            println!("{{\"outcome\":\"keep\"}}");
        }
        Err(err) => {
            eprintln!("harness-indicator-collector: encode failed: {err}");
            std::process::exit(1);
        }
    }
}

// ── focus (free, injected by the host) ─────────────────────────────────────

/// The subset of `HERDR_FOCUS_JSON` this collector uses. The host sets it on
/// every collector launch, so the focused pane's identity, agent and cwd cost
/// nothing — no socket call is needed to learn WHICH pane to describe.
#[derive(Debug, Default)]
struct Focus {
    pane_id: Option<String>,
    agent: Option<String>,
    cwd: Option<String>,
    session_ref: Option<String>,
    is_self_focus: bool,
}

fn read_focus() -> Focus {
    let Some(raw) = env_nonempty("HERDR_FOCUS_JSON") else {
        return Focus::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Focus::default();
    };
    Focus {
        pane_id: string_at(&value, "pane_id"),
        // `AgentKind` serializes as its variant name (`Claude`, `Codex`, `Pi`,
        // `Jcode`, `None`). Lowercase it and treat the unit `None` as absent —
        // a pane with no agent must not be described as running one called
        // "none".
        agent: string_at(&value, "agent")
            .map(|a| a.to_lowercase())
            .filter(|a| a != "none" && !a.is_empty()),
        cwd: string_at(&value, "cwd"),
        session_ref: string_at(&value, "session_ref"),
        is_self_focus: value
            .get("is_self_focus")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    }
}

fn string_at(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

// ── resolution ─────────────────────────────────────────────────────────────

fn resolve(focus: &Focus) -> IndicatorInput {
    let mut input = IndicatorInput {
        pane_id: focus.pane_id.clone(),
        agent: focus.agent.clone(),
        // focus.cwd is the PANE's cwd (its shell), not the runtime's, so it
        // seeds shell_cwd and agent_cwd is left to the runtime process itself.
        // Seeding agent_cwd from it would make every pane claim the agent runs
        // wherever the shell happens to be standing.
        shell_cwd: focus.cwd.clone(),
        self_focus: focus.is_self_focus,
        // The guest has no environment, so it cannot shorten `$HOME` itself.
        home: Some(home()).filter(|home| !home.is_empty()),
        ..Default::default()
    };

    // Focusing the strip itself is not a pane change; say so and let the guest
    // keep its previous frame rather than blanking.
    if focus.is_self_focus {
        return input;
    }
    let Some(pane_id) = focus.pane_id.clone() else {
        return input;
    };

    // The socket adds what focus cannot: the pane's live process list (for the
    // environment) and the shell's cwd. Its absence degrades the answer instead
    // of erasing it — pane id, agent and the focus cwd are already in hand.
    let client = ApiClient::connect();
    if let Some(client) = &client {
        if let Some(pane) = client.pane_info(&pane_id) {
            // `PaneInfo.cwd` is the pane SHELL's live cwd, not a launch record.
            if let Some(cwd) = string_at(&pane, "cwd") {
                input.shell_cwd = Some(cwd);
            }
            // herdr's own pane roster is a better agent source than the focus
            // snapshot for a pane that just changed runtime.
            if let Some(agent) = string_at(&pane, "agent") {
                input.agent = Some(agent.to_lowercase());
            }
        }
    } else {
        input.note = Some("herdr api unreachable".into());
    }

    let pids = client
        .as_ref()
        .map(|client| client.pane_processes(&pane_id))
        .unwrap_or_default();

    let harnesses = load_harnesses();

    // 1. The runtime process's own environment — the strongest evidence there
    //    is, because it is what the process is actually reading. The same walk
    //    yields that process's cwd, which is where the agent was launched.
    // Where the AGENT runs, from the runtime process itself. Matched by name so
    // it also works for a default-root agent, which carries no config variable
    // at all — the pane's shell is NOT a fallback here, because a shell standing
    // somewhere else is exactly the divergence this field exists to expose.
    if input.agent.is_some() {
        input.agent_cwd = pids
            .iter()
            .find(|(_, name)| RUNTIME_NAMES.contains(&name.as_str()))
            .and_then(|(pid, _)| process_cwd(*pid));
    }

    for (pid, _) in &pids {
        let Some(config_dir) = config_dir_of(*pid) else {
            continue;
        };
        if input.config_dir.is_none() {
            // A config dir that matches no harness is the positive proof of "no
            // harness": the runtime is reading a root we can name, and it is
            // not one of the fleet's. Keep it for the click detail either way.
            input.config_dir = Some(config_dir.to_string_lossy().into_owned());
        }
        if let Some(harness) = harnesses.get(&config_dir) {
            input.harness = Some(HarnessRef {
                source: HarnessSource::Env,
                ..harness.clone()
            });
            return input;
        }
    }

    if harnesses.is_empty() {
        // No fleet on this machine: "no harness" would then be a statement
        // about the machine rather than about the pane, so say so instead of
        // implying the pane is misconfigured.
        input.note = Some("no fleet".into());
        return input;
    }

    // 2. Transcript under a harness root.
    if let (Some(agent), Some(session_ref)) = (&input.agent, &focus.session_ref) {
        if let Some(harness) = harness_by_transcript(&harnesses, agent, session_ref, &input) {
            input.harness = Some(harness);
        }
    }

    input
}

/// The config dir a live process is reading, from its own environment.
/// Symlinks are resolved so `~/harnesses/x/config` and a symlink to it are the
/// same answer.
fn config_dir_of(pid: u32) -> Option<PathBuf> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for entry in raw.split(|byte| *byte == 0) {
        let Ok(text) = std::str::from_utf8(entry) else {
            continue;
        };
        let Some((key, value)) = text.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        if let Some(known) = CONFIG_ENV_VARS.iter().find(|var| **var == key) {
            seen.insert(known, value);
        }
    }
    // A codex pane carries BOTH CODEX_HOME and CLAUDE_CONFIG_DIR (the fleet
    // sets both), so the order of CONFIG_ENV_VARS decides, not /proc's order —
    // which is the process's and means nothing to us.
    CONFIG_ENV_VARS
        .iter()
        .find_map(|var| seen.get(var))
        .map(|value| canonical(Path::new(value)))
}

/// Locate a harness by finding the focused session's transcript under one of
/// their config dirs. Bounded on purpose: a direct stat for claude's
/// cwd-encoded layout, and at most a handful of day directories for codex.
fn harness_by_transcript(
    harnesses: &HashMap<PathBuf, HarnessRef>,
    agent: &str,
    session_ref: &str,
    input: &IndicatorInput,
) -> Option<HarnessRef> {
    let stem = Path::new(session_ref)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(session_ref);
    for (config_dir, harness) in harnesses {
        let hit = match agent {
            "claude" => {
                let cwd = input.agent_cwd.as_deref().unwrap_or_default();
                !cwd.is_empty()
                    && config_dir
                        .join("projects")
                        .join(encode_cwd(cwd))
                        .join(format!("{stem}.jsonl"))
                        .exists()
            }
            "codex" => codex_rollout_exists(config_dir, stem),
            _ => false,
        };
        if hit {
            return Some(HarnessRef {
                source: HarnessSource::Transcript,
                ..harness.clone()
            });
        }
    }
    None
}

/// Claude Code's project-dir encoding: `/`, `_` and `.` all become `-`.
fn encode_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| {
            if c == '/' || c == '_' || c == '.' {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// Codex writes `sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`. Scan the two
/// most recent day directories only — a session older than that is not the one
/// currently focused, and an unbounded walk has no place on a per-tick path.
fn codex_rollout_exists(config_dir: &Path, uuid: &str) -> bool {
    let sessions = config_dir.join("sessions");
    let mut days: Vec<PathBuf> = Vec::new();
    for year in newest_children(&sessions, 1) {
        for month in newest_children(&year, 1) {
            days.extend(newest_children(&month, 2));
        }
    }
    days.iter().any(|day| {
        std::fs::read_dir(day)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .any(|entry| entry.file_name().to_string_lossy().contains(uuid))
            })
            .unwrap_or(false)
    })
}

/// The `limit` lexicographically-largest child directories of `dir`. Codex's
/// layout is zero-padded numeric, so lexicographic order IS chronological.
fn newest_children(dir: &Path, limit: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    names.sort();
    names.reverse();
    names.truncate(limit);
    names
}

// ── fleet enumeration ──────────────────────────────────────────────────────

/// Map every fleet harness's CONFIG dir → its identity. Keyed by config dir
/// because that is what a running process's environment points at.
fn load_harnesses() -> HashMap<PathBuf, HarnessRef> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(harness_root()) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let dir = entry.path();
        let Ok(raw) = std::fs::read_to_string(dir.join("harness.json")) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let name = string_at(&value, "name")
            .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
        // A harness.json with no `runtime` means claude (`go/runtime.go`
        // runtimeOfDir), and that default is what the mismatch check compares
        // against — so it is resolved here, not left as unknown.
        let runtime = Some(string_at(&value, "runtime").unwrap_or_else(|| "claude".into()));
        let profile = value
            .get("profile")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let config_dir = string_at(&value, "configDir")
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join("config"));
        let config_dir = canonical(&config_dir);
        // A profile whose config dir IS a runtime's default root (the `main`
        // profile points at `~/.claude`) is not isolation — it is the default,
        // registered under a name. Reporting it as a harness would label every
        // ordinary pane on the machine, which is precisely the case this strip
        // exists to distinguish FROM a harness.
        if default_roots().contains(&config_dir) {
            continue;
        }
        out.insert(
            config_dir,
            HarnessRef {
                name,
                runtime,
                profile,
                source: HarnessSource::Env,
            },
        );
    }
    out
}

/// The per-runtime DEFAULT config roots. A pane reading one of these is on the
/// default roots by definition, whatever a `harness.json` may also claim.
fn default_roots() -> Vec<PathBuf> {
    let home = PathBuf::from(home());
    [".claude", ".codex", ".jcode"]
        .iter()
        .map(|dir| canonical(&home.join(dir)))
        .collect()
}

fn harness_root() -> PathBuf {
    env_nonempty("HARNESS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home()).join("harnesses"))
}

// ── herdr JSON API (newline-delimited over a unix socket) ──────────────────

struct ApiClient {
    socket: PathBuf,
}

impl ApiClient {
    /// Resolve the socket of the instance that launched us. The collector is a
    /// child of the herdr server, so the server's own environment is inherited
    /// and `XDG_CONFIG_HOME` + `HERDR_SESSION` reproduce exactly what
    /// `session::api_socket_path_for` computes. Getting this wrong would
    /// describe ANOTHER instance's focused pane, so the explicit
    /// `HERDR_SOCKET_PATH` wins when present and a missing socket file is
    /// treated as "no API" rather than guessed around.
    fn connect() -> Option<Self> {
        let socket = if let Some(explicit) = env_nonempty("HERDR_SOCKET_PATH") {
            PathBuf::from(explicit)
        } else {
            let config = env_nonempty("XDG_CONFIG_HOME")
                .map(|dir| PathBuf::from(dir).join(app_dir_name()))
                .unwrap_or_else(|| PathBuf::from(home()).join(".config").join(app_dir_name()));
            match env_nonempty("HERDR_SESSION") {
                Some(session) => config.join("sessions").join(session).join("herdr.sock"),
                None => config.join("herdr.sock"),
            }
        };
        socket.exists().then_some(Self { socket })
    }

    fn request(&self, method: &str, params: serde_json::Value) -> Option<serde_json::Value> {
        let mut stream = UnixStream::connect(&self.socket).ok()?;
        stream.set_read_timeout(Some(SOCKET_TIMEOUT)).ok()?;
        stream.set_write_timeout(Some(SOCKET_TIMEOUT)).ok()?;
        let request = serde_json::json!({
            "id": format!("harness-indicator:{method}"),
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_vec(&request).ok()?;
        line.push(b'\n');
        stream.write_all(&line).ok()?;
        stream.flush().ok()?;
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).ok()?;
        let value: serde_json::Value = serde_json::from_str(&response).ok()?;
        value.get("result").cloned()
    }

    /// The focused pane's roster row.
    fn pane_info(&self, pane_id: &str) -> Option<serde_json::Value> {
        let result = self.request("pane.list", serde_json::json!({}))?;
        result
            .get("panes")?
            .as_array()?
            .iter()
            .find(|pane| pane.get("pane_id").and_then(|v| v.as_str()) == Some(pane_id))
            .cloned()
    }

    /// The pane's foreground processes as `(pid, name)`, outermost first. The
    /// runtime may be the shell's child (`sh -c claude …`), so every one is a
    /// candidate and the caller decides which answers which question.
    fn pane_processes(&self, pane_id: &str) -> Vec<(u32, String)> {
        let Some(result) = self.request(
            "pane.process_info",
            serde_json::json!({ "pane_id": pane_id }),
        ) else {
            return Vec::new();
        };
        let info = result.get("process_info");
        let mut pids: Vec<(u32, String)> = info
            .and_then(|info| info.get("foreground_processes"))
            .and_then(|list| list.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|proc| {
                        let pid = proc.get("pid").and_then(serde_json::Value::as_u64)? as u32;
                        let name = proc
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        Some((pid, name))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The shell itself carries the env a `--env`-created tab was given, so
        // it is a legitimate last resort when the runtime is not its child.
        if let Some(shell) = info
            .and_then(|info| info.get("shell_pid"))
            .and_then(serde_json::Value::as_u64)
        {
            let shell = shell as u32;
            if !pids.iter().any(|(pid, _)| *pid == shell) {
                pids.push((shell, String::new()));
            }
        }
        pids
    }
}

/// Release builds use `herdr`, debug builds `herdr-dev` (`config::app_dir_name`).
/// The collector ships beside whichever host built it, so it must agree.
fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

// ── small helpers ──────────────────────────────────────────────────────────

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn home() -> String {
    env_nonempty("HOME").unwrap_or_default()
}

/// The cwd of a live process. This is where a runtime was LAUNCHED: agent
/// CLIs do not chdir after start, and herdr keeps no launch record of its own
/// (`PaneInfo.cwd` and `PaneInfo.foreground_cwd` are both live cwds), so the
/// process itself is the only remaining source of that fact.
fn process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Where the host reads this plugin's snapshot from.
///
/// `HERDR_PLUGIN_STATE_DIR` is set by the host on every collector launch and
/// points at THIS module's state dir, so the snapshot always lands in the
/// instance that asked for it — no XDG guessing and no per-instance job.
/// `HARNESS_INDICATOR_OUTPUT` overrides it for tests and manual runs.
fn output_path() -> PathBuf {
    if let Some(explicit) = env_nonempty("HARNESS_INDICATOR_OUTPUT") {
        return PathBuf::from(explicit);
    }
    if let Some(state) = env_nonempty("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(state).join(INPUT_FILE);
    }
    PathBuf::from(".").join(INPUT_FILE)
}

/// Write through a temp file + rename so the host never reads a half-written
/// snapshot on the very tick we are replacing it.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_project_dir_encoding_matches_claude_code() {
        assert_eq!(
            encode_cwd("/home/tester/harnesses/e2e-claude/workspace"),
            "-home-tester-harnesses-e2e-claude-workspace"
        );
        // `_` and `.` collapse to `-` as well, which is what makes a repo like
        // `my_app.v2` resolvable at all.
        assert_eq!(encode_cwd("/a/my_app.v2"), "-a-my-app-v2");
    }

    #[test]
    fn agent_none_is_absent_not_an_agent_named_none() {
        std::env::set_var(
            "HERDR_FOCUS_JSON",
            r#"{"pane_id":"w1:p1","agent":"None","cwd":"/tmp","is_self_focus":false}"#,
        );
        let focus = read_focus();
        assert_eq!(focus.pane_id.as_deref(), Some("w1:p1"));
        assert!(focus.agent.is_none());
        std::env::set_var(
            "HERDR_FOCUS_JSON",
            r#"{"pane_id":"w1:p2","agent":"Claude","cwd":"/tmp"}"#,
        );
        assert_eq!(read_focus().agent.as_deref(), Some("claude"));
        std::env::remove_var("HERDR_FOCUS_JSON");
    }

    #[test]
    fn self_focus_short_circuits_before_any_io() {
        let focus = Focus {
            pane_id: Some("w1:p1".into()),
            is_self_focus: true,
            ..Default::default()
        };
        let input = resolve(&focus);
        assert!(input.self_focus);
        assert!(input.harness.is_none());
    }

    #[test]
    fn newest_children_orders_codex_day_dirs_newest_first() {
        let root = std::env::temp_dir().join(format!("hi-days-{}", std::process::id()));
        for day in ["01", "09", "13"] {
            std::fs::create_dir_all(root.join(day)).expect("mkdir");
        }
        let newest = newest_children(&root, 2);
        assert_eq!(newest.len(), 2);
        assert!(newest[0].ends_with("13"));
        assert!(newest[1].ends_with("09"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// One test owns HOME + HARNESS_ROOT: they are process-global, so splitting
    /// these assertions across threaded tests would race.
    #[test]
    fn fleet_scan_keeps_isolated_roots_and_drops_default_ones() {
        let root = std::env::temp_dir().join(format!("hi-fleet-{}", std::process::id()));
        let fake_home = root.join("home");
        let real = root.join("fleet/real/config");
        std::fs::create_dir_all(&real).expect("mkdir real");
        std::fs::create_dir_all(fake_home.join(".claude")).expect("mkdir default root");
        std::fs::create_dir_all(root.join("fleet/real")).expect("mkdir harness");
        std::fs::create_dir_all(root.join("fleet/asdefault")).expect("mkdir profile");
        std::fs::write(
            root.join("fleet/real/harness.json"),
            br#"{"name":"real","runtime":"codex"}"#,
        )
        .expect("write harness.json");
        // A profile pointing straight at the default root — `main`'s shape.
        std::fs::write(
            root.join("fleet/asdefault/harness.json"),
            format!(
                r#"{{"name":"asdefault","profile":true,"configDir":"{}"}}"#,
                fake_home.join(".claude").display()
            )
            .as_bytes(),
        )
        .expect("write profile harness.json");

        let real_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &fake_home);
        std::env::set_var("HARNESS_ROOT", root.join("fleet"));
        let harnesses = load_harnesses();
        std::env::remove_var("HARNESS_ROOT");
        match real_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        let names: Vec<&str> = harnesses.values().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["real"],
            "the default root must not be a harness"
        );
        let real_entry = harnesses
            .get(&canonical(&real))
            .expect("keyed by config dir");
        assert_eq!(real_entry.runtime.as_deref(), Some("codex"));
        assert!(!real_entry.profile);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn output_path_prefers_explicit_then_host_state_dir() {
        std::env::set_var("HARNESS_INDICATOR_OUTPUT", "/tmp/explicit.json");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", "/tmp/state");
        assert_eq!(output_path(), PathBuf::from("/tmp/explicit.json"));
        std::env::remove_var("HARNESS_INDICATOR_OUTPUT");
        assert_eq!(output_path(), PathBuf::from("/tmp/state").join(INPUT_FILE));
        std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
    }
}
