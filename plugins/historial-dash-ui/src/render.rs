//! Model-based renderer for the historial (pane-close history) right-panel
//! plugin (v3). Pure: no clock/fs/env access — every string (including the
//! relative time) is precomputed host-side in [`HistorialRenderModel`]. Mirrors
//! the goal-dash renderer shape: render into an off-screen ratatui buffer, and
//! return the frame's clickable [`UiHitRect`]s in absolute screen coordinates.
//!
//! Interaction model: the restore/fork/delete hit-rects are RESOLVED by the host
//! (it owns the pane-close log and the resume machinery), so the guest only
//! declares them — it never handles those actions itself. The one action the
//! guest owns is the header collapse toggle, matching goal-dash.

use historial_dash_model::HistorialRenderModel;
use herdr_ui_plugin_api::UiHitRect;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

/// Header collapse button / collapsed-handle expand affordance. Guest-owned:
/// `lib.rs` flips guest collapse state and emits `SetSurfaceCollapsed` on a
/// `left_down` of this id. NOT host-intercepted (unlike the row actions).
pub const ACTION_TOGGLE_PANEL: &str = "historial:toggle_panel";

/// Prefixes for host-resolved row actions. The suffix after the final `:` in the
/// restore/fork/delete ids is the row's stable key (`"<ts>:<pane_id>"`), so the
/// host parses it back to the source record. These are declared here but handled
/// entirely host-side.
pub const ACTION_RESTORE_PREFIX: &str = "historial:restore:";
pub const ACTION_FORK_PREFIX: &str = "historial:fork:";
pub const ACTION_DEL_PREFIX: &str = "historial:del:";

const PANEL_TITLE: &str = "HISTORIAL";
const COLLAPSE_BTN: &str = "[<]";
const EXPAND_HANDLE: &str = ">";
const EMPTY_HINT: &str = "no closed panes yet";
const FORK_BTN: &str = "[f]";
const DEL_BTN: &str = "[x]";

/// Render the historial list into `buf`. Returns the frame's clickable hit
/// rects (collapse button, and per restorable row: restore / fork / delete).
pub fn render(model: &HistorialRenderModel, area: Rect, buf: &mut Buffer) -> Vec<UiHitRect> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    if area.width <= 3 {
        return render_handle(area, buf);
    }
    render_full(model, area, buf)
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

fn render_full(model: &HistorialRenderModel, area: Rect, buf: &mut Buffer) -> Vec<UiHitRect> {
    let width = area.width as usize;
    let mut hits = Vec::new();

    // Row 0: title left, "[<]" collapse button far right.
    let btn_w = COLLAPSE_BTN.width() as u16;
    let mut header = vec![Span::styled(
        PANEL_TITLE,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    let title_w = PANEL_TITLE.width() as u16;
    if area.width > title_w + btn_w + 1 {
        let pad = area.width - title_w - btn_w;
        header.push(Span::raw(" ".repeat(pad as usize)));
    } else {
        header.push(Span::raw(" "));
    }
    header.push(Span::styled(COLLAPSE_BTN, Style::default().fg(Color::DarkGray)));
    Paragraph::new(Line::from(header)).render(
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
        buf,
    );
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

    if model.rows.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            EMPTY_HINT,
            Style::default().fg(Color::DarkGray),
        )))
        .render(
            Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            },
            buf,
        );
        return hits;
    }

    // One line per closed pane, newest first, within the available rows.
    let max_rows = (area.height - 1) as usize;
    for (i, row) in model.rows.iter().take(max_rows).enumerate() {
        let y = area.y + 1 + i as u16;
        render_row(row, area.x, y, width, buf, &mut hits);
    }

    hits
}

fn render_row(
    row: &historial_dash_model::HistorialRow,
    x: u16,
    y: u16,
    width: usize,
    buf: &mut Buffer,
    hits: &mut Vec<UiHitRect>,
) {
    // Right side: fork + delete buttons for restorable rows.
    let actions_w = if row.restorable {
        (FORK_BTN.width() + DEL_BTN.width()) as u16
    } else {
        0
    };
    let reltime_w = row.reltime.width() as u16;
    // Left text budget = total - reltime - actions - separators.
    let reserved = reltime_w + actions_w + 2;
    let text_budget = (width as u16).saturating_sub(reserved).max(4) as usize;

    let label = truncate_to(&format!("{} {}", row.agent, row.label), text_budget);
    let mut spans: Vec<Span> = Vec::new();
    let restore_w = label.width() as u16;
    spans.push(Span::styled(
        label,
        Style::default().fg(if row.restorable {
            Color::White
        } else {
            Color::DarkGray
        }),
    ));

    // Pad to push reltime + actions to the right edge.
    let used = restore_w + reltime_w + actions_w;
    let total = width as u16;
    let pad = total.saturating_sub(used).max(1);
    spans.push(Span::raw(" ".repeat(pad as usize)));
    spans.push(Span::styled(
        row.reltime.clone(),
        Style::default().fg(Color::DarkGray),
    ));
    if row.restorable {
        spans.push(Span::styled(FORK_BTN, Style::default().fg(Color::Yellow)));
        spans.push(Span::styled(DEL_BTN, Style::default().fg(Color::Red)));
    }

    Paragraph::new(Line::from(spans)).render(
        Rect {
            x,
            y,
            width: total,
            height: 1,
        },
        buf,
    );

    // Restore hit: the left label region.
    if row.restorable && restore_w > 0 {
        hits.push(UiHitRect {
            x,
            y,
            width: restore_w,
            height: 1,
            action_id: format!("{ACTION_RESTORE_PREFIX}{}", row.key),
        });
        // Fork + delete hits at the right edge.
        let del_w = DEL_BTN.width() as u16;
        let fork_w = FORK_BTN.width() as u16;
        let del_x = x + total.saturating_sub(del_w);
        let fork_x = del_x.saturating_sub(fork_w);
        hits.push(UiHitRect {
            x: fork_x,
            y,
            width: fork_w,
            height: 1,
            action_id: format!("{ACTION_FORK_PREFIX}{}", row.key),
        });
        hits.push(UiHitRect {
            x: del_x,
            y,
            width: del_w,
            height: 1,
            action_id: format!("{ACTION_DEL_PREFIX}{}", row.key),
        });
    }
}

/// Truncate `s` to at most `budget` display columns, appending nothing (hard
/// cut). ASCII-safe and multibyte-safe via unicode width.
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
    use historial_dash_model::HistorialRow;

    fn row(key: &str, restorable: bool) -> HistorialRow {
        HistorialRow {
            key: key.to_string(),
            agent: "claude".to_string(),
            label: "herdr".to_string(),
            reltime: "5m".to_string(),
            cwd: "/home/dev/herdr".to_string(),
            restorable,
        }
    }

    #[test]
    fn restorable_row_declares_restore_fork_delete_hits() {
        let model = HistorialRenderModel {
            rows: vec![row("100:w_1_p_1", true)],
        };
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&model, area, &mut buf);
        let ids: Vec<&str> = hits.iter().map(|h| h.action_id.as_str()).collect();
        assert!(ids.contains(&"historial:toggle_panel"));
        assert!(ids.iter().any(|id| *id == "historial:restore:100:w_1_p_1"));
        assert!(ids.iter().any(|id| *id == "historial:fork:100:w_1_p_1"));
        assert!(ids.iter().any(|id| *id == "historial:del:100:w_1_p_1"));
    }

    #[test]
    fn non_restorable_row_has_no_row_actions() {
        let model = HistorialRenderModel {
            rows: vec![row("100:w_1_p_1", false)],
        };
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&model, area, &mut buf);
        assert!(hits.iter().all(|h| !h.action_id.starts_with("historial:restore:")));
        assert!(hits.iter().all(|h| !h.action_id.starts_with("historial:fork:")));
        assert!(hits.iter().all(|h| !h.action_id.starts_with("historial:del:")));
    }

    #[test]
    fn empty_model_renders_header_only() {
        let model = HistorialRenderModel::default();
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&model, area, &mut buf);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action_id, "historial:toggle_panel");
    }

    #[test]
    fn collapsed_handle_declares_toggle() {
        let area = Rect::new(0, 0, 3, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&HistorialRenderModel::default(), area, &mut buf);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action_id, "historial:toggle_panel");
    }
}
