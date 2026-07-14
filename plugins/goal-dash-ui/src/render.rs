//! Model-based tree renderer for the goal dashboard right-panel plugin (v3).
//!
//! v1 scope: **Tree view only**. Renders MAIN, CURRENT, section headers, and
//! sub-goals from the fully-precomputed [`GoalRenderModel`] the host ships
//! each frame (no clock/fs access here — every label is a plain string/opt
//! already computed host-side). Deliberately out of scope for this pass:
//! History/Proposals/PromptLog views, editing, hover tooltips, contextual
//! chips, and the plans section — all native-only for now.
//!
//! Collapse handling: when the host carves the panel down to a 3-column
//! handle (`area.width <= 3`), this renders only a slim vertical expand
//! affordance instead of the tree.

use goal_dash_model::{GoalNodeData, GoalRenderModel, Node, Par, St, SubData};
use herdr_ui_plugin_api::UiHitRect;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Action id for the header collapse button / collapsed-handle expand affordance.
/// Shared with `lib.rs`, which flips guest-owned collapse state and emits the
/// `SetSurfaceCollapsed` effect on a `left_down` of this id.
pub const ACTION_TOGGLE_PANEL: &str = "goal:toggle_panel";

const PANEL_TITLE: &str = "🎯 GOALS";
const COLLAPSE_BTN: &str = "[▶]";
const EXPAND_HANDLE: &str = "◀";
const NO_SESSION_HINT: &str = "focus an agent pane to see its goals";

/// Render the goal tree into `buf`. Returns the frame's clickable hit rects
/// (header collapse button, or the collapsed-handle expand affordance).
pub fn render(model: &GoalRenderModel, area: Rect, buf: &mut Buffer) -> Vec<UiHitRect> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    if area.width <= 3 {
        return render_handle(area, buf);
    }
    render_full(model, area, buf)
}

/// Slim collapsed-handle: a single "◀" glyph near the top, clickable to expand.
fn render_handle(area: Rect, buf: &mut Buffer) -> Vec<UiHitRect> {
    let row = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    Paragraph::new(Line::from(Span::styled(
        EXPAND_HANDLE,
        Style::default().fg(Color::DarkGray),
    )))
    .render(row, buf);
    vec![UiHitRect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
        action_id: ACTION_TOGGLE_PANEL.to_string(),
    }]
}

fn render_full(model: &GoalRenderModel, area: Rect, buf: &mut Buffer) -> Vec<UiHitRect> {
    let width = area.width as usize;
    let mut hits = Vec::new();

    // Row 0: title left, "[▶]" collapse button far right.
    let (header, btn_x, btn_w) = header_line(width);
    let header_row = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    Paragraph::new(header).render(header_row, buf);
    hits.push(UiHitRect {
        x: area.x + btn_x,
        y: area.y,
        width: btn_w,
        height: 1,
        action_id: ACTION_TOGGLE_PANEL.to_string(),
    });

    if area.height <= 1 {
        return hits;
    }
    let body = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height - 1,
    };

    if !model.has_session {
        render_empty_hint(body, buf);
        return hits;
    }

    let lines = build_tree_lines(model, width);
    for (i, line) in lines.into_iter().take(body.height as usize).enumerate() {
        let row = Rect {
            x: body.x,
            y: body.y + i as u16,
            width: body.width,
            height: 1,
        };
        Paragraph::new(line).render(row, buf);
    }
    hits
}

/// Title + collapse button, right-aligned. Returns (line, button col offset, button width).
fn header_line(width: usize) -> (Line<'static>, u16, u16) {
    let title_w = UnicodeWidthStr::width(PANEL_TITLE);
    let btn_w = UnicodeWidthStr::width(COLLAPSE_BTN);
    let gap = width.saturating_sub(title_w + btn_w);
    let line = Line::from(vec![
        Span::styled(PANEL_TITLE, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(gap)),
        Span::styled(COLLAPSE_BTN, Style::default().fg(Color::DarkGray)),
    ]);
    let btn_x = (title_w + gap) as u16;
    (line, btn_x, btn_w as u16)
}

fn render_empty_hint(area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let (shown, _) = truncate_display(NO_SESSION_HINT, area.width.max(1) as usize);
    let w = UnicodeWidthStr::width(shown.as_str()) as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + area.height / 2;
    let rect = Rect {
        x,
        y,
        width: w.min(area.width),
        height: 1,
    };
    Paragraph::new(Line::from(Span::styled(shown, Style::default().fg(Color::DarkGray))))
        .render(rect, buf);
}

fn build_tree_lines(model: &GoalRenderModel, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for node in &model.nodes {
        match node {
            Node::Main => {
                if let Some(m) = &model.main {
                    out.extend(goal_node_lines("🎯 ", "MAIN ", m, width));
                }
            }
            Node::Current => {
                let label = if model.current.agent_changed {
                    "CURRENT ↻ "
                } else {
                    "CURRENT "
                };
                out.extend(goal_node_lines("◆ ", label, &model.current, width));
            }
            Node::Section(par, st) => out.push(section_line(model, *par, *st)),
            Node::Sub(id) => {
                if let Some(s) = model.subs.iter().find(|s| s.id == *id) {
                    out.push(sub_line(s, width));
                }
            }
        }
    }
    out
}

/// MAIN/CURRENT: one truncated goal line (icon + label + badge + text), then
/// one truncated meta line (`est · due · time-left`), both from precomputed labels.
fn goal_node_lines(icon: &str, label: &str, g: &GoalNodeData, width: usize) -> Vec<Line<'static>> {
    let (marker, color) = badge_marker(&g.source, g.approved);
    let has = has_real(&g.text);
    let shown_text = if has { g.text.as_str() } else { "(unset)" };
    let text_style = if has {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let badge = format!("{marker} ");
    let prefix_w =
        UnicodeWidthStr::width(icon) + UnicodeWidthStr::width(label) + UnicodeWidthStr::width(badge.as_str());
    let budget = width.saturating_sub(prefix_w);
    let (chunk, _) = truncate_display(shown_text, budget);

    let goal_line = Line::from(vec![
        Span::raw(icon.to_string()),
        Span::styled(
            label.to_string(),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
        ),
        Span::styled(badge, Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(chunk, text_style),
    ]);

    let indent = 3usize;
    let dim = Style::default().fg(Color::DarkGray);
    let (time_lbl, time_color) = match g.minutes_until {
        Some(mins) => {
            let (lbl, overdue) = left_label(mins);
            (lbl, if overdue { Color::Red } else { Color::Green })
        }
        None => ("/time-gate to estimate".to_string(), Color::DarkGray),
    };
    let meta_spans = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(format!("est {} · due {} · ", g.est_label, g.due_label), dim),
        Span::styled(time_lbl.clone(), Style::default().fg(time_color).add_modifier(Modifier::BOLD)),
    ];
    let meta_line = Line::from(meta_spans);
    let meta_line = if (meta_line.width() as usize) <= width {
        meta_line
    } else {
        // Rare (only very narrow panels): fall back to one truncated dim span
        // rather than truncating mid-span.
        let plain = format!("est {} · due {} · {time_lbl}", g.est_label, g.due_label);
        let (shown, _) = truncate_display(&plain, width.saturating_sub(indent));
        Line::from(vec![Span::raw(" ".repeat(indent)), Span::styled(shown, dim)])
    };

    vec![goal_line, meta_line]
}

fn section_line(model: &GoalRenderModel, par: Par, st: St) -> Line<'static> {
    let expanded = model.toggled.iter().any(|(p, s)| *p == par && *s == st);
    let arrow = if expanded { "▾" } else { "▸" };
    let hint = match par {
        Par::Current => "",
        Par::Main => " ·main",
        Par::Indep => " ·indep",
    };
    let n = model
        .subs
        .iter()
        .filter(|s| s.parent == par_str(par) && s.state == st_key(st))
        .count();
    Line::from(Span::styled(
        format!("  {arrow} {}{hint} ({n})", st_label(st)),
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    ))
}

fn sub_line(s: &SubData, width: usize) -> Line<'static> {
    let (marker, color) = badge_marker(&s.source, s.approved);
    let has = has_real(&s.text);
    let shown = if has { s.text.as_str() } else { "(empty)" };
    let indent = 4usize;
    let badge = format!("{marker} ");
    let prefix_w = indent + UnicodeWidthStr::width(badge.as_str());
    let budget = width.saturating_sub(prefix_w);
    let (chunk, _) = truncate_display(shown, budget);
    let style = if has {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Line::from(vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(badge, Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(chunk, style),
    ])
}

/// The source/approval badge glyph + color for a goal node. Mirror of the
/// pre-purge `goal_dash_core::view::badge_marker`.
fn badge_marker(source: &str, approved: bool) -> (&'static str, Color) {
    if approved {
        ("✓", Color::Green)
    } else if source == "user" {
        ("•", Color::Green)
    } else {
        ("~", Color::Yellow)
    }
}

/// True when a goal has real (non-placeholder) text. Mirror of `goal_dash_core::model::has_real`.
fn has_real(text: &str) -> bool {
    !text.is_empty() && !text.starts_with('(')
}

/// Mirror of `goal_dash_core::model::st_label`.
fn st_label(st: St) -> &'static str {
    match st {
        St::Active => "Active",
        St::Paused => "Paused",
        St::Done => "Done",
        St::Discarded => "Discarded",
    }
}

/// Lowercase key matching `SubData::state` values (mirror of `model::st_label` lowercased,
/// as written by the host's `st_key`/normalize helpers).
fn st_key(st: St) -> &'static str {
    match st {
        St::Active => "active",
        St::Paused => "paused",
        St::Done => "done",
        St::Discarded => "discarded",
    }
}

/// Lowercase key matching `SubData::parent` values. Mirror of `goal_dash_core::model::par_str`.
fn par_str(par: Par) -> &'static str {
    match par {
        Par::Current => "current",
        Par::Main => "main",
        Par::Indep => "independent",
    }
}

/// Human "time left": `3h left`, `45m left`, or `-2h`/`-15m` when overdue.
/// Byte-exact mirror of `goal_dash_core::timegate::left_label` (pure arithmetic
/// on the host-precomputed `minutes_until` — no chrono needed here).
fn left_label(mins: i64) -> (String, bool) {
    let overdue = mins < 0;
    let a = mins.abs();
    let body = if a >= 60 {
        format!("{}h", a / 60)
    } else {
        format!("{}m", a)
    };
    if overdue {
        (format!("-{body}"), true)
    } else {
        (format!("{body} left"), false)
    }
}

/// Truncate `text` to fit `width` display cells on ONE line, appending `…` when it overflows.
/// Returns (shown, truncated?). Multibyte-safe; width 0 → empty.
/// Mirror of the pre-purge `goal_dash_core::render::truncate_display`.
fn truncate_display(text: &str, width: usize) -> (String, bool) {
    if UnicodeWidthStr::width(text) <= width {
        return (text.to_string(), false);
    }
    if width == 0 {
        return (String::new(), true);
    }
    let budget = width.saturating_sub(1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use goal_dash_model::{Par, St, SubData};

    fn sample_model() -> GoalRenderModel {
        let mut m = GoalRenderModel::default();
        m.has_session = true;
        m.main = Some(GoalNodeData {
            text: "ship the widget".into(),
            source: "user".into(),
            approved: true,
            est_label: "2h".into(),
            due_label: "07-11 18:00".into(),
            minutes_until: Some(45),
            ..Default::default()
        });
        m.current = GoalNodeData {
            text: "write the render module".into(),
            source: "agent".into(),
            approved: false,
            est_label: "30m".into(),
            due_label: "—".into(),
            minutes_until: None,
            ..Default::default()
        };
        m.subs = vec![SubData {
            id: 1,
            text: "wire up hit rects".into(),
            source: "user".into(),
            approved: false,
            state: "active".into(),
            parent: "current".into(),
        }];
        m.toggled = vec![(Par::Current, St::Active)];
        m.nodes = vec![
            Node::Main,
            Node::Current,
            Node::Section(Par::Current, St::Active),
            Node::Sub(1),
        ];
        m
    }

    #[test]
    fn renders_nodes_for_a_minimal_model() {
        let model = sample_model();
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&model, area, &mut buf);
        let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);
        assert!(!nodes.is_empty(), "expected rendered lines for a minimal model");
        assert!(!hits.is_empty(), "expected at least the header collapse hit");
    }

    #[test]
    fn empty_model_shows_no_session_hint() {
        let model = GoalRenderModel::default(); // has_session = false
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        render(&model, area, &mut buf);
        let nodes = herdr_ui_plugin_api::guest::flatten_buffer(&buf);
        let text: String = nodes
            .iter()
            .map(|n| match n {
                herdr_ui_plugin_api::UiNode::Line { spans } => {
                    spans.iter().map(|s| s.text.as_str()).collect::<String>()
                }
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(NO_SESSION_HINT));
    }

    #[test]
    fn collapse_hit_rect_sits_inside_the_rect() {
        let model = sample_model();
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&model, area, &mut buf);
        let hit = hits
            .iter()
            .find(|h| h.action_id == ACTION_TOGGLE_PANEL)
            .expect("collapse hit rect present");
        assert!(hit.x + hit.width <= area.x + area.width);
        assert!(hit.y + hit.height <= area.y + area.height);
        assert!(hit.x >= area.x && hit.y >= area.y);
    }

    #[test]
    fn slim_rect_renders_only_the_handle() {
        let model = sample_model();
        let area = Rect::new(0, 0, 3, 20);
        let mut buf = Buffer::empty(area);
        let hits = render(&model, area, &mut buf);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action_id, ACTION_TOGGLE_PANEL);
        assert!(hits[0].x + hits[0].width <= area.width);
        assert!(hits[0].y + hits[0].height <= area.height);
    }
}
