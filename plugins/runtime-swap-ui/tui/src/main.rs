//! runtime-swap overlay TUI. Runs inside a herdr `placement = "overlay"` plugin
//! pane, the same terminal-in-a-floating-pane mechanism the agent launcher's
//! popup uses; the host paints the chrome and this process owns the keyboard and
//! mouse natively via crossterm.
//!
//! It shows the focused pane's live agent session and offers the other runtimes
//! as reopen targets. Selecting one shells out to `scripts/swap-runtime.ts`,
//! which performs the import and replaces the pane in place.

use std::io;
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

// ── source session ─────────────────────────────────────────────────────────

#[derive(Default)]
struct Source {
    pane_id: String,
    tab_id: String,
    cwd: String,
    runtime: String,
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

/// The overlay's plugin context is captured before the overlay steals focus, so
/// `focused_pane_id` is the pane the user was looking at. `herdr pane list` is
/// the fallback for direct/manual launches.
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
        .find(|pane| {
            pane.get("focused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .or_else(|| panes.first())?
        .get("pane_id")?
        .as_str()
        .map(str::to_string)
}

fn normalize_runtime(name: &str) -> Option<String> {
    let value = name.trim().to_lowercase();
    let canonical = if value == "cx" {
        "claude".to_string()
    } else {
        value
    };
    RUNTIMES.contains(&canonical.as_str()).then_some(canonical)
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
    let agent = session
        .get("agent")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| text("agent"));
    match normalize_runtime(&agent) {
        Some(runtime) => source.runtime = runtime,
        None => {
            source.error = Some(format!("pane {pane_id} is not running a supported agent"));
            return source;
        }
    }
    source.session = session
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if source.session.is_empty() {
        // Not an error: herdr only reports a session id when the runtime's
        // integration hook does, and the engine recovers the session from the
        // newest transcript for this cwd. Say so instead of blocking the pick.
        source.session = "(resolved from the transcript at launch)".to_string();
    }
    source
}

/// Mirrors the engine's `which()`. A plugin pane inherits the herdr server's
/// environment, where per-user bin dirs an interactive shell rc adds (notably
/// `~/.bun/bin`) are absent — probing PATH alone reports installed runtimes as
/// missing, so the well-known install dirs are checked too.
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
    let home = std::env::var("HOME").unwrap_or_default();
    [".local/bin", ".bun/bin", ".npm-global/bin"]
        .iter()
        .map(|dir| format!("{home}/{dir}/{bin}"))
        .chain(
            ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"]
                .iter()
                .map(|dir| format!("{dir}/{bin}")),
        )
        .any(|path| std::path::Path::new(&path).exists())
}

// ── target rows ────────────────────────────────────────────────────────────

struct Target {
    runtime: String,
    installed: bool,
    how: String,
}

fn targets(source: &Source) -> Vec<Target> {
    RUNTIMES
        .iter()
        .filter(|runtime| **runtime != source.runtime)
        .map(|runtime| {
            let is_installed = installed(runtime);
            let how = if !is_installed {
                "not installed".to_string()
            } else if *runtime == "jcode" {
                format!("jcode imports the {} session natively", source.runtime)
            } else if *runtime == "codex" && source.runtime == "claude" {
                "codex imports it natively — a real resume".to_string()
            } else {
                "flattened handoff, jcode-style import".to_string()
            };
            Target {
                runtime: runtime.to_string(),
                installed: is_installed,
                how,
            }
        })
        .collect()
}

// ── app ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Click {
    Row(usize),
    Reopen,
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Reopen,
    Cancel,
}

enum Outcome {
    Continue,
    Cancelled,
    Reopen,
}

struct App {
    source: Source,
    targets: Vec<Target>,
    cursor: usize,
    focus: Focus,
    status: Option<String>,
    error: Option<String>,
    clicks: Vec<(Rect, Click)>,
}

impl App {
    fn new() -> Self {
        let source = load_source();
        let targets = targets(&source);
        let error = source.error.clone();
        Self {
            source,
            targets,
            cursor: 0,
            focus: Focus::List,
            status: None,
            error,
            clicks: Vec::new(),
        }
    }

    fn selected(&self) -> Option<&Target> {
        self.targets.get(self.cursor)
    }

    fn ready(&self) -> bool {
        self.source.error.is_none() && self.selected().map(|t| t.installed).unwrap_or(false)
    }

    /// Runs the engine synchronously. On success the source pane is already gone
    /// and the overlay should close; on failure the message stays on screen.
    fn reopen(&mut self) -> Result<(), String> {
        let Some(target) = self.selected() else {
            return Err("nothing selected".into());
        };
        if !target.installed {
            return Err(format!(
                "{} is not installed on this machine",
                target.runtime
            ));
        }
        let root = std::env::var("HERDR_PLUGIN_ROOT")
            .map_err(|_| "HERDR_PLUGIN_ROOT unset".to_string())?;
        let script = format!("{root}/scripts/swap-runtime.ts");
        let out = Command::new("sh")
            .arg("-lc")
            .arg(format!(
                "exec \"$BUN\" {} --pane {} --to {}",
                shell_quote(&script),
                shell_quote(&self.source.pane_id),
                shell_quote(&target.runtime)
            ))
            .env("BUN", bun_path())
            .output()
            .map_err(|err| err.to_string())?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

fn bun_path() -> String {
    if let Ok(bun) = std::env::var("BUN_PATH") {
        return bun;
    }
    let probe = Command::new("sh").arg("-lc").arg("command -v bun").output();
    if let Ok(out) = probe {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() {
            return path;
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.bun/bin/bun")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// ── styles (matching the agent launcher's modal language) ──────────────────

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn accent() -> Style {
    Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::BOLD)
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
        format!(
            "…{}",
            value.chars().skip(count - max + 1).collect::<String>()
        )
    }
}

// ── draw ───────────────────────────────────────────────────────────────────

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    app.clicks.clear();
    let x = area.x + 2;
    let width = area.width.saturating_sub(4);
    let label_width = 10usize;
    let mut y = area.y + 1;

    let put = |frame: &mut ratatui::Frame, y: u16, line: Line| {
        frame.render_widget(Paragraph::new(line), Rect::new(x, y, width, 1));
    };

    put(
        frame,
        y,
        Line::from(Span::styled(
            "reopen in another runtime",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    );
    // Mouse-clickable close control, top right (UX canon §3.1.7). herdr is a
    // mouse-first TUI, so an overlay whose only exit is Esc has no exit for the
    // way the app is actually driven. It registers a real click rect rather
    // than being painted decoration — the canon gates it under the REAL-CLICK
    // rule, so it has to *work*, not merely appear.
    const CLOSE: &str = "[x]";
    let close_x = x + width.saturating_sub(CLOSE.len() as u16);
    let close_rect = Rect::new(close_x, y, CLOSE.len() as u16, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(CLOSE, dim()))),
        close_rect,
    );
    app.clicks.push((close_rect, Click::Cancel));
    y += 2;

    for (label, value) in [
        ("from", app.source.runtime.clone()),
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

    put(frame, y, Line::from(Span::styled("  reopen as", dim())));
    y += 1;

    for (index, target) in app.targets.iter().enumerate() {
        let focused = app.focus == Focus::List && index == app.cursor;
        let style = if focused {
            selected_row()
        } else if target.installed {
            Style::default()
        } else {
            dim()
        };
        let text = format!("  {:<10} {}", target.runtime, target.how);
        put(
            frame,
            y,
            Line::from(Span::styled(
                format!("{text:<w$}", w = width as usize),
                style,
            )),
        );
        app.clicks
            .push((Rect::new(x, y, width, 1), Click::Row(index)));
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
    }
    y += 2;

    let reopen_chip = " ↵ reopen ";
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
        put(
            frame,
            hint_y,
            Line::from(Span::styled(
                "↑↓ move · ↵ reopen · esc / [x] close · click to pick",
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
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => return Outcome::Cancelled,
        KeyCode::Up | KeyCode::Char('k') => {
            app.focus = Focus::List;
            app.cursor = app.cursor.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.focus = Focus::List;
            if app.cursor + 1 < app.targets.len() {
                app.cursor += 1;
            }
        }
        KeyCode::Tab | KeyCode::Right => {
            app.focus = match app.focus {
                Focus::List => Focus::Reopen,
                Focus::Reopen => Focus::Cancel,
                Focus::Cancel => Focus::List,
            }
        }
        KeyCode::BackTab | KeyCode::Left => {
            app.focus = match app.focus {
                Focus::List => Focus::Cancel,
                Focus::Reopen => Focus::List,
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
    Outcome::Continue
}

fn on_mouse(app: &mut App, mouse: MouseEvent) -> Outcome {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.cursor = app.cursor.saturating_sub(1);
            return Outcome::Continue;
        }
        MouseEventKind::ScrollDown => {
            if app.cursor + 1 < app.targets.len() {
                app.cursor += 1;
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
    match clicked {
        Some(Click::Row(index)) => {
            app.focus = Focus::List;
            // A click on the already-selected row commits, mirroring the
            // launcher's click-to-open-then-click-to-pick feel.
            if app.cursor == index {
                return Outcome::Reopen;
            }
            app.cursor = index;
        }
        Some(Click::Reopen) => return Outcome::Reopen,
        Some(Click::Cancel) => return Outcome::Cancelled,
        None => {}
    }
    Outcome::Continue
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new();

    let result = loop {
        terminal.draw(|frame| draw(frame, &mut app))?;
        if !crossterm::event::poll(Duration::from_millis(200))? {
            continue;
        }
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
                    app.error = Some(
                        app.source
                            .error
                            .clone()
                            .unwrap_or_else(|| "that runtime is not installed".into()),
                    );
                    continue;
                }
                let target = app
                    .selected()
                    .map(|t| t.runtime.clone())
                    .unwrap_or_default();
                app.status = Some(format!("importing session into {target}…"));
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
