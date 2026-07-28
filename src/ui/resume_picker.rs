//! Renders the resume-search picker overlay (`Mode::ResumePicker`).
//!
//! A floating bordered window over the pane-content area (like the settings
//! modal, but larger), leaving the tabs/sidebar/rails visible around it. The
//! window hosts the picker's hidden PTY: its screen is painted into the inner
//! rect, and the PTY is resized to that rect every frame (idempotent when
//! unchanged) so the picker's own layout tracks the popup.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::widgets::{centered_popup_rect, render_panel_shell};
use crate::app::AppState;
use crate::terminal::TerminalRuntimeRegistry;

/// Fraction of the pane-content area the overlay occupies (bigger than the
/// settings modal, but with a visible margin so the UI shows around it).
const OVERLAY_AREA_NUM: u16 = 9;
const OVERLAY_AREA_DEN: u16 = 10;

pub(super) fn render_resume_picker_overlay(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    terminal_area: Rect,
) {
    let Some(overlay) = app.resume_picker.as_ref() else {
        return;
    };
    let Some(runtime) = terminal_runtimes.get(&overlay.terminal_id) else {
        return;
    };

    let popup_w = (terminal_area.width * OVERLAY_AREA_NUM / OVERLAY_AREA_DEN).max(20);
    let popup_h = (terminal_area.height * OVERLAY_AREA_NUM / OVERLAY_AREA_DEN).max(8);
    let Some(popup) = centered_popup_rect(terminal_area, popup_w, popup_h) else {
        return;
    };

    let p = &app.palette;
    let Some(inner) = render_panel_shell(frame, popup, p.accent, p.panel_bg) else {
        return;
    };
    if inner.height < 3 || inner.width < 10 {
        return;
    }

    // Header (title) + a one-row gap + the picker screen.
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " resume search",
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )])),
        rows[0],
    );

    let screen = rows[2];
    // Keep the hidden PTY sized to the painted rect so the picker lays itself
    // out for exactly this window. `resize` short-circuits when unchanged.
    let cell = crate::kitty_graphics::HostCellSize::default();
    runtime.resize(screen.height, screen.width, cell.width_px, cell.height_px);
    runtime.render(frame, screen, true);
}
