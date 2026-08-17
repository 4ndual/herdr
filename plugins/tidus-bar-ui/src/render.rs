//! Guest-side renderer for the tidus subagent bar.
//!
//! One or two compact bottom rows into an off-screen ratatui [`Buffer`], which
//! the shared `flatten_buffer` helper turns into host `Line` nodes. Named
//! ratatui colors map to host theme roles via `guest::color_role`, so the bar
//! follows the active theme instead of painting raw ANSI.
//!
//! Shape — ancestors pinned left, then the SELECTED node carrying the selected
//! indicator, then its brothers; the selected node's own children go on a
//! second row, indented to start underneath it:
//!
//! ```text
//! ⑂ main │ ▌squad▐ ● reviewer ○ writer
//!         └ ◆ builder ○ scribe
//! ```
//!
//! Two rules make the shape stable under navigation:
//!
//!   * the brother set belongs to the PARENT, so clicking a brother re-roots
//!     the bar on it without the row you clicked from disappearing;
//!   * the second row exists only when the selected node actually has
//!     children, so the band costs one line at rest and two only while there
//!     is something to put on the second.
//!
//! Every chip is its own click target, so the bar is a grid of buttons rather
//! than a line with a meaning. Left-click navigates (the terminal area swaps to
//! that agent, and the bar re-roots on it); right-click opens the chip menu.

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
/// Brackets around the SELECTED node. Half-block glyphs rather than `[ ]`: they
/// read as a filled cartouche around the name at terminal resolution, which is
/// what makes "this one is selected" answerable without comparing colours.
const HERE_OPEN: &str = "\u{258C}"; // ▌
const HERE_CLOSE: &str = "\u{2590}"; // ▐
/// Elbow that opens the child row, drawn in the selected chip's own column so
/// the row below is visibly hanging off the selected chip and not off the
/// lineage or off a brother.
const BRANCH: &str = "\u{2514} "; // "└ "

// The bar publishes NO hit-rects any more: hover needs the raw mouse stream and
// the host suppresses that over a module's own rects, so clicks are resolved
// from [`layout`] instead. The old "tidus-bar:chip:<n>" action id is deliberately
// gone rather than kept as a reserved constant — an id nothing can dispatch is
// how a dead click target survives a refactor.

/// One laid-out chip: where it sits in the grid the bar drew, so the guest can
/// resolve a press against the exact cells it painted.
pub struct Placed {
    pub index: usize,
    /// Which row of the bar the chip is on: 0 is the lineage/brother row, 1 is
    /// the child row. Carried explicitly because a two-row bar can place two
    /// different chips at the same column.
    pub row: u16,
    /// Column offset from the start of the row.
    pub x: u16,
    pub width: u16,
}

/// Build the bar's rows and the placement of every chip in them.
///
/// Returns `None` when there is nothing worth a row at all — the guest then
/// emits no layer and the host carves no band, rather than reserving a line to
/// display emptiness.
///
/// Row 0 is always present: `⑂ <ancestors> │ <selected> <brothers…>`. Row 1
/// exists only when the selected node has children, and starts with the elbow
/// in the selected chip's own column, so the band is one line tall at rest and
/// two only while there is a child row to justify the second.
pub fn layout(input: &BarInput) -> Option<(Vec<Line<'static>>, Vec<Placed>)> {
    if input.is_empty() {
        return None;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut placed: Vec<Placed> = Vec::new();
    let mut x: u16 = 0;
    // Column the selected chip starts at, which the child row indents to.
    let mut selected_x: u16 = 0;

    push(
        &mut spans,
        &mut x,
        format!("{ICON} "),
        Style::default().fg(SUBTEXT),
    );

    let mut first_ancestor = true;
    for (index, chip) in input.chips.iter().enumerate() {
        if chip.role != ChipRole::Ancestor {
            continue;
        }
        if !first_ancestor {
            push(
                &mut spans,
                &mut x,
                DESCEND.to_string(),
                Style::default().fg(MUTED),
            );
        }
        first_ancestor = false;
        let start = x;
        for (text, style) in ancestor_spans(chip) {
            push(&mut spans, &mut x, text, style);
        }
        placed.push(Placed {
            index,
            row: 0,
            x: start,
            width: x - start,
        });
    }

    // The divider separates "how I got here" from "what is here". With no
    // ancestors there is nothing on its left, and a leading bare divider would
    // claim a lineage the bar does not have.
    if !first_ancestor {
        push(
            &mut spans,
            &mut x,
            DIVIDER.to_string(),
            Style::default().fg(MUTED),
        );
    }

    // The selected node FIRST, then its brothers — spec order, and the reason
    // the child row's indent is knowable before the brothers are laid out.
    let mut any_peer = false;
    for (index, chip) in input.chips.iter().enumerate() {
        let selected = match chip.role {
            ChipRole::Current => true,
            ChipRole::Sibling => false,
            _ => continue,
        };
        if any_peer {
            push(&mut spans, &mut x, " ".to_string(), Style::default());
        }
        any_peer = true;
        let start = x;
        if selected {
            selected_x = start;
        }
        let cells = if selected {
            selected_spans(chip)
        } else {
            child_spans(chip)
        };
        for (text, style) in cells {
            push(&mut spans, &mut x, text, style);
        }
        placed.push(Placed {
            index,
            row: 0,
            x: start,
            width: x - start,
        });
    }

    let mut lines = vec![Line::from(spans)];

    // Row 1: the selected node's children, hanging off its own column. Built
    // only when there is at least one — an empty second row would cost the
    // operator a line of pane height to display nothing.
    let mut kids: Vec<Span<'static>> = Vec::new();
    let mut kx: u16 = 0;
    let mut any_child = false;
    for (index, chip) in input.chips.iter().enumerate() {
        if chip.role != ChipRole::Child {
            continue;
        }
        if !any_child {
            push(
                &mut kids,
                &mut kx,
                " ".repeat(selected_x as usize),
                Style::default(),
            );
            push(
                &mut kids,
                &mut kx,
                BRANCH.to_string(),
                Style::default().fg(MUTED),
            );
        } else {
            push(&mut kids, &mut kx, " ".to_string(), Style::default());
        }
        any_child = true;
        let start = kx;
        for (text, style) in child_spans(chip) {
            push(&mut kids, &mut kx, text, style);
        }
        placed.push(Placed {
            index,
            row: 1,
            x: start,
            width: kx - start,
        });
    }
    if any_child {
        lines.push(Line::from(kids));
    }

    Some((lines, placed))
}

/// Ancestors render as plain names: they are the path back up, not the subject.
fn ancestor_spans(chip: &Chip) -> Vec<(String, Style)> {
    vec![
        (
            format!("{} ", chip.status.dot()),
            Style::default().fg(status_color(chip.status)),
        ),
        (chip.name.clone(), Style::default().fg(SUBTEXT)),
    ]
}

/// The selected chip. It sits shoulder to shoulder with its brothers, which are
/// drawn as ordinary chips, so the selection has to be readable from the chip
/// itself rather than from its position: bracketed, in the "you are here"
/// colour, bold AND underlined. Bold alone is not enough — several brother
/// states are already coloured, and `flatten_buffer` carries only bold /
/// underline / italic to the host, so a reverse-video marker would be dropped
/// on the way to the screen.
fn selected_spans(chip: &Chip) -> Vec<(String, Style)> {
    vec![
        (HERE_OPEN.to_string(), Style::default().fg(CURRENT)),
        (
            chip.name.clone(),
            Style::default()
                .fg(CURRENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ),
        (HERE_CLOSE.to_string(), Style::default().fg(CURRENT)),
    ]
}

/// An ordinary agent chip — a brother on row 0 or a child on row 1: status dot,
/// name, and the job tail when there is one to disambiguate two dispatches to
/// the same agent (brief §7). Brothers and children are drawn identically on
/// purpose: they are the same kind of thing (a click target that re-roots the
/// bar), and only the selected chip is meant to look different.
fn child_spans(chip: &Chip) -> Vec<(String, Style)> {
    let color = status_color(chip.status);
    let mut name_style = Style::default().fg(color);
    if chip.status.is_alarming() {
        name_style = name_style.add_modifier(Modifier::BOLD);
    }
    if chip.status.is_dormant() || chip.finished {
        // Reaped children, and children whose dispatch is over and are being
        // held through their retention grace, stay attached but must not
        // compete with live ones for attention.
        //
        // `finished` is checked separately from `is_dormant`: a child can end a
        // job and still be a running process — a supervised harness goes back
        // to `idle`, not `stopped` — so status alone would leave a finished
        // dispatch rendered as brightly as a live one.
        name_style = Style::default().fg(MUTED);
    }
    let dot_style = if chip.finished {
        // The dot carries most of the row's colour, so leaving it bright beside
        // a greyed name would keep reading as "live".
        Style::default().fg(MUTED)
    } else {
        Style::default().fg(color)
    };
    let mut out = vec![
        (format!("{} ", chip.status.dot()), dot_style),
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

/// Natural width of the widest row, so the host can carve the slot on the first
/// content frame (before which `ctx.rect` is still zero).
pub fn content_width(input: &BarInput) -> u16 {
    layout(input)
        .map(|(lines, _)| {
            lines
                .iter()
                .map(|line| line.spans.iter().map(|s| width_of(&s.content)).sum())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Natural height in rows: 1, or 2 while the selected node has children. The
/// manifest sizes this band `auto`, so this number is literally how many rows
/// of pane height the bar takes — which is why the child row is built only when
/// it has something on it.
pub fn content_height(input: &BarInput) -> u16 {
    layout(input)
        .map(|(lines, _)| lines.len() as u16)
        .unwrap_or(0)
}

pub fn render(input: &BarInput, area: Rect, buf: &mut Buffer) {
    let Some((lines, _)) = layout(input) else {
        return;
    };
    Paragraph::new(lines).render(area, buf);
}

/// Item labels for the right-click chip menu, in display order. Shared by the
/// renderer (draws them) and the guest's click handler (matches a press
/// against the same order), so the two can never disagree about which label
/// sits where.
pub const MENU_ITEMS: [&str; 3] = ["Copy metadata", "Close", "Cancel"];

const MENU_PREFIX: &str = "\u{2261} "; // ≡ — distinguishes the menu row from the lineage icon (⑂)
const MENU_SEP: &str = "   ";

/// Build the menu row for a right-click on `chip_name`, and the click
/// placement of each item — same shape as [`layout`]/[`Placed`], so the
/// guest resolves a menu press with the identical "click lands on what was
/// drawn" pattern it already uses for chips.
///
/// The menu borrows the bar's own one-row band rather than floating a popup
/// over pane content: this module has no declared claim on any surface but
/// `panel`, and there is no host-mediated way for a plugin's own panel to pop
/// a bordered overlay the way the built-in tab/pane menus do.
pub fn menu_layout(chip_name: &str) -> (Line<'static>, Vec<Placed>) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut placed: Vec<Placed> = Vec::new();
    let mut x: u16 = 0;

    push(
        &mut spans,
        &mut x,
        MENU_PREFIX.to_string(),
        Style::default().fg(SUBTEXT),
    );
    push(
        &mut spans,
        &mut x,
        format!("{chip_name}  "),
        Style::default().fg(CURRENT).add_modifier(Modifier::BOLD),
    );

    for (index, label) in MENU_ITEMS.iter().enumerate() {
        if index > 0 {
            push(
                &mut spans,
                &mut x,
                MENU_SEP.to_string(),
                Style::default().fg(MUTED),
            );
        }
        let start = x;
        let style = if *label == "Close" {
            Style::default().fg(FAILED)
        } else {
            Style::default().fg(SUBTEXT)
        };
        push(&mut spans, &mut x, format!("[{label}]"), style);
        placed.push(Placed {
            index,
            // The menu borrows the bar's own band and always draws as ONE row,
            // so every item is on row 0 regardless of how tall the chip grid
            // was before it opened.
            row: 0,
            x: start,
            width: x - start,
        });
    }

    (Line::from(spans), placed)
}

pub fn menu_content_width(chip_name: &str) -> u16 {
    let (line, _) = menu_layout(chip_name);
    line.spans.iter().map(|s| width_of(&s.content)).sum()
}

pub fn render_menu(chip_name: &str, area: Rect, buf: &mut Buffer) {
    let (line, _) = menu_layout(chip_name);
    Paragraph::new(line).render(area, buf);
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
            finished: false,
            folder: None,
        }
    }

    fn bar(chips: Vec<Chip>) -> BarInput {
        BarInput {
            chips,
            empty_reason: None,
            ..Default::default()
        }
    }

    /// The text of one row, by index.
    fn row_text(input: &BarInput, row: usize) -> String {
        let (lines, _) = layout(input).expect("a row");
        lines
            .get(row)
            .unwrap_or_else(|| panic!("row {row} of {} was not drawn", lines.len()))
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect()
    }

    /// Every row, joined — for assertions that only care that a token reached
    /// the screen at all.
    fn text_of(input: &BarInput) -> String {
        let (lines, _) = layout(input).expect("a row");
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The full shape under test: a lineage, the selected node, two brothers
    /// and two children of the selected node.
    fn family() -> BarInput {
        bar(vec![
            chip("main", ChipRole::Ancestor, ChipStatus::Idle),
            chip("squad", ChipRole::Current, ChipStatus::Busy),
            chip("bro-a", ChipRole::Sibling, ChipStatus::Busy),
            chip("bro-b", ChipRole::Sibling, ChipStatus::Idle),
            chip("kid-a", ChipRole::Child, ChipStatus::Busy),
            chip("kid-b", ChipRole::Child, ChipStatus::Idle),
        ])
    }

    #[test]
    fn an_empty_bar_draws_no_row_at_all() {
        assert!(layout(&BarInput::default()).is_none());
        assert_eq!(content_width(&BarInput::default()), 0);
        assert_eq!(content_height(&BarInput::default()), 0);
    }

    /// Spec (3): the selected node comes FIRST after its parent, and the
    /// brothers sit next to it — not before it, and not on another row.
    #[test]
    fn the_selected_node_leads_its_brothers_right_after_the_lineage() {
        let top = row_text(&family(), 0);
        let main = top.find("main").expect("ancestor");
        let squad = top.find("squad").expect("selected");
        let a = top.find("bro-a").expect("brother a");
        let b = top.find("bro-b").expect("brother b");
        assert!(main < squad && squad < a && a < b, "row order: {top}");
        assert!(top.contains(DIVIDER), "lineage and family are separated");
        // Spec (1): the brothers are on the bar at all. This is the whole
        // regression — selecting a child used to replace the row with that
        // child's own children and take its brothers with it.
        assert!(!top.contains("kid-a"), "children belong on row 1: {top}");
    }

    /// Spec (2): the selected chip is distinguishable from a brother by its own
    /// drawing, not by where it happens to sit.
    #[test]
    fn the_selected_chip_carries_a_visible_selected_indicator() {
        let input = family();
        let top = row_text(&input, 0);
        assert!(
            top.contains(&format!("{HERE_OPEN}squad{HERE_CLOSE}")),
            "{top}"
        );
        // ...and the style survives to the host, which only carries bold /
        // underline / italic.
        let (lines, _) = layout(&input).expect("a row");
        let name = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "squad")
            .expect("the selected name is its own span");
        assert!(name.style.add_modifier.contains(Modifier::BOLD), "bold");
        assert!(
            name.style.add_modifier.contains(Modifier::UNDERLINED),
            "underlined"
        );
        // A brother must NOT wear it, or "selected" means nothing.
        let bro = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "bro-a")
            .expect("a brother is its own span");
        assert!(!bro.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    /// Spec (4): children go on a row UNDER the selected chip, and the band is
    /// only two rows tall when there are children to put on the second.
    #[test]
    fn children_get_a_second_row_indented_under_the_selected_chip() {
        let input = family();
        assert_eq!(content_height(&input), 2, "a child row was earned");
        let (lines, placed) = layout(&input).expect("rows");
        assert_eq!(lines.len(), 2);

        let bottom = row_text(&input, 1);
        assert!(bottom.contains("kid-a") && bottom.contains("kid-b"), "{bottom}");
        assert!(!bottom.contains("bro-a"), "brothers stay on row 0: {bottom}");

        // The elbow sits in the selected chip's own column.
        let selected = placed
            .iter()
            .find(|p| p.index == 1)
            .expect("the selected chip is placed");
        assert_eq!(selected.row, 0);
        let elbow = bottom.find(BRANCH.trim()).expect("an elbow");
        assert_eq!(
            elbow as u16, selected.x,
            "the child row must hang off the selected chip: {bottom}"
        );

        for kid in placed.iter().filter(|p| p.index >= 4) {
            assert_eq!(kid.row, 1, "children are on the second row");
            assert!(kid.x >= selected.x, "and start at or past the elbow");
        }
    }

    #[test]
    fn a_selection_with_no_children_stays_one_row_tall() {
        let input = bar(vec![
            chip("main", ChipRole::Ancestor, ChipStatus::Idle),
            chip("squad", ChipRole::Current, ChipStatus::Busy),
            chip("bro-a", ChipRole::Sibling, ChipStatus::Busy),
        ]);
        assert_eq!(
            content_height(&input),
            1,
            "an empty second row would cost a line of pane height for nothing"
        );
    }

    #[test]
    fn a_lone_orchestrator_renders_no_bare_divider() {
        let input = bar(vec![chip("main", ChipRole::Current, ChipStatus::Idle)]);
        let text = text_of(&input);
        assert!(
            !text.contains(DIVIDER),
            "nothing sits left of the divider: {text}"
        );
        assert!(text.contains(&format!("{HERE_OPEN}main{HERE_CLOSE}")), "{text}");
        assert_eq!(content_height(&input), 1);
    }

    #[test]
    fn every_chip_gets_its_own_hit_placement_and_they_do_not_overlap() {
        let input = family();
        let (_, placed) = layout(&input).expect("a row");
        assert_eq!(placed.len(), 6, "one placement per chip");
        // Indices must address the ORIGINAL chip vector, or a click focuses the
        // wrong agent — the exact class of bug that has no error message.
        assert_eq!(
            placed.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5]
        );
        // Overlap is only meaningful WITHIN a row: two rows may legitimately
        // draw different chips at the same column, which is the entire point of
        // indenting the child row under the selected chip.
        for row in [0u16, 1] {
            let on_row: Vec<&Placed> = placed.iter().filter(|p| p.row == row).collect();
            for pair in on_row.windows(2) {
                assert!(
                    pair[0].x + pair[0].width <= pair[1].x,
                    "chips overlap on row {row}: {}..{}",
                    pair[0].x,
                    pair[1].x
                );
            }
            for p in on_row {
                assert!(p.width > 0);
            }
        }
    }

    #[test]
    fn placement_matches_where_the_name_actually_lands() {
        let input = family();
        let (_, placed) = layout(&input).expect("a row");
        for (index, name) in [(2usize, "bro-a"), (4, "kid-a")] {
            let p = placed
                .iter()
                .find(|p| p.index == index)
                .expect("chip placed");
            let text = row_text(&input, p.row as usize);
            // The chip starts at its status dot, so the name sits just inside
            // the rect; this pins the arithmetic the hit test depends on.
            let at = text.chars().collect::<Vec<_>>()[p.x as usize..]
                .iter()
                .collect::<String>();
            assert!(at.starts_with(ChipStatus::Busy.dot()), "{name}: {at}");
            assert!(at.contains(name), "{name}: {at}");
        }
    }

    #[test]
    fn content_width_counts_the_widest_row() {
        let input = family();
        let widest = text_of(&input)
            .lines()
            .map(|l| l.chars().count() as u16)
            .max()
            .expect("rows");
        assert_eq!(content_width(&input), widest);
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
    fn renders_into_a_buffer_without_panicking_on_a_narrow_area() {
        let input = bar(vec![
            chip("main", ChipRole::Current, ChipStatus::Idle),
            chip("kid", ChipRole::Child, ChipStatus::Busy),
        ]);
        let area = Rect::new(0, 0, 4, 1); // narrower than the content
        let mut buf = Buffer::empty(area);
        render(&input, area, &mut buf);
    }

    #[test]
    fn the_menu_row_places_every_item_without_overlap() {
        let (_, placed) = menu_layout("kid");
        assert_eq!(placed.len(), MENU_ITEMS.len());
        assert_eq!(
            placed.iter().map(|p| p.index).collect::<Vec<_>>(),
            (0..MENU_ITEMS.len()).collect::<Vec<_>>()
        );
        for pair in placed.windows(2) {
            assert!(pair[0].x + pair[0].width <= pair[1].x, "menu items overlap");
        }
    }

    #[test]
    fn the_menu_row_names_the_chip_it_was_opened_for() {
        let (line, _) = menu_layout("my-kid");
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("my-kid"), "{text}");
        for label in MENU_ITEMS {
            assert!(text.contains(label), "{text} missing {label}");
        }
    }
}
