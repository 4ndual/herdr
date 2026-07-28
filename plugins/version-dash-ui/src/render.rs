//! Guest-side renderer for the version dashboard.
//!
//! Renders ONE compact line into an off-screen ratatui [`Buffer`] which the
//! shared `flatten_buffer` helper turns into host `Line` nodes. Named ratatui
//! colors map to host theme roles via `guest::color_role`, so the strip matches
//! the active theme.
//!
//! Layout (kept LEFT-compact so the detached-sessions plugin can cohabit the
//! row to its right):
//!   `hr 0.7.3+107 ↑ · test 0.7.5 ✓ · latest 0.7.5`
//! Per-instance color: green=up to date, yellow=behind latest, red=broken,
//! dim=unknown. A trailing `(age)` appears when the data is stale/offline.
//!
//! [`instance_spans`] returns one [`InstanceSpan`] per instance giving its
//! column range in the line, so `lib.rs` can offset absolute hit-rects and route
//! left-click (toast) / right-click (update pipeline) to the right instance.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use version_dash_model::{InstanceState, InstanceStatus, VersionRenderInput};

const MUTED: Color = Color::DarkGray;
const SUBTEXT: Color = Color::Gray;

/// Placeholder shown when a version field is not yet known.
const UNKNOWN: &str = "\u{2026}"; // …

/// Separator between tokens: " · ".
const SEP: &str = " \u{00B7} ";

/// Column range (relative to the line start) an instance token occupies, tagged
/// with its label so the guest can map a click to the right instance's action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpan {
    pub label: String,
    pub start: u16,
    pub width: u16,
}

fn state_color(state: InstanceState) -> Color {
    match state {
        InstanceState::UpToDate => Color::Green,
        InstanceState::Behind => Color::Yellow,
        InstanceState::Broken => Color::Red,
        InstanceState::Unknown => MUTED,
    }
}

/// State glyph: `✓` up to date, `↑` behind, `✗` broken, `?` unknown.
fn state_glyph(state: InstanceState) -> &'static str {
    match state {
        InstanceState::UpToDate => "\u{2713}", // ✓
        InstanceState::Behind => "\u{2191}",   // ↑
        InstanceState::Broken => "\u{2717}",   // ✗
        InstanceState::Unknown => "?",
    }
}

/// Visible text for one instance token: `label version[+N] glyph`.
fn instance_text(inst: &InstanceStatus) -> String {
    let version = inst.version.as_deref().unwrap_or(UNKNOWN);
    let changes = match inst.local_changes {
        Some(n) if n > 0 => format!("+{n}"),
        _ => String::new(),
    };
    format!(
        "{} {}{} {}",
        inst.label,
        version,
        changes,
        state_glyph(inst.state)
    )
}

/// Build the styled one-line strip plus the per-instance column ranges. Rendered
/// into a CONTENT-sized buffer (not the incoming panel rect) so the host can
/// measure the natural width and carve the slot — a panel that only rendered when
/// it already had a rect would deadlock (host needs content dims to carve; guest
/// needs a rect to render). Mirrors button-dash, whose grid is rect-independent.
pub fn build(input: &VersionRenderInput) -> (Line<'static>, Vec<InstanceSpan>) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut ranges: Vec<InstanceSpan> = Vec::new();

    if input.instances.is_empty() {
        // Bootstrap / no data yet: a muted placeholder so the host still measures
        // a width and carves the slot on the first frame.
        spans.push(Span::styled(
            format!("hr {UNKNOWN}"),
            Style::default().fg(MUTED),
        ));
        return (Line::from(spans), ranges);
    }

    let mut col: u16 = 0;
    for (i, inst) in input.instances.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEP, Style::default().fg(MUTED)));
            col += SEP.chars().count() as u16;
        }
        let text = instance_text(inst);
        let width = text.chars().count() as u16;
        let style = Style::default()
            .fg(state_color(inst.state))
            .add_modifier(Modifier::BOLD);
        spans.push(Span::styled(text, style));
        ranges.push(InstanceSpan {
            label: inst.label.clone(),
            start: col,
            width,
        });
        col += width;
    }

    // Trailing `· latest X.Y.Z`, dim — the widget's raison d'être (vs latest).
    if let Some(latest) = input.latest.as_deref() {
        spans.push(Span::styled(SEP, Style::default().fg(MUTED)));
        spans.push(Span::styled("latest ", Style::default().fg(SUBTEXT)));
        spans.push(Span::styled(
            latest.to_string(),
            Style::default().fg(SUBTEXT),
        ));
    }

    // Stale/offline: show the age of the last known value so a dead collector is
    // visible instead of a silently frozen strip.
    if input.stale && !input.age_label.is_empty() {
        spans.push(Span::styled(
            format!(" ({})", input.age_label),
            Style::default().fg(MUTED),
        ));
    } else if input.latest.is_none() && input.error.is_some() {
        spans.push(Span::styled(" (offline)", Style::default().fg(MUTED)));
    }

    (Line::from(spans), ranges)
}

/// Natural cell width of the strip content.
pub fn content_width(input: &VersionRenderInput) -> u16 {
    build(input).0.width() as u16
}

/// Render the strip into `area` (a content-sized buffer) and return the
/// per-instance column ranges (relative to the line start).
pub fn render(input: &VersionRenderInput, area: Rect, buf: &mut Buffer) -> Vec<InstanceSpan> {
    let (line, ranges) = build(input);
    if area.width == 0 || area.height == 0 {
        return ranges;
    }
    Paragraph::new(line).render(area, buf);
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(label: &str, ver: &str, changes: Option<u32>, state: InstanceState) -> InstanceStatus {
        InstanceStatus {
            label: label.into(),
            version: Some(ver.into()),
            local_changes: changes,
            state,
        }
    }

    fn render_to_string(input: &VersionRenderInput) -> String {
        let w = content_width(input).max(1);
        let area = Rect::new(0, 0, w, 1);
        let mut buf = Buffer::empty(area);
        render(input, area, &mut buf);
        (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect::<String>()
    }

    #[test]
    fn renders_two_instances_with_labels_and_glyphs() {
        let input = VersionRenderInput {
            instances: vec![
                inst("hr", "0.7.3", Some(107), InstanceState::Behind),
                inst("test", "0.7.5", Some(0), InstanceState::UpToDate),
            ],
            latest: Some("0.7.5".into()),
            ..Default::default()
        };
        let text = render_to_string(&input);
        assert!(text.contains("hr 0.7.3+107"), "{text}");
        assert!(text.contains("test 0.7.5"), "{text}");
        assert!(text.contains("latest 0.7.5"), "{text}");
        assert!(text.contains('\u{2191}'), "behind glyph ↑: {text}");
        assert!(text.contains('\u{2713}'), "uptodate glyph ✓: {text}");
    }

    #[test]
    fn broken_instance_shows_broken_glyph_and_placeholder() {
        let input = VersionRenderInput {
            instances: vec![
                inst("hr", "0.7.3", Some(107), InstanceState::Behind),
                InstanceStatus {
                    label: "test".into(),
                    version: None,
                    local_changes: None,
                    state: InstanceState::Broken,
                },
            ],
            latest: Some("0.7.5".into()),
            ..Default::default()
        };
        let text = render_to_string(&input);
        assert!(text.contains('\u{2717}'), "broken glyph ✗: {text}");
        assert!(
            text.contains('\u{2026}'),
            "unknown version placeholder: {text}"
        );
    }

    #[test]
    fn instance_ranges_map_to_token_positions() {
        let input = VersionRenderInput {
            instances: vec![
                inst("hr", "0.7.3", Some(107), InstanceState::Behind),
                inst("test", "0.7.5", None, InstanceState::UpToDate),
            ],
            latest: Some("0.7.5".into()),
            ..Default::default()
        };
        let (_, ranges) = build(&input);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].label, "hr");
        assert_eq!(ranges[0].start, 0);
        assert_eq!(
            ranges[0].width,
            "hr 0.7.3+107 \u{2191}".chars().count() as u16
        );
        // Second token starts after the first token + " · " separator.
        assert_eq!(ranges[1].label, "test");
        assert_eq!(
            ranges[1].start,
            ranges[0].width + SEP.chars().count() as u16
        );
    }

    #[test]
    fn no_zero_change_suffix() {
        let input = VersionRenderInput {
            instances: vec![inst("test", "0.7.5", Some(0), InstanceState::UpToDate)],
            ..Default::default()
        };
        assert!(!render_to_string(&input).contains("+0"));
    }

    #[test]
    fn empty_instances_still_measures_width() {
        assert!(content_width(&VersionRenderInput::default()) > 0);
    }

    #[test]
    fn stale_shows_age_suffix() {
        let input = VersionRenderInput {
            instances: vec![inst("hr", "0.7.3", Some(107), InstanceState::Behind)],
            latest: Some("0.7.5".into()),
            age_label: "3h".into(),
            stale: true,
            ..Default::default()
        };
        assert!(render_to_string(&input).contains("(3h)"));
    }
}
