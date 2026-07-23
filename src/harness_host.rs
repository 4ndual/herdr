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

#[derive(Default)]
struct JsonlStats {
    hooks: HashMap<String, HookStat>,
    mcp_calls: HashMap<String, u64>,
    skill_calls: HashMap<String, u64>,
    tool_errors: u64,
    ctx_files: HashMap<String, usize>,
    in_tokens: u64,
    out_tokens: u64,
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
    skills: Vec<(String, PathBuf)>,
    profile: Option<String>,
    /// Which agent CLI drives the focused session: "claude", "cx" (Claude Code
    /// routed through the CCR gateway), "codex", "pi", or "terminal". Derived on
    /// the full rescan (cx vs claude comes from which projects root the resolved
    /// transcript lives under). Empty until first resolved; callers fall back to
    /// "claude".
    agent_kind: String,
    session_lines: Vec<(String, String, String, u8)>, // (label, meta, tooltip, indent)
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

/// Republish the harness render input from the focused Claude pane. Called on
/// the dash tick (~1 Hz) next to [`crate::historial_host::tick`].
pub fn tick(app: &mut App) {
    let focus = crate::plugin_focus::focus_input(&app.state);
    // Resolve the app-derived session facts (live agent pid, herdr pane id and
    // attach command, any inner tmux) the same way `handle_pane_process_info`
    // reaches the pane's foreground job, before locking the cache. This works in
    // the headless server tick path too, since that is where this tick runs for
    // client/server mode.
    let env = resolve_session_env(app);
    let json = {
        let mut guard = match CACHE.lock() {
            Ok(g) => g,
            Err(err) => {
                tracing::warn!(error = %err, "harness cache mutex poisoned; skipping tick");
                return;
            }
        };
        let cache = guard.get_or_insert_with(HarnessCache::default);
        refresh(cache, &focus, &env);
        render_json(cache)
    };
    if let Some(json) = json {
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
    let clients = run_capture("tmux", &["-S", socket, "list-clients", "-t", &session, "-F", "x"])
        .map(|c| c.lines().count())
        .unwrap_or(0);
    Some(TmuxAttach {
        attach_cmd: format!("tmux -S {socket} attach -t {session}"),
        session,
        clients,
    })
}

fn refresh(
    cache: &mut HarnessCache,
    focus: &herdr_ui_plugin_api::FocusInput,
    env: &SessionEnv,
) {
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
    if cache.tick % FULL_RESCAN_TICKS == 0 {
        full_rescan(cache, &sid, &cwd, env);
    }
    cache.tick = cache.tick.wrapping_add(1);
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

fn ingest_line(stats: &mut JsonlStats, v: &serde_json::Value) {
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
                        stats.ctx_files.insert(name.to_string(), len);
                    }
                }
                Some("nested_memory") => {
                    if let Some(name) = a.get("path").and_then(|f| f.as_str()) {
                        let len = a
                            .get("content")
                            .and_then(|c| c.as_str())
                            .map(|c| c.len())
                            .unwrap_or(0);
                        stats.ctx_files.insert(name.to_string(), len);
                    }
                }
                Some("invoked_skills") => {
                    if let Some(skills) = a.get("skills").and_then(|s| s.as_array()) {
                        for s in skills {
                            if let Some(name) = s.get("name").and_then(|n| n.as_str()) {
                                *stats.skill_calls.entry(name.to_string()).or_default() += 1;
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

/// First token of the command that is an existing file (the hook script), for
/// the open-in-editor affordance.
fn hook_script_path(command: &str) -> Option<PathBuf> {
    command
        .split_whitespace()
        .map(PathBuf::from)
        .find(|p| p.is_absolute() && p.is_file())
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

fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn full_rescan(cache: &mut HarnessCache, sid: &str, cwd: &str, env: &SessionEnv) {
    let home = home_dir();

    // Which agent drives this session. The target is only ever set for a Claude
    // pane (see `refresh`), so the split is claude vs cx: a cx session (Claude
    // Code routed through the CCR gateway) writes its transcript under
    // `~/.claude-code-router/profiles/<p>/claude/projects/…` rather than
    // `~/.claude/projects/…`, so the resolved transcript path tells them apart.
    cache.agent_kind = match &cache.transcript {
        Some(p) if p.to_string_lossy().contains(".claude-code-router/profiles/") => "cx",
        _ => "claude",
    }
    .to_string();
    let agent_kind = cache.agent_kind.clone();

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
                cache.skills.push((name, p.join("SKILL.md")));
            }
        }
        cache.skills.sort();
    }

    // Session / process lines.
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
    // Claude process: match the session id in a command line. On a match we also
    // emit one indented row per child process (up to 5), mirroring the parent's
    // pid/cpu/rss and carrying the full command line in the tooltip.
    let mut proc_line = ("process: n/det".to_string(), String::new(),
        format!("no {agent_kind} process with this session id in its command line (fresh sessions may not carry --resume)"), 0u8);
    let mut child_lines: Vec<(String, String, String, u8)> = Vec::new();
    if let Some(ps) = run_capture("ps", &["-axo", "pid=,ppid=,%cpu=,rss=,command="]) {
        let rows: Vec<&str> = ps.lines().collect();
        // Prefer the pid herdr resolved from the pane's foreground job (works
        // for fresh sessions with no --resume in argv); fall back to the sid
        // grep for older/resumed sessions that carry it.
        let by_pid = env.agent_pid.and_then(|pid| {
            let pidstr = pid.to_string();
            rows.iter()
                .find(|l| l.split_whitespace().next() == Some(pidstr.as_str()))
        });
        if let Some(line) = by_pid.or_else(|| rows.iter().find(|l| l.contains(sid))) {
            let mut it = line.split_whitespace();
            let pid = it.next().unwrap_or("?").to_string();
            let _ppid = it.next();
            let cpu = it.next().unwrap_or("?").to_string();
            let rss_kb: u64 = it.next().and_then(|r| r.parse().ok()).unwrap_or(0);
            let children: Vec<&&str> = rows
                .iter()
                .filter(|l| l.split_whitespace().nth(1) == Some(pid.as_str()))
                .collect();
            proc_line = (
                format!("{agent_kind} pid {pid}"),
                format!("{cpu}% {}MB +{}", rss_kb / 1024, children.len()),
                format!("cpu {cpu}% · rss {}MB · {} child procs", rss_kb / 1024, children.len()),
                0,
            );
            for child in children.iter().take(5) {
                let mut cit = child.split_whitespace();
                let cpid = cit.next().unwrap_or("?").to_string();
                let _cppid = cit.next();
                let ccpu = cit.next().unwrap_or("?").to_string();
                let crss_kb: u64 = cit.next().and_then(|r| r.parse().ok()).unwrap_or(0);
                let command: String = cit.collect::<Vec<_>>().join(" ");
                let base = command
                    .split_whitespace()
                    .next()
                    .map(|t| Path::new(t))
                    .map(basename)
                    .unwrap_or_else(|| "?".to_string());
                let cmd_trunc: String = command.chars().take(200).collect();
                child_lines.push((
                    format!("└ {base}"),
                    format!("{cpid} {ccpu}% {}MB", crss_kb / 1024),
                    format!("cmd: {cmd_trunc}"),
                    1,
                ));
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

fn fmt_tokens_approx(bytes: usize) -> String {
    let tok = bytes / 4;
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

fn context_section(cache: &HarnessCache) -> HarnessSection {
    let mut rows = Vec::new();
    let mut total = 0usize;
    for (i, path) in cache.ctx_paths.iter().take(MAX_ROWS).enumerate() {
        let bytes = std::fs::metadata(path).map(|m| m.len() as usize).unwrap_or(0);
        total += bytes;
        rows.push(HarnessRow {
            key: format!("ctx:{i}"),
            label: display_path(path),
            meta: format!("{} {}", fmt_bytes(bytes), fmt_tokens_approx(bytes)),
            state: RowState::None,
            tooltip: format!("{}\nclick to open in a pane", display_path(path)),
            openable: true,
            can_disable: false,
            indent: 0,
        });
    }
    // Files the transcript itself reports as loaded (Read/nested_memory).
    let mut seen: Vec<(&String, &usize)> = cache.stats.ctx_files.iter().collect();
    seen.sort_by(|a, b| b.1.cmp(a.1));
    for (name, len) in seen.into_iter().take(MAX_ROWS.saturating_sub(rows.len())) {
        total += *len;
        let idx = cache.ctx_paths.len() + rows.len();
        rows.push(HarnessRow {
            key: format!("ctx:{idx}"),
            label: basename(Path::new(name)),
            meta: format!("{} {}", fmt_bytes(*len), fmt_tokens_approx(*len)),
            state: RowState::None,
            tooltip: format!("{name}\nloaded in-session (content-length proxy)"),
            openable: false,
            can_disable: false,
            indent: 1,
        });
    }
    HarnessSection {
        title: "CONTEXT".to_string(),
        summary: fmt_tokens_approx(total),
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
        })
        .collect();
    HarnessSection {
        title: "SESSION".to_string(),
        summary: format!(
            "{}k in / {}k out",
            cache.stats.in_tokens / 1000,
            cache.stats.out_tokens / 1000
        ),
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
        .chain(cache.hooks.iter().enumerate().filter(|(_, e)| !is_project(e)))
        .collect();
    for (i, entry) in ordered {
        // Transcript hook names look like "PreToolUse:Bash" or "UserPromptSubmit";
        // match stats by event prefix + command-derived name when possible.
        let stat = cache
            .stats
            .hooks
            .iter()
            .find(|(k, _)| k.starts_with(&entry.event))
            .map(|(_, s)| s);
        let (count, meta, errs, timing) = match stat {
            Some(s) => {
                let count = if s.count > 0 { s.count } else { s.ctx_count };
                let avg = if s.count > 0 { s.total_ms / s.count } else { 0 };
                let last = s.last_ms;
                let mut meta = format!("x{count} {avg}ms/{last}ms");
                if s.err_count > 0 {
                    meta.push_str(&format!(" !{}", s.err_count));
                }
                (count, meta, (s.err_count, s.err_ms), (avg, last))
            }
            None => (0, "x0".to_string(), (0, 0), (0, 0)),
        };
        let active = stat
            .and_then(|s| s.last_seen)
            .map(|t| t.elapsed() < ACTIVE_WINDOW)
            .unwrap_or(false);
        let scope = if entry.file.starts_with(home_dir()) {
            "user"
        } else {
            "project"
        };
        let mut tooltip = format!(
            "{} ({})\nevent {} · matcher \"{}\"\ncmd: {}\nfired x{count} this session · avg {}ms · last {}ms · errors {} ({}ms lost)\ntoken cost: not determinable\nright-click to disable",
            entry.name, scope, entry.event, entry.matcher, entry.command, timing.0, timing.1, errs.0, errs.1
        );
        if entry.script.is_some() {
            tooltip.push_str("\nclick opens the hook script");
        }
        rows.push(HarnessRow {
            key: format!("hook:{i}"),
            label: format!("{} {}", entry.event, entry.name),
            meta,
            state: if active { RowState::Active } else { RowState::Idle },
            tooltip,
            openable: entry.script.is_some(),
            can_disable: true,
            indent: 0,
        });
    }
    rows.truncate(MAX_ROWS * 2);
    HarnessSection {
        title: "HOOKS".to_string(),
        summary: format!("{}", cache.hooks.len()),
        rows,
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
        rows.push(HarnessRow {
            key: format!("mcp:{i}"),
            label: entry.name.clone(),
            meta: format!("x{calls}"),
            state: if calls > 0 { RowState::Idle } else { RowState::Off },
            tooltip: format!(
                "MCP server ({scope} scope)\ncalls this session: {calls}\ntoken cost: not determinable\nright-click to disable"
            ),
            openable: false,
            can_disable: true,
            indent: 0,
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
        });
    }
    HarnessSection {
        title: "MCPS".to_string(),
        summary: format!("{}", rows.len()),
        rows,
    }
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
                tooltip: "claude code plugin (enabledPlugins in ~/.claude/settings.json)\nright-click to disable".to_string(),
                openable: false,
                can_disable: !off,
                indent: 0,
            }
        })
        .collect();
    HarnessSection {
        title: "PLUGINS".to_string(),
        summary: format!("{}", cache.plugins.len()),
        rows,
    }
}

fn skills_section(cache: &HarnessCache) -> HarnessSection {
    let mut rows: Vec<HarnessRow> = Vec::new();
    // Invoked skills first (with counts), then the rest of the catalog.
    let mut entries: Vec<(usize, &(String, PathBuf), u64)> = cache
        .skills
        .iter()
        .enumerate()
        .map(|(i, e)| (i, e, cache.stats.skill_calls.get(&e.0).copied().unwrap_or(0)))
        .collect();
    entries.sort_by(|a, b| b.2.cmp(&a.2).then(a.1 .0.cmp(&b.1 .0)));
    for (i, (name, path), calls) in entries.into_iter().take(MAX_ROWS) {
        rows.push(HarnessRow {
            key: format!("skill:{i}"),
            label: name.clone(),
            meta: if calls > 0 { format!("x{calls}") } else { String::new() },
            state: if calls > 0 { RowState::Idle } else { RowState::Off },
            tooltip: format!(
                "{}\ninvoked x{calls} this session\ntoken cost: not determinable\nskills load on demand — no disable (archive the skill dir instead)\nclick opens SKILL.md",
                display_path(path)
            ),
            openable: path.is_file(),
            can_disable: false,
            indent: 0,
        });
    }
    HarnessSection {
        title: "SKILLS".to_string(),
        summary: format!("{}", cache.skills.len()),
        rows,
    }
}

// ── Host-resolved actions ──────────────────────────────────────────────────

/// Snapshot of what a queued action key resolves to (extracted under the cache
/// lock, applied after releasing it).
enum ResolvedAction {
    Open(PathBuf),
    DisableHook(HookEntry, bool),
    DisableMcp(McpEntry),
    DisablePlugin(String),
}

fn resolve_action(action: &PendingHarnessAction) -> Option<ResolvedAction> {
    let guard = CACHE.lock().ok()?;
    let cache = guard.as_ref()?;
    let (kind, idx) = action.key.split_once(':')?;
    let profile_wide = action.kind == HarnessActionKind::DisableProfile;
    match (action.kind, kind) {
        (HarnessActionKind::Open, "ctx") => {
            let i: usize = idx.parse().ok()?;
            cache.ctx_paths.get(i).cloned().map(ResolvedAction::Open)
        }
        (HarnessActionKind::Open, "hook") => {
            let i: usize = idx.parse().ok()?;
            cache
                .hooks
                .get(i)
                .and_then(|h| h.script.clone())
                .map(ResolvedAction::Open)
        }
        (HarnessActionKind::Open, "skill") => {
            let i: usize = idx.parse().ok()?;
            cache
                .skills
                .get(i)
                .map(|(_, p)| ResolvedAction::Open(p.clone()))
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
    let Ok(json) = serde_json::to_string_pretty(&root) else {
        return false;
    };
    if let Err(err) = std::fs::write(&entry.file, json) {
        tracing::warn!(error = %err, file = %entry.file.display(), "harness disable: settings write failed");
        return false;
    }
    // Append to the stash (best effort, reversible-by-hand record).
    let record = serde_json::json!({
        "file": entry.file.to_string_lossy(),
        "event": entry.event,
        "matcher": entry.matcher,
        "removed": removed,
    });
    let mut stashed: Vec<serde_json::Value> = read_json(stash)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    stashed.push(record);
    if let Ok(json) = serde_json::to_string_pretty(&stashed) {
        if let Err(err) = std::fs::write(stash, json) {
            tracing::warn!(error = %err, "harness disable: stash write failed");
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
                        full_rescan(cache, &sid, &cwd, &env);
                    }
                    render_json(cache)
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
        let stash =
            crate::plugin_paths::plugin_state_dir(crate::app::HARNESS_PLUGIN_ID)
                .join("harness-disabled.json");
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
        let Some(mut root) = read_json(&file) else {
            return false;
        };
        let removed = match &entry.project {
            None => root
                .get_mut("mcpServers")
                .and_then(|m| m.as_object_mut())
                .and_then(|m| m.remove(&entry.name))
                .is_some(),
            Some(cwd) => root
                .get_mut("projects")
                .and_then(|p| p.get_mut(cwd))
                .and_then(|p| p.get_mut("mcpServers"))
                .and_then(|m| m.as_object_mut())
                .and_then(|m| m.remove(&entry.name))
                .is_some(),
        };
        if !removed {
            return false;
        }
        let Ok(json) = serde_json::to_string(&root) else {
            return false;
        };
        if let Err(err) = std::fs::write(&file, json) {
            tracing::warn!(error = %err, "harness disable: ~/.claude.json write failed");
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
    use super::*;

    #[test]
    fn hook_display_name_skips_generic_entrypoints() {
        assert_eq!(
            hook_display_name("/Users/x/.bun/bin/bun /Users/x/.claude/hooks/pkg-guard/hook.ts"),
            "pkg-guard"
        );
        assert_eq!(hook_display_name("bash /Users/x/.claude/hooks/orchestrator/emit.sh"), "orchestrator");
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
        assert_eq!(
            root["hooks"]["Stop"].as_array().map(|a| a.len()),
            Some(0)
        );
        assert!(stash.is_file());
        // A second removal drifts (entry gone) and must refuse.
        assert!(!remove_hook_from_file(&entry, &stash));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
