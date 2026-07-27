//! Guest-side renderer.
//!
//! Two things are drawn:
//!
//! 1. [`button_line`] — the compact `[🩺 …]` shortcut button for the `Panel`
//!    surface, sitting in the bottom dash beside button-dash.
//! 2. [`overlay`] — the modal subpanel for the `Overlay` surface, whose rect is
//!    the whole pane area. It is built the same way `src/ui/settings.rs` builds
//!    the settings overlay: a centred, shrink-to-fit rounded panel shell, a
//!    bold title row, a `─` rule, a keyboard-navigable list with a `▸`
//!    selection marker, and an action-button footer.
//!
//! Both render into an off-screen ratatui [`Buffer`] which the shared
//! `guest::flatten_buffer` helper turns into host `Line` nodes, so named
//! ratatui colours map onto the host theme's roles and the panel follows the
//! active theme.
//!
//! ## Two deliberate departures from `settings.rs`, both forced by the bridge
//!
//! * **No dimmed backdrop, no panel background.** `flatten_buffer` carries fg
//!   role + bold/underline/italic and nothing else — background colour does not
//!   survive the crossing, so `dim_background` and `panel_bg` are unavailable to
//!   a guest. Instead the overlay paints its whole rect with spaces, which
//!   *clears* the panes behind it: an opaque modal rather than a dimmed one.
//! * **No `List` highlight_style.** Same reason: its selection is a background
//!   colour. Selection is carried by the `▸` marker plus BOLD, which is the
//!   other half of what settings already does (`.highlight_symbol(" ▸ ")`).
//!
//! ## Glyph + colour vocabulary
//!
//! `✓` green / `✗` red / `?` muted are lifted verbatim from
//! `plugins/version-dash-ui/src/render.rs` so one language covers both widgets.
//! `⟳` blue is the one addition — version-dash has no "an agent is working on
//! it" state to borrow from.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use crate::model::{FixerState, Snapshot, Status};

const MUTED: Color = Color::DarkGray;
const SUBTEXT: Color = Color::Gray;
const ACCENT: Color = Color::Cyan;

/// Widest the modal is allowed to get; it shrinks to fit a narrow pane area.
const MAX_BOX_WIDTH: u16 = 64;
/// Rows a single fixer occupies: title line + detail line.
const ROW_HEIGHT: u16 = 2;

/// State glyph — `✓ ✗ ?` exactly as version-dash uses them, plus `⟳` for a fix
/// or escalation in flight.
pub fn state_glyph(state: FixerState) -> &'static str {
    match state {
        FixerState::Healthy => "\u{2713}", // ✓
        FixerState::Broken => "\u{2717}",  // ✗
        FixerState::Unknown => "?",
        FixerState::Working => "\u{27F3}", // ⟳
    }
}

pub fn state_color(state: FixerState) -> Color {
    match state {
        FixerState::Healthy => Color::Green,
        FixerState::Broken => Color::Red,
        FixerState::Unknown => MUTED,
        FixerState::Working => Color::Blue,
    }
}

/// Short word shown after the glyph, so the state is legible without relying on
/// colour alone.
pub fn state_word(state: FixerState) -> &'static str {
    match state {
        FixerState::Healthy => "healthy",
        FixerState::Broken => "broken",
        FixerState::Unknown => "unknown",
        FixerState::Working => "working",
    }
}

// ── the bottom-dash button ───────────────────────────────────────────────────

/// The compact shortcut button: `[🩺 ✗1]`, or `[🩺 ⟳1]` while an agent is on a
/// row, or `[🩺 ✓]` when everything is healthy, or `[🩺 ?]` when there is no
/// usable snapshot. Its colour is the worst state present, so the button alone
/// answers "is anything broken" — and, with the overlay shut, "is anything
/// still being worked on".
pub fn button_line(snapshot: &Snapshot) -> Line<'static> {
    let (glyph, color) = match snapshot.status {
        Status::Ready => {
            let counts = snapshot.rollup();
            if counts.broken > 0 {
                (format!("\u{2717}{}", counts.broken), Color::Red)
            } else if counts.working > 0 {
                (format!("\u{27F3}{}", counts.working), Color::Blue)
            } else if counts.unknown > 0 {
                (format!("?{}", counts.unknown), Color::Yellow)
            } else {
                ("\u{2713}".to_string(), Color::Green)
            }
        }
        // Loading / Empty / Failed all mean "we do not know" — muted `?`, never
        // a false green.
        _ => ("?".to_string(), MUTED),
    };
    Line::from(vec![
        Span::styled("[", Style::default().fg(MUTED)),
        Span::styled("\u{1FA7A} ", Style::default().fg(ACCENT)), // 🩺
        Span::styled(
            glyph,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("]", Style::default().fg(MUTED)),
    ])
}

// ── the overlay subpanel ─────────────────────────────────────────────────────

/// Where a clickable region sits, in coordinates RELATIVE to the overlay rect.
/// `lib.rs` offsets these by the frame rect origin to build absolute
/// `UiHitRect`s, so geometry has exactly one source of truth: this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitBox {
    pub action_id: String,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Action ids the overlay emits. Kept as constants because `lib.rs` matches on
/// them and the tests assert on them.
pub const ACTION_CLOSE: &str = "doctor:close";
pub const ACTION_RECHECK: &str = "doctor:recheck";
pub const ACTION_FIX_SELECTED: &str = "doctor:fix-selected";
/// Row actions are `doctor:row:<fixer id>`.
pub const ACTION_ROW_PREFIX: &str = "doctor:row:";

/// Total modal height for a given body row count, mirroring
/// `settings_popup_height`: borders 2 + title 1 + rule 1 + body + spacer 1 +
/// footer 2.
pub fn box_height(body_rows: u16) -> u16 {
    2 + 1 + 1 + body_rows + 1 + 2
}

/// Rows the body needs: two per fixer, or three for an empty/failed message.
fn body_rows(snapshot: &Snapshot) -> u16 {
    match snapshot.status {
        Status::Ready => (snapshot.rows.len() as u16).saturating_mul(ROW_HEIGHT),
        _ => 3,
    }
}

/// Trim `text` to `budget` cells, ending in `…` when anything was dropped, so a
/// long line reads as truncated instead of merely stopping mid-word. Returns an
/// empty string when there is no room at all.
fn elide(text: &str, budget: u16) -> String {
    let budget = budget as usize;
    if text.chars().count() <= budget {
        return text.to_string();
    }
    if budget <= 1 {
        return String::new();
    }
    let mut out: String = text.chars().take(budget - 1).collect();
    out.push('\u{2026}');
    out
}

/// Trim `text` from the FRONT to `budget` cells, starting with `…`. Used for
/// paths, where the tail (which file) carries the information and the head
/// (`/Users/…`) does not.
fn elide_left(text: &str, budget: u16) -> String {
    let budget = budget as usize;
    let len = text.chars().count();
    if len <= budget {
        return text.to_string();
    }
    if budget <= 1 {
        return String::new();
    }
    let mut out = String::from("\u{2026}");
    out.extend(text.chars().skip(len - (budget - 1)));
    out
}

/// Centre a `w × h` box inside `area`, clamped so it always fits.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    )
}

/// Draw the whole overlay into `buf` (whose area is the overlay rect, origin
/// `(0,0)`) and return the clickable regions. Painting every cell of the rect —
/// including the blank backdrop — is what makes this opaque instead of letting
/// pane text show through the modal.
pub fn overlay(snapshot: &Snapshot, selected: usize, area: Rect, buf: &mut Buffer) -> Vec<HitBox> {
    let mut hits = Vec::new();
    if area.width < 24 || area.height < 8 {
        // Too small for a modal; the caller still gets a cleared rect and a
        // whole-area close target so the user is never trapped.
        hits.push(HitBox {
            action_id: ACTION_CLOSE.to_string(),
            x: 0,
            y: 0,
            width: area.width,
            height: area.height,
        });
        return hits;
    }

    // Opaque backdrop: clear every cell of the pane area.
    Paragraph::new(vec![
        Line::from(" ".repeat(area.width as usize));
        area.height as usize
    ])
    .render(area, buf);

    let popup = centered(
        area,
        MAX_BOX_WIDTH.min(area.width.saturating_sub(2)),
        box_height(body_rows(snapshot)).min(area.height),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(popup);
    block.render(popup, buf);
    if inner.width < 8 || inner.height < 4 {
        return hits;
    }

    let [title, rule, body, _gap, buttons, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    render_title(snapshot, title, buf);
    Paragraph::new(Span::styled(
        "\u{2500}".repeat(inner.width as usize),
        Style::default().fg(MUTED),
    ))
    .render(rule, buf);

    match &snapshot.status {
        Status::Ready => hits.extend(render_rows(snapshot, selected, body, buf)),
        status => render_empty_state(status, &snapshot.manifest_path, body, buf),
    }

    hits.extend(render_footer(snapshot, selected, buttons, buf));
    render_hint(snapshot, hint, buf);

    // Backdrop close target, declared LAST so every row/button rect above wins
    // the first-match hit test in the host. Clicking anywhere outside the modal
    // closes it, which is the mouse equivalent of `esc`.
    hits.push(HitBox {
        action_id: ACTION_CLOSE.to_string(),
        x: 0,
        y: 0,
        width: area.width,
        height: area.height,
    });
    hits
}

fn render_title(snapshot: &Snapshot, area: Rect, buf: &mut Buffer) {
    let mut spans = vec![Span::styled(
        " doctor",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )];
    if snapshot.status == Status::Ready {
        let counts = snapshot.rollup();
        spans.push(Span::styled("   ", Style::default().fg(MUTED)));
        spans.push(Span::styled(
            format!("\u{2713}{}", counts.healthy),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("\u{2717}{}", counts.broken),
            Style::default().fg(Color::Red),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("?{}", counts.unknown),
            Style::default().fg(MUTED),
        ));
        // Only shown when non-zero: a permanent `⟳0` would read as noise, but a
        // hidden `⟳1` would hide the fact that an agent is mid-repair.
        if counts.working > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("\u{27F3}{}", counts.working),
                Style::default().fg(Color::Blue),
            ));
        }
    }
    Paragraph::new(Line::from(spans)).render(area, buf);
}

/// One fixer per two rows:
/// ```text
///  ▸ ✗ hr / herdr server        broken
///      socket missing · click to fix
/// ```
fn render_rows(snapshot: &Snapshot, selected: usize, area: Rect, buf: &mut Buffer) -> Vec<HitBox> {
    let mut hits = Vec::new();
    for (index, row) in snapshot.rows.iter().enumerate() {
        let y = area.y + (index as u16).saturating_mul(ROW_HEIGHT);
        if y + 1 >= area.y + area.height {
            break;
        }
        let is_selected = index == selected;
        let marker = if is_selected { " \u{25B8} " } else { "   " };
        let mut label_style = Style::default().fg(SUBTEXT);
        if is_selected {
            label_style = label_style.fg(Color::White).add_modifier(Modifier::BOLD);
        }
        let glyph_style = Style::default()
            .fg(state_color(row.state))
            .add_modifier(Modifier::BOLD);

        // The NOTE lives on the title line, not the subtitle. It is the outcome
        // an exit code cannot express — "handed to an agent" versus "the fix
        // broke" — and it must never be the thing that gets clipped. The detail,
        // which is long and machine-generated, is the one that gets elided.
        // Last resort only: a pathologically long note is elided so it reads as
        // truncated instead of being hard-clipped by the terminal at the border.
        // The budget keeps room for a stub of the label and the state word, both
        // of which outrank the note.
        const MIN_LABEL: u16 = 8;
        let fixed_head = marker.chars().count() as u16
            + 2
            + 2
            + state_word(row.state).chars().count() as u16
            + 3; // " · "
        let note = elide(
            row.note.as_deref().unwrap_or_default(),
            area.width
                .saturating_sub(fixed_head)
                .saturating_sub(MIN_LABEL),
        );
        let note = note.as_str();
        let mut head = vec![
            Span::styled(marker, Style::default().fg(ACCENT)),
            Span::styled(format!("{} ", state_glyph(row.state)), glyph_style),
        ];
        let state_text = if note.is_empty() {
            format!("  {}", state_word(row.state))
        } else {
            format!("  {} \u{00B7} {note}", state_word(row.state))
        };
        // Give the label whatever the fixed-width right-hand part leaves over.
        let label_budget = area
            .width
            .saturating_sub(marker.chars().count() as u16 + 2)
            .saturating_sub(state_text.chars().count() as u16);
        head.push(Span::styled(elide(&row.label, label_budget), label_style));
        head.push(Span::styled(
            state_text,
            Style::default().fg(match row.state {
                // A note on a still-broken row means nobody picked it up.
                FixerState::Broken if !note.is_empty() => Color::Red,
                FixerState::Working if !note.is_empty() => Color::Blue,
                other => state_color(other),
            }),
        ));
        Paragraph::new(Line::from(head)).render(Rect::new(area.x, y, area.width, 1), buf);

        // Subtitle: the fixer's own `detail` line, elided to fit, plus — for a
        // broken row with nothing else to say — the click-to-fix affordance.
        const SUB_INDENT: &str = "     ";
        let mut sub = vec![Span::styled(SUB_INDENT, Style::default().fg(MUTED))];
        let affordance = if note.is_empty() && row.state.is_fixable() {
            " \u{00B7} click to fix"
        } else {
            ""
        };
        let detail_budget = area
            .width
            .saturating_sub(SUB_INDENT.len() as u16)
            .saturating_sub(affordance.chars().count() as u16);
        let detail = if row.detail.is_empty() {
            "\u{2014}".to_string() // —
        } else {
            elide(&row.detail, detail_budget)
        };
        sub.push(Span::styled(detail, Style::default().fg(MUTED)));
        if !affordance.is_empty() {
            sub.push(Span::styled(affordance, Style::default().fg(Color::Yellow)));
        }
        Paragraph::new(Line::from(sub)).render(Rect::new(area.x, y + 1, area.width, 1), buf);

        hits.push(HitBox {
            action_id: format!("{ACTION_ROW_PREFIX}{}", row.id),
            x: area.x,
            y,
            width: area.width,
            height: ROW_HEIGHT,
        });
    }
    hits
}

/// The informative empty states. Never a blank box, never a crash: each says
/// what happened and which file to look at.
fn render_empty_state(status: &Status, manifest_path: &str, area: Rect, buf: &mut Buffer) {
    let (headline, headline_color, detail) = match status {
        Status::Loading => (
            "running checks\u{2026}",
            SUBTEXT,
            "the doctor runner has not written a snapshot yet".to_string(),
        ),
        Status::Empty => (
            "no fixers defined",
            Color::Yellow,
            "the manifest parsed but declares no entries".to_string(),
        ),
        Status::Failed(message) => ("cannot read fixers", Color::Red, message.clone()),
        Status::Ready => return,
    };

    // Budget for the text after the two-space indent.
    let budget = area.width.saturating_sub(2);
    // The path is elided from the LEFT: when a manifest path is too long, the
    // filename is the part that tells you which file to go fix, not `/Users`.
    let path_line = if manifest_path.is_empty() {
        "manifest: ~/.config/herdr/fixers.json".to_string()
    } else {
        format!(
            "manifest: {}",
            elide_left(manifest_path, budget.saturating_sub(10))
        )
    };

    let lines = vec![
        Line::from(Span::styled(
            format!("  {}", elide(headline, budget)),
            Style::default()
                .fg(headline_color)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  {}", elide(&detail, budget)),
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            format!("  {path_line}"),
            Style::default().fg(MUTED),
        )),
    ];
    Paragraph::new(lines).render(area, buf);
}

/// Footer action buttons, in the shape `render_action_button` gives the settings
/// overlay: a hint glyph then a label. `fix` is dimmed out when the selected row
/// is not repairable, so the button never lies about what a click will do.
fn render_footer(
    snapshot: &Snapshot,
    selected: usize,
    area: Rect,
    buf: &mut Buffer,
) -> Vec<HitBox> {
    let fix_enabled = snapshot
        .rows
        .get(selected)
        .map(|row| row.state.is_fixable())
        .unwrap_or(false);

    let specs: [(&str, &str, Color, bool); 3] = [
        ("\u{27F3}", "recheck", ACCENT, true),
        (
            "\u{2699}",
            "fix",
            if fix_enabled { Color::Yellow } else { MUTED },
            fix_enabled,
        ),
        ("esc", "close", SUBTEXT, true),
    ];
    let ids = [ACTION_RECHECK, ACTION_FIX_SELECTED, ACTION_CLOSE];

    let mut hits = Vec::new();
    let mut spans = Vec::new();
    let mut x = area.x;
    spans.push(Span::raw(" "));
    x += 1;
    for (index, (hint, label, color, enabled)) in specs.into_iter().enumerate() {
        let text = format!(" {hint} {label} ");
        let width = text.chars().count() as u16;
        let style = if enabled {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };
        spans.push(Span::styled(text, style));
        if enabled {
            hits.push(HitBox {
                action_id: ids[index].to_string(),
                x,
                y: area.y,
                width,
                height: 1,
            });
        }
        x += width;
        spans.push(Span::raw("  "));
        x += 2;
    }
    Paragraph::new(Line::from(spans)).render(area, buf);
    hits
}

fn render_hint(snapshot: &Snapshot, area: Rect, buf: &mut Buffer) {
    let text = match snapshot.status {
        Status::Ready => " \u{2191}\u{2193} select   click a broken row to fix   esc close",
        _ => " \u{27F3} recheck re-runs every check   esc close",
    };
    Paragraph::new(Span::styled(text, Style::default().fg(MUTED))).render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FixerRow, Snapshot};

    fn row(id: &str, state: FixerState, detail: &str) -> FixerRow {
        FixerRow {
            id: id.to_string(),
            label: format!("{id} / label"),
            state,
            detail: detail.to_string(),
            note: None,
        }
    }

    fn ready(rows: Vec<FixerRow>) -> Snapshot {
        Snapshot {
            status: Status::Ready,
            manifest_path: "/tmp/fixers.json".into(),
            generation: 1,
            rows,
        }
    }

    fn draw(snapshot: &Snapshot, selected: usize, w: u16, h: u16) -> (String, Vec<HitBox>) {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let hits = overlay(snapshot, selected, area, &mut buf);
        let text = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, hits)
    }

    #[test]
    fn healthy_broken_unknown_working_each_render_their_glyph() {
        let snapshot = ready(vec![
            row("cx", FixerState::Healthy, "gateway up on :3456"),
            row("hr", FixerState::Broken, "socket missing"),
            row("disk", FixerState::Unknown, "smartctl unavailable"),
            row("net", FixerState::Working, "repairing"),
        ]);
        let (text, _) = draw(&snapshot, 0, 100, 30);
        assert!(text.contains('\u{2713}'), "healthy ✓ missing:\n{text}");
        assert!(text.contains('\u{2717}'), "broken ✗ missing:\n{text}");
        assert!(text.contains('?'), "unknown ? missing:\n{text}");
        assert!(text.contains('\u{27F3}'), "working ⟳ missing:\n{text}");
        for word in ["healthy", "broken", "unknown", "working"] {
            assert!(text.contains(word), "state word {word} missing:\n{text}");
        }
    }

    #[test]
    fn detail_line_is_rendered_as_the_row_subtitle() {
        let snapshot = ready(vec![row("cx", FixerState::Healthy, "gateway up on :3456")]);
        let (text, _) = draw(&snapshot, 0, 100, 30);
        assert!(text.contains("gateway up on :3456"), "{text}");
    }

    #[test]
    fn broken_row_advertises_click_to_fix_and_healthy_row_does_not() {
        let broken = ready(vec![row("hr", FixerState::Broken, "socket missing")]);
        let (text, _) = draw(&broken, 0, 100, 30);
        assert!(text.contains("click to fix"), "{text}");

        let healthy = ready(vec![row("cx", FixerState::Healthy, "up")]);
        let (text, _) = draw(&healthy, 0, 100, 30);
        assert!(!text.contains("click to fix"), "{text}");
    }

    #[test]
    fn a_long_detail_never_clips_the_note_off_the_row() {
        // Regression, found live: with the note appended to the subtitle, a
        // realistic fixer report pushed "handed to an agent" past the box edge —
        // losing exactly the word that distinguishes an escalation from a broken
        // fix. The note now lives on the title line and the detail is elided.
        let mut snapshot = ready(vec![row(
            "disk",
            FixerState::Working,
            "cleared 2 of 6 items; the rest need judgement and a human decision",
        )]);
        snapshot.rows[0].note = Some("handed to an agent".into());
        let (text, _) = draw(&snapshot, 0, 100, 30);
        assert!(text.contains("handed to an agent"), "note clipped:\n{text}");
        assert!(
            text.contains('\u{2026}'),
            "detail should be elided:\n{text}"
        );
        // And nothing escaped the modal: every painted row is inside the border.
        for line in text.lines().filter(|l| l.contains("handed to an agent")) {
            assert!(line.matches('\u{2502}').count() >= 2, "row escaped: {line}");
        }
    }

    #[test]
    fn a_long_label_is_elided_rather_than_pushing_the_state_out() {
        let snapshot = ready(vec![FixerRow {
            id: "x".into(),
            label: "an extremely long fixer label that will not fit in any sane modal".into(),
            state: FixerState::Broken,
            detail: "short".into(),
            note: None,
        }]);
        let (text, _) = draw(&snapshot, 0, 100, 30);
        assert!(text.contains("broken"), "state word pushed out:\n{text}");
        assert!(text.contains('\u{2026}'), "label should be elided:\n{text}");
    }

    #[test]
    fn elide_is_total_on_tiny_budgets() {
        assert_eq!(elide("abc", 10), "abc");
        assert_eq!(elide("abcdef", 4), "abc\u{2026}");
        assert_eq!(elide("abcdef", 1), "");
        assert_eq!(elide("abcdef", 0), "");
    }

    #[test]
    fn working_row_shows_the_escalation_note_instead_of_the_fix_affordance() {
        let mut snapshot = ready(vec![row("cx", FixerState::Working, "restarting ccr")]);
        snapshot.rows[0].note = Some("escalated to an agent".into());
        let (text, _) = draw(&snapshot, 0, 100, 30);
        assert!(text.contains("escalated to an agent"), "{text}");
        assert!(!text.contains("click to fix"), "{text}");
    }

    #[test]
    fn every_row_gets_a_hit_box_plus_footer_and_backdrop() {
        let snapshot = ready(vec![
            row("cx", FixerState::Healthy, "up"),
            row("hr", FixerState::Broken, "down"),
        ]);
        let (_, hits) = draw(&snapshot, 1, 100, 30);
        assert!(hits
            .iter()
            .any(|h| h.action_id == format!("{ACTION_ROW_PREFIX}cx")));
        assert!(hits
            .iter()
            .any(|h| h.action_id == format!("{ACTION_ROW_PREFIX}hr")));
        assert!(hits.iter().any(|h| h.action_id == ACTION_RECHECK));
        assert!(hits.iter().any(|h| h.action_id == ACTION_FIX_SELECTED));
        // Backdrop close is last so row/button rects win the host's first-match.
        assert_eq!(
            hits.last().map(|h| h.action_id.as_str()),
            Some(ACTION_CLOSE)
        );
    }

    #[test]
    fn fix_button_is_disabled_and_unclickable_when_selection_is_not_broken() {
        let snapshot = ready(vec![row("cx", FixerState::Healthy, "up")]);
        let (_, hits) = draw(&snapshot, 0, 100, 30);
        assert!(
            !hits.iter().any(|h| h.action_id == ACTION_FIX_SELECTED),
            "healthy selection must not expose a fix hit-rect"
        );
    }

    #[test]
    fn selection_marker_moves_with_the_selected_index() {
        let snapshot = ready(vec![
            row("cx", FixerState::Healthy, "up"),
            row("hr", FixerState::Broken, "down"),
        ]);
        let (text, _) = draw(&snapshot, 1, 100, 30);
        let marked: Vec<&str> = text
            .lines()
            .filter(|line| line.contains('\u{25B8}'))
            .collect();
        assert_eq!(marked.len(), 1, "exactly one ▸ marker:\n{text}");
        assert!(marked[0].contains("hr / label"), "{}", marked[0]);
    }

    #[test]
    fn empty_state_names_the_manifest_path() {
        let snapshot = Snapshot {
            status: Status::Empty,
            manifest_path: "/Users/x/.config/herdr/fixers.json".into(),
            ..Default::default()
        };
        let (text, hits) = draw(&snapshot, 0, 100, 30);
        assert!(text.contains("no fixers defined"), "{text}");
        assert!(
            text.contains("/Users/x/.config/herdr/fixers.json"),
            "{text}"
        );
        assert!(hits.iter().any(|h| h.action_id == ACTION_CLOSE));
    }

    /// A long manifest path used to run into the border and stop mid-directory,
    /// which reads as a different path rather than a truncated one. Keep the
    /// filename — the part that says which file to open — and mark the cut.
    #[test]
    fn a_long_manifest_path_keeps_its_filename() {
        let snapshot = Snapshot {
            status: Status::Empty,
            manifest_path: "/Users/andual/Projects/herdr-worktrees/doctor-dash/plugins/doctor-dash-ui/fixtures/fixers.states.json".into(),
            ..Default::default()
        };
        let (text, _) = draw(&snapshot, 0, 70, 20);
        let line = text
            .lines()
            .find(|l| l.contains("manifest:"))
            .unwrap_or_default();
        assert!(line.contains("fixers.states.json"), "{line}");
        assert!(line.contains('\u{2026}'), "{line}");
    }

    #[test]
    fn failed_state_shows_the_reason_and_still_offers_recheck_and_close() {
        let snapshot = Snapshot {
            status: Status::Failed("fixers.json line 3: trailing comma".into()),
            manifest_path: "/tmp/fixers.json".into(),
            ..Default::default()
        };
        let (text, hits) = draw(&snapshot, 0, 100, 30);
        assert!(text.contains("cannot read fixers"), "{text}");
        assert!(text.contains("trailing comma"), "{text}");
        assert!(hits.iter().any(|h| h.action_id == ACTION_RECHECK));
        assert!(hits.iter().any(|h| h.action_id == ACTION_CLOSE));
    }

    #[test]
    fn loading_state_is_informative_not_blank() {
        let (text, _) = draw(&Snapshot::default(), 0, 100, 30);
        assert!(text.contains("running checks"), "{text}");
    }

    #[test]
    fn overlay_is_opaque_over_the_whole_rect() {
        // Pre-fill the buffer with pane-like content; the overlay must clear all
        // of it, otherwise pane text bleeds through the modal.
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        for y in 0..area.height {
            for x in 0..area.width {
                buf[(x, y)].set_symbol("X");
            }
        }
        let snapshot = ready(vec![row("cx", FixerState::Healthy, "up")]);
        overlay(&snapshot, 0, area, &mut buf);
        let leftover = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].symbol() == "X")
            .count();
        assert_eq!(leftover, 0, "{leftover} pane cells bled through the modal");
    }

    #[test]
    fn tiny_rect_degrades_to_a_close_target_instead_of_panicking() {
        let snapshot = ready(vec![row("cx", FixerState::Healthy, "up")]);
        for (w, h) in [(0, 0), (1, 1), (10, 4), (23, 7)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            let hits = overlay(&snapshot, 0, area, &mut buf);
            assert!(hits.iter().any(|hit| hit.action_id == ACTION_CLOSE));
        }
    }

    #[test]
    fn many_rows_never_overflow_the_available_height() {
        let rows: Vec<_> = (0..40)
            .map(|i| row(&format!("f{i}"), FixerState::Broken, "detail"))
            .collect();
        let snapshot = ready(rows);
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let hits = overlay(&snapshot, 0, area, &mut buf);
        for hit in &hits {
            assert!(
                hit.y + hit.height <= area.height,
                "hit {hit:?} escapes the overlay rect"
            );
        }
    }

    #[test]
    fn button_colour_and_glyph_track_the_worst_state() {
        let broken = ready(vec![
            row("cx", FixerState::Healthy, ""),
            row("hr", FixerState::Broken, ""),
        ]);
        assert!(line_text(&button_line(&broken)).contains('\u{2717}'));

        let unknown = ready(vec![row("disk", FixerState::Unknown, "")]);
        assert!(line_text(&button_line(&unknown)).contains('?'));

        let all_good = ready(vec![row("cx", FixerState::Healthy, "")]);
        let text = line_text(&button_line(&all_good));
        assert!(text.contains('\u{2713}'), "{text}");
        assert!(!text.contains('\u{2717}'), "{text}");

        // No snapshot at all must never read as green.
        let text = line_text(&button_line(&Snapshot::default()));
        assert!(text.contains('?'), "{text}");
        assert!(!text.contains('\u{2713}'), "{text}");
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    /// With the overlay shut, the button is the only place an in-flight repair
    /// is visible. Folding `working` into the `?` bucket made an agent already
    /// on the job look like a check we could not run.
    #[test]
    fn a_working_row_is_not_reported_as_unknown() {
        let working = ready(vec![
            row("cx", FixerState::Healthy, ""),
            row("hr", FixerState::Working, ""),
        ]);
        let text = line_text(&button_line(&working));
        assert!(text.contains('\u{27F3}'), "{text}");
        assert!(!text.contains('?'), "{text}");

        let (text, _) = draw(&working, 0, 70, 16);
        assert!(text.contains("\u{27F3}1"), "{text}");
        assert!(text.contains("?0"), "{text}");
    }

    /// A note longer than the row must degrade with `…`, not be silently cut
    /// off by the border — and the state word must survive either way.
    #[test]
    fn a_pathological_note_is_elided_not_hard_clipped() {
        let mut snapshot = ready(vec![row("hr", FixerState::Working, "detail")]);
        snapshot.rows[0].note = Some("x".repeat(400));
        let (text, _) = draw(&snapshot, 0, 70, 14);
        let line = text
            .lines()
            .find(|l| l.contains("working"))
            .unwrap_or_default();
        assert!(line.contains('\u{2026}'), "{line}");
        assert!(line.chars().count() <= 70, "{line}");
    }

    /// A broken row outranks a working one: something still needs a human.
    #[test]
    fn broken_still_wins_over_working_on_the_button() {
        let mixed = ready(vec![
            row("cx", FixerState::Working, ""),
            row("hr", FixerState::Broken, ""),
        ]);
        assert!(line_text(&button_line(&mixed)).contains('\u{2717}'));
    }

    #[test]
    fn button_always_carries_the_stethoscope() {
        assert!(line_text(&button_line(&Snapshot::default())).contains('\u{1FA7A}'));
    }
}
