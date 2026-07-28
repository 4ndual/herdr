//! The goal panel renderer, carved from goal-widget `src/main.rs` (~3096–3966).
//! Draws an [`App`] into a ratatui `Frame`. Bumped from ratatui 0.29 → 0.30.
//! Pure draw + the layout helpers (`goal_lines`/`node_lines`/`plans_section_lines`)
//! the Model also uses for hit-test row-height math.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::model::{has_real, st_label, GoalConstraint, GoalState, Par, Sub};
use crate::state::App;
use crate::timegate::{est_label, fmt_updated, left_label, minutes_until, short_time};
use crate::view::{badge_marker, Act, ChipSpec, EditTarget, Node, View, SESSION};

/// Selected-row bar colour (goal-widget theme).
const SEL_BAR: Color = Color::Rgb(246, 165, 160);
/// Shown when no agent session is focused.
const NO_SESSION_HINT: &str = "focus an agent pane to see its goals";

fn dim_rule(width: u16) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(Color::DarkGray),
    )))
}

pub fn draw(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    match app.view {
        View::History => draw_history(f, area, app),
        View::Proposals => draw_proposals(f, area, app),
        View::PromptLog => draw_prompt_log(f, area, app),
        View::Tree => draw_tree(f, area, app),
    }
    // Hover tooltip overlay (Tree view): when the cursor sits over a registered truncated region
    // (MAIN goal row, paraphrase footer), pop the full text in a floating box.
    if app.view == View::Tree {
        if let Some((cx, cy)) = app.hover {
            if let Some((r, text)) = app
                .tip_rects
                .iter()
                .find(|(r, _)| cy == r.y && cx >= r.x && cx < r.x + r.width)
                .map(|(r, t)| (*r, t.clone()))
            {
                render_tooltip(f, area, r, &text);
            }
        }
    }
}

/// Greedy word-wrap by terminal display width. The first output line gets `first_w` cells,
/// every continuation line `cont_w`. Long unbreakable words are hard-split. Always ≥1 line.
fn wrap_display(text: &str, first_w: usize, cont_w: usize) -> Vec<String> {
    let first_w = first_w.max(1);
    let cont_w = cont_w.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_w = 0usize;
    let mut last_space: Option<usize> = None; // byte index in `line` of the last ' '
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        let budget = if out.is_empty() { first_w } else { cont_w };
        // never start a line with a space
        if ch == ' ' && line_w == 0 {
            continue;
        }
        if line_w + cw > budget && line_w > 0 {
            if ch == ' ' {
                out.push(std::mem::take(&mut line));
                line_w = 0;
                last_space = None;
                continue;
            }
            if let Some(sp) = last_space {
                let rest = line.split_off(sp); // `line` keeps [..sp] (no trailing space)
                out.push(std::mem::take(&mut line));
                line = rest.trim_start().to_string();
                line_w = UnicodeWidthStr::width(line.as_str());
                last_space = None;
            } else {
                out.push(std::mem::take(&mut line));
                line_w = 0;
            }
        }
        if ch == ' ' {
            last_space = Some(line.len());
        }
        line.push(ch);
        line_w += cw;
    }
    while line.ends_with(' ') {
        line.pop();
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// Truncate `text` to fit `width` display cells on ONE line, appending `…` when it overflows.
/// Returns (shown, truncated?). Multibyte-safe; width 0 → empty.
fn truncate_display(text: &str, width: usize) -> (String, bool) {
    if UnicodeWidthStr::width(text) <= width {
        return (text.to_string(), false);
    }
    if width == 0 {
        return (String::new(), true);
    }
    let budget = width.saturating_sub(1); // reserve a cell for the ellipsis
    let mut acc = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        acc.push(ch);
        w += cw;
    }
    acc.push('…');
    (acc, true)
}

/// Floating tooltip: a yellow rounded box (Clear-backed) showing `text` wrapped ≤56 cols, anchored
/// just below `anchor` (or above when there's no room below), clamped on-screen. Cheap — ratatui
/// diffs identical frames, so re-rendering on every hover move is near-free.
fn render_tooltip(f: &mut ratatui::Frame, area: Rect, anchor: Rect, text: &str) {
    let maxw = (area.width.saturating_sub(2)).clamp(10, 56) as usize;
    let lines = wrap_display(text, maxw, maxw);
    let w = (lines
        .iter()
        .map(|l| UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0) as u16
        + 2)
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

pub fn goal_lines(
    icon: &str,
    label: &str,
    text: &str,
    source: &str,
    approved: bool,
    width: usize,
    cont_indent: usize,
    wrap: bool,
) -> Vec<Line<'static>> {
    let (m, c) = badge_marker(source, approved);
    let has = has_real(text);
    let shown = if has { text } else { "(unset — e to set)" };
    let text_style = if has {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let badge = format!("{m} ");
    let prefix_w = UnicodeWidthStr::width(icon)
        + UnicodeWidthStr::width(label)
        + UnicodeWidthStr::width(badge.as_str());
    let first_budget = width.saturating_sub(prefix_w);
    let cont_budget = width.saturating_sub(cont_indent);
    // `wrap=false` ⇒ exactly ONE line, truncated with `…` (a hover tooltip reveals the full text);
    // `wrap=true` ⇒ as many wrapped lines as needed, never truncated.
    let chunks = if wrap {
        wrap_display(shown, first_budget, cont_budget)
    } else {
        vec![truncate_display(shown, first_budget).0]
    };
    let mut lines = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::raw(icon.to_string()),
                Span::styled(
                    label.to_string(),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    badge.clone(),
                    Style::default().fg(c).add_modifier(Modifier::BOLD),
                ),
                Span::styled(chunk, text_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(cont_indent)),
                Span::styled(chunk, text_style),
            ]));
        }
    }
    lines
}

/// A sub-goal node wrapped to as many lines as needed; continuation hangs under the text.
fn sub_lines(s: &Sub, width: usize) -> Vec<Line<'static>> {
    let (m, c) = badge_marker(&s.source, s.approved);
    let indent = 4usize;
    let badge = format!("{m} ");
    let prefix_w = indent + UnicodeWidthStr::width(badge.as_str());
    let has = has_real(&s.text);
    let shown = if has { s.text.as_str() } else { "(empty)" };
    let text_style = if has {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let chunks = wrap_display(
        shown,
        width.saturating_sub(prefix_w),
        width.saturating_sub(prefix_w),
    );
    let mut lines = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(
                    badge.clone(),
                    Style::default().fg(c).add_modifier(Modifier::BOLD),
                ),
                Span::styled(chunk, text_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(prefix_w)),
                Span::styled(chunk, text_style),
            ]));
        }
    }
    lines
}

/// Meta line(s) shown under MAIN/CURRENT: `set · chg`, then `est · due · time-left` (time-left green,
/// or RED when overdue), then a collapsible `▸/▾ constraints (N)` row + the bullets when expanded.
fn meta_lines(
    app: &App,
    entity: &str,
    st: &GoalState,
    indent: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut out: Vec<Line<'static>> = Vec::new();
    // ONE meta line: est · due · time-left. The set/changed timestamps moved to a hover tooltip on this
    // row (see meta_tip_info) — they cost a whole line and are low-value at a glance.
    let due = if st.deadline.is_empty() {
        "—".to_string()
    } else {
        short_time(&st.deadline)
    };
    let mut spans = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(
            format!("est {} · due {due} · ", est_label(st.est_minutes)),
            dim,
        ),
    ];
    match minutes_until(&st.deadline) {
        Some(mins) => {
            let (lbl, overdue) = left_label(mins);
            let color = if overdue { Color::Red } else { Color::Green };
            spans.push(Span::styled(
                lbl,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }
        None => spans.push(Span::styled("/time-gate to estimate", dim)),
    }
    out.push(Line::from(spans));
    let cons: Vec<&GoalConstraint> = app
        .constraints
        .iter()
        .filter(|c| c.entity == entity)
        .collect();
    let expanded = app.cons_expanded.contains(entity);
    let caret = if expanded { "▾" } else { "▸" };
    out.push(Line::from(vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(format!("{caret} constraints ({})", cons.len()), dim),
    ]));
    if expanded {
        // bullets WRAP (never truncate); continuation lines hang-indent under the bullet text.
        let avail = width.saturating_sub(indent + 2).max(1);
        let cont = width.saturating_sub(indent + 4).max(1);
        for (i, c) in cons.iter().enumerate() {
            let sel = app.cons_sel == Some((entity.to_string(), i));
            let bullet = if sel { "▸ •" } else { "•" };
            let src = if c.source == "agent" { "  (agent)" } else { "" };
            let style = if sel {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let segs = wrap_display(&format!("{bullet} {}{src}", c.text), avail, cont);
            for (j, seg) in segs.into_iter().enumerate() {
                let pad = if j == 0 { indent + 2 } else { indent + 4 };
                out.push(Line::from(vec![
                    Span::raw(" ".repeat(pad)),
                    Span::styled(seg, style),
                ]));
            }
        }
    }
    out
}

/// MAIN goal row identity — kept as consts so node_lines (render) and draw_tree (tooltip budget) agree.
pub const MAIN_ICON: &str = "🎯 ";
pub const MAIN_LABEL: &str = "MAIN ";
/// First-line text budget (cells) for the MAIN goal row at `width` — used to decide truncation for
/// the hover tooltip. Mirrors `goal_lines`' prefix math (icon + label + badge).
fn main_goal_budget(source: &str, approved: bool, width: usize) -> usize {
    let (m, _) = badge_marker(source, approved);
    let prefix_w = UnicodeWidthStr::width(MAIN_ICON)
        + UnicodeWidthStr::width(MAIN_LABEL)
        + UnicodeWidthStr::width(format!("{m} ").as_str());
    width.saturating_sub(prefix_w)
}

/// Build the rendered line(s) for one visible tree node. CURRENT + subs wrap; MAIN is ONE truncated
/// line (hover reveals the full text); section headers are 1 line.
fn node_lines(app: &App, node: Node, width: usize) -> Vec<Line<'static>> {
    match node {
        Node::Main => {
            let s = app.state.clone().unwrap_or_default();
            // MAIN renders as a single truncated line (wrap=false); CURRENT stays fully wrapped.
            let mut lines = goal_lines(
                MAIN_ICON, MAIN_LABEL, &s.text, &s.source, s.approved, width, 3, false,
            );
            lines.extend(meta_lines(app, "main", &s, 3, width));
            lines
        }
        Node::Current => {
            // ↻ marks a CURRENT the Goal Observer changed (reason is in its ☰ Log).
            let label = if app.tree.current.source == "agent" && app.tree.current.reason.is_some() {
                "CURRENT ↻ "
            } else {
                "CURRENT "
            };
            let mut lines = goal_lines(
                "◆ ",
                label,
                &app.tree.current.text,
                &app.tree.current.source,
                app.tree.current.approved,
                width,
                3,
                true,
            );
            lines.extend(meta_lines(app, "current", &app.tree.current, 3, width));
            lines
        }
        Node::Section(par, st) => {
            let arrow = if app.is_collapsed(par, st) {
                "▸"
            } else {
                "▾"
            };
            let hint = match par {
                Par::Current => "",
                Par::Main => " ·main",
                Par::Indep => " ·indep",
            };
            let n = app.subs_in(par, st).len();
            vec![Line::from(Span::styled(
                format!("  {arrow} {}{hint} ({n})", st_label(st)),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ))]
        }
        Node::Sub(id) => {
            let s = app.sub_ref(id).cloned().unwrap_or_else(|| Sub {
                id,
                text: String::new(),
                source: String::new(),
                approved: false,
                state: "active".into(),
                parent: "current".into(),
                created_at: String::new(),
                updated_at: String::new(),
                pinned: false,
            });
            sub_lines(&s, width)
        }
    }
}

/// What a plans-section row does when clicked: nothing, fold the whole section, or fold one plan.
enum PlanClick {
    None,
    Header,
    Row(String),
}

/// Color one line of a plan's markdown body (mirror claude-dash `plan_line_color`): H1 magenta,
/// H2/H3 blue, table + HTML-comment rows dimmed, everything else default foreground.
fn plan_line_style(l: &str) -> Style {
    let t = l.trim_start();
    if l.starts_with("# ") {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if l.starts_with("## ") || l.starts_with("### ") {
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    } else if t.starts_with('|') || t.starts_with("<!--") {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    }
}

/// Build the plans section that hangs below the goal tree in the goal pane: a blank separator, a
/// `▾/▸ plans · <project> · N` header that folds the WHOLE list, then (when open) each plan as a
/// compact `▸ title` row + a dim `· updated · est` meta row; an expanded plan also emits its full
/// body, color-coded and wrapped. Every line carries its click action. Read-only — bodies come from
/// the lazily-cached `plans_body`.
fn plans_section_lines(app: &App, width: usize) -> Vec<(Line<'static>, PlanClick)> {
    let dim = Style::default().fg(Color::DarkGray);
    let head_st = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let mut out: Vec<(Line<'static>, PlanClick)> = Vec::new();
    // Hidden entirely for non-project (global) goals — no project ⇒ no plans section.
    let label = &app.plans_label;
    if label.is_empty() {
        return out;
    }
    // (No leading blank: the last tree node already appends a spacer row above us.)
    let caret = if app.plans_section_open { "▾" } else { "▸" };
    let header = format!("{caret} plans · {label} · {}", app.plans.len());
    out.push((Line::from(Span::styled(header, head_st)), PlanClick::Header));
    if !app.plans_section_open {
        return out;
    }
    if app.plans.is_empty() {
        out.push((
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "no plans for this project — Claude Code writes them to ~/.claude/plans",
                    dim,
                ),
            ]),
            PlanClick::None,
        ));
        return out;
    }
    for (i, p) in app.plans.iter().enumerate() {
        // The newest plan (i==0) is "current": expanded by default unless folded. Others stay a single
        // title line (compressed) until clicked.
        let is_current = i == 0;
        let expanded = if is_current {
            !app.plans_current_folded
        } else {
            app.plans_expanded.contains(&p.basename)
        };
        let pc = if expanded { "▾" } else { "▸" };
        let marker = if is_current { "▶ " } else { "" }; // ▶ flags the plan you're currently on
                                                         // compact title row — title always wraps (honors the always-wrap constraint), never truncates.
        let title_w = width.saturating_sub(2).max(1);
        let cont_w = width.saturating_sub(4).max(1);
        let segs = wrap_display(&format!("{pc} {marker}{}", p.title), title_w, cont_w);
        for (j, seg) in segs.into_iter().enumerate() {
            let pad = if j == 0 { 2 } else { 4 };
            let st = if is_current {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            out.push((
                Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(seg, st)]),
                PlanClick::Row(p.basename.clone()),
            ));
        }
        // collapsed (non-current) → title only, no meta row. Expanded → meta row + full body.
        if !expanded {
            continue;
        }
        let meta = format!(
            "· updated {} · est {}",
            fmt_updated(p.mtime_ms),
            est_label(p.p50)
        );
        out.push((
            Line::from(vec![Span::raw("    "), Span::styled(meta, dim)]),
            PlanClick::Row(p.basename.clone()),
        ));
        if let Some(body) = app.plans_body.get(&p.basename) {
            let bw = width.saturating_sub(4).max(1);
            for raw in body.lines() {
                let style = plan_line_style(raw);
                let segs = wrap_display(raw, bw, bw);
                if segs.is_empty() {
                    out.push((Line::from(""), PlanClick::None));
                } else {
                    for seg in segs {
                        out.push((
                            Line::from(vec![Span::raw("    "), Span::styled(seg, style)]),
                            PlanClick::None,
                        ));
                    }
                }
            }
        }
        out.push((Line::from(""), PlanClick::None));
    }
    out
}

fn draw_tree(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    // Paraphrase footer ("the last message" — the latest prompt's meaning): ONE truncated line; a
    // hover tooltip reveals the full text. (Capped to ≤240 chars when written.)
    let para_full = format!("💬 {}", app.paraphrase);
    let (para_line, para_trunc) = if app.paraphrase.is_empty() {
        (String::new(), false)
    } else {
        truncate_display(&para_full, area.width.max(1) as usize)
    };
    let para_text: Vec<String> = if app.paraphrase.is_empty() {
        Vec::new()
    } else {
        vec![para_line]
    };
    let para_h = para_text.len().min(9) as u16; // cap so a long paraphrase never swallows the tree
                                                // Layout is dynamic: a proposals banner row appears only when proposals are pending.
    let has_banner = !app.proposals.is_empty();
    let mut cons: Vec<Constraint> = vec![Constraint::Length(1)]; // title
    let banner_idx = if has_banner {
        cons.push(Constraint::Length(1));
        Some(cons.len() - 1)
    } else {
        None
    };
    cons.push(Constraint::Length(1)); // divider (top)
    cons.push(Constraint::Min(1));
    let body_idx = cons.len() - 1; // tree body
    cons.push(Constraint::Length(1));
    let div_bot = cons.len() - 1; // divider (bottom)
    cons.push(Constraint::Length(para_h));
    let para_idx = cons.len() - 1; // paraphrase footer (full, wrapped)
    cons.push(Constraint::Length(1));
    let status_idx = cons.len() - 1; // status
    cons.push(Constraint::Length(1));
    let bar_idx = cons.len() - 1; // single session bar (last screen row)
    let rows = Layout::vertical(cons).split(area);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "🎯 GOALS",
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );
    app.banner_rect = None;
    if let Some(bi) = banner_idx {
        let n = app.proposals.len();
        let txt = format!("▲ {n} proposal{} · v review", if n == 1 { "" } else { "s" });
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                txt,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))),
            rows[bi],
        );
        app.banner_rect = Some(rows[bi]);
    }
    f.render_widget(dim_rule(area.width), rows[banner_idx.map_or(1, |b| b + 1)]);

    app.node_rects.clear();
    app.controls.clear();
    app.tip_rects.clear();
    app.chip_rects.clear();

    if app.current_sid.is_none() {
        let body = vec![
            Line::from(Span::styled(
                "— no focused session —",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                NO_SESSION_HINT,
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: true }),
            rows[body_idx],
        );
        f.render_widget(dim_rule(area.width), rows[div_bot]);
        return;
    }

    app.rebuild_nodes();
    let body = rows[body_idx];
    let avail_h = body.height.max(1) as usize;
    // Reserve column 0 of the body as a selection gutter: the selected node shows a thin accent bar
    // there, so we highlight WITHOUT a full-line background over the goal text (readability). Text is
    // rendered one column in; clicks still register against the full-width row.
    let text_x = body.x + 1;
    let text_width = body.width.saturating_sub(1);
    let text_w = text_width as usize;

    // Render each node to its wrapped line block; goal text never truncates (it wraps). The SELECTED
    // node gets a contextual chip row appended right under its text; a trailing blank gives breathing
    // room after the always-present top entities (and after a sub's chip row).
    let mut blocks: Vec<Vec<Line<'static>>> = Vec::with_capacity(app.nodes.len());
    let mut chip_info: Vec<Option<(usize, Vec<ChipSpec>)>> = Vec::with_capacity(app.nodes.len());
    for (idx, n) in app.nodes.iter().enumerate() {
        let mut b = node_lines(app, *n, text_w);
        let mut ci = None;
        if idx == app.sel_node {
            let specs = app.chip_specs(*n);
            if !specs.is_empty() {
                let (chip_line, _) = layout_chips(&specs, app.sel_ctrl);
                let row = b.len();
                b.push(chip_line);
                ci = Some((row, specs));
            }
        }
        if matches!(*n, Node::Main | Node::Current) || ci.is_some() {
            b.push(Line::from(""));
        }
        blocks.push(b);
        chip_info.push(ci);
    }
    // For each node, the relative row of its `▸/▾ constraints (N)` header (so a click on that row folds
    // it like the `c` key). Computed before the mutable render walk.
    let cons_info: Vec<Option<(usize, String)>> = app
        .nodes
        .iter()
        .map(|n| app.cons_header_info(*n, text_w))
        .collect();
    // Per node, the est-line row + the "set·changed" hover text (folded off its own line).
    let meta_info: Vec<Option<(usize, String)>> = app
        .nodes
        .iter()
        .map(|n| app.meta_tip_info(*n, text_w))
        .collect();
    app.cons_header_rects.clear();
    let heights: Vec<usize> = blocks.iter().map(|b| b.len()).collect();
    let mut starts: Vec<usize> = Vec::with_capacity(heights.len());
    let mut acc = 0usize;
    for h in &heights {
        starts.push(acc);
        acc += h;
    }
    let node_rows = acc;

    // The plans section hangs below the goal tree, sharing the same scroll buffer so selection/scroll
    // math stays unified. Build its lines once; they render after the node blocks.
    app.plans_head_rect = None;
    app.plans_row_rects.clear();
    let plan_lines = plans_section_lines(app, text_w);
    let total_rows = node_rows + plan_lines.len();

    // Row-based scroll: keep the selected node's block visible (prefer its top if it's tall).
    let mut offset = 0usize;
    if total_rows > avail_h {
        let sel = app.sel_node.min(starts.len().saturating_sub(1));
        let sel_start = *starts.get(sel).unwrap_or(&0);
        let sel_end = sel_start + heights.get(sel).copied().unwrap_or(1);
        offset = sel_end.saturating_sub(avail_h).min(sel_start);
        offset = offset.min(total_rows - avail_h);
    }

    // MAIN goal is one truncated line; if it overflowed, its first row gets a hover tooltip with the
    // full text. Compute the full text + whether it truncated once, before the render walk.
    let main_idx = app.nodes.iter().position(|n| matches!(n, Node::Main));
    let main_tip: Option<String> = app
        .state
        .as_ref()
        .map(|s| (s.text.clone(), s.source.clone(), s.approved))
        .filter(|(t, _, _)| has_real(t))
        .and_then(|(t, src, ap)| {
            let (_, trunc) = truncate_display(&t, main_goal_budget(&src, ap, text_w));
            if trunc {
                Some(t)
            } else {
                None
            }
        });

    // Walk blocks, emitting only the rows inside the [offset, offset+avail_h) window.
    'outer: for (idx, block) in blocks.into_iter().enumerate() {
        let node_top = starts[idx];
        let selected = idx == app.sel_node;
        for (li, line) in block.into_iter().enumerate() {
            let abs = node_top + li;
            if abs < offset {
                continue;
            }
            let sr = abs - offset;
            if sr >= avail_h {
                break 'outer;
            }
            let y = body.y + sr as u16;
            let rect = Rect::new(body.x, y, body.width, 1); // full-width row → click anywhere
                                                            // selection gutter: a solid accent stripe (a bg-colored space, NOT a glyph) in column 0 for
                                                            // the selected node's content rows — highlights the node without touching the goal text.
                                                            // Skip blank spacer rows so the stripe doesn't dangle below the block.
            if selected && line.width() > 0 {
                f.render_widget(
                    Paragraph::new(" ").style(Style::default().bg(SEL_BAR)),
                    Rect::new(body.x, y, 1, 1),
                );
            }
            f.render_widget(Paragraph::new(line), Rect::new(text_x, y, text_width, 1));
            app.node_rects.push((rect, idx)); // one rect per visible row → click any row selects the node
                                              // MAIN's first (truncated) row → hover tooltip region with the full goal text.
            if li == 0 && main_idx == Some(idx) {
                if let Some(t) = &main_tip {
                    app.tip_rects.push((rect, t.clone()));
                }
            }
            // the `▸/▾ constraints (N)` row is also a fold control (clicking it expands/collapses).
            if let Some((hr, ent)) = cons_info.get(idx).and_then(|o| o.as_ref()) {
                if li == *hr {
                    app.cons_header_rects.push((rect, ent.clone()));
                }
            }
            // the est meta line carries the set/changed timestamps as a hover tooltip.
            if let Some((mr, tip)) = meta_info.get(idx).and_then(|o| o.as_ref()) {
                if li == *mr {
                    app.tip_rects.push((rect, tip.clone()));
                }
            }
            // the selected node's chip row: register a click rect + hover tooltip per chip.
            if let Some((crow, specs)) = chip_info.get(idx).and_then(|o| o.as_ref()) {
                if li == *crow {
                    let (_, offs) = layout_chips(specs, app.sel_ctrl);
                    for (ci, (rx, w)) in offs.iter().enumerate() {
                        let cr = Rect::new(text_x + rx, y, *w, 1);
                        app.chip_rects.push((cr, specs[ci].act));
                        app.tip_rects.push((cr, specs[ci].tip.clone()));
                    }
                }
            }
        }
    }

    // Plans section: emit its lines right after the node blocks, in the same window. Register the
    // header rect and each plan-row rect so clicks fold the section / a plan (Tree click handler).
    for (i, (line, click)) in plan_lines.into_iter().enumerate() {
        let abs = node_rows + i;
        if abs < offset {
            continue;
        }
        let sr = abs - offset;
        if sr >= avail_h {
            break;
        }
        let y = body.y + sr as u16;
        let rect = Rect::new(body.x, y, body.width, 1);
        f.render_widget(Paragraph::new(line), Rect::new(text_x, y, text_width, 1));
        match click {
            PlanClick::Header => app.plans_head_rect = Some(rect),
            PlanClick::Row(b) => app.plans_row_rects.push((rect, b)),
            PlanClick::None => {}
        }
    }

    f.render_widget(dim_rule(area.width), rows[div_bot]);

    // Paraphrase footer — the latest prompt's ≤240-char meaning ("did it understand me"), ONE
    // truncated line; hover reveals the full text via the tooltip region registered below.
    if !para_text.is_empty() {
        let lines: Vec<Line> = para_text
            .iter()
            .map(|s| Line::from(Span::styled(s.clone(), Style::default().fg(Color::Cyan))))
            .collect();
        f.render_widget(Paragraph::new(lines), rows[para_idx]);
        if para_trunc {
            app.tip_rects.push((rows[para_idx], app.paraphrase.clone()));
        }
    }

    // status row (or a key hint when idle)
    let status = if app.status.is_empty() {
        "↑↓ pick · ⏎ do · t time · c/C constraints · P plans · w save · v props · m log · q"
            .to_string()
    } else {
        app.status.clone()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::DarkGray),
        ))),
        rows[status_idx],
    );

    if app.editing {
        // Edit prompt occupies the first bar row; second bar row stays empty.
        let prompt = match app.edit_target {
            EditTarget::NewSub => "sub> ",
            EditTarget::TimeMain | EditTarget::TimeCurrent => "est (2h/90m)> ",
            EditTarget::ConstraintMain | EditTarget::ConstraintCurrent => "constraint> ",
            _ => "set> ",
        };
        let line = Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Cyan)),
            Span::raw(app.edit_buf.clone()),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ]);
        let footer = Rect::new(
            rows[bar_idx].x,
            rows[bar_idx].y,
            rows[bar_idx].width.saturating_sub(1),
            1,
        );
        f.render_widget(Paragraph::new(line), footer);
        return;
    }

    // The single contextual session bar. Reserve the bottom-right cell so nothing writes it (that
    // write would scroll the terminal and ghost the frame). Item actions live in the chip row instead.
    draw_session_bar(f, app, rows[bar_idx]);
}

/// The Proposals view — each suggestion is a collapsible dropdown: the headline (mark + goal text,
/// always WRAPPED, never truncated at the pane edge) is the non-collapsible part; the reason folds
/// under a ▸/▾ caret. `↑↓` pick, `⏎` expand/collapse, `y` accept, `x` dismiss. Long content scrolls so
/// the selected proposal stays visible.
fn draw_proposals(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("▲ PROPOSALS ({})", app.proposals.len()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );
    f.render_widget(dim_rule(area.width), rows[1]);
    app.prop_rects.clear();
    let body = rows[2];
    let w = (body.width as usize).max(1);
    let ind = "  "; // continuation / body indent

    // Build the full wrapped line list once, tagged with the owning proposal index; a header line per
    // proposal lets us scroll to keep the selected one in view.
    let mut lines: Vec<(Line, usize, bool)> = Vec::new(); // (line, prop_idx, is_header)
    for (idx, p) in app.proposals.iter().enumerate() {
        let selected = idx == app.sel_prop;
        let expanded = app.prop_expanded.contains(&idx);
        let has_reason = !p.reason.is_empty();
        let mark = match p.kind.as_str() {
            "main" => "MAIN",
            "current" => "CURRENT",
            "constraint:main" => "CONSTRAINT→MAIN",
            "constraint:current" => "CONSTRAINT→CURRENT",
            _ => "?",
        };
        let caret = if !has_reason {
            "• "
        } else if expanded {
            "▾ "
        } else {
            "▸ "
        };
        // Header: caret + mark (the fold control). Always one short line; mark is bounded.
        lines.push((
            Line::from(vec![
                Span::styled(
                    caret.to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(
                    mark.to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            idx,
            true,
        ));
        // Goal text: ALWAYS shown, ALWAYS wrapped (indent under the caret), capped at MAX_INLINE upstream.
        let tstyle = Style::default().add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
        for seg in wrap_display(
            &p.text,
            w.saturating_sub(ind.len()),
            w.saturating_sub(ind.len()),
        ) {
            lines.push((
                Line::from(vec![Span::raw(ind.to_string()), Span::styled(seg, tstyle)]),
                idx,
                false,
            ));
        }
        // Reason: the collapsible part — only when this dropdown is expanded.
        if has_reason && expanded {
            let segs = wrap_display(
                &p.reason,
                w.saturating_sub(ind.len() + 8),
                w.saturating_sub(ind.len()),
            );
            for (i, seg) in segs.into_iter().enumerate() {
                let txt = if i == 0 {
                    format!("{ind}reason: {seg}")
                } else {
                    format!("{ind}{seg}")
                };
                lines.push((
                    Line::from(Span::styled(txt, Style::default().fg(Color::DarkGray))),
                    idx,
                    false,
                ));
            }
        }
    }

    let height = body.height as usize;
    let total = lines.len();
    let max_off = total.saturating_sub(height);
    // Anchor the selected proposal's header at/near the top when the list overflows.
    let sel_line = lines
        .iter()
        .position(|(_, i, h)| *h && *i == app.sel_prop)
        .unwrap_or(0);
    let off = if total <= height {
        0
    } else {
        sel_line.min(max_off)
    };

    let mut y = body.y;
    for (line, idx, _) in lines.into_iter().skip(off).take(height) {
        let rect = Rect::new(body.x, y, body.width, 1);
        f.render_widget(Paragraph::new(line), rect);
        app.prop_rects.push((rect, idx));
        y += 1;
    }

    f.render_widget(dim_rule(area.width), rows[3]);
    let hint = if app
        .proposals
        .get(app.sel_prop)
        .map(|p| !p.reason.is_empty())
        .unwrap_or(false)
    {
        "↑↓ pick · ⏎ expand · y accept · x dismiss"
    } else {
        "↑↓ pick · y accept · x dismiss · Esc back"
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        rows[4],
    );
}

/// The Prompt-Log view — the per-prompt paraphrase trail, newest first.
fn draw_prompt_log(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "💬 PROMPT LOG",
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );
    f.render_widget(dim_rule(area.width), rows[1]);
    app.plog_rects.clear();
    let body = rows[2];
    let w = body.width as usize;
    let mut y = body.y;
    // newest first
    for (idx, e) in app.plog.iter().enumerate().rev() {
        if y >= body.y + body.height {
            break;
        }
        let (mark, color) = plog_marker(&e.action);
        let t = short_time(&e.ts);
        let head = Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(color)),
            Span::styled(format!("{t}  "), Style::default().fg(Color::DarkGray)),
            Span::raw(
                wrap_display(&e.paraphrase, w.saturating_sub(10), w)
                    .into_iter()
                    .next()
                    .unwrap_or_default(),
            ),
        ]);
        let rect = Rect::new(body.x, y, body.width, 1);
        f.render_widget(Paragraph::new(head), rect);
        app.plog_rects.push((rect, idx));
        y += 1;
        let detail = if e.action == "noop" {
            String::new()
        } else if e.reason.is_empty() {
            format!("→ {}", e.action)
        } else {
            format!("→ {} · {}", e.action, e.reason)
        };
        if !detail.is_empty() && y < body.y + body.height {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(
                        "   {}",
                        wrap_display(&detail, w.saturating_sub(3), w)
                            .into_iter()
                            .next()
                            .unwrap_or_default()
                    ),
                    Style::default().fg(Color::DarkGray),
                ))),
                Rect::new(body.x, y, body.width, 1),
            );
            y += 1;
        }
    }
    f.render_widget(dim_rule(area.width), rows[3]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ scroll · Esc back · q quit",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[4],
    );
}

/// Marker + color for a prompt-log row, keyed off the recorded action summary.
fn plog_marker(action: &str) -> (&'static str, Color) {
    if action == "noop" {
        ("~", Color::DarkGray)
    } else if action.contains("main") {
        ("▲", Color::Yellow)
    } else if action.contains("current") {
        ("↻", Color::Cyan)
    } else if action.contains("sub") {
        ("+", Color::Green)
    } else {
        ("·", Color::Gray)
    }
}

/// Render one control-bar row; record clickable rects (with their global action index).
/// Lay out a chip row: returns the styled `Line` plus each chip's (relative-x, width) so the caller
/// can register click/hover rects at render time. `sel` highlights one chip (REVERSED). Each chip is
/// ` glyph ` — a padded icon cell; Approve is green, Discard red, everything else gray.
fn layout_chips(specs: &[ChipSpec], sel: usize) -> (Line<'static>, Vec<(u16, u16)>) {
    let mut spans: Vec<Span> = Vec::new();
    let mut offs: Vec<(u16, u16)> = Vec::new();
    let mut x: u16 = 0;
    for (i, c) in specs.iter().enumerate() {
        let cell = format!(" {} ", c.glyph);
        let w = UnicodeWidthStr::width(cell.as_str()) as u16;
        let base = match c.act {
            Act::Approve => Color::Green,
            Act::Discard => Color::Red,
            _ => Color::Gray,
        };
        let style = if i == sel {
            Style::default()
                .fg(base)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(base)
        };
        spans.push(Span::styled(cell, style));
        offs.push((x, w));
        x = x.saturating_add(w);
    }
    (Line::from(spans), offs)
}

/// The single bottom bar: whole-session actions only (clear & re-brief, auto-sort, add subgoal). Each
/// label registers a click rect (`controls`) and a hover tooltip (`tip_rects`). Reserves the last cell.
fn draw_session_bar(f: &mut ratatui::Frame, app: &mut App, row: Rect) {
    let mut spans: Vec<Span> = Vec::new();
    let mut x = row.x;
    for (act, label, tip) in SESSION.iter() {
        let w = UnicodeWidthStr::width(*label) as u16;
        let r = Rect::new(x, row.y, w, 1);
        app.controls.push((r, *act));
        app.tip_rects.push((r, (*tip).to_string()));
        spans.push(Span::styled(*label, Style::default().fg(Color::Gray)));
        x = x.saturating_add(w);
    }
    let dest = Rect::new(row.x, row.y, row.width.saturating_sub(1), 1); // reserve last cell (ghost-frame guard)
    f.render_widget(Paragraph::new(Line::from(spans)), dest);
}

/// The History screen: a clickable list of every goal change (newest first) with badge + time.
/// Clicking (or Enter on) a row re-applies that goal as the current one.
/// One history entry as a block of wrapped lines (never truncated): first line = `time marker text…`,
/// continuation lines hang-indent under the text, then the `why: …` reason wrapped + dimmed. `width` is
/// the text column (gutter already reserved by the caller).
fn history_entry_lines(
    entry: &GoalState,
    width: usize,
    marker: &str,
    color: Color,
) -> Vec<Line<'static>> {
    let time = short_time(&entry.updated_at);
    let prefix_w = UnicodeWidthStr::width(format!("{time} {marker} ").as_str());
    // align continuation under the text, but fall back to a small hang in a narrow pane
    let indent_w = if prefix_w + 8 <= width { prefix_w } else { 2 };
    let first_w = width.saturating_sub(prefix_w).max(1);
    let cont_w = width.saturating_sub(indent_w).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, seg) in wrap_display(&entry.text, first_w, cont_w)
        .into_iter()
        .enumerate()
    {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(format!("{time} "), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{marker} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(seg),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent_w)),
                Span::raw(seg),
            ]));
        }
    }
    if let Some(reason) = entry.reason.as_ref().filter(|r| !r.is_empty()) {
        for seg in wrap_display(&format!("why: {reason}"), cont_w, cont_w) {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent_w)),
                Span::styled(seg, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    lines
}

fn draw_history(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let rows = Layout::vertical([
        Constraint::Length(1), // title
        Constraint::Length(1), // divider
        Constraint::Min(1),    // list
        Constraint::Length(1), // divider
        Constraint::Length(1), // status
        Constraint::Length(1), // help
    ])
    .split(area);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("☰ HISTORY", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("  ({} changes)", app.history.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        rows[0],
    );
    f.render_widget(dim_rule(area.width), rows[1]);

    app.hist_rects.clear();
    let list = rows[2];
    let avail_h = list.height as usize;
    let text_w = list.width.saturating_sub(1) as usize; // reserve column 0 for the selection gutter
                                                        // Newest first: display row r -> history index (len-1-r). Each entry wraps to a block of lines.
    let blocks: Vec<Vec<Line<'static>>> = (0..app.history.len())
        .map(|r| {
            let entry = &app.history[app.history.len() - 1 - r];
            let (marker, color) = if entry.approved {
                ("✓", Color::Green)
            } else if entry.source == "user" {
                ("•", Color::Green)
            } else {
                ("~", Color::Yellow)
            };
            history_entry_lines(entry, text_w, marker, color)
        })
        .collect();
    let heights: Vec<usize> = blocks.iter().map(|b| b.len()).collect();
    let mut starts: Vec<usize> = Vec::with_capacity(heights.len());
    let mut acc = 0usize;
    for h in &heights {
        starts.push(acc);
        acc += h;
    }
    let total_rows = acc;

    // Scroll so the selected entry's block stays visible (mirror draw_tree's row-based scroll).
    let sel_r = app
        .history
        .len()
        .saturating_sub(1)
        .saturating_sub(app.sel_hist);
    let mut offset = 0usize;
    if total_rows > avail_h {
        let sel_start = starts.get(sel_r).copied().unwrap_or(0);
        let sel_end = sel_start + heights.get(sel_r).copied().unwrap_or(1);
        offset = sel_end.saturating_sub(avail_h).min(sel_start);
        offset = offset.min(total_rows - avail_h);
    }

    'outer: for (r, block) in blocks.into_iter().enumerate() {
        let top = starts[r];
        let idx = app.history.len() - 1 - r;
        let selected = idx == app.sel_hist;
        for (li, line) in block.into_iter().enumerate() {
            let abs = top + li;
            if abs < offset {
                continue;
            }
            let sr = abs - offset;
            if sr >= avail_h {
                break 'outer;
            }
            let y = list.y + sr as u16;
            let rect = Rect::new(list.x, y, list.width, 1); // full-width row → click anywhere selects
                                                            // minimal selection highlight: a solid accent stripe in column 0 (NOT a full-line reverse,
                                                            // which made the text ugly/hard to read), skipping blank rows.
            if selected && line.width() > 0 {
                f.render_widget(
                    Paragraph::new(" ").style(Style::default().bg(SEL_BAR)),
                    Rect::new(list.x, y, 1, 1),
                );
            }
            f.render_widget(
                Paragraph::new(line),
                Rect::new(list.x + 1, y, list.width.saturating_sub(1), 1),
            );
            app.hist_rects.push((rect, idx));
        }
    }

    f.render_widget(dim_rule(area.width), rows[3]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            app.status.clone(),
            Style::default().fg(Color::DarkGray),
        ))),
        rows[4],
    );
    // Reserve the bottom-right cell (see note in draw): never write the last cell of the last row.
    let footer = Rect::new(rows[5].x, rows[5].y, rows[5].width.saturating_sub(1), 1);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ pick · ⏎/click restore · Esc back",
            Style::default().fg(Color::DarkGray),
        ))),
        footer,
    );
}
