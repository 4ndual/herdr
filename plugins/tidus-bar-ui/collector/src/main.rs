//! Out-of-process collector for the tidus subagent bar.
//!
//! Answers one question about the FOCUSED pane — *which orchestrator am I
//! looking at, who spawned it, and what hangs off it* — and writes the answer
//! to the plugin's `ui_input_file`, where the host's generic reader injects it
//! into the guest each frame.
//!
//! Run by `App::tick_plugin_collectors` on the dash tick, so it must be cheap:
//! no network, no subprocess, no unbounded walks. One scan of `~/harnesses/*`
//! (a handful of small file reads per harness), a few `/proc` reads for the
//! focused pane, and at most three local socket round trips. Wall time is
//! reported in `collect_us` so the brief's 50 ms budget is checkable from the
//! artefact itself rather than asserted.
//!
//! ## Parentage — the source of truth
//!
//! The fleet already records who spawned whom, so **no harness-fleet change was
//! needed** (`docs/tidus-ui-plugin-brief.md` §3). Three edges, strongest first:
//!
//! | file | edge | meaning |
//! | --- | --- | --- |
//! | `<child>/state/dispatch.json` | `Dispatch` | a routed task is in flight RIGHT NOW (`claimDispatch`, go/tidus.go:231) |
//! | `<child>/harness.json → team` | `Team` | declared, persistent affiliation |
//! | `<child>/state/last-result.json` | `History` | the most recent job came from this parent |
//!
//! `dispatch.json` is deleted the moment a task finishes, so it alone would make
//! children blink off the bar between jobs. The brief requires children to stay
//! *visually attached at all times*, so `team` and the last result keep the chip
//! present and the status dot carries "done"/"stopped" instead.
//!
//! ## Attribution of the focused pane
//!
//! Strongest evidence first; a failed lookup is never upgraded to a guess:
//!   1. `HARNESS_NAME` in the runtime process's environment — set by
//!      `harness spawn` (go/lifecycle.go:349). Definitive.
//!   2. `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `JCODE_HOME` → the config dir the
//!      process is really reading → the harness or **profile** that owns it.
//!      This is what attributes the human's own session, which is normally a
//!      profile (`main`/`vanilla`) and is the usual orchestrator.
//!   3. `<n>/state/herdr-pane` matching the focused pane id — tidus's own
//!      record, used last because it goes stale when a pane is replaced and may
//!      name another herdr instance's pane.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tidus_bar_model::{
    job_tail, BarInput, Chip, ChipRole, ChipStatus, EdgeKind, EmptyReason, MAX_ANCESTRY, SCHEMA,
};

/// The manifest's `ui_input_file` — the file the host reads each frame.
const INPUT_FILE: &str = "tidus_bar_input.json";

/// Socket budget. The server is local and answers in microseconds; a short
/// deadline means a wedged server costs the dash tick nothing.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(400);

/// Per-runtime config-dir environment variables, in the order we trust them.
const CONFIG_ENV_VARS: [&str; 3] = ["CODEX_HOME", "CLAUDE_CONFIG_DIR", "JCODE_HOME"];

/// Process names that ARE an agent runtime.
const RUNTIME_NAMES: [&str; 4] = ["claude", "codex", "pi", "jcode"];

/// `route.go` writes this literal parent when a dispatch came from the CLI
/// rather than from another harness. It is a sentinel, not a harness.
const CLI_PARENT: &str = "(cli)";

/// `statusOf` (go/lifecycle.go:46) treats a spawn lock younger than this as
/// "starting" rather than "stopped" — mirrored so the bar and `harness list`
/// never disagree about a harness mid-spawn.
const SPAWN_LOCK_FRESH: Duration = Duration::from_secs(30);

fn main() {
    let started = Instant::now();
    let out_path = output_path();
    if let Some(parent) = out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let focus = read_focus();
    let mut input = resolve(&focus);
    input.collect_us = started.elapsed().as_micros() as u64;

    match serde_json::to_vec(&input) {
        Ok(bytes) => {
            if let Err(err) = write_atomic(&out_path, &bytes) {
                eprintln!(
                    "tidus-bar-collector: write {} failed: {err}",
                    out_path.display()
                );
                std::process::exit(1);
            }
            // Host collector protocol (`src/plugin_collector.rs`): this plugin
            // owns its `ui_input_file` outright and has nothing for the host to
            // apply, so `keep` is the honest answer. Printing nothing would log
            // a parse failure for every successful run.
            println!("{{\"outcome\":\"keep\"}}");
        }
        Err(err) => {
            eprintln!("tidus-bar-collector: encode failed: {err}");
            std::process::exit(1);
        }
    }
}

// ── focus ──────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
struct Focus {
    pane_id: Option<String>,
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

// ── fleet model ────────────────────────────────────────────────────────────

/// One harness as the collector sees it on disk.
#[derive(Debug, Clone)]
struct Node {
    name: String,
    runtime: Option<String>,
    exec: Option<String>,
    /// Config dir this harness owns, canonicalised — the key that maps a
    /// running process back to its harness or profile.
    config_dir: Option<PathBuf>,
    /// A profile is an external config dir registered as a fleet member. It has
    /// no managed process, but it IS usually the orchestrator (the human's own
    /// session), so it must never be filtered out of attribution.
    profile: bool,
    /// Declared affiliation (`harness.json → team`).
    team: Option<String>,
    /// Parent of the in-flight dispatch, if any.
    dispatch_parent: Option<String>,
    /// Parent of the most recent finished job.
    history_parent: Option<String>,
    live: bool,
    task: Option<String>,
    job_tail: Option<String>,
    /// Outcome of the last finished job (`done` / `failed`).
    last_event: Option<String>,
    /// Fleet status, `statusOf` semantics.
    fleet_status: ChipStatus,
    /// Tab/pane as tidus recorded them; validated against this instance later.
    tab_id: Option<String>,
    pane_id: Option<String>,
    /// Runtime session id, used to attribute profile panes.
    session_id: Option<String>,
}

/// What herdr knows about the pane behind a harness's tab, keyed by tab id.
///
/// Keyed by TAB rather than pane because `state/herdr-tab` is the id the fleet
/// keeps fresh for every child (the SessionStart hook writes it), while
/// `state/herdr-pane` is only written by `harness spawn`.
#[derive(Debug, Clone, Default)]
struct AgentPane {
    pane_id: Option<String>,
    status: Option<String>,
}

impl Node {
    /// The edge to `parent`, strongest first, or `None` if unrelated.
    fn edge_to(&self, parent: &str) -> Option<EdgeKind> {
        if self.dispatch_parent.as_deref() == Some(parent) {
            return Some(EdgeKind::Dispatch);
        }
        if self.team.as_deref() == Some(parent) {
            return Some(EdgeKind::Team);
        }
        if self.history_parent.as_deref() == Some(parent) {
            return Some(EdgeKind::History);
        }
        None
    }

    /// The single parent used for walking UP the ancestor chain. A live
    /// dispatch is the truest "who is running me"; team is the durable
    /// fallback; history is last.
    fn primary_parent(&self) -> Option<&str> {
        self.dispatch_parent
            .as_deref()
            .or(self.team.as_deref())
            .or(self.history_parent.as_deref())
    }
}

fn load_fleet(root: &Path) -> Vec<Node> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let dir = entry.path();
        let Ok(raw) = std::fs::read(dir.join("harness.json")) else {
            continue; // a directory without harness.json is not a harness
        };
        let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&raw) else {
            continue;
        };

        let profile = meta
            .get("profile")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let config_dir = string_at(&meta, "configDir")
            .map(expand_tilde)
            .unwrap_or_else(|| dir.join("config"));
        let config_dir = std::fs::canonicalize(&config_dir).ok();

        let state = dir.join("state");
        let (dispatch_parent, live, task, job_tail_now) = read_dispatch(&state);
        let (history_parent, last_event, last_task, job_tail_last) = read_last_result(&state);

        nodes.push(Node {
            name: name.clone(),
            runtime: string_at(&meta, "runtime").or_else(|| Some("claude".to_string())),
            exec: string_at(&meta, "exec"),
            config_dir,
            profile,
            team: string_at(&meta, "team"),
            dispatch_parent,
            history_parent,
            live,
            task: task.or(last_task),
            job_tail: job_tail_now.or(job_tail_last),
            last_event,
            fleet_status: fleet_status(&name, &state, profile),
            tab_id: read_trim(&state.join("herdr-tab")),
            pane_id: read_trim(&state.join("herdr-pane")),
            session_id: read_trim(&state.join("session-id")),
        });
    }
    nodes
}

/// In-flight dispatch → `(parent, live, task, job_tail)`.
fn read_dispatch(state: &Path) -> (Option<String>, bool, Option<String>, Option<String>) {
    let Ok(raw) = std::fs::read(state.join("dispatch.json")) else {
        return (None, false, None, None);
    };
    let Ok(rec) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return (None, false, None, None);
    };
    // `claimDispatch` creates the file O_EXCL and then writes it; a record with
    // no id is a half-written file, not a job.
    let Some(id) = string_at(&rec, "id") else {
        return (None, false, None, None);
    };
    (
        string_at(&rec, "parent").filter(|p| p != CLI_PARENT),
        true,
        string_at(&rec, "task"),
        job_tail(&id),
    )
}

/// Most recent finished job → `(parent, event, task, job_tail)`.
fn read_last_result(
    state: &Path,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let Ok(raw) = std::fs::read(state.join("last-result.json")) else {
        return (None, None, None, None);
    };
    let Ok(ev) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return (None, None, None, None);
    };
    (
        string_at(&ev, "parent").filter(|p| p != CLI_PARENT),
        string_at(&ev, "event"),
        string_at(&ev, "task"),
        string_at(&ev, "id").and_then(|id| job_tail(&id)),
    )
}

/// Mirror of `statusOf` (go/lifecycle.go:30). Kept in lock-step deliberately:
/// a bar that disagrees with `harness list` about whether something is running
/// is worse than no bar.
fn fleet_status(name: &str, state: &Path, profile: bool) -> ChipStatus {
    if profile {
        // A profile has no supervisor; herdr's pane truth is the only signal,
        // and it is merged in later.
        return ChipStatus::Unknown;
    }
    if supervisor_alive(state) {
        if state.join("refresh-requested").exists() {
            return ChipStatus::RefreshQueued;
        }
        if state.join("idle").exists() {
            return ChipStatus::Idle;
        }
        return ChipStatus::Busy;
    }
    if let Ok(meta) = std::fs::metadata(state.join("spawn.lock")) {
        if let Ok(age) = meta.modified().and_then(|m| m.elapsed().map_err(io_other)) {
            if age < SPAWN_LOCK_FRESH {
                return ChipStatus::Starting;
            }
        }
    }
    let _ = name;
    ChipStatus::Stopped
}

fn io_other(err: std::time::SystemTimeError) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

/// `supAlive` (go/lifecycle.go:18): a recorded supervisor pid that still exists.
fn supervisor_alive(state: &Path) -> bool {
    let Some(pid) = read_trim(&state.join("supervisor.pid")) else {
        return false;
    };
    let Ok(pid) = pid.parse::<i32>() else {
        return false;
    };
    // `kill(pid, 0)` without libc: /proc is the same question, and the
    // collector is already linux-only for exactly this reason.
    Path::new(&format!("/proc/{pid}")).exists()
}

// ── resolution ─────────────────────────────────────────────────────────────

fn resolve(focus: &Focus) -> BarInput {
    let home = home();
    let mut input = BarInput {
        schema: SCHEMA,
        home: Some(home.clone()),
        self_focus: focus.is_self_focus,
        ..Default::default()
    };
    // A focus event about our own surface carries no pane to attribute. The
    // guest repaints its previous snapshot; answering "not tidus" would make
    // clicking a chip blank the bar you just clicked.
    if focus.is_self_focus {
        return input;
    }

    let nodes = load_fleet(&harness_root());
    if nodes.is_empty() {
        input.empty_reason = Some(EmptyReason::NoFleet);
        return input;
    }

    let api = ApiClient::connect();
    let agent_status = api
        .as_ref()
        .map(ApiClient::agent_status)
        .unwrap_or_default();

    let Some((focus_name, evidence)) = attribute(focus, &nodes, api.as_ref()) else {
        return input;
    };
    input.focus_harness = Some(focus_name.clone());
    input.evidence = Some(evidence);

    let by_name: HashMap<&str, &Node> = nodes.iter().map(|n| (n.name.as_str(), n)).collect();

    // Ancestors: walk up the spawn edges, root-most first. Cycle-guarded — a
    // malformed pair of records must not hang the dash tick.
    let mut ancestry: Vec<&Node> = Vec::new();
    let mut seen: Vec<&str> = vec![focus_name.as_str()];
    let mut cursor = by_name.get(focus_name.as_str()).copied();
    while let Some(node) = cursor {
        let Some(parent_name) = node.primary_parent() else {
            break;
        };
        if seen.contains(&parent_name) || ancestry.len() >= MAX_ANCESTRY {
            break;
        }
        let Some(parent) = by_name.get(parent_name).copied() else {
            break;
        };
        seen.push(parent_name);
        ancestry.push(parent);
        cursor = Some(parent);
    }
    ancestry.reverse(); // root-most first

    // Direct children ONLY — never uncles or cousins. This is the whole reason
    // the bar walks the spawn graph instead of reading herdr's tab order.
    let mut children: Vec<(&Node, EdgeKind)> = nodes
        .iter()
        .filter(|n| n.name != focus_name)
        // A profile is an external config dir registered as a fleet member; it
        // has no managed process and nobody spawns it, so it can be an
        // orchestrator but never a child.
        .filter(|n| !n.profile)
        .filter_map(|n| n.edge_to(&focus_name).map(|edge| (n, edge)))
        .collect();
    // Live work first, then declared members, then history; alphabetical inside
    // each band so the bar does not reshuffle under the cursor every tick.
    children.sort_by(|(a, ea), (b, eb)| {
        edge_rank(*ea)
            .cmp(&edge_rank(*eb))
            .then_with(|| a.name.cmp(&b.name))
    });

    // Validate every tab against THIS instance before it becomes a click
    // target: `state/herdr-tab` is written by whichever herdr the harness was
    // opened in, so an unvalidated id focuses a stranger's tab or nothing.
    let live_tabs = api.as_ref().map(ApiClient::tab_ids).unwrap_or_default();

    let mut chips: Vec<Chip> = Vec::new();
    for node in ancestry {
        chips.push(chip_for(
            node,
            ChipRole::Ancestor,
            None,
            &live_tabs,
            &agent_status,
        ));
    }
    if let Some(node) = by_name.get(focus_name.as_str()).copied() {
        chips.push(chip_for(
            node,
            ChipRole::Current,
            None,
            &live_tabs,
            &agent_status,
        ));
    }
    for (node, edge) in children {
        chips.push(chip_for(
            node,
            ChipRole::Child,
            Some(edge),
            &live_tabs,
            &agent_status,
        ));
    }

    input.empty_reason = None;
    input.chips = chips;
    input
}

fn edge_rank(edge: EdgeKind) -> u8 {
    match edge {
        EdgeKind::Dispatch => 0,
        EdgeKind::Team => 1,
        EdgeKind::History => 2,
    }
}

fn chip_for(
    node: &Node,
    role: ChipRole,
    edge: Option<EdgeKind>,
    live_tabs: &[String],
    agent_status: &HashMap<String, AgentPane>,
) -> Chip {
    let tab_id = node
        .tab_id
        .as_ref()
        .filter(|tab| live_tabs.iter().any(|t| t == *tab))
        .cloned();
    // Resolve the pane from the VALIDATED TAB rather than from
    // `state/herdr-pane`: only `harness spawn` writes that file, while the
    // fleet's SessionStart hook keeps `state/herdr-tab` fresh for every child.
    // Trusting the file alone silently loses live status for every
    // hook-registered child — they all reported "stopped" while sitting in
    // front of a permission prompt.
    let agent = tab_id.as_ref().and_then(|tab| agent_status.get(tab));
    let pane_id = agent
        .and_then(|a| a.pane_id.clone())
        .or_else(|| tab_id.as_ref().and(node.pane_id.clone()));
    Chip {
        name: node.name.clone(),
        role,
        status: status_for(node, agent.and_then(|a| a.status.as_deref())),
        edge,
        tab_id,
        pane_id,
        // Nothing is hidden until the host's create-hidden work lands; the
        // field exists now so phase 2 is a collector change, not a protocol one.
        hidden: false,
        task: node.task.clone(),
        job_tail: node.job_tail.clone(),
        exec: node.exec.clone(),
        runtime: node.runtime.clone(),
    }
}

/// Merge the two truths the brief names: *files = fleet truth, herdr events =
/// pane truth*. herdr is watching the actual terminal, so it wins on anything
/// it can see; the fleet's own status files answer for children with no pane in
/// this instance (reaped, `light`, or parked elsewhere).
fn status_for(node: &Node, pane_status: Option<&str>) -> ChipStatus {
    {
        match pane_status {
            // "blocked" IS the attention state: a child sitting on an
            // interactive prompt is the silent stall the brief says to shout
            // about. The supervisor debounces the same signal for 5s before
            // escalating it to the parent's inbox (go/supervisor.go:174); the
            // bar shows it immediately because it costs nothing to be early.
            Some("blocked") => return ChipStatus::Attention,
            Some("working") => return ChipStatus::Busy,
            // herdr's "done" is a PER-TURN completion, not a finished job: the
            // agent is alive and waiting, which the fleet calls idle. Mapping
            // it to ChipStatus::Done made a running orchestrator render as a
            // greyed, finished child while `harness list` said idle — the bar
            // must never contradict the fleet about whether something is
            // running. ChipStatus::Done stays reserved for a job outcome from
            // `last-result.json`.
            Some("idle") | Some("done") => return ChipStatus::Idle,
            _ => {}
        }
    }
    // A live dispatch outranks a stale idle marker: the file says what the
    // supervisor last wrote, the dispatch says a job is in flight now.
    if node.live && matches!(node.fleet_status, ChipStatus::Idle | ChipStatus::Unknown) {
        return ChipStatus::Busy;
    }
    if !matches!(node.fleet_status, ChipStatus::Stopped | ChipStatus::Unknown) {
        return node.fleet_status;
    }
    // Stopped: a reaped child keeps the outcome of its last job rather than
    // collapsing to a bare dot, so the bar still says how it went.
    match node.last_event.as_deref() {
        Some("done") => ChipStatus::Done,
        Some("failed") => ChipStatus::Failed,
        _ => node.fleet_status,
    }
}

/// Attribute the focused pane to a harness or profile. Returns the name and a
/// short description of the evidence, which the toast shows so a wrong answer
/// can be argued with rather than merely disbelieved.
fn attribute(
    focus: &Focus,
    nodes: &[Node],
    api: Option<&ApiClient>,
) -> Option<(String, String)> {
    let pane_id = focus.pane_id.as_deref()?;

    if let Some(api) = api {
        // Runtime processes first — their environment is the strongest claim —
        // then the rest, including the pane's own shell. A tab created with
        // `--env` carries the config dir in the SHELL, and the runtime is not
        // always a foreground process (`agent start` leaves the shell in
        // front), so restricting this scan to runtime names loses the whole
        // attribution on exactly the panes the fleet creates.
        let mut procs = api.pane_processes(pane_id);
        procs.sort_by_key(|(_, name)| !RUNTIME_NAMES.iter().any(|r| r == name));
        for (pid, name) in procs {
            let _ = &name;
            let env = read_proc_env(pid);

            // 1. `harness spawn` stamps the identity directly. Nothing beats it.
            if let Some(declared) = env.get("HARNESS_NAME").filter(|v| !v.is_empty()) {
                if nodes.iter().any(|n| &n.name == declared) {
                    return Some((declared.clone(), format!("HARNESS_NAME of pid {pid}")));
                }
            }

            // 2. The config dir the process is really reading. Profiles are
            //    included on purpose: the human's own session is normally a
            //    profile, and it is normally the orchestrator.
            for var in CONFIG_ENV_VARS {
                let Some(raw) = env.get(var) else { continue };
                let Ok(dir) = std::fs::canonicalize(raw) else {
                    continue;
                };
                if let Some(node) = nodes
                    .iter()
                    .find(|n| n.config_dir.as_deref() == Some(dir.as_path()))
                {
                    return Some((node.name.clone(), format!("{var} of pid {pid}")));
                }
            }
        }
    }

    // 3. A profile records its session id via the SessionStart hook, so the
    //    focused pane's session ref identifies it even though a profile has no
    //    HARNESS_NAME and may share a config dir with the default root.
    if let Some(session) = focus.session_ref.as_deref() {
        if let Some(node) = nodes
            .iter()
            .find(|n| n.session_id.as_deref() == Some(session))
        {
            return Some((node.name.clone(), "state/session-id".to_string()));
        }
    }

    // 4. tidus's own record of where it put the pane. Last, because it is not
    //    updated when a pane is replaced and may name another instance's pane.
    if let Some(node) = nodes.iter().find(|n| n.pane_id.as_deref() == Some(pane_id)) {
        return Some((node.name.clone(), "state/herdr-pane".to_string()));
    }
    None
}

fn read_proc_env(pid: i64) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return out;
    };
    for entry in raw.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some((key, value)) = text.split_once('=') {
            out.insert(key.to_string(), value.to_string());
        }
    }
    out
}

// ── herdr JSON API (newline-delimited over a unix socket) ──────────────────

struct ApiClient {
    socket: PathBuf,
}

impl ApiClient {
    /// Resolve the socket of the instance that launched us. The collector is a
    /// child of the herdr server, so the server's own environment reproduces
    /// what `session::api_socket_path_for` computes. Getting this wrong would
    /// validate tabs against ANOTHER instance, so an explicit
    /// `HERDR_SOCKET_PATH` wins and a missing socket is "no API", never a guess.
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
            "id": format!("tidus-bar:{method}"),
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

    /// Every tab id that exists in THIS instance right now.
    fn tab_ids(&self) -> Vec<String> {
        let Some(result) = self.request("tab.list", serde_json::json!({})) else {
            return Vec::new();
        };
        result
            .get("tabs")
            .and_then(serde_json::Value::as_array)
            .map(|tabs| tabs.iter().filter_map(|t| string_at(t, "tab_id")).collect())
            .unwrap_or_default()
    }

    /// `pane_id → agent_status` for every detected agent pane.
    fn agent_status(&self) -> HashMap<String, AgentPane> {
        let mut out = HashMap::new();
        let Some(result) = self.request("agent.list", serde_json::json!({})) else {
            return out;
        };
        let Some(agents) = result.get("agents").and_then(serde_json::Value::as_array) else {
            return out;
        };
        for agent in agents {
            let Some(tab) = string_at(agent, "tab_id") else {
                continue;
            };
            out.insert(
                tab,
                AgentPane {
                    pane_id: string_at(agent, "pane_id"),
                    status: string_at(agent, "agent_status"),
                },
            );
        }
        out
    }

    /// The pane's foreground processes as `(pid, name)`.
    fn pane_processes(&self, pane_id: &str) -> Vec<(i64, String)> {
        let Some(result) = self.request(
            "pane.process_info",
            serde_json::json!({ "pane_id": pane_id }),
        ) else {
            return Vec::new();
        };
        let info = result.get("process_info");
        let mut out: Vec<(i64, String)> = info
            .and_then(|info| info.get("foreground_processes"))
            .and_then(serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|proc| {
                        let pid = proc.get("pid").and_then(serde_json::Value::as_i64)?;
                        Some((pid, string_at(proc, "name").unwrap_or_default()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The pane's own shell carries the env a `--env`-created tab was given,
        // so it is a legitimate last resort when the runtime is not in the
        // foreground group.
        if let Some(shell) = info
            .and_then(|info| info.get("shell_pid"))
            .and_then(serde_json::Value::as_i64)
        {
            if !out.iter().any(|(pid, _)| *pid == shell) {
                out.push((shell, "shell".to_string()));
            }
        }
        out
    }
}

// ── environment ────────────────────────────────────────────────────────────

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn home() -> String {
    env_nonempty("HOME").unwrap_or_default()
}

/// `configDir` in `harness.json` is operator-written and may be `~`-relative.
fn expand_tilde(raw: String) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        return PathBuf::from(home()).join(rest);
    }
    if raw == "~" {
        return PathBuf::from(home());
    }
    PathBuf::from(raw)
}

/// Matches the host's own config dir name so a fork installed as `herdr` and an
/// upstream install do not fight over one socket path.
fn app_dir_name() -> String {
    env_nonempty("HERDR_APP_DIR").unwrap_or_else(|| "herdr".to_string())
}

fn harness_root() -> PathBuf {
    env_nonempty("HARNESS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home()).join("harnesses"))
}

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn output_path() -> PathBuf {
    if let Some(explicit) = env_nonempty("TIDUS_BAR_OUTPUT") {
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

    fn node(name: &str) -> Node {
        Node {
            name: name.to_string(),
            runtime: Some("claude".into()),
            exec: None,
            config_dir: None,
            profile: false,
            team: None,
            dispatch_parent: None,
            history_parent: None,
            live: false,
            task: None,
            job_tail: None,
            last_event: None,
            fleet_status: ChipStatus::Stopped,
            tab_id: None,
            pane_id: None,
            session_id: None,
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("tidus-bar-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn dispatch_beats_team_beats_history() {
        let mut n = node("kid");
        n.history_parent = Some("old".into());
        assert_eq!(n.edge_to("old"), Some(EdgeKind::History));
        assert_eq!(n.primary_parent(), Some("old"));

        n.team = Some("boss".into());
        assert_eq!(n.edge_to("boss"), Some(EdgeKind::Team));
        assert_eq!(n.primary_parent(), Some("boss"));

        n.dispatch_parent = Some("now".into());
        assert_eq!(n.edge_to("now"), Some(EdgeKind::Dispatch));
        assert_eq!(n.primary_parent(), Some("now"));
        // The weaker edges still resolve for their own parents — a child can be
        // on one parent's bar by team while serving a dispatch from another.
        assert_eq!(n.edge_to("boss"), Some(EdgeKind::Team));
        assert_eq!(n.edge_to("stranger"), None);
    }

    #[test]
    fn cli_sentinel_is_not_a_parent() {
        let dir = tempdir("cli");
        std::fs::write(
            dir.join("dispatch.json"),
            br#"{"id":"j1","parent":"(cli)","child":"kid","task":"t"}"#,
        )
        .unwrap();
        let (parent, live, task, tail) = read_dispatch(&dir);
        assert_eq!(parent, None, "(cli) is a sentinel, not a harness");
        assert!(live, "an in-flight dispatch is live even without a parent");
        assert_eq!(task.as_deref(), Some("t"));
        assert_eq!(tail.as_deref(), Some("j1"));
    }

    #[test]
    fn a_half_written_dispatch_is_not_a_job() {
        let dir = tempdir("half");
        // claimDispatch creates the file O_EXCL and only then writes it.
        std::fs::write(dir.join("dispatch.json"), br#"{"parent":"p"}"#).unwrap();
        let (parent, live, _, _) = read_dispatch(&dir);
        assert_eq!(parent, None);
        assert!(!live);
    }

    #[test]
    fn last_result_carries_parent_outcome_and_job_tail() {
        let dir = tempdir("last");
        std::fs::write(
            dir.join("last-result.json"),
            br#"{"id":"main-1786360945768895251","parent":"boss","event":"failed","task":"t"}"#,
        )
        .unwrap();
        let (parent, event, task, tail) = read_last_result(&dir);
        assert_eq!(parent.as_deref(), Some("boss"));
        assert_eq!(event.as_deref(), Some("failed"));
        assert_eq!(task.as_deref(), Some("t"));
        assert_eq!(tail.as_deref(), Some("895251"));
    }

    #[test]
    fn fleet_status_mirrors_status_of() {
        let dir = tempdir("status");
        // No supervisor, no lock → stopped.
        assert_eq!(fleet_status("n", &dir, false), ChipStatus::Stopped);
        // A profile never reports a fleet status; herdr's pane truth answers.
        assert_eq!(fleet_status("n", &dir, true), ChipStatus::Unknown);

        // A live supervisor: our own pid is guaranteed to exist.
        std::fs::write(dir.join("supervisor.pid"), std::process::id().to_string()).unwrap();
        assert_eq!(fleet_status("n", &dir, false), ChipStatus::Busy);
        std::fs::write(dir.join("idle"), b"").unwrap();
        assert_eq!(fleet_status("n", &dir, false), ChipStatus::Idle);
        std::fs::write(dir.join("refresh-requested"), b"").unwrap();
        assert_eq!(fleet_status("n", &dir, false), ChipStatus::RefreshQueued);

        // A dead supervisor with a fresh spawn lock is starting, not stopped —
        // reporting "stopped" here is what caused double-spawns in the fleet.
        std::fs::remove_file(dir.join("supervisor.pid")).unwrap();
        std::fs::write(dir.join("spawn.lock"), b"").unwrap();
        assert_eq!(fleet_status("n", &dir, false), ChipStatus::Starting);
    }

    #[test]
    fn blocked_pane_becomes_attention() {
        let n = node("kid");
        assert_eq!(status_for(&n, Some("blocked")), ChipStatus::Attention);
    }

    #[test]
    fn herdr_pane_truth_outranks_the_fleet_status_file() {
        let mut n = node("kid");
        n.fleet_status = ChipStatus::Idle; // stale idle marker
        assert_eq!(status_for(&n, Some("working")), ChipStatus::Busy);
    }

    #[test]
    fn a_child_registered_only_by_the_session_hook_still_gets_live_status() {
        // `state/herdr-pane` is written by `harness spawn` alone, so a child
        // that registered through the SessionStart hook has a fresh tab and NO
        // pane file. Resolving the pane from the validated TAB is what keeps
        // its status live — reading the file instead reported every such child
        // as "stopped" while it sat in front of a permission prompt.
        let mut n = node("kid");
        n.tab_id = Some("w1:t9".into());
        n.pane_id = None;
        let mut agents = HashMap::new();
        agents.insert(
            "w1:t9".to_string(),
            AgentPane {
                pane_id: Some("w1:p9".into()),
                status: Some("blocked".into()),
            },
        );
        let chip = chip_for(
            &n,
            ChipRole::Child,
            Some(EdgeKind::Team),
            &["w1:t9".to_string()],
            &agents,
        );
        assert_eq!(chip.status, ChipStatus::Attention);
        assert_eq!(chip.pane_id.as_deref(), Some("w1:p9"));
    }

    #[test]
    fn herdrs_per_turn_done_is_idle_not_a_finished_job() {
        // A live agent that just finished a turn is waiting, which is what the
        // fleet calls idle. Rendering it as Done greys a running orchestrator
        // and contradicts `harness list`.
        let mut n = node("kid");
        n.fleet_status = ChipStatus::Idle;
        assert_eq!(status_for(&n, Some("done")), ChipStatus::Idle);

        // A finished JOB is still Done — that fact comes from last-result.json.
        let mut finished = node("kid");
        finished.fleet_status = ChipStatus::Stopped;
        finished.last_event = Some("done".into());
        assert_eq!(status_for(&finished, None), ChipStatus::Done);
    }

    #[test]
    fn a_live_dispatch_overrides_a_stale_idle_marker() {
        let mut n = node("kid");
        n.live = true;
        n.fleet_status = ChipStatus::Idle;
        assert_eq!(status_for(&n, None), ChipStatus::Busy);
    }

    #[test]
    fn a_reaped_child_keeps_the_outcome_of_its_last_job() {
        let mut n = node("kid");
        n.fleet_status = ChipStatus::Stopped;
        n.last_event = Some("done".into());
        // The brief requires reaped children to stay attached and greyed, not
        // vanish — so the chip still says how the job went.
        assert_eq!(status_for(&n, None), ChipStatus::Done);
        assert!(ChipStatus::Done.is_dormant());

        n.last_event = None;
        assert_eq!(status_for(&n, None), ChipStatus::Stopped);
    }

    #[test]
    fn an_unvalidated_tab_is_dropped_and_takes_the_pane_with_it() {
        let mut n = node("kid");
        n.tab_id = Some("w9:tZ".into()); // belongs to another instance
        n.pane_id = Some("w9:pZ".into());
        let chip = chip_for(
            &n,
            ChipRole::Child,
            Some(EdgeKind::Team),
            &["w1:t2".to_string()],
            &HashMap::new(),
        );
        assert_eq!(chip.tab_id, None);
        assert_eq!(
            chip.pane_id, None,
            "a pane without its tab is not clickable"
        );
        assert!(!chip.clickable());

        let chip = chip_for(
            &n,
            ChipRole::Child,
            Some(EdgeKind::Team),
            &["w9:tZ".to_string()],
            &HashMap::new(),
        );
        assert_eq!(chip.tab_id.as_deref(), Some("w9:tZ"));
        assert!(chip.clickable());
    }

    #[test]
    fn edge_rank_puts_live_work_first() {
        assert!(edge_rank(EdgeKind::Dispatch) < edge_rank(EdgeKind::Team));
        assert!(edge_rank(EdgeKind::Team) < edge_rank(EdgeKind::History));
    }

    #[test]
    fn tilde_config_dirs_resolve_against_home() {
        let expanded = expand_tilde("~/x".to_string());
        assert_eq!(expanded, PathBuf::from(home()).join("x"));
        assert_eq!(expand_tilde("/abs".to_string()), PathBuf::from("/abs"));
    }

    #[test]
    fn output_path_prefers_explicit_then_host_state_dir() {
        std::env::set_var("TIDUS_BAR_OUTPUT", "/tmp/explicit.json");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", "/tmp/state");
        assert_eq!(output_path(), PathBuf::from("/tmp/explicit.json"));
        std::env::remove_var("TIDUS_BAR_OUTPUT");
        assert_eq!(output_path(), PathBuf::from("/tmp/state").join(INPUT_FILE));
        std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
    }
}
