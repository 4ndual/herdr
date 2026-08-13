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

/// Reserved span label for the trailing `latest X.Y.Z` token. Not an instance:
/// clicking it opens the release notes rather than touching any binary. Instance
/// labels come from the collector's fixed set (`hr`, `test`), so the `@` prefix
/// cannot collide with one.
pub const LATEST_LABEL: &str = "@latest";

/// Column range (relative to the line start) an instance token occupies, tagged
/// with its label so the guest can map a click to the right instance's action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpan {
    pub label: String,
    pub start: u16,
    pub width: u16,
}

/// Per-state color. Every value here maps to a host THEME ROLE via
/// `guest::color_role` (green→success, yellow→warning, red→danger, cyan→teal,
/// magenta→mauve, gray→subtext, dark gray→muted), so the strip follows the
/// active theme instead of painting raw ANSI.
fn state_color(state: InstanceState) -> Color {
    match state {
        // Bright green is EARNED: only a sha-bound verification receipt gets it.
        InstanceState::Verified => Color::Green,
        // "The version number looks current" is a weaker claim than "it works",
        // so it renders muted rather than green. This is the whole point of the
        // split: an unbridged upstream binary reports a current version, and it
        // must never be able to paint a reassuring green check.
        InstanceState::UpToDate => SUBTEXT,
        InstanceState::Behind => Color::Yellow,
        // Same red as Broken, on purpose. An ungated binary staged under a
        // live instance is not a milder problem than a broken one — it is a
        // problem that fires on the user's next keystroke.
        InstanceState::Broken | InstanceState::UnsafeRestart => Color::Red,
        InstanceState::Working | InstanceState::AwaitingRestart => Color::Cyan,
        InstanceState::Escalated => Color::Magenta,
        InstanceState::Unknown => MUTED,
    }
}

/// State glyph. Chosen so no two states that can be confused for one another
/// share a shape AND a color; where two states do share a color (cyan: working
/// vs awaiting-restart) they are further separated by their suffix text, which
/// is far more legible than a glyph nuance.
fn state_glyph(state: InstanceState) -> &'static str {
    match state {
        InstanceState::Verified => "\u{2713}", // ✓ verified good
        InstanceState::UpToDate => "\u{2713}", // ✓ but muted — unverified
        InstanceState::Behind => "\u{2191}",   // ↑ an update exists
        InstanceState::Broken => "\u{2717}",   // ✗ broken
        InstanceState::Working => "\u{27F3}",  // ⟳ running right now
        InstanceState::AwaitingRestart => "\u{21BB}", // ↻ restart to finish
        InstanceState::UnsafeRestart => "\u{2691}", // ⚑ staged, ungated
        InstanceState::Escalated => "\u{2691}", // ⚑ an agent has it
        InstanceState::Unknown => "?",
    }
}

/// Whether a state should render bold. `UpToDate` deliberately does not: it is
/// the one "fine, probably" state, and it should not compete visually with a
/// verified check or a real problem.
fn state_bold(state: InstanceState) -> bool {
    !matches!(state, InstanceState::UpToDate | InstanceState::Unknown)
}

/// Suffix appended INSIDE an instance token for the states that would otherwise
/// be ambiguous or silent.
///
/// `Working` carries the elapsed clock because a 90-second silence is
/// indistinguishable from a hang — motion on the strip is what tells the user
/// the pipeline is alive. `AwaitingRestart` carries the literal keystroke
/// because that state is a request addressed to the user, and a glyph cannot ask
/// for anything.
fn state_suffix(inst: &InstanceStatus, pipeline_elapsed: Option<&str>) -> String {
    match inst.state {
        InstanceState::Working => match pipeline_elapsed {
            Some(e) if !e.is_empty() => format!(" ({e})"),
            _ => String::new(),
        },
        InstanceState::AwaitingRestart => " press \u{2303}\u{2325}X".to_string(),
        // Says what to do, not just what is wrong. "staged" alone invites a
        // restart; the point of this state is that a restart is the hazard.
        InstanceState::UnsafeRestart => " staged UNVERIFIED \u{2014} do not restart".to_string(),
        InstanceState::Escalated => " agent".to_string(),
        _ => String::new(),
    }
}

/// Visible text for one instance token: `label version[+N] glyph[ suffix]`.
fn instance_text(inst: &InstanceStatus, pipeline_elapsed: Option<&str>) -> String {
    let version = inst.version.as_deref().unwrap_or(UNKNOWN);
    let changes = match inst.local_changes {
        Some(n) if n > 0 => format!("+{n}"),
        _ => String::new(),
    };
    format!(
        "{} {}{} {}{}",
        inst.label,
        version,
        changes,
        state_glyph(inst.state),
        state_suffix(inst, pipeline_elapsed)
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

    // Elapsed clock for whichever instance the pipeline is currently acting on,
    // so only the working token carries it.
    let running_stage_label = input.pipeline.as_ref().and_then(|p| {
        if p.phase == version_dash_model::PipelinePhase::Running {
            p.stage.map(|s| s.label())
        } else {
            None
        }
    });
    let elapsed = input
        .pipeline
        .as_ref()
        .map(|p| p.elapsed_label.as_str())
        .unwrap_or("");

    let mut col: u16 = 0;
    for (i, inst) in input.instances.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEP, Style::default().fg(MUTED)));
            col += SEP.chars().count() as u16;
        }
        let own_elapsed = if running_stage_label == Some(inst.label.as_str()) {
            Some(elapsed)
        } else {
            None
        };
        let text = instance_text(inst, own_elapsed);
        let width = text.chars().count() as u16;
        // Stale data is DIMMED, not just suffixed. A trailing `(3h)` is easy to
        // miss at the edge of a one-row strip, and the failure it marks is the
        // dangerous one: a dead collector leaves a confident-looking green ✓
        // describing the world as it was hours ago. Dropping the colour and the
        // bold makes the whole strip visibly quiet, so "this is not current"
        // reads at a glance instead of requiring the user to notice a suffix.
        // The glyphs stay, so the last known state is still legible.
        let mut style = if input.stale {
            Style::default().fg(MUTED).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(state_color(inst.state))
        };
        if state_bold(inst.state) && !input.stale {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(text, style));
        ranges.push(InstanceSpan {
            label: inst.label.clone(),
            start: col,
            width,
        });
        col += width;
    }

    // Trailing `· latest X.Y.Z`, dim — the widget's raison d'être (vs latest).
    // It gets its own span range so it is CLICKABLE: clicking the version you
    // are being compared against should show you what is in it. The label is a
    // reserved sentinel (`@latest`), which cannot collide with an instance
    // label because those come from the collector's fixed set (hr / test).
    if let Some(latest) = input.latest.as_deref() {
        spans.push(Span::styled(SEP, Style::default().fg(MUTED)));
        col += SEP.chars().count() as u16;
        let text = format!("latest {latest}");
        let width = text.chars().count() as u16;
        spans.push(Span::styled(text, Style::default().fg(SUBTEXT)));
        ranges.push(InstanceSpan {
            label: LATEST_LABEL.to_string(),
            start: col,
            width,
        });
        col += width;
    }
    let _ = col;

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

    use version_dash_model::{PipelinePhase, PipelineStage, PipelineStatus};

    fn inst(label: &str, ver: &str, changes: Option<u32>, state: InstanceState) -> InstanceStatus {
        InstanceStatus {
            label: label.into(),
            version: Some(ver.into()),
            local_changes: changes,
            state,
            detail: None,
        }
    }

    /// Render one instance in one state and return the visible line.
    fn render_state(state: InstanceState) -> String {
        render_to_string(&VersionRenderInput {
            instances: vec![inst("test", "0.7.5", None, state)],
            latest: Some("0.7.5".into()),
            ..Default::default()
        })
    }

    /// The per-state verification table, asserted rather than described.
    /// Every rendered state the strip can reach gets its glyph pinned here, so a
    /// state can never silently start rendering as another one.
    #[test]
    fn every_state_renders_its_own_glyph() {
        let cases = [
            (InstanceState::Verified, '\u{2713}'),        // ✓ green
            (InstanceState::UpToDate, '\u{2713}'),        // ✓ muted
            (InstanceState::Behind, '\u{2191}'),          // ↑ yellow
            (InstanceState::Broken, '\u{2717}'),          // ✗ red
            (InstanceState::Working, '\u{27F3}'),         // ⟳ cyan
            (InstanceState::AwaitingRestart, '\u{21BB}'), // ↻ cyan
            (InstanceState::Escalated, '\u{2691}'),       // ⚑ magenta
            (InstanceState::Unknown, '?'),                // ? muted
        ];
        for (state, glyph) in cases {
            let text = render_state(state);
            assert!(
                text.contains(glyph),
                "{state:?} must render {glyph:?}, got: {text}"
            );
        }
    }

    #[test]
    fn verified_and_uptodate_share_a_glyph_but_never_a_color() {
        // They intentionally share ✓, so the COLOR carries the distinction and
        // must not collapse: verified is success-green, up-to-date is subtext.
        assert_eq!(state_color(InstanceState::Verified), Color::Green);
        assert_eq!(state_color(InstanceState::UpToDate), SUBTEXT);
        assert_ne!(
            state_color(InstanceState::Verified),
            state_color(InstanceState::UpToDate)
        );
        // …and only the earned one is bold.
        assert!(state_bold(InstanceState::Verified));
        assert!(!state_bold(InstanceState::UpToDate));
    }

    #[test]
    fn no_two_states_share_both_glyph_and_color() {
        let all = [
            InstanceState::Verified,
            InstanceState::UpToDate,
            InstanceState::Behind,
            InstanceState::Broken,
            InstanceState::Working,
            InstanceState::AwaitingRestart,
            InstanceState::Escalated,
            InstanceState::Unknown,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert!(
                    !(state_glyph(*a) == state_glyph(*b) && state_color(*a) == state_color(*b)),
                    "{a:?} and {b:?} are visually identical"
                );
            }
        }
    }

    #[test]
    fn working_token_shows_the_elapsed_clock() {
        // A silent 90-second step is indistinguishable from a hang, so the
        // working token must visibly count.
        let input = VersionRenderInput {
            instances: vec![
                inst("hr", "0.7.3", None, InstanceState::UpToDate),
                inst("test", "0.7.5", None, InstanceState::Working),
            ],
            latest: Some("0.7.5".into()),
            pipeline: Some(PipelineStatus {
                stage: Some(PipelineStage::Test),
                phase: PipelinePhase::Running,
                elapsed_label: "1:07".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let text = render_to_string(&input);
        assert!(text.contains("(1:07)"), "{text}");
    }

    #[test]
    fn the_elapsed_clock_attaches_only_to_the_running_instance() {
        let input = VersionRenderInput {
            instances: vec![
                inst("hr", "0.7.3", None, InstanceState::Working),
                inst("test", "0.7.5", None, InstanceState::Working),
            ],
            latest: Some("0.7.5".into()),
            pipeline: Some(PipelineStatus {
                stage: Some(PipelineStage::Hr),
                phase: PipelinePhase::Running,
                elapsed_label: "0:09".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let text = render_to_string(&input);
        assert_eq!(text.matches("(0:09)").count(), 1, "{text}");
    }

    #[test]
    fn awaiting_restart_asks_for_the_keystroke() {
        // This state is a request addressed to the user; a glyph cannot ask.
        let text = render_state(InstanceState::AwaitingRestart);
        assert!(text.contains("press \u{2303}\u{2325}X"), "{text}");
    }

    #[test]
    fn escalated_says_an_agent_has_it() {
        assert!(render_state(InstanceState::Escalated).contains("agent"));
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
                    detail: None,
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

    /// The clickable `latest` token must cover exactly the text a user sees,
    /// or the click lands on the separator and nothing happens.
    #[test]
    fn latest_token_gets_its_own_span_covering_its_text() {
        let input = VersionRenderInput {
            instances: vec![inst("hr", "0.7.3", Some(107), InstanceState::UpToDate)],
            latest: Some("0.7.5".into()),
            ..Default::default()
        };
        let (line, ranges) = build(&input);
        let text: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .concat();
        let span = ranges
            .iter()
            .find(|r| r.label == LATEST_LABEL)
            .expect("latest span");
        let covered: String = text
            .chars()
            .skip(span.start as usize)
            .take(span.width as usize)
            .collect();
        assert_eq!(covered, "latest 0.7.5", "full text: {text}");
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
        // Two instances + the clickable `latest` token.
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[2].label, LATEST_LABEL);
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

    #[test]
    fn stale_dims_the_instance_tokens() {
        // A dead collector must not keep painting a confident coloured state.
        let fresh = VersionRenderInput {
            instances: vec![inst("hr", "0.7.3", Some(107), InstanceState::Verified)],
            latest: Some("0.7.5".into()),
            ..Default::default()
        };
        let stale = VersionRenderInput {
            age_label: "3h".into(),
            stale: true,
            ..fresh.clone()
        };
        let style_of = |i: &VersionRenderInput| {
            build(i)
                .0
                .spans
                .iter()
                .find(|s| s.content.starts_with("hr "))
                .map(|s| s.style)
                .expect("hr token span")
        };
        let fresh_style = style_of(&fresh);
        let stale_style = style_of(&stale);
        assert_eq!(fresh_style.fg, Some(state_color(InstanceState::Verified)));
        assert!(fresh_style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(stale_style.fg, Some(MUTED));
        assert!(stale_style.add_modifier.contains(Modifier::DIM));
        assert!(!stale_style.add_modifier.contains(Modifier::BOLD));
    }
}
