//! swap-here overlay TUI — "resume this session anywhere".
//!
//! Runs inside a herdr `placement = "overlay"` plugin pane, the same
//! terminal-in-a-floating-pane mechanism the agent launcher's popup and the
//! button reference use; the host paints the chrome and this process owns the
//! keyboard and mouse natively via crossterm.
//!
//! It shows the focused pane's live session and offers TWO axes for reopening
//! it — the RUNTIME (claude/codex/pi/jcode) and the HARNESS (any fleet harness,
//! the one that owns this pane, or none at all). Every decision about what that
//! actually means for the conversation belongs to `harness swap-here`
//! (harness-fleet/go/swaphere.go), which this overlay only drives: the picker
//! must never grow a second, drifting copy of the swap algorithm. That is also
//! why the `plan:` line is `swap-here --dry-run` output rather than a guess —
//! the preview and the action come from the same code.

use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

const RUNTIMES: [&str; 4] = ["claude", "codex", "pi", "jcode"];
/// Rows of the harness list shown at once. The fleet can hold dozens; a list
/// that runs off the overlay with no scroll affordance is the defect the button
/// reference overlay was created to fix, so this one scrolls in a window.
const HARNESS_WINDOW: usize = 7;

fn home() -> PathBuf {
    // An empty HOME turns every join() into a relative path rooted at whatever
    // cwd the herdr server happens to have — a plugin pane inherits that
    // environment, and a systemd unit can hand it over scrubbed.
    match std::env::var("HOME") {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from("/"),
    }
}

// ── source pane ────────────────────────────────────────────────────────────

#[derive(Default)]
struct Source {
    pane_id: String,
    tab_id: String,
    cwd: String,
    runtime: String,
    harness: String,
    session: String,
    error: Option<String>,
}

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

fn herdr_json(args: &[&str]) -> Option<serde_json::Value> {
    let out = Command::new(herdr_bin()).args(args).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().rfind(|line| !line.trim().is_empty())?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    value.get("result").cloned()
}

/// The overlay's plugin context is captured BEFORE the overlay takes focus, so
/// `focused_pane_id` is the pane the user was looking at — not this one.
/// `herdr pane list` is the fallback for a manual run.
fn source_pane_id() -> Option<String> {
    if let Ok(raw) = std::env::var("HERDR_PLUGIN_CONTEXT_JSON") {
        if let Ok(context) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(pane) = context.get("focused_pane_id").and_then(|v| v.as_str()) {
                return Some(pane.to_string());
            }
        }
    }
    let panes = herdr_json(&["pane", "list"])?;
    let panes = panes.get("panes")?.as_array()?;
    panes
        .iter()
        .find(|pane| pane.get("focused").and_then(|v| v.as_bool()).unwrap_or(false))
        .or_else(|| panes.first())?
        .get("pane_id")?
        .as_str()
        .map(str::to_string)
}

fn load_source() -> Source {
    let mut source = Source::default();
    let Some(pane_id) = source_pane_id() else {
        source.error = Some("no focused pane".into());
        return source;
    };
    source.pane_id = pane_id.clone();
    let Some(result) = herdr_json(&["pane", "get", &pane_id]) else {
        source.error = Some(format!("pane {pane_id} not found"));
        return source;
    };
    let pane = result.get("pane").cloned().unwrap_or_default();
    let text = |key: &str| {
        pane.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    source.tab_id = text("tab_id");
    source.cwd = {
        let cwd = text("cwd");
        if cwd.is_empty() {
            text("foreground_cwd")
        } else {
            cwd
        }
    };
    let session = pane.get("agent_session").cloned().unwrap_or_default();
    source.runtime = session
        .get("agent")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| text("agent"));
    source.session = session
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Not an error when empty: herdr only reports a session id once the
    // runtime's integration hook does, and swap-here recovers the session from
    // the harness state or the transcript. Say so rather than blocking a pick.
    if source.session.is_empty() {
        source.session = "(resolved at swap time)".into();
    }
    source.harness = owning_harness(&source.cwd);
    // A plain terminal pane has no agent at all. That is still swappable —
    // "reopen this FOLDER in a runtime" — so it is stated, not refused.
    if source.runtime.is_empty() {
        source.runtime = "none".into();
    }
    source
}

/// Mirrors `harnessForCwd` in swaphere.go for DISPLAY only: the real mapping
/// (including the external-cwd fallback) is the Go side's, and the picker never
/// second-guesses it — leaving the harness row on "(this pane)" hands the
/// decision back to it.
fn owning_harness(cwd: &str) -> String {
    if let Ok(name) = std::env::var("HARNESS_NAME") {
        if !name.is_empty() {
            return name;
        }
    }
    let root = harness_root();
    let Ok(rest) = PathBuf::from(cwd).strip_prefix(&root).map(PathBuf::from) else {
        return String::new();
    };
    let Some(first) = rest.iter().next() else {
        return String::new();
    };
    let name = first.to_string_lossy().to_string();
    if root.join(&name).join("harness.json").is_file() {
        name
    } else {
        String::new()
    }
}

// ── fleet ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Harness {
    name: String,
    runtime: String,
    /// A profile points at a pre-existing config dir (`~/.claude`); it is a
    /// real swap destination but never a supervised one, so it is marked.
    profile: bool,
}

fn harness_root() -> PathBuf {
    std::env::var("HARNESS_ROOT")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("harnesses"))
}

/// The filesystem is the source of truth on purpose: enumeration must work with
/// the fleet daemon down and even with the `harness` binary missing, because
/// this list is also what tells the user WHY a swap is unavailable.
fn load_fleet() -> Vec<Harness> {
    let Ok(reader) = std::fs::read_dir(harness_root()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in reader.filter_map(Result::ok) {
        let Ok(raw) = std::fs::read_to_string(entry.path().join("harness.json")) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        out.push(Harness {
            name: value
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string()),
            runtime: value
                .get("runtime")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .unwrap_or("claude")
                .to_string(),
            profile: value
                .get("profile")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `harness` (a.k.a. `tidus` — same binary). Resolved the way the dash scripts
/// do it, because a plugin pane inherits the herdr server's PATH, which may
/// have none of the user bin dirs an interactive shell adds.
fn harness_bin() -> Option<String> {
    if let Ok(bin) = std::env::var("HARNESS_BIN") {
        if !bin.is_empty() {
            return Some(bin);
        }
    }
    let probe = Command::new("sh").arg("-lc").arg("command -v harness").output();
    if let Ok(out) = probe {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(path);
        }
    }
    let home = home();
    [
        home.join(".local/bin/harness"),
        home.join("Projects/harness-fleet/bin/harness"),
        home.join(".claude/plugins/cache/harness-fleet/harness-tidus/1.0.1/bin/harness"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .map(|path| path.to_string_lossy().to_string())
}

fn installed(bin: &str) -> bool {
    let on_path = Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {bin}"))
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false);
    if on_path {
        return true;
    }
    let home = home();
    [".local/bin", ".bun/bin", ".npm-global/bin"]
        .iter()
        .map(|dir| home.join(dir).join(bin))
        .chain(
            ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"]
                .iter()
                .map(|dir| PathBuf::from(dir).join(bin)),
        )
        .any(|path| path.exists())
}

// ── destinations ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum DestKind {
    /// Pass no --harness: swap-here infers the owner from the pane cwd, using
    /// the same rules (including the external-project fallback) it always has.
    Inherit,
    /// Pass --harness "": explicitly ambient — the operator's own config, no
    /// fleet identity. Named so the "no-harness trap" is a choice, not a slip.
    Ambient,
    Named,
}

struct Dest {
    kind: DestKind,
    name: String,
    note: String,
}

fn destinations(source: &Source, fleet: &[Harness]) -> Vec<Dest> {
    let mut out = vec![Dest {
        kind: DestKind::Inherit,
        name: if source.harness.is_empty() {
            "(this pane — inferred)".into()
        } else {
            format!("(this pane — {})", source.harness)
        },
        note: "keep whatever harness owns this pane".into(),
    }];
    out.push(Dest {
        kind: DestKind::Ambient,
        name: "(none — your own profile)".into(),
        note: "no fleet identity: no status, events or notifications".into(),
    });
    for harness in fleet {
        out.push(Dest {
            kind: DestKind::Named,
            name: harness.name.clone(),
            note: if harness.profile {
                format!("profile · runtime {}", harness.runtime)
            } else {
                format!("harness · runtime {}", harness.runtime)
            },
        });
    }
    out
}

// ── app ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Click {
    Runtime(usize),
    Open(usize),
    Mode(usize),
    Dest(usize),
    Reopen,
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Runtime,
    Open,
    Mode,
    Dest,
    Reopen,
    Cancel,
}

enum Outcome {
    Continue,
    Cancelled,
    Reopen,
}

const MODES: [(&str, &str); 2] = [
    ("session", "carried as a native session in the target (default)"),
    ("handoff", "rendered as a HANDOFF-*.md the target reads first"),
];

/// One picker owns both gestures now (the 🔄 and 🧬 buttons were merged):
/// replace = swap this pane in place; fork = new tab, this pane keeps running,
/// nothing persisted.
const OPENS: [(&str, &str); 2] = [
    ("replace", "swap THIS pane in place (default)"),
    ("fork", "new tab — this pane keeps running, nothing persisted"),
];

struct App {
    source: Source,
    dests: Vec<Dest>,
    /// installed() probed ONCE at startup. It shells out (`command -v`); doing
    /// that per runtime per frame was ~20 subprocess spawns/second — the
    /// "cpu heavy, memory leaky" picker (user report, 2026-08-21).
    installed: Vec<bool>,
    runtime_cursor: usize,
    dest_cursor: usize,
    mode_cursor: usize,
    open_cursor: usize,
    dest_top: usize,
    focus: Focus,
    plan: Option<String>,
    status: Option<String>,
    error: Option<String>,
    harness_bin: Option<String>,
    clicks: Vec<(Rect, Click)>,
}

impl App {
    fn new() -> Self {
        let source = load_source();
        let dests = destinations(&source, &load_fleet());
        let error = source.error.clone();
        // Start on a runtime that is not the one already running: the common
        // case for this overlay is "somewhere else", and pre-selecting the
        // current runtime would make ↵ a no-op relaunch.
        let runtime_cursor = RUNTIMES
            .iter()
            .position(|runtime| *runtime != source.runtime)
            .unwrap_or(0);
        let installed_cache: Vec<bool> = RUNTIMES.iter().map(|r| installed(r)).collect();
        let mut app = Self {
            source,
            dests,
            installed: installed_cache,
            runtime_cursor,
            dest_cursor: 0,
            mode_cursor: 0,
            open_cursor: if fork_mode() { 1 } else { 0 },
            dest_top: 0,
            focus: Focus::Runtime,
            plan: None,
            status: None,
            error,
            harness_bin: harness_bin(),
            clicks: Vec::new(),
        };
        app.refresh_plan();
        app
    }

    fn fork(&self) -> bool {
        self.open_cursor == 1
    }

    fn runtime(&self) -> &str {
        RUNTIMES[self.runtime_cursor.min(RUNTIMES.len() - 1)]
    }

    fn dest(&self) -> Option<&Dest> {
        self.dests.get(self.dest_cursor)
    }

    /// The argv shared by the preview and the launch, so the two can never
    /// describe different swaps.
    fn swap_args(&self) -> Vec<String> {
        let mut args = vec![
            "swap-here".to_string(),
            "--runtime".into(),
            self.runtime().to_string(),
            "--pane".into(),
            self.source.pane_id.clone(),
            "--mode".into(),
            MODES[self.mode_cursor.min(MODES.len() - 1)].0.to_string(),
        ];
        if self.fork() {
            args.push("--fork".into());
        }
        match self.dest().map(|dest| (dest.kind, dest.name.clone())) {
            Some((DestKind::Named, name)) => {
                args.push("--harness".into());
                args.push(name);
            }
            Some((DestKind::Ambient, _)) => {
                args.push("--harness".into());
                args.push(String::new());
            }
            _ => {}
        }
        args
    }

    fn ready(&self) -> bool {
        self.source.error.is_none()
            && self.harness_bin.is_some()
            && self.installed[self.runtime_cursor.min(RUNTIMES.len() - 1)]
            && !self.source.pane_id.is_empty()
    }

    /// `swap-here --dry-run` on the current selection. It writes nothing (the
    /// Go side probes instead of importing when --dry-run is set), so it is
    /// safe to run on every cursor move.
    fn refresh_plan(&mut self) {
        self.plan = None;
        let Some(bin) = self.harness_bin.clone() else {
            self.error = Some("harness binary not found — install the harness-tidus plugin".into());
            return;
        };
        if self.source.pane_id.is_empty() {
            return;
        }
        let mut args = self.swap_args();
        args.push("--dry-run".into());
        let Ok(out) = Command::new(&bin).args(&args).output() else {
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let Some(line) = text.lines().rfind(|line| line.trim_start().starts_with('{')) else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if !err.is_empty() {
                self.plan = Some(err);
            }
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        let plan = value
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let to = value.get("to").cloned().unwrap_or_default();
        let where_ = to
            .get("harness")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .unwrap_or("your own profile")
            .to_string();
        self.plan = Some(format!("{} in {}  ·  {}", self.runtime(), where_, plan));
    }

    /// Runs the swap synchronously. On success the source pane is already gone
    /// and the overlay should close; on failure the message stays on screen and
    /// the OLD PANE IS STILL THERE — swap-here never closes it until the
    /// replacement has proved it survived.
    fn reopen(&mut self) -> Result<(), String> {
        let bin = self
            .harness_bin
            .clone()
            .ok_or("harness binary not found".to_string())?;
        if !self.installed[self.runtime_cursor.min(RUNTIMES.len() - 1)] {
            return Err(format!("{} is not installed on this machine", self.runtime()));
        }
        let out = Command::new(&bin)
            .args(self.swap_args())
            .output()
            .map_err(|err| err.to_string())?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Err(if stderr.is_empty() { stdout } else { stderr })
    }

    fn scroll_dest_into_view(&mut self) {
        if self.dest_cursor < self.dest_top {
            self.dest_top = self.dest_cursor;
        } else if self.dest_cursor >= self.dest_top + HARNESS_WINDOW {
            self.dest_top = self.dest_cursor + 1 - HARNESS_WINDOW;
        }
    }
}

// ── styles (matching the agent launcher's modal language) ──────────────────

const CLOSE: &str = "[x]";

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn accent() -> Style {
    Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)
}
fn chip_primary() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Blue)
        .add_modifier(Modifier::BOLD)
}
fn chip() -> Style {
    Style::default().fg(Color::Gray).bg(Color::DarkGray)
}
fn selected_row() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

fn tail_fit(value: &str, max: usize) -> String {
    let count = value.chars().count();
    if count <= max || max == 0 {
        value.to_string()
    } else {
        format!("…{}", value.chars().skip(count - max + 1).collect::<String>())
    }
}

/// What picking this runtime will do to the conversation, in one phrase. The
/// authority is swaphere.go's emitter table; this is its plain-language echo,
/// and the `plan:` line below it is the real answer for the CURRENT selection.
fn runtime_note(runtime: &str, source: &str) -> String {
    if runtime == source {
        return "same runtime — resumed or raw-copied byte-for-byte (lossless)".into();
    }
    match runtime {
        "codex" => "carried as a codex rollout + thread row (a real resume)".into(),
        "claude" => "carried as a claude transcript in the target config".into(),
        "jcode" => "carried as a jcode imported_* session".into(),
        "pi" => "carried as a native pi session (--session <file>)".into(),
        _ => "handed over as a handoff document".into(),
    }
}

// ── draw ───────────────────────────────────────────────────────────────────

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    app.clicks.clear();
    // The overlay's PTY can report a ZERO-SIZED (or sliver) area on its first
    // frame(s) — rendering into it panics ratatui ("index outside of buffer"),
    // the process dies, and the user sees the picker flash and vanish
    // (reported 2026-08-21, after an hr restart). Skip painting until the
    // host hands over a real surface; the 200ms poll redraws immediately.
    if area.width < 10 || area.height < 5 {
        return;
    }
    let x = area.x + 2;
    let width = area.width.saturating_sub(4);
    let label_width = 10usize;
    let mut y = area.y + 1;
    // Hard bottom edge. The 0×0 guard above fixed the FIRST flash; this fixes
    // the second one (2026-08-21, same evening): the merged picker draws ~28
    // rows, the real overlay can be shorter, and one row rendered past the
    // bottom is the same ratatui buffer panic → process exit → flicker. Every
    // put() clips, and the harness window shrinks to the space that exists.
    let bottom = area.y + area.height;

    let put_at = |frame: &mut ratatui::Frame, at_x: u16, y: u16, line: Line| {
        if y >= bottom {
            return;
        }
        let room = width.saturating_sub(at_x.saturating_sub(x));
        frame.render_widget(Paragraph::new(line), Rect::new(at_x, y, room, 1));
    };
    let put = |frame: &mut ratatui::Frame, y: u16, line: Line| {
        if y >= bottom {
            return;
        }
        frame.render_widget(Paragraph::new(line), Rect::new(x, y, width, 1));
    };

    // Close control at the START of the title row: the host hands this program
    // a PTY wider than the box it paints, so anything anchored to the right
    // edge can be clipped away. The top-LEFT corner is the one both agree on.
    let close_rect = Rect::new(x, y, CLOSE.len() as u16, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(CLOSE, accent()))),
        close_rect,
    );
    app.clicks.push((close_rect, Click::Cancel));
    put_at(
        frame,
        x + CLOSE.len() as u16 + 1,
        y,
        Line::from(Span::styled(
            "resume this session anywhere",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    );
    y += 2;

    let from = if app.source.harness.is_empty() {
        app.source.runtime.clone()
    } else {
        format!("{} · harness {}", app.source.runtime, app.source.harness)
    };
    for (label, value) in [
        ("from", from),
        ("session", app.source.session.clone()),
        ("folder", app.source.cwd.clone()),
        (
            "pane",
            format!("{}  ·  tab {}", app.source.pane_id, app.source.tab_id),
        ),
    ] {
        put(
            frame,
            y,
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{label:label_width$}"), dim()),
                Span::styled(
                    tail_fit(&value, (width as usize).saturating_sub(label_width + 2)),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
        );
        y += 1;
    }
    y += 1;

    put(frame, y, Line::from(Span::styled("  runtime", dim())));
    y += 1;
    for (index, runtime) in RUNTIMES.iter().enumerate() {
        let is_installed = app.installed[index];
        let focused = app.focus == Focus::Runtime && index == app.runtime_cursor;
        let style = if focused {
            selected_row()
        } else if is_installed {
            Style::default()
        } else {
            dim()
        };
        let note = if is_installed {
            runtime_note(runtime, &app.source.runtime)
        } else {
            "not installed on this machine".into()
        };
        // '*' marks the CHOSEN row even when focus is elsewhere — bold alone
        // was invisible at a glance (user feedback, 2026-08-21)
        let mark = if index == app.runtime_cursor { "*" } else { " " };
        let text = format!(" {mark}{runtime:<9} {note}");
        put(
            frame,
            y,
            Line::from(Span::styled(format!("{text:<w$}", w = width as usize), style)),
        );
        app.clicks
            .push((Rect::new(x, y, width, 1), Click::Runtime(index)));
        y += 1;
    }
    y += 1;

    put(frame, y, Line::from(Span::styled("  open as", dim())));
    y += 1;
    for (index, (name, note)) in OPENS.iter().enumerate() {
        let focused = app.focus == Focus::Open && index == app.open_cursor;
        let style = if focused { selected_row() } else { Style::default() };
        let mark = if index == app.open_cursor { "*" } else { " " };
        let text = format!(" {mark}{name:<9} {note}");
        put(
            frame,
            y,
            Line::from(Span::styled(format!("{text:<w$}", w = width as usize), style)),
        );
        app.clicks.push((Rect::new(x, y, width, 1), Click::Open(index)));
        y += 1;
    }
    y += 1;

    put(frame, y, Line::from(Span::styled("  mode", dim())));
    y += 1;
    for (index, (name, note)) in MODES.iter().enumerate() {
        let focused = app.focus == Focus::Mode && index == app.mode_cursor;
        let style = if focused { selected_row() } else { Style::default() };
        let mark = if index == app.mode_cursor { "*" } else { " " };
        let text = format!(" {mark}{name:<9} {note}");
        put(
            frame,
            y,
            Line::from(Span::styled(format!("{text:<w$}", w = width as usize), style)),
        );
        app.clicks.push((Rect::new(x, y, width, 1), Click::Mode(index)));
        y += 1;
    }
    y += 1;

    let avail = usize::from(bottom.saturating_sub(y)).saturating_sub(7);
    let window = HARNESS_WINDOW.min(avail.max(2));
    let more = app.dests.len().saturating_sub(app.dest_top + window);
    put(
        frame,
        y,
        Line::from(vec![
            Span::styled("  harness", dim()),
            Span::styled(
                if more > 0 {
                    format!("   (+{more} more below)")
                } else {
                    String::new()
                },
                dim(),
            ),
        ]),
    );
    y += 1;
    let end = (app.dest_top + window).min(app.dests.len());
    for index in app.dest_top..end {
        let dest = &app.dests[index];
        let focused = app.focus == Focus::Dest && index == app.dest_cursor;
        let style = if focused {
            selected_row()
        } else if dest.kind == DestKind::Ambient {
            dim()
        } else {
            Style::default()
        };
        let mark = if index == app.dest_cursor { "*" } else { " " };
        let text = format!(" {mark}{:<22} {}", dest.name, dest.note);
        put(
            frame,
            y,
            Line::from(Span::styled(format!("{text:<w$}", w = width as usize), style)),
        );
        app.clicks
            .push((Rect::new(x, y, width, 1), Click::Dest(index)));
        y += 1;
    }
    y += 1;

    if let Some(status) = &app.status {
        put(frame, y, Line::from(Span::styled(status.clone(), accent())));
    } else if let Some(error) = &app.error {
        put(
            frame,
            y,
            Line::from(Span::styled(
                tail_fit(error, width as usize),
                Style::default().fg(Color::Red),
            )),
        );
    } else if let Some(plan) = &app.plan {
        put(
            frame,
            y,
            Line::from(vec![
                Span::styled("  plan  ", dim()),
                Span::styled(
                    tail_fit(plan, (width as usize).saturating_sub(8)),
                    Style::default().fg(Color::Green),
                ),
            ]),
        );
    }
    y += 2;

    let reopen_chip = if app.fork() { " ↵ fork " } else { " ↵ reopen " };
    let cancel_chip = " esc cancel ";
    let reopen_style = if !app.ready() {
        chip().add_modifier(Modifier::DIM)
    } else if app.focus == Focus::Reopen {
        chip_primary()
    } else {
        chip_primary().add_modifier(Modifier::DIM)
    };
    put(
        frame,
        y,
        Line::from(vec![
            Span::raw("  "),
            Span::styled(reopen_chip, reopen_style),
            Span::raw("   "),
            Span::styled(
                cancel_chip,
                if app.focus == Focus::Cancel {
                    chip_primary()
                } else {
                    chip()
                },
            ),
        ]),
    );
    app.clicks.push((
        Rect::new(x + 2, y, reopen_chip.chars().count() as u16, 1),
        Click::Reopen,
    ));
    app.clicks.push((
        Rect::new(
            x + 2 + reopen_chip.chars().count() as u16 + 3,
            y,
            cancel_chip.chars().count() as u16,
            1,
        ),
        Click::Cancel,
    ));

    let hint_y = area.y + area.height.saturating_sub(2);
    if hint_y > y {
        // This copy of the close control is the one that can be trusted: the
        // footer's LEFT edge is never in doubt, the PTY's right edge is.
        let close_rect = Rect::new(x, hint_y, CLOSE.len() as u16, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(CLOSE, accent()))),
            close_rect,
        );
        app.clicks.push((close_rect, Click::Cancel));
        put_at(
            frame,
            x + CLOSE.len() as u16,
            hint_y,
            Line::from(Span::styled(
                " close · ↑↓ move · tab switch list · ↵ reopen · esc cancels",
                dim(),
            )),
        );
    }
}

// ── input ──────────────────────────────────────────────────────────────────

fn on_key(app: &mut App, key: KeyEvent) -> Outcome {
    if key.kind == KeyEventKind::Release {
        return Outcome::Continue;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return Outcome::Cancelled;
    }
    let before = (app.runtime_cursor, app.dest_cursor, app.mode_cursor, app.open_cursor);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => return Outcome::Cancelled,
        // ↑↓ flow through ALL sections as one continuous list — reaching the
        // edge of a section steps into the neighbour. Tab still jumps whole
        // sections, clicks still land anywhere: both input styles are equal
        // citizens (user feedback, recording of 2026-08-21).
        KeyCode::Up | KeyCode::Char('k') => match app.focus {
            Focus::Open => {
                if app.open_cursor == 0 {
                    app.focus = Focus::Runtime;
                    app.runtime_cursor = RUNTIMES.len() - 1;
                } else {
                    app.open_cursor -= 1;
                }
            }
            Focus::Mode => {
                if app.mode_cursor == 0 {
                    app.focus = Focus::Open;
                    app.open_cursor = OPENS.len() - 1;
                } else {
                    app.mode_cursor -= 1;
                }
            }
            Focus::Dest => {
                if app.dest_cursor == 0 {
                    app.focus = Focus::Mode;
                    app.mode_cursor = MODES.len() - 1;
                } else {
                    app.dest_cursor -= 1;
                    app.scroll_dest_into_view();
                }
            }
            _ => {
                app.focus = Focus::Runtime;
                app.runtime_cursor = app.runtime_cursor.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.focus {
            Focus::Runtime => {
                if app.runtime_cursor + 1 < RUNTIMES.len() {
                    app.runtime_cursor += 1;
                } else {
                    app.focus = Focus::Open;
                    app.open_cursor = 0;
                }
            }
            Focus::Open => {
                if app.open_cursor + 1 < OPENS.len() {
                    app.open_cursor += 1;
                } else {
                    app.focus = Focus::Mode;
                    app.mode_cursor = 0;
                }
            }
            Focus::Mode => {
                if app.mode_cursor + 1 < MODES.len() {
                    app.mode_cursor += 1;
                } else {
                    app.focus = Focus::Dest;
                    app.dest_cursor = 0;
                    app.scroll_dest_into_view();
                }
            }
            Focus::Dest => {
                if app.dest_cursor + 1 < app.dests.len() {
                    app.dest_cursor += 1;
                    app.scroll_dest_into_view();
                }
            }
            _ => {
                app.focus = Focus::Runtime;
            }
        },
        KeyCode::Tab | KeyCode::Right => {
            app.focus = match app.focus {
                Focus::Runtime => Focus::Open,
                Focus::Open => Focus::Mode,
                Focus::Mode => Focus::Dest,
                Focus::Dest => Focus::Reopen,
                Focus::Reopen => Focus::Cancel,
                Focus::Cancel => Focus::Runtime,
            }
        }
        KeyCode::BackTab | KeyCode::Left => {
            app.focus = match app.focus {
                Focus::Runtime => Focus::Cancel,
                Focus::Open => Focus::Runtime,
                Focus::Mode => Focus::Open,
                Focus::Dest => Focus::Mode,
                Focus::Reopen => Focus::Dest,
                Focus::Cancel => Focus::Reopen,
            }
        }
        KeyCode::Enter => {
            if app.focus == Focus::Cancel {
                return Outcome::Cancelled;
            }
            return Outcome::Reopen;
        }
        _ => {}
    }
    if before != (app.runtime_cursor, app.dest_cursor, app.mode_cursor, app.open_cursor) {
        app.error = None;
        app.refresh_plan();
    }
    Outcome::Continue
}

fn on_mouse(app: &mut App, mouse: MouseEvent) -> Outcome {
    let before = (app.runtime_cursor, app.dest_cursor, app.mode_cursor, app.open_cursor);
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let down = mouse.kind == MouseEventKind::ScrollDown;
            match app.focus {
                Focus::Dest => {
                    if down {
                        if app.dest_cursor + 1 < app.dests.len() {
                            app.dest_cursor += 1;
                        }
                    } else {
                        app.dest_cursor = app.dest_cursor.saturating_sub(1);
                    }
                    app.scroll_dest_into_view();
                }
                _ => {
                    if down {
                        if app.runtime_cursor + 1 < RUNTIMES.len() {
                            app.runtime_cursor += 1;
                        }
                    } else {
                        app.runtime_cursor = app.runtime_cursor.saturating_sub(1);
                    }
                }
            }
            if before != (app.runtime_cursor, app.dest_cursor, app.mode_cursor, app.open_cursor) {
                app.refresh_plan();
            }
            return Outcome::Continue;
        }
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return Outcome::Continue,
    }
    let clicked = app
        .clicks
        .iter()
        .find(|(rect, _)| {
            mouse.column >= rect.x
                && mouse.column < rect.x + rect.width
                && mouse.row >= rect.y
                && mouse.row < rect.y + rect.height
        })
        .map(|(_, click)| *click);
    let outcome = match clicked {
        Some(Click::Runtime(index)) => {
            // Click SELECTS, never commits. The launcher's click-again-to-go
            // idiom is wrong here: the cursor starts pre-positioned, so a
            // user's very first click can land on the selected row and fire a
            // pane-replacing swap with the inferred harness (reported
            // 2026-08-21). Committing is the ↵ chip / Enter, and nothing else.
            app.focus = Focus::Runtime;
            app.runtime_cursor = index;
            Outcome::Continue
        }
        Some(Click::Open(index)) => {
            app.focus = Focus::Open;
            app.open_cursor = index;
            Outcome::Continue
        }
        Some(Click::Mode(index)) => {
            app.focus = Focus::Mode;
            app.mode_cursor = index;
            Outcome::Continue
        }
        Some(Click::Dest(index)) => {
            app.focus = Focus::Dest;
            app.dest_cursor = index;
            Outcome::Continue
        }
        Some(Click::Reopen) => Outcome::Reopen,
        Some(Click::Cancel) => Outcome::Cancelled,
        None => Outcome::Continue,
    };
    if before != (app.runtime_cursor, app.dest_cursor, app.mode_cursor, app.open_cursor) {
        app.error = None;
        app.refresh_plan();
    }
    outcome
}

fn fork_mode() -> bool {
    std::env::args().any(|a| a == "--fork")
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new();

    // Event-driven rendering: draw once, then only after an input event (or
    // a resize, which crossterm delivers as an event too). The old loop
    // redrew unconditionally every 200ms forever — G3 of the gate goal.
    let mut dirty = true;
    let result = loop {
        if dirty {
            terminal.draw(|frame| draw(frame, &mut app))?;
            dirty = false;
        }
        if !crossterm::event::poll(Duration::from_millis(1000))? {
            continue;
        }
        dirty = true;
        let outcome = match crossterm::event::read()? {
            Event::Key(key) => on_key(&mut app, key),
            Event::Mouse(mouse) => on_mouse(&mut app, mouse),
            _ => Outcome::Continue,
        };
        match outcome {
            Outcome::Continue => {}
            Outcome::Cancelled => break Ok(()),
            Outcome::Reopen => {
                if !app.ready() {
                    app.error = Some(app.source.error.clone().unwrap_or_else(|| {
                        format!("{} is not installed on this machine", app.runtime())
                    }));
                    continue;
                }
                let runtime = app.runtime().to_string();
                let where_ = app
                    .dest()
                    .map(|dest| dest.name.clone())
                    .unwrap_or_default();
                // The swap verifies the new pane for up to ~4s before closing
                // the old one, so say what is happening rather than freezing.
                let verb = if app.fork() { "forking" } else { "reopening" };
                app.status = Some(format!("  {verb} in {runtime} · {where_} …"));
                app.error = None;
                terminal.draw(|frame| draw(frame, &mut app))?;
                match app.reopen() {
                    Ok(()) => break Ok(()),
                    Err(err) => {
                        app.status = None;
                        app.error = Some(err);
                    }
                }
            }
        }
    };

    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    result
}
