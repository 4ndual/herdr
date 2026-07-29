//! WASM-safe port of `usage-dash-core/src/render.rs`.
//!
//! All logic is identical to the native renderer.  The differences are:
//!   - Entry point is `render(input, area, buf)` instead of `draw(frame, area, snap, view)`.
//!   - `frame.render_widget(w, r)` → `w.render(r, buf)`.
//!   - `abs_time` (calls `std::process::Command`) is deleted; callers supply the
//!     precomputed strings via `input.five_hour_abs` / `input.weekly_abs`.
//!   - Returns `Vec<UiHitRect>` instead of `Hits`.
//!   - All model types come from `usage_dash_model`.

use herdr_ui_plugin_api::UiHitRect;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget, Wrap};
use usage_dash_model::{
    countdown, fmt_k, Cli, CliVersion, Freshness, ThreadState, UsageRenderInput, UsageSnapshot,
    VersionToken, DAILY_BUDGET_PCT,
};

const DIM: Color = Color::DarkGray;
/// Action id for the strip-level collapse/expand toggle. Routed back to this
/// guest as a v3 `UiEvent::Action`; the guest answers with
/// `HostEffect::SetSurfaceCollapsed { UsageStrip, .. }` so the host gives the
/// reclaimed rows back to panes (a collapsed strip keeps a single row).
pub const ACTION_TOGGLE_COLLAPSE: &str = "usage:toggle_collapse";
/// The clickable collapse button drawn on the box's top border.
/// Action ids for the clickable version tokens on the caption border.
pub const ACTION_VERSION_CLAUDE: &str = "usage:version_claude";
pub const ACTION_VERSION_CODEX: &str = "usage:version_codex";

const COLLAPSE_BTN: &str = "[▾]";
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
/// `abs` is the pre-formatted reset time string (supplied by the host since WASM
/// can't call `date`). Pass `""` when there is no reset time.
fn window_row(
    name: &str,
    w: &usage_dash_model::Window,
    bar_w: usize,
    now: i64,
    abs: &str,
) -> Line<'static> {
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
/// is not up to date, and is not known to need updating.
fn freshness_color(f: Freshness) -> Color {
    match f {
        Freshness::Current => Color::Green,
        Freshness::Outdated => Color::Red,
        Freshness::Unknown => DIM,
    }
}

/// A version token's rendered text. Mirrors `usage-dash-core::render`.
fn version_token_text(v: &CliVersion, hovered: bool, confirming: bool) -> Option<String> {
    let current = v.current.as_deref()?;
    let freshness = v.freshness();
    let latest = v.latest.as_deref();

    Some(match (confirming, hovered, freshness, latest) {
        (true, _, _, Some(l)) => format!("{current}→{l} click to update?"),
        (_, true, Freshness::Outdated, Some(l)) => format!("{current}→{l}"),
        (_, true, Freshness::Current, Some(l)) => format!("{current} (latest {l})"),
        (_, true, Freshness::Unknown, _) => format!("{current} (latest unknown)"),
        _ => current.to_string(),
    })
}

/// Interaction state the guest owns for the caption's version tokens.
#[derive(Debug, Clone, Copy, Default)]
pub struct VersionUi {
    /// Token the cursor is over, if any.
    pub hovered: Option<VersionToken>,
    /// Token awaiting a confirming second click.
    pub confirming: Option<VersionToken>,
}

/// Caption spans plus each version token's offset/width within the caption.
struct Caption {
    line: Line<'static>,
    claude_span: Option<(u16, u16)>,
    codex_span: Option<(u16, u16)>,
}

/// The box caption rendered ON the top border, with the version tokens as
/// separate, individually coloured spans so they can be hit-tested.
fn caption_line(snap: &UsageSnapshot, ui: VersionUi) -> Caption {
    let mut spans = vec![Span::styled(
        snap.cli.label().to_string(),
        Style::default()
            .fg(cli_color(snap.cli))
            .add_modifier(Modifier::BOLD),
    )];
    let mut offset: u16 = spans[0].content.chars().count() as u16;
    let mut claude_span = None;
    let mut codex_span = None;

    if let Some(text) = version_token_text(
        &snap.versions.claude,
        ui.hovered == Some(VersionToken::Claude),
        ui.confirming == Some(VersionToken::Claude),
    ) {
        spans.push(Span::styled(" · ".to_string(), Style::default().fg(DIM)));
        offset += 3;
        let width = text.chars().count() as u16;
        claude_span = Some((offset, width));
        spans.push(Span::styled(
            text,
            Style::default().fg(freshness_color(snap.versions.claude.freshness())),
        ));
        offset += width;
    }

    if let Some(m) = &snap.model {
        let text = format!(" · {m}");
        offset += text.chars().count() as u16;
        spans.push(Span::styled(text, Style::default().fg(Color::White)));
    }

    if let Some(codex) = snap.versions.codex.as_ref() {
        if let Some(text) = version_token_text(
            codex,
            ui.hovered == Some(VersionToken::Codex),
            ui.confirming == Some(VersionToken::Codex),
        ) {
            let prefix = " · codex ";
            spans.push(Span::styled(prefix.to_string(), Style::default().fg(DIM)));
            offset += prefix.chars().count() as u16;
            let width = text.chars().count() as u16;
            codex_span = Some((offset, width));
            spans.push(Span::styled(
                text,
                Style::default().fg(freshness_color(codex.freshness())),
            ));
        }
    }

    if let Some(plan) = &snap.plan {
        if !plan.is_empty() {
            spans.push(Span::styled(format!(" · {plan}"), Style::default().fg(DIM)));
        }
    }

    Caption {
        line: Line::from(spans),
        claude_span,
        codex_span,
    }
}

/// The USAGE left-column cells: ctx, 5h, week, today.
fn usage_cells(
    snap: &UsageSnapshot,
    now: i64,
    five_hour_abs: &str,
    weekly_abs: &str,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(ctx_row(snap, BAR_W));
    lines.push(window_row("5h", &snap.five_hour, BAR_W, now, five_hour_abs));
    lines.push(window_row("week", &snap.weekly, BAR_W, now, weekly_abs));
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
/// lines: the AC/AR/AD/AU count row, then a single condensed metrics line.
/// Empty when `snap.stats` is None — never fabricated.
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

/// Greedy word-wrap to `width` columns.
/// Retained for when hover/tooltip support is wired into the WASM contract.
#[allow(dead_code)]
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
/// and clamped within `area`.
/// Retained for when hover/tooltip support is wired into the WASM contract.
#[allow(dead_code)]
fn render_tooltip(buf: &mut Buffer, area: Rect, anchor: Rect, text: &str) {
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
    Clear.render(tr, buf);
    let blk = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));
    let body: Vec<Line> = lines
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::White))))
        .collect();
    Paragraph::new(body).block(blk).render(tr, buf);
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

/// Render the usage panel into `buf`.
///
/// This is a pure port of `usage-dash-core::render::draw`.  No I/O, no Frame.
/// The `hover` tooltip feature is omitted — the host drives tooltips through its
/// own overlay; tooltips would require an `(hx, hy)` input that the WASM
/// contract doesn't yet expose, so we skip it here (same behavior as the native
/// renderer when `hover = None`).
///
/// Returns the clickable hit rects for this frame.
/// Draw the collapse button on the top-right of `rect`'s border and return its
/// hit rect. Skipped when the box is too narrow to keep the title readable.
fn collapse_button(rect: Rect, buf: &mut Buffer) -> Option<UiHitRect> {
    let w = COLLAPSE_BTN.chars().count() as u16;
    if rect.width < w + 4 || rect.height == 0 {
        return None;
    }
    let bx = rect.x + rect.width - w - 1;
    buf.set_string(bx, rect.y, COLLAPSE_BTN, Style::default().fg(DIM));
    Some(UiHitRect {
        x: bx,
        y: rect.y,
        width: w,
        height: 1,
        action_id: ACTION_TOGGLE_COLLAPSE.into(),
    })
}

pub fn render(
    input: &UsageRenderInput,
    area: Rect,
    buf: &mut Buffer,
    version_ui: VersionUi,
) -> Vec<UiHitRect> {
    let now = input.now;
    let expanded = input.expanded;
    let collapsed = input.collapsed;
    let stats_shown = input.stats_shown;
    let scroll = input.scroll;

    // Reserve the rightmost screen column → never write the bottom-right cell.
    let area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };

    // Surface-collapsed strip (host carves a single row): render a compact
    // summary with an expand affordance; the whole line is the click target.
    if area.height <= 1 {
        let mut spans = vec![Span::styled("[▸]", Style::default().fg(DIM))];
        match input.snapshot.as_ref() {
            Some(snap) => {
                spans.push(Span::raw(" "));
                spans.extend(collapsed_line(snap).spans);
            }
            None => spans.push(Span::styled(" usage", Style::default().fg(DIM))),
        }
        let line = Line::from(spans);
        let hit_w = (line.width() as u16).min(area.width);
        Paragraph::new(line).render(area, buf);
        return vec![UiHitRect {
            x: area.x,
            y: area.y,
            width: hit_w.max(3),
            height: 1,
            action_id: ACTION_TOGGLE_COLLAPSE.into(),
        }];
    }

    let snap = match input.snapshot.as_ref() {
        Some(s) => s,
        None => {
            // None snapshot: empty box with a placeholder message.
            let block = rounded_block();
            let inner = block.inner(area);
            block.render(area, buf);
            Paragraph::new(Line::from(Span::styled(
                "— no focused agent session —",
                Style::default().fg(DIM),
            )))
            .render(inner, buf);
            return collapse_button(area, buf).into_iter().collect();
        }
    };

    // Collapsed: whole panel folds to one summary line.
    if collapsed {
        let block = rounded_block();
        let inner = block.inner(area);
        block.render(area, buf);
        Paragraph::new(collapsed_line(snap))
            .wrap(Wrap { trim: true })
            .render(inner, buf);
        return collapse_button(area, buf).into_iter().collect();
    }

    let expandable = snap
        .state_full
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    let caption = caption_line(snap, version_ui);
    let usage = usage_cells(snap, now, &input.five_hour_abs, &input.weekly_abs);
    let stats = stats_compact(snap);
    let has_stats = !stats.is_empty();
    let (state_line, hw) = state_footer(snap, expanded);

    // Assemble body lines, tracking interactive row indices.
    //
    // The host reserves a FIXED number of rows for this surface
    // (`USAGE_STRIP_HEIGHT`, 7) and the guest cannot ask for more — there is no
    // height effect in the ABI, only `SetSurfaceCollapsed`. So expanding trades
    // the gauge/stats rows for the detail region inside the same reserve;
    // keeping them would push the footer and the detail out of the box, which
    // is exactly why expanding used to paint nothing.
    let mut body: Vec<Line> = if expanded { Vec::new() } else { usage.clone() };

    let mut stats_row: Option<usize> = None;
    let mut stats_hw: u16 = 0;
    if has_stats && !expanded {
        stats_row = Some(body.len());
        if stats_shown {
            let mut first = vec![Span::styled("▾ ", Style::default().fg(DIM))];
            first.extend(stats[0].spans.clone());
            let line = Line::from(first);
            stats_hw = line.width() as u16;
            body.push(line);
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

    // +1 for the rule inserted between body and state footer.
    let state_row = body.len() + 1;

    // Box width = widest of caption / body / state, + borders(2) + padding(2).
    let content_w = body
        .iter()
        .chain(std::iter::once(&state_line))
        .chain(std::iter::once(&caption.line))
        .map(|l| l.width())
        .max()
        .unwrap_or(0) as u16;
    let box_w = (content_w + 4).min(area.width);
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

    // Version tokens ride the top border; ratatui's left-aligned title starts one
    // column in from the box edge. Convert caption-local offsets to absolute
    // rects, dropping any token the box is too narrow to actually show — a hit
    // rect over clipped text would be an invisible click target.
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
    let claude_rect = token_rect(caption.claude_span);
    let codex_rect = token_rect(caption.codex_span);

    let block = rounded_block().title(caption.line);
    let inner = block.inner(rect);
    block.render(rect, buf);

    // Render body + rule + state footer.
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
    p.render(inner, buf);

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
                Paragraph::new(Line::from(Span::styled(
                    full.clone(),
                    Style::default().fg(Color::Gray),
                )))
                .wrap(Wrap { trim: true })
                .scroll((scroll, 0))
                .render(frect, buf);
            }
        }
    }

    // No hover → no tooltip (hover input is not in the WASM contract yet).

    // Build hit rects.
    let mut hits = Vec::new();
    hits.extend(collapse_button(rect, buf));
    if expandable {
        hits.push(UiHitRect {
            x: inner.x,
            y: state_y,
            width: hw.min(inner.width),
            height: 1,
            action_id: "usage:toggle_expand".into(),
        });
    }
    if let Some(r) = stats_row {
        hits.push(UiHitRect {
            x: inner.x,
            y: inner.y + r as u16,
            width: stats_hw.min(inner.width),
            height: 1,
            action_id: "usage:toggle_stats".into(),
        });
    }
    if let Some(r) = claude_rect {
        hits.push(UiHitRect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
            action_id: ACTION_VERSION_CLAUDE.into(),
        });
    }
    if let Some(r) = codex_rect {
        hits.push(UiHitRect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
            action_id: ACTION_VERSION_CODEX.into(),
        });
    }
    hits
}
