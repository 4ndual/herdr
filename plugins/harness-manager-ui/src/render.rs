//! Model-based renderer for the harness-manager right-panel plugin (v3). Pure:
//! no clock/fs/env access — every string is precomputed host-side in
//! [`HarnessRenderModel`]. Mirrors the historial-dash renderer shape: render
//! into an off-screen ratatui buffer, and return the frame's clickable
//! [`UiHitRect`]s in absolute screen coordinates.
//!
//! Interaction model: the open / disable hit-rects are RESOLVED by the host (it
//! owns pane spawning and the Claude settings files), so the guest only declares
//! them. The guest owns the header collapse toggle, per-section collapse, the
//! hover tooltip, and the right-click disable menu (which then declares
//! host-resolved disable ids).
//!
//! Layout: sections render as a header line (title + collapse marker + a
//! right-aligned aggregate) followed, when expanded, by their rows grouped under
//! one dim group header per contiguous `group` run. Rows are dense: dot + scope
//! marker + label + host meta + compact token cost on a single line.

use harness_manager_model::{HarnessRenderModel, HarnessRow, HarnessSection, RowState};
use herdr_ui_plugin_api::UiHitRect;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

/// Header collapse button / collapsed-handle expand affordance. Guest-owned.
pub const ACTION_TOGGLE_PANEL: &str = "harness:toggle_panel";
/// Section header collapse/expand toggle (guest-owned; keyed by section title).
pub const ACTION_SECTION_PREFIX: &str = "harness:section:";
/// Left-click open (host-resolved: spawns an editor pane on the row's path).
pub const ACTION_OPEN_PREFIX: &str = "harness:open:";
/// Row without an open target but with a disable menu (guest right-click only).
pub const ACTION_ROW_PREFIX: &str = "harness:row:";
/// Disable-menu options (host-resolved config edits).
pub const ACTION_DISABLE_SESSION_PREFIX: &str = "harness:disable_session:";
pub const ACTION_DISABLE_PROFILE_PREFIX: &str = "harness:disable_profile:";
/// Copies the row's absolute path (host-resolved: the host owns the path, the
/// row model never carries one, so this mirrors `harness:open:` and resolves
/// through the same collector cache).
pub const ACTION_COPY_PREFIX: &str = "harness:copy:";
/// Closes the disable menu (guest-owned).
pub const ACTION_MENU_CANCEL: &str = "harness:menu_cancel";

const PANEL_TITLE: &str = "HARNESS";
const COLLAPSE_BTN: &str = "[<]";
const EXPAND_HANDLE: &str = ">";
const EMPTY_HINT: &str = "focus a claude pane to inspect it";
const MENU_SESSION: &str = "| disable: this session   |";
const MENU_PROFILE: &str = "| disable: user-global    |";
const MENU_COPY: &str = "| copy path               |";
const MENU_CANCEL: &str = "| cancel                  |";

/// An open right-click disable menu: the row key it targets and the absolute
/// screen row it is anchored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuState {
    pub key: String,
    pub anchor_y: u16,
    /// Whether the targeted row resolves to a file the host can open. Only such
    /// rows get the "copy path" entry — the guest never sees a path itself, so
    /// offering it on a pathless row would produce a silent no-op.
    pub can_copy: bool,
    /// Whether the targeted row offers the disable options. A row can be
    /// copyable without being disableable (and vice versa), so the menu is
    /// built from these two flags rather than assumed to be all three options.
    pub can_disable: bool,
}

/// One visible line of the body. Shared by the renderer and the guest's hover
/// hit-testing so both agree on line indices. Line `i` renders at
/// `area.y + 2 + i`. A `Group` line is a dim header for a contiguous run of rows
/// that share a `group`. Collapsed sections contribute only their `Section` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyLine {
    Section(usize),
    Group(usize, String),
    Row(usize, usize),
}

/// Whether the section titled `title` is collapsed in the current view.
fn is_collapsed(collapsed: &[String], title: &str) -> bool {
    collapsed.iter().any(|t| t == title)
}

/// Flatten sections and (expanded) rows into visible body lines. Collapsed
/// sections contribute only their header; expanded sections emit one dim group
/// header per contiguous `group` run, then the rows beneath it.
pub fn body_lines(model: &HarnessRenderModel, collapsed: &[String]) -> Vec<BodyLine> {
    let mut lines = Vec::new();
    for (si, section) in model.sections.iter().enumerate() {
        lines.push(BodyLine::Section(si));
        if is_collapsed(collapsed, &section.title) {
            continue;
        }
        let mut last_group: Option<&str> = None;
        for (ri, row) in section.rows.iter().enumerate() {
            match row.group.as_deref() {
                Some(g) => {
                    if last_group != Some(g) {
                        lines.push(BodyLine::Group(si, g.to_string()));
                        last_group = Some(g);
                    }
                }
                None => last_group = None,
            }
            lines.push(BodyLine::Row(si, ri));
        }
    }
    lines
}

fn row_at<'m>(model: &'m HarnessRenderModel, line: &BodyLine) -> Option<&'m HarnessRow> {
    match line {
        BodyLine::Row(si, ri) => model.sections.get(*si).and_then(|s| s.rows.get(*ri)),
        _ => None,
    }
}

/// Render the panel into `buf`. Returns the frame's clickable hit rects.
/// `hovered` is the index (into [`body_lines`]) of the line whose tooltip should
/// be drawn beneath it; suppressed while `menu` is open. `collapsed` is the set
/// of section titles currently collapsed.
pub fn render(
    model: &HarnessRenderModel,
    area: Rect,
    buf: &mut Buffer,
    hovered: Option<usize>,
    menu: Option<&MenuState>,
    scroll: usize,
    collapsed: &[String],
) -> Vec<UiHitRect> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    if area.width <= 3 {
        return render_handle(area, buf);
    }
    render_full(model, area, buf, hovered, menu, scroll, collapsed)
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

#[allow(clippy::too_many_arguments)]
fn render_full(
    model: &HarnessRenderModel,
    area: Rect,
    buf: &mut Buffer,
    hovered: Option<usize>,
    menu: Option<&MenuState>,
    scroll: usize,
    collapsed: &[String],
) -> Vec<UiHitRect> {
    let mut hits = Vec::new();

    // While the disable menu is open the panel is modal: suppress every body
    // hit-rect (rows AND section headers) so the menu's own rects are the only
    // body hits. The host resolves overlapping declared hits first-wins (see
    // `ui_plugin_hit_at` in src/ui.rs); without this a menu click lands on the
    // rect beneath it and no-ops. Header toggle + menu hits still emit.
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
    let lines = body_lines(model, collapsed);
    let max_lines = area.height.saturating_sub(2) as usize;
    let scroll = scroll.min(lines.len().saturating_sub(max_lines));
    for (i, line) in lines.iter().skip(scroll).take(max_lines).enumerate() {
        let y = area.y + 2 + i as u16;
        match line {
            BodyLine::Section(si) => {
                if let Some(section) = model.sections.get(*si) {
                    let sec_collapsed = is_collapsed(collapsed, &section.title);
                    render_section_header(section, area, y, buf, sec_collapsed, &mut hits, menu_open);
                }
            }
            BodyLine::Group(_, name) => render_group_header(name, area, y, buf),
            BodyLine::Row(si, ri) => {
                if let Some(row) = model.sections.get(*si).and_then(|s| s.rows.get(*ri)) {
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
            if let Some(row) = row_at(model, &lines[idx]) {
                if !tooltip_lines(row).is_empty() {
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

/// Render a section header: collapse marker + title on the left, the aggregate
/// (falling back to the plain summary) right-aligned. Declares a full-width
/// toggle hit-rect keyed by the section title.
fn render_section_header(
    section: &HarnessSection,
    area: Rect,
    y: u16,
    buf: &mut Buffer,
    collapsed: bool,
    hits: &mut Vec<UiHitRect>,
    suppress_hits: bool,
) {
    let marker = if collapsed { "▸ " } else { "▾ " };
    // Prefer the dense computed aggregate; fall back to the plain summary.
    let right = section
        .agg
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(section.summary.as_str());

    let left_w = (marker.width() + section.title.width()) as u16;
    let right_w = right.width() as u16;
    let mut spans = vec![
        Span::styled(marker, Style::default().fg(Color::DarkGray)),
        Span::styled(
            section.title.clone(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !right.is_empty() && area.width > left_w + right_w + 1 {
        spans.push(Span::raw(" ".repeat((area.width - left_w - right_w) as usize)));
        spans.push(Span::styled(
            truncate_to(right, area.width as usize),
            Style::default().fg(Color::Gray),
        ));
    }
    Paragraph::new(Line::from(spans)).render(line_rect(area, y), buf);

    if !suppress_hits {
        hits.push(UiHitRect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
            action_id: format!("{ACTION_SECTION_PREFIX}{}", section.title),
        });
    }
}

/// Render a dim group header line (e.g. "PreToolUse") nesting the rows beneath.
fn render_group_header(name: &str, area: Rect, y: u16, buf: &mut Buffer) {
    let text = format!(" {name}");
    Paragraph::new(Line::from(Span::styled(
        truncate_to(&text, area.width as usize),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )))
    .render(line_rect(area, y), buf);
}

fn state_dot(state: RowState) -> Span<'static> {
    match state {
        RowState::Idle => Span::styled("\u{25CF} ", Style::default().fg(Color::Green)),
        RowState::Active => Span::styled("\u{25CF} ", Style::default().fg(Color::Red)),
        RowState::Off => Span::styled("\u{25CB} ", Style::default().fg(Color::DarkGray)),
        RowState::None => Span::raw("  "),
    }
}

/// Compact token count: `45`, `2.7k`, `12k`.
fn fmt_tok(n: u32) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        let k = n as f64 / 1000.0;
        if k >= 10.0 {
            format!("{k:.0}k")
        } else {
            format!("{k:.1}k")
        }
    }
}

/// Dense token cost for a row: `45→2.7k tok` (loaded-now→if-invoked),
/// `45 tok` (loaded, no distinct full cost), `~2.7k tok` (on-demand only), or
/// `None` when both token fields are absent (never fabricate a zero — R9).
fn token_text(row: &HarnessRow) -> Option<String> {
    match (row.tok_loaded, row.tok_full) {
        (Some(l), Some(f)) if l != f => Some(format!("{}→{} tok", fmt_tok(l), fmt_tok(f))),
        (Some(l), _) => Some(format!("{} tok", fmt_tok(l))),
        (None, Some(f)) => Some(format!("~{} tok", fmt_tok(f))),
        (None, None) => None,
    }
}

/// Small scope marker span: `G` global (blue) / `S` session (cyan). `None` when
/// scope is absent or unrecognized (rendered defensively — R13).
fn scope_tag(scope: &Option<String>) -> Option<Span<'static>> {
    match scope.as_deref() {
        Some("global") => Some(Span::styled(
            "G",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )),
        Some("session") => Some(Span::styled(
            "S",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        _ => None,
    }
}

/// Label color driven by load timing (R11): loaded-at-start is green, on-demand
/// rows are dim until they have loaded this session (`loaded_at` present) and
/// cyan-highlighted once they have. Disabled rows stay dark gray regardless.
fn load_color(row: &HarnessRow) -> Color {
    if row.state == RowState::Off {
        return Color::DarkGray;
    }
    match row.load_when.as_deref() {
        Some("start") => Color::Green,
        Some("on-invoke") | Some("on-recall") | Some("on-spawn") | Some("deferred") => {
            if row.loaded_at.is_some() {
                Color::Cyan
            } else {
                Color::DarkGray
            }
        }
        Some("evicted") => Color::DarkGray,
        _ => Color::White,
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
    // Grouped rows nest one column deeper than the host-declared indent.
    let group_indent = if row.group.is_some() { 1 } else { 0 };
    let indent = ((row.indent as usize) + group_indent).min(6);
    let dot_w = 2usize;

    let scope = scope_tag(&row.scope);
    let scope_w = if scope.is_some() { 2 } else { 0 }; // marker + trailing space

    // Right-hand cluster: host meta then compact token cost.
    let tok = token_text(row);
    let mut right_w = row.meta.width();
    if let Some(t) = &tok {
        if right_w > 0 {
            right_w += 1;
        }
        right_w += t.width();
    }

    let label_budget = width
        .saturating_sub(right_w + dot_w + indent + scope_w + 1)
        .max(4);
    let label = truncate_to(&row.label, label_budget);
    let label_w = label.width();

    let mut spans: Vec<Span> = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(" ".repeat(indent)));
    }
    spans.push(state_dot(row.state));
    if let Some(s) = scope {
        spans.push(s);
        spans.push(Span::raw(" "));
    }
    let label_color = load_color(row);
    spans.push(Span::styled(label, Style::default().fg(label_color)));

    let used = indent + dot_w + scope_w + label_w + right_w;
    let pad = width.saturating_sub(used).max(1);
    spans.push(Span::raw(" ".repeat(pad)));
    if !row.meta.is_empty() {
        spans.push(Span::styled(
            row.meta.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(t) = tok {
        if !row.meta.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(t, Style::default().fg(label_color)));
    }
    Paragraph::new(Line::from(spans)).render(line_rect(area, y), buf);

    // One hit-rect spanning the FULL row width: left-click opens (host-resolved)
    // when openable; right-click opens the guest disable menu when can_disable.
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
    let mut entries: Vec<(&str, String)> = Vec::with_capacity(4);
    if menu.can_disable {
        entries.push((
            MENU_SESSION,
            format!("{ACTION_DISABLE_SESSION_PREFIX}{}", menu.key),
        ));
        entries.push((
            MENU_PROFILE,
            format!("{ACTION_DISABLE_PROFILE_PREFIX}{}", menu.key),
        ));
    }
    if menu.can_copy {
        entries.push((MENU_COPY, format!("{ACTION_COPY_PREFIX}{}", menu.key)));
    }
    entries.push((MENU_CANCEL, ACTION_MENU_CANCEL.to_string()));
    // Anchor directly under the clicked row, clamped so every line fits. The
    // entry count is variable (a row may be copyable, disableable, or both), so
    // clamp against the built list rather than a hardcoded height.
    let start_y = menu
        .anchor_y
        .saturating_add(1)
        .min(bottom.saturating_sub(entries.len() as u16).max(area.y));
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

/// The full tooltip content for a row: the host-provided tooltip text plus
/// provenance the model carries — token source, load timing (+ when it last
/// loaded), and config scope. Empty vec = no tooltip.
fn tooltip_lines(row: &HarnessRow) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in row.tooltip.lines() {
        if !part.trim().is_empty() {
            out.push(part.to_string());
        }
    }
    if let Some(src) = row.tok_src.as_deref().filter(|s| !s.is_empty()) {
        out.push(format!("tokens: {src}"));
    }
    if let Some(lw) = row.load_when.as_deref().filter(|s| !s.is_empty()) {
        match row.loaded_at.as_deref().filter(|s| !s.is_empty()) {
            Some(la) => out.push(format!("loads: {lw} (last @ {la})")),
            None => out.push(format!("loads: {lw}")),
        }
    }
    if let Some(sc) = row.scope.as_deref().filter(|s| !s.is_empty()) {
        out.push(format!("scope: {sc}"));
    }
    out
}

/// Draw the hover tooltip lines starting at `start_y`, wrapping to the panel
/// width and stopping at the panel bottom.
fn render_tooltip(row: &HarnessRow, area: Rect, start_y: u16, buf: &mut Buffer) {
    let bottom = area.y + area.height; // exclusive
    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for part in tooltip_lines(row) {
        push_wrapped(&mut lines, &part, width);
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

    /// The menu is built from what the row actually supports, so a copyable
    /// row and a disableable row get different option sets. Regression cover
    /// for "copy path" being offered on rows the host cannot resolve a path
    /// for, which would look like a dead click.
    #[test]
    fn menu_offers_only_the_options_the_row_supports() {
        fn ids(can_copy: bool, can_disable: bool) -> Vec<String> {
            let menu = MenuState {
                key: "ctx:0".to_string(),
                anchor_y: 3,
                can_copy,
                can_disable,
            };
            let area = Rect::new(0, 0, 40, 20);
            let mut buf = Buffer::empty(area);
            let mut hits = Vec::new();
            render_menu(&menu, area, &mut buf, &mut hits);
            hits.into_iter().map(|h| h.action_id).collect()
        }

        assert_eq!(
            ids(true, true),
            vec![
                "harness:disable_session:ctx:0",
                "harness:disable_profile:ctx:0",
                "harness:copy:ctx:0",
                ACTION_MENU_CANCEL,
            ]
        );
        assert_eq!(
            ids(true, false),
            vec!["harness:copy:ctx:0", ACTION_MENU_CANCEL]
        );
        assert_eq!(
            ids(false, true),
            vec![
                "harness:disable_session:ctx:0",
                "harness:disable_profile:ctx:0",
                ACTION_MENU_CANCEL,
            ]
        );
    }

    /// Every menu line must be the same width or the bordered box looks broken.
    #[test]
    fn menu_labels_share_one_width() {
        let w = MENU_SESSION.chars().count();
        for label in [MENU_PROFILE, MENU_COPY, MENU_CANCEL] {
            assert_eq!(label.chars().count(), w, "{label:?} is a different width");
        }
    }

    fn row(key: &str, label: &str, group: Option<&str>) -> HarnessRow {
        HarnessRow {
            key: key.to_string(),
            label: label.to_string(),
            group: group.map(|g| g.to_string()),
            ..Default::default()
        }
    }

    fn model() -> HarnessRenderModel {
        HarnessRenderModel {
            target: "claude · 0ccc55a3 · herdr".to_string(),
            sections: vec![
                HarnessSection {
                    title: "CONTEXT".to_string(),
                    summary: "3 files".to_string(),
                    agg: None,
                    collapsed_default: false,
                    rows: vec![HarnessRow {
                        openable: true,
                        tooltip: "click to open".to_string(),
                        meta: "6.1KB".to_string(),
                        tok_loaded: Some(1500),
                        ..row("ctx:0", "~/.claude/CLAUDE.md", None)
                    }],
                },
                HarnessSection {
                    title: "HOOKS".to_string(),
                    summary: String::new(),
                    agg: Some("x214 · 8.6s".to_string()),
                    collapsed_default: false,
                    rows: vec![
                        HarnessRow {
                            can_disable: true,
                            state: RowState::Idle,
                            tooltip: "guards installs".to_string(),
                            meta: "x4 32ms".to_string(),
                            tok_src: Some("SKILL.md 8.1KB".to_string()),
                            load_when: Some("on-invoke".to_string()),
                            scope: Some("global".to_string()),
                            ..row("hook:0", "pkg-guard", Some("PreToolUse"))
                        },
                        HarnessRow {
                            can_disable: true,
                            state: RowState::Idle,
                            ..row("hook:1", "time-gate", Some("PreToolUse"))
                        },
                    ],
                },
            ],
            generated_at: None,
        }
    }

    #[test]
    fn declares_open_and_row_hits() {
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let hits = render(&model(), area, &mut buf, None, None, 0, &[]);
        let ids: Vec<&str> = hits.iter().map(|h| h.action_id.as_str()).collect();
        assert!(ids.contains(&"harness:toggle_panel"));
        assert!(ids.iter().any(|id| *id == "harness:open:ctx:0"));
        assert!(ids.iter().any(|id| *id == "harness:row:hook:0"));
    }

    #[test]
    fn declares_section_header_hits() {
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let hits = render(&model(), area, &mut buf, None, None, 0, &[]);
        let ids: Vec<&str> = hits.iter().map(|h| h.action_id.as_str()).collect();
        assert!(ids.contains(&"harness:section:CONTEXT"));
        assert!(ids.contains(&"harness:section:HOOKS"));
    }

    #[test]
    fn row_hit_rect_spans_full_width() {
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let hits = render(&model(), area, &mut buf, None, None, 0, &[]);
        let open = hits
            .iter()
            .find(|h| h.action_id == "harness:open:ctx:0")
            .expect("open hit present");
        assert_eq!(open.x, area.x);
        assert_eq!(open.width, area.width);
    }

    #[test]
    fn collapsed_section_hides_rows_but_keeps_header() {
        let collapsed = vec!["HOOKS".to_string()];
        let lines = body_lines(&model(), &collapsed);
        // CONTEXT header + its row, then HOOKS header only (no group/rows).
        assert!(lines.contains(&BodyLine::Section(1)));
        assert!(!lines.iter().any(|l| matches!(l, BodyLine::Row(1, _))));
        assert!(!lines.iter().any(|l| matches!(l, BodyLine::Group(1, _))));
        // Expanded CONTEXT still shows its row.
        assert!(lines.iter().any(|l| matches!(l, BodyLine::Row(0, 0))));
    }

    #[test]
    fn collapsed_section_declares_no_row_hits() {
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let collapsed = vec!["HOOKS".to_string()];
        let hits = render(&model(), area, &mut buf, None, None, 0, &collapsed);
        let ids: Vec<&str> = hits.iter().map(|h| h.action_id.as_str()).collect();
        assert!(ids.contains(&"harness:section:HOOKS"));
        assert!(!ids.iter().any(|id| id.starts_with("harness:row:hook")));
    }

    #[test]
    fn contiguous_group_emits_one_header() {
        let lines = body_lines(&model(), &[]);
        let groups: Vec<_> = lines
            .iter()
            .filter(|l| matches!(l, BodyLine::Group(_, _)))
            .collect();
        // Both hook rows share "PreToolUse" contiguously → exactly one header.
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], &BodyLine::Group(1, "PreToolUse".to_string()));
    }

    #[test]
    fn token_text_formats_variants() {
        let mut r = row("k", "l", None);
        r.tok_loaded = Some(45);
        r.tok_full = Some(2700);
        assert_eq!(token_text(&r).as_deref(), Some("45→2.7k tok"));
        r.tok_full = None;
        assert_eq!(token_text(&r).as_deref(), Some("45 tok"));
        r.tok_loaded = None;
        r.tok_full = Some(2700);
        assert_eq!(token_text(&r).as_deref(), Some("~2.7k tok"));
        r.tok_full = None;
        assert_eq!(token_text(&r), None);
    }

    #[test]
    fn load_color_reflects_timing() {
        let mut r = row("k", "l", None);
        r.load_when = Some("start".to_string());
        assert_eq!(load_color(&r), Color::Green);
        r.load_when = Some("on-invoke".to_string());
        assert_eq!(load_color(&r), Color::DarkGray);
        r.loaded_at = Some("14:03".to_string());
        assert_eq!(load_color(&r), Color::Cyan);
        r.state = RowState::Off;
        assert_eq!(load_color(&r), Color::DarkGray);
    }

    #[test]
    fn tooltip_includes_provenance() {
        let r = &model().sections[1].rows[0];
        let tips = tooltip_lines(r);
        assert!(tips.iter().any(|t| t.contains("guards installs")));
        assert!(tips.iter().any(|t| t.contains("tokens: SKILL.md 8.1KB")));
        assert!(tips.iter().any(|t| t.contains("loads: on-invoke")));
        assert!(tips.iter().any(|t| t.contains("scope: global")));
    }

    #[test]
    fn scroll_offset_shifts_visible_body_lines() {
        let area = Rect::new(0, 0, 44, 4); // header + target + 2 body rows
        let mut buf = Buffer::empty(area);
        let hits = render(&model(), area, &mut buf, None, None, 0, &[]);
        assert!(hits.iter().any(|h| h.action_id == "harness:section:CONTEXT"));
        // Scroll down two lines: CONTEXT header + its row scroll off, the HOOKS
        // header (body line 2) becomes the first visible line.
        let mut buf2 = Buffer::empty(area);
        let hits2 = render(&model(), area, &mut buf2, None, None, 2, &[]);
        assert!(hits2.iter().any(|h| h.action_id == "harness:section:HOOKS"));
        assert!(!hits2.iter().any(|h| h.action_id == "harness:section:CONTEXT"));
    }

    #[test]
    fn menu_declares_disable_hits() {
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let menu = MenuState {
            key: "hook:0".to_string(),
            anchor_y: 4,
            can_copy: false,
            can_disable: true,
        };
        let hits = render(&model(), area, &mut buf, None, Some(&menu), 0, &[]);
        let ids: Vec<&str> = hits.iter().map(|h| h.action_id.as_str()).collect();
        assert!(ids.contains(&"harness:disable_session:hook:0"));
        assert!(ids.contains(&"harness:disable_profile:hook:0"));
        assert!(ids.contains(&"harness:menu_cancel"));
        // Modal while open: no underlying row/open/section hits may leak.
        assert!(
            !ids.iter().any(|id| id.starts_with(ACTION_OPEN_PREFIX)),
            "open hits must be suppressed while menu open: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.starts_with(ACTION_ROW_PREFIX)),
            "row hits must be suppressed while menu open: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.starts_with(ACTION_SECTION_PREFIX)),
            "section hits must be suppressed while menu open: {ids:?}"
        );
        // Only the header toggle + the 3 menu entries remain.
        assert_eq!(hits.len(), 4, "{ids:?}");
    }

    #[test]
    fn menu_hit_rects_match_drawn_lines() {
        let area = Rect::new(0, 0, 44, 16);
        let mut buf = Buffer::empty(area);
        let anchor_y = 4u16;
        let menu = MenuState {
            key: "hook:0".to_string(),
            anchor_y,
            can_copy: false,
            can_disable: true,
        };
        let hits = render(&model(), area, &mut buf, None, Some(&menu), 0, &[]);
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
            assert!(hit.width > 0 && hit.width <= area.width, "{id} width");
        }
    }

    #[test]
    fn collapsed_handle_declares_toggle() {
        let area = Rect::new(0, 0, 3, 12);
        let mut buf = Buffer::empty(area);
        let hits = render(&HarnessRenderModel::default(), area, &mut buf, None, None, 0, &[]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action_id, "harness:toggle_panel");
    }

    #[test]
    fn body_lines_flatten_sections_groups_and_rows() {
        let lines = body_lines(&model(), &[]);
        // CONTEXT(sec) + ctx row + HOOKS(sec) + PreToolUse(group) + 2 hook rows.
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], BodyLine::Section(0));
        assert_eq!(lines[1], BodyLine::Row(0, 0));
        assert_eq!(lines[2], BodyLine::Section(1));
        assert_eq!(lines[3], BodyLine::Group(1, "PreToolUse".to_string()));
        assert_eq!(lines[4], BodyLine::Row(1, 0));
    }
}
