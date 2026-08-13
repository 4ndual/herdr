//! Guest-side renderer for the harness indicator.
//!
//! One compact line into an off-screen ratatui [`Buffer`], which the shared
//! `flatten_buffer` helper turns into host `Line` nodes. Named ratatui colors
//! map to host theme roles via `guest::color_role`, so the strip follows the
//! active theme instead of painting raw ANSI.
//!
//! Shapes, one per [`PaneState`]:
//!
//! ```text
//! ⛺ e2e-claude · claude · ~/harnesses/e2e-claude/workspace
//! ⛺ e2e-codex ⚠ codex≠claude · claude · ~/harnesses/e2e-codex/workspace
//! ⛺ no harness · claude · ~/Projects/herdr
//! ⛺ shell · ~/Projects/herdr
//! ```
//!
//! When the agent's folder and the pane's shell folder differ — `(cd x && claude)`,
//! or a shell that moved after launch — the shell's is appended after `→`, so
//! "where the work happens" and "where your prompt is" are never conflated.

use harness_indicator_model::{tilde, IndicatorInput, PaneState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

const MUTED: Color = Color::DarkGray;
const SUBTEXT: Color = Color::Gray;

/// Leading glyph. A tent: the harness you are camped in.
const ICON: &str = "\u{26FA}"; // ⛺
/// Separator between tokens.
const SEP: &str = " \u{00B7} "; // " · "
/// Divergence arrow: agent folder → shell folder.
const MOVED: &str = " \u{2192} "; // " → "
/// Mismatch marker: the harness declares one runtime, the pane runs another.
const WARN: &str = "\u{26A0}"; // ⚠
/// Prefix on a harness name that was inferred from a transcript rather than
/// observed in the process environment. A weaker claim must not look identical
/// to a stronger one.
const INFERRED: &str = "\u{2248}"; // ≈

/// The strip's single hit target, so a click can show the full attribution.
pub const DETAIL_ACTION: &str = "harness-indicator:detail";

/// Build the line. Returns `None` when there is nothing worth a row at all —
/// the guest then emits no layer and the host carves no band, rather than
/// reserving a row to display emptiness.
pub fn line(input: &IndicatorInput) -> Option<Line<'static>> {
    let home = input.home.as_deref().unwrap_or_default();
    let mut spans = vec![Span::styled(
        format!("{ICON} "),
        Style::default().fg(SUBTEXT),
    )];

    match input.state() {
        PaneState::Empty => return None,
        PaneState::Shell => {
            spans.push(Span::styled("shell", Style::default().fg(MUTED)));
        }
        PaneState::NoHarness => {
            // Deliberately not styled as a problem: running on the default
            // roots is the normal case for a pane you started yourself.
            spans.push(Span::styled("no harness", Style::default().fg(MUTED)));
            spans.push(Span::styled(SEP, Style::default().fg(MUTED)));
            spans.push(agent_span(input));
        }
        PaneState::InHarness => {
            let harness = input.harness.as_ref()?;
            let inferred = harness.source == harness_indicator_model::HarnessSource::Transcript;
            let name = if inferred {
                format!("{INFERRED}{}", harness.name)
            } else {
                harness.name.clone()
            };
            spans.push(Span::styled(
                name,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            if harness.profile {
                spans.push(Span::styled(" profile", Style::default().fg(MUTED)));
            }
            if let Some((declared, detected)) = input.mismatch() {
                spans.push(Span::styled(
                    format!(" {WARN} {declared}\u{2260}{detected}"),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(SEP, Style::default().fg(MUTED)));
            spans.push(agent_span(input));
        }
    }

    let (agent_folder, shell_folder) = input.folders();
    if let Some(agent_folder) = agent_folder {
        spans.push(Span::styled(SEP, Style::default().fg(MUTED)));
        spans.push(Span::styled(
            tilde(agent_folder, home),
            Style::default().fg(SUBTEXT),
        ));
        // Yellow, not red: a shell somewhere else is a fact worth seeing, not a
        // fault. It is how `(cd x && claude)` looks, and that is a valid launch.
        if let Some(shell_folder) = shell_folder {
            spans.push(Span::styled(MOVED, Style::default().fg(Color::Yellow)));
            spans.push(Span::styled(
                tilde(shell_folder, home),
                Style::default().fg(Color::Yellow),
            ));
        }
    }

    if let Some(note) = &input.note {
        spans.push(Span::styled(
            format!("{SEP}{note}"),
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ));
    }

    Some(Line::from(spans))
}

fn agent_span(input: &IndicatorInput) -> Span<'static> {
    match input.agent.as_deref() {
        Some(agent) => Span::styled(agent.to_string(), Style::default().fg(Color::Magenta)),
        None => Span::styled("—", Style::default().fg(MUTED)),
    }
}

/// Natural width of the rendered line, so the host can carve a slot that fits
/// on the very first content frame (before which the incoming rect is zero).
pub fn content_width(input: &IndicatorInput) -> u16 {
    line(input).map(|line| line.width() as u16).unwrap_or(0)
}

pub fn render(input: &IndicatorInput, area: Rect, buf: &mut Buffer) {
    if let Some(line) = line(input) {
        Paragraph::new(line).render(area, buf);
    }
}

/// The full attribution, for the click toast. Says where every claim came from,
/// including the ones the strip had no room for.
///
/// Joined with " · " rather than newlines: the host renders a toast as ONE line
/// and drops `\n` outright, which ran every field together into an unreadable
/// "pane: w1:p3Jagent: claudeharness: …". Measured on the real toast, not
/// assumed from the type signature.
pub fn detail(input: &IndicatorInput) -> String {
    let home = input.home.as_deref().unwrap_or_default();
    let mut out = Vec::new();
    out.push(format!("pane: {}", input.pane_id.as_deref().unwrap_or("—")));
    out.push(format!(
        "agent: {}",
        input.agent.as_deref().unwrap_or("none")
    ));
    match &input.harness {
        Some(harness) => {
            out.push(format!(
                "harness: {}{}",
                harness.name,
                if harness.profile { " (profile)" } else { "" }
            ));
            out.push(format!(
                "declared runtime: {}",
                harness.runtime.as_deref().unwrap_or("—")
            ));
            out.push(format!(
                "evidence: {}",
                match harness.source {
                    harness_indicator_model::HarnessSource::Env =>
                        "process environment (CLAUDE_CONFIG_DIR / CODEX_HOME / JCODE_HOME)",
                    harness_indicator_model::HarnessSource::Transcript =>
                        "transcript found under the harness root (inferred)",
                }
            ));
        }
        None => out.push("harness: none (default roots)".into()),
    }
    if let Some(config) = &input.config_dir {
        out.push(format!("config dir: {}", tilde(config, home)));
    }
    if let Some(agent_folder) = &input.agent_cwd {
        out.push(format!(
            "agent runs in: {} (runtime process cwd)",
            tilde(agent_folder, home)
        ));
    }
    if let Some(shell_folder) = &input.shell_cwd {
        out.push(format!(
            "pane shell in: {} (herdr pane cwd)",
            tilde(shell_folder, home)
        ));
    }
    if let Some((declared, detected)) = input.mismatch() {
        out.push(format!(
            "MISMATCH: harness declares {declared}, pane runs {detected}"
        ));
    }
    if let Some(note) = &input.note {
        out.push(format!("note: {note}"));
    }
    out.join(" \u{00B7} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_indicator_model::{HarnessRef, HarnessSource};

    fn text(input: &IndicatorInput) -> String {
        line(input)
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .unwrap_or_default()
    }

    fn in_harness() -> IndicatorInput {
        IndicatorInput {
            pane_id: Some("w1:p1".into()),
            agent: Some("claude".into()),
            harness: Some(HarnessRef {
                name: "e2e-claude".into(),
                runtime: Some("claude".into()),
                profile: false,
                source: HarnessSource::Env,
            }),
            agent_cwd: Some("/home/x/harnesses/e2e-claude/workspace".into()),
            shell_cwd: Some("/home/x/harnesses/e2e-claude/workspace".into()),
            home: Some("/home/x".into()),
            ..Default::default()
        }
    }

    #[test]
    fn harness_pane_reads_name_agent_folder() {
        assert_eq!(
            text(&in_harness()),
            "⛺ e2e-claude · claude · ~/harnesses/e2e-claude/workspace"
        );
    }

    #[test]
    fn mismatch_is_spelled_out_both_ways_round() {
        let mut input = in_harness();
        input.agent = Some("codex".into());
        assert_eq!(
            text(&input),
            "⛺ e2e-claude ⚠ claude≠codex · codex · ~/harnesses/e2e-claude/workspace"
        );
    }

    #[test]
    fn divergent_folders_render_both() {
        let mut input = in_harness();
        input.shell_cwd = Some("/home/x/Projects/herdr".into());
        assert_eq!(
            text(&input),
            "⛺ e2e-claude · claude · ~/harnesses/e2e-claude/workspace → ~/Projects/herdr"
        );
    }

    #[test]
    fn inferred_attribution_is_marked_not_asserted() {
        let mut input = in_harness();
        if let Some(harness) = input.harness.as_mut() {
            harness.source = HarnessSource::Transcript;
        }
        assert!(text(&input).contains("≈e2e-claude"));
    }

    #[test]
    fn default_roots_say_no_harness_without_alarm() {
        let mut input = in_harness();
        input.harness = None;
        input.agent_cwd = Some("/home/x/Projects/herdr".into());
        input.shell_cwd = Some("/home/x/Projects/herdr".into());
        assert_eq!(text(&input), "⛺ no harness · claude · ~/Projects/herdr");
    }

    #[test]
    fn shell_pane_is_neutral_and_agentless() {
        let input = IndicatorInput {
            pane_id: Some("w1:p1".into()),
            agent_cwd: Some("/home/x".into()),
            shell_cwd: Some("/home/x".into()),
            home: Some("/home/x".into()),
            ..Default::default()
        };
        assert_eq!(text(&input), "⛺ shell · ~");
    }

    #[test]
    fn nothing_focused_renders_no_row_at_all() {
        assert!(line(&IndicatorInput::default()).is_none());
        assert_eq!(content_width(&IndicatorInput::default()), 0);
    }

    #[test]
    fn content_width_counts_display_columns_not_chars() {
        // ⛺ is a WIDE glyph: one char, two columns. Measuring chars would carve
        // a slot one column short and clip the last character of the folder —
        // the exact end of the line that carries the information.
        let input = in_harness();
        assert_eq!(
            content_width(&input),
            text(&input).chars().count() as u16 + 1
        );
    }

    #[test]
    fn detail_is_one_line_because_the_host_toast_is() {
        let text = detail(&in_harness());
        assert!(
            !text.contains('\n'),
            "newlines are dropped by the toast: {text}"
        );
        assert!(text.contains(" \u{00B7} "));
    }

    #[test]
    fn detail_names_its_evidence() {
        let text = detail(&in_harness());
        assert!(text.contains("harness: e2e-claude"));
        assert!(text.contains("process environment"));
        let mut inferred = in_harness();
        if let Some(harness) = inferred.harness.as_mut() {
            harness.source = HarnessSource::Transcript;
        }
        assert!(detail(&inferred).contains("inferred"));
    }
}
