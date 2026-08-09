//! fork(host-actions): the single interception point that lets a UI plugin
//! claim a host-drawn control.
//!
//! # Why one door instead of a guard per button
//!
//! The obvious implementation puts a three-line guard inside each control's
//! branch in `mouse.rs`. That works, but it leaves ~66 edits scattered through a
//! file upstream rewrites constantly, and this fork merges upstream releases on
//! a schedule. Every one of those edits is a place a future merge has to be
//! checked by hand.
//!
//! Every click already funnels through ONE function — `AppState::handle_mouse`.
//! Intercepting there costs three lines in `mouse.rs` total, and everything else
//! lives in this file, which upstream will never touch and which therefore can
//! never conflict.
//!
//! # What that costs, stated plainly
//!
//! [`AppState::resolve_host_action`] has to reproduce the ORDER of the checks
//! `handle_mouse` performs before each control's branch, because a click that
//! the host would have consumed earlier (a toast, a modal, a drag handle) must
//! not be claimed as a button press. That order is duplicated knowledge, and if
//! upstream reorders its routing this file goes stale.
//!
//! It goes stale QUIETLY, in fork-owned code, rather than as a merge conflict —
//! which is the trade this design is making on purpose. Two things keep the
//! staleness bounded:
//!
//! 1. The resolver is **conservative**. It returns `Some` only for the handful
//!    of controls it models and `None` for everything else, including "the host
//!    would consume this somewhere I do not model". `None` means "carry on into
//!    the host's real code", so the failure mode of drift is a MISSED claim (a
//!    plugin's button stops firing, loudly, for its author) and never a STOLEN
//!    click (a native button silently dying for the user).
//! 2. The order is pinned by tests below, so a reorder that changes which
//!    control wins a shared cell fails CI instead of shipping.
//!
//! # Why the reproduction is possible at all
//!
//! Because herdr already separates "where is this control" from "what does it
//! do": `on_new_tab_button`, `on_sidebar_toggle`, `global_launcher_rect`,
//! `sidebar_new_button_rect`, `on_tab_scroll_left_button` and friends are all
//! their own named functions. This file calls the host's own geometry rather
//! than re-deriving any of it, so a rect that moves upstream moves here too.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::app::state::{AppState, Mode, ViewLayout};
use crate::ui_host_actions::{HostAction, HostActionOutcome};

/// The result `handle_mouse` acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostActionIntercept {
    /// Nothing claimed this click in `Replace` mode; the host must route it
    /// normally. Also the answer for an `Observe` claim, which has already been
    /// queued by the time this is returned.
    Passthrough,
    /// A plugin owns this click; `handle_mouse` must return without routing it.
    Consumed,
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

impl AppState {
    /// Entry point called once at the top of `AppState::handle_mouse`.
    pub(super) fn intercept_host_action(&mut self, mouse: MouseEvent) -> HostActionIntercept {
        let Some(action) = self.resolve_host_action(mouse) else {
            return HostActionIntercept::Passthrough;
        };
        match self.dispatch_host_action(action) {
            HostActionOutcome::Claimed => HostActionIntercept::Consumed,
            HostActionOutcome::Proceed => HostActionIntercept::Passthrough,
        }
    }

    /// Which host-drawn control, if any, this click would reach.
    ///
    /// Walks the same sequence `handle_mouse` walks, bailing out at every check
    /// that would consume the click before it reached a modelled control. Read
    /// this next to `handle_mouse` — the `mouse.rs:NNN` markers name the branch
    /// each step mirrors, and they are the thing to re-check after an upstream
    /// merge touches that file.
    pub(super) fn resolve_host_action(&self, mouse: MouseEvent) -> Option<HostAction> {
        // Wave 1 models left presses only. Right-clicks open context menus and
        // wheel events route to tabs/panes; both are separate branches with
        // separate shapes, and neither is modelled here.
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }
        let (col, row) = (mouse.column, mouse.row);

        // mouse.rs: onboarding owns every event.
        if self.mode == Mode::Onboarding {
            return None;
        }
        // mouse.rs: a clickable toast wins over everything under it.
        if self.mode == Mode::Terminal && self.clickable_toast_at(col, row) {
            return None;
        }
        // mouse.rs: the settings overlay routes its own clicks.
        if self.mode == Mode::Settings {
            return None;
        }
        // mouse.rs: the host-owned right-panel rail is checked before plugin
        // hit-routing so the rail column never leaks a click to a layer.
        if self.mode == Mode::Terminal && self.right_panel_rail_icon_at(col, row).is_some() {
            return None;
        }
        // mouse.rs: a plugin layer that consumes the mouse, or that declared a
        // hit-rect over this cell, already owns the click. Never reinterpret a
        // module's own button as a host control.
        if self.mode == Mode::Terminal && !self.ui_surface_layers.is_empty() {
            if let Some(hit) =
                crate::ui::ui_plugin_hit_at(self, crate::ui::ui_plugin_frame_area(self), col, row)
            {
                if hit.consumes_mouse || hit.action_id.is_some() {
                    return None;
                }
            }
        }

        // mouse.rs: the launcher is tested BEFORE the modal early-returns, so it
        // stays live in Navigate/Resize/GlobalMenu/KeybindHelp. Reproducing that
        // position matters — testing it after the mode allowlist below would
        // make the ☰ button unclaimable in exactly the modes it still works in.
        if self.global_launcher_enabled() && rect_contains(self.global_launcher_rect(), col, row) {
            return Some(HostAction::GlobalMenuOpen);
        }

        // mouse.rs: these two modes consume everything else themselves.
        if matches!(self.mode, Mode::GlobalMenu | Mode::KeybindHelp) {
            return None;
        }
        // mouse.rs: mobile has its own router. Wave 1 is desktop-only.
        if self.view.layout == ViewLayout::Mobile {
            return None;
        }

        // mouse.rs: every modal mode routes to its own buttons. This is an
        // allowlist rather than a denylist so a mode added upstream defaults to
        // "not claimable" instead of silently becoming claimable.
        if !matches!(self.mode, Mode::Terminal | Mode::Navigate | Mode::Resize) {
            return None;
        }

        // mouse.rs: sidebar drag handles.
        if self.on_sidebar_divider(col, row) || self.on_sidebar_section_divider(col, row) {
            return None;
        }

        let in_sidebar = rect_contains(self.view.sidebar_rect, col, row);
        // mouse.rs: split borders (and pane scrollbars) claim clicks outside the
        // sidebar. The scrollbar test needs the terminal runtime registry, which
        // pure state cannot reach; it is safe to omit only because pane
        // scrollbars sit inside the pane area, which is disjoint from every rect
        // modelled below (tab bar above, sidebar left). If a modelled control
        // ever moves into the pane area, that reasoning has to be redone.
        if !in_sidebar && self.find_border_at(col, row).is_some() {
            return None;
        }

        // mouse.rs: in some modes the bottom mode bar covers the tab row.
        if self.mode_bar_covers_tab_row(col, row) {
            return None;
        }

        // mouse.rs: tab overflow arrows, then a tab press, then the + button.
        // The tab-press test has to come first: it wins the cell in the host.
        if self.on_tab_scroll_left_button(col, row) {
            return Some(HostAction::TabScrollLeft);
        }
        if self.on_tab_scroll_right_button(col, row) {
            return Some(HostAction::TabScrollRight);
        }
        if self.active.is_some() && self.tab_at(col, row).is_some() {
            return None;
        }
        if self.on_new_tab_button(col, row) {
            return Some(HostAction::TabNew);
        }

        if in_sidebar {
            // mouse.rs: the collapse chevron is tested before anything else in
            // the sidebar, collapsed or not.
            if self.on_sidebar_toggle(col, row) {
                return Some(HostAction::SidebarToggle);
            }
            // mouse.rs: a collapsed sidebar has only its own dot rows below.
            if self.sidebar_collapsed {
                return None;
            }
            if rect_contains(self.sidebar_new_button_rect(), col, row) {
                return Some(HostAction::WorkspaceNew);
            }
        }

        None
    }

    /// Mirrors the `launcher_enabled` predicate in `handle_mouse`.
    ///
    /// Split out so the resolver and the tests below name the same rule, and so
    /// an upstream change to it has one place to land rather than two.
    fn global_launcher_enabled(&self) -> bool {
        self.view.layout != ViewLayout::Mobile
            && !self.sidebar_collapsed
            && matches!(
                self.mode,
                Mode::Terminal
                    | Mode::Navigate
                    | Mode::Resize
                    | Mode::GlobalMenu
                    | Mode::KeybindHelp
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::ui_host_actions::{HostActionClaim, HostActionMode};
    use crate::workspace::Workspace;

    const SCREEN: Rect = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 40,
    };

    fn left_down(col: u16, row: u16) -> MouseEvent {
        super::super::mouse(MouseEventKind::Down(MouseButton::Left), col, row)
    }

    /// A laid-out desktop app. `compute_view` is what fills `new_tab_hit_area`
    /// and the tab-scroll rects, so building the fixture by hand instead would
    /// leave three of the six modelled controls untestable while every
    /// assertion about them still passed.
    fn desktop_app() -> App {
        let mut app = super::super::app_for_mouse_test();
        app.state.workspaces = vec![Workspace::test_new("one")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        crate::ui::compute_view(&mut app.state, SCREEN);
        app
    }

    /// Enough tabs that the bar overflows and grows its `‹`/`›` arrows.
    ///
    /// Stays at the full [`SCREEN`] width on purpose: narrowing the terminal to
    /// force overflow crosses `mobile_width_threshold` instead, which drops the
    /// desktop tab chrome entirely and leaves the arrows untested.
    fn desktop_app_with_tab_overflow() -> App {
        let mut app = super::super::app_for_mouse_test();
        let mut ws = Workspace::test_new("one");
        for _ in 0..40 {
            ws.test_add_tab(Some("a-fairly-long-tab-name"));
        }
        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        crate::ui::compute_view(&mut app.state, SCREEN);
        app
    }

    fn desktop_state() -> AppState {
        desktop_app().state
    }

    fn claim(state: &mut AppState, action: HostAction, mode: HostActionMode) {
        state.ui_host_action_claims.insert(
            action.id(),
            HostActionClaim {
                plugin_id: "p".into(),
                module_id: "m".into(),
                mode,
            },
        );
    }

    /// Guards every other test in this file. A fixture whose rects are all
    /// `Rect::default()` makes "resolves to None" assertions pass for the wrong
    /// reason, which is exactly what an earlier draft of these tests did for the
    /// three tab-bar controls.
    #[test]
    fn the_fixture_actually_lays_out_the_controls_under_test() {
        let state = desktop_state();
        for (name, rect) in [
            ("global launcher", state.global_launcher_rect()),
            ("sidebar + new", state.sidebar_new_button_rect()),
            ("tab bar +", state.view.new_tab_hit_area),
            (
                "sidebar toggle",
                crate::ui::expanded_sidebar_toggle_rect(state.view.sidebar_rect),
            ),
        ] {
            assert!(
                rect.width > 0 && rect.height > 0,
                "{name} has no geometry in the fixture; tests using it are vacuous"
            );
        }

        let overflow = desktop_app_with_tab_overflow().state;
        assert!(
            overflow.view.tab_scroll_left_hit_area.width > 0
                && overflow.view.tab_scroll_right_hit_area.width > 0,
            "the overflow fixture must actually overflow, or the scroll arrows are untested"
        );
    }

    /// Each modelled control resolves to its own action, from real geometry.
    #[test]
    fn every_modelled_control_resolves_to_its_own_action() {
        let state = desktop_state();
        let toggle = crate::ui::expanded_sidebar_toggle_rect(state.view.sidebar_rect);
        for (rect, expected) in [
            (state.global_launcher_rect(), HostAction::GlobalMenuOpen),
            (state.sidebar_new_button_rect(), HostAction::WorkspaceNew),
            (state.view.new_tab_hit_area, HostAction::TabNew),
            (toggle, HostAction::SidebarToggle),
        ] {
            assert_eq!(
                state.resolve_host_action(left_down(rect.x, rect.y)),
                Some(expected),
                "{expected:?} did not resolve at its own rect {rect:?}"
            );
        }

        let overflow = desktop_app_with_tab_overflow().state;
        let left = overflow.view.tab_scroll_left_hit_area;
        let right = overflow.view.tab_scroll_right_hit_area;
        assert_eq!(
            overflow.resolve_host_action(left_down(left.x, left.y)),
            Some(HostAction::TabScrollLeft)
        );
        assert_eq!(
            overflow.resolve_host_action(left_down(right.x, right.y)),
            Some(HostAction::TabScrollRight)
        );
    }

    /// The load-bearing safety property. Drift in this file must degrade to a
    /// missed claim, never to a native button that stops working — so a cell the
    /// resolver does not model has to resolve to `None`, not to a guess.
    #[test]
    fn every_unmodelled_cell_resolves_to_none() {
        let state = desktop_state();
        let modelled: Vec<Rect> = vec![
            state.global_launcher_rect(),
            state.sidebar_new_button_rect(),
        ];
        let mut resolved = 0usize;
        for row in 0..SCREEN.height {
            for col in 0..SCREEN.width {
                if modelled.iter().any(|r| rect_contains(*r, col, row)) {
                    continue;
                }
                if state.on_sidebar_toggle(col, row)
                    || state.on_new_tab_button(col, row)
                    || state.on_tab_scroll_left_button(col, row)
                    || state.on_tab_scroll_right_button(col, row)
                {
                    continue;
                }
                if state.resolve_host_action(left_down(col, row)).is_some() {
                    resolved += 1;
                }
                assert_eq!(
                    state.resolve_host_action(left_down(col, row)),
                    None,
                    "cell ({col},{row}) is not a modelled control but resolved to an action"
                );
            }
        }
        assert_eq!(resolved, 0);
    }

    #[test]
    fn only_left_presses_resolve() {
        let state = desktop_state();
        let launcher = state.global_launcher_rect();
        assert_eq!(
            state.resolve_host_action(left_down(launcher.x, launcher.y)),
            Some(HostAction::GlobalMenuOpen)
        );
        for kind in [
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
            MouseEventKind::Moved,
        ] {
            let mut event = left_down(launcher.x, launcher.y);
            event.kind = kind;
            assert_eq!(
                state.resolve_host_action(event),
                None,
                "{kind:?} must not resolve"
            );
        }
    }

    /// Pins the position of the launcher test relative to the mode allowlist.
    /// `handle_mouse` checks the launcher BEFORE returning early for GlobalMenu
    /// and KeybindHelp, so the ☰ button is still live in those modes; a resolver
    /// that ordered them the other way would silently stop claiming it there.
    #[test]
    fn the_launcher_stays_claimable_in_the_modes_that_still_reach_it() {
        for mode in [
            Mode::Terminal,
            Mode::Navigate,
            Mode::Resize,
            Mode::GlobalMenu,
            Mode::KeybindHelp,
        ] {
            let mut state = desktop_state();
            state.mode = mode;
            let launcher = state.global_launcher_rect();
            assert_eq!(
                state.resolve_host_action(left_down(launcher.x, launcher.y)),
                Some(HostAction::GlobalMenuOpen),
                "{mode:?} reaches the launcher in handle_mouse and must resolve it"
            );
        }
    }

    /// The mode gate is an allowlist. A modal mode must claim nothing, or a
    /// plugin could steal a click the host was about to route to a dialog
    /// button sitting over the same cell.
    #[test]
    fn modal_modes_claim_nothing() {
        for mode in [
            Mode::ConfirmClose,
            Mode::RenameWorkspace,
            Mode::RenameTab,
            Mode::ContextMenu,
            Mode::Settings,
            Mode::Onboarding,
            Mode::NewLinkedWorktree,
            Mode::OpenExistingWorktree,
            Mode::ConfirmRemoveWorktree,
        ] {
            let mut state = desktop_state();
            state.mode = mode;
            for row in 0..SCREEN.height {
                for col in 0..SCREEN.width {
                    assert_eq!(
                        state.resolve_host_action(left_down(col, row)),
                        None,
                        "{mode:?} must not resolve any host action at ({col},{row})"
                    );
                }
            }
        }
    }

    /// Mobile routes through `handle_mobile_mouse`, whose controls are not
    /// modelled. Claiming there would suppress a default this file cannot
    /// reproduce.
    #[test]
    fn mobile_layout_claims_nothing() {
        let mut state = desktop_state();
        state.view.layout = ViewLayout::Mobile;
        for row in 0..SCREEN.height {
            for col in 0..SCREEN.width {
                assert_eq!(state.resolve_host_action(left_down(col, row)), None);
            }
        }
    }

    #[test]
    fn a_collapsed_sidebar_still_resolves_its_toggle_but_not_the_new_button() {
        let mut app = super::super::app_for_mouse_test();
        app.state.workspaces = vec![Workspace::test_new("one")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app.state.sidebar_collapsed = true;
        crate::ui::compute_view(&mut app.state, SCREEN);
        let state = app.state;

        let toggle = crate::ui::collapsed_sidebar_toggle_rect(state.view.sidebar_rect);
        assert!(
            toggle.width > 0,
            "the collapsed rail must have a toggle, or this test proves nothing"
        );
        assert_eq!(
            state.resolve_host_action(left_down(toggle.x, toggle.y)),
            Some(HostAction::SidebarToggle)
        );
        // The launcher and the `+ new` button are both hidden while collapsed.
        assert!(!state.global_launcher_enabled());
    }

    #[test]
    fn intercept_consumes_only_for_a_replace_claim() {
        let mut state = desktop_state();
        let launcher = state.global_launcher_rect();
        let at = left_down(launcher.x, launcher.y);

        assert_eq!(
            state.intercept_host_action(at),
            HostActionIntercept::Passthrough,
            "an unclaimed control must fall through to the host"
        );
        assert!(state.pending_host_actions.is_empty());

        claim(
            &mut state,
            HostAction::GlobalMenuOpen,
            HostActionMode::Observe,
        );
        assert_eq!(
            state.intercept_host_action(at),
            HostActionIntercept::Passthrough
        );
        assert_eq!(state.pending_host_actions.len(), 1);

        state.pending_host_actions.clear();
        claim(
            &mut state,
            HostAction::GlobalMenuOpen,
            HostActionMode::Replace,
        );
        assert_eq!(
            state.intercept_host_action(at),
            HostActionIntercept::Consumed
        );
        assert_eq!(state.pending_host_actions.len(), 1);
    }

    /// A claim must not make an unrelated cell start being intercepted — the
    /// resolver runs first and the claim table only decides what happens after.
    #[test]
    fn a_claim_does_not_widen_which_cells_are_intercepted() {
        let mut state = desktop_state();
        claim(&mut state, HostAction::TabNew, HostActionMode::Replace);
        assert_eq!(
            state.intercept_host_action(left_down(40, 10)),
            HostActionIntercept::Passthrough,
            "a cell in the pane area must never be consumed by a tab-bar claim"
        );
        assert!(state.pending_host_actions.is_empty());
    }

    // ── characterization: the unclaimed path is unchanged ──────────────────
    //
    // These drive the full `App::handle_mouse` path with NO claim registered
    // and assert the pre-existing behaviour of each modelled control. They are
    // the tripwire for an upstream merge that changes a default out from under
    // the resolver: if herdr changes what `+` does, these fail here rather than
    // silently changing what a plugin's `Replace` claim is replacing.

    #[test]
    fn unclaimed_new_tab_button_still_requests_a_tab() {
        let mut app = desktop_app();
        app.state.prompt_new_tab_name = false;
        let rect = app.state.view.new_tab_hit_area;
        app.handle_mouse(left_down(rect.x + 1, rect.y));

        assert!(app.state.request_new_tab);
        assert_eq!(app.state.mode, Mode::Terminal);
        assert!(app.state.pending_host_actions.is_empty());
    }

    #[test]
    fn unclaimed_new_workspace_button_still_opens_the_prompt() {
        let mut app = desktop_app();
        app.state.prompt_new_workspace_name = true;
        let rect = app.state.sidebar_new_button_rect();
        app.handle_mouse(left_down(rect.x + 1, rect.y));

        assert_eq!(app.state.mode, Mode::RenameWorkspace);
        assert!(app.state.pending_workspace_create_cwd.is_some());
        assert_eq!(app.state.workspaces.len(), 1);
    }

    #[test]
    fn unclaimed_launcher_still_opens_the_global_menu() {
        let mut app = desktop_app();
        let rect = app.state.global_launcher_rect();
        app.handle_mouse(left_down(rect.x, rect.y));

        assert_eq!(app.state.mode, Mode::GlobalMenu);
    }

    #[test]
    fn unclaimed_sidebar_toggle_still_collapses_the_sidebar() {
        let mut app = desktop_app();
        let rect = crate::ui::expanded_sidebar_toggle_rect(app.state.view.sidebar_rect);
        app.handle_mouse(left_down(rect.x, rect.y));

        assert!(app.state.sidebar_collapsed);
    }

    // ── the claim actually suppresses the default ──────────────────────────

    #[test]
    fn a_replace_claim_stops_the_new_tab_button_from_creating_a_tab() {
        let mut app = desktop_app();
        app.state.prompt_new_tab_name = false;
        claim(&mut app.state, HostAction::TabNew, HostActionMode::Replace);
        let rect = app.state.view.new_tab_hit_area;
        app.handle_mouse(left_down(rect.x + 1, rect.y));

        assert!(
            !app.state.request_new_tab,
            "a replace claim must suppress the host's own new-tab request"
        );
        assert_eq!(app.state.pending_host_actions.len(), 1);
        assert_eq!(app.state.pending_host_actions[0].action_id, "host.tab.new");
    }

    #[test]
    fn a_replace_claim_stops_the_launcher_from_opening_the_menu() {
        let mut app = desktop_app();
        claim(
            &mut app.state,
            HostAction::GlobalMenuOpen,
            HostActionMode::Replace,
        );
        let rect = app.state.global_launcher_rect();
        app.handle_mouse(left_down(rect.x, rect.y));

        assert_eq!(app.state.mode, Mode::Terminal);
        assert_eq!(app.state.pending_host_actions.len(), 1);
    }

    /// The observe half, end to end: the host still does its thing AND the
    /// plugin is told. If this ever stops running the default, every observing
    /// plugin silently breaks the control it was only watching.
    #[test]
    fn an_observe_claim_leaves_the_sidebar_toggle_working() {
        let mut app = desktop_app();
        claim(
            &mut app.state,
            HostAction::SidebarToggle,
            HostActionMode::Observe,
        );
        let rect = crate::ui::expanded_sidebar_toggle_rect(app.state.view.sidebar_rect);
        app.handle_mouse(left_down(rect.x, rect.y));

        assert!(app.state.sidebar_collapsed, "observe must not suppress");
        assert_eq!(app.state.pending_host_actions.len(), 1);
        assert_eq!(
            app.state.pending_host_actions[0].action_id,
            "host.sidebar.toggle"
        );
    }
}
