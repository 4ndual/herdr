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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tidus_bar_model::codename;
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
    // `--dump-tree` resolves the snapshot exactly as a dash tick would and
    // prints the parent → children mapping it produced, instead of writing the
    // bar's input file.
    //
    // It exists so the binding between "what the fleet recorded" and "what the
    // bar renders" is auditable without reading pixels. A bar that draws two
    // children when the orchestrator dispatched five is indistinguishable, on
    // screen, from an orchestrator that only dispatched two — this prints which
    // of the two it is, and for every instance the collector could NOT attach,
    // says why. It is generated from `resolve()`, so it cannot drift from what
    // the bar draws: if this list is wrong, the bar is wrong in the same way.
    if std::env::args().any(|arg| arg == "--dump-tree") {
        dump_tree();
        return;
    }

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

/// Print the resolved lineage the bar would draw for the focused pane, plus
/// every fleet instance the collector could NOT attach to it and the reason.
///
/// The attached half comes straight out of [`resolve`] — the same call the dash
/// tick makes — so the mapping printed here IS the mapping rendered. The
/// unattached half is the part worth reading: an instance with no
/// `dispatch.json`, no `team`, and no `last-result.json` has NO recorded edge
/// to anything, and no amount of rendering work will put it under its parent.
/// That is a fleet-side gap, and printing it is how the two get told apart.
fn dump_tree() {
    let focus = read_focus();
    let input = resolve(&focus);

    println!("tidus-bar resolved lineage");
    println!(
        "  focused pane   {}",
        focus.pane_id.as_deref().unwrap_or("(none)")
    );
    println!(
        "  focus session  {}",
        focus.session_ref.as_deref().unwrap_or("(none)")
    );
    println!(
        "  orchestrator   {}",
        input.focus_harness.as_deref().unwrap_or("(unattributed)")
    );
    if let Some(evidence) = input.evidence.as_deref() {
        println!("  attributed by  {evidence}");
    }

    let row = |chip: &Chip| {
        println!(
            "    {:<9} {:<22} pane={:<12} tab={:<12} edge={:<8} status={}",
            format!("{:?}", chip.role).to_lowercase(),
            chip.name,
            chip.pane_id.as_deref().unwrap_or("-"),
            chip.tab_id.as_deref().unwrap_or("-"),
            chip.edge
                .map(|e| format!("{e:?}").to_lowercase())
                .unwrap_or_else(|| "-".into()),
            chip.status.label(),
        );
    };

    println!("\n  lineage (root-most first, all clickable):");
    for chip in input.lineage() {
        row(chip);
    }
    println!("\n  brothers of the selected node:");
    for chip in input.siblings() {
        row(chip);
    }
    println!("\n  children of the selected node:");
    for chip in input.children() {
        row(chip);
    }

    // Everything the fleet knows about that did NOT make it onto the bar, with
    // the reason. This is the half that answers "why is my child missing".
    let nodes = load_fleet(&harness_root());
    let now = SystemTime::now();
    let on_bar: Vec<&str> = input.chips.iter().map(|c| c.name.as_str()).collect();
    println!("\n  NOT attached to the focused orchestrator:");
    for node in &nodes {
        if on_bar.contains(&node.name.as_str()) {
            continue;
        }
        let why = if node.profile {
            "profile — never a child"
        } else if node.dispatch_parent.is_none()
            && node.team.is_none()
            && node.history_parent.is_none()
        {
            "NO recorded edge: no dispatch.json, no team, no last-result.json"
        } else if let Some(parent) = node.primary_parent(now) {
            if input.focus_harness.as_deref() != Some(parent) {
                "child of another orchestrator"
            } else if node.team.as_deref() == Some(parent) && node.tab_id.is_none() {
                // A team edge is never session-filtered and never expires, so
                // the only thing that can drop one is having no pane open here.
                "team member of this orchestrator, but no open tab in this herdr"
            } else {
                "edge to this orchestrator, but its job belongs to another session"
            }
        } else {
            "edge expired past the retention window"
        };
        let parent = node
            .dispatch_parent
            .as_deref()
            .or(node.team.as_deref())
            .or(node.history_parent.as_deref())
            .unwrap_or("-");
        println!(
            "    {:<22} parent={:<16} {}",
            node.name, parent, why
        );
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

/// How long a FINISHED dispatch keeps its chip on the bar, greyed.
///
/// Twelve hours — a working day, deliberately.
///
/// This was ten minutes, chosen as the gap between two glances at the bar so a
/// seven-second `codex-gp` dispatch could not appear and vanish unseen. That
/// answered "did anything run just now?", but it made the bar answer the WRONG
/// question: a child dispatched this morning and finished at lunch silently
/// left the structure, so "show me what this orchestrator is running" quietly
/// became "show me what it ran in the last ten minutes". Measured live: `main`
/// had dispatched docs-writer, test-w1 and others, and the bar showed two of
/// them — test-w1 dropped off the moment its result aged past the window.
///
/// It also broke the lineage rule outright. A child's parent edge expires with
/// the same clock (`primary_parent`), so an expired history edge truncated the
/// ancestor chain — and the way back UP the tree must not depend on how long
/// ago the work finished.
///
/// A day-scale window keeps every instance the orchestrator dispatched today on
/// the bar. The status dot, not the chip's presence, is what says "this one is
/// done"; dropping the chip says "this never happened", which is a different
/// and false claim.
///
/// Measured from `last-result.json`'s own `t`, so this needs no state carried
/// between ticks: the collector is a fresh process on every dash tick, and a
/// sidecar file recording finish times would be one more thing to keep
/// consistent, expire, and explain when it goes stale.
///
/// A TEAM edge is not subject to this. Team is a declared, persistent
/// affiliation — those children stay on the bar whether or not they have run
/// recently, which is the whole difference between the two edges.
const DISPATCH_GRACE: Duration = Duration::from_secs(12 * 60 * 60);

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
    /// Session that DISPATCHED the in-flight job, when the fleet stamped one.
    dispatch_session: Option<String>,
    /// Session that dispatched the most recent finished job.
    history_session: Option<String>,
    /// When that job finished, from `last-result.json → t`. `None` when the
    /// file has no parseable timestamp, which is treated as "too old to hold"
    /// rather than "hold forever": an unreadable clock must not pin a chip to
    /// the bar permanently.
    history_at: Option<SystemTime>,
    live: bool,
    task: Option<String>,
    job_tail: Option<String>,
    /// Outcome of the last finished job (`done` / `failed`).
    last_event: Option<String>,
    /// Fleet status, `statusOf` semantics.
    fleet_status: ChipStatus,
    /// `<harness>/workspace`, canonicalised — the cwd a fleet pane runs in, and
    /// the only link to its tab that cannot go stale.
    workspace_dir: Option<PathBuf>,
    /// Tab/pane as tidus recorded them; validated against this instance later.
    tab_id: Option<String>,
    pane_id: Option<String>,
    /// `state/session-id` — the managed harness's own primary session, written
    /// by its session-start hook. The fallback focus-session when the host's
    /// focus event carries no `session_ref` (herdr does not always detect an
    /// `agent_session` for a pane; measured live 2026-08-16: a supervised
    /// harness pane with a dispatch in flight had none, so every
    /// session-stamped child edge was filtered out and the bar said
    /// "no children" while a job was visibly running).
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

/// The raw inputs for a chip's "instance name" (titlesync,
/// harness-fleet@bcfd73f/51b3323: session title = pane label = tab name):
/// the pane's manual label, and its terminal title with the host's own ANSI
/// stripping already applied. Keyed by tab id, same as [`AgentPane`].
#[derive(Debug, Clone, Default)]
struct PaneTitle {
    label: Option<String>,
    terminal_title_stripped: Option<String>,
}

impl Node {
    /// The edge to `parent`, strongest first, or `None` if unrelated.
    ///
    /// `now` is threaded in rather than read here so the whole snapshot is
    /// computed against ONE instant: a tick that read the clock per node could
    /// expire one chip and keep its sibling from the same job.
    fn edge_to(
        &self,
        parent: &str,
        now: SystemTime,
        focus_session: Option<&str>,
    ) -> Option<EdgeKind> {
        if self.dispatch_parent.as_deref() == Some(parent)
            && Self::job_belongs_to_session(self.dispatch_session.as_deref(), focus_session)
        {
            return Some(EdgeKind::Dispatch);
        }
        if self.team.as_deref() == Some(parent) {
            return Some(EdgeKind::Team);
        }
        if self.history_parent.as_deref() == Some(parent)
            && self.within_grace(now)
            && Self::job_belongs_to_session(self.history_session.as_deref(), focus_session)
        {
            return Some(EdgeKind::History);
        }
        None
    }

    /// Whether a JOB edge belongs to the pane now being looked at.
    ///
    /// The fleet stamps `parentSession` — the dispatching session's uuid — on
    /// every dispatch and job record. A profile has many live sessions at once,
    /// so "parent = main" identifies the fleet member but not which of its panes
    /// sent the work; without this every pane of a profile showed every other
    /// pane's children, i.e. its nephews.
    ///
    /// Unowned work (the field absent — a job the fleet stamped before this
    /// existed) shows EVERYWHERE. There is no session to match it against, and
    /// hiding it would silently drop legacy jobs from every bar rather than from
    /// the wrong one.
    ///
    /// Team edges never reach here: a team affiliation is profile-level and
    /// belongs to every session of that profile, which is what makes it
    /// different from a job in the first place.
    fn job_belongs_to_session(owner: Option<&str>, focus_session: Option<&str>) -> bool {
        match owner {
            None => true,
            Some(owner) => focus_session == Some(owner),
        }
    }

    /// Whether the last finished job is recent enough to keep its chip.
    ///
    /// A missing or unparseable timestamp answers false — an unreadable clock
    /// must not pin a chip to the bar forever, and the fleet has written `t` on
    /// every result since this file existed.
    fn within_grace(&self, now: SystemTime) -> bool {
        let Some(at) = self.history_at else {
            return false;
        };
        now.duration_since(at)
            // A result stamped in the future is a clock skew, not an ancient
            // job; treat it as fresh rather than expiring it instantly.
            .map_or(true, |elapsed| elapsed <= DISPATCH_GRACE)
    }

    /// The single parent used for walking UP the ancestor chain. A live
    /// dispatch is the truest "who is running me"; team is the durable
    /// fallback; history is last.
    fn primary_parent(&self, now: SystemTime) -> Option<&str> {
        self.dispatch_parent
            .as_deref()
            .or(self.team.as_deref())
            .or_else(|| {
                // Same grace as `edge_to`: an expired history edge must not keep
                // walking the ancestor chain, or the bar would pin a lineage
                // that the children band no longer shows.
                self.within_grace(now)
                    .then_some(self.history_parent.as_deref())
                    .flatten()
            })
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
        let dispatch = read_dispatch(&state);
        let (dispatch_parent, live, task, job_tail_now) = (
            dispatch.parent,
            dispatch.live,
            dispatch.task,
            dispatch.job_tail,
        );
        let last = read_last_result(&state);
        let history_session = last
            .job_id
            .as_deref()
            .and_then(|id| job_parent_session(root, id));
        let (history_parent, last_event, last_task, job_tail_last, history_at) =
            (last.parent, last.event, last.task, last.job_tail, last.at);

        nodes.push(Node {
            name: name.clone(),
            runtime: string_at(&meta, "runtime").or_else(|| Some("claude".to_string())),
            exec: string_at(&meta, "exec"),
            config_dir,
            profile,
            team: string_at(&meta, "team"),
            dispatch_parent,
            dispatch_session: dispatch.parent_session,
            history_parent,
            history_session,
            history_at,
            live,
            task: task.or(last_task),
            job_tail: job_tail_now.or(job_tail_last),
            last_event,
            workspace_dir: std::fs::canonicalize(dir.join("workspace")).ok(),
            fleet_status: fleet_status(&name, &state, profile),
            tab_id: read_trim(&state.join("herdr-tab")),
            pane_id: read_trim(&state.join("herdr-pane")),
            session_id: read_trim(&state.join("session-id")),
        });
    }
    nodes
}

/// In-flight dispatch → `(parent, live, task, job_tail)`.
fn read_dispatch(state: &Path) -> Dispatch {
    let Ok(raw) = std::fs::read(state.join("dispatch.json")) else {
        return Dispatch::default();
    };
    let Ok(rec) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return Dispatch::default();
    };
    // `claimDispatch` creates the file O_EXCL and then writes it; a record with
    // no id is a half-written file, not a job.
    let Some(id) = string_at(&rec, "id") else {
        return Dispatch::default();
    };
    Dispatch {
        parent: string_at(&rec, "parent").filter(|p| p != CLI_PARENT),
        live: true,
        task: string_at(&rec, "task"),
        job_tail: job_tail(&id),
        parent_session: string_at(&rec, "parentSession").filter(|s| !s.is_empty()),
    }
}

/// A routed task in flight, as `dispatch.json` records it.
#[derive(Debug, Default, Clone)]
struct Dispatch {
    parent: Option<String>,
    live: bool,
    task: Option<String>,
    job_tail: Option<String>,
    /// The dispatching SESSION, not just the dispatching harness.
    ///
    /// A profile has many live sessions at once — that is the whole reason the
    /// per-pane attribution fix exists — so "parent = main" says which fleet
    /// member sent the job but not which of its panes. With this, a pane can
    /// show its own children instead of its siblings' (nephews).
    ///
    /// `None` for a job the fleet stamped before this field existed.
    parent_session: Option<String>,
}

/// The dispatching session recorded for a finished job.
///
/// `last-result.json` does not carry `parentSession`; the job record under
/// `.jobs/<id>.json` does. Looked up by the id `last-result.json` already
/// carries, so no new bookkeeping is needed and a missing job file simply reads
/// as unowned.
fn job_parent_session(root: &Path, job_id: &str) -> Option<String> {
    if job_id.is_empty() || job_id.contains('/') || job_id.contains("..") {
        return None;
    }
    let raw = std::fs::read(root.join(".jobs").join(format!("{job_id}.json"))).ok()?;
    let rec = serde_json::from_slice::<serde_json::Value>(&raw).ok()?;
    string_at(&rec, "parentSession").filter(|s| !s.is_empty())
}

/// The most recent finished job, as `last-result.json` records it.
#[derive(Debug, Default, Clone)]
struct LastResult {
    parent: Option<String>,
    event: Option<String>,
    task: Option<String>,
    job_tail: Option<String>,
    /// Full job id, used to find the job record that carries `parentSession`.
    job_id: Option<String>,
    /// When it finished. `None` means undatable, which the grace treats as too
    /// old to hold rather than as "hold forever".
    at: Option<SystemTime>,
}

/// Read the most recent finished job.
fn read_last_result(state: &Path) -> LastResult {
    let Ok(raw) = std::fs::read(state.join("last-result.json")) else {
        return LastResult::default();
    };
    let Ok(ev) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return LastResult::default();
    };
    // The file's own `t` is preferred over its mtime: mtime moves when anything
    // touches the file, and a copy or a restore would silently renew every
    // chip's grace. Falling back to mtime is still better than dropping the
    // chip outright when `t` is missing or unparseable.
    let finished_at = string_at(&ev, "t")
        .and_then(|stamp| parse_rfc3339(&stamp))
        .or_else(|| {
            std::fs::metadata(state.join("last-result.json"))
                .ok()
                .and_then(|meta| meta.modified().ok())
        });
    LastResult {
        parent: string_at(&ev, "parent").filter(|p| p != CLI_PARENT),
        event: string_at(&ev, "event"),
        task: string_at(&ev, "task"),
        job_tail: string_at(&ev, "id").and_then(|id| job_tail(&id)),
        job_id: string_at(&ev, "id"),
        at: finished_at,
    }
}

/// Parse the fleet's timestamps (`2026-08-14T09:05:30-05:00`) into a
/// `SystemTime`.
///
/// Hand-rolled rather than pulling `chrono` in: the collector ships in every
/// herdr install, the fleet writes exactly one shape, and a date crate for one
/// field is a dependency the plugin would carry forever. Anything that does not
/// match that shape returns `None`, which the caller treats as "no grace".
fn parse_rfc3339(stamp: &str) -> Option<SystemTime> {
    let bytes = stamp.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num =
        |range: std::ops::Range<usize>| -> Option<i64> { stamp.get(range)?.parse::<i64>().ok() };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Days from the civil epoch — Howard Hinnant's days_from_civil, which is
    // exact for every proleptic Gregorian date and avoids a calendar table.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let mut epoch = days * 86_400 + hour * 3_600 + minute * 60 + second;

    // Offset. `Z` (or an absent offset) is already UTC; otherwise subtract the
    // stated offset to get there. The fleet writes local time with an offset,
    // so ignoring this would mis-date every chip by the timezone.
    let rest = &stamp[19..];
    let rest = rest.strip_prefix(|c| c == '.').map_or(rest, |frac| {
        // Drop fractional seconds if present.
        let end = frac
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(frac.len());
        &frac[end..]
    });
    if let Some(sign) = rest.chars().next() {
        if sign == '+' || sign == '-' {
            let oh: i64 = rest.get(1..3)?.parse().ok()?;
            let om: i64 = rest.get(4..6).unwrap_or("0").parse().unwrap_or(0);
            let offset = oh * 3_600 + om * 60;
            epoch += if sign == '+' { -offset } else { offset };
        }
    }

    if epoch < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(epoch as u64))
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

    // One instant for the whole snapshot: see `Node::edge_to`.
    let now = SystemTime::now();
    let by_name: HashMap<&str, &Node> = nodes.iter().map(|n| (n.name.as_str(), n)).collect();

    // The session that decides which JOBS belong to this pane. The host's
    // focus event is the strongest claim, but herdr does not always detect an
    // agent session for a pane (measured: a supervised harness pane mid-
    // dispatch with none) — and with no session every session-stamped edge
    // fails `job_belongs_to_session` and the bar lies "no children" while a
    // job runs. A MANAGED harness has exactly one primary session and its own
    // hook records it in `state/session-id`, so that file is the fallback.
    // Profiles stay strict (session_ref only): they host many concurrent
    // sessions and last-writer-wins on that file is exactly the nephews bug
    // the per-pane attribution fixed.
    let focus_session: Option<String> = focus.session_ref.clone().or_else(|| {
        by_name
            .get(focus_name.as_str())
            .filter(|n| !n.profile)
            .and_then(|n| n.session_id.clone())
    });

    // Ancestors: walk up the spawn edges, root-most first. Cycle-guarded — a
    // malformed pair of records must not hang the dash tick.
    let mut ancestry: Vec<&Node> = Vec::new();
    let mut seen: Vec<&str> = vec![focus_name.as_str()];
    let mut cursor = by_name.get(focus_name.as_str()).copied();
    while let Some(node) = cursor {
        let Some(parent_name) = node.primary_parent(now) else {
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
    let children = children_of(&nodes, &focus_name, now, focus_session.as_deref());

    // Brothers: the OTHER children of the selected node's parent. They are what
    // makes the bar survive navigation — selecting one of them re-roots the bar
    // on it without emptying the row it was selected from.
    //
    // The session key is the selected node's OWN dispatching session, not the
    // focused pane's: `job_belongs_to_session` matches a job against the pane
    // that SENT it, and the pane that sent the selected node's job is the same
    // pane that sent its brothers'. Using the focused session here would filter
    // out every brother the moment the selection moved down a level, which is
    // the disappearing act this redesign exists to remove.
    let brothers: Vec<(&Node, EdgeKind)> = match ancestry.last() {
        Some(parent) => {
            let parent_name = parent.name.clone();
            let parent_session = by_name
                .get(focus_name.as_str())
                .and_then(|n| {
                    n.dispatch_session
                        .clone()
                        .or_else(|| n.history_session.clone())
                })
                // A selection attached by TEAM carries no session of its own;
                // fall back to the focused pane's, which is the only other
                // claim available and is correct when the parent IS the focused
                // profile's pane.
                .or_else(|| focus_session.clone());
            children_of(&nodes, &parent_name, now, parent_session.as_deref())
                .into_iter()
                .filter(|(n, _)| n.name != focus_name)
                .collect()
        }
        // No parent means no brothers — the selected node is the root of the
        // chain the bar can see.
        None => Vec::new(),
    };

    // Validate every tab against THIS instance before it becomes a click
    // target: `state/herdr-tab` is written by whichever herdr the harness was
    // opened in, so an unvalidated id focuses a stranger's tab or nothing.
    let live_tabs = api.as_ref().map(ApiClient::tab_ids).unwrap_or_default();
    let tab_by_workspace = api
        .as_ref()
        .map(|api| api.tabs_by_cwd(&live_tabs))
        .unwrap_or_default();
    let pane_titles = api
        .as_ref()
        .map(ApiClient::pane_titles)
        .unwrap_or_default();
    let codenames = load_codenames(&harness_root());

    // Collect all nodes that will be rendered to build session_codenames mapping
    let mut all_nodes_for_mapping: Vec<&Node> = Vec::new();
    all_nodes_for_mapping.extend(&ancestry);
    if let Some(node) = by_name.get(focus_name.as_str()).copied() {
        all_nodes_for_mapping.push(node);
    }
    for (node, _) in &brothers {
        all_nodes_for_mapping.push(node);
    }
    for (node, _) in &children {
        all_nodes_for_mapping.push(node);
    }
    let session_codenames = build_session_codenames(&all_nodes_for_mapping, &codenames);

    let mut chips: Vec<Chip> = Vec::new();
    for node in ancestry {
        chips.push(chip_for(
            node,
            ChipRole::Ancestor,
            None,
            &live_tabs,
            &tab_by_workspace,
            &agent_status,
            &pane_titles,
            &codenames,
            &session_codenames,
        ));
    }
    if let Some(node) = by_name.get(focus_name.as_str()).copied() {
        chips.push(chip_for(
            node,
            ChipRole::Current,
            None,
            &live_tabs,
            &tab_by_workspace,
            &agent_status,
            &pane_titles,
            &codenames,
            &session_codenames,
        ));
    }
    // Order is the render order and click handling addresses it by index:
    // lineage, the selected node, its brothers, then its children.
    for (role, band) in [
        (ChipRole::Sibling, brothers),
        (ChipRole::Child, children),
    ] {
        for (node, edge) in band {
            let chip = chip_for(
                node,
                role,
                Some(edge),
                &live_tabs,
                &tab_by_workspace,
                &agent_status,
                &pane_titles,
                &codenames,
                &session_codenames,
            );
            if chip_is_worth_a_slot(&chip, edge) {
                chips.push(chip);
            }
        }
    }

    input.empty_reason = None;
    input.chips = chips;
    input
}

/// The direct children of `parent`, in render order.
///
/// Shared by the two bands the bar draws — the selected node's children and its
/// brothers (which are its PARENT's children) — so the two can never disagree
/// about what counts as a child. A brother that only appeared under one of the
/// two rules would blink in and out as the selection moved between it and its
/// sibling, which is exactly the class of disappearance this bar must not have.
///
/// `session` is the pane whose JOBS count: a dispatch is stamped with the
/// session that sent it, and asking for `parent`'s children means asking about
/// the session that dispatched from `parent`, not about whichever pane happens
/// to be focused (see `Node::job_belongs_to_session`).
fn children_of<'a>(
    nodes: &'a [Node],
    parent: &str,
    now: SystemTime,
    session: Option<&str>,
) -> Vec<(&'a Node, EdgeKind)> {
    let mut out: Vec<(&Node, EdgeKind)> = nodes
        .iter()
        .filter(|n| n.name != parent)
        // A profile is an external config dir registered as a fleet member; it
        // has no managed process and nobody spawns it, so it can be an
        // orchestrator but never a child.
        .filter(|n| !n.profile)
        .filter_map(|n| n.edge_to(parent, now, session).map(|edge| (n, edge)))
        .collect();
    // Live work first, then declared members, then history; alphabetical inside
    // each band so the bar does not reshuffle under the cursor every tick.
    out.sort_by(|(a, ea), (b, eb)| {
        edge_rank(*ea)
            .cmp(&edge_rank(*eb))
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Whether a child has earned a slot on the bar.
///
/// The row is a live picture of what is happening around the orchestrator, not
/// a roster of everything affiliated with it. A fleet accumulates team members
/// — one operator's bar showed seven (`codex1`, `conductor`, `docs-writer`,
/// `jcode1`, `pi1`, `security-reviewer`, `test`) none of which had a pane open,
/// so the bar was mostly a list of things that were not there, and the chips
/// that mattered had to be found among them.
///
/// A chip renders when at least one of these is true:
///
///   * it has an open pane/tab IN THIS herdr session — something to navigate to;
///   * a dispatch is in flight — it is working right now, and a `light` child
///     running inside conductor has no pane of its own but is still live;
///   * it is inside the finished-dispatch retention grace (`DISPATCH_GRACE`) —
///     it ran recently enough to be worth reporting.
///
/// Mere team membership with no pane renders nothing. The pinned orchestrator
/// and its ancestors are not subject to this: "where am I" must always answer.
fn chip_is_worth_a_slot(chip: &Chip, edge: EdgeKind) -> bool {
    if chip.tab_id.is_some() {
        return true;
    }
    match edge {
        // In flight, or recently finished — both are events, and an event with
        // no pane is exactly what a `light` child looks like.
        EdgeKind::Dispatch | EdgeKind::History => true,
        // Affiliation alone, with nothing open: not news.
        EdgeKind::Team => false,
    }
}

fn edge_rank(edge: EdgeKind) -> u8 {
    match edge {
        EdgeKind::Dispatch => 0,
        EdgeKind::Team => 1,
        EdgeKind::History => 2,
    }
}

/// The fleet's codename ledger: pane id → the word it was given.
///
/// `$HARNESS_ROOT/.codenames.json`, written by `harness titlesync`
/// (harness-fleet, go/codename.go). Read rather than re-derived because it is
/// keyed by pane id and therefore exact — the name grammar can only recognise
/// an id that is wrapped around a title, and a pane whose runtime has not
/// produced one yet is called just "Hexagon", which by shape alone is
/// indistinguishable from a pane somebody labelled "Refactor".
///
/// An absent or unreadable ledger is not an error: the fleet may not be
/// running the daemon, and the parse path still recovers every name that
/// carries a title.
fn load_codenames(root: &Path) -> HashMap<String, String> {
    let Ok(raw) = std::fs::read(root.join(".codenames.json")) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return HashMap::new();
    };
    let Some(live) = value.get("live").and_then(|v| v.as_object()) else {
        return HashMap::new();
    };
    live.iter()
        .filter_map(|(pane, rec)| {
            let word = rec.get("codename")?.as_str()?;
            (!word.is_empty()).then(|| (pane.clone(), word.to_string()))
        })
        .collect()
}

/// Build a session_id -> codename mapping from pane_id -> codename.
/// Searches through all accessible nodes to find which session_ids have codenames.
fn build_session_codenames(
    nodes: &[&Node],
    pane_codenames: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut session_map: HashMap<String, String> = HashMap::new();

    for node in nodes {
        // Try to find a codename for this node by looking up all its known pane_ids
        if let Some(session_id) = &node.session_id {
            if !session_id.is_empty() && session_map.contains_key(session_id) {
                // Already have a codename for this session
                continue;
            }

            if let Some(pane_id) = &node.pane_id {
                if let Some(codename) = pane_codenames.get(pane_id) {
                    session_map.insert(session_id.clone(), codename.clone());
                }
            }
        }
    }

    session_map
}

fn chip_for(
    node: &Node,
    role: ChipRole,
    edge: Option<EdgeKind>,
    live_tabs: &[String],
    tab_by_workspace: &HashMap<PathBuf, String>,
    agent_status: &HashMap<String, AgentPane>,
    pane_titles: &HashMap<String, PaneTitle>,
    codenames: &HashMap<String, String>,
    session_codenames: &HashMap<String, String>,
) -> Chip {
    // Resolve the tab the chip navigates to, freshest source first:
    //
    //   1. the pane whose cwd IS this harness's workspace — costs nothing extra
    //      (pane.list already carries cwd) and cannot go stale;
    //   2. `state/herdr-tab`, when it still names a tab that exists here.
    //
    // The file alone is not enough. It is rewritten only by the fleet's
    // SessionStart hook, so every tab replacement invalidates it — including
    // one caused by moving a pane between workspaces, which is now an ordinary
    // operation. A bar in that state renders chips that refuse to navigate
    // ("no pane in this instance") for agents that are plainly on screen.
    let tab_id = node
        .workspace_dir
        .as_ref()
        .and_then(|dir| tab_by_workspace.get(dir))
        .cloned()
        .or_else(|| {
            node.tab_id
                .as_ref()
                .filter(|tab| live_tabs.iter().any(|t| t == *tab))
                .cloned()
        });
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
    // Instance name (titlesync): pane label first — it is the one a human set
    // on purpose — then the pane's own (stripped-of-spinner) terminal title,
    // falling back to the harness identity when the tab has neither, which is
    // also correct for a chip with no live pane at all.
    let title = tab_id.as_ref().and_then(|tab| pane_titles.get(tab));
    let display_name = title
        .and_then(|t| t.label.clone())
        .or_else(|| {
            title
                .and_then(|t| t.terminal_title_stripped.as_deref())
                .map(strip_spinner)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| node.name.clone());
    // Split the instance id off the front of the name. The LEDGER wins: it is
    // keyed by session id (durable across relaunches) and therefore exact,
    // while the parse is a shape rule that deliberately refuses anything
    // ambiguous (a bare "Hexagon" with no title reads as an ordinary label,
    // and would otherwise be lost).
    let parsed = codename::parse(&display_name);
    let codename = node
        .session_id
        .as_deref()
        .and_then(|sid| session_codenames.get(sid))
        .map(|word| codename::display(word))
        .or_else(|| {
            // Fallback to pane_id lookup for panes that don't have session_id
            pane_id
                .as_deref()
                .and_then(|pane| codenames.get(pane))
                .map(|word| codename::display(word))
        })
        .unwrap_or_else(|| parsed.codename.clone());
    // ...but the TITLE always comes from the parse: the ledger knows the id,
    // not the name it was wrapped around.
    let instance_title = if parsed.codename.is_empty() && !codename.is_empty() {
        // Ledger-confirmed id with an unparseable name — e.g. a pane still
        // called just "Hexagon", before its runtime has produced a title.
        // There is no real name yet, and inventing one from the id would make
        // the tooltip echo the chip.
        if display_name == codename {
            String::new()
        } else {
            display_name.clone()
        }
    } else {
        parsed.title.clone()
    };
    Chip {
        name: node.name.clone(),
        display_name,
        codename,
        instance_title,
        role,
        status: status_for(node, agent.and_then(|a| a.status.as_deref())),
        edge,
        tab_id,
        pane_id,
        session_id: node.session_id.clone(),
        // Nothing is hidden until the host's create-hidden work lands; the
        // field exists now so phase 2 is a collector change, not a protocol one.
        hidden: false,
        // Greyed on the bar: the job is over, the chip is being held through
        // its grace so a seven-second dispatch is still visible afterwards.
        // A team child is never `finished` — its edge is an affiliation, not a
        // job, so it has nothing to have finished.
        finished: edge == Some(EdgeKind::History),
        task: node.task.clone(),
        job_tail: node.job_tail.clone(),
        exec: node.exec.clone(),
        runtime: node.runtime.clone(),
        folder: node
            .workspace_dir
            .as_ref()
            .map(|dir| dir.display().to_string()),
    }
}

/// Strip a leading agent status glyph — a spinner frame or state dot Claude
/// Code and friends prepend to their own pane title (e.g. "◑ Doing work") —
/// so the bar shows the operator-meaningful part of the title, not a symbol
/// that already has its own status dot drawn on the chip.
///
/// A spinner glyph is a single non-alphanumeric, non-ASCII character standing
/// alone before the first space; herdr's own `terminal_title_stripped` only
/// removes ANSI/OSC wrapping, so this symbol survives into it untouched.
fn strip_spinner(title: &str) -> &str {
    let trimmed = title.trim_start();
    let mut chars = trimmed.char_indices();
    let Some((_, first)) = chars.next() else {
        return trimmed;
    };
    if first.is_ascii() || first.is_alphanumeric() {
        return trimmed;
    }
    match chars.next() {
        Some((idx, c)) if c.is_whitespace() => trimmed[idx..].trim_start(),
        _ => trimmed,
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
fn attribute(focus: &Focus, nodes: &[Node], api: Option<&ApiClient>) -> Option<(String, String)> {
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

    // 3. Resolve a PROFILE from the focused pane's own session, by asking
    //    whether that session's transcript lives under the profile's config
    //    dir.
    //
    //    This is per-pane on purpose. The obvious check — comparing the focused
    //    session against `<profile>/state/session-id` — is last-writer-wins
    //    across every concurrent session of that profile, and a profile is
    //    precisely the fleet member that has many at once: it is the human's
    //    own config root (`~/.claude`), and every agent pane the operator opens
    //    on it rewrites that one file. Measured during this fix: three live
    //    sessions under `~/.claude`, the file naming exactly one of them (and
    //    not the operator's), so the operator's own orchestrator pane attributed
    //    to nothing and the bar declared it `not_tidus`. `state/herdr-tab` is
    //    stale for the same reason.
    //
    //    The fleet already resolves it correctly and this mirrors it:
    //    `profileAgent` (go/tidus.go) walks the herdr agent registry and keeps
    //    the agent whose `agent_session.value` has a transcript under the
    //    profile, via `findSession`. Here the focused pane IS the agent, so the
    //    same question is asked of one session instead of all of them.
    //
    //    Consequence, and the point of the fix: EVERY live session of a profile
    //    resolves to that profile, so each is its own orchestrator root. Two
    //    operator panes on `~/.claude` are two roots, not one root and one
    //    orphan.
    if let Some(session) = focus.session_ref.as_deref() {
        for node in nodes.iter().filter(|n| n.profile) {
            let Some(dir) = node.config_dir.as_deref() else {
                continue;
            };
            if profile_owns_session(dir, session) {
                return Some((node.name.clone(), format!("{}'s transcript", node.name)));
            }
        }
    }

    // 4. tidus's own record of where it put the pane. Last, because it is not
    //    updated when a pane is replaced and may name another instance's pane.
    if let Some(node) = nodes.iter().find(|n| n.pane_id.as_deref() == Some(pane_id)) {
        return Some((node.name.clone(), "state/herdr-pane".to_string()));
    }
    None
}

/// Whether `session` has a transcript under this config dir.
///
/// Mirrors the fleet's `findSession`, but shaped for a per-tick collector:
/// `findSession` walks the whole `projects` tree, while this stats one candidate
/// path per project directory. Measured on the operator's root — 10 project
/// dirs — at ~16 ms for the walk, so the cheaper shape is a margin rather than
/// a necessity, and it stays a margin as the tree grows.
///
/// Both on-disk layouts are checked because a profile may point at either
/// runtime's root:
///
///   claude: `<config>/projects/<encoded-cwd>/<session>.jsonl`
///   codex:  `<config>/sessions/<yyyy>/<mm>/<dd>/rollout-<ts>-<session>.jsonl`
///
/// pi and jcode are deliberately absent: pi is not config-isolated by env and
/// has no id-addressable session list, so there is nothing here to match. A
/// profile on one of those falls through to the later mechanisms rather than
/// being answered wrongly.
fn profile_owns_session(config_dir: &Path, session: &str) -> bool {
    // A session ref that is itself a path (pi) is not a claude/codex id; the
    // component check keeps such a value from being pasted into a path.
    if session.is_empty() || session.contains('/') || session.contains("..") {
        return false;
    }

    let projects = config_dir.join("projects");
    if let Ok(entries) = std::fs::read_dir(&projects) {
        let needle = format!("{session}.jsonl");
        for entry in entries.flatten() {
            if entry.path().join(&needle).exists() {
                return true;
            }
        }
    }

    // codex: the id is the filename's trailing uuid, so the date directories
    // have to be walked. Bounded by the layout (year/month/day), not by the
    // number of sessions.
    let sessions = config_dir.join("sessions");
    let Ok(years) = std::fs::read_dir(&sessions) else {
        return false;
    };
    for year in years.flatten() {
        let Ok(months) = std::fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.flatten() {
            let Ok(days) = std::fs::read_dir(month.path()) else {
                continue;
            };
            for day in days.flatten() {
                let Ok(files) = std::fs::read_dir(day.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let name = file.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if name.starts_with("rollout-") && name.ends_with(&format!("-{session}.jsonl"))
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
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

    /// `canonical cwd → tab id` for every live pane, so a harness can find its
    /// tab by the directory it runs in. Free: `pane.list` is already fetched
    /// for nothing else here, and cwd rides along on every row.
    fn tabs_by_cwd(&self, live_tabs: &[String]) -> HashMap<PathBuf, String> {
        let mut out = HashMap::new();
        let Some(result) = self.request("pane.list", serde_json::json!({})) else {
            return out;
        };
        let Some(panes) = result.get("panes").and_then(serde_json::Value::as_array) else {
            return out;
        };
        for pane in panes {
            let (Some(cwd), Some(tab)) = (string_at(pane, "cwd"), string_at(pane, "tab_id")) else {
                continue;
            };
            if !live_tabs.contains(&tab) {
                continue;
            }
            if let Ok(dir) = std::fs::canonicalize(&cwd) {
                out.entry(dir).or_insert(tab);
            }
        }
        out
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

    /// `tab_id → label/title` for every live pane — the titlesync inputs
    /// (harness-fleet@bcfd73f/51b3323 keeps pane label = tab name = session
    /// title in sync), so the bar can show the instance's own name instead of
    /// `<harness>:<jobid>`.
    fn pane_titles(&self) -> HashMap<String, PaneTitle> {
        let mut out = HashMap::new();
        let Some(result) = self.request("pane.list", serde_json::json!({})) else {
            return out;
        };
        let Some(panes) = result.get("panes").and_then(serde_json::Value::as_array) else {
            return out;
        };
        for pane in panes {
            let Some(tab) = string_at(pane, "tab_id") else {
                continue;
            };
            out.insert(
                tab,
                PaneTitle {
                    label: string_at(pane, "label").filter(|s| !s.trim().is_empty()),
                    terminal_title_stripped: string_at(pane, "terminal_title_stripped")
                        .filter(|s| !s.trim().is_empty()),
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

    /// The brother band and the child band come from ONE rule, so a chip
    /// cannot qualify as a child of its parent yet fail to qualify as a
    /// brother of its sibling — which is what would make it blink in and out
    /// as the selection moves between the two.
    #[test]
    fn brothers_are_just_the_parents_children_minus_the_selected_one() {
        let now = SystemTime::now();
        let mut kid_a = node("kid-a");
        kid_a.dispatch_parent = Some("boss".into());
        kid_a.dispatch_session = Some("sess-1".into());
        let mut kid_b = node("kid-b");
        kid_b.dispatch_parent = Some("boss".into());
        kid_b.dispatch_session = Some("sess-1".into());
        let mut grandkid = node("grandkid");
        grandkid.dispatch_parent = Some("kid-a".into());
        grandkid.dispatch_session = Some("sess-2".into());
        let nodes = vec![node("boss"), kid_a, kid_b, grandkid];

        // Looking at the parent: both children, no brothers of its own.
        let names = |v: Vec<(&Node, EdgeKind)>| {
            v.into_iter().map(|(n, _)| n.name.clone()).collect::<Vec<_>>()
        };
        assert_eq!(
            names(children_of(&nodes, "boss", now, Some("sess-1"))),
            vec!["kid-a", "kid-b"]
        );

        // Now select kid-a. Its brothers are its parent's children minus
        // itself — keyed by the session that DISPATCHED kid-a, not by whatever
        // session kid-a's own pane runs. kid-b must survive the move.
        let brothers: Vec<String> = names(children_of(&nodes, "boss", now, Some("sess-1")))
            .into_iter()
            .filter(|n| n != "kid-a")
            .collect();
        assert_eq!(brothers, vec!["kid-b"], "the brother must not disappear");

        // ...and kid-a's own children are a separate, deeper band.
        assert_eq!(
            names(children_of(&nodes, "kid-a", now, Some("sess-2"))),
            vec!["grandkid"]
        );
    }

    /// Keying the brother query on the FOCUSED pane's session instead of the
    /// dispatching one is the exact mistake that empties the row: a child's own
    /// session never matches the job its parent sent to its brother.
    #[test]
    fn the_brother_query_is_keyed_on_the_dispatching_session() {
        let now = SystemTime::now();
        let mut kid_b = node("kid-b");
        kid_b.dispatch_parent = Some("boss".into());
        kid_b.dispatch_session = Some("boss-sess".into());
        let nodes = vec![node("boss"), kid_b];

        assert!(
            children_of(&nodes, "boss", now, Some("boss-sess")).len() == 1,
            "the dispatching session sees the job"
        );
        assert!(
            children_of(&nodes, "boss", now, Some("some-childs-own-session")).is_empty(),
            "an unrelated session must not claim another pane's dispatch"
        );
    }

    fn node(name: &str) -> Node {
        Node {
            name: name.to_string(),
            runtime: Some("claude".into()),
            exec: None,
            config_dir: None,
            profile: false,
            team: None,
            dispatch_parent: None,
            dispatch_session: None,
            history_parent: None,
            history_session: None,
            // Fresh by default so edge tests exercise edge PRECEDENCE without
            // every one of them also having to opt into the grace window.
            history_at: Some(SystemTime::now()),
            live: false,
            task: None,
            job_tail: None,
            last_event: None,
            workspace_dir: None,
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
        let now = SystemTime::now();
        let mut n = node("kid");
        n.history_parent = Some("old".into());
        assert_eq!(n.edge_to("old", now, None), Some(EdgeKind::History));
        assert_eq!(n.primary_parent(now), Some("old"));

        n.team = Some("boss".into());
        assert_eq!(n.edge_to("boss", now, None), Some(EdgeKind::Team));
        assert_eq!(n.primary_parent(now), Some("boss"));

        n.dispatch_parent = Some("now".into());
        assert_eq!(n.edge_to("now", now, None), Some(EdgeKind::Dispatch));
        assert_eq!(n.primary_parent(now), Some("now"));
        // The weaker edges still resolve for their own parents — a child can be
        // on one parent's bar by team while serving a dispatch from another.
        assert_eq!(n.edge_to("boss", now, None), Some(EdgeKind::Team));
        assert_eq!(n.edge_to("stranger", now, None), None);
    }

    // ── #51: per-pane profile resolution ────────────────────────────────────

    /// Build a profile whose config dir holds transcripts for `sessions`.
    fn profile_with_sessions(tag: &str, sessions: &[&str]) -> (Node, PathBuf) {
        let dir = tempdir(tag);
        let projects = dir.join("projects").join("-home-andual-Projects");
        std::fs::create_dir_all(&projects).unwrap();
        for session in sessions {
            std::fs::write(projects.join(format!("{session}.jsonl")), b"{}").unwrap();
        }
        let mut node = node("main");
        node.profile = true;
        node.config_dir = Some(dir.clone());
        (node, dir)
    }

    /// EVERY live session of a profile is its own orchestrator root.
    ///
    /// The bug this pins: profile attribution used to compare the focused
    /// session against the single `<profile>/state/session-id` file, which is
    /// last-writer-wins across every concurrent session of that profile. A
    /// profile IS the fleet member with many at once — it is the human's own
    /// config root — so the operator's orchestrator pane attributed to nothing
    /// and the bar declared it `not_tidus`. Reproduced live: three sessions
    /// under `~/.claude`, the file naming one of them, and not the operator's.
    #[test]
    fn every_concurrent_session_of_a_profile_resolves_to_it() {
        let operator = "e26394d9-3bc6-4a23-b69f-814d3c27214b";
        let other = "a22b6837-411e-4597-8156-22434bd4652a";
        let third = "ec160211-1b5e-4902-9f54-617e160081f6";
        let (node, _dir) = profile_with_sessions("profile-concurrent", &[operator, other, third]);
        let nodes = vec![node];

        for session in [operator, other, third] {
            let focus = Focus {
                pane_id: Some("w1:p1".into()),
                session_ref: Some(session.to_string()),
                is_self_focus: false,
            };
            let resolved = attribute(&focus, &nodes, None);
            assert_eq!(
                resolved.as_ref().map(|(name, _)| name.as_str()),
                Some("main"),
                "session {session} must resolve to the profile that owns its transcript"
            );
        }
    }

    /// A session with no transcript under the profile is not that profile.
    /// Without this the fix would attribute every pane in the instance to the
    /// first profile it found.
    #[test]
    fn a_foreign_session_does_not_resolve_to_the_profile() {
        let (node, _dir) =
            profile_with_sessions("profile-foreign", &["aaaaaaaa-0000-0000-0000-000000000000"]);
        let focus = Focus {
            pane_id: Some("w1:p1".into()),
            session_ref: Some("bbbbbbbb-1111-1111-1111-111111111111".into()),
            is_self_focus: false,
        };
        assert!(attribute(&focus, &[node], None).is_none());
    }

    /// The stale `state/session-id` file must have no say at all.
    ///
    /// It is left holding another pane's session on purpose: if the old
    /// mechanism were still consulted, this focus would resolve through it.
    #[test]
    fn the_single_session_id_file_is_no_longer_consulted() {
        let operator = "e26394d9-3bc6-4a23-b69f-814d3c27214b";
        let (node, dir) = profile_with_sessions("profile-stale", &[operator]);
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::write(
            dir.join("state").join("session-id"),
            b"a22b6837-411e-4597-8156-22434bd4652a\n",
        )
        .unwrap();

        // The pane whose session the file names has NO transcript here, so the
        // only way it could resolve is through the file.
        let focus = Focus {
            pane_id: Some("w1:p1".into()),
            session_ref: Some("a22b6837-411e-4597-8156-22434bd4652a".into()),
            is_self_focus: false,
        };
        assert!(
            attribute(&focus, &[node], None).is_none(),
            "the stale file must not attribute a pane whose transcript is elsewhere"
        );
    }

    /// codex profiles keep their rollout layout.
    #[test]
    fn a_codex_rollout_resolves_the_profile() {
        let dir = tempdir("profile-codex");
        let day = dir.join("sessions").join("2026").join("08").join("13");
        std::fs::create_dir_all(&day).unwrap();
        let session = "019ffb15-1111-2222-3333-444455556666";
        std::fs::write(
            day.join(format!("rollout-2026-08-13T07-25-44-{session}.jsonl")),
            b"{}",
        )
        .unwrap();
        assert!(profile_owns_session(&dir, session));
        assert!(!profile_owns_session(
            &dir,
            "019ffb15-0000-0000-0000-000000000000"
        ));
    }

    /// A pi session ref is a PATH, not an id. Pasting it into a path join would
    /// escape the config dir, so it is refused before any filesystem call.
    #[test]
    fn a_path_shaped_session_ref_is_refused() {
        let dir = tempdir("profile-path");
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        assert!(!profile_owns_session(
            &dir,
            "/home/andual/.pi/agent/sessions/x.jsonl"
        ));
        assert!(!profile_owns_session(&dir, "../../etc/passwd"));
        assert!(!profile_owns_session(&dir, ""));
    }

    // ── #51: dispatch retention grace ───────────────────────────────────────

    /// A finished dispatch keeps its chip through the grace window, then drops.
    ///
    /// The operator scenario: a `codex-gp` dispatch answers in about seven
    /// seconds with no team edge to fall back on, so without a grace the chip
    /// appears and vanishes between two glances and the bar reports that
    /// nothing happened at exactly the moment something did.
    #[test]
    fn a_finished_dispatch_is_held_for_the_grace_window() {
        let now = SystemTime::now();
        let mut child = node("codex-gp");
        child.history_parent = Some("main".into());

        child.history_at = Some(now - Duration::from_secs(7));
        assert_eq!(
            child.edge_to("main", now, None),
            Some(EdgeKind::History),
            "a job that just finished is still on the bar"
        );

        child.history_at = Some(now - DISPATCH_GRACE + Duration::from_secs(30));
        assert_eq!(
            child.edge_to("main", now, None),
            Some(EdgeKind::History),
            "and stays until the window is actually up"
        );

        child.history_at = Some(now - DISPATCH_GRACE - Duration::from_secs(1));
        assert_eq!(
            child.edge_to("main", now, None),
            None,
            "past the window it drops off"
        );
    }

    /// A TEAM child is not subject to the grace: team is a declared, persistent
    /// affiliation rather than a job, so it has nothing to have finished.
    #[test]
    fn a_team_child_outlives_the_grace_window() {
        let now = SystemTime::now();
        let mut child = node("member");
        child.team = Some("main".into());
        child.history_parent = Some("main".into());
        child.history_at = Some(now - DISPATCH_GRACE - Duration::from_secs(3600));

        assert_eq!(
            child.edge_to("main", now, None),
            Some(EdgeKind::Team),
            "team keeps the chip long after any job of its own expired"
        );
        assert_eq!(child.primary_parent(now), Some("main"));
    }

    /// An undatable result must not pin a chip to the bar forever.
    #[test]
    fn a_result_with_no_timestamp_is_not_held() {
        let now = SystemTime::now();
        let mut child = node("codex-gp");
        child.history_parent = Some("main".into());
        child.history_at = None;
        assert_eq!(child.edge_to("main", now, None), None);
        assert_eq!(child.primary_parent(now), None);
    }

    /// A result stamped in the future is clock skew, not an ancient job.
    #[test]
    fn a_future_timestamp_counts_as_fresh() {
        let now = SystemTime::now();
        let mut child = node("codex-gp");
        child.history_parent = Some("main".into());
        child.history_at = Some(now + Duration::from_secs(600));
        assert_eq!(child.edge_to("main", now, None), Some(EdgeKind::History));
    }

    #[test]
    fn rfc3339_parsing_matches_the_fleets_stamps() {
        // The exact shape last-result.json writes, with a negative offset.
        let parsed = parse_rfc3339("2026-08-14T09:05:30-05:00").expect("parses");
        let epoch = parsed.duration_since(UNIX_EPOCH).unwrap().as_secs();
        // 2026-08-14T14:05:30Z, cross-checked against `date -u -d` rather than
        // computed by hand — the first version of this constant was a day out,
        // and a wrong expectation here would have been read as a parser bug.
        assert_eq!(epoch, 1_786_716_330);

        // Z and a positive offset land where they should relative to it.
        let utc = parse_rfc3339("2026-08-14T14:05:30Z").expect("parses");
        assert_eq!(utc.duration_since(UNIX_EPOCH).unwrap().as_secs(), epoch);
        let plus = parse_rfc3339("2026-08-14T16:05:30+02:00").expect("parses");
        assert_eq!(plus.duration_since(UNIX_EPOCH).unwrap().as_secs(), epoch);

        for bad in ["", "nonsense", "2026-13-01T00:00:00Z", "2026-08-14"] {
            assert!(parse_rfc3339(bad).is_none(), "{bad:?} must not parse");
        }
    }

    // ── #51: a chip must earn its slot ──────────────────────────────────────

    fn slot_chip(tab: Option<&str>) -> Chip {
        Chip {
            name: "member".into(),
            display_name: "member".into(),
            role: ChipRole::Child,
            status: ChipStatus::Idle,
            edge: None,
            tab_id: tab.map(str::to_string),
            pane_id: None,
            session_id: None,
            hidden: false,
            finished: false,
            task: None,
            job_tail: None,
            exec: None,
            runtime: Some("claude".into()),
            folder: None,
            codename: String::new(),
            instance_title: String::new(),
        }
    }

    /// Team membership alone does not render. One operator bar carried seven
    /// such chips — none with a pane open — so the row was mostly a list of
    /// things that were not there.
    #[test]
    fn a_team_member_with_no_pane_gets_no_slot() {
        assert!(!chip_is_worth_a_slot(&slot_chip(None), EdgeKind::Team));
        assert!(
            chip_is_worth_a_slot(&slot_chip(Some("w1:t9")), EdgeKind::Team),
            "the same member WITH a pane open is worth showing"
        );
    }

    /// A live dispatch and a recently finished one always render, pane or not:
    /// a `light` child runs inside conductor and never has a pane of its own.
    #[test]
    fn an_event_earns_a_slot_without_a_pane() {
        assert!(chip_is_worth_a_slot(&slot_chip(None), EdgeKind::Dispatch));
        assert!(chip_is_worth_a_slot(&slot_chip(None), EdgeKind::History));
    }

    // ── #51 phase 2.5: children, not nephews ────────────────────────────────

    /// A dispatch belongs to the SESSION that sent it, not merely to the
    /// harness. A profile has many live sessions at once, so without this every
    /// pane of a profile showed every other pane's children.
    #[test]
    fn a_dispatch_shows_only_on_the_pane_that_sent_it() {
        let now = SystemTime::now();
        let mine = "9def60d0-8cb5-47d8-a9e0-86b8471e8ebb";
        let theirs = "e26394d9-3bc6-4a23-b69f-814d3c27214b";

        let mut child = node("codex1");
        child.dispatch_parent = Some("main".into());
        child.dispatch_session = Some(mine.into());

        assert_eq!(
            child.edge_to("main", now, Some(mine)),
            Some(EdgeKind::Dispatch),
            "the pane that dispatched it sees it"
        );
        assert_eq!(
            child.edge_to("main", now, Some(theirs)),
            None,
            "a sibling pane of the same profile must NOT see it"
        );
        assert_eq!(
            child.edge_to("main", now, None),
            None,
            "and neither does a pane with no session of its own"
        );
    }

    /// The same rule applies to a finished job held by the grace: it stays on
    /// the bar of the pane that sent it, and only that one.
    #[test]
    fn a_finished_job_is_held_only_on_its_own_pane() {
        let now = SystemTime::now();
        let mine = "9def60d0-8cb5-47d8-a9e0-86b8471e8ebb";
        let theirs = "e26394d9-3bc6-4a23-b69f-814d3c27214b";

        let mut child = node("codex-gp");
        child.history_parent = Some("main".into());
        child.history_at = Some(now - Duration::from_secs(7));
        child.history_session = Some(mine.into());

        assert_eq!(
            child.edge_to("main", now, Some(mine)),
            Some(EdgeKind::History)
        );
        assert_eq!(child.edge_to("main", now, Some(theirs)), None);
    }

    /// Work the fleet stamped before `parentSession` existed has no session to
    /// match, so it shows EVERYWHERE. Hiding it would drop legacy jobs from
    /// every bar rather than from the wrong one.
    #[test]
    fn unowned_legacy_work_shows_on_every_pane() {
        let now = SystemTime::now();
        let mut child = node("legacy");
        child.dispatch_parent = Some("main".into());
        child.dispatch_session = None;

        for session in [Some("aaaa"), Some("bbbb"), None] {
            assert_eq!(
                child.edge_to("main", now, session),
                Some(EdgeKind::Dispatch),
                "unowned work is visible from {session:?}"
            );
        }
    }

    /// A TEAM edge is profile-level and belongs to every session of that
    /// profile — that is what makes it different from a job.
    #[test]
    fn team_edges_are_not_filtered_by_session() {
        let now = SystemTime::now();
        let mut member = node("member");
        member.team = Some("main".into());

        for session in [Some("aaaa"), Some("bbbb"), None] {
            assert_eq!(
                member.edge_to("main", now, session),
                Some(EdgeKind::Team),
                "team membership is visible from {session:?}"
            );
        }
    }

    /// The dispatching session is read off the job record the fleet writes.
    #[test]
    fn the_job_record_supplies_the_owning_session() {
        let root = tempdir("jobs-root");
        std::fs::create_dir_all(root.join(".jobs")).unwrap();
        std::fs::write(
            root.join(".jobs").join("main-1786719179313009317.json"),
            br#"{"id":"main-1786719179313009317","parent":"main","child":"codex1","parentSession":"9def60d0-8cb5-47d8-a9e0-86b8471e8ebb"}"#,
        )
        .unwrap();

        assert_eq!(
            job_parent_session(&root, "main-1786719179313009317").as_deref(),
            Some("9def60d0-8cb5-47d8-a9e0-86b8471e8ebb")
        );
        // A legacy record without the field, and a missing one, both read as
        // unowned rather than as an error.
        std::fs::write(root.join(".jobs").join("old.json"), br#"{"id":"old"}"#).unwrap();
        assert_eq!(job_parent_session(&root, "old"), None);
        assert_eq!(job_parent_session(&root, "absent"), None);
        // A job id is pasted into a path, so it is refused if it could escape.
        assert_eq!(job_parent_session(&root, "../../etc/passwd"), None);
    }

    #[test]
    fn cli_sentinel_is_not_a_parent() {
        let dir = tempdir("cli");
        std::fs::write(
            dir.join("dispatch.json"),
            br#"{"id":"j1","parent":"(cli)","child":"kid","task":"t"}"#,
        )
        .unwrap();
        let d = read_dispatch(&dir);
        assert_eq!(d.parent, None, "(cli) is a sentinel, not a harness");
        assert!(
            d.live,
            "an in-flight dispatch is live even without a parent"
        );
        assert_eq!(d.task.as_deref(), Some("t"));
        assert_eq!(d.job_tail.as_deref(), Some("j1"));
        assert_eq!(d.parent_session, None, "an unstamped dispatch is unowned");
    }

    #[test]
    fn a_half_written_dispatch_is_not_a_job() {
        let dir = tempdir("half");
        // claimDispatch creates the file O_EXCL and only then writes it.
        std::fs::write(dir.join("dispatch.json"), br#"{"parent":"p"}"#).unwrap();
        let d = read_dispatch(&dir);
        assert_eq!(d.parent, None);
        assert!(!d.live);
    }

    #[test]
    fn last_result_carries_parent_outcome_and_job_tail() {
        let dir = tempdir("last");
        std::fs::write(
            dir.join("last-result.json"),
            br#"{"id":"main-1786360945768895251","parent":"boss","event":"failed","task":"t"}"#,
        )
        .unwrap();
        let last = read_last_result(&dir);
        assert_eq!(last.parent.as_deref(), Some("boss"));
        assert_eq!(last.event.as_deref(), Some("failed"));
        assert_eq!(last.task.as_deref(), Some("t"));
        assert_eq!(last.job_tail.as_deref(), Some("895251"));
        // No `t` in this fixture, so the mtime fallback answers — the point is
        // that SOMETHING dates the result, or the chip could never be held.
        assert!(
            last.at.is_some(),
            "a result with no t must still be datable"
        );
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
            &HashMap::new(),
            &agents,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
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
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
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
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(chip.tab_id.as_deref(), Some("w9:tZ"));
        assert!(chip.clickable());
    }

    #[test]
    fn a_chip_finds_its_tab_by_workspace_when_the_state_file_is_stale() {
        // `state/herdr-tab` names a tab that no longer exists — the ordinary
        // outcome of any tab replacement, including moving the pane to another
        // workspace. Before this, the chip rendered as "no pane in this
        // instance" and refused to navigate for an agent plainly on screen.
        let mut n = node("kid");
        n.tab_id = Some("w1:tGONE".into());
        n.workspace_dir = Some(PathBuf::from("/harnesses/kid/workspace"));

        let mut by_cwd = HashMap::new();
        by_cwd.insert(
            PathBuf::from("/harnesses/kid/workspace"),
            "w1:tLIVE".to_string(),
        );

        let chip = chip_for(
            &n,
            ChipRole::Child,
            Some(EdgeKind::Team),
            &["w1:tLIVE".to_string()],
            &by_cwd,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(chip.tab_id.as_deref(), Some("w1:tLIVE"));
        assert!(chip.clickable(), "the chip must navigate again");
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

    // ── CODENAME: the id on the chip, the name in the tooltip ───────────────

    /// Build a chip for a pane whose synced label is `label`, optionally with
    /// a ledger entry for its pane id — the two sources the split resolves
    /// against, and nothing else.
    fn named_chip(label: &str, ledger: &[(&str, &str)]) -> Chip {
        let mut n = node("kid");
        n.tab_id = Some("w1:t9".into());
        let mut agents = HashMap::new();
        agents.insert(
            "w1:t9".to_string(),
            AgentPane {
                pane_id: Some("w1:p9".into()),
                status: Some("idle".into()),
            },
        );
        let mut titles = HashMap::new();
        titles.insert(
            "w1:t9".to_string(),
            PaneTitle {
                label: Some(label.to_string()),
                terminal_title_stripped: None,
            },
        );
        let codenames: HashMap<String, String> = ledger
            .iter()
            .map(|(p, w)| (p.to_string(), w.to_string()))
            .collect();
        chip_for(
            &n,
            ChipRole::Child,
            Some(EdgeKind::Team),
            &["w1:t9".to_string()],
            &HashMap::new(),
            &agents,
            &titles,
            &codenames,
            &HashMap::new(),
        )
    }

    /// The spec's own example, end to end through the collector: the chip
    /// carries the BARE codename (no "Hexagon >" lineage, no title) and the
    /// real name is what hover will show.
    #[test]
    fn a_decorated_label_splits_into_the_id_and_the_real_name() {
        let chip = named_chip("Hexagon > Bulbasur (add tab to context menu)", &[]);
        assert_eq!(chip.codename, "Bulbasur");
        assert_eq!(chip.instance_title, "add tab to context menu");
        assert_eq!(chip.label(), "Bulbasur", "the chip draws the id alone");
        assert_eq!(chip.detail(), "add tab to context menu");
        // display_name is untouched: everything else on the bar (Copy
        // metadata, the collector's own logs) still sees the full name.
        assert_eq!(chip.display_name, "Hexagon > Bulbasur (add tab to context menu)");
    }

    /// The ledger is keyed by pane id and therefore exact; the parse is a
    /// shape rule. When they disagree the ledger wins.
    #[test]
    fn the_ledger_wins_over_the_parse() {
        let chip = named_chip("Hexagon (a task)", &[("w1:p9", "bulbasur")]);
        assert_eq!(chip.codename, "Bulbasur");
        assert_eq!(chip.instance_title, "a task");
    }

    /// A pane the fleet has named but whose runtime has not produced a title
    /// yet is called just "Hexagon" — which the grammar refuses on purpose.
    /// The ledger recovers the id, and the tooltip stays empty rather than
    /// echoing the chip back at the operator.
    #[test]
    fn a_bare_codename_is_recovered_from_the_ledger_with_no_title() {
        let chip = named_chip("Hexagon", &[("w1:p9", "hexagon")]);
        assert_eq!(chip.codename, "Hexagon");
        assert_eq!(chip.instance_title, "");
        assert_eq!(chip.detail(), "kid", "falls back to the harness identity");
    }

    /// An ordinary label is never mistaken for an id. This is the failure that
    /// would put a task description on the chip and the instance id in the
    /// tooltip — exactly backwards — and it has no error message.
    #[test]
    fn an_ordinary_label_yields_no_codename() {
        let chip = named_chip("Fix the parser (again)", &[]);
        assert_eq!(chip.codename, "");
        assert_eq!(chip.instance_title, "Fix the parser (again)");
        assert_eq!(chip.label(), "Fix the parser (again)");
    }

    /// The ledger reader tolerates every shape a missing or half-written file
    /// can have: the bar must degrade to the parse, never to a panic.
    #[test]
    fn a_missing_or_malformed_ledger_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("tidus-cn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp");
        assert!(load_codenames(&dir).is_empty(), "absent file");
        for body in ["{not json", "{}", r#"{"live":[]}"#, r#"{"live":{"w1:p1":{}}}"#] {
            std::fs::write(dir.join(".codenames.json"), body).expect("write");
            assert!(load_codenames(&dir).is_empty(), "body {body:?}");
        }
        std::fs::write(
            dir.join(".codenames.json"),
            r#"{"version":1,"live":{"w1:p1":{"codename":"hexagon","harness":"h"}}}"#,
        )
        .expect("write");
        assert_eq!(
            load_codenames(&dir).get("w1:p1").map(String::as_str),
            Some("hexagon")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── LUNA-VISIBILITY: recursion at any depth ─────────────────────────────

    /// A pool worker under an orchestrator under the profile — the shape the
    /// operator asked for, three deep: `Orbiter > Darknet > Hexagon`.
    ///
    /// The bar re-roots on whatever is FOCUSED, so "the selected chip's
    /// children row lists its own subchildren, at any depth" is really two
    /// claims about the same node set, and both are pinned here: asking for a
    /// MIDDLE node's children returns its own children (not the root's), and
    /// walking up from the LEAF yields the whole chain in root-most order.
    ///
    /// This is a regression test, not a new feature: the recursion was always
    /// right. What was missing was the panes — pool workers ran headless, so
    /// there were no chips to recurse INTO (harness-fleet, isWorkerSlot).
    #[test]
    fn a_three_deep_chain_recurses_from_either_end() {
        let now = SystemTime::now();
        let mut root = node("main");
        root.profile = true; // the profile is an orchestrator, never a child
        let mut mid = node("task-handler");
        mid.team = Some("main".into());
        let mut leaf = node("luna-probe-w1");
        leaf.dispatch_parent = Some("task-handler".into());
        leaf.dispatch_session = Some("sid-mid".into());
        let nodes = vec![root, mid, leaf];

        // The MIDDLE node's children are its own — the root's are not mixed in,
        // and the leaf is not skipped over as a grandchild of the root.
        let kids = children_of(&nodes, "task-handler", now, Some("sid-mid"));
        assert_eq!(
            kids.iter().map(|(n, _)| n.name.as_str()).collect::<Vec<_>>(),
            vec!["luna-probe-w1"],
            "the selected node lists ITS OWN children"
        );
        assert_eq!(kids[0].1, EdgeKind::Dispatch);
        // ...and the root's children are the middle node, not the leaf: a bar
        // rooted on main must not show its grandchild as if it were its child.
        let top = children_of(&nodes, "main", now, Some("sid-mid"));
        assert_eq!(
            top.iter().map(|(n, _)| n.name.as_str()).collect::<Vec<_>>(),
            vec!["task-handler"],
            "never uncles or cousins — and never grandchildren either"
        );

        // Walking UP from the leaf yields the whole chain, root-most first,
        // which is what makes every ancestor drawn and clickable however deep
        // the selection sits.
        let by_name: HashMap<&str, &Node> =
            nodes.iter().map(|n| (n.name.as_str(), n)).collect();
        let mut chain: Vec<&str> = Vec::new();
        let mut cursor = by_name.get("luna-probe-w1").copied();
        while let Some(n) = cursor {
            let Some(parent) = n.primary_parent(now) else { break };
            if chain.contains(&parent) || chain.len() >= MAX_ANCESTRY {
                break;
            }
            chain.push(parent);
            cursor = by_name.get(parent).copied();
        }
        chain.reverse();
        assert_eq!(chain, vec!["main", "task-handler"], "root-most first");
    }
}
