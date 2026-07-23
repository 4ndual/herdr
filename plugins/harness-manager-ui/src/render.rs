//! Model-based renderer for the harness-manager right-panel plugin (v3). Pure:
//! no clock/fs/env access — every string is precomputed host-side in
//! [`HarnessRenderModel`]. Mirrors the historial-dash renderer shape: render
//! into an off-screen ratatui buffer, and return the frame's clickable
//! [`UiHitRect`]s in absolute screen coordinates.
//!
//! Interaction model: the open / disable hit-rects are RESOLVED by the host (it
//! owns pane spawning and the Claude settings files), so the guest only declares
//! them. The guest owns the header collapse toggle, the hover tooltip, and the
//! right-click disable menu (which then declares host-resolved disable ids).

use harness_manager_model::{HarnessRenderModel, HarnessRow, RowState};
use herdr_ui_plugin_api::UiHitRect;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

/// Header collapse button / collapsed-handle expand affordance. Guest-owned.
pub const ACTION_TOGGLE_PANEL: &str = "harness:toggle_panel";
/// Left-click open (host-resolved: spawns an editor pane on the row's path).
pub const ACTION_OPEN_PREFIX: &str = "harness:open:";
/// Row without an open target but with a disable menu (guest right-click only).
pub const ACTION_ROW_PREFIX: &str = "harness:row:";
/// Disable-menu options (host-resolved config edits).
pub const ACTION_DISABLE_SESSION_PREFIX: &str = "harness:disable_session:";
pub const ACTION_DISABLE_PROFILE_PREFIX: &str = "harness:disable_profile:";
/// Closes the disable menu (guest-owned).
pub const ACTION_MENU_CANCEL: &str = "harness:menu_cancel";

const PANEL_TITLE: &str = "HARNESS";
const COLLAPSE_BTN: &str = "[<]";
const EXPAND_HANDLE: &str = ">";
const EMPTY_HINT: &str = "focus a claude pane to inspect it";
const MENU_SESSION: &str = "| disable: this session   |";
const MENU_PROFILE: &str = "| disable: profile-global |";
const MENU_CANCEL: &str = "| cancel                  |";

/// An open right-click disable menu: the row key it targets and the absolute
/// screen row it is anchored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuState {
    pub key: String,
    pub anchor_y: u16,
}

/// One visible line of the body: a section header or a `(section, row)` pair.
/// Shared by the renderer and the guest's hover hit-testing so both agree on
/// line indices. Line `i` renders at `area.y + 2 + i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyLine {
    Section(usize),
    Row(usize, usize),
}

pub fn body_lines(model: &HarnessRenderModel) -> Vec<BodyLine> {
    let mut lines = Vec::new();
    for (si, section) in model.sections.iter().enumerate() {
        lines.push(BodyLine::Section(si));
        for ri in 0..section.rows.len() {
            lines.push(BodyLine::Row(si, ri));
        }
    }
    lines
}

fn row_at<'m>(model: &'m HarnessRenderModel, line: BodyLine) -> Option<&'m HarnessRow> {
    match line {
        BodyLine::Row(si, ri) => model.sections.get(si).and_then(|s| s.rows.get(ri)),
        BodyLine::Section(_) => None,
    }
}

/// Render the panel into `buf`. Returns the frame's clickable hit rects.
/// `hovered` is the index (into [`body_lines`]) of the line whose tooltip should
/// be drawn beneath it; suppressed while `menu` is open.
pub fn render(
    model: &HarnessRenderModel,
    area: Rect,
    buf: &mut Buffer,
    hovered: Option<usize>,
    menu: Option<&MenuState>,
    scroll: usize,
) -> Vec<UiHitRect> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    if area.width <= 3 {
        return render_handle(area, buf);
    }
    render_full(model, area, buf, hovered, menu, scroll)
}

/// Slim collapsed handle: a single ">" glyph near the top, clickable to expand.
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

fn line_rect(area: Rect, y: u16) -> Rect {
    Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    }
}

fn render_full(
    model: &HarnessRenderModel,
    area: Rect,
    buf: &mut Buffer,
    hovered: Option<usize>,
    menu: Option<&MenuState>,
    scroll: usize,
) -> Vec<UiHitRect> {
    let mut hits = Vec::new();

    // While the disable menu is open the panel is modal: suppress every row
    // hit-rect so the menu's own rects are the only body hits. The host resolves
    // overlapping declared hits first-wins (see `ui_plugin_hit_at` in src/ui.rs),
    // and the menu lines overlay the rows beneath the anchor; without this a
    // menu click lands on the underlying `harness:row:<key>` rect (host ignores
    // it) and no-ops instead of disabling. Header toggle + menu hits still emit.
    let menu_open = menu.is_some();

    // Row 0: title left, collapse button far right.
    let btn_w = COLLAPSE_BTN.width() as u16;
    let title_w = PANEL_TITLE.width() as u16;
    let mut header = vec![Span::styled(
        PANEL_TITLE,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if area.width > title_w + btn_w + 1 {
        header.push(Span::raw(
            " ".repeat((area.width - title_w - btn_w) as usize),
        ));
    } else {
        header.push(Span::raw(" "));
    }
    header.push(Span::styled(
        COLLAPSE_BTN,
        Style::default().fg(Color::DarkGray),
    ));
    Paragraph::new(Line::from(header)).render(line_rect(area, area.y), buf);
    hits.push(UiHitRect {
        x: area.x + area.width.saturating_sub(btn_w),
        y: area.y,
        width: btn_w,
        height: 1,
        action_id: ACTION_TOGGLE_PANEL.to_string(),
    });

    if area.height <= 1 {
        return hits;
    }

    // Row 1: target identity (or the focus hint when there is no target yet).
    let target = if model.target.is_empty() {
        EMPTY_HINT
    } else {
        model.target.as_str()
    };
    Paragraph::new(Line::from(Span::styled(
        truncate_to(target, area.width as usize),
        Style::default().fg(Color::DarkGray),
    )))
    .render(line_rect(area, area.y + 1), buf);

    // Body lines from y+2, starting at `scroll` (clamped defensively to match
    // the guest's clamp so hover/menu line-index math stays consistent).
    let lines = body_lines(model);
    let max_lines = area.height.saturating_sub(2) as usize;
    let scroll = scroll.min(lines.len().saturating_sub(max_lines));
    for (i, line) in lines.iter().skip(scroll).take(max_lines).enumerate() {
        let y = area.y + 2 + i as u16;
        match *line {
            BodyLine::Section(si) => {
                if let Some(section) = model.sections.get(si) {
                    render_section_header(section, area, y, buf);
                }
            }
            BodyLine::Row(si, ri) => {
                if let Some(row) = model
                    .sections
                    .get(si)
                    .and_then(|s| s.rows.get(ri))
                {
                    render_row(row, area, y, buf, &mut hits, menu_open);
                }
            }
        }
    }

    // "▼ more" indicator on the last body line when lines are clipped below.
    if max_lines > 0 && scroll + max_lines < lines.len() {
        render_more_indicator(area, area.y + 2 + (max_lines as u16 - 1), buf);
    }

    // Right-click disable menu overlays everything (tooltip suppressed).
    if let Some(menu) = menu {
        render_menu(menu, area, buf, &mut hits);
        return hits;
    }

    // Hover tooltip beneath the hovered row. `hovered` is an absolute
    // body-line index; draw only when it lies inside the visible window.
    if let Some(idx) = hovered {
        if idx >= scroll && idx < (scroll + max_lines).min(lines.len()) {
            if let Some(row) = row_at(model, lines[idx]) {
                if !row.tooltip.trim().is_empty() {
                    let start_y = area.y + 3 + (idx - scroll) as u16;
                    render_tooltip(row, area, start_y, buf);
                }
            }
        }
    }

    hits
}

/// Draw a right-aligned "▼ more" hint on `y` when body lines are clipped below.
fn render_more_indicator(area: Rect, y: u16, buf: &mut Buffer) {
    const MORE: &str = "▼ more";
    let w = MORE.width() as u16;
    if area.width < w {
        return;
    }
    let rect = Rect {
        x: area.x + area.width - w,
        y,
        width: w,
        height: 1,
    };
    Paragraph::new(Line::from(Span::styled(
        MORE,
        Style::default().fg(Color::Cyan).bg(Color::Black),
    )))
    .render(rect, buf);
}

fn render_section_header(
    section: &harness_manager_model::HarnessSection,
    area: Rect,
    y: u16,
    buf: &mut Buffer,
) {
    let title_w = section.title.width() as u16;
    let sum_w = section.summary.width() as u16;
    let mut spans = vec![Span::styled(
        section.title.clone(),
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )];
    if !section.summary.is_empty() && area.width > title_w + sum_w + 1 {
        spans.push(Span::raw(
            " ".repeat((area.width - title_w - sum_w) as usize),
        ));
        spans.push(Span::styled(
            section.summary.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Paragraph::new(Line::from(spans)).render(line_rect(area, y), buf);
}

fn state_dot(state: RowState) -> Span<'static> {
    match state {
        RowState::Idle => Span::styled("\u{25CF} ", Style::default().fg(Color::Green)),
        RowState::Active => Span::styled("\u{25CF} ", Style::default().fg(Color::Red)),
        RowState::Off => Span::styled("\u{25CB} ", Style::default().fg(Color::DarkGray)),
        RowState::None => Span::raw("  "),
    }
}

fn render_row(
    row: &HarnessRow,
    area: Rect,
    y: u16,
    buf: &mut Buffer,
    hits: &mut Vec<UiHitRect>,
    suppress_hits: bool,
) {
    let width = area.width as usize;
    let indent = (row.indent as usize).min(4);
    let meta_w = row.meta.width();
    let dot_w = 2usize;
    let label_budget = width
        .saturating_sub(meta_w + dot_w + indent + 1)
        .max(4);
    let label = truncate_to(&row.label, label_budget);
    let label_w = label.width();

    let mut spans: Vec<Span> = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(" ".repeat(indent)));
    }
    spans.push(state_dot(row.state));
    spans.push(Span::styled(
        label,
        Style::default().fg(if row.state == RowState::Off {
            Color::DarkGray
        } else {
            Color::White
        }),
    ));
    let used = indent + dot_w + label_w + meta_w;
    let pad = width.saturating_sub(used).max(1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(
        row.meta.clone(),
        Style::default().fg(Color::DarkGray),
    ));
    Paragraph::new(Line::from(spans)).render(line_rect(area, y), buf);

    // One hit-rect spanning the FULL row width (label + meta half): left-click
    // opens (host-resolved) when openable; right-click opens the guest disable
    // menu when can_disable. Full-width so clicks on the right meta half also hit.
    if !suppress_hits && (row.openable || row.can_disable) {
        let action_id = if row.openable {
            format!("{ACTION_OPEN_PREFIX}{}", row.key)
        } else {
            format!("{ACTION_ROW_PREFIX}{}", row.key)
        };
        hits.push(UiHitRect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
            action_id,
        });
    }
}

fn render_menu(menu: &MenuState, area: Rect, buf: &mut Buffer, hits: &mut Vec<UiHitRect>) {
    let bottom = area.y + area.height; // exclusive
    // Anchor directly under the clicked row, clamped so all 3 lines fit.
    let start_y = menu
        .anchor_y
        .saturating_add(1)
        .min(bottom.saturating_sub(3).max(area.y));
    let entries: [(&str, String); 3] = [
        (
            MENU_SESSION,
            format!("{ACTION_DISABLE_SESSION_PREFIX}{}", menu.key),
        ),
        (
            MENU_PROFILE,
            format!("{ACTION_DISABLE_PROFILE_PREFIX}{}", menu.key),
        ),
        (MENU_CANCEL, ACTION_MENU_CANCEL.to_string()),
    ];
    for (i, (text, action_id)) in entries.iter().enumerate() {
        let y = start_y + i as u16;
        if y >= bottom {
            break;
        }
        Paragraph::new(Line::from(Span::styled(
            truncate_to(text, area.width as usize),
            Style::default().fg(Color::Yellow).bg(Color::Black),
        )))
        .render(line_rect(area, y), buf);
        hits.push(UiHitRect {
            x: area.x,
            y,
            width: (text.width() as u16).min(area.width),
            height: 1,
            action_id: action_id.clone(),
        });
    }
}

/// Draw the hover tooltip lines starting at `start_y`, wrapping to the panel
/// width and stopping at the panel bottom.
fn render_tooltip(row: &HarnessRow, area: Rect, start_y: u16, buf: &mut Buffer) {
    let bottom = area.y + area.height; // exclusive
    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for part in row.tooltip.lines() {
        push_wrapped(&mut lines, part, width);
    }
    for (i, line) in lines.into_iter().enumerate() {
        let y = start_y + i as u16;
        if y >= bottom {
            break;
        }
        Paragraph::new(line).render(line_rect(area, y), buf);
    }
}

/// Push `text` wrapped to `width` columns as muted tooltip lines.
fn push_wrapped(lines: &mut Vec<Line<'static>>, text: &str, width: usize) {
    if text.trim().is_empty() {
        return;
    }
    let cols = width.max(4);
    let chars: Vec<char> = text.chars().collect();
    for chunk in chars.chunks(cols) {
        let s: String = chunk.iter().collect();
        lines.push(Line::from(Span::styled(
            s,
            Style::default().fg(Color::DarkGray).bg(Color::Black),
        )));
    }
}

/// Truncate `s` to at most `budget` display columns (hard cut, width-safe).
fn truncate_to(s: &str, budget: usize) -> String {
    if s.width() <= budget {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_manager_model::HarnessSection;

    fn model() -> HarnessRenderModel {
        HarnessRenderModel {
            target: "claude · 0ccc55a3 · herdr".to_string(),
            sections: vec![
                HarnessSection {
                    title: "CONTEXT".to_string(),
                    summary: "3 files".to_string(),
                    rows: vec![HarnessRow {
                        key: "ctx:0".to_string(),
                        label: "~/.claude/CLAUDE.md".to_string(),
                        meta: "6.1KB".to_string(),
                        state: RowState::None,
                        tooltip: "click to open".to_string(),
                        openable: true,
                        can_disable: false,
                        indent: 0,
                    }],
                },
                HarnessSection {
                    title: "HOOKS".to_string(),
                    summary: String::new(),
                    rows: vec![HarnessRow {
                        key: "hook:0".to_string(),
                        label: "PreToolUse pkg-guard".to_string(),
                        meta: "x4 32ms".to_string(),
                        state: RowState::Idle,
                        tooltip: "guards installs".to_string(),
                        openable: false,
                        can_disable: true,
                        indent: 0,
                    }],
                },
            ],
        }
    }

    #[test]
    fn declares_open_and_row_hits() {
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let hits = render(&model(), area, &mut buf, None, None, 0);
        let ids: Vec<&str> = hits.iter().map(|h| h.action_id.as_str()).collect();
        assert!(ids.contains(&"harness:toggle_panel"));
        assert!(ids.iter().any(|id| *id == "harness:open:ctx:0"));
        assert!(ids.iter().any(|id| *id == "harness:row:hook:0"));
    }

    #[test]
    fn row_hit_rect_spans_full_width() {
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let hits = render(&model(), area, &mut buf, None, None, 0);
        let open = hits
            .iter()
            .find(|h| h.action_id == "harness:open:ctx:0")
            .expect("open hit present");
        assert_eq!(open.x, area.x);
        assert_eq!(open.width, area.width);
    }

    #[test]
    fn scroll_offset_shifts_visible_body_lines() {
        // A short panel that can only show a couple of body lines: with scroll=0
        // the CONTEXT header is visible; scrolled past it the HOOKS row appears.
        let area = Rect::new(0, 0, 44, 4); // header + target + 2 body rows
        let mut buf = Buffer::empty(area);
        let hits = render(&model(), area, &mut buf, None, None, 0);
        assert!(hits.iter().any(|h| h.action_id == "harness:open:ctx:0"));
        // Scroll down two lines: ctx row scrolls off, hook row becomes visible.
        let mut buf2 = Buffer::empty(area);
        let hits2 = render(&model(), area, &mut buf2, None, None, 2);
        assert!(hits2.iter().any(|h| h.action_id == "harness:row:hook:0"));
        assert!(!hits2.iter().any(|h| h.action_id == "harness:open:ctx:0"));
    }

    #[test]
    fn menu_declares_disable_hits() {
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let menu = MenuState {
            key: "hook:0".to_string(),
            anchor_y: 4,
        };
        let hits = render(&model(), area, &mut buf, None, Some(&menu), 0);
        let ids: Vec<&str> = hits.iter().map(|h| h.action_id.as_str()).collect();
        assert!(ids.contains(&"harness:disable_session:hook:0"));
        assert!(ids.contains(&"harness:disable_profile:hook:0"));
        assert!(ids.contains(&"harness:menu_cancel"));
        // Modal while open: no underlying row/open hits may leak, or a first-wins
        // host would resolve a menu click to the row beneath it and no-op.
        assert!(
            !ids.iter().any(|id| id.starts_with(ACTION_OPEN_PREFIX)),
            "open hits must be suppressed while menu open: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.starts_with(ACTION_ROW_PREFIX)),
            "row hits must be suppressed while menu open: {ids:?}"
        );
        // Only the header toggle + the 3 menu entries remain.
        assert_eq!(hits.len(), 4, "{ids:?}");
    }

    #[test]
    fn menu_hit_rects_match_drawn_lines() {
        // The declared menu hit-rects must sit exactly where render_menu drew the
        // three lines: at start_y = anchor_y + 1, one per line, left-aligned.
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let anchor_y = 4u16;
        let menu = MenuState {
            key: "hook:0".to_string(),
            anchor_y,
        };
        let hits = render(&model(), area, &mut buf, None, Some(&menu), 0);
        let start_y = anchor_y + 1;
        for (i, id) in [
            "harness:disable_session:hook:0",
            "harness:disable_profile:hook:0",
            "harness:menu_cancel",
        ]
        .iter()
        .enumerate()
        {
            let hit = hits
                .iter()
                .find(|h| h.action_id == *id)
                .unwrap_or_else(|| panic!("missing {id}"));
            assert_eq!(hit.x, area.x, "{id} x");
            assert_eq!(hit.y, start_y + i as u16, "{id} y");
            assert_eq!(hit.height, 1, "{id} height");
            // The menu strings are all the same fixed width; ensure the rect
            // covers the drawn glyphs and stays within the panel.
            assert!(hit.width > 0 && hit.width <= area.width, "{id} width");
        }
    }

    #[test]
    fn collapsed_handle_declares_toggle() {
        let area = Rect::new(0, 0, 3, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&HarnessRenderModel::default(), area, &mut buf, None, None, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action_id, "harness:toggle_panel");
    }

    #[test]
    fn body_lines_flatten_sections_and_rows() {
        let lines = body_lines(&model());
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], BodyLine::Section(0));
        assert_eq!(lines[1], BodyLine::Row(0, 0));
    }
}
