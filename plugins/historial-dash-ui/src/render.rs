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
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

/// Accent color for an agent kind. Shared by the row identity and the tooltip
/// agent value so the same agent always reads as the same color (color =
/// meaning): a glance down the list distinguishes claude/codex/pi/jcode rows
/// even when several are unnamed and therefore share the agent name as their
/// title.
fn agent_color(agent: &str) -> Color {
    match agent {
        "claude" => Color::Magenta,
        "codex" => Color::Green,
        "pi" => Color::Yellow,
        "jcode" => Color::Blue,
        "terminal" => Color::DarkGray,
        _ => Color::White,
    }
}

/// Header collapse button / collapsed-handle expand affordance. Guest-owned:
/// `lib.rs` flips guest collapse state and emits `SetSurfaceCollapsed` on a
/// `left_down` of this id. NOT host-intercepted (unlike the row actions).
pub const ACTION_TOGGLE_PANEL: &str = "historial:toggle_panel";

/// Show/hide-terminals toggle in the header. Guest-owned: `lib.rs` flips a
/// static flag and re-renders on a `left_down` of this id. Not host-intercepted.
pub const ACTION_TOGGLE_TERMINALS: &str = "historial:toggle_terminals";

/// Per-agent filter cycle button in the header. Guest-owned: each `left_down`
/// advances the filter all -> claude -> codex -> pi -> jcode -> all.
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
    Codex,
    Pi,
    Jcode,
}

impl AgentFilter {
    /// Map a monotonically-incremented click counter to a filter, so `lib.rs` can
    /// persist the selection as a single atomic usize.
    pub fn from_index(i: usize) -> Self {
        match i % 5 {
            0 => AgentFilter::All,
            1 => AgentFilter::Claude,
            2 => AgentFilter::Codex,
            3 => AgentFilter::Pi,
            _ => AgentFilter::Jcode,
        }
    }

    /// Fixed-width (3-col) header glyph for this filter so the layout is stable.
    fn glyph(self) -> &'static str {
        match self {
            AgentFilter::All => "[*]",
            AgentFilter::Claude => "[c]",
            AgentFilter::Codex => "[o]",
            AgentFilter::Pi => "[p]",
            AgentFilter::Jcode => "[j]",
        }
    }

    /// Human name for this filter, used by the legend overlay.
    fn label(self) -> &'static str {
        match self {
            AgentFilter::All => "all",
            AgentFilter::Claude => "claude",
            AgentFilter::Codex => "codex",
            AgentFilter::Pi => "pi",
            AgentFilter::Jcode => "jcode",
        }
    }

    /// Whether a row with display agent `agent` passes this filter. `All` passes
    /// everything (including terminals); a specific kind matches only that agent.
    fn matches(self, agent: &str) -> bool {
        match self {
            AgentFilter::All => true,
            AgentFilter::Claude => agent == "claude",
            AgentFilter::Codex => agent == "codex",
            AgentFilter::Pi => agent == "pi",
            AgentFilter::Jcode => agent == "jcode",
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
                format!(
                    "[*] filter: {}  (all>claude>codex>pi>jcode)",
                    filters.agent.label()
                )
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

/// The single identity string a row shows, in precedence order:
///   1. the manual pane label (`label`) — rare, most specific (e.g. "labs");
///   2. the pane's TAB display name (`tab_name`) — herdr always assigns one at
///      tab creation (a user rename, "new"/"test", or a number), so this is the
///      name the user scans by and is effectively never empty for a live pane;
///   3. the agent name (`claude`/`codex`/…) — a last-resort safety net only for
///      legacy records written before `tab_name` was captured.
/// We deliberately never substitute the first user prompt (noisy) nor the
/// cwd/folder name (rejected). All other metadata lives in the hover tooltip.
fn row_identifier(row: &historial_dash_model::HistorialRow) -> &str {
    let label = row.label.trim();
    if !label.is_empty() {
        return label;
    }
    let tab = row.tab_name.trim();
    if !tab.is_empty() {
        return tab;
    }
    row.agent.trim()
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
        "    all/claude/codex/pi/jcode".to_string(),
        "[t] show terminal panes".to_string(),
        "[<] collapse panel".to_string(),
        "[?] toggle this help".to_string(),
        "row: pane name (color=agent)".to_string(),
        "  hover = full details".to_string(),
        "  click name = resume".to_string(),
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

/// Dim fixed-width gutter for tooltip field labels ("agent", "cwd", …) so values
/// line up in a column.
const TIP_LABEL_W: usize = 7;

/// One `label   value` tooltip line: dim gutter label, then the type-colored
/// value (color = field meaning).
fn tip_field(label: &str, value: String, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<TIP_LABEL_W$}"),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(value, value_style),
    ])
}

/// Push a possibly-long value under `label`, wrapping to the tooltip's inner
/// width: the first visual line carries the dim label gutter, continuations are
/// indented to align under the value. Capped at `max_lines` (an over-long prompt
/// must not fill the panel); the last kept line ends in `…` when text remains.
/// No-op when `text` is empty.
fn tip_wrapped(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    text: &str,
    value_style: Style,
    inner_w: usize,
    max_lines: usize,
) {
    let text = text.trim();
    if text.is_empty() || max_lines == 0 {
        return;
    }
    let value_w = inner_w.saturating_sub(TIP_LABEL_W).max(4);
    let chars: Vec<char> = text.chars().collect();
    let dim = Style::default().fg(Color::DarkGray);
    for (i, chunk) in chars.chunks(value_w).enumerate() {
        if i >= max_lines {
            break;
        }
        let mut value: Vec<char> = chunk.to_vec();
        // On the last allowed line with more text remaining, mark the cut.
        if i + 1 == max_lines && (i + 1) * value_w < chars.len() {
            if let Some(last) = value.last_mut() {
                *last = '…';
            }
        }
        let gutter = if i == 0 {
            format!("{label:<TIP_LABEL_W$}")
        } else {
            " ".repeat(TIP_LABEL_W)
        };
        lines.push(Line::from(vec![
            Span::styled(gutter, dim),
            Span::styled(value.into_iter().collect::<String>(), value_style),
        ]));
    }
}

/// Draw the rich hover tooltip: a rounded, dim-bordered box (identity on the
/// border) holding the row's full metadata, grouped and color-coded — session
/// (agent / restorability / id), location (workspace / tab / cwd), time
/// (relative + absolute), and the first/last user prompts. Drawn just beneath
/// the hovered row (`start_y`); if it would overflow the panel bottom it shifts
/// up so it stays fully on-screen. All content lives here, not on the row.
fn render_tooltip(row: &historial_dash_model::HistorialRow, area: Rect, start_y: u16, buf: &mut Buffer) {
    let bottom = area.y + area.height; // exclusive
    if area.width < 6 || area.height < 4 {
        return; // too small for a boxed tooltip
    }
    let inner_w = (area.width as usize).saturating_sub(2); // minus L/R border

    let mut lines: Vec<Line<'static>> = Vec::new();

    // --- session: agent, restorability, full id ---
    lines.push(tip_field(
        "agent",
        row.agent.clone(),
        Style::default().fg(agent_color(&row.agent)),
    ));
    let (status_text, status_color) = if row.restorable {
        ("resumable", Color::Green)
    } else {
        ("no resume", Color::DarkGray)
    };
    lines.push(tip_field(
        "status",
        status_text.to_string(),
        Style::default().fg(status_color),
    ));
    tip_wrapped(&mut lines, "id", &row.sid, Style::default().fg(Color::Gray), inner_w, 1);

    // --- location: workspace, tab, cwd (blank-line separated group) ---
    let mut loc: Vec<Line<'static>> = Vec::new();
    if !row.workspace_name.trim().is_empty() {
        loc.push(tip_field(
            "ws",
            row.workspace_name.clone(),
            Style::default().fg(Color::Cyan),
        ));
    }
    if !row.tab_name.trim().is_empty() {
        loc.push(tip_field("tab", row.tab_name.clone(), Style::default().fg(Color::Cyan)));
    }
    tip_wrapped(&mut loc, "cwd", &row.cwd, Style::default().fg(Color::Blue), inner_w, 2);
    if !loc.is_empty() {
        lines.push(Line::from(""));
        lines.extend(loc);
    }

    // --- time: relative (accent) then absolute (dim) on one "seen" line ---
    if !row.reltime.trim().is_empty() || !row.abstime.trim().is_empty() {
        lines.push(Line::from(""));
        let mut spans = vec![Span::styled(
            format!("{:<TIP_LABEL_W$}", "seen"),
            Style::default().fg(Color::DarkGray),
        )];
        if !row.reltime.trim().is_empty() {
            spans.push(Span::styled(
                row.reltime.clone(),
                Style::default().fg(Color::Yellow),
            ));
        }
        if !row.abstime.trim().is_empty() {
            if !row.reltime.trim().is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                row.abstime.clone(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }

    // --- prompts: first (bright) + last (dim), each wrapped and capped ---
    let mut prompts: Vec<Line<'static>> = Vec::new();
    tip_wrapped(
        &mut prompts,
        "first",
        &row.first_prompt,
        Style::default().fg(Color::White),
        inner_w,
        3,
    );
    tip_wrapped(
        &mut prompts,
        "last",
        &row.last_prompt,
        Style::default().fg(Color::DarkGray),
        inner_w,
        3,
    );
    if !prompts.is_empty() {
        lines.push(Line::from(""));
        lines.extend(prompts);
    }

    if lines.is_empty() {
        return;
    }

    // Box = content + 2 borders, clamped to the panel (keep the header row). If
    // it would overflow the bottom, shift up so it stays fully on-screen.
    let content_h = lines.len() as u16;
    let max_h = area.height.saturating_sub(1);
    let box_h = (content_h + 2).min(max_h).max(3);
    let mut top = start_y;
    if top + box_h > bottom {
        top = bottom.saturating_sub(box_h);
    }
    if top < area.y + 1 {
        top = area.y + 1;
    }
    let tip_rect = Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: box_h,
    };

    let ident = row_identifier(row).to_string();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {ident} "),
            Style::default()
                .fg(agent_color(&row.agent))
                .add_modifier(Modifier::BOLD),
        ));
    Clear.render(tip_rect, buf);
    let inner = block.inner(tip_rect);
    block.render(tip_rect, buf);

    for (i, line) in lines.into_iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        Paragraph::new(line).render(
            Rect {
                x: inner.x,
                y: inner.y + i as u16,
                width: inner.width,
                height: 1,
            },
            buf,
        );
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

    // Row = just the pane's herdr identity (name, or agent name when unnamed),
    // colored by agent so kinds are distinguishable at a glance. Everything else
    // (session id, cwd, time, prompts, workspace/tab) is in the hover tooltip.
    let label = truncate_to(row_identifier(row), text_budget);
    let mut spans: Vec<Span> = Vec::new();
    let restore_w = label.width() as u16;
    // Restorable rows read in their agent color and bold (the actionable ones);
    // non-restorable rows (dead agent refs, plain terminals) read dim.
    let style = if row.restorable {
        Style::default()
            .fg(agent_color(&row.agent))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    spans.push(Span::styled(label, style));

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
            sid: "sess-c123".to_string(),
            reltime: "5m".to_string(),
            abstime: "2026-07-24 15:57".to_string(),
            cwd: "/home/dev/herdr".to_string(),
            restorable,
            workspace_name: "myws".to_string(),
            tab_name: "main".to_string(),
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

    /// The cycle covers every supported agent runtime and returns to `all`, so
    /// no kind of pane can be filtered out with no way to get it back.
    #[test]
    fn the_agent_filter_cycles_through_all_supported_runtimes() {
        let cycle: Vec<AgentFilter> = (0..6).map(AgentFilter::from_index).collect();
        assert_eq!(
            cycle,
            vec![
                AgentFilter::All,
                AgentFilter::Claude,
                AgentFilter::Codex,
                AgentFilter::Pi,
                AgentFilter::Jcode,
                AgentFilter::All,
            ]
        );
        // Every glyph is the same width, so the header layout cannot shift as
        // the selection advances.
        for f in &cycle {
            assert_eq!(f.glyph().chars().count(), HEADER_BTN_W as usize);
        }
    }

    #[test]
    fn per_agent_filter_selects_matching_rows() {
        let claude = row("1:w_1_p_1", true);
        let jcode = {
            let mut r = row("2:w_1_p_2", true);
            r.agent = "jcode".to_string();
            r
        };
        let model = HistorialRenderModel {
            rows: vec![claude, jcode],
        };
        let f = RowFilters {
            agent: AgentFilter::Jcode,
            ..RowFilters::default()
        };
        let visible = visible_rows(&model, f);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].agent, "jcode");

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
    fn unlabeled_row_shows_tab_name_never_first_prompt() {
        // A row with no manual pane label falls back to its TAB display name (the
        // always-present identifier the user scans by) — NOT the first user
        // prompt, and NOT the session id (both are tooltip-only).
        let mut r = row("1:w_1_p_1", true);
        r.label = String::new(); // no manual pane label
        r.tab_name = "bitwarden".to_string(); // herdr's tab display name
        r.first_prompt = "fix the historial panel please now urgently".to_string();
        let model = HistorialRenderModel { rows: vec![r] };
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        // No hover: only the row is drawn (no tooltip).
        let _ = render(&model, area, &mut buf, None, RowFilters::default(), None);
        let text = buffer_text(&buf);
        assert!(text.contains("bitwarden")); // tab name is the row identity
        assert!(!text.contains("fix the historial")); // never the first prompt
        assert!(!text.contains("c123")); // sid is tooltip-only
    }

    #[test]
    fn legacy_row_without_tab_falls_back_to_agent() {
        // A legacy record predating tab_name capture (no manual label, no tab
        // name) shows the agent kind as a last resort — still never a prompt.
        let mut r = row("1:w_1_p_1", true);
        r.label = String::new();
        r.tab_name = String::new();
        let model = HistorialRenderModel { rows: vec![r] };
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, None, RowFilters::default(), None);
        assert!(buffer_text(&buf).contains("claude"));
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
        // A restorable agent row with no pane label, no tab name, and no first
        // prompt shows its agent name only — never the cwd basename ("herdr").
        // (No hover, so the tooltip — which does show cwd — is not drawn.)
        let mut r = row("1:w_1_p_1", true);
        r.label = String::new();
        r.tab_name = String::new();
        r.first_prompt = String::new();
        r.cwd = "/home/dev/herdr".to_string();
        let model = HistorialRenderModel { rows: vec![r] };
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, None, RowFilters::default(), None);
        let text = buffer_text(&buf);
        assert!(text.contains("claude")); // agent name is the row identity
        assert!(!text.contains("c123")); // sid is tooltip-only
        assert!(!text.contains("herdr")); // folder never leaks onto the row
    }

    fn terminal_row(key: &str) -> HistorialRow {
        HistorialRow {
            key: key.to_string(),
            agent: "terminal".to_string(),
            reltime: "1h".to_string(),
            cwd: "/home/dev/scratch".to_string(),
            restorable: false,
            ..Default::default()
        }
    }

    #[test]
    fn terminals_hidden_by_default_and_revealed_by_toggle() {
        let mut agent_row = row("1:w_1_p_1", true);
        agent_row.label = "myproj".to_string(); // a real, distinct pane name
        let model = HistorialRenderModel {
            rows: vec![agent_row, terminal_row("2:w_1_p_2")],
        };
        let area = Rect::new(0, 0, 44, 12);

        // Hidden: the terminal row is not drawn; the agent row is (by its pane
        // name). Neither folder basename ("scratch") appears on any row.
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, None, RowFilters::default(), None);
        let hidden = buffer_text(&buf);
        assert!(hidden.contains("myproj"));
        assert!(!hidden.contains("terminal"));
        assert!(!hidden.contains("scratch"));

        // Shown: the unnamed terminal row appears by its agent-kind identity
        // ("terminal"), still never the folder name.
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

    #[test]
    fn labeled_row_shows_pane_name_not_metadata() {
        // A named, restorable row shows its pane name as the identity — not the
        // agent text, sid, or prompt (all of which are tooltip-only).
        let r = row("1:w_1_p_1", true); // label "herdr", sid "sess-c123"
        let model = HistorialRenderModel { rows: vec![r] };
        let area = Rect::new(0, 0, 50, 12);
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, None, RowFilters::default(), None);
        let text = buffer_text(&buf);
        assert!(text.contains("herdr")); // the pane name
        assert!(!text.contains("sess-c123")); // full sid is tooltip-only
        assert!(!text.contains("first prompt")); // prompt never on the row
    }

    #[test]
    fn hover_tooltip_shows_full_metadata_in_a_box() {
        // Hovering a row draws the boxed tooltip with every metadata field:
        // agent, restorability, full session id, workspace, tab, cwd, absolute
        // time, and both prompts. Uses a tall panel so nothing is clamped away.
        let model = HistorialRenderModel {
            rows: vec![row("1:w_1_p_1", true)],
        };
        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, Some(0), RowFilters::default(), None);
        let text = buffer_text(&buf);
        assert!(text.contains("resumable")); // restorability status
        assert!(text.contains("sess-c123")); // FULL session id (not just sid4)
        assert!(text.contains("myws")); // workspace
        assert!(text.contains("main")); // tab
        assert!(text.contains("/home/dev/herdr")); // cwd
        assert!(text.contains("2026-07-24 15:57")); // absolute time
        assert!(text.contains("first prompt text"));
        assert!(text.contains("last prompt text"));
        // The box is drawn (rounded corner glyph present).
        assert!(text.contains('╮') || text.contains('╭'));
    }

    #[test]
    fn tooltip_omits_absent_optional_fields() {
        // A terminal row with no session/workspace/tab/prompts must not render
        // empty "id"/"ws"/"tab" gutters — those lines are skipped entirely.
        let model = HistorialRenderModel {
            rows: vec![terminal_row("1:w_1_p_1")],
        };
        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        let _ = render(
            &model,
            area,
            &mut buf,
            Some(0),
            RowFilters { show_terminals: true, ..RowFilters::default() },
            None,
        );
        let text = buffer_text(&buf);
        assert!(text.contains("no resume")); // status still shown
        assert!(text.contains("/home/dev/scratch")); // cwd still shown
        assert!(!text.contains("ws ")); // no empty workspace gutter
        assert!(!text.contains("first ")); // no empty prompt gutter
    }

    /// Visual-proof helper (no-op in normal runs): when `HISTORIAL_SNAPSHOT_FILE`
    /// points at a real `historial_input.json`, render the panel exactly as the
    /// guest would — first restorable agent row hovered — and print the buffer.
    /// Lets us capture the true rendered panel + tooltip from live data without a
    /// running herdr, driving the same `render()` path the wasm guest uses.
    #[test]
    fn snapshot_real_projection_when_env_set() {
        let Ok(path) = std::env::var("HISTORIAL_SNAPSHOT_FILE") else {
            return;
        };
        let json = std::fs::read_to_string(&path).expect("read snapshot json");
        let model: HistorialRenderModel = serde_json::from_str(&json).expect("parse model");
        let filters = RowFilters::default(); // terminals hidden, like the live default
        let visible = visible_rows(&model, filters);
        let hover = visible
            .iter()
            .position(|r| r.restorable && r.agent != "terminal");
        let area = Rect::new(0, 0, 46, 44);
        let mut buf = Buffer::empty(area);
        let _ = render(&model, area, &mut buf, hover, filters, None);
        eprintln!(
            "\n===== HISTORIAL PANEL (real data, {} visible rows, hover idx {:?}) =====",
            visible.len(),
            hover
        );
        eprintln!("{}", buffer_text(&buf));
    }
}
