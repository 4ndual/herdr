//! Guest-side renderer for the tidus subagent bar.
//!
//! One compact bottom row into an off-screen ratatui [`Buffer`], which the
//! shared `flatten_buffer` helper turns into host `Line` nodes. Named ratatui
//! colors map to host theme roles via `guest::color_role`, so the bar follows
//! the active theme instead of painting raw ANSI.
//!
//! Shape — ancestors pinned left, the focused node marked, then its children:
//!
//! ```text
//! ⑂ main ▸ ▛you▟ │ ● reviewer ○ writer ◆ builder
//! ⑂ main ▸ squad ▸ ▛you▟ │ ○ grandchild
//! ⑂ ▛you▟ │ (no children)
//! ```
//!
//! Every chip is its own click target, so the bar is a row of buttons rather
//! than one line with a meaning. HOVER a chip and the row swaps to that chip's
//! detail; left-click navigates (the terminal area swaps to that agent);
//! right-click is inert, because the canon reserves it for a contextual menu
//! and this bar has no secondary actions.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use tidus_bar_model::{BarInput, Chip, ChipRole, ChipStatus, EdgeKind};

const MUTED: Color = Color::DarkGray;
const SUBTEXT: Color = Color::Gray;
/// Live work. Green reads as "running" in every terminal theme we ship.
const BUSY: Color = Color::Green;
/// The one state the brief says to surface LOUDLY.
const ALARM: Color = Color::Yellow;
const FAILED: Color = Color::Red;
/// The focused node — "you are here".
const CURRENT: Color = Color::Cyan;

/// Leading glyph: a fork in the tree. Marks the row as the lineage bar rather
/// than one more status strip.
const ICON: &str = "\u{2442}"; // ⑂
/// Between lineage chips: descent.
const DESCEND: &str = " \u{25B8} "; // " ▸ "
/// Between the lineage and the children.
const DIVIDER: &str = " \u{2502} "; // " │ "
/// Brackets around the focused node.
const HERE_OPEN: &str = "\u{258C}"; // ▌
const HERE_CLOSE: &str = "\u{2590}"; // ▐
/// Shown when the focused orchestrator has spawned nothing.
const NO_CHILDREN: &str = "no children";

// The bar publishes NO hit-rects any more: hover needs the raw mouse stream and
// the host suppresses that over a module's own rects, so clicks are resolved
// from [`layout`] instead. The old "tidus-bar:chip:<n>" action id is deliberately
// gone rather than kept as a reserved constant — an id nothing can dispatch is
// how a dead click target survives a refactor.

/// One laid-out chip: the spans that draw it and where it sits on the row, so
/// the guest can resolve a press against the exact cells it drew.
pub struct Placed {
    pub index: usize,
    /// Column offset from the start of the row.
    pub x: u16,
    pub width: u16,
}

/// Build the row and the placement of every chip in it.
///
/// Returns `None` when there is nothing worth a row at all — the guest then
/// emits no layer and the host carves no band, rather than reserving a line to
/// display emptiness.
pub fn layout(input: &BarInput) -> Option<(Line<'static>, Vec<Placed>)> {
    if input.is_empty() {
        return None;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut placed: Vec<Placed> = Vec::new();
    let mut x: u16 = 0;

    push(
        &mut spans,
        &mut x,
        format!("{ICON} "),
        Style::default().fg(SUBTEXT),
    );

    let mut first_lineage = true;
    for (index, chip) in input.chips.iter().enumerate() {
        if chip.role == ChipRole::Child {
            continue;
        }
        if !first_lineage {
            push(
                &mut spans,
                &mut x,
                DESCEND.to_string(),
                Style::default().fg(MUTED),
            );
        }
        first_lineage = false;
        let start = x;
        for (text, style) in lineage_spans(chip) {
            push(&mut spans, &mut x, text, style);
        }
        placed.push(Placed {
            index,
            x: start,
            width: x - start,
        });
    }

    push(
        &mut spans,
        &mut x,
        DIVIDER.to_string(),
        Style::default().fg(MUTED),
    );

    let mut any_child = false;
    for (index, chip) in input.chips.iter().enumerate() {
        if chip.role != ChipRole::Child {
            continue;
        }
        if any_child {
            push(&mut spans, &mut x, " ".to_string(), Style::default());
        }
        any_child = true;
        let start = x;
        for (text, style) in child_spans(chip) {
            push(&mut spans, &mut x, text, style);
        }
        placed.push(Placed {
            index,
            x: start,
            width: x - start,
        });
    }
    if !any_child {
        // Deliberately not styled as a problem: an orchestrator that has not
        // dispatched anything yet is the normal starting state.
        push(
            &mut spans,
            &mut x,
            NO_CHILDREN.to_string(),
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        );
    }

    Some((Line::from(spans), placed))
}

/// Ancestors render as plain names; the focused node is bracketed so "where am
/// I" is answerable at a glance without counting positions.
fn lineage_spans(chip: &Chip) -> Vec<(String, Style)> {
    let dot = Style::default().fg(status_color(chip.status));
    match chip.role {
        ChipRole::Current => vec![
            (HERE_OPEN.to_string(), Style::default().fg(CURRENT)),
            (
                chip.name.clone(),
                Style::default().fg(CURRENT).add_modifier(Modifier::BOLD),
            ),
            (HERE_CLOSE.to_string(), Style::default().fg(CURRENT)),
        ],
        _ => vec![
            (format!("{} ", chip.status.dot()), dot),
            (chip.name.clone(), Style::default().fg(SUBTEXT)),
        ],
    }
}

/// A child chip: status dot, name, and the job tail when there is one to
/// disambiguate two dispatches to the same child (brief §7).
fn child_spans(chip: &Chip) -> Vec<(String, Style)> {
    let color = status_color(chip.status);
    let mut name_style = Style::default().fg(color);
    if chip.status.is_alarming() {
        name_style = name_style.add_modifier(Modifier::BOLD);
    }
    if chip.status.is_dormant() {
        // Reaped/finished children stay attached but must not compete with
        // live ones for attention.
        name_style = Style::default().fg(MUTED);
    }
    let mut out = vec![
        (
            format!("{} ", chip.status.dot()),
            Style::default().fg(color),
        ),
        (chip.name.clone(), name_style),
    ];
    // Only a job actually in flight earns the disambiguating tail; a finished
    // chip showing one would read as still running.
    let in_flight = chip.edge == Some(EdgeKind::Dispatch);
    if let Some(tail) = chip.job_tail.as_ref().filter(|_| in_flight) {
        out.push((format!(":{tail}"), Style::default().fg(MUTED)));
    }
    out
}

fn status_color(status: ChipStatus) -> Color {
    match status {
        ChipStatus::Attention => ALARM,
        ChipStatus::Failed => FAILED,
        ChipStatus::Busy | ChipStatus::Starting => BUSY,
        ChipStatus::Idle => SUBTEXT,
        ChipStatus::RefreshQueued => SUBTEXT,
        ChipStatus::Done | ChipStatus::Stopped | ChipStatus::Unknown => MUTED,
    }
}

fn push(spans: &mut Vec<Span<'static>>, x: &mut u16, text: String, style: Style) {
    *x += width_of(&text);
    spans.push(Span::styled(text, style));
}

/// Terminal cell width. The glyphs here are all single-width BMP characters, so
/// counting chars is exact and avoids pulling in a unicode-width dependency for
/// a wasm guest that must stay small.
fn width_of(text: &str) -> u16 {
    text.chars().count() as u16
}

/// Natural width of the whole row, so the host can carve the slot on the first
/// content frame (before which `ctx.rect` is still zero).
pub fn content_width(input: &BarInput) -> u16 {
    layout(input)
        .map(|(line, _)| line.spans.iter().map(|s| width_of(&s.content)).sum())
        .unwrap_or(0)
}

pub fn render(input: &BarInput, area: Rect, buf: &mut Buffer) {
    let Some((line, _)) = layout(input) else {
        return;
    };
    Paragraph::new(line).render(area, buf);
}

/// The row as it reads WHILE A CHIP IS HOVERED: that chip's detail, in place of
/// the summary row.
///
/// The canon puts detail on hover, but this module's carve is exactly one row
/// (`[ui_modules.placement].size = "1"`), so there is no slack row for a tooltip
/// and the guest API has no positioned node to float one beside the bar.
/// Swapping the row's own content keeps the single hover trigger without
/// covering anything or resizing anything — the shape harness-indicator uses.
pub fn hovered_line(input: &BarInput, index: usize) -> Option<Line<'static>> {
    let chip = input.chips.get(index)?;
    Some(Line::from(vec![
        Span::styled(format!("{ICON} "), Style::default().fg(MUTED)),
        Span::styled(detail(input, chip), Style::default().fg(MUTED)),
    ]))
}

/// Width of the hovered form, so the host carves a slot that fits it.
pub fn hovered_width(input: &BarInput, index: usize) -> u16 {
    hovered_line(input, index)
        .map(|line| line.width() as u16)
        .unwrap_or(0)
}

pub fn render_hovered(input: &BarInput, index: usize, area: Rect, buf: &mut Buffer) {
    if let Some(line) = hovered_line(input, index) {
        Paragraph::new(line).render(area, buf);
    }
}

/// The detail shown on HOVER, and reused as the refusal text when a chip has no
/// pane to navigate to. Joined with ` · ` because the host's toast is single
/// line and silently strips newlines.
pub fn detail(input: &BarInput, chip: &Chip) -> String {
    let mut parts = vec![format!("{} [{}]", chip.name, chip.status.label())];
    if let Some(runtime) = &chip.runtime {
        parts.push(runtime.clone());
    }
    parts.push(
        match chip.role {
            ChipRole::Ancestor => "ancestor",
            ChipRole::Current => "focused",
            ChipRole::Child => match chip.edge {
                Some(EdgeKind::Dispatch) => "child via live dispatch",
                Some(EdgeKind::Team) => "child via team",
                Some(EdgeKind::History) => "child via last job",
                None => "child",
            },
        }
        .to_string(),
    );
    if let Some(exec) = &chip.exec {
        // `light` children run inside conductor and have no PTY of their own,
        // so "no pane" is a property of the child, not a lookup failure.
        parts.push(format!("exec {exec}"));
    }
    if let Some(tail) = &chip.job_tail {
        parts.push(format!("job …{tail}"));
    }
    match &chip.tab_id {
        Some(tab) => parts.push(format!("tab {tab}")),
        None => parts.push("no pane in this instance".to_string()),
    }
    if let Some(task) = &chip.task {
        parts.push(format!("task: {}", first_line(task, 60)));
    }
    if let Some(evidence) = &input.evidence {
        parts.push(format!("via {evidence}"));
    }
    parts.join(" \u{00B7} ")
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max {
        return line.to_string();
    }
    let mut out: String = chars[..max].iter().collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(name: &str, role: ChipRole, status: ChipStatus) -> Chip {
        Chip {
            name: name.to_string(),
            role,
            status,
            edge: (role == ChipRole::Child).then_some(EdgeKind::Dispatch),
            tab_id: Some("w1:t2".into()),
            pane_id: Some("w1:p2".into()),
            hidden: false,
            task: None,
            job_tail: None,
            exec: None,
            runtime: Some("claude".into()),
        }
    }

    fn bar(chips: Vec<Chip>) -> BarInput {
        BarInput {
            chips,
            empty_reason: None,
            ..Default::default()
        }
    }

    fn text_of(input: &BarInput) -> String {
        let (line, _) = layout(input).expect("a row");
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn an_empty_bar_draws_no_row_at_all() {
        assert!(layout(&BarInput::default()).is_none());
        assert_eq!(content_width(&BarInput::default()), 0);
    }

    #[test]
    fn lineage_then_children_in_order() {
        let input = bar(vec![
            chip("main", ChipRole::Ancestor, ChipStatus::Idle),
            chip("squad", ChipRole::Current, ChipStatus::Busy),
            chip("kid-a", ChipRole::Child, ChipStatus::Busy),
            chip("kid-b", ChipRole::Child, ChipStatus::Idle),
        ]);
        let text = text_of(&input);
        let main = text.find("main").expect("ancestor");
        let squad = text.find("squad").expect("current");
        let a = text.find("kid-a").expect("child a");
        let b = text.find("kid-b").expect("child b");
        assert!(main < squad && squad < a && a < b, "render order: {text}");
        // The focused node is bracketed so "you are here" needs no counting.
        assert!(
            text.contains(&format!("{HERE_OPEN}squad{HERE_CLOSE}")),
            "{text}"
        );
        assert!(text.contains(DIVIDER), "lineage and children are separated");
    }

    #[test]
    fn a_lone_orchestrator_says_so_rather_than_rendering_a_bare_divider() {
        let input = bar(vec![chip("main", ChipRole::Current, ChipStatus::Idle)]);
        let text = text_of(&input);
        assert!(text.contains(NO_CHILDREN), "{text}");
    }

    #[test]
    fn every_chip_gets_its_own_hit_placement_and_they_do_not_overlap() {
        let input = bar(vec![
            chip("main", ChipRole::Ancestor, ChipStatus::Idle),
            chip("squad", ChipRole::Current, ChipStatus::Busy),
            chip("kid-a", ChipRole::Child, ChipStatus::Busy),
            chip("kid-b", ChipRole::Child, ChipStatus::Idle),
        ]);
        let (_, placed) = layout(&input).expect("a row");
        assert_eq!(placed.len(), 4, "one placement per chip");
        // Indices must address the ORIGINAL chip vector, or a click focuses the
        // wrong agent — the exact class of bug that has no error message.
        assert_eq!(
            placed.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        for pair in placed.windows(2) {
            assert!(
                pair[0].x + pair[0].width <= pair[1].x,
                "chips overlap: {:?}..{:?}",
                pair[0].x,
                pair[1].x
            );
            assert!(pair[0].width > 0);
        }
    }

    #[test]
    fn placement_matches_where_the_name_actually_lands() {
        let input = bar(vec![
            chip("main", ChipRole::Current, ChipStatus::Idle),
            chip("kid-a", ChipRole::Child, ChipStatus::Busy),
        ]);
        let text = text_of(&input);
        let (_, placed) = layout(&input).expect("a row");
        let kid = placed.iter().find(|p| p.index == 1).expect("child placed");
        // The chip starts at its status dot, so the name sits just inside the
        // rect; this pins the arithmetic that the hit rect depends on.
        let name_at = text.chars().collect::<Vec<_>>()[kid.x as usize..]
            .iter()
            .collect::<String>();
        assert!(name_at.starts_with(ChipStatus::Busy.dot()), "{name_at}");
        assert!(name_at.contains("kid-a"));
    }

    #[test]
    fn content_width_counts_the_whole_row() {
        let input = bar(vec![
            chip("main", ChipRole::Current, ChipStatus::Idle),
            chip("kid-a", ChipRole::Child, ChipStatus::Busy),
        ]);
        assert_eq!(
            content_width(&input),
            text_of(&input).chars().count() as u16
        );
    }

    #[test]
    fn a_live_child_shows_its_job_tail_and_a_finished_one_does_not() {
        let mut live = chip("kid", ChipRole::Child, ChipStatus::Busy);
        live.job_tail = Some("895251".into());
        live.edge = Some(EdgeKind::Dispatch);
        let input = bar(vec![
            chip("main", ChipRole::Current, ChipStatus::Idle),
            live,
        ]);
        assert!(text_of(&input).contains(":895251"));

        let mut done = chip("kid", ChipRole::Child, ChipStatus::Done);
        done.job_tail = Some("895251".into());
        done.edge = Some(EdgeKind::History);
        let input = bar(vec![
            chip("main", ChipRole::Current, ChipStatus::Idle),
            done,
        ]);
        assert!(
            !text_of(&input).contains(":895251"),
            "a finished chip is not a running job"
        );
    }

    #[test]
    fn attention_and_failure_are_the_only_loud_colors() {
        assert_eq!(status_color(ChipStatus::Attention), ALARM);
        assert_eq!(status_color(ChipStatus::Failed), FAILED);
        assert_eq!(status_color(ChipStatus::Busy), BUSY);
        // Dormant states must not compete with live ones for the eye.
        assert_eq!(status_color(ChipStatus::Stopped), MUTED);
        assert_eq!(status_color(ChipStatus::Done), MUTED);
    }

    #[test]
    fn detail_names_the_edge_and_never_contains_a_newline() {
        let mut kid = chip("kid", ChipRole::Child, ChipStatus::Idle);
        kid.edge = Some(EdgeKind::Team);
        kid.task = Some("line one\nline two".into());
        let input = bar(vec![
            chip("main", ChipRole::Current, ChipStatus::Idle),
            kid.clone(),
        ]);
        let text = detail(&input, &kid);
        assert!(text.contains("child via team"), "{text}");
        assert!(text.contains("line one"));
        assert!(!text.contains("line two"), "only the first line survives");
        // The host's toast is single-line and silently strips newlines, which
        // is how a multi-field report becomes one unreadable run-on.
        assert!(!text.contains('\n'), "{text}");
    }

    #[test]
    fn detail_says_when_a_chip_has_no_pane_to_open() {
        let mut light = chip("lightling", ChipRole::Child, ChipStatus::Done);
        light.tab_id = None;
        light.pane_id = None;
        light.exec = Some("light".into());
        let input = bar(vec![chip("main", ChipRole::Current, ChipStatus::Idle)]);
        let text = detail(&input, &light);
        assert!(text.contains("no pane in this instance"), "{text}");
        assert!(text.contains("exec light"), "{text}");
    }

    #[test]
    fn renders_into_a_buffer_without_panicking_on_a_narrow_area() {
        let input = bar(vec![
            chip("main", ChipRole::Current, ChipStatus::Idle),
            chip("kid", ChipRole::Child, ChipStatus::Busy),
        ]);
        let area = Rect::new(0, 0, 4, 1); // narrower than the content
        let mut buf = Buffer::empty(area);
        render(&input, area, &mut buf);
    }
}
