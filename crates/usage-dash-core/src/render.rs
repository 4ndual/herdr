//! Renders a `UsageSnapshot` into a bordered, width-adaptive panel.
//! Layout (uses the width to the right for bars + trailers):
//!
//!   ┌ USAGE · Codex ───────────────── prolite ┐
//!   │ model  gpt-5.4                           │
//!   │ ctx    ▕████░░░░░░░░▏  6%  16k/258k       │
//!   │ 5h     ▕░░░░░░░░░░░░▏  0%  in 4h · 11:57  │
//!   │ week   ▕██████████░░▏ 96%  in 2d · Sat    │
//!   │ today  ▕███░░░░░░░░░▏  3% of 12% wk       │
//!   └──────────────────────────────────────────┘
//!
//! Ported from usage-dash `src/render.rs`. The only structural change vs the
//! standalone binary: `draw` takes a [`UsageView`] (the interactive UI state the
//! host owns) instead of loose `expanded/collapsed/stats_shown/scroll/hover`
//! params, and it writes the clickable hit-rects back into the view so the host
//! can map clicks via [`crate::handle`].

use crate::model::{
    countdown, fmt_k, Cli, CliVersion, Freshness, ThreadState, UsageSnapshot, DAILY_BUDGET_PCT,
};
use crate::{UsageView, VersionToken};
use chrono::{Local, TimeZone};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui::Frame;

const DIM: Color = Color::DarkGray;
/// Fixed bar width for the left box (keeps the shrink-to-fit measurement stable).
const BAR_W: usize = 10;

fn cli_color(cli: Cli) -> Color {
    match cli {
        Cli::Claude => Color::Magenta,
        Cli::Codex => Color::Green,
        Cli::Pi => Color::Cyan,
    }
}

fn state_color(s: ThreadState) -> Color {
    match s {
        ThreadState::Working => Color::Green,
        ThreadState::Waiting => Color::Cyan,
        ThreadState::AwaitingReview => Color::Blue,
        ThreadState::Blocked => Color::Red,
        ThreadState::Degraded => Color::Yellow,
        ThreadState::Done => DIM,
        ThreadState::Unknown => DIM,
    }
}

/// The slim state footer line: a caret (▸ collapsed / ▾ expanded, only when
/// there's a full status to reveal), the underlined color-coded state word,
/// the brief note when there's nothing to expand, and the dim session `age`.
/// Returns the line plus the caret+word display width (the clickable hit width).
fn state_footer(snap: &UsageSnapshot, expanded: bool) -> (Line<'static>, u16) {
    let expandable = snap
        .state_full
        .as_ref()
        .map(|f| !f.trim().is_empty())
        .unwrap_or(false);
    let caret = if !expandable {
        Span::styled(" ", Style::default().fg(DIM))
    } else if expanded {
        Span::styled("▾", Style::default().fg(DIM))
    } else {
        Span::styled("▸", Style::default().fg(DIM))
    };
    let word = Span::styled(
        snap.state.label().to_string(),
        Style::default()
            .fg(state_color(snap.state))
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    );
    let hw = Line::from(vec![caret.clone(), word.clone()]).width() as u16;

    let mut spans = vec![caret, word];
    // When there's no expandable detail, show the brief note inline instead.
    if !expandable {
        if let Some(note) = &snap.state_note {
            spans.push(Span::styled(format!("  {note}"), Style::default().fg(DIM)));
        }
    }
    if let Some(s) = &snap.stats {
        spans.push(Span::styled(
            format!("  · age {}", fmt_dur(s.age_secs)),
            Style::default().fg(DIM),
        ));
    }
    // Staleness now dims the whole box; keep an explicit cue in the footer too.
    if snap.is_stale() {
        spans.push(Span::styled(
            format!("  · idle {}m ago", snap.source_age_secs / 60),
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        ));
    }
    (Line::from(spans), hw)
}

fn pct_color(p: f32) -> Color {
    if p >= 90.0 {
        Color::Red
    } else if p >= 70.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Absolute LOCAL time of an epoch: weekday for the weekly row, `HH:MM` for the
/// 5h row (std has no local tz).
///
/// Was `date -r <epoch>`, which is the BSD/macOS spelling — GNU `date` reads
/// `-r` as "reference FILE", so this returned an empty string on Linux and both
/// reset labels rendered blank. It also spawned a process per row per render
/// tick; chrono answers in-process. An epoch chrono cannot represent yields the
/// same empty string as before, so callers keep their existing fallback.
fn abs_time(epoch: i64, weekly: bool) -> String {
    let fmt = if weekly { "%a" } else { "%H:%M" };
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|dt| dt.format(fmt).to_string())
        .unwrap_or_default()
}

/// Bar spans of inner `width`, filled to `pct`%.
fn bar(pct: Option<f32>, width: usize) -> Vec<Span<'static>> {
    match pct {
        None => vec![
            Span::styled("▕", Style::default().fg(DIM)),
            Span::styled("░".repeat(width), Style::default().fg(DIM)),
            Span::styled("▏", Style::default().fg(DIM)),
        ],
        Some(p) => {
            let p = p.clamp(0.0, 100.0);
            let filled = ((p / 100.0) * width as f32).round() as usize;
            let filled = filled.min(width);
            vec![
                Span::styled("▕", Style::default().fg(DIM)),
                Span::styled("█".repeat(filled), Style::default().fg(pct_color(p))),
                Span::styled("░".repeat(width - filled), Style::default().fg(DIM)),
                Span::styled("▏", Style::default().fg(DIM)),
            ]
        }
    }
}

fn label(s: &str) -> Span<'static> {
    Span::styled(format!("{:<6}", s), Style::default().fg(Color::Gray))
}

fn dash_row(name: &str) -> Line<'static> {
    Line::from(vec![
        label(name),
        Span::styled("—", Style::default().fg(DIM)),
    ])
}

/// One window row: label, bar, pct, countdown · absolute, optional (shared).
fn window_row(name: &str, w: &crate::model::Window, bar_w: usize, now: i64) -> Line<'static> {
    if w.used_pct.is_none() {
        return dash_row(name);
    }
    let pct = w.used_pct.unwrap();
    let mut spans = vec![label(name)];
    spans.extend(bar(Some(pct), bar_w));
    spans.push(Span::styled(
        format!(" {:>3.0}%", pct),
        Style::default().fg(pct_color(pct)),
    ));
    if let Some(r) = w.resets_at {
        let weekly = name == "week";
        let abs = abs_time(r, weekly);
        let cd = countdown(now, r);
        let txt = if abs.is_empty() {
            format!(" {}", cd)
        } else {
            format!(" {} · {}", cd, abs)
        };
        let col = if cd == "overdue" { Color::Red } else { DIM };
        spans.push(Span::styled(txt, Style::default().fg(col)));
    }
    if w.shared {
        spans.push(Span::styled(
            "  (shared)",
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
}

/// A rounded, dim-bordered box with 1 column of horizontal padding (matches the
/// approved HTML prototype: rounded corners, title rendered *inside* the box).
fn rounded_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .padding(Padding::horizontal(1))
}

/// The box caption rendered ON the top border (like a ratatui block title):
/// the CLI name (bold, CLI color), `· model` (white), `· plan` (dim).
/// Replaces the old in-box `USAGE · Cli` + `model` rows.
/// Colour for a version token. Green = current, red = outdated, dim = unknown.
///
/// `Unknown` must be visually distinct from both: a version we could not check
/// is not a version that is up to date, and it is not one that needs updating.
fn freshness_color(f: Freshness) -> Color {
    match f {
        Freshness::Current => Color::Green,
        Freshness::Outdated => Color::Red,
        Freshness::Unknown => DIM,
    }
}

/// One version token's rendered text plus the span that draws it.
///
/// Text varies with interaction state:
///   * armed for confirmation → `2.1.219→2.1.220 click to update?`
///   * hovered                → `2.1.219→2.1.220`
///   * otherwise              → `2.1.219`
///
/// The latest version is revealed on hover rather than shown always, so the
/// common (up-to-date) case stays as narrow as it is today.
fn version_token_text(v: &CliVersion, hovered: bool, confirming: bool) -> Option<String> {
    let current = v.current.as_deref()?;
    let freshness = v.freshness();
    let latest = v.latest.as_deref();

    Some(match (confirming, hovered, freshness, latest) {
        (true, _, _, Some(l)) => format!("{current}→{l} click to update?"),
        (_, true, Freshness::Outdated, Some(l)) => format!("{current}→{l}"),
        // Hovering a current token confirms what it was compared against.
        (_, true, Freshness::Current, Some(l)) => format!("{current} (latest {l})"),
        (_, true, Freshness::Unknown, _) => format!("{current} (latest unknown)"),
        _ => current.to_string(),
    })
}

/// Caption spans plus, for each version token, its column offset and width
/// within the caption — the caller turns those into absolute [`Rect`]s once the
/// box position is known.
struct Caption {
    line: Line<'static>,
    /// `(offset_in_caption, width)` for the Claude token, when drawn.
    claude_span: Option<(u16, u16)>,
}

/// The box caption rendered ON the top border:
/// `Claude · 2.1.220 · Opus 5 1M · low · prolite`
///
/// The version token is a separate, individually coloured span so it can be
/// hit-tested; everything else is plain text.
fn caption_line(snap: &UsageSnapshot, view: &UsageView) -> Caption {
    let mut spans = vec![Span::styled(
        snap.cli.label().to_string(),
        Style::default()
            .fg(cli_color(snap.cli))
            .add_modifier(Modifier::BOLD),
    )];
    // Running width of the caption so far, used to locate the version tokens.
    let mut offset: u16 = spans[0].content.chars().count() as u16;
    let mut claude_span = None;

    let hovered = hovered_token(view);

    // Claude Code version, immediately after the CLI name.
    if let Some(text) = version_token_text(
        &snap.versions.claude,
        hovered == Some(VersionToken::Claude),
        view.version_confirm == Some(VersionToken::Claude),
    ) {
        spans.push(Span::styled(" · ".to_string(), Style::default().fg(DIM)));
        offset += 3;
        let width = text.chars().count() as u16;
        claude_span = Some((offset, width));
        spans.push(Span::styled(
            text,
            Style::default().fg(freshness_color(snap.versions.claude.freshness())),
        ));
        // No further offset bookkeeping: the Claude token is the only positioned
        // one, and the spans after it are not hit-tested.
    }

    if let Some(m) = &snap.model {
        spans.push(Span::styled(
            format!(" · {m}"),
            Style::default().fg(Color::White),
        ));
    }

    if let Some(plan) = &snap.plan {
        if !plan.is_empty() {
            spans.push(Span::styled(format!(" · {plan}"), Style::default().fg(DIM)));
        }
    }

    Caption {
        line: Line::from(spans),
        claude_span,
    }
}

/// Which version token the cursor is currently over, from the previous frame's
/// hit rects. Hover is resolved against the LAST frame because that is the
/// geometry the cursor was actually over.
fn hovered_token(view: &UsageView) -> Option<VersionToken> {
    let (col, row) = view.hover?;
    let inside = |r: Option<Rect>| {
        r.map(|r| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height)
            .unwrap_or(false)
    };
    if inside(view.hits.claude_version) {
        Some(VersionToken::Claude)
    } else {
        None
    }
}

/// The USAGE left-column cells: ctx, 5h, week, today. (model moved to the
/// caption; the stale note is dropped — staleness now dims the whole box.)
fn usage_cells(snap: &UsageSnapshot, now: i64) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(ctx_row(snap, BAR_W));
    lines.push(window_row("5h", &snap.five_hour, BAR_W, now));
    lines.push(window_row("week", &snap.weekly, BAR_W, now));
    if let Some(d) = snap.daily_used_pct {
        let toward = (d / DAILY_BUDGET_PCT * 100.0).clamp(0.0, 100.0);
        let mut spans = vec![label("today")];
        spans.extend(bar(Some(toward), BAR_W));
        spans.push(Span::styled(
            format!(" {:>4.1}%", d),
            Style::default().fg(pct_color(toward)),
        ));
        spans.push(Span::styled(
            format!(" of {:.0}% wk", DAILY_BUDGET_PCT),
            Style::default().fg(DIM),
        ));
        lines.push(Line::from(spans));
    }
    lines
}

/// Compact duration: "45s", "22m", "1h38m", "2d3h".
fn fmt_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    }
}

/// One compact `CODEn` count tag: dim 2-letter code + colored number + a single
/// trailing space.
fn count_tag(code: &str, n: u32, col: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled(code.to_string(), Style::default().fg(DIM)),
        Span::styled(n.to_string(), Style::default().fg(col)),
        Span::raw(" "),
    ]
}

/// Compact STATS (session-scoped), stacked UNDER the USAGE rows as two dim
/// lines: the AC/AR/AD/AU count row, then a single condensed metrics line
/// (out-tokens·cache% · turns·tool-calls · busy/idle). Empty when `snap.stats`
/// is None (Codex/pi) — never fabricated. The caller prepends the toggle caret.
fn stats_compact(snap: &UsageSnapshot) -> Vec<Line<'static>> {
    let st = match &snap.stats {
        Some(s) => s,
        None => return Vec::new(),
    };
    // count row: AC autocompacts · AR running · AD done · AU types
    let mut counts: Vec<Span> = Vec::new();
    counts.extend(count_tag("AC", st.autocompacts, Color::Yellow));
    counts.extend(count_tag("AR", st.agents_running, Color::Green));
    counts.extend(count_tag("AD", st.agents_done, Color::Cyan));
    counts.extend(count_tag("AU", st.agent_types, Color::Blue));

    // one condensed metrics line, all dim: 1.6M·97%c · 1419t·655tc · busy/idle
    let mut m = fmt_k(st.out_tokens);
    if let Some(c) = st.cache_pct {
        m.push_str(&format!("·{:.0}%c", c));
    }
    m.push_str(&format!(
        " · {}t·{}tc · {}/{}",
        st.assistant_turns.saturating_add(st.user_turns),
        st.tool_calls,
        fmt_dur(st.busy_secs),
        fmt_dur(st.idle_secs),
    ));

    vec![
        Line::from(counts),
        Line::from(Span::styled(m, Style::default().fg(DIM))),
    ]
}

/// One-line summary shown when the panel is collapsed (the `z` toggle): the
/// state word + the few numbers worth always seeing (ctx/5h/wk + AC/AR/AD).
fn collapsed_line(snap: &UsageSnapshot) -> Line<'static> {
    let mut spans = vec![Span::styled(
        snap.state.label().to_string(),
        Style::default()
            .fg(state_color(snap.state))
            .add_modifier(Modifier::BOLD),
    )];
    let mut dim = String::new();
    if let Some(p) = snap.ctx_used_pct {
        dim.push_str(&format!("  ctx {:.0}%", p));
    }
    if let Some(p) = snap.five_hour.used_pct {
        dim.push_str(&format!(" · 5h {:.0}%", p));
    }
    if let Some(p) = snap.weekly.used_pct {
        dim.push_str(&format!(" · wk {:.0}%", p));
    }
    if let Some(s) = &snap.stats {
        dim.push_str(&format!(
            " · AC{} AR{} AD{}",
            s.autocompacts, s.agents_running, s.agents_done
        ));
    }
    spans.push(Span::styled(dim, Style::default().fg(DIM)));
    Line::from(spans)
}

/// Greedy word-wrap to `width` columns (long words may exceed it; the caller
/// caps the box to the screen width so they truncate rather than overflow).
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur = word.to_string();
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// A small floating tooltip (yellow rounded box) anchored at `anchor`, wrapped
/// and clamped within `area`. Placed below the anchor row when there's room,
/// else above.
fn render_tooltip(f: &mut Frame, area: Rect, anchor: Rect, text: &str) {
    let maxw = (area.width.saturating_sub(2)).clamp(10, 56) as usize;
    let lines = word_wrap(text, maxw);
    let w = (lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16 + 2)
        .min(area.width.max(3));
    let h = (lines.len() as u16 + 2).min(area.height.max(3));
    let ty = if anchor.y + 1 + h <= area.y + area.height {
        anchor.y + 1
    } else if anchor.y >= area.y + h {
        anchor.y - h
    } else {
        area.y
    };
    let tx = anchor.x.min(area.x + area.width.saturating_sub(w));
    let tr = Rect {
        x: tx,
        y: ty,
        width: w,
        height: h,
    };
    f.render_widget(Clear, tr);
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));
    let body: Vec<Line> = lines
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::White))))
        .collect();
    f.render_widget(Paragraph::new(body).block(blk), tr);
}

/// Clickable hit rects produced by [`draw`] for the event loop to map mouse
/// clicks onto: the state footer (expand toggle) and the stats caret (show/hide
/// the STATS block). Either is `None` when that control isn't present this frame.
#[derive(Debug, Default, Clone, Copy)]
pub struct Hits {
    pub state: Option<Rect>,
    pub stats: Option<Rect>,
    /// The Claude Code version token on the caption border, when drawn.
    pub claude_version: Option<Rect>,
}

/// Renders the panel into `area` for the given snapshot and interactive `view`.
/// Returns the clickable [`Hits`] for this frame (the host stores them on the
/// view so [`crate::handle`] can resolve a click into an expand/stats toggle).
pub fn draw(f: &mut Frame, area: Rect, snap: Option<&UsageSnapshot>, view: &UsageView) -> Hits {
    let now = view.now;
    let expanded = view.expanded;
    let collapsed = view.collapsed;
    let stats_shown = view.stats_shown;
    let scroll = view.scroll;
    let hover = view.hover;

    // Reserve the rightmost screen column → never write the bottom-right cell
    // (avoids the terminal auto-scroll / ghost-frame desync).
    let area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };

    let snap = match snap {
        Some(s) => s,
        None => {
            let block = rounded_block();
            let inner = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "— no focused agent session —",
                    Style::default().fg(DIM),
                ))),
                inner,
            );
            return Hits::default();
        }
    };

    // Collapsed: whole panel folds to one summary line, freeing the rows below.
    if collapsed {
        let block = rounded_block();
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(collapsed_line(snap)).wrap(Wrap { trim: true }),
            inner,
        );
        return Hits::default();
    }

    let expandable = snap
        .state_full
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    // ── ONE consolidated box, stacked top-to-bottom ───────────────────────
    // CLI · model · plan ride the top border (caption). Body, in order:
    //   USAGE rows (ctx/5h/week/today) → compact STATS block (toggle caret,
    //   under the context) → a dim rule → the slim state footer. When the state
    //   is expanded the full status scrolls below the footer. The box shrinks to
    //   its content; the rest of the strip stays free for the AGENTS·TASKS pane.
    let caption = caption_line(snap, view);
    let usage = usage_cells(snap, now);
    let stats = stats_compact(snap);
    let has_stats = !stats.is_empty();
    let (state_line, hw) = state_footer(snap, expanded);

    // Assemble the body, tracking the row index of the two interactive lines.
    let mut body: Vec<Line> = usage.clone();

    // STATS block with a leading toggle caret (▾ shown / ▸ hidden), placed right
    // under the USAGE rows. When hidden it collapses to a single dim "stats" line.
    let mut stats_row: Option<usize> = None;
    let mut stats_hw: u16 = 0;
    if has_stats {
        stats_row = Some(body.len());
        if stats_shown {
            let mut first = vec![Span::styled("▾ ", Style::default().fg(DIM))];
            first.extend(stats[0].spans.clone());
            let line = Line::from(first);
            stats_hw = line.width() as u16;
            body.push(line);
            // remaining stats lines, indented two cols to align under the counts
            for extra in stats.iter().skip(1) {
                let mut s = vec![Span::raw("  ")];
                s.extend(extra.spans.clone());
                body.push(Line::from(s));
            }
        } else {
            let line = Line::from(vec![
                Span::styled("▸ ", Style::default().fg(DIM)),
                Span::styled("stats", Style::default().fg(DIM)),
            ]);
            stats_hw = line.width() as u16;
            body.push(line);
        }
    }

    // A dim rule then the state footer.
    let state_row = body.len() + 1; // +1 for the rule we insert just below

    // Box width = widest of caption / body / state, + borders(2) + padding(2).
    let content_w = body
        .iter()
        .chain(std::iter::once(&state_line))
        .chain(std::iter::once(&caption.line))
        .map(|l| l.width())
        .max()
        .unwrap_or(0) as u16;
    let box_w = (content_w + 4).min(area.width);
    // Shrink to content height (body + rule + state + 2 borders), leaving the
    // strip below free. When expanded, grow to fit the scrolling full status.
    let base_h = body.len() as u16 + 4;
    let box_h = if expanded {
        area.height
    } else {
        base_h.min(area.height)
    };
    let rect = Rect {
        width: box_w,
        height: box_h,
        ..area
    };

    // Caption tokens ride the top border, which ratatui's left-aligned title
    // starts one column in from the box edge. Convert each token's caption-local
    // offset into an absolute rect now that the box position is settled, and
    // drop any token the box is too narrow to actually show — a hit rect for
    // text that was clipped away would be an invisible click target.
    let title_x = rect.x + 1;
    let box_right = rect.x + rect.width;
    let token_rect = |span: Option<(u16, u16)>| -> Option<Rect> {
        let (offset, width) = span?;
        let x = title_x + offset;
        if width == 0 || x >= box_right {
            return None;
        }
        Some(Rect {
            x,
            y: rect.y,
            width: width.min(box_right.saturating_sub(x)),
            height: 1,
        })
    };
    let claude_version = token_rect(caption.claude_span);

    let block = rounded_block().title(caption.line);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Render body + rule + state footer, no wrap (so interactive rows sit at
    // known, clickable positions).
    let mut lines = body;
    lines.push(Line::from(Span::styled(
        "─".repeat(inner.width as usize),
        Style::default().fg(DIM),
    )));
    lines.push(state_line);
    let mut p = Paragraph::new(lines);
    if snap.is_stale() {
        p = p.style(Style::default().add_modifier(Modifier::DIM));
    }
    f.render_widget(p, inner);

    // Expanded full status: scrollable region below the footer.
    let state_y = inner.y + state_row as u16;
    if expanded {
        if let Some(full) = &snap.state_full {
            let fy = state_y + 1;
            if fy < inner.y + inner.height {
                let frect = Rect {
                    x: inner.x,
                    y: fy,
                    width: inner.width,
                    height: inner.y + inner.height - fy,
                };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        full.clone(),
                        Style::default().fg(Color::Gray),
                    )))
                    .wrap(Wrap { trim: true })
                    .scroll((scroll, 0)),
                    frect,
                );
            }
        }
    }

    // Hover tooltips: build a table of (region, description) over every metric,
    // then render the legend for whichever region the cursor is on.
    if let Some((hx, hy)) = hover {
        let mut regions: Vec<(Rect, String)> = Vec::new();
        let full_w = inner.width;

        // USAGE rows (ctx / 5h / week / today), one description each.
        let mut udesc: Vec<String> = Vec::new();
        udesc.push(match (snap.ctx_used_tokens, snap.ctx_window) {
            (Some(u), Some(w)) => format!(
                "ctx — context window used: {} of {} tokens{}",
                fmt_k(u),
                fmt_k(w),
                if snap.ctx_approx {
                    " (approx, estimated from bytes)"
                } else {
                    ""
                }
            ),
            _ => "ctx — context window used".into(),
        });
        udesc.push("5h — rolling 5-hour rate-limit window used; the time is when it resets".into());
        udesc.push("week — weekly rate-limit window used; the time is when it resets".into());
        if snap.daily_used_pct.is_some() {
            udesc.push(format!(
                "today — share of the weekly cap consumed today (delta since midnight); target ≤ {:.0}%",
                DAILY_BUDGET_PCT
            ));
        }
        for (i, d) in udesc.into_iter().enumerate() {
            regions.push((
                Rect {
                    x: inner.x,
                    y: inner.y + i as u16,
                    width: full_w,
                    height: 1,
                },
                d,
            ));
        }

        // STATS block.
        if has_stats {
            let counts_y = inner.y + usage.len() as u16;
            if stats_shown {
                if let Some(st) = snap.stats.as_ref() {
                    // per-tag count regions
                    let tags = [
                        (
                            "AC",
                            st.autocompacts,
                            "autocompacts (context compactions so far)",
                        ),
                        (
                            "AR",
                            st.agents_running,
                            "agents running now (Agent-tool subagents launched − done)",
                        ),
                        (
                            "AD",
                            st.agents_done,
                            "agents done (completed Agent-tool subagents)",
                        ),
                        (
                            "AU",
                            st.agent_types,
                            "agent types (distinct subagent kinds used)",
                        ),
                    ];
                    let mut x = inner.x + 2; // after the "▾ " caret
                    for (code, n, desc) in tags {
                        let w = 2 + n.to_string().len() as u16;
                        regions.push((
                            Rect {
                                x,
                                y: counts_y,
                                width: w,
                                height: 1,
                            },
                            format!("{code} — {desc}"),
                        ));
                        x += w + 1;
                    }
                    // metrics line, one combined description
                    let metrics = format!(
                        "session totals — {} output tokens · {} cache-hit rate · {} turns · {} tool calls · agent busy {} / idle {}",
                        fmt_k(st.out_tokens),
                        st.cache_pct.map(|c| format!("{:.0}%", c)).unwrap_or_else(|| "—".into()),
                        st.assistant_turns.saturating_add(st.user_turns),
                        st.tool_calls,
                        fmt_dur(st.busy_secs),
                        fmt_dur(st.idle_secs),
                    );
                    regions.push((
                        Rect {
                            x: inner.x,
                            y: counts_y + 1,
                            width: full_w,
                            height: 1,
                        },
                        metrics,
                    ));
                }
            } else {
                regions.push((
                    Rect {
                        x: inner.x,
                        y: counts_y,
                        width: full_w,
                        height: 1,
                    },
                    "stats — press s (or click) to show session stats".into(),
                ));
            }
        }

        // State footer: the word reveals the full status; the rest explains age.
        if expandable || snap.state_note.is_some() {
            let word_text = snap
                .state_full
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| snap.state_note.clone())
                .unwrap_or_else(|| snap.state.label().to_string());
            regions.push((
                Rect {
                    x: inner.x,
                    y: state_y,
                    width: hw,
                    height: 1,
                },
                format!("{} — {}", snap.state.label(), word_text),
            ));
        }
        if snap.stats.is_some() {
            let mut agedesc = "age — session lifetime (first activity → last activity)".to_string();
            if snap.is_stale() {
                agedesc.push_str("; idle — minutes since the source last updated (stale)");
            }
            let ax = inner.x + hw;
            let full_w = inner.width;
            if full_w > hw {
                regions.push((
                    Rect {
                        x: ax,
                        y: state_y,
                        width: full_w - hw,
                        height: 1,
                    },
                    agedesc,
                ));
            }
        }

        if let Some((r, text)) = regions
            .into_iter()
            .find(|(r, _)| hx >= r.x && hx < r.x + r.width && hy == r.y)
        {
            render_tooltip(f, area, r, &text);
        }
    }

    Hits {
        state: if expandable {
            Some(Rect {
                x: inner.x,
                y: state_y,
                width: hw.min(inner.width),
                height: 1,
            })
        } else {
            None
        },
        stats: stats_row.map(|r| Rect {
            x: inner.x,
            y: inner.y + r as u16,
            width: stats_hw.min(inner.width),
            height: 1,
        }),
        claude_version,
    }
}

fn ctx_row(snap: &UsageSnapshot, bar_w: usize) -> Line<'static> {
    if snap.ctx_used_pct.is_none() {
        return dash_row("ctx");
    }
    let pct = snap.ctx_used_pct.unwrap();
    let mut spans = vec![label("ctx")];
    if snap.ctx_approx {
        // overwrite the trailing space of the label with a ~ marker
        spans = vec![
            Span::styled(format!("{:<5}", "ctx"), Style::default().fg(Color::Gray)),
            Span::styled("~", Style::default().fg(Color::Yellow)),
        ];
    }
    spans.extend(bar(Some(pct), bar_w));
    spans.push(Span::styled(
        format!(" {:>3.0}%", pct),
        Style::default().fg(pct_color(pct)),
    ));
    if let (Some(u), Some(w)) = (snap.ctx_used_tokens, snap.ctx_window) {
        spans.push(Span::styled(
            format!(" {}/{}", fmt_k(u), fmt_k(w)),
            Style::default().fg(DIM),
        ));
    }
    if snap.ctx_approx {
        spans.push(Span::styled(
            "  approx",
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
}
