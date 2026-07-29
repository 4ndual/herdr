//! Harness-manager host glue — mirrors [`crate::historial_host`], the wave-1
//! host-collector pattern, but with a real collector: it inspects the focused
//! (or latest-focused) Claude Code pane's session transcript, settings files,
//! ccp profile, and process, and projects the result into the wasm-safe
//! [`harness_manager_model::HarnessRenderModel`] republished each ~1 Hz dash
//! tick via [`App::write_harness_input_file`].
//!
//! Interaction: the guest declares `harness:open:` / `harness:disable_*` hit
//! rects; the mouse handler queues them as [`PendingHarnessAction`]s and
//! [`App::apply_pending_harness_action`] (here) resolves them against the
//! collector snapshot — spawning an editor tab, or editing the Claude settings
//! files (the disable mechanism: Claude Code hot-reloads direct settings edits
//! via its file watcher; if a scripted edit is quarantined behind the `/hooks`
//! review, `claude --resume <sid>` re-reads settings while preserving the
//! conversation — the toast says so).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use harness_manager_model::{HarnessRenderModel, HarnessRow, HarnessSection, RowState};

use crate::app::state::{HarnessActionKind, PendingHarnessAction};
use crate::app::App;

/// Full (settings / skills / process) rescan cadence, in dash ticks (~1 Hz).
const FULL_RESCAN_TICKS: u32 = 5;
/// A hook that fired within this window renders "active" (red).
const ACTIVE_WINDOW: Duration = Duration::from_secs(2);
/// Hard cap on rows per section so the model stays under the 64 KiB input cap.
const MAX_ROWS: usize = 12;
/// Row cap for the process tree: the SUM in the SESSION aggregate covers the
/// whole tree, but only this many individual descendant rows are emitted.
const MAX_PROC_ROWS: usize = 15;

/// Chars-per-token ratio used for every context-size estimate here. Phase-3 R7
/// calibration measured this against real `/context` category totals for two
/// live Claude Code sessions (fable-5). The two cleanly-attributable anchors —
/// skill descriptions (59 skills, 7.6k `/context` tokens) and custom-agent
/// descriptions (6 agents, 308 tokens) — both land at ~3.0 chars/tok: at 3.0 the
/// panel reads skills within +1.7% and agents within +2.3% of `/context`. (The
/// CLAUDE.md memory anchor implies a denser ~2.8 but its `/context` file set is
/// ambiguous, so the reliable description anchors win.) Tune this single
/// constant so all estimates move together; do not scatter `/ 4` divides.
const CHARS_PER_TOKEN: f64 = 3.0;

/// Estimate tokens from a byte length via [`CHARS_PER_TOKEN`]. Markdown/context
/// files are mostly ASCII, so byte length ≈ char length — accurate enough for a
/// budget readout and calibratable through the one ratio constant.
fn estimate_tokens(bytes: usize) -> u32 {
    (bytes as f64 / CHARS_PER_TOKEN).ceil() as u32
}

// ── Collector cache ────────────────────────────────────────────────────────

#[derive(Default)]
struct HookStat {
    count: u64,
    ctx_count: u64,
    total_ms: u64,
    last_ms: u64,
    err_count: u64,
    err_ms: u64,
    last_seen: Option<Instant>,
}

/// One in-session file/memory read observed in the transcript, with enough to
/// bill it honestly (R9), timestamp it (R11), and evict it (R12).
#[derive(Default, Clone)]
struct CtxRead {
    /// Recorded content length from the attachment (0 if the attachment carried
    /// no inline content — the collector then stats the real path).
    bytes: usize,
    /// Monotonic event index at load time; `<= last_compaction_seq` ⇒ evicted.
    seq: u64,
    /// Load timestamp ("HH:MM"), empty if the line had none.
    at: String,
}

#[derive(Default)]
struct JsonlStats {
    hooks: HashMap<String, HookStat>,
    mcp_calls: HashMap<String, u64>,
    skill_calls: HashMap<String, u64>,
    /// Last (seq, "HH:MM") a skill was invoked — for loaded_at + eviction.
    skill_last: HashMap<String, (u64, String)>,
    tool_errors: u64,
    /// In-session reads keyed by path (replaces the old bytes-only map).
    ctx_reads: HashMap<String, CtxRead>,
    in_tokens: u64,
    out_tokens: u64,
    /// Monotonic transcript-event counter (drives eviction ordering).
    seq: u64,
    /// Number of compaction boundaries seen, and the seq/time of the last one.
    compaction_count: u64,
    last_compaction_seq: u64,
    last_compaction_at: String,
    // ── Measured start-of-conversation context costs (R6 provenance) ─────────
    // These come straight from the transcript's own bookkeeping attachments, so
    // the token numbers are measured bytes of what Claude Code actually loaded,
    // not a guess.
    /// Bytes of the `skill_listing` attachment: the skill catalog (name +
    /// description lines) injected into the system prompt at conversation start.
    skill_listing_bytes: usize,
    /// server name → bytes of its `mcp_instructions_delta` block loaded at start.
    mcp_instr_bytes: HashMap<String, usize>,
    /// Deferred tool NAMES currently in context (they cost only their name line,
    /// not a full schema, until fetched). Kept to bill each MCP its share.
    deferred_names: Vec<String>,
    /// agent type → bytes of its `agent_listing_delta` description line loaded
    /// at start.
    agent_desc_bytes: HashMap<String, usize>,
}

/// A hook entry resolved from a settings file, addressable for disable edits.
#[derive(Clone)]
struct HookEntry {
    file: PathBuf,
    event: String,
    matcher: String,
    matcher_idx: usize,
    hook_idx: usize,
    name: String,
    command: String,
    script: Option<PathBuf>,
}

/// A user skill: its catalog name, `SKILL.md` path, and the measured byte
/// length of its frontmatter `description` (the part injected into the system
/// prompt at conversation start; the body loads only on invoke).
#[derive(Clone)]
struct SkillEntry {
    name: String,
    path: PathBuf,
    desc_bytes: usize,
}

#[derive(Clone)]
struct McpEntry {
    /// `None` = top-level `mcpServers`; `Some(cwd)` = `projects[cwd].mcpServers`.
    project: Option<String>,
    name: String,
}

#[derive(Default)]
struct HarnessCache {
    /// Latest-focused Claude target: (session id, cwd).
    target: Option<(String, String)>,
    transcript: Option<PathBuf>,
    offset: u64,
    stats: JsonlStats,
    tick: u32,
    // Snapshot resolved on the full rescan; action keys index into these.
    ctx_paths: Vec<PathBuf>,
    hooks: Vec<HookEntry>,
    mcps: Vec<McpEntry>,
    plugins: Vec<String>,
    skills: Vec<SkillEntry>,
    /// Custom agent types: (name, definition .md path). From `~/.claude/agents`.
    agents: Vec<(String, PathBuf)>,
    /// Individual memory files under the session project's `memory/` dir (each
    /// loads on-recall, not at start — only MEMORY.md is loaded at start).
    mem_files: Vec<PathBuf>,
    /// Resolved memory index path (`MEMORY.md`) for the session, if present.
    mem_index: Option<PathBuf>,
    profile: Option<String>,
    /// R10: human one-line hook explanations keyed by hook display name, loaded
    /// from `<plugin_state>/hook-descriptions.json`. Empty ⇒ tooltips fall back
    /// to the raw command. Reloaded on the config rescan.
    hook_descriptions: HashMap<String, String>,
    /// Which agent CLI drives the focused session: "claude", "cx" (Claude Code
    /// routed through the CCR gateway), "codex", "pi", or "terminal". Derived on
    /// the full rescan (cx vs claude comes from which projects root the resolved
    /// transcript lives under). Empty until first resolved; callers fall back to
    /// "claude".
    agent_kind: String,
    session_lines: Vec<(String, String, String, u8)>, // (label, meta, tooltip, indent)
    /// R14: whether the panel was visibly expanded on the previous tick (drives
    /// the refresh-on-open force + the lazy/hidden cadence).
    was_visible: bool,
    /// R14: consecutive hidden ticks skipped since the last hidden refresh.
    hidden_skips: u32,
    /// R15: the last JSON written, so an unchanged model skips the file write
    /// entirely (unchanged input file = zero guest work).
    last_written: Option<String>,
    /// R15: mtime signature of the config inputs, so `full_rescan` re-parses
    /// settings/skills/agents only when one actually changed.
    config_sig: u64,
    /// SESSION header aggregate: total RAM + CPU over the WHOLE process tree
    /// (e.g. "1.2GB rss · 34% cpu · 22 procs · gpu n/det"). Empty until resolved.
    session_agg: String,
}

/// App-derived, per-tick facts the pure `refresh`/`full_rescan` path cannot get
/// from disk alone: the focused pane's live agent pid, the herdr pane id and
/// session attach command, and — when the user launched a tmux *inside* the
/// pane — that inner tmux's attach info. Resolved in [`tick`] where `App` is in
/// hand, then threaded down.
#[derive(Default)]
struct SessionEnv {
    agent_pid: Option<u32>,
    pane_id: Option<u32>,
    /// `herdr session attach <name>` (or bare `herdr` for the default session).
    herdr_attach_cmd: String,
    /// The herdr session name, `None` for the default session.
    herdr_session: Option<String>,
    /// Set only when a tmux client/server runs within the focused pane's own
    /// foreground job (i.e. the user ran `tmux` inside the herdr pane). The
    /// herdr app's own outer tmux, if any, is an ancestor of the server and
    /// never appears here.
    inner_tmux: Option<TmuxAttach>,
}

struct TmuxAttach {
    session: String,
    attach_cmd: String,
    clients: usize,
}

static CACHE: Mutex<Option<HarnessCache>> = Mutex::new(None);

/// R14: how many ~1 Hz ticks to skip between refreshes while the panel is
/// hidden. ~15 ticks ≈ 15 s: just enough that the rail icon state stays sane.
const HIDDEN_TICKS: u32 = 15;

/// R14: is the harness panel currently expanded (a carved rect wider than the
/// slim collapsed handle)? The host tracks carved panel rects per plugin; a
/// width <= 3 is the collapsed handle (see `render_handle`).
fn harness_panel_visible(app: &App) -> bool {
    app.state.view.panel_rects.iter().any(|p| {
        p.plugin_id == crate::app::HARNESS_PLUGIN_ID && p.rect.width > 3 && p.rect.height > 1
    })
}

/// Republish the harness render input from the focused Claude pane. Called on
/// the dash tick (~1 Hz) next to [`crate::historial_host::tick`].
///
/// R14 adaptive cadence: refresh at the full ~1 Hz only while the panel is
/// visibly expanded; when hidden, do the expensive work at most every
/// [`HIDDEN_TICKS`] ticks — but always force one refresh on the hidden→visible
/// transition so the first open frame is never stale. R15: skip the file write
/// entirely when the rendered model is byte-identical to what was last written.
pub fn tick(app: &mut App) {
    let visible = harness_panel_visible(app);
    // Decide (cheaply, before the costly env probe) whether to work this tick.
    let do_work = {
        let mut guard = match CACHE.lock() {
            Ok(g) => g,
            Err(err) => {
                tracing::warn!(error = %err, "harness cache mutex poisoned; skipping tick");
                return;
            }
        };
        let cache = guard.get_or_insert_with(HarnessCache::default);
        let became_visible = visible && !cache.was_visible;
        cache.was_visible = visible;
        if visible || became_visible {
            cache.hidden_skips = 0;
            true
        } else if cache.hidden_skips + 1 >= HIDDEN_TICKS {
            cache.hidden_skips = 0;
            true
        } else {
            cache.hidden_skips += 1;
            false
        }
    };
    if !do_work {
        return;
    }
    // Resolve the app-derived session facts (live agent pid, herdr pane id and
    // attach command, any inner tmux) the same way `handle_pane_process_info`
    // reaches the pane's foreground job. Only on a working tick, so a hidden
    // panel does no process probing.
    let focus = crate::plugin_focus::focus_input(&app.state);
    let env = resolve_session_env(app);
    let to_write = {
        let mut guard = match CACHE.lock() {
            Ok(g) => g,
            Err(err) => {
                tracing::warn!(error = %err, "harness cache mutex poisoned; skipping tick");
                return;
            }
        };
        let cache = guard.get_or_insert_with(HarnessCache::default);
        refresh(cache, &focus, &env);
        match render_json(cache) {
            // R15: unchanged model ⇒ don't rewrite the input file (zero guest work).
            Some(json) if cache.last_written.as_deref() == Some(json.as_str()) => None,
            Some(json) => {
                cache.last_written = Some(json.clone());
                Some(json)
            }
            None => None,
        }
    };
    if let Some(json) = to_write {
        app.write_harness_input_file(&json);
    }
}

/// Resolve the focused pane's session facts from its terminal runtime's
/// foreground job (the same source `App::handle_pane_process_info` uses): the
/// live agent pid (newest `claude` / `claude-*` / `node`, so a fresh session
/// with no `--resume <sid>` in argv still resolves), the herdr pane id and
/// session attach command, and any tmux the user launched *inside* the pane.
/// Only the pane's own foreground job is scanned, so herdr's own outer tmux (an
/// ancestor of the server, e.g. under the E2E `test-host`) is never mistaken for
/// an inner one.
fn resolve_session_env(app: &App) -> SessionEnv {
    let mut env = SessionEnv {
        herdr_attach_cmd: crate::session::local_attach_command(),
        herdr_session: crate::session::active_name(),
        ..SessionEnv::default()
    };
    let Some(ws_idx) = app.state.active else {
        return env;
    };
    let Some(ws) = app.state.workspaces.get(ws_idx) else {
        return env;
    };
    let Some(pane_id) = ws.focused_pane_id() else {
        return env;
    };
    env.pane_id = Some(pane_id.raw());
    let Some(runtime) =
        app.state
            .runtime_for_pane_in_workspace(&app.terminal_runtimes, ws_idx, pane_id)
    else {
        return env;
    };
    let Some(shell_pid) = runtime.child_pid() else {
        return env;
    };
    let Some(job) = crate::detect::foreground_job(shell_pid) else {
        return env;
    };
    let mut best: Option<u32> = None;
    let mut tmux_pid: Option<u32> = None;
    for p in &job.processes {
        let name = p.argv0.as_deref().unwrap_or(&p.name);
        let base = Path::new(name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(name);
        if base == "node" || base == "claude" || base.starts_with("claude-") {
            best = Some(best.map_or(p.pid, |b| b.max(p.pid)));
        } else if base == "tmux" || base.starts_with("tmux:") {
            tmux_pid = Some(tmux_pid.map_or(p.pid, |b| b.max(p.pid)));
        }
    }
    env.agent_pid = best;
    if let Some(pid) = tmux_pid {
        env.inner_tmux = resolve_tmux_attach(pid);
    }
    env
}

/// Resolve the tmux session a process runs inside from its `TMUX` env var
/// (`<socket>,<server_pid>,<session_id>`), mapping the numeric session id to a
/// name on that server's socket. Best-effort; `None` if the env or lookup fails.
fn resolve_tmux_attach(pid: u32) -> Option<TmuxAttach> {
    let procenv = run_capture("ps", &["eww", "-p", &pid.to_string(), "-o", "command="])?;
    let tmux_var = procenv
        .split_whitespace()
        .find_map(|t| t.strip_prefix("TMUX="))?;
    let mut parts = tmux_var.split(',');
    let socket = parts.next()?;
    let _server_pid = parts.next();
    let want = format!("${}", parts.next().unwrap_or(""));
    let list = run_capture(
        "tmux",
        &[
            "-S",
            socket,
            "list-sessions",
            "-F",
            "#{session_id} #{session_name}",
        ],
    )?;
    let session = list.lines().find_map(|l| {
        let (id, name) = l.split_once(' ')?;
        (id == want).then(|| name.to_string())
    })?;
    let clients = run_capture(
        "tmux",
        &["-S", socket, "list-clients", "-t", &session, "-F", "x"],
    )
    .map(|c| c.lines().count())
    .unwrap_or(0);
    Some(TmuxAttach {
        attach_cmd: format!("tmux -S {socket} attach -t {session}"),
        session,
        clients,
    })
}

fn refresh(cache: &mut HarnessCache, focus: &herdr_ui_plugin_api::FocusInput, env: &SessionEnv) {
    // Latest-focused Claude pane wins; other focuses keep the last target.
    if focus.agent == herdr_ui_plugin_api::AgentKind::Claude {
        if let (Some(sid), Some(cwd)) = (focus.session_id.as_deref(), focus.cwd.as_deref()) {
            if !sid.is_empty() {
                let cwd = cwd.to_string_lossy().into_owned();
                if cache.target.as_ref().map(|(s, _)| s.as_str()) != Some(sid) {
                    cache.target = Some((sid.to_string(), cwd));
                    cache.transcript = usage_dash_core::sources::claude::transcript_for_sid(sid);
                    cache.offset = 0;
                    cache.stats = JsonlStats::default();
                    cache.tick = 0;
                }
            }
        }
    }
    let Some((sid, cwd)) = cache.target.clone() else {
        return;
    };
    if cache.transcript.is_none() {
        cache.transcript = usage_dash_core::sources::claude::transcript_for_sid(&sid);
    }
    if let Some(path) = cache.transcript.clone() {
        scan_jsonl(cache, &path);
    }
    // R15: the config scan (settings/hooks/skills/agents/memory/ctx) is expensive
    // but rarely changes, so run it on the slow cadence AND only re-parse when a
    // cheap mtime signature moved. The process/RAM rollup is cheap and must
    // update at 1 Hz, so it runs every working tick.
    if cache.tick % FULL_RESCAN_TICKS == 0 {
        let sig = config_signature(&cwd);
        if sig != cache.config_sig || cache.ctx_paths.is_empty() {
            cache.config_sig = sig;
            full_rescan(cache, &sid, &cwd);
        } else {
            // Config unchanged: still refresh agent_kind cheaply (transcript-derived).
            refresh_agent_kind(cache);
        }
    }
    refresh_session_lines(cache, &sid, env);
    cache.tick = cache.tick.wrapping_add(1);
}

/// Cheap mtime signature over the config inputs (R15): re-parse only on change.
/// Combines mtimes of the settings files, `~/.claude.json`, the hook-descriptions
/// map, and the skills/agents/memory directory listings. Content edits inside a
/// skill/agent body are billed by re-stat at render time, so they need not force
/// a config re-parse.
fn config_signature(cwd: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let home = home_dir();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mtime = |p: &Path| -> u64 {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };
    let enc: String = cwd
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect();
    for p in [
        home.join(".claude/settings.json"),
        home.join(".claude/settings.local.json"),
        home.join(".claude.json"),
        home.join(".claude/skills"),
        home.join(".claude/agents"),
        home.join(format!(".claude/projects/{enc}/memory")),
        crate::plugin_paths::plugin_state_dir(crate::app::HARNESS_PLUGIN_ID)
            .join("hook-descriptions.json"),
        PathBuf::from(cwd).join(".claude/settings.json"),
        PathBuf::from(cwd).join(".claude/settings.local.json"),
        PathBuf::from(cwd).join("CLAUDE.md"),
    ] {
        mtime(&p).hash(&mut h);
    }
    h.finish()
}

// ── Transcript scanning (incremental) ──────────────────────────────────────

fn scan_jsonl(cache: &mut HarnessCache, path: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < cache.offset {
        // Truncated/rotated: start over.
        cache.offset = 0;
        cache.stats = JsonlStats::default();
    }
    if meta.len() == cache.offset {
        return;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    if file.seek(SeekFrom::Start(cache.offset)).is_err() {
        return;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return;
    }
    // Only consume complete lines; a partially-written tail is re-read next tick.
    let complete = match buf.rfind('\n') {
        Some(i) => i + 1,
        None => return,
    };
    cache.offset += complete as u64;
    for line in buf[..complete].lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        ingest_line(&mut cache.stats, &v);
    }
}

/// Convert an ISO-8601 transcript timestamp ("2026-07-23T18:46:42.086Z") to a
/// short local-ish "HH:MM" for display. Returns empty on any parse miss (the UI
/// simply omits the marker then).
fn short_time(ts: &str) -> String {
    // The transcript uses UTC "…THH:MM:SS…Z"; show HH:MM as-is (UTC). A precise
    // local conversion would need a tz dependency we deliberately avoid here.
    ts.split_once('T')
        .and_then(|(_, t)| t.get(..5))
        .map(str::to_string)
        .unwrap_or_default()
}

fn ingest_line(stats: &mut JsonlStats, v: &serde_json::Value) {
    stats.seq += 1;
    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .map(short_time)
        .unwrap_or_default();
    // R12: a compaction boundary summarizes and drops prior conversation reads.
    if v.get("type").and_then(|t| t.as_str()) == Some("system")
        && v.get("subtype").and_then(|s| s.as_str()) == Some("compact_boundary")
    {
        stats.compaction_count += 1;
        stats.last_compaction_seq = stats.seq;
        stats.last_compaction_at = ts.clone();
        return;
    }
    match v.get("type").and_then(|t| t.as_str()) {
        Some("attachment") => {
            let Some(a) = v.get("attachment") else { return };
            match a.get("type").and_then(|t| t.as_str()) {
                Some("hook_success") => {
                    let name = a
                        .get("hookName")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let stat = stats.hooks.entry(name).or_default();
                    if let Some(ms) = a.get("durationMs").and_then(|d| d.as_u64()) {
                        stat.count += 1;
                        stat.total_ms += ms;
                        stat.last_ms = ms;
                        stat.last_seen = Some(Instant::now());
                        let exit = a.get("exitCode").and_then(|e| e.as_i64()).unwrap_or(0);
                        if exit != 0 {
                            stat.err_count += 1;
                            stat.err_ms += ms;
                        }
                    }
                }
                Some("hook_additional_context") => {
                    let name = a
                        .get("hookName")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let stat = stats.hooks.entry(name).or_default();
                    stat.ctx_count += 1;
                    stat.last_seen = Some(Instant::now());
                }
                Some("file") => {
                    if let Some(name) = a.get("filename").and_then(|f| f.as_str()) {
                        let len = a
                            .get("content")
                            .and_then(|c| c.as_str())
                            .map(|c| c.len())
                            .unwrap_or(0);
                        stats.ctx_reads.insert(
                            name.to_string(),
                            CtxRead {
                                bytes: len,
                                seq: stats.seq,
                                at: ts.clone(),
                            },
                        );
                    }
                }
                Some("nested_memory") => {
                    if let Some(name) = a.get("path").and_then(|f| f.as_str()) {
                        let len = a
                            .get("content")
                            .and_then(|c| c.as_str())
                            .map(|c| c.len())
                            .unwrap_or(0);
                        stats.ctx_reads.insert(
                            name.to_string(),
                            CtxRead {
                                bytes: len,
                                seq: stats.seq,
                                at: ts.clone(),
                            },
                        );
                    }
                }
                Some("skill_listing") => {
                    // The whole skill catalog (name + description per skill) is
                    // injected at conversation start; its byte length is the
                    // measured loaded cost of "skills in context".
                    if let Some(c) = a.get("content").and_then(|c| c.as_str()) {
                        stats.skill_listing_bytes = c.len();
                    }
                }
                Some("mcp_instructions_delta") => {
                    // Per-server instruction blocks loaded at start; measure each.
                    if let (Some(names), Some(blocks)) = (
                        a.get("addedNames").and_then(|n| n.as_array()),
                        a.get("addedBlocks").and_then(|b| b.as_array()),
                    ) {
                        for (n, b) in names.iter().zip(blocks.iter()) {
                            if let (Some(name), Some(block)) = (n.as_str(), b.as_str()) {
                                stats.mcp_instr_bytes.insert(name.to_string(), block.len());
                            }
                        }
                    }
                }
                Some("deferred_tools_delta") => {
                    if let Some(added) = a.get("addedNames").and_then(|n| n.as_array()) {
                        for n in added {
                            if let Some(name) = n.as_str() {
                                stats.deferred_names.push(name.to_string());
                            }
                        }
                    }
                }
                Some("agent_listing_delta") => {
                    // addedTypes[i] pairs with addedLines[i] (the description line
                    // loaded into the system prompt at start).
                    if let (Some(types), Some(lines)) = (
                        a.get("addedTypes").and_then(|t| t.as_array()),
                        a.get("addedLines").and_then(|l| l.as_array()),
                    ) {
                        for (t, l) in types.iter().zip(lines.iter()) {
                            if let (Some(ty), Some(line)) = (t.as_str(), l.as_str()) {
                                stats.agent_desc_bytes.insert(ty.to_string(), line.len());
                            }
                        }
                    }
                }
                Some("invoked_skills") => {
                    if let Some(skills) = a.get("skills").and_then(|s| s.as_array()) {
                        for s in skills {
                            if let Some(name) = s.get("name").and_then(|n| n.as_str()) {
                                *stats.skill_calls.entry(name.to_string()).or_default() += 1;
                                stats
                                    .skill_last
                                    .insert(name.to_string(), (stats.seq, ts.clone()));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Some("assistant") => {
            let Some(content) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            else {
                return;
            };
            for block in content {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    continue;
                }
                let Some(name) = block.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                if let Some(rest) = name.strip_prefix("mcp__") {
                    let server = rest.split("__").next().unwrap_or(rest);
                    *stats.mcp_calls.entry(server.to_string()).or_default() += 1;
                } else if name == "Skill" {
                    if let Some(skill) = block
                        .get("input")
                        .and_then(|i| i.get("skill"))
                        .and_then(|s| s.as_str())
                    {
                        *stats.skill_calls.entry(skill.to_string()).or_default() += 1;
                        stats
                            .skill_last
                            .insert(skill.to_string(), (stats.seq, ts.clone()));
                    }
                }
            }
            if let Some(usage) = v.get("message").and_then(|m| m.get("usage")) {
                stats.in_tokens += usage
                    .get("input_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                stats.out_tokens += usage
                    .get("output_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
            }
        }
        Some("user") => {
            if let Some(content) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        && block.get("is_error").and_then(|e| e.as_bool()) == Some(true)
                    {
                        stats.tool_errors += 1;
                    }
                }
            }
        }
        _ => {}
    }
}

// ── Full rescan: settings / config / process ───────────────────────────────

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Derive a human hook name from its command string: the last path segment that
/// isn't a generic entrypoint name, falling back to the raw basename.
fn hook_display_name(command: &str) -> String {
    const GENERIC: [&str; 8] = [
        "hook", "index", "main", "route", "emit", "run", "cli", "start",
    ];
    // Prefer the last path-like token (the hook script) over the first, which is
    // usually the interpreter (`bun`, `bash`); fall back to the command name.
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let path_tok = tokens
        .iter()
        .rev()
        .find(|t| t.contains('/'))
        .or_else(|| tokens.first())
        .copied()
        .unwrap_or(command);
    let mut segments: Vec<&str> = path_tok.split('/').filter(|s| !s.is_empty()).collect();
    while let Some(last) = segments.pop() {
        let stem = last.split('.').next().unwrap_or(last);
        if !GENERIC.contains(&stem) && !stem.is_empty() {
            return stem.to_string();
        }
    }
    command.chars().take(24).collect()
}

/// The token of the command that is the hook SCRIPT, for the open-in-editor and
/// copy-path affordances.
///
/// Scanned in reverse, for the same reason `hook_display_name` does it: a hook
/// command is usually `<interpreter> <script>`, and the interpreter is just as
/// absolute and just as much a real file as the script is. Taking the first
/// match resolved every one of this machine's 13 configured hooks to
/// `~/.bun/bin/bun` — one identical, useless path on every row, with open and
/// copy both pointing at the runtime binary instead of the hook. A
/// single-token command (`/usr/local/bin/mytool --flag`) has first == last, so
/// it still resolves to itself.
fn hook_script_path(command: &str) -> Option<PathBuf> {
    command
        .split_whitespace()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_file())
        .next_back()
}

/// Read a single scalar field from a markdown file's leading YAML frontmatter
/// (`--- … ---`). Returns the trimmed value, or `None` if absent. Used for a
/// skill's `description` and an agent's `name`/`description`. Best-effort: only
/// handles the flat `key: value` shape these files use.
fn frontmatter_field(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }
    let prefix = format!("{key}:");
    for line in lines {
        let t = line.trim();
        if t == "---" {
            break;
        }
        if let Some(rest) = t.strip_prefix(&prefix) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn collect_hooks_from(file: &Path, out: &mut Vec<HookEntry>) {
    let Some(root) = read_json(file) else { return };
    let Some(events) = root.get("hooks").and_then(|h| h.as_object()) else {
        return;
    };
    for (event, matchers) in events {
        let Some(matchers) = matchers.as_array() else {
            continue;
        };
        for (matcher_idx, m) in matchers.iter().enumerate() {
            let matcher = m
                .get("matcher")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let Some(hooks) = m.get("hooks").and_then(|h| h.as_array()) else {
                continue;
            };
            for (hook_idx, h) in hooks.iter().enumerate() {
                let command = h
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                out.push(HookEntry {
                    file: file.to_path_buf(),
                    event: event.clone(),
                    matcher: matcher.clone(),
                    matcher_idx,
                    hook_idx,
                    name: hook_display_name(&command),
                    script: hook_script_path(&command),
                    command,
                });
            }
        }
    }
}

/// One row of the `ps -axo pid=,ppid=,%cpu=,rss=,command=` snapshot.
struct ProcInfo {
    ppid: u32,
    cpu: f64,
    rss_kb: u64,
    command: String,
}

/// Parse the `ps` snapshot into a pid → [`ProcInfo`] map. Malformed lines are
/// skipped rather than panicking (no `unwrap`).
fn parse_ps(ps: &str) -> HashMap<u32, ProcInfo> {
    let mut map = HashMap::new();
    for line in ps.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(cpu), Some(rss)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        map.insert(
            pid,
            ProcInfo {
                ppid,
                cpu: cpu.parse().unwrap_or(0.0),
                rss_kb: rss.parse().unwrap_or(0),
                command: it.collect::<Vec<_>>().join(" "),
            },
        );
    }
    map
}

/// Return `(pid, depth)` for `root` and every descendant, breadth-first, so the
/// caller can sum resources over the whole tree and indent per depth. `root` is
/// depth 0. Bounded by the snapshot size; cycles (shouldn't happen in a real
/// process table) are broken by a visited set.
fn descendant_tree(procs: &HashMap<u32, ProcInfo>, root: u32) -> Vec<(u32, usize)> {
    let mut out = vec![(root, 0usize)];
    let mut visited = std::collections::HashSet::from([root]);
    let mut i = 0;
    while i < out.len() {
        let (parent, depth) = out[i];
        i += 1;
        // Deterministic order: children sorted by pid.
        let mut kids: Vec<u32> = procs
            .iter()
            .filter(|(pid, p)| p.ppid == parent && !visited.contains(pid))
            .map(|(pid, _)| *pid)
            .collect();
        kids.sort_unstable();
        for k in kids {
            if visited.insert(k) {
                out.push((k, depth + 1));
            }
        }
    }
    out
}

/// Format a kilobyte count as MB or GB.
fn fmt_mb(kb: u64) -> String {
    let mb = kb as f64 / 1024.0;
    if mb >= 1024.0 {
        format!("{:.1}GB", mb / 1024.0)
    } else {
        format!("{mb:.0}MB")
    }
}

fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Derive which CLI drives the session from the resolved transcript path: a cx
/// session (Claude Code via the CCR gateway) writes under
/// `~/.claude-code-router/profiles/…`, plain Claude under `~/.claude/projects/…`.
/// Cheap; safe to call every tick without a full config re-parse.
fn refresh_agent_kind(cache: &mut HarnessCache) {
    cache.agent_kind = match &cache.transcript {
        Some(p)
            if p.to_string_lossy()
                .contains(".claude-code-router/profiles/") =>
        {
            "cx"
        }
        _ => "claude",
    }
    .to_string();
}

/// Re-parse the config inputs (settings/hooks/mcps/plugins/skills/agents/ctx/
/// memory). Expensive but mtime-gated by the caller (R15). Does NOT build the
/// process/session lines — those run every tick via [`refresh_session_lines`].
fn full_rescan(cache: &mut HarnessCache, _sid: &str, cwd: &str) {
    let home = home_dir();

    refresh_agent_kind(cache);

    // Context file candidates: static harness inputs that exist on disk.
    cache.ctx_paths.clear();
    let mut ctx = vec![home.join(".claude/CLAUDE.md")];
    // Project CLAUDE.md chain up from cwd.
    let mut dir = Some(PathBuf::from(cwd));
    while let Some(d) = dir {
        ctx.push(d.join("CLAUDE.md"));
        ctx.push(d.join(".claude/settings.json"));
        dir = d.parent().map(Path::to_path_buf);
    }
    // Memory index for the session's project dir encoding.
    let enc: String = cwd
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect();
    ctx.push(home.join(format!(".claude/projects/{enc}/memory/MEMORY.md")));
    ctx.push(home.join(".claude/settings.json"));
    cache.ctx_paths = ctx.into_iter().filter(|p| p.is_file()).collect();

    // Hooks from every settings scope that applies to this cwd.
    cache.hooks.clear();
    collect_hooks_from(&home.join(".claude/settings.json"), &mut cache.hooks);
    collect_hooks_from(&home.join(".claude/settings.local.json"), &mut cache.hooks);
    let proj = PathBuf::from(cwd);
    collect_hooks_from(&proj.join(".claude/settings.json"), &mut cache.hooks);
    collect_hooks_from(&proj.join(".claude/settings.local.json"), &mut cache.hooks);

    // R10: human hook descriptions (authored map; falls back to command).
    cache.hook_descriptions.clear();
    let desc_file = crate::plugin_paths::plugin_state_dir(crate::app::HARNESS_PLUGIN_ID)
        .join("hook-descriptions.json");
    if let Some(map) = read_json(&desc_file).and_then(|v| v.as_object().cloned()) {
        for (k, v) in map {
            if let Some(s) = v.as_str() {
                if !k.starts_with('_') {
                    cache.hook_descriptions.insert(k, s.to_string());
                }
            }
        }
    }

    // ccp profile.
    cache.profile = read_json(&home.join(".claude/settings.json"))
        .as_ref()
        .and_then(|s| s.get("_ccp"))
        .and_then(|c| c.get("profile"))
        .and_then(|p| p.as_str())
        .map(str::to_string);

    // MCP servers: top-level + this project's entry in ~/.claude.json.
    cache.mcps.clear();
    if let Some(root) = read_json(&home.join(".claude.json")) {
        if let Some(servers) = root.get("mcpServers").and_then(|m| m.as_object()) {
            for name in servers.keys() {
                cache.mcps.push(McpEntry {
                    project: None,
                    name: name.clone(),
                });
            }
        }
        if let Some(servers) = root
            .get("projects")
            .and_then(|p| p.get(cwd))
            .and_then(|p| p.get("mcpServers"))
            .and_then(|m| m.as_object())
        {
            for name in servers.keys() {
                cache.mcps.push(McpEntry {
                    project: Some(cwd.to_string()),
                    name: name.clone(),
                });
            }
        }
    }

    // Plugins (enabled map) and user skills catalog.
    cache.plugins.clear();
    if let Some(settings) = read_json(&home.join(".claude/settings.json")) {
        if let Some(map) = settings.get("enabledPlugins").and_then(|m| m.as_object()) {
            for (name, enabled) in map {
                if enabled.as_bool().unwrap_or(false) {
                    cache.plugins.push(name.clone());
                } else {
                    cache.plugins.push(format!("{name} (off)"));
                }
            }
        }
    }
    cache.skills.clear();
    if let Ok(entries) = std::fs::read_dir(home.join(".claude/skills")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = e.file_name().to_string_lossy().into_owned();
                let skill_md = p.join("SKILL.md");
                let desc_bytes = frontmatter_field(&skill_md, "description")
                    .map(|d| d.len())
                    .unwrap_or(0);
                cache.skills.push(SkillEntry {
                    name,
                    path: skill_md,
                    desc_bytes,
                });
            }
        }
        cache.skills.sort_by(|a, b| a.name.cmp(&b.name));
    }

    // Custom agent types (`~/.claude/agents/*.md`). Each definition's frontmatter
    // description loads into the system prompt at start; the body loads on spawn.
    cache.agents.clear();
    if let Ok(entries) = std::fs::read_dir(home.join(".claude/agents")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("md") {
                let name = frontmatter_field(&p, "name")
                    .unwrap_or_else(|| basename(&p).trim_end_matches(".md").to_string());
                cache.agents.push((name, p));
            }
        }
        cache.agents.sort();
    }

    // Memory files for the session's project encoding: MEMORY.md is the index
    // loaded at start; each sibling file loads on-recall.
    cache.mem_files.clear();
    cache.mem_index = None;
    let mem_dir = home.join(format!(".claude/projects/{enc}/memory"));
    let mem_index = mem_dir.join("MEMORY.md");
    if mem_index.is_file() {
        cache.mem_index = Some(mem_index.clone());
    }
    if let Ok(entries) = std::fs::read_dir(&mem_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("md")
                && p.file_name().and_then(|f| f.to_str()) != Some("MEMORY.md")
            {
                cache.mem_files.push(p);
            }
        }
        cache.mem_files.sort();
    }
}

/// Rebuild the SESSION section's process/attach lines and the whole-tree RAM/CPU
/// aggregate. Runs EVERY working tick (cheap: one `ps` snapshot + string work)
/// so RAM/CPU update at ~1 Hz while the panel is open (R14/R15). Reads the
/// agent_kind/profile the config scan already cached.
fn refresh_session_lines(cache: &mut HarnessCache, sid: &str, env: &SessionEnv) {
    let agent_kind = if cache.agent_kind.is_empty() {
        "claude".to_string()
    } else {
        cache.agent_kind.clone()
    };
    cache.session_lines.clear();
    let sid8: String = sid.chars().take(8).collect();
    cache.session_lines.push((
        format!("session {sid8}"),
        String::new(),
        format!("session id: {sid}\nresume: {agent_kind} --resume {sid}"),
        0,
    ));
    cache.session_lines.push((
        format!("agent {agent_kind}"),
        String::new(),
        match agent_kind.as_str() {
            "cx" => "cx: Claude Code driving GPT models through the CCR gateway (transcript under ~/.claude-code-router/profiles)".to_string(),
            _ => "plain Claude Code (transcript under ~/.claude/projects)".to_string(),
        },
        0,
    ));
    if let Some(profile) = &cache.profile {
        cache.session_lines.push((
            format!("ccp profile {profile}"),
            String::new(),
            format!("managed by ccp (~/claude-hub); profile settings: ~/claude-hub/profiles/{profile}/settings.json"),
            0,
        ));
    } else {
        cache.session_lines.push((
            "ccp: not detected".to_string(),
            String::new(),
            "no _ccp marker in ~/.claude/settings.json".to_string(),
            0,
        ));
    }
    // Claude process tree: resolve the root pid, then walk the FULL descendant
    // tree (recursive ppid descent over one `ps` snapshot) so the SESSION
    // aggregate reflects every worker/tool subprocess, not just depth-1 children.
    cache.session_agg.clear();
    let mut proc_line = ("process: n/det".to_string(), String::new(),
        format!("no {agent_kind} process with this session id in its command line (fresh sessions may not carry --resume)"), 0u8);
    let mut child_lines: Vec<(String, String, String, u8)> = Vec::new();
    if let Some(ps) = run_capture("ps", &["-axo", "pid=,ppid=,%cpu=,rss=,command="]) {
        let procs = parse_ps(&ps);
        // Prefer the pid herdr resolved from the pane's foreground job; fall back
        // to the sid grep for older/resumed sessions that carry it in argv.
        let root_pid = env.agent_pid.filter(|p| procs.contains_key(p)).or_else(|| {
            procs
                .iter()
                .find(|(_, p)| p.command.contains(sid))
                .map(|(pid, _)| *pid)
        });
        if let Some(root_pid) = root_pid {
            // Whole-tree rollup (breadth-first over ppid links).
            let tree = descendant_tree(&procs, root_pid);
            let mut total_cpu = 0.0f64;
            let mut total_rss_kb: u64 = 0;
            for (pid, depth) in &tree {
                if let Some(p) = procs.get(pid) {
                    total_cpu += p.cpu;
                    total_rss_kb += p.rss_kb;
                    let _ = depth;
                }
            }
            let n = tree.len();
            cache.session_agg = format!(
                "{} rss · {:.0}% cpu · {n} procs · gpu n/det",
                fmt_mb(total_rss_kb),
                total_cpu
            );
            if let Some(root) = procs.get(&root_pid) {
                proc_line = (
                    format!("{agent_kind} pid {root_pid}"),
                    format!("{:.0}% {} +{}", root.cpu, fmt_mb(root.rss_kb), n.saturating_sub(1)),
                    format!(
                        "cpu {:.0}% · rss {} · {} descendant procs (whole-tree sum in header)\nrss overstates real memory on macOS (shared pages counted per-process)",
                        root.cpu, fmt_mb(root.rss_kb), n.saturating_sub(1)
                    ),
                    0,
                );
            }
            // Per-descendant rows (skip the root itself), capped; the SUM above
            // still covers every process even past the cap.
            for (pid, depth) in tree
                .iter()
                .filter(|(p, _)| *p != root_pid)
                .take(MAX_PROC_ROWS)
            {
                if let Some(p) = procs.get(pid) {
                    let base = p
                        .command
                        .split_whitespace()
                        .next()
                        .map(Path::new)
                        .map(basename)
                        .unwrap_or_else(|| "?".to_string());
                    let cmd_trunc: String = p.command.chars().take(200).collect();
                    child_lines.push((
                        format!("└ {base}"),
                        format!("{pid} {:.0}% {}", p.cpu, fmt_mb(p.rss_kb)),
                        format!("cmd: {cmd_trunc}"),
                        (*depth as u8).min(4).max(1),
                    ));
                }
            }
        }
    }
    cache.session_lines.push(proc_line);
    cache.session_lines.extend(child_lines);
    // Attach row. A herdr pane is a herdr-native PTY, not a tmux pane, so the
    // old "tmux by matching cwd" probe always said "none detected". Instead: if
    // the user launched a tmux *inside* the pane, surface that inner tmux;
    // otherwise surface how to reattach the herdr pane itself. "none detected"
    // stays only when neither is known.
    let attach_line = if let Some(t) = &env.inner_tmux {
        (
            format!("tmux {}", t.session),
            format!("{} client(s)", t.clients),
            format!("attach: {}", t.attach_cmd),
            0u8,
        )
    } else if let Some(pane_id) = env.pane_id {
        let session = env.herdr_session.as_deref().unwrap_or("default");
        (
            format!("herdr pane {pane_id}"),
            format!("session {session}"),
            format!(
                "herdr pane {pane_id} · session {session}\nattach: {}",
                env.herdr_attach_cmd
            ),
            0u8,
        )
    } else {
        (
            "attach: none detected".to_string(),
            String::new(),
            "no herdr pane or inner tmux resolved for this session".to_string(),
            0u8,
        )
    };
    cache.session_lines.push(attach_line);
}

// ── Model projection ───────────────────────────────────────────────────────

fn fmt_bytes(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1}MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{n}B")
    }
}

/// Format a raw token count for display.
fn fmt_tokens(tok: u32) -> String {
    if tok >= 1000 {
        format!("~{:.1}k tok", tok as f64 / 1000.0)
    } else {
        format!("~{tok} tok")
    }
}

fn basename(p: &Path) -> String {
    p.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

fn display_path(p: &Path) -> String {
    let home = home_dir();
    let s = p.to_string_lossy();
    match p.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.to_string_lossy()),
        Err(_) => s.into_owned(),
    }
}

fn render_json(cache: &HarnessCache) -> Option<String> {
    let mut sections = Vec::new();
    let target = match &cache.target {
        Some((sid, cwd)) => {
            let sid8: String = sid.chars().take(8).collect();
            let folder = basename(Path::new(cwd));
            let kind = if cache.agent_kind.is_empty() {
                "claude"
            } else {
                cache.agent_kind.as_str()
            };
            format!("{kind} · {sid8} · {folder}")
        }
        None => String::new(),
    };
    if cache.target.is_some() {
        sections.push(context_section(cache));
        sections.push(session_section(cache));
        sections.push(hooks_section(cache));
        sections.push(mcps_section(cache));
        sections.push(plugins_section(cache));
        sections.push(skills_section(cache));
        sections.push(agents_section(cache));
    }
    let model = HarnessRenderModel { target, sections };
    match serde_json::to_string(&model) {
        Ok(json) => Some(json),
        Err(err) => {
            tracing::warn!(error = %err, "harness render model failed to serialize");
            None
        }
    }
}

/// Config scope of a context path: user-level `~/.claude` files load in EVERY
/// session ("global"); a per-project memory dir or a project file is "session".
fn ctx_scope(path: &Path, home: &Path) -> &'static str {
    if path.starts_with(home.join(".claude/projects")) {
        "session" // per-project memory
    } else if path.starts_with(home.join(".claude")) {
        "global"
    } else {
        "session"
    }
}

fn context_section(cache: &HarnessCache) -> HarnessSection {
    let home = home_dir();
    let mut rows = Vec::new();
    let mut loaded_tok = 0u32; // tokens occupying context NOW.
    for (i, path) in cache.ctx_paths.iter().take(MAX_ROWS).enumerate() {
        let bytes = std::fs::metadata(path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let tok = estimate_tokens(bytes);
        loaded_tok += tok;
        let scope = ctx_scope(path, &home);
        rows.push(HarnessRow {
            key: format!("ctx:{i}"),
            label: display_path(path),
            meta: format!("{} {}", fmt_bytes(bytes), fmt_tokens(tok)),
            state: RowState::None,
            tooltip: format!(
                "{}\nloaded at start ({scope}) · {} measured on disk\nclick to open in a pane",
                display_path(path),
                fmt_bytes(bytes)
            ),
            openable: true,
            can_disable: false,
            indent: 0,
            group: Some("persistent".to_string()),
            tok_loaded: Some(tok),
            tok_full: Some(tok),
            tok_src: Some(format!("file {}", fmt_bytes(bytes))),
            load_when: Some("start".to_string()),
            scope: Some(scope.to_string()),
            ..Default::default()
        });
    }
    // Memory index MEMORY.md loads at start; individual memory files load
    // on-recall (grouped under CONTEXT so the UI can nest them).
    if let Some(idx) = &cache.mem_index {
        let bytes = std::fs::metadata(idx)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let tok = estimate_tokens(bytes);
        loaded_tok += tok;
        rows.push(HarnessRow {
            key: "memidx:0".to_string(),
            label: "MEMORY.md (index)".to_string(),
            meta: format!("{} {}", fmt_bytes(bytes), fmt_tokens(tok)),
            state: RowState::None,
            tooltip: format!(
                "{}\nmemory index loaded at start · {} measured on disk\nclick to open",
                display_path(idx),
                fmt_bytes(bytes)
            ),
            openable: true,
            can_disable: false,
            indent: 0,
            group: Some("memory".to_string()),
            tok_loaded: Some(tok),
            tok_full: Some(tok),
            tok_src: Some(format!("MEMORY.md {}", fmt_bytes(bytes))),
            load_when: Some("start".to_string()),
            scope: Some("session".to_string()),
            ..Default::default()
        });
    }
    for (j, mp) in cache.mem_files.iter().take(MAX_ROWS).enumerate() {
        let bytes = std::fs::metadata(mp).map(|m| m.len() as usize).unwrap_or(0);
        let tok = estimate_tokens(bytes);
        rows.push(HarnessRow {
            key: format!("mem:{j}"),
            label: basename(mp).trim_end_matches(".md").to_string(),
            meta: format!("{} {}", fmt_bytes(bytes), fmt_tokens(tok)),
            state: RowState::Off,
            tooltip: format!(
                "{}\nnot in context now — loads on-recall if referenced · {} on disk\nclick to open",
                display_path(mp),
                fmt_bytes(bytes)
            ),
            openable: true,
            can_disable: false,
            indent: 1,
            group: Some("memory".to_string()),
            tok_loaded: None,
            tok_full: Some(tok),
            tok_src: Some(format!("file {}", fmt_bytes(bytes))),
            load_when: Some("on-recall".to_string()),
            scope: Some("session".to_string()),
            ..Default::default()
        });
    }
    // Files the transcript reports as read this session (R9: never a fake 0 —
    // use the recorded content length, else stat the real path, else "n/det";
    // R11 loaded_at; R12 eviction after a compaction boundary).
    let compacted = cache.stats.last_compaction_seq > 0;
    let mut seen: Vec<(&String, &CtxRead)> = cache.stats.ctx_reads.iter().collect();
    seen.sort_by(|a, b| b.1.seq.cmp(&a.1.seq)); // most-recent first
    for (name, read) in seen.into_iter().take(MAX_ROWS.saturating_sub(rows.len())) {
        // Real byte count: recorded content length wins; else stat the path.
        let disk = std::fs::metadata(Path::new(name))
            .ok()
            .map(|m| m.len() as usize);
        let bytes: Option<usize> = if read.bytes > 0 {
            Some(read.bytes)
        } else {
            disk.filter(|b| *b > 0)
        };
        let evicted = compacted && read.seq <= cache.stats.last_compaction_seq;
        let (meta, tok_full, tok_src) = match bytes {
            Some(b) => (
                format!("{} {}", fmt_bytes(b), fmt_tokens(estimate_tokens(b))),
                Some(estimate_tokens(b)),
                Some(format!(
                    "{} {}",
                    if read.bytes > 0 { "transcript" } else { "disk" },
                    fmt_bytes(b)
                )),
            ),
            None => (
                "n/det".to_string(),
                None,
                Some("not determinable".to_string()),
            ),
        };
        let mut tooltip = format!("{name}\nread this session");
        if !read.at.is_empty() {
            tooltip.push_str(&format!(" at {}", read.at));
        }
        if evicted {
            tooltip.push_str(&format!(
                "\nEVICTED by compaction at {} — no longer verbatim in context",
                cache.stats.last_compaction_at
            ));
        }
        rows.push(HarnessRow {
            key: format!("ctxr:{name}"),
            label: basename(Path::new(name)),
            meta,
            state: if evicted {
                RowState::Off
            } else {
                RowState::None
            },
            tooltip,
            openable: false,
            can_disable: false,
            indent: 1,
            group: Some("session reads".to_string()),
            tok_full,
            tok_src,
            load_when: Some(if evicted { "evicted" } else { "on-recall" }.to_string()),
            loaded_at: (!read.at.is_empty()).then(|| read.at.clone()),
            scope: Some("session".to_string()),
            ..Default::default()
        });
    }
    // R12: surface compaction so evicted rows have context.
    let agg = if cache.stats.compaction_count > 0 {
        format!(
            "{} loaded · {}× compacted",
            fmt_tokens(loaded_tok),
            cache.stats.compaction_count
        )
    } else {
        format!("{} loaded", fmt_tokens(loaded_tok))
    };
    HarnessSection {
        title: "CONTEXT".to_string(),
        summary: fmt_tokens(loaded_tok),
        agg: Some(agg),
        collapsed_default: false,
        rows,
    }
}

fn session_section(cache: &HarnessCache) -> HarnessSection {
    let rows = cache
        .session_lines
        .iter()
        .enumerate()
        .map(|(i, (label, meta, tooltip, indent))| HarnessRow {
            key: format!("sess:{i}"),
            label: label.clone(),
            meta: meta.clone(),
            state: RowState::None,
            tooltip: tooltip.clone(),
            openable: false,
            can_disable: false,
            indent: *indent,
            ..Default::default()
        })
        .collect();
    HarnessSection {
        title: "SESSION".to_string(),
        summary: format!(
            "{}k in / {}k out",
            cache.stats.in_tokens / 1000,
            cache.stats.out_tokens / 1000
        ),
        agg: (!cache.session_agg.is_empty()).then(|| cache.session_agg.clone()),
        collapsed_default: false,
        rows,
    }
}

fn hooks_section(cache: &HarnessCache) -> HarnessSection {
    let mut rows = Vec::new();
    // Order project-scope hooks (file under the target cwd) FIRST, then
    // user-scope, preserving each settings file's event order within its group,
    // so the hooks the user is most likely to act on render without scrolling.
    // The row key keeps each entry's ORIGINAL index into `cache.hooks` so
    // `resolve_action` still finds the right entry.
    let cwd = cache.target.as_ref().map(|(_, cwd)| PathBuf::from(cwd));
    let is_project = |entry: &HookEntry| {
        cwd.as_ref()
            .map(|cwd| entry.file.starts_with(cwd))
            .unwrap_or(false)
    };
    let ordered: Vec<(usize, &HookEntry)> = cache
        .hooks
        .iter()
        .enumerate()
        .filter(|(_, e)| is_project(e))
        .chain(
            cache
                .hooks
                .iter()
                .enumerate()
                .filter(|(_, e)| !is_project(e)),
        )
        .collect();
    for (i, entry) in ordered {
        // Transcript hook attachments are keyed at EVENT granularity (multiple
        // hooks on one event are indistinguishable there), so attribute the
        // aggregate of every stat for this event to the event GROUP. R3 grouping
        // makes this correct by design: the number belongs to the group header,
        // not to a single hook's row. `agg_event_stat` sums the event's stats.
        let ev = agg_event_stat(&cache.stats, &entry.event);
        let meta = if ev.count > 0 {
            let avg = ev.total_ms / ev.count;
            let mut m = format!("x{} {avg}ms/{}ms", ev.count, ev.last_ms);
            if ev.err_count > 0 {
                m.push_str(&format!(" !{}", ev.err_count));
            }
            m
        } else {
            "x0".to_string()
        };
        let active = ev
            .last_seen
            .map(|t| t.elapsed() < ACTIVE_WINDOW)
            .unwrap_or(false);
        let user_scope = entry.file.starts_with(home_dir());
        let scope = if user_scope { "user" } else { "project" };
        // R10: human explanation if authored, else the raw command as fallback.
        let human = cache
            .hook_descriptions
            .get(&entry.name)
            .cloned()
            .unwrap_or_else(|| format!("cmd: {}", entry.command));
        let mut tooltip = format!(
            "{} ({})\n{}\nevent {} · matcher \"{}\"\nevent fired x{} this session · avg {}ms · last {}ms · errors {} ({}ms lost)\ncounts are per-event group (transcript gives event-level granularity)\ntoken cost: not determinable\nright-click to disable",
            entry.name, scope, human, entry.event, entry.matcher,
            ev.count, if ev.count>0 {ev.total_ms/ev.count} else {0}, ev.last_ms, ev.err_count, ev.err_ms
        );
        // #31: surface the script PATH. Every other row type puts its path on
        // tooltip line 1, but a hook's line 1 is `name (scope)` — the guest
        // treats line 1 as the row's identity and titles the tooltip/detail box
        // with it, so overwriting it would trade the name and scope for the
        // path. Adding a labelled line keeps both, and the row model still
        // carries no path field, so `resolve_row_path` stays the single place
        // one exists and copy/open keep resolving identically.
        if let Some(script) = entry.script.as_ref() {
            tooltip.push_str(&format!("\nscript: {}", script.display()));
            tooltip.push_str("\nclick opens the hook script");
        }
        rows.push(HarnessRow {
            key: format!("hook:{i}"),
            label: entry.name.clone(),
            meta,
            state: if active {
                RowState::Active
            } else {
                RowState::Idle
            },
            tooltip,
            openable: entry.script.is_some(),
            can_disable: true,
            indent: 0,
            group: Some(entry.event.clone()),
            tok_loaded: None,
            tok_full: None,
            tok_src: Some("not determinable".to_string()),
            // A user-level hook runs in every session (global); a project hook is
            // scoped to this repo (session).
            scope: Some(if user_scope { "global" } else { "session" }.to_string()),
            ..Default::default()
        });
    }
    rows.truncate(MAX_ROWS * 2);
    // Header aggregate: total hook calls + total wall time across the session.
    let mut total_calls = 0u64;
    let mut total_ms = 0u64;
    for s in cache.stats.hooks.values() {
        total_calls += s.count;
        total_ms += s.total_ms;
    }
    HarnessSection {
        title: "HOOKS".to_string(),
        summary: format!("{}", cache.hooks.len()),
        agg: Some(format!("x{total_calls} · {}", fmt_ms(total_ms))),
        collapsed_default: false,
        rows,
    }
}

/// Sum every recorded [`HookStat`] whose transcript key belongs to `event`
/// (keys look like "PreToolUse" or "PreToolUse:Bash"), collapsing the
/// event-level transcript granularity into one aggregate for the event group.
fn agg_event_stat(stats: &JsonlStats, event: &str) -> HookStat {
    let mut agg = HookStat::default();
    for (k, s) in &stats.hooks {
        if k == event || k.starts_with(&format!("{event}:")) {
            let count = if s.count > 0 { s.count } else { s.ctx_count };
            agg.count += count;
            agg.total_ms += s.total_ms;
            agg.err_count += s.err_count;
            agg.err_ms += s.err_ms;
            if s.last_ms != 0 {
                agg.last_ms = s.last_ms;
            }
            if let Some(t) = s.last_seen {
                agg.last_seen = Some(match agg.last_seen {
                    Some(prev) if prev > t => prev,
                    _ => t,
                });
            }
        }
    }
    agg
}

/// Format milliseconds as "8.6s" for large totals, "820ms" otherwise.
fn fmt_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn mcps_section(cache: &HarnessCache) -> HarnessSection {
    let mut rows = Vec::new();
    for (i, entry) in cache.mcps.iter().take(MAX_ROWS).enumerate() {
        let calls = cache.stats.mcp_calls.get(&entry.name).copied().unwrap_or(0);
        let scope = match &entry.project {
            Some(_) => "project",
            None => "global",
        };
        // Measured start-of-conversation cost: this server's instruction block
        // plus its deferred tool NAME lines (schemas cost nothing until fetched).
        let instr_bytes = mcp_instr_bytes_for(&cache.stats, &entry.name);
        let (defer_bytes, defer_n) = deferred_bytes_for(&cache.stats, &entry.name);
        let (tok_loaded, tok_src, meta_tok) = if instr_bytes > 0 || defer_n > 0 {
            let t = estimate_tokens(instr_bytes + defer_bytes);
            (
                Some(t),
                Some(format!(
                    "instr {} + {defer_n} deferred names",
                    fmt_bytes(instr_bytes)
                )),
                fmt_tokens(t),
            )
        } else {
            (None, Some("not determinable".to_string()), String::new())
        };
        rows.push(HarnessRow {
            key: format!("mcp:{i}"),
            label: entry.name.clone(),
            meta: format!("x{calls} {meta_tok}").trim().to_string(),
            state: if calls > 0 { RowState::Idle } else { RowState::Off },
            tooltip: format!(
                "MCP server ({scope} scope)\ncalls this session: {calls}\nloaded now: instructions {} + {defer_n} deferred tool names (schemas load on ToolSearch)\nright-click to disable",
                fmt_bytes(instr_bytes)
            ),
            openable: false,
            can_disable: true,
            indent: 0,
            tok_loaded,
            tok_full: None,
            tok_src,
            load_when: Some("start".to_string()),
            ..Default::default()
        });
    }
    // MCP servers observed in the transcript but not in this cwd's config
    // (e.g. plugin-provided) — listed read-only.
    for (name, calls) in cache.stats.mcp_calls.iter() {
        if rows.iter().any(|r| r.label == *name) || rows.len() >= MAX_ROWS {
            continue;
        }
        rows.push(HarnessRow {
            key: format!("mcpx:{name}"),
            label: name.clone(),
            meta: format!("x{calls}"),
            state: RowState::Idle,
            tooltip: "observed in transcript; not declared in ~/.claude.json for this cwd (plugin- or policy-provided)".to_string(),
            openable: false,
            can_disable: false,
            indent: 0,
            tok_src: Some("not determinable".to_string()),
            ..Default::default()
        });
    }
    // Header aggregate: total MCP calls + total measured instruction bytes loaded.
    let total_calls: u64 = cache.stats.mcp_calls.values().sum();
    let instr_tok: u32 = cache
        .stats
        .mcp_instr_bytes
        .values()
        .map(|b| estimate_tokens(*b))
        .sum();
    HarnessSection {
        title: "MCPS".to_string(),
        summary: format!("{}", rows.len()),
        agg: Some(format!("x{total_calls} · {} instr", fmt_tokens(instr_tok))),
        collapsed_default: false,
        rows,
    }
}

/// Bytes of the `mcp_instructions_delta` block for `server` (0 if none observed).
fn mcp_instr_bytes_for(stats: &JsonlStats, server: &str) -> usize {
    stats.mcp_instr_bytes.get(server).copied().unwrap_or(0)
}

/// Total bytes and count of deferred tool NAMES that belong to `server`
/// (`mcp__<server>__*`). These names occupy context now; their schemas do not.
fn deferred_bytes_for(stats: &JsonlStats, server: &str) -> (usize, usize) {
    let prefix = format!("mcp__{server}__");
    let mut bytes = 0;
    let mut n = 0;
    for name in &stats.deferred_names {
        if name.starts_with(&prefix) {
            bytes += name.len();
            n += 1;
        }
    }
    (bytes, n)
}

fn plugins_section(cache: &HarnessCache) -> HarnessSection {
    let rows = cache
        .plugins
        .iter()
        .take(MAX_ROWS)
        .enumerate()
        .map(|(i, name)| {
            let off = name.ends_with(" (off)");
            HarnessRow {
                key: format!("plugin:{i}"),
                label: name.clone(),
                meta: String::new(),
                state: if off { RowState::Off } else { RowState::Idle },
                tooltip: "claude code plugin (enabledPlugins in ~/.claude/settings.json)\ncontext cost folds into the skills/agents/MCP it registers (not separable per plugin)\nright-click to disable".to_string(),
                openable: false,
                can_disable: !off,
                indent: 0,
                tok_src: Some("not determinable".to_string()),
                ..Default::default()
            }
        })
        .collect();
    let enabled = cache
        .plugins
        .iter()
        .filter(|n| !n.ends_with(" (off)"))
        .count();
    HarnessSection {
        title: "PLUGINS".to_string(),
        summary: format!("{}", cache.plugins.len()),
        agg: Some(format!("{enabled} enabled")),
        collapsed_default: false,
        rows,
    }
}

fn skills_section(cache: &HarnessCache) -> HarnessSection {
    let mut rows: Vec<HarnessRow> = Vec::new();
    // Invoked skills first (with counts), then the rest of the catalog.
    let mut entries: Vec<(usize, &SkillEntry, u64)> = cache
        .skills
        .iter()
        .enumerate()
        .map(|(i, e)| {
            (
                i,
                e,
                cache.stats.skill_calls.get(&e.name).copied().unwrap_or(0),
            )
        })
        .collect();
    entries.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.name.cmp(&b.1.name)));
    let compacted = cache.stats.last_compaction_seq > 0;
    for (i, skill, calls) in entries.into_iter().take(MAX_ROWS) {
        // tok_loaded: this skill's name + description line, injected into the
        // system prompt at conversation start (measured from SKILL.md
        // frontmatter). tok_full: the whole SKILL.md body, loaded only on invoke.
        let desc_tok = estimate_tokens(skill.desc_bytes);
        let body_bytes = std::fs::metadata(&skill.path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let body_tok = estimate_tokens(body_bytes);
        // R11 loaded_at / R12 eviction from the last invoke.
        let last = cache.stats.skill_last.get(&skill.name);
        let loaded_at = last.map(|(_, at)| at.clone()).filter(|s| !s.is_empty());
        let evicted = compacted
            && last
                .map(|(seq, _)| *seq <= cache.stats.last_compaction_seq)
                .unwrap_or(false);
        let mut tooltip = format!(
            "{}\ninvoked x{calls} this session\nloaded now: {} (description in catalog, loads every session)\nif invoked: +{} (SKILL.md body, {} on disk)\nskills load on demand — no disable (archive the skill dir instead)\nclick opens SKILL.md",
            display_path(&skill.path), fmt_tokens(desc_tok), fmt_tokens(body_tok), fmt_bytes(body_bytes)
        );
        if let Some(at) = &loaded_at {
            tooltip.push_str(&format!("\nlast invoked at {at}"));
        }
        if evicted {
            tooltip.push_str(&format!(
                "\nits body was summarized away at compaction {}",
                cache.stats.last_compaction_at
            ));
        }
        rows.push(HarnessRow {
            key: format!("skill:{i}"),
            label: skill.name.clone(),
            meta: if calls > 0 {
                format!("x{calls} {}", fmt_tokens(desc_tok))
            } else {
                fmt_tokens(desc_tok)
            },
            state: if calls > 0 {
                RowState::Idle
            } else {
                RowState::Off
            },
            tooltip,
            openable: skill.path.is_file(),
            can_disable: false,
            indent: 0,
            tok_loaded: Some(desc_tok),
            tok_full: Some(body_tok),
            tok_src: Some(format!(
                "desc {}B · body {}",
                skill.desc_bytes,
                fmt_bytes(body_bytes)
            )),
            load_when: Some(if evicted { "evicted" } else { "on-invoke" }.to_string()),
            loaded_at,
            scope: Some("global".to_string()),
            ..Default::default()
        });
    }
    // Header aggregate: invoked count + the whole catalog's loaded-description
    // cost (measured from the transcript's skill_listing when present, else the
    // sum of per-skill description bytes).
    let invoked = cache
        .skills
        .iter()
        .filter(|s| cache.stats.skill_calls.get(&s.name).copied().unwrap_or(0) > 0)
        .count();
    let listing_tok = if cache.stats.skill_listing_bytes > 0 {
        estimate_tokens(cache.stats.skill_listing_bytes)
    } else {
        estimate_tokens(cache.skills.iter().map(|s| s.desc_bytes).sum())
    };
    HarnessSection {
        title: "SKILLS".to_string(),
        summary: format!("{}", cache.skills.len()),
        agg: Some(format!(
            "{invoked} used · {} loaded",
            fmt_tokens(listing_tok)
        )),
        collapsed_default: false,
        rows,
    }
}

fn agents_section(cache: &HarnessCache) -> HarnessSection {
    let mut rows: Vec<HarnessRow> = Vec::new();
    let mut loaded_tok = 0u32;
    for (i, (name, path)) in cache.agents.iter().take(MAX_ROWS).enumerate() {
        // Description loads at start (measured from the transcript's
        // agent_listing when present, else the frontmatter description on disk);
        // the full definition body loads on spawn.
        let desc_bytes = cache
            .stats
            .agent_desc_bytes
            .get(name)
            .copied()
            .or_else(|| frontmatter_field(path, "description").map(|d| d.len()))
            .unwrap_or(0);
        let desc_tok = estimate_tokens(desc_bytes);
        loaded_tok += desc_tok;
        let body_bytes = std::fs::metadata(path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let body_tok = estimate_tokens(body_bytes);
        rows.push(HarnessRow {
            key: format!("agent:{i}"),
            label: name.clone(),
            meta: fmt_tokens(desc_tok),
            state: RowState::Idle,
            tooltip: format!(
                "{}\nloaded now: {} (agent description in the type listing)\nif spawned: +{} (definition body, {} on disk)\nclick opens the agent .md",
                display_path(path), fmt_tokens(desc_tok), fmt_tokens(body_tok), fmt_bytes(body_bytes)
            ),
            openable: path.is_file(),
            can_disable: false,
            indent: 0,
            tok_loaded: Some(desc_tok),
            tok_full: Some(body_tok),
            tok_src: Some(format!("desc {desc_bytes}B · body {}", fmt_bytes(body_bytes))),
            load_when: Some("on-spawn".to_string()),
            scope: Some("global".to_string()),
            ..Default::default()
        });
    }
    HarnessSection {
        title: "AGENTS".to_string(),
        summary: format!("{}", cache.agents.len()),
        agg: Some(format!("{} loaded", fmt_tokens(loaded_tok))),
        collapsed_default: false,
        rows,
    }
}

// ── Host-resolved actions ──────────────────────────────────────────────────

/// Snapshot of what a queued action key resolves to (extracted under the cache
/// lock, applied after releasing it).
enum ResolvedAction {
    Open(PathBuf),
    CopyPath(PathBuf),
    DisableHook(HookEntry, bool),
    DisableMcp(McpEntry),
    DisablePlugin(String),
}

/// Maps a row key's `<kind>:<idx>` halves to the file that row stands for, or
/// `None` when the row has no file (a hook with no script, an unknown kind, a
/// stale index). The row model deliberately carries no path — this cache is the
/// only place one exists — so both `Open` and `CopyPath` come through here.
fn resolve_row_path(cache: &HarnessCache, kind: &str, idx: &str) -> Option<PathBuf> {
    match kind {
        "memidx" => cache.mem_index.clone(),
        "ctx" => cache.ctx_paths.get(idx.parse::<usize>().ok()?).cloned(),
        "mem" => cache.mem_files.get(idx.parse::<usize>().ok()?).cloned(),
        "hook" => cache
            .hooks
            .get(idx.parse::<usize>().ok()?)
            .and_then(|h| h.script.clone()),
        "skill" => cache
            .skills
            .get(idx.parse::<usize>().ok()?)
            .map(|s| s.path.clone()),
        "agent" => cache
            .agents
            .get(idx.parse::<usize>().ok()?)
            .map(|(_, p)| p.clone()),
        _ => None,
    }
}

fn resolve_action(action: &PendingHarnessAction) -> Option<ResolvedAction> {
    let guard = CACHE.lock().ok()?;
    let cache = guard.as_ref()?;
    let (kind, idx) = action.key.split_once(':')?;
    let profile_wide = action.kind == HarnessActionKind::DisableProfile;
    match (action.kind, kind) {
        // Open and CopyPath answer the same question — "which file is this row?"
        // — and differ only in what the host then does with the answer, so they
        // share one resolver. Keeping them in sync matters: a row advertises
        // `openable`, and the guest offers "copy path" off that same flag.
        (HarnessActionKind::Open, _) => {
            resolve_row_path(cache, kind, idx).map(ResolvedAction::Open)
        }
        (HarnessActionKind::CopyPath, _) => {
            resolve_row_path(cache, kind, idx).map(ResolvedAction::CopyPath)
        }
        (_, "hook") => {
            let i: usize = idx.parse().ok()?;
            cache
                .hooks
                .get(i)
                .cloned()
                .map(|h| ResolvedAction::DisableHook(h, profile_wide))
        }
        (_, "mcp") => {
            let i: usize = idx.parse().ok()?;
            cache.mcps.get(i).cloned().map(ResolvedAction::DisableMcp)
        }
        (_, "plugin") => {
            let i: usize = idx.parse().ok()?;
            cache
                .plugins
                .get(i)
                .cloned()
                .map(ResolvedAction::DisablePlugin)
        }
        _ => None,
    }
}

/// Serialize `value` and replace `path` with it atomically: write a temp file in
/// the SAME directory, fsync it, then rename over the destination. Returns
/// whether the destination now holds the new content.
///
/// These writes land on the user's real `settings.json` and `~/.claude.json`.
/// A plain `fs::write` truncates first and streams after, so an interrupted
/// write leaves a partial file -- and `~/.claude.json` holds far more than the
/// one key we came to edit, so that trades a single disabled MCP for the user's
/// whole Claude config. Rename on the same filesystem is atomic, so a reader
/// sees either the entire old file or the entire new one. The fsync is what
/// makes that true across a crash rather than only across a concurrent read.
/// Filename of the disable stash, under the harness plugin's state dir. Every
/// non-destructive disable appends its removed entry here; it is the only record
/// of what a delete-shaped disable took away, so both the hook and MCP paths
/// must agree on it.
const HARNESS_STASH_FILE: &str = "harness-disabled.json";

/// Callers pass already-serialized text rather than a `Value` so each keeps its
/// own formatting: settings files are pretty-printed, but `~/.claude.json` is
/// written compact and re-pretty-printing that whole file would rewrite every
/// line of a file we only came to remove one key from.
fn write_atomic(path: &Path, json: &str) -> bool {
    use std::io::Write as _;

    let Some(dir) = path.parent() else {
        tracing::warn!(path = %path.display(), "harness write: no parent dir");
        return false;
    };
    // Same directory ⇒ same filesystem ⇒ the rename below is atomic.
    let tmp = dir.join(format!(
        ".{}.herdr-tmp-{}",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
        std::process::id()
    ));
    let write_and_sync = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()
    };
    if let Err(err) = write_and_sync() {
        tracing::warn!(error = %err, path = %tmp.display(), "harness write: temp write failed");
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        tracing::warn!(error = %err, path = %path.display(), "harness write: rename failed");
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Append `record` to the disable stash at `stash`, atomically. Returns whether
/// the record is durably on disk.
///
/// Callers MUST treat `false` as "do not proceed with the delete": the stash is
/// the only record of what was removed, so writing it after the deletion (or
/// ignoring its failure) is how a disable becomes data loss.
fn stash_record(stash: &Path, record: serde_json::Value) -> bool {
    if let Some(dir) = stash.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut stashed: Vec<serde_json::Value> = read_json(stash)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    stashed.push(record);
    match serde_json::to_string_pretty(&serde_json::Value::Array(stashed)) {
        Ok(json) => write_atomic(stash, &json),
        Err(err) => {
            tracing::warn!(error = %err, "harness disable: stash serialize failed");
            false
        }
    }
}

/// Remove `hooks[event][matcher_idx].hooks[hook_idx]` from the settings JSON at
/// `file`, pruning empty containers, and stash the removed entry in `stash` so
/// the disable is reversible by hand. Returns whether the file changed.
fn remove_hook_from_file(entry: &HookEntry, stash: &Path) -> bool {
    let Some(mut root) = read_json(&entry.file) else {
        return false;
    };
    let removed = (|| {
        let matchers = root
            .get_mut("hooks")?
            .get_mut(&entry.event)?
            .as_array_mut()?;
        let m = matchers.get_mut(entry.matcher_idx)?;
        // Guard against drift between snapshot and file.
        let m_matcher = m.get("matcher").and_then(|s| s.as_str()).unwrap_or("");
        if m_matcher != entry.matcher {
            return None;
        }
        let hooks = m.get_mut("hooks")?.as_array_mut()?;
        let h = hooks.get(entry.hook_idx)?;
        if h.get("command").and_then(|c| c.as_str()) != Some(entry.command.as_str()) {
            return None;
        }
        let removed = hooks.remove(entry.hook_idx);
        if hooks.is_empty() {
            matchers.remove(entry.matcher_idx);
        }
        Some(removed)
    })();
    let Some(removed) = removed else {
        tracing::warn!(file = %entry.file.display(), event = %entry.event, "harness disable: hook entry drifted; not editing");
        return false;
    };
    // Stash BEFORE the delete lands. The stash is the only record of what was
    // removed, so a stash written afterwards (or a failure merely logged) leaves
    // a window where the entry is gone from settings and recorded nowhere.
    // Refusing to disable is recoverable; a silent unrecoverable delete is not.
    let record = serde_json::json!({
        "file": entry.file.to_string_lossy(),
        "kind": "hook",
        "name": entry.name,
        "event": entry.event,
        "matcher": entry.matcher,
        "removed": removed,
    });
    if !stash_record(stash, record) {
        tracing::warn!(file = %entry.file.display(), "harness disable: stash failed; not editing settings");
        return false;
    }
    let Ok(json) = serde_json::to_string_pretty(&root) else {
        return false;
    };
    if !write_atomic(&entry.file, &json) {
        return false;
    }
    true
}

/// Remove `entry` from the `mcpServers` map in the config JSON at `file`,
/// stashing the removed definition in `stash` FIRST. Returns whether the file
/// changed.
///
/// Both paths are parameters rather than derived from `home_dir()` inside, so
/// this is testable against a temp dir: a function that can only be exercised
/// against the user's real `~/.claude.json` is untestable by construction, and
/// this one deletes credential-bearing config.
fn remove_mcp_from_file(entry: &McpEntry, file: &Path, stash: &Path) -> bool {
    let Some(mut root) = read_json(file) else {
        return false;
    };
    // Keep the removed VALUE, not just whether something was removed. It is the
    // server's whole definition -- command, args, env, and whatever credentials
    // live in that env -- and dropping it made "disable" an unrecoverable delete.
    let removed = match &entry.project {
        None => root
            .get_mut("mcpServers")
            .and_then(|m| m.as_object_mut())
            .and_then(|m| m.remove(&entry.name)),
        Some(cwd) => root
            .get_mut("projects")
            .and_then(|p| p.get_mut(cwd))
            .and_then(|p| p.get_mut("mcpServers"))
            .and_then(|m| m.as_object_mut())
            .and_then(|m| m.remove(&entry.name)),
    };
    let Some(removed) = removed else {
        return false;
    };
    // Stash before the delete lands, same ordering and same refusal as hooks.
    let record = serde_json::json!({
        "file": file.to_string_lossy(),
        "kind": "mcp",
        "name": entry.name,
        "project": entry.project,
        "removed": removed,
    });
    if !stash_record(stash, record) {
        tracing::warn!(name = %entry.name, "harness disable: stash failed; not editing config");
        return false;
    }
    // Compact, matching how this file is already written: it is large and
    // machine-managed, and pretty-printing it would rewrite every line.
    let Ok(json) = serde_json::to_string(&root) else {
        return false;
    };
    write_atomic(file, &json)
}

/// Put a stashed entry back where it came from and drop its stash record.
/// Returns whether the config file changed.
///
/// Matches the LAST record for this identity, so re-disabling and re-enabling
/// the same thing restores the most recent definition rather than a stale one.
///
/// Ordering is the mirror of disable, for the same reason: the config is
/// written FIRST and the stash record is dropped only once the entry is safely
/// back. If the prune then fails, the cost is a duplicate record -- restoring
/// twice is harmless because the second restore just overwrites with the same
/// value -- whereas pruning first would let a failed restore lose the entry for
/// good.
///
/// `project` scopes MCP rows: `None` = top-level, `Some(cwd)` = that project's
/// map. A row is restored to its recorded location, never promoted or demoted.
fn restore_from_stash(stash: &Path, kind: &str, name: &str, project: Option<&str>) -> bool {
    let Some(stashed) = read_json(stash) else {
        return false;
    };
    let Some(records) = stashed.as_array() else {
        return false;
    };
    // Records written before `kind` existed are all hooks.
    let idx = records.iter().rposition(|r| {
        r.get("kind").and_then(|k| k.as_str()).unwrap_or("hook") == kind
            && r.get("name").and_then(|n| n.as_str()) == Some(name)
            && r.get("project").and_then(|p| p.as_str()) == project
    });
    let Some(idx) = idx else {
        tracing::warn!(kind, name, "harness enable: no stash record");
        return false;
    };
    let record = &records[idx];
    let (Some(file), Some(removed)) = (
        record
            .get("file")
            .and_then(|f| f.as_str())
            .map(PathBuf::from),
        record.get("removed").cloned(),
    ) else {
        tracing::warn!(kind, name, "harness enable: stash record malformed");
        return false;
    };
    let Some(mut root) = read_json(&file) else {
        tracing::warn!(file = %file.display(), "harness enable: config unreadable");
        return false;
    };

    let restored = match kind {
        "mcp" => {
            let map = match project {
                None => root
                    .as_object_mut()
                    .map(|r| {
                        r.entry("mcpServers")
                            .or_insert_with(|| serde_json::json!({}))
                    })
                    .and_then(|m| m.as_object_mut()),
                Some(cwd) => root
                    .as_object_mut()
                    .map(|r| r.entry("projects").or_insert_with(|| serde_json::json!({})))
                    .and_then(|p| p.as_object_mut())
                    .map(|p| p.entry(cwd).or_insert_with(|| serde_json::json!({})))
                    .and_then(|p| p.as_object_mut())
                    .map(|p| {
                        p.entry("mcpServers")
                            .or_insert_with(|| serde_json::json!({}))
                    })
                    .and_then(|m| m.as_object_mut()),
            };
            match map {
                Some(m) => {
                    m.insert(name.to_string(), removed);
                    true
                }
                None => false,
            }
        }
        "hook" => {
            let (Some(event), Some(matcher)) = (
                record.get("event").and_then(|e| e.as_str()),
                record.get("matcher").and_then(|m| m.as_str()),
            ) else {
                return false;
            };
            let groups = root
                .as_object_mut()
                .map(|r| r.entry("hooks").or_insert_with(|| serde_json::json!({})))
                .and_then(|h| h.as_object_mut())
                .map(|h| {
                    h.entry(event.to_string())
                        .or_insert_with(|| serde_json::json!([]))
                })
                .and_then(|g| g.as_array_mut());
            match groups {
                // The matcher group may itself have been pruned when the last
                // hook in it was disabled, so recreate it rather than failing.
                Some(groups) => {
                    match groups
                        .iter_mut()
                        .find(|g| g.get("matcher").and_then(|m| m.as_str()) == Some(matcher))
                        .and_then(|g| g.get_mut("hooks"))
                        .and_then(|h| h.as_array_mut())
                    {
                        Some(hooks) => hooks.push(removed),
                        None => groups.push(serde_json::json!({
                            "matcher": matcher,
                            "hooks": [removed],
                        })),
                    }
                    true
                }
                None => false,
            }
        }
        _ => {
            tracing::warn!(kind, "harness enable: unknown stash kind");
            false
        }
    };
    if !restored {
        return false;
    }

    // Settings files are pretty-printed; ~/.claude.json is kept compact.
    let json = if kind == "mcp" {
        serde_json::to_string(&root)
    } else {
        serde_json::to_string_pretty(&root)
    };
    let Ok(json) = json else {
        return false;
    };
    if !write_atomic(&file, &json) {
        return false;
    }

    // Entry is back; now drop its record. A failure here is logged, not fatal.
    let mut remaining = records.clone();
    remaining.remove(idx);
    match serde_json::to_string_pretty(&serde_json::Value::Array(remaining)) {
        Ok(pruned) => {
            if !write_atomic(stash, &pruned) {
                tracing::warn!(kind, name, "harness enable: restored, but stash prune failed");
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "harness enable: stash prune serialize failed");
        }
    }
    true
}

impl App {
    /// Drain and apply a pending harness-manager action (open / disable)
    /// recorded by the mouse handler. Returns true if it acted.
    pub(crate) fn apply_pending_harness_action(&mut self) -> bool {
        let Some(action) = self.state.pending_harness_action.take() else {
            return false;
        };
        let Some(resolved) = resolve_action(&action) else {
            tracing::warn!(key = %action.key, "harness action: key not in collector snapshot");
            return false;
        };
        let acted = match resolved {
            ResolvedAction::Open(path) => self.spawn_harness_editor_tab(&path),
            // Copying changes nothing on disk, so return before the
            // rescan+republish below, which exists to make a just-disabled
            // entry show its new state.
            ResolvedAction::CopyPath(path) => {
                let text = path.to_string_lossy().into_owned();
                self.state.request_clipboard_write = Some(text.clone().into_bytes());
                self.harness_toast(format!("copied {text}"));
                return true;
            }
            ResolvedAction::DisableHook(entry, profile_wide) => {
                self.disable_hook(&entry, profile_wide)
            }
            ResolvedAction::DisableMcp(entry) => self.disable_mcp(&entry),
            ResolvedAction::DisablePlugin(name) => self.disable_plugin(&name),
        };
        if acted {
            // Force a full rescan + immediate republish so the panel (and the
            // guest's open menu, keyed off a model change) updates this frame.
            let env = resolve_session_env(self);
            let json = {
                let mut guard = match CACHE.lock() {
                    Ok(g) => g,
                    Err(_) => return acted,
                };
                if let Some(cache) = guard.as_mut() {
                    if let Some((sid, cwd)) = cache.target.clone() {
                        cache.config_sig = config_signature(&cwd);
                        full_rescan(cache, &sid, &cwd);
                        refresh_session_lines(cache, &sid, &env);
                    }
                    // Force the write path (bypass R15 skip) by clearing the cache.
                    let json = render_json(cache);
                    cache.last_written = json.clone();
                    json
                } else {
                    None
                }
            };
            if let Some(json) = json {
                self.write_harness_input_file(&json);
            }
        }
        acted
    }

    fn harness_toast(&mut self, title: String) {
        let previous = self.state.toast.clone();
        self.state.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::Finished,
            title,
            context: "harness manager".into(),
            position: None,
            target: None,
        });
        self.sync_toast_deadline(previous);
    }

    /// Open `path` in `$EDITOR`, preferring a split of the focused pane so the
    /// editor sits beside the Claude pane; falls back to a new tab if the
    /// workspace can't split.
    fn spawn_harness_editor_tab(&mut self, path: &Path) -> bool {
        let Some(ws_idx) = self.state.active else {
            tracing::warn!("harness open: no active workspace");
            return false;
        };
        let editor = std::env::var("EDITOR")
            .ok()
            .filter(|e| !e.trim().is_empty())
            .unwrap_or_else(|| "vi".to_string());
        let argv = vec![editor, path.to_string_lossy().into_owned()];
        let cwd = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        if self.spawn_harness_editor_split(ws_idx, &argv, &cwd) {
            return true;
        }
        let (rows, cols) = self.state.estimate_pane_size();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab_argv_command(
                    rows.max(4),
                    cols.max(10),
                    cwd,
                    &argv,
                    Vec::new(),
                    scrollback_limit_bytes,
                    host_terminal_theme,
                )
            });
        match result {
            Ok((tab_idx, terminal, runtime)) => {
                self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                self.state.terminals.insert(terminal.id.clone(), terminal);
                self.state.remove_alias_shadowed_by_new_pane(
                    self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
                );
                let label = basename(path);
                if let Some(tab) = self
                    .state
                    .workspaces
                    .get_mut(ws_idx)
                    .and_then(|ws| ws.tabs.get_mut(tab_idx))
                {
                    tab.set_custom_name(label);
                }
                self.state.switch_workspace_tab(ws_idx, tab_idx);
                self.state.mode = crate::app::state::Mode::Terminal;
                self.schedule_session_save();
                self.emit_tab_created_events(ws_idx, tab_idx);
                true
            }
            Err(err) => {
                tracing::warn!(error = %err, "harness open: tab create failed");
                false
            }
        }
    }

    /// Split the focused pane to the right and run `argv` there. Returns false
    /// (without side effects) if the workspace has no focused pane or can't
    /// split, so the caller can fall back to a new tab.
    fn spawn_harness_editor_split(&mut self, ws_idx: usize, argv: &[String], cwd: &Path) -> bool {
        let Some(target_pane) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(crate::workspace::Workspace::focused_pane_id)
        else {
            return false;
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let previous_focus = self.state.current_pane_focus_target();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let result = self.state.workspaces.get_mut(ws_idx).and_then(|ws| {
            ws.split_pane_argv_command(
                target_pane,
                ratatui::layout::Direction::Horizontal,
                rows.max(4),
                cols.max(10),
                Some(cwd.to_path_buf()),
                argv,
                Vec::new(),
                scrollback_limit_bytes,
                host_terminal_theme,
                true,
            )
        });
        let new_pane = match result {
            Some(Ok(new_pane)) => new_pane,
            Some(Err(err)) => {
                tracing::warn!(error = %err, "harness open: split failed; falling back to tab");
                return false;
            }
            None => return false,
        };
        self.terminal_runtimes
            .insert(new_pane.1.terminal.id.clone(), new_pane.1.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.1.pane_id);
        self.state
            .terminals
            .insert(new_pane.1.terminal.id.clone(), new_pane.1.terminal);
        self.state.switch_workspace_tab(ws_idx, new_pane.0);
        self.state
            .record_pane_focus_change(previous_focus, ws_idx, new_pane.1.pane_id);
        self.state.mode = crate::app::state::Mode::Terminal;
        self.schedule_session_save();
        true
    }

    fn disable_hook(&mut self, entry: &HookEntry, profile_wide: bool) -> bool {
        let stash = crate::plugin_paths::plugin_state_dir(crate::app::HARNESS_PLUGIN_ID)
            .join(HARNESS_STASH_FILE);
        let mut changed = remove_hook_from_file(entry, &stash);
        if profile_wide {
            // Also remove from the ccp profile source so a profile rebuild
            // doesn't resurrect it.
            let profile = CACHE
                .lock()
                .ok()
                .and_then(|g| g.as_ref().and_then(|c| c.profile.clone()));
            if let Some(profile) = profile {
                let profile_file =
                    home_dir().join(format!("claude-hub/profiles/{profile}/settings.json"));
                if profile_file.is_file() && profile_file != entry.file {
                    let mut profile_entry = entry.clone();
                    profile_entry.file = profile_file;
                    changed |= remove_hook_from_file(&profile_entry, &stash);
                }
            }
        }
        if changed {
            let sid = CACHE
                .lock()
                .ok()
                .and_then(|g| g.as_ref().and_then(|c| c.target.clone()))
                .map(|(sid, _)| sid)
                .unwrap_or_default();
            self.harness_toast(format!(
                "hook {} disabled — if it still fires: claude --resume {sid}",
                entry.name
            ));
        }
        changed
    }

    fn disable_mcp(&mut self, entry: &McpEntry) -> bool {
        let file = home_dir().join(".claude.json");
        let stash = crate::plugin_paths::plugin_state_dir(crate::app::HARNESS_PLUGIN_ID)
            .join(HARNESS_STASH_FILE);
        if !remove_mcp_from_file(entry, &file, &stash) {
            return false;
        }
        self.harness_toast(format!(
            "mcp {} removed from config — takes effect on next session start/resume",
            entry.name
        ));
        true
    }

    fn disable_plugin(&mut self, name: &str) -> bool {
        let file = home_dir().join(".claude/settings.json");
        let Some(mut root) = read_json(&file) else {
            return false;
        };
        let clean = name.trim_end_matches(" (off)");
        let Some(map) = root
            .get_mut("enabledPlugins")
            .and_then(|m| m.as_object_mut())
        else {
            return false;
        };
        if !map.contains_key(clean) {
            return false;
        }
        map.insert(clean.to_string(), serde_json::Value::Bool(false));
        let Ok(json) = serde_json::to_string_pretty(&root) else {
            return false;
        };
        if let Err(err) = std::fs::write(&file, json) {
            tracing::warn!(error = %err, "harness disable: settings write failed");
            return false;
        }
        self.harness_toast(format!(
            "plugin {clean} disabled — takes effect on next session start/resume"
        ));
        true
    }
}

#[cfg(test)]
mod tests {

    /// Copy-path must resolve the SAME file open does, for every row kind that
    /// has one — and must decline the ones that do not, so the guest never
    /// offers a copy that would silently do nothing.
    #[test]
    fn copy_path_resolves_the_same_file_open_does() {
        fn hook(script: Option<PathBuf>) -> HookEntry {
            HookEntry {
                file: PathBuf::from("/w/.claude/settings.json"),
                event: "PreToolUse".to_string(),
                matcher: "*".to_string(),
                matcher_idx: 0,
                hook_idx: 0,
                name: "h".to_string(),
                command: "h.sh".to_string(),
                script,
            }
        }

        let cache = HarnessCache {
            ctx_paths: vec![PathBuf::from("/w/CLAUDE.md")],
            mem_index: Some(PathBuf::from("/w/memory/MEMORY.md")),
            mem_files: vec![PathBuf::from("/w/memory/one.md")],
            skills: vec![SkillEntry {
                name: "s".to_string(),
                path: PathBuf::from("/w/skills/s/SKILL.md"),
                desc_bytes: 0,
            }],
            agents: vec![("rev".to_string(), PathBuf::from("/w/agents/rev.md"))],
            hooks: vec![
                hook(Some(PathBuf::from("/w/hooks/h.sh"))),
                // A hook with no script: nothing to open, nothing to copy.
                hook(None),
            ],
            ..Default::default()
        };

        for (key, want) in [
            ("ctx:0", Some("/w/CLAUDE.md")),
            ("memidx:0", Some("/w/memory/MEMORY.md")),
            ("mem:0", Some("/w/memory/one.md")),
            ("skill:0", Some("/w/skills/s/SKILL.md")),
            ("agent:0", Some("/w/agents/rev.md")),
            ("hook:0", Some("/w/hooks/h.sh")),
            ("hook:1", None),
            ("ctx:9", None),
            ("mcp:0", None),
        ] {
            let (kind, idx) = key.split_once(':').expect("test key shape");
            let got = resolve_row_path(&cache, kind, idx);
            assert_eq!(
                got.as_deref().and_then(|p| p.to_str()),
                want,
                "resolve_row_path({key})"
            );
        }
    }
    use super::*;

    /// A hook command is `<interpreter> <script>` and BOTH tokens are absolute
    /// existing files, so "first match" silently resolves to the interpreter.
    /// On this machine that made all 13 hooks report `~/.bun/bin/bun`: one
    /// identical path on every row, with open/copy pointing at the runtime
    /// binary. The script is the last such token, never the first.
    #[test]
    fn hook_script_path_picks_the_script_not_the_interpreter() {
        let dir = std::env::temp_dir()
            .join(format!("harness-script-path-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let interp = dir.join("bun");
        let script = dir.join("pre_tool_use.ts");
        std::fs::write(&interp, "").expect("write interp");
        std::fs::write(&script, "").expect("write script");

        // Both tokens exist and are absolute; the script must win.
        assert_eq!(
            hook_script_path(&format!("{} {}", interp.display(), script.display())).as_deref(),
            Some(script.as_path()),
        );

        // Single-token command: first == last, still resolves to itself.
        assert_eq!(
            hook_script_path(&format!("{} --flag", interp.display())).as_deref(),
            Some(interp.as_path()),
        );

        // Nothing on disk and nothing absolute ⇒ no path, so the guest offers
        // neither open nor copy rather than a dead affordance.
        assert_eq!(hook_script_path("mytool --flag"), None);
        assert_eq!(
            hook_script_path(&dir.join("gone.ts").display().to_string()),
            None,
        );
    }

    #[test]
    fn hook_display_name_skips_generic_entrypoints() {
        assert_eq!(
            hook_display_name("/Users/x/.bun/bin/bun /Users/x/.claude/hooks/pkg-guard/hook.ts"),
            "pkg-guard"
        );
        assert_eq!(
            hook_display_name("bash /Users/x/.claude/hooks/orchestrator/emit.sh"),
            "orchestrator"
        );
        assert_eq!(hook_display_name("mytool --flag"), "mytool");
    }

    #[test]
    fn ingest_counts_hook_success_and_errors() {
        let mut stats = JsonlStats::default();
        let line = serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "hook_success", "hookName": "PreToolUse:Bash",
                            "durationMs": 40, "exitCode": 1}
        });
        ingest_line(&mut stats, &line);
        let s = &stats.hooks["PreToolUse:Bash"];
        assert_eq!(s.count, 1);
        assert_eq!(s.err_count, 1);
        assert_eq!(s.err_ms, 40);
    }

    #[test]
    fn estimate_tokens_uses_ratio() {
        // 30 chars / 3.0 = 10 tokens exactly.
        assert_eq!(estimate_tokens(30), 10);
        assert_eq!(estimate_tokens(0), 0);
        // Rounds up (ceil) so a nonzero source never reads as 0 tokens.
        assert_eq!(estimate_tokens(1), 1);
    }

    #[test]
    fn descendant_tree_walks_full_depth() {
        // 1 → 2 → 4, and 1 → 3. Root 1 yields all four, depths 0/1/1/2.
        let ps = "1 0 1.0 100 root\n2 1 2.0 200 child\n3 1 0.5 50 child2\n4 2 3.0 300 grandchild\n99 98 0.0 10 unrelated";
        let procs = parse_ps(ps);
        let tree = descendant_tree(&procs, 1);
        let pids: Vec<u32> = tree.iter().map(|(p, _)| *p).collect();
        assert_eq!(pids, vec![1, 2, 3, 4]);
        let total_rss: u64 = tree
            .iter()
            .filter_map(|(p, _)| procs.get(p))
            .map(|p| p.rss_kb)
            .sum();
        assert_eq!(total_rss, 650);
        // Depth of the grandchild is 2.
        assert_eq!(tree.iter().find(|(p, _)| *p == 4).map(|(_, d)| *d), Some(2));
    }

    #[test]
    fn agg_event_stat_sums_event_granularity() {
        let mut stats = JsonlStats::default();
        for (k, ms) in [("PreToolUse", 40u64), ("PreToolUse:Bash", 60), ("Stop", 10)] {
            let s = stats.hooks.entry(k.to_string()).or_default();
            s.count = 1;
            s.total_ms = ms;
            s.last_ms = ms;
        }
        let agg = agg_event_stat(&stats, "PreToolUse");
        assert_eq!(agg.count, 2);
        assert_eq!(agg.total_ms, 100);
        // The unrelated "Stop" event is not folded in.
        assert_eq!(agg_event_stat(&stats, "Stop").count, 1);
    }

    #[test]
    fn ingest_measures_start_context_attachments() {
        let mut stats = JsonlStats::default();
        ingest_line(
            &mut stats,
            &serde_json::json!({"type":"attachment","attachment":{
                "type":"skill_listing","content":"abcdef"}}),
        );
        ingest_line(
            &mut stats,
            &serde_json::json!({"type":"attachment","attachment":{
                "type":"mcp_instructions_delta",
                "addedNames":["srv"],"addedBlocks":["0123456789"]}}),
        );
        ingest_line(
            &mut stats,
            &serde_json::json!({"type":"attachment","attachment":{
                "type":"deferred_tools_delta",
                "addedNames":["mcp__srv__foo","WebFetch"]}}),
        );
        assert_eq!(stats.skill_listing_bytes, 6);
        assert_eq!(stats.mcp_instr_bytes.get("srv"), Some(&10));
        let (bytes, n) = deferred_bytes_for(&stats, "srv");
        assert_eq!((bytes, n), ("mcp__srv__foo".len(), 1));
    }

    #[test]
    fn short_time_extracts_hhmm() {
        assert_eq!(short_time("2026-07-23T18:46:42.086Z"), "18:46");
        assert_eq!(short_time("garbage"), "");
    }

    #[test]
    fn compaction_boundary_marks_prior_reads_evicted() {
        let mut stats = JsonlStats::default();
        // A read before the boundary, then a compaction, then a read after.
        ingest_line(
            &mut stats,
            &serde_json::json!({"type":"attachment","timestamp":"2026-01-01T09:00:00Z",
                "attachment":{"type":"file","filename":"/a.txt","content":"hello"}}),
        );
        let before_seq = stats.ctx_reads["/a.txt"].seq;
        ingest_line(
            &mut stats,
            &serde_json::json!({"type":"system","subtype":"compact_boundary",
                "timestamp":"2026-01-01T10:00:00Z"}),
        );
        ingest_line(
            &mut stats,
            &serde_json::json!({"type":"attachment","timestamp":"2026-01-01T11:00:00Z",
                "attachment":{"type":"file","filename":"/b.txt","content":"world"}}),
        );
        assert_eq!(stats.compaction_count, 1);
        assert_eq!(stats.last_compaction_at, "10:00");
        // /a.txt read before the boundary is evicted; /b.txt after is live.
        assert!(before_seq <= stats.last_compaction_seq);
        assert!(stats.ctx_reads["/b.txt"].seq > stats.last_compaction_seq);
        assert_eq!(stats.ctx_reads["/a.txt"].at, "09:00");
    }

    #[test]
    fn frontmatter_field_reads_description() {
        let dir = std::env::temp_dir().join(format!("harness-fm-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("SKILL.md");
        std::fs::write(&f, "---\nname: foo\ndescription: does a thing\n---\nbody\n")
            .expect("write");
        assert_eq!(
            frontmatter_field(&f, "description").as_deref(),
            Some("does a thing")
        );
        assert_eq!(frontmatter_field(&f, "name").as_deref(), Some("foo"));
        assert_eq!(frontmatter_field(&f, "missing"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_counts_mcp_and_skill_calls() {
        let mut stats = JsonlStats::default();
        let line = serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_use", "name": "mcp__claude-in-chrome__navigate", "input": {}},
                {"type": "tool_use", "name": "Skill", "input": {"skill": "telegram"}}
            ], "usage": {"input_tokens": 10, "output_tokens": 5}}
        });
        ingest_line(&mut stats, &line);
        assert_eq!(stats.mcp_calls["claude-in-chrome"], 1);
        assert_eq!(stats.skill_calls["telegram"], 1);
        assert_eq!(stats.in_tokens, 10);
    }

    /// The stash is the only record of what a disable removed, so it must be
    /// durable BEFORE the edit lands. If it cannot be written, the disable must
    /// not happen at all -- a refused disable is recoverable, a delete recorded
    /// nowhere is not.
    #[test]
    fn a_failed_stash_leaves_the_hook_in_place() {
        let dir = std::env::temp_dir().join(format!("harness-stashfail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("settings.json");
        let original = serde_json::json!({
            "hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
                {"type": "command", "command": "/bin/echo hi"}
            ]}]}
        });
        std::fs::write(
            &file,
            serde_json::to_string_pretty(&original).expect("serialize"),
        )
        .expect("write settings");

        // Make the stash unwritable: its PARENT is a regular file, so both
        // create_dir_all and the temp-file create beneath it must fail.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "not a directory").expect("write blocker");
        let stash = blocker.join("harness-disabled.json");

        let entry = HookEntry {
            file: file.clone(),
            event: "PreToolUse".to_string(),
            matcher: "*".to_string(),
            matcher_idx: 0,
            hook_idx: 0,
            name: "echo".to_string(),
            command: "/bin/echo hi".to_string(),
            script: None,
        };

        assert!(
            !remove_hook_from_file(&entry, &stash),
            "disable must refuse when it cannot record what it removed"
        );
        let after = read_json(&file).expect("settings still readable");
        assert_eq!(after, original, "settings must be byte-for-byte untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Disabling an MCP must stash enough to reconstruct it: the full
    /// definition, not just the name. The env is the part that matters -- it is
    /// where credentials live, and it is what the old boolean-only code threw
    /// away irrecoverably.
    #[test]
    fn disabling_an_mcp_stashes_its_whole_definition() {
        let dir = std::env::temp_dir().join(format!("harness-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("claude.json");
        let stash = dir.join("state").join(HARNESS_STASH_FILE);

        let definition = serde_json::json!({
            "command": "npx",
            "args": ["-y", "some-server"],
            "env": {"API_KEY": "secret-value"},
        });
        std::fs::write(
            &file,
            serde_json::to_string(&serde_json::json!({
                "mcpServers": {"keepme": {"command": "other"}, "gone": definition},
                "projects": {"/w": {"mcpServers": {"scoped": definition}}},
            }))
            .expect("serialize"),
        )
        .expect("write config");

        let entry = McpEntry {
            name: "gone".to_string(),
            project: None,
        };
        assert!(remove_mcp_from_file(&entry, &file, &stash));

        // Removed from config, and only the one key -- siblings survive.
        let after = read_json(&file).expect("config still readable");
        assert!(after["mcpServers"].get("gone").is_none());
        assert!(after["mcpServers"].get("keepme").is_some());

        // The stash holds the whole definition, including the env.
        let stashed = read_json(&stash).expect("stash written");
        let rec = &stashed[0];
        assert_eq!(rec["kind"], "mcp");
        assert_eq!(rec["name"], "gone");
        assert_eq!(rec["project"], serde_json::Value::Null);
        assert_eq!(rec["removed"], definition);

        // Project-scoped rows record their scope, so enable can put it back
        // where it came from rather than promoting it to global.
        let scoped = McpEntry {
            name: "scoped".to_string(),
            project: Some("/w".to_string()),
        };
        assert!(remove_mcp_from_file(&scoped, &file, &stash));
        let stashed = read_json(&stash).expect("stash written");
        assert_eq!(stashed.as_array().map(|a| a.len()), Some(2), "appends");
        assert_eq!(stashed[1]["project"], "/w");
        assert_eq!(stashed[1]["removed"], definition);

        // A name that is not there changes nothing and stashes nothing.
        let missing = McpEntry {
            name: "never".to_string(),
            project: None,
        };
        assert!(!remove_mcp_from_file(&missing, &file, &stash));
        let stashed = read_json(&stash).expect("stash readable");
        assert_eq!(stashed.as_array().map(|a| a.len()), Some(2), "no phantom");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The round trip the stash exists for: disable then enable must put the
    /// entry back exactly where it came from, with its definition intact, and
    /// consume the stash record so a second enable is not offered.
    #[test]
    fn disable_then_enable_restores_the_original_exactly() {
        let dir = std::env::temp_dir().join(format!("harness-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let stash = dir.join("state").join(HARNESS_STASH_FILE);

        // --- MCP, project-scoped: must come back to its project, not global.
        let cfg = dir.join("claude.json");
        let definition = serde_json::json!({
            "command": "npx",
            "env": {"API_KEY": "secret-value"},
        });
        let before = serde_json::json!({
            "mcpServers": {"global-one": {"command": "keep"}},
            "projects": {"/w": {"mcpServers": {"scoped": definition}}},
        });
        std::fs::write(&cfg, serde_json::to_string(&before).expect("serialize"))
            .expect("write cfg");

        let entry = McpEntry {
            name: "scoped".to_string(),
            project: Some("/w".to_string()),
        };
        assert!(remove_mcp_from_file(&entry, &cfg, &stash));
        assert!(restore_from_stash(&stash, "mcp", "scoped", Some("/w")));
        assert_eq!(
            read_json(&cfg).expect("cfg readable"),
            before,
            "config must be identical to before the disable"
        );
        // Record consumed: a second enable has nothing to restore.
        assert!(!restore_from_stash(&stash, "mcp", "scoped", Some("/w")));

        // --- Hook: the matcher group is PRUNED when its last hook goes, so
        // restore has to recreate the group rather than assume it survived.
        let settings = dir.join("settings.json");
        let before = serde_json::json!({
            "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "/bin/echo only"}
            ]}]}
        });
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&before).expect("serialize"),
        )
        .expect("write settings");

        let hook = HookEntry {
            file: settings.clone(),
            event: "PreToolUse".to_string(),
            matcher: "Bash".to_string(),
            matcher_idx: 0,
            hook_idx: 0,
            name: "only".to_string(),
            command: "/bin/echo only".to_string(),
            script: None,
        };
        assert!(remove_hook_from_file(&hook, &stash));
        // Precondition for the interesting case: the group really is gone.
        let emptied = read_json(&settings).expect("settings readable");
        assert_eq!(
            emptied["hooks"]["PreToolUse"].as_array().map(|a| a.len()),
            Some(0),
            "the matcher group should have been pruned"
        );
        assert!(restore_from_stash(&stash, "hook", "only", None));
        assert_eq!(
            read_json(&settings).expect("settings readable"),
            before,
            "hook must return to its original matcher group"
        );

        // Unknown identities and kinds restore nothing rather than guessing.
        assert!(!restore_from_stash(&stash, "hook", "never-existed", None));
        assert!(!restore_from_stash(&stash, "skill", "only", None));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A replace must be all-or-nothing, and must not litter temp files next to
    /// the user's config when it fails.
    #[test]
    fn write_atomic_replaces_wholly_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("harness-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("conf.json");
        std::fs::write(&f, "{\"old\":true}").expect("seed");

        assert!(write_atomic(&f, "{\"new\":true}"));
        assert_eq!(
            std::fs::read_to_string(&f).expect("read"),
            "{\"new\":true}"
        );

        // Exactly the destination remains -- no leftover temp file beside it.
        let left: Vec<String> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["conf.json".to_string()], "stray files: {left:?}");

        // Undrivable destination fails cleanly rather than half-writing.
        assert!(!write_atomic(&dir.join("nope").join("conf.json"), "{}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_hook_prunes_and_stashes() {
        let dir = std::env::temp_dir().join(format!("harness-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("settings.json");
        let stash = dir.join("stash.json");
        std::fs::write(
            &file,
            r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        )
        .expect("write settings");
        let entry = HookEntry {
            file: file.clone(),
            event: "Stop".to_string(),
            matcher: String::new(),
            matcher_idx: 0,
            hook_idx: 0,
            name: "echo".to_string(),
            command: "echo hi".to_string(),
            script: None,
        };
        assert!(remove_hook_from_file(&entry, &stash));
        let root = read_json(&file).expect("reread");
        assert_eq!(root["hooks"]["Stop"].as_array().map(|a| a.len()), Some(0));
        assert!(stash.is_file());
        // A second removal drifts (entry gone) and must refuse.
        assert!(!remove_hook_from_file(&entry, &stash));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
