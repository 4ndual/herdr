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

/// Show/hide-terminals toggle in the header. Guest-owned: `lib.rs` flips a
/// static flag and re-renders on a `left_down` of this id. Not host-intercepted.
pub const ACTION_TOGGLE_TERMINALS: &str = "historial:toggle_terminals";

/// Per-agent filter cycle button in the header. Guest-owned: each `left_down`
/// advances the filter all -> claude -> cx -> codex -> pi -> all.
pub const ACTION_CYCLE_AGENT: &str = "historial:cycle_agent";

/// Legend/help toggle in the header (`[?]`). Guest-owned: each `left_down` flips
/// an overlay that explains what every header/row glyph does, replacing the list
/// while shown.
pub const ACTION_TOGGLE_LEGEND: &str = "historial:toggle_legend";

/// Per-agent-kind filter selected in the header (item: per-agent filter).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum AgentFilter {
    #[default]
    All,
    Claude,
    Cx,
    Codex,
    Pi,
}

impl AgentFilter {
    /// Map a monotonically-incremented click counter to a filter, so `lib.rs` can
    /// persist the selection as a single atomic usize.
    pub fn from_index(i: usize) -> Self {
        match i % 5 {
            0 => AgentFilter::All,
            1 => AgentFilter::Claude,
            2 => AgentFilter::Cx,
            3 => AgentFilter::Codex,
            _ => AgentFilter::Pi,
        }
    }

    /// Fixed-width (3-col) header glyph for this filter so the layout is stable.
    fn glyph(self) -> &'static str {
        match self {
            AgentFilter::All => "[*]",
            AgentFilter::Claude => "[c]",
            AgentFilter::Cx => "[x]",
            AgentFilter::Codex => "[o]",
            AgentFilter::Pi => "[p]",
        }
    }

    /// Human name for this filter, used by the legend overlay.
    fn label(self) -> &'static str {
        match self {
            AgentFilter::All => "all",
            AgentFilter::Claude => "claude",
            AgentFilter::Cx => "cx",
            AgentFilter::Codex => "codex",
            AgentFilter::Pi => "pi",
        }
    }

    /// Whether a row with display agent `agent` passes this filter. `All` passes
    /// everything (including terminals); a specific kind matches only that agent.
    fn matches(self, agent: &str) -> bool {
        match self {
            AgentFilter::All => true,
            AgentFilter::Claude => agent == "claude",
            AgentFilter::Cx => agent == "cx",
            AgentFilter::Codex => agent == "codex",
            AgentFilter::Pi => agent == "pi",
        }
    }
}

/// The composed header state: terminals toggle, the per-agent filter, and the
/// legend-overlay toggle. Shared by the renderer and the guest's hover
/// hit-testing so both agree on which rows are visible. (Subagent rows are
/// excluded at the host source, so there is no subagent toggle here.)
#[derive(Clone, Copy, Default)]
pub struct RowFilters {
    pub show_terminals: bool,
    pub agent: AgentFilter,
    pub show_legend: bool,
}

impl RowFilters {
    fn keep(&self, row: &historial_dash_model::HistorialRow) -> bool {
        if !self.show_terminals && row.is_terminal() {
            return false;
        }
        self.agent.matches(&row.agent)
    }
}

/// A clickable header button, used both to describe it on hover and to resolve
/// which button an absolute mouse coordinate landed on (shared geometry with the
/// rendered hit-rects so hover and click always agree).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderButton {
    Agent,
    Terminals,
    Legend,
    Collapse,
}

impl HeaderButton {
    /// One-line human description shown as a hover hint at the panel bottom, so
    /// the otherwise-cryptic `[*][t][?][<]` glyphs are self-explanatory.
    pub fn describe(self, filters: RowFilters) -> String {
        match self {
            HeaderButton::Agent => {
                format!("[*] filter: {}  (all>claude>cx>codex>pi)", filters.agent.label())
            }
            HeaderButton::Terminals => {
                if filters.show_terminals {
                    "[t] terminals shown  (click to hide)".to_string()
                } else {
                    "[t] terminals hidden  (click to show)".to_string()
                }
            }
            HeaderButton::Legend => "[?] toggle the help legend".to_string(),
            HeaderButton::Collapse => "[<] collapse this panel".to_string(),
        }
    }

    /// Accent color for this button, applied even when inactive so each glyph
    /// reads as a distinct control rather than gray text.
    fn color(self, filters: RowFilters) -> Color {
        match self {
            HeaderButton::Agent => {
                if filters.agent == AgentFilter::All { Color::Magenta } else { Color::LightMagenta }
            }
            HeaderButton::Terminals => {
                if filters.show_terminals { Color::Green } else { Color::LightGreen }
            }
            HeaderButton::Legend => Color::Yellow,
            HeaderButton::Collapse => Color::Cyan,
        }
    }
}

/// Fixed glyph width shared by every header button (all glyphs are 3 cols).
const HEADER_BTN_W: u16 = 3;

/// Resolve which header button (if any) the absolute coordinate `(col, row)` is
/// over. Mirrors the right-to-left hit-rect layout in [`render_full`] exactly so
/// hover and click agree. Returns `None` off the header row or between buttons.
pub fn header_button_at(area: Rect, col: u16, row: u16) -> Option<HeaderButton> {
    if area.width <= 3 || area.height == 0 || row != area.y {
        return None;
    }
    let collapse_x = area.x + area.width.saturating_sub(HEADER_BTN_W);
    let legend_x = collapse_x.saturating_sub(1 + HEADER_BTN_W);
    let term_x = legend_x.saturating_sub(HEADER_BTN_W);
    let agent_x = term_x.saturating_sub(HEADER_BTN_W);
    let hit = |x: u16| col >= x && col < x + HEADER_BTN_W;
    if hit(collapse_x) {
        Some(HeaderButton::Collapse)
    } else if hit(legend_x) {
        Some(HeaderButton::Legend)
    } else if hit(term_x) {
        Some(HeaderButton::Terminals)
    } else if hit(agent_x) {
        Some(HeaderButton::Agent)
    } else {
        None
    }
}

/// Draw the header hover hint at row `y` (full width), in the hovered button's
/// accent color. Callers place it near the header/content so it stays close to
/// the button being hovered (the right panel can be full-height, so a bottom-line
/// hint would be far away). No-op if `y` is outside the panel.
fn draw_header_hint(area: Rect, buf: &mut Buffer, y: u16, text: &str, color: Color) {
    if area.height < 2 || y < area.y || y >= area.y + area.height {
        return;
    }
    Paragraph::new(Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )))
    .render(Rect { x: area.x, y, width: area.width, height: 1 }, buf);
}

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
const EMPTY_AGENTS_HINT: &str = "no agent panes (t: show terminals)";
const FORK_BTN: &str = "[f]";
const DEL_BTN: &str = "[x]";
/// Terminals-shown / terminals-hidden toggle glyphs (fixed width so the header
/// layout doesn't shift when the state flips).
const TERM_TOGGLE_ON: &str = "[T]";
const TERM_TOGGLE_OFF: &str = "[t]";
/// Legend/help toggle glyph (fixed width). Highlighted while the overlay is open.
const LEGEND_BTN: &str = "[?]";

/// The rows to actually render, honoring the show-terminals toggle: when off
/// (default), plain terminal/shell rows are filtered out. Shared by the renderer
/// and the guest's hover hit-testing so both agree on row indices.
pub fn visible_rows(
    model: &HistorialRenderModel,
    filters: RowFilters,
) -> Vec<&historial_dash_model::HistorialRow> {
    model.rows.iter().filter(|row| filters.keep(row)).collect()
}

/// Render the historial list into `buf`. Returns the frame's clickable hit
/// rects (collapse button, terminals toggle, and per restorable row: restore /
/// fork / delete). `hovered` is the index (into the *visible* rows) of the row
/// whose tooltip should be drawn beneath it. `show_terminals` gates whether
/// plain terminal rows are listed.
pub fn render(
    model: &HistorialRenderModel,
    area: Rect,
    buf: &mut Buffer,
    hovered: Option<usize>,
    filters: RowFilters,
    header_hover: Option<HeaderButton>,
) -> Vec<UiHitRect> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    if area.width <= 3 {
        return render_handle(area, buf);
    }
    render_full(model, area, buf, hovered, filters, header_hover)
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

fn render_full(
    model: &HistorialRenderModel,
    area: Rect,
    buf: &mut Buffer,
    hovered: Option<usize>,
    filters: RowFilters,
    header_hover: Option<HeaderButton>,
) -> Vec<UiHitRect> {
    let width = area.width as usize;
    let mut hits = Vec::new();

    // Row 0: title left; the cluster "[agent][t][?] [<]" pinned far right. Every
    // glyph is fixed 3 cols so the header never shifts as state flips.
    let btn_w = COLLAPSE_BTN.width() as u16; // [<]
    let agent_glyph = filters.agent.glyph();
    let agent_w = agent_glyph.width() as u16;
    let term_label = if filters.show_terminals {
        TERM_TOGGLE_ON
    } else {
        TERM_TOGGLE_OFF
    };
    let term_w = term_label.width() as u16;
    let legend_w = LEGEND_BTN.width() as u16;

    let mut header = vec![Span::styled(
        PANEL_TITLE,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    let title_w = PANEL_TITLE.width() as u16;
    // agent + terminals + legend + space + collapse.
    let right_w = agent_w + term_w + legend_w + 1 + btn_w;
    if area.width > title_w + right_w + 1 {
        let pad = area.width - title_w - right_w;
        header.push(Span::raw(" ".repeat(pad as usize)));
    } else {
        header.push(Span::raw(" "));
    }
    // Each button keeps its accent color even when inactive so the glyphs read
    // as distinct controls; the active state adds BOLD.
    let btn_style = |btn: HeaderButton, active: bool| {
        let s = Style::default().fg(btn.color(filters));
        if active { s.add_modifier(Modifier::BOLD) } else { s }
    };
    header.push(Span::styled(
        agent_glyph,
        btn_style(HeaderButton::Agent, filters.agent != AgentFilter::All),
    ));
    header.push(Span::styled(
        term_label,
        btn_style(HeaderButton::Terminals, filters.show_terminals),
    ));
    header.push(Span::styled(
        LEGEND_BTN,
        btn_style(HeaderButton::Legend, filters.show_legend),
    ));
    header.push(Span::raw(" "));
    header.push(Span::styled(
        COLLAPSE_BTN,
        btn_style(HeaderButton::Collapse, false),
    ));
    Paragraph::new(Line::from(header)).render(
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
        buf,
    );
    // Hit-rects, laid out right-to-left to match the rendered cluster.
    let collapse_x = area.x + area.width.saturating_sub(btn_w);
    hits.push(UiHitRect {
        x: collapse_x,
        y: area.y,
        width: btn_w,
        height: 1,
        action_id: ACTION_TOGGLE_PANEL.to_string(),
    });
    let legend_x = collapse_x.saturating_sub(1 + legend_w);
    hits.push(UiHitRect {
        x: legend_x,
        y: area.y,
        width: legend_w,
        height: 1,
        action_id: ACTION_TOGGLE_LEGEND.to_string(),
    });
    let term_x = legend_x.saturating_sub(term_w);
    hits.push(UiHitRect {
        x: term_x,
        y: area.y,
        width: term_w,
        height: 1,
        action_id: ACTION_TOGGLE_TERMINALS.to_string(),
    });
    let agent_x = term_x.saturating_sub(agent_w);
    hits.push(UiHitRect {
        x: agent_x,
        y: area.y,
        width: agent_w,
        height: 1,
        action_id: ACTION_CYCLE_AGENT.to_string(),
    });

    if area.height <= 1 {
        return hits;
    }

    // Hover hint for a header button, drawn on the bottom line. Reserve that line
    // (below) so it never collides with a row.
    let header_hint = header_hover.map(|b| (b.describe(filters), b.color(filters)));

    // Legend overlay: replace the list with a dense, dim help panel while open.
    if filters.show_legend {
        render_legend(model, area, buf, filters.agent);
        if let Some((text, color)) = &header_hint {
            draw_header_hint(area, buf, area.y + area.height - 1, text, *color);
        }
        return hits;
    }

    let rows = visible_rows(model, filters);
    if rows.is_empty() {
        // Distinguish "log is empty" from "all rows are terminals but hidden".
        let hint = if model.rows.is_empty() {
            EMPTY_HINT
        } else {
            EMPTY_AGENTS_HINT
        };
        Paragraph::new(Line::from(Span::styled(
            hint,
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
        if let Some((text, color)) = &header_hint {
            draw_header_hint(area, buf, area.y + 2, text, *color);
        }
        return hits;
    }

    // One line per visible closed pane, newest first, within the available rows.
    // Reserve one line for the header hover hint (drawn just under the list) so it
    // never collides with a row.
    let mut max_rows = (area.height - 1) as usize;
    if header_hint.is_some() && max_rows > 1 {
        max_rows -= 1;
    }
    let visible = rows.len().min(max_rows);
    for (i, row) in rows.iter().take(max_rows).enumerate() {
        let y = area.y + 1 + i as u16;
        render_row(row, area.x, y, width, buf, &mut hits);
    }

    // Hover tooltip: draw workspace name + first/last prompt directly beneath the
    // hovered row, clipped to the panel and overwriting the rows below it.
    if let Some(idx) = hovered {
        if idx < visible {
            let start_y = area.y + 2 + idx as u16;
            render_tooltip(rows[idx], area, start_y, buf);
        }
    }

    // Place the hint on the line just under the row list, keeping it close to the
    // header buttons rather than at a far-away panel bottom.
    if let Some((text, color)) = &header_hint {
        draw_header_hint(area, buf, area.y + 1 + visible as u16, text, *color);
    }

    hits
}

/// The display label for a row and whether it is a *fallback* (rendered dim).
/// The label is the herdr pane name only (`label` = the pane's OSC/manual title
/// as it shows on the live pane border) — this is how the user recognizes a
/// conversation, so it is the sole source. We deliberately do NOT fall back to
/// the first user prompt (noisy, not how panes are named) nor to the cwd/folder
/// name (the project-folder encoding the user explicitly rejected). A row with
/// no pane name shows only its agent + sid, which is honest rather than
/// misleading. The bool is retained for the dim styling contract but is always
/// `false` now that there is no fallback text.
fn row_label(row: &historial_dash_model::HistorialRow) -> (String, bool) {
    let label = row.label.trim();
    if !label.is_empty() {
        return (label.to_string(), false);
    }
    (String::new(), false)
}

/// Render the legend overlay (shown while `[?]` is toggled on), describing the
/// current header/row glyph semantics. Dense and dim; drawn from `area.y + 1`
/// down, clipped to the panel bottom.
fn render_legend(
    model: &HistorialRenderModel,
    area: Rect,
    buf: &mut Buffer,
    agent: AgentFilter,
) {
    let _ = model;
    let dim = Style::default().fg(Color::DarkGray);
    let lines: Vec<String> = vec![
        format!("[*] agent filter (now: {})", agent.label()),
        "    all/claude/cx/codex/pi".to_string(),
        "[t] show terminal panes".to_string(),
        "[<] collapse panel".to_string(),
        "[?] toggle this help".to_string(),
        "row: agent  label  sid".to_string(),
        "  click label = resume".to_string(),
        "  [f] fork into new tab".to_string(),
        "  [x] delete record".to_string(),
    ];
    let bottom = area.y + area.height; // exclusive
    for (i, text) in lines.into_iter().enumerate() {
        let y = area.y + 1 + i as u16;
        if y >= bottom {
            break;
        }
        Paragraph::new(Line::from(Span::styled(text, dim))).render(
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
            buf,
        );
    }
}

/// Draw the hover tooltip (workspace name, first prompt, last prompt) starting at
/// `start_y`, wrapping each field to the panel width and stopping at the panel
/// bottom.
fn render_tooltip(row: &historial_dash_model::HistorialRow, area: Rect, start_y: u16, buf: &mut Buffer) {
    let bottom = area.y + area.height; // exclusive
    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    if !row.workspace_name.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("@ {}", row.workspace_name),
            Style::default().fg(Color::Cyan),
        )));
    }
    push_wrapped(&mut lines, "1st: ", &row.first_prompt, width);
    push_wrapped(&mut lines, "last: ", &row.last_prompt, width);
    if lines.is_empty() {
        return;
    }

    for (i, line) in lines.into_iter().enumerate() {
        let y = start_y + i as u16;
        if y >= bottom {
            break;
        }
        Paragraph::new(line).render(
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
            buf,
        );
    }
}

/// Push `prefix + text` wrapped to `width` columns as muted tooltip lines.
/// No-op when `text` is empty.
fn push_wrapped(lines: &mut Vec<Line<'static>>, prefix: &str, text: &str, width: usize) {
    if text.trim().is_empty() {
        return;
    }
    let full = format!("{prefix}{text}");
    let cols = width.max(4);
    let chars: Vec<char> = full.chars().collect();
    for chunk in chars.chunks(cols) {
        let s: String = chunk.iter().collect();
        lines.push(Line::from(Span::styled(
            s,
            Style::default().fg(Color::DarkGray),
        )));
    }
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

    // Row text: "<agent> <pane-label> <sid4?>". The pane label is the primary
    // display; when the pane had no title/manual label we fall back (dimmed) to
    // the first prompt words. Never the folder/cwd — a nameless, promptless row
    // shows just its agent + sid. No cwd column.
    let (label_text, is_fallback) = row_label(row);
    let mut parts: Vec<&str> = vec![row.agent.as_str()];
    if !label_text.is_empty() {
        parts.push(label_text.as_str());
    }
    if !row.sid4.is_empty() {
        parts.push(row.sid4.as_str());
    }
    let label = truncate_to(&parts.join(" "), text_budget);
    let mut spans: Vec<Span> = Vec::new();
    let restore_w = label.width() as u16;
    // Real label on a restorable row = bright; fallback text or non-restorable
    // rows read dim.
    let fg = if row.restorable && !is_fallback {
        Color::White
    } else {
        Color::DarkGray
    };
    spans.push(Span::styled(label, Style::default().fg(fg)));

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
            sid4: "c123".to_string(),
            reltime: "5m".to_string(),
            cwd: "/home/dev/herdr".to_string(),
            restorable,
            workspace_name: "myws".to_string(),
            first_prompt: "first prompt text".to_string(),
            last_prompt: "last prompt text".to_string(),
            is_subagent: false,
        }
    }

    fn buffer_text(buf: &Buffer) -> String {
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn restorable_row_declares_restore_fork_delete_hits() {
        let model = HistorialRenderModel {
            rows: vec![row("100:w_1_p_1", true)],
        };
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&model, area, &mut buf, None, RowFilters { show_terminals: true, ..RowFilters::default() }, None);
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
        let hits = render(&model, area, &mut buf, None, RowFilters { show_terminals: true, ..RowFilters::default() }, None);
        assert!(hits.iter().all(|h| !h.action_id.starts_with("historial:restore:")));
        assert!(hits.iter().all(|h| !h.action_id.starts_with("historial:fork:")));
        assert!(hits.iter().all(|h| !h.action_id.starts_with("historial:del:")));
    }

    #[test]
    fn empty_model_renders_header_toggles_only() {
        let model = HistorialRenderModel::default();
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&model, area, &mut buf, None, RowFilters { show_terminals: true, ..RowFilters::default() }, None);
        let ids: Vec<&str> = hits.iter().map(|h| h.action_id.as_str()).collect();
        assert_eq!(ids.len(), 4);
        assert!(ids.contains(&"historial:toggle_panel"));
        assert!(ids.contains(&"historial:toggle_terminals"));
        assert!(ids.contains(&"historial:toggle_legend"));
        assert!(ids.contains(&"historial:cycle_agent"));
    }

    #[test]
    fn per_agent_filter_selects_matching_rows() {
        let claude = row("1:w_1_p_1", true);
        let cx = {
            let mut r = row("2:w_1_p_2", true);
            r.agent = "cx".to_string();
            r
        };
        let model = HistorialRenderModel {
            rows: vec![claude, cx],
        };
        let f = RowFilters {
            agent: AgentFilter::Cx,
            ..RowFilters::default()
        };
        let visible = visible_rows(&model, f);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].agent, "cx");

        let f = RowFilters {
            agent: AgentFilter::Claude,
            ..RowFilters::default()
        };
        let visible = visible_rows(&model, f);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].agent, "claude");
    }

    #[test]
    fn legend_overlay_replaces_list_and_hides_rows() {
        let model = HistorialRenderModel {
            rows: vec![row("1:w_1_p_1", true)],
        };
        let area = Rect::new(0, 0, 44, 12);
        let mut buf = Buffer::empty(area);
        let f = RowFilters {
            show_legend: true,
            ..RowFilters::default()
        };
        let _ = render(&model, area, &mut buf, None, f, None);
        let text = buffer_text(&buf);
        // Legend content is shown; the row's label ("herdr") is not.
        assert!(text.contains("agent filter"));
        assert!(text.contains("[f] fork"));
        assert!(!text.contains("c123"));
    }

    #[test]
    fn unlabeled_row_never_shows_first_prompt() {
        // The pane name (label) is the only row title. A row with no pane name
        // must NOT fall back to the first user prompt — it shows agent + sid only,
        // matching how the user recognizes panes by their herdr name.
        let mut r = row("1:w_1_p_1", true);
        r.label = String::new();
        r.first_prompt = "fix the historial panel please now urgently".to_string();
        let model = HistorialRenderModel { rows: vec![r] };
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, None, RowFilters::default(), None);
        let text = buffer_text(&buf);
        assert!(!text.contains("fix the historial"));
        assert!(text.contains("claude")); // agent still shown
        assert!(text.contains("c123")); // sid still shown
    }

    #[test]
    fn header_button_at_resolves_each_button_right_to_left() {
        let area = Rect::new(0, 0, 44, 12);
        // Layout (right-to-left): collapse at width-3, then a space, then legend,
        // terminals, agent — each 3 cols.
        let collapse_x = area.width - 3;
        let legend_x = collapse_x - 1 - 3;
        let term_x = legend_x - 3;
        let agent_x = term_x - 3;
        assert_eq!(header_button_at(area, collapse_x, 0), Some(HeaderButton::Collapse));
        assert_eq!(header_button_at(area, legend_x, 0), Some(HeaderButton::Legend));
        assert_eq!(header_button_at(area, term_x, 0), Some(HeaderButton::Terminals));
        assert_eq!(header_button_at(area, agent_x, 0), Some(HeaderButton::Agent));
        // Off the header row -> None.
        assert_eq!(header_button_at(area, collapse_x, 1), None);
        // Left of the button cluster -> None.
        assert_eq!(header_button_at(area, 0, 0), None);
    }

    #[test]
    fn header_hover_draws_description_hint_on_bottom_line() {
        let model = HistorialRenderModel {
            rows: vec![row("1:w_1_p_1", true)],
        };
        let area = Rect::new(0, 0, 50, 12);
        let mut buf = Buffer::empty(area);
        let _ = render(
            &model,
            area,
            &mut buf,
            None,
            RowFilters::default(),
            Some(HeaderButton::Agent),
        );
        // The hint text appears somewhere in the panel (bottom line).
        assert!(buffer_text(&buf).contains("filter:"));
    }

    #[test]
    fn nameless_promptless_row_never_shows_folder() {
        // A restorable agent row with no pane label and no first prompt must fall
        // through to agent + sid only — never the cwd basename ("herdr").
        let mut r = row("1:w_1_p_1", true);
        r.label = String::new();
        r.first_prompt = String::new();
        r.cwd = "/home/dev/herdr".to_string();
        let model = HistorialRenderModel { rows: vec![r] };
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, None, RowFilters::default(), None);
        let text = buffer_text(&buf);
        assert!(text.contains("claude"));
        assert!(text.contains("c123")); // sid still shown
        assert!(!text.contains("herdr")); // folder never leaks in
    }

    fn terminal_row(key: &str) -> HistorialRow {
        HistorialRow {
            key: key.to_string(),
            agent: "terminal".to_string(),
            label: String::new(),
            sid4: String::new(),
            reltime: "1h".to_string(),
            cwd: "/home/dev/scratch".to_string(),
            restorable: false,
            workspace_name: String::new(),
            first_prompt: String::new(),
            last_prompt: String::new(),
            is_subagent: false,
        }
    }

    #[test]
    fn terminals_hidden_by_default_and_revealed_by_toggle() {
        let model = HistorialRenderModel {
            rows: vec![row("1:w_1_p_1", true), terminal_row("2:w_1_p_2")],
        };
        let area = Rect::new(0, 0, 44, 12);

        // Hidden: the terminal row is not drawn; the claude row is. The folder
        // name ("scratch") must never appear regardless of the toggle.
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, None, RowFilters::default(), None);
        let hidden = buffer_text(&buf);
        assert!(hidden.contains("claude"));
        assert!(!hidden.contains("terminal"));
        assert!(!hidden.contains("scratch"));

        // Shown: the terminal row appears (as its agent kind), still never the
        // folder name.
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, None, RowFilters { show_terminals: true, ..RowFilters::default() }, None);
        let shown = buffer_text(&buf);
        assert!(shown.contains("terminal"));
        assert!(!shown.contains("scratch"));
    }

    #[test]
    fn header_declares_terminals_toggle_hit() {
        let model = HistorialRenderModel {
            rows: vec![row("1:w_1_p_1", true)],
        };
        let area = Rect::new(0, 0, 44, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&model, area, &mut buf, None, RowFilters::default(), None);
        assert!(hits
            .iter()
            .any(|h| h.action_id == "historial:toggle_terminals"));
    }

    #[test]
    fn all_terminal_rows_hidden_shows_agents_hint() {
        let model = HistorialRenderModel {
            rows: vec![terminal_row("1:w_1_p_1"), terminal_row("2:w_1_p_2")],
        };
        let area = Rect::new(0, 0, 44, 12);
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, None, RowFilters::default(), None);
        assert!(buffer_text(&buf).contains("no agent panes"));
    }

    #[test]
    fn collapsed_handle_declares_toggle() {
        let area = Rect::new(0, 0, 3, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&HistorialRenderModel::default(), area, &mut buf, None, RowFilters { show_terminals: true, ..RowFilters::default() }, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action_id, "historial:toggle_panel");
    }
}
