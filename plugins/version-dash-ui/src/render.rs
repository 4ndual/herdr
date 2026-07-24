//! Guest-side renderer for the version dashboard.
//!
//! Renders one compact line into an off-screen ratatui [`Buffer`] which the
//! shared `flatten_buffer` helper turns into host `Line` nodes. The named
//! ratatui colors map to host theme roles via `guest::color_role`, so the strip
//! matches the active theme.
//!
//! Shapes (all one row):
//!   up to date    `herdr 0.7.3 · latest 0.7.2`            (dim, quiet)
//!   update avail  `herdr 0.7.3 · latest 0.7.5 ⬆`          (latest bold/yellow)
//!   stale/offline `herdr 0.7.3 · latest 0.7.2 (3h)`       (dim + age)
//!   no data yet   `herdr … · latest …`                    (muted placeholder)

use herdr_ui_plugin_api::UiHitRect;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use version_dash_model::VersionRenderInput;

/// Action id for a click anywhere on the strip. Routed back to the guest as a
/// v3 `UiEvent::Action`; the guest answers with a `ShowToast` effect carrying
/// the latest release date + notes URL.
pub const ACTION_OPEN: &str = "version:open";

const MUTED: Color = Color::DarkGray;
const SUBTEXT: Color = Color::Gray;
const UPDATE: Color = Color::Yellow;

/// Placeholder shown when a version field is not yet known.
const UNKNOWN: &str = "\u{2026}"; // …

/// Build the styled one-line strip for `input`. Rendered into a CONTENT-sized
/// buffer (not the incoming panel rect) so the host can measure the natural
/// width and carve the slot — a panel that only rendered when it already had a
/// rect would deadlock (host needs content dims to carve; guest needs a rect to
/// render). Mirrors button-dash, whose grid is likewise rect-independent.
pub fn styled_line(input: &VersionRenderInput) -> Line<'static> {
    let update = input.update_available();
    let current = input.current.as_deref().unwrap_or(UNKNOWN);
    let latest = input.latest.as_deref().unwrap_or(UNKNOWN);

    // Dim the whole line when up to date or offline; the ONE emphasized element
    // is the latest version when an update is actually available.
    let label_style = Style::default().fg(SUBTEXT);
    let current_style = Style::default().fg(if update { Color::White } else { SUBTEXT });
    let latest_style = if update {
        Style::default().fg(UPDATE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED)
    };

    let mut spans = vec![
        Span::styled("herdr ", label_style),
        Span::styled(current.to_string(), current_style),
        Span::styled(" \u{00B7} ", Style::default().fg(MUTED)), // ·
        Span::styled("latest ", label_style),
        Span::styled(latest.to_string(), latest_style),
    ];

    if update {
        // Up-arrow marker signalling an available update.
        spans.push(Span::styled(" \u{2B06}", Style::default().fg(UPDATE).add_modifier(Modifier::BOLD)));
    }

    // Staleness / offline: show the age of the last known value, dim, so a dead
    // collector or offline host is visible instead of a silently frozen strip.
    if input.stale && !input.age_label.is_empty() {
        spans.push(Span::styled(
            format!(" ({})", input.age_label),
            Style::default().fg(MUTED),
        ));
    } else if input.latest.is_none() && input.error.is_some() {
        // Never fetched successfully at all: mark it offline rather than blank.
        spans.push(Span::styled(" (offline)", Style::default().fg(MUTED)));
    }

    Line::from(spans)
}

/// Natural cell width of the strip content, used to size the render buffer so
/// the host measures a real width from the flattened nodes.
pub fn content_width(input: &VersionRenderInput) -> u16 {
    styled_line(input).width() as u16
}

/// Render the strip's line into `area` (a content-sized buffer). Drawing only;
/// the clickable hit rect is built from the real panel rect in `lib.rs`.
pub fn render(input: &VersionRenderInput, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    Paragraph::new(styled_line(input)).render(area, buf);
}

/// The single hit rect covering the panel row, so a click anywhere toasts.
/// Built from the host-carved `panel` rect; empty when not yet carved.
pub fn hit_rect(panel: Rect) -> Vec<UiHitRect> {
    if panel.width == 0 || panel.height == 0 {
        return Vec::new();
    }
    vec![UiHitRect {
        x: panel.x,
        y: panel.y,
        width: panel.width,
        height: 1,
        action_id: ACTION_OPEN.to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_to_string(input: &VersionRenderInput) -> String {
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        render(input, area, &mut buf);
        (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect::<String>()
    }

    #[test]
    fn up_to_date_shows_both_versions_without_arrow() {
        let input = VersionRenderInput {
            current: Some("0.7.3".into()),
            latest: Some("0.7.2".into()),
            ..Default::default()
        };
        let text = render_to_string(&input);
        assert!(text.contains("herdr 0.7.3"));
        assert!(text.contains("latest 0.7.2"));
        assert!(!text.contains('\u{2B06}'), "no update arrow when up to date");
    }

    #[test]
    fn update_available_shows_arrow_and_latest() {
        let input = VersionRenderInput {
            current: Some("0.7.3".into()),
            latest: Some("0.7.5".into()),
            ..Default::default()
        };
        let text = render_to_string(&input);
        assert!(text.contains("latest 0.7.5"));
        assert!(text.contains('\u{2B06}'), "update arrow present");
    }

    #[test]
    fn stale_shows_age_suffix() {
        let input = VersionRenderInput {
            current: Some("0.7.3".into()),
            latest: Some("0.7.2".into()),
            age_label: "3h".into(),
            stale: true,
            ..Default::default()
        };
        let text = render_to_string(&input);
        assert!(text.contains("(3h)"), "stale age suffix present: {text}");
    }

    #[test]
    fn no_data_renders_placeholders_not_blank() {
        let input = VersionRenderInput {
            error: Some("offline".into()),
            ..Default::default()
        };
        let text = render_to_string(&input);
        assert!(text.contains('\u{2026}'), "placeholder shown for unknown version");
        assert!(text.contains("offline"));
    }

    #[test]
    fn hit_rect_covers_the_carved_panel_row() {
        let panel = Rect::new(5, 9, 28, 7);
        let hits = hit_rect(panel);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action_id, ACTION_OPEN);
        assert_eq!((hits[0].x, hits[0].y, hits[0].width, hits[0].height), (5, 9, 28, 1));
    }

    #[test]
    fn hit_rect_empty_before_panel_is_carved() {
        assert!(hit_rect(Rect::new(0, 0, 0, 0)).is_empty());
    }

    #[test]
    fn content_width_is_nonzero_even_with_no_data() {
        // Bootstrap: an empty input must still measure a width so the host can
        // carve the slot on the first frame.
        assert!(content_width(&VersionRenderInput::default()) > 0);
    }
}
