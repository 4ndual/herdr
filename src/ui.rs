use herdr_ui_plugin_api::{UiLayer, UiNode, UiSurface, UiTextSpan};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

mod dialogs;
mod keybind_help;
mod menus;
mod mobile;
mod navigator;
mod onboarding;
mod panes;
mod release_notes;
mod scrollbar;
mod settings;
mod sidebar;
mod status;
mod tabs;
mod widgets;

use self::dialogs::{
    render_confirm_close_overlay, render_new_linked_worktree_overlay,
    render_open_existing_worktree_overlay, render_remove_worktree_overlay, render_rename_overlay,
};
use self::keybind_help::render_keybind_help_overlay;
use self::menus::{
    render_context_menu, render_copy_mode_overlay, render_global_launcher_menu,
    render_navigate_overlay, render_prefix_overlay, render_resize_overlay,
};
use self::mobile::{
    compute_mobile_header_hit_areas, is_mobile_width, mobile_switcher_max_scroll_for_height,
    mobile_toast_banner_rect, render_mobile_header, render_mobile_panel,
    render_mobile_toast_banner,
};
use self::navigator::render_navigator_overlay;
pub(crate) use self::onboarding::onboarding_welcome_continue_rect;
use self::onboarding::render_onboarding_overlay;
use self::panes::{compute_pane_infos, render_panes, resize_tab_panes};
pub(crate) use self::release_notes::{
    product_announcement_display_lines, release_notes_close_button_rect,
    release_notes_display_lines, release_notes_wrapped_line_count, PRODUCT_ANNOUNCEMENT_MODAL_SIZE,
    RELEASE_NOTES_MODAL_SIZE,
};
use self::release_notes::{render_product_announcement_overlay, render_release_notes_overlay};
pub(crate) use self::scrollbar::{
    pane_scrollbar_rect, release_notes_scrollbar_rect, scrollbar_offset_from_drag_row,
    scrollbar_offset_from_row, scrollbar_thumb_grab_offset, should_show_scrollbar,
};
use self::settings::render_settings_overlay;
use self::sidebar::{render_sidebar, render_sidebar_collapsed};
use self::status::{
    copy_feedback_rect, render_config_diagnostic, render_copy_feedback, render_toast_notification,
    toast_notification_rect,
};
// fork: non-foreground "panes pinned to ..." badge, drawn by the headless
// server's per-client render path (see server::render_stream).
pub(crate) use self::status::render_foreground_pin_badge;
use self::tabs::render_tab_bar;
pub(crate) use self::{
    dialogs::{
        confirm_close_button_rects, confirm_close_popup_rect, new_linked_worktree_button_rects,
        new_linked_worktree_inner_rect, open_existing_worktree_button_rects,
        open_existing_worktree_inner_rect, open_existing_worktree_max_visible_rows,
        open_existing_worktree_visible_start, remove_worktree_button_rects,
        remove_worktree_popup_rect, rename_button_rects,
    },
    settings::{
        settings_button_rects, settings_popup_height, settings_show_primary_action,
        SETTINGS_POPUP_WIDTH,
    },
    sidebar::{
        agent_panel_body_rect, agent_panel_entries, agent_panel_scroll_metrics,
        agent_panel_scrollbar_rect, agent_panel_toggle_rect, collapsed_sidebar_sections,
        collapsed_sidebar_toggle_rect, compute_workspace_card_areas, expanded_sidebar_sections,
        expanded_sidebar_toggle_rect, normalized_workspace_scroll, sidebar_section_divider_rect,
        workspace_drop_indicator_row, workspace_list_entries, workspace_list_rect,
        workspace_list_scroll_metrics, workspace_list_scrollbar_rect, workspace_parent_group_state,
        WorkspaceListEntry,
    },
};
pub(crate) use self::{
    keybind_help::keybind_help_lines,
    mobile::{
        mobile_switcher_areas, mobile_switcher_max_scroll, mobile_switcher_target_at,
        mobile_switcher_workspace_doc_range, MobileSwitcherTarget,
    },
    panes::pane_is_scrolled_back,
    tabs::compute_tab_bar_view,
    widgets::{centered_popup_rect, modal_stack_areas},
};
use crate::app::state::ViewLayout;
use crate::app::{AppState, Mode};
use crate::terminal::TerminalRuntimeRegistry;

const COLLAPSED_WIDTH: u16 = 4;

// feat/native-dashes: fixed reserve for the bottom usage strip and the bounds of
// the toggleable right goal panel. P4 will derive the strip height from usage
// content height rather than reserving a fixed band.
const USAGE_STRIP_HEIGHT: u16 = 7;
const GOAL_PANEL_MIN_WIDTH: u16 = 28;
const GOAL_PANEL_MAX_WIDTH: u16 = 56; // num + space + dot + separator

// Braille spinner frames — smooth rotation
const SPINNERS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Map spinner_tick (incremented every frame at ~60fps) to a spinner frame.
/// We want ~8 updates/sec so divide by 8.
pub(super) fn spinner_frame(tick: u32) -> &'static str {
    SPINNERS[(tick as usize / 8) % SPINNERS.len()]
}

/// feat/native-dashes: anchor area for desktop herdr toasts that keeps them clear
/// of the native dashboards. The bottom usage strip sits along the window bottom
/// and the goal panel along the right edge; a bottom/right-anchored toast drawn
/// against the full window would overlap them and (since `dash::route_mouse`
/// resolves the usage strip and goal panel before the toast) have its clicks
/// swallowed by the strip. Shrinking the anchor by those rects moves the toast
/// just above the strip and left of the panel so render and hit-area agree.
fn toast_desktop_anchor_area(area: Rect, usage_dash_area: Rect, goal_dash_area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y,
        area.width.saturating_sub(goal_dash_area.width),
        area.height.saturating_sub(usage_dash_area.height),
    )
}

/// Compute view geometry and reconcile pane sizes.
/// Called before render to separate mutation from drawing.
#[cfg_attr(not(test), allow(dead_code))]
pub fn compute_view(app: &mut AppState, area: Rect) {
    let terminal_runtimes = TerminalRuntimeRegistry::new();
    compute_view_with_runtime_registry(app, &terminal_runtimes, area);
}

pub fn compute_view_with_runtime_registry(
    app: &mut AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
) {
    compute_view_internal(
        app,
        terminal_runtimes,
        area,
        true,
        crate::kitty_graphics::HostCellSize::default(),
    );
}

pub fn compute_view_with_cell_size(
    app: &mut AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
) {
    compute_view_internal(app, terminal_runtimes, area, true, cell_size);
}

/// Compute view geometry for a client-sized render without resizing pane runtimes.
///
/// This is used by the headless server when a non-foreground client needs its
/// own frame size while the shared pane runtimes stay pinned to the foreground
/// client.
pub(crate) fn compute_view_without_resizing_panes(
    app: &mut AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
) {
    compute_view_internal(
        app,
        terminal_runtimes,
        area,
        false,
        crate::kitty_graphics::HostCellSize::default(),
    );
}

fn resize_background_tab_panes_to_terminal_area(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    terminal_area: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
) {
    for (ws_idx, ws) in app.workspaces.iter().enumerate() {
        for (tab_idx, tab) in ws.tabs.iter().enumerate() {
            if app.active == Some(ws_idx) && tab_idx == ws.active_tab_index() {
                continue;
            }
            resize_tab_panes(app, terminal_runtimes, tab, terminal_area, cell_size);
        }
    }
}

fn compute_view_internal(
    app: &mut AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) {
    if is_mobile_width(area, app.mobile_width_threshold) {
        compute_mobile_view(app, terminal_runtimes, area, resize_panes, cell_size);
        return;
    }

    let sidebar_w = if app.sidebar_collapsed {
        COLLAPSED_WIDTH
    } else {
        app.sidebar_width
            .clamp(app.sidebar_min_width, app.sidebar_max_width)
    };

    let [sidebar_area, main_area] =
        Layout::horizontal([Constraint::Length(sidebar_w), Constraint::Min(1)]).areas(area);

    let has_tabs = app.active.and_then(|i| app.workspaces.get(i)).is_some();
    let (tab_bar_rect, terminal_area) = if has_tabs && main_area.height > 1 {
        let [tab_bar_rect, terminal_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(main_area);
        (tab_bar_rect, terminal_area)
    } else {
        (Rect::default(), main_area)
    };

    // feat/native-dashes: carve the persistent bottom usage strip and the
    // toggleable right goal panel out of the terminal area before panes are laid
    // out, so panes never overlap them. Both collapse to an empty Rect when there
    // isn't room (or the goal panel is toggled off). P4 will derive the strip
    // height from usage content instead of the fixed reserve used here.
    let (terminal_area, usage_dash_area) =
        if app.usage_strip_enabled && terminal_area.height > USAGE_STRIP_HEIGHT + 3 {
            let [panes, strip] =
                Layout::vertical([Constraint::Min(3), Constraint::Length(USAGE_STRIP_HEIGHT)])
                    .areas(terminal_area);
            (panes, strip)
        } else {
            (terminal_area, Rect::default())
        };
    let (terminal_area, goal_dash_area) = if app.goal_panel_visible
        && terminal_area.width > GOAL_PANEL_MIN_WIDTH + 24
    {
        let auto_w = (terminal_area.width / 4).clamp(GOAL_PANEL_MIN_WIDTH, GOAL_PANEL_MAX_WIDTH);
        let panel_w = app
            .goal_panel_width
            .unwrap_or(auto_w)
            .clamp(GOAL_PANEL_MIN_WIDTH, GOAL_PANEL_MAX_WIDTH)
            .min(terminal_area.width.saturating_sub(24));
        let [panes, panel] = Layout::horizontal([Constraint::Min(24), Constraint::Length(panel_w)])
            .areas(terminal_area);
        (panes, panel)
    } else {
        (terminal_area, Rect::default())
    };

    if !app.sidebar_collapsed {
        app.workspace_scroll = normalized_workspace_scroll(app, sidebar_area, app.workspace_scroll);
        let (_, detail_area) = expanded_sidebar_sections(sidebar_area, app.sidebar_section_split);
        let max_agent_scroll = agent_panel_scroll_metrics(app, detail_area).max_offset_from_bottom;
        app.agent_panel_scroll = app.agent_panel_scroll.min(max_agent_scroll);
    } else {
        app.workspace_scroll = app
            .workspace_scroll
            .min(app.workspaces.len().saturating_sub(1));
        app.agent_panel_scroll = 0;
    }

    let workspace_card_areas = if app.sidebar_collapsed {
        Vec::new()
    } else {
        compute_workspace_card_areas(app, sidebar_area)
    };

    let tab_bar_view = app
        .active
        .and_then(|i| app.workspaces.get(i))
        .map(|ws| {
            compute_tab_bar_view(
                ws,
                tab_bar_rect,
                app.tab_scroll,
                app.tab_scroll_follow_active,
                app.mouse_capture,
            )
        })
        .unwrap_or_default();
    app.tab_scroll = tab_bar_view.scroll;

    let split_borders = app
        .active
        .and_then(|i| app.workspaces.get(i))
        .map(|ws| ws.layout.splits(terminal_area))
        .unwrap_or_default();

    let pane_infos = compute_pane_infos(
        app,
        terminal_runtimes,
        terminal_area,
        resize_panes,
        cell_size,
    );
    if resize_panes {
        resize_background_tab_panes_to_terminal_area(
            app,
            terminal_runtimes,
            terminal_area,
            cell_size,
        );
    }

    let toast_anchor_area = toast_desktop_anchor_area(area, usage_dash_area, goal_dash_area);
    let toast_hit_area = app
        .toast
        .as_ref()
        .map(|toast| {
            toast_notification_rect(
                toast_anchor_area,
                toast,
                app.config_diagnostic.is_some(),
                toast.position.unwrap_or(app.toast_config.herdr.position),
            )
        })
        .unwrap_or_default();

    app.view = crate::app::ViewState {
        layout: ViewLayout::Desktop,
        sidebar_rect: sidebar_area,
        workspace_card_areas,
        tab_bar_rect,
        tab_hit_areas: tab_bar_view.tab_hit_areas,
        tab_scroll_left_hit_area: tab_bar_view.scroll_left_hit_area,
        tab_scroll_right_hit_area: tab_bar_view.scroll_right_hit_area,
        new_tab_hit_area: tab_bar_view.new_tab_hit_area,
        terminal_area,
        mobile_header_rect: Rect::default(),
        mobile_menu_hit_area: Rect::default(),
        toast_hit_area,
        pane_infos,
        split_borders,
        usage_dash_area,
        goal_dash_area,
    };
}

fn compute_mobile_view(
    app: &mut AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) {
    let header_h = area.height.min(2);
    let (header_rect, terminal_area) = if area.height > header_h {
        let [header_rect, terminal_area] =
            Layout::vertical([Constraint::Length(header_h), Constraint::Min(1)]).areas(area);
        (header_rect, terminal_area)
    } else {
        (area, Rect::default())
    };

    if app.mode == Mode::Navigate {
        let switcher_viewport_h = area.height.saturating_sub(header_h + 1);
        let max_scroll = mobile_switcher_max_scroll_for_height(app, switcher_viewport_h);
        app.mobile_switcher_scroll = app.mobile_switcher_scroll.min(max_scroll);
    }

    let split_borders = app
        .active
        .and_then(|i| app.workspaces.get(i))
        .map(|ws| ws.layout.splits(terminal_area))
        .unwrap_or_default();

    let pane_infos = compute_pane_infos(
        app,
        terminal_runtimes,
        terminal_area,
        resize_panes,
        cell_size,
    );
    if resize_panes {
        resize_background_tab_panes_to_terminal_area(
            app,
            terminal_runtimes,
            terminal_area,
            cell_size,
        );
    }
    let header_hits = compute_mobile_header_hit_areas(app, header_rect);

    let toast_hit_area = app
        .toast
        .as_ref()
        .map(|_| mobile_toast_banner_rect(area, app.config_diagnostic.is_some()))
        .unwrap_or_default();

    app.view = crate::app::ViewState {
        layout: ViewLayout::Mobile,
        sidebar_rect: Rect::default(),
        workspace_card_areas: Vec::new(),
        tab_bar_rect: Rect::default(),
        tab_hit_areas: Vec::new(),
        tab_scroll_left_hit_area: Rect::default(),
        tab_scroll_right_hit_area: Rect::default(),
        new_tab_hit_area: Rect::default(),
        terminal_area,
        mobile_header_rect: header_rect,
        mobile_menu_hit_area: header_hits.menu,
        toast_hit_area,
        pane_infos,
        split_borders,
        usage_dash_area: Rect::default(),
        goal_dash_area: Rect::default(),
    };
}

/// Render the UI — reads AppState but does not mutate it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn render(app: &AppState, frame: &mut Frame) {
    let terminal_runtimes = TerminalRuntimeRegistry::new();
    render_with_runtime_registry(app, &terminal_runtimes, frame);
}

pub fn render_with_runtime_registry(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
) {
    let sidebar_area = app.view.sidebar_rect;
    let tab_bar_area = app.view.tab_bar_rect;
    let terminal_area = app.view.terminal_area;

    if app.view.layout == ViewLayout::Mobile {
        render_mobile_header(app, terminal_runtimes, frame, app.view.mobile_header_rect);
    } else if app.sidebar_collapsed {
        render_sidebar_collapsed(app, frame, sidebar_area);
    } else {
        render_sidebar(app, terminal_runtimes, frame, sidebar_area);
    }
    if app.view.layout != ViewLayout::Mobile {
        render_tab_bar(app, frame, tab_bar_area);
    }
    render_panes(app, terminal_runtimes, frame, terminal_area);
    render_ui_plugin_layers(app, frame);

    // Ambient notifications sit above panes, but below interactive overlays.
    render_notifications(app, frame, terminal_area);

    match app.mode {
        Mode::Onboarding => render_onboarding_overlay(app, frame, frame.area()),
        Mode::ReleaseNotes => render_release_notes_overlay(app, frame, frame.area()),
        Mode::ProductAnnouncement => render_product_announcement_overlay(app, frame, frame.area()),
        Mode::Navigate if app.view.layout == ViewLayout::Mobile => {
            render_mobile_panel(app, terminal_runtimes, frame, frame.area())
        }
        Mode::Navigate => render_navigate_overlay(app, frame, terminal_area),
        Mode::Prefix => render_prefix_overlay(app, frame, terminal_area),
        Mode::Copy => render_copy_mode_overlay(app, frame, terminal_area),
        Mode::Resize => render_resize_overlay(app, frame, terminal_area),
        Mode::ConfirmClose => render_confirm_close_overlay(app, frame, terminal_area),
        Mode::ContextMenu => {
            render_context_menu(app, frame);
        }
        Mode::Settings => render_settings_overlay(app, frame, frame.area()),
        Mode::RenameWorkspace | Mode::RenameTab | Mode::RenamePane => {
            render_rename_overlay(app, frame, frame.area())
        }
        Mode::NewLinkedWorktree => render_new_linked_worktree_overlay(app, frame, frame.area()),
        Mode::OpenExistingWorktree => {
            render_open_existing_worktree_overlay(app, frame, frame.area())
        }
        Mode::ConfirmRemoveWorktree => render_remove_worktree_overlay(app, frame, frame.area()),
        Mode::GlobalMenu => render_global_launcher_menu(app, frame),
        Mode::KeybindHelp => render_keybind_help_overlay(app, frame),
        Mode::Navigator => render_navigator_overlay(app, terminal_runtimes, frame),
        Mode::Terminal => {}
    }
}

pub(crate) fn ui_plugin_surface_rect(app: &AppState, frame_area: Rect, surface: UiSurface) -> Rect {
    match surface {
        UiSurface::Header => {
            if app.view.layout == ViewLayout::Mobile {
                app.view.mobile_header_rect
            } else if app.view.terminal_area.height > 0 {
                Rect::new(
                    app.view.terminal_area.x,
                    app.view.terminal_area.y,
                    app.view.terminal_area.width,
                    1,
                )
            } else {
                Rect::default()
            }
        }
        UiSurface::Footer => {
            let area = if app.view.layout == ViewLayout::Mobile {
                app.view.terminal_area
            } else {
                app.view.terminal_area
            };
            if area.height > 0 {
                Rect::new(
                    area.x,
                    area.y + area.height.saturating_sub(1),
                    area.width,
                    1,
                )
            } else {
                Rect::default()
            }
        }
        UiSurface::TabBar => app.view.tab_bar_rect,
        UiSurface::Sidebar => app.view.sidebar_rect,
        UiSurface::RightPanel => app.view.goal_dash_area,
        UiSurface::UsageStrip => app.view.usage_dash_area,
        UiSurface::Overlay => app.view.terminal_area,
        UiSurface::ContextMenu => frame_area,
    }
}

fn plugin_span_style(app: &AppState, role: Option<&str>) -> Style {
    match role.unwrap_or_default() {
        "accent" => Style::default()
            .fg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
        "muted" => Style::default().fg(app.palette.overlay0),
        "success" | "green" => Style::default().fg(app.palette.green),
        "warning" | "yellow" => Style::default().fg(app.palette.yellow),
        "danger" | "red" => Style::default().fg(app.palette.red),
        "blue" => Style::default().fg(app.palette.blue),
        "teal" | "cyan" => Style::default().fg(app.palette.teal),
        "mauve" | "magenta" => Style::default().fg(app.palette.mauve),
        "subtext" | "gray" => Style::default().fg(app.palette.subtext0),
        "peach" => Style::default().fg(app.palette.peach),
        "panel" => Style::default()
            .fg(app.palette.text)
            .bg(app.palette.panel_bg),
        _ => Style::default().fg(app.palette.text),
    }
}

fn plugin_line(app: &AppState, spans: &[UiTextSpan]) -> Line<'static> {
    Line::from(
        spans
            .iter()
            .map(|span| {
                let mut style = plugin_span_style(app, span.role.as_deref());
                if span.bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if span.underline {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if span.italic {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                Span::styled(span.text.clone(), style)
            })
            .collect::<Vec<_>>(),
    )
}

pub(crate) struct UiPluginHit {
    pub action_id: Option<String>,
    pub consumes_mouse: bool,
    /// Spec §4: set only when the hit resolved through a declared `UiHitRect`
    /// (not the Menu/Tabs node-walk fallback below) — "Emitted when a resolved
    /// click lands on a `UiHitRect` declared by a v3 module's current layers."
    /// `(plugin_id, module_id, negotiated_version)` of the owning module.
    pub declared_hit_owner: Option<(String, String, u32)>,
    pub surface: UiSurface,
}

fn ui_plugin_node_action_at(
    _app: &AppState,
    area: Rect,
    mut line_y: u16,
    node: &UiNode,
    col: u16,
    row: u16,
) -> (Option<String>, u16) {
    match node {
        UiNode::Line { .. } | UiNode::Status { .. } => (None, line_y.saturating_add(1)),
        UiNode::Tabs { items, .. } => {
            if row == line_y && col >= area.x && col < area.x + area.width {
                let mut x = area.x;
                for item in items {
                    let label = format!(" {} ", item.label);
                    let w = label.chars().count() as u16;
                    if col >= x && col < x.saturating_add(w) {
                        return (Some(item.id.clone()), line_y.saturating_add(1));
                    }
                    x = x.saturating_add(w + 1);
                }
            }
            (None, line_y.saturating_add(1))
        }
        UiNode::Menu { items, .. } => {
            line_y = line_y.saturating_add(1); // title row
            for item in items {
                if row == line_y && col >= area.x && col < area.x + area.width {
                    return (Some(item.id.clone()), line_y.saturating_add(1));
                }
                line_y = line_y.saturating_add(1);
            }
            (None, line_y)
        }
    }
}

pub(crate) fn ui_plugin_frame_area(app: &AppState) -> Rect {
    let rects = [
        app.view.sidebar_rect,
        app.view.tab_bar_rect,
        app.view.terminal_area,
        app.view.mobile_header_rect,
        app.view.goal_dash_area,
        app.view.usage_dash_area,
    ];
    let mut min_x = u16::MAX;
    let mut min_y = u16::MAX;
    let mut max_x = 0;
    let mut max_y = 0;
    for rect in rects {
        if rect.width == 0 || rect.height == 0 {
            continue;
        }
        min_x = min_x.min(rect.x);
        min_y = min_y.min(rect.y);
        max_x = max_x.max(rect.x + rect.width);
        max_y = max_y.max(rect.y + rect.height);
    }
    if min_x == u16::MAX {
        Rect::default()
    } else {
        Rect::new(
            min_x,
            min_y,
            max_x.saturating_sub(min_x),
            max_y.saturating_sub(min_y),
        )
    }
}

pub(crate) fn ui_plugin_hit_at(
    app: &AppState,
    frame_area: Rect,
    col: u16,
    row: u16,
) -> Option<UiPluginHit> {
    // §4 deterministic overlap ownership: `ui_surface_layers` is already ordered
    // `(z_index desc, plugin_id asc, module_id asc)` by `surface_layers()`, i.e.
    // topmost-owner-first, so walking it forward (no `.rev()`) already checks the
    // topmost layer first.
    for entry in app.ui_surface_layers.iter() {
        let layer = &entry.layer;
        let area = ui_plugin_surface_rect(app, frame_area, layer.surface);
        if area.width == 0 || area.height == 0 {
            continue;
        }
        if col < area.x || col >= area.x + area.width || row < area.y || row >= area.y + area.height
        {
            continue;
        }
        // API v2: absolute-coordinate hit-rects the guest declared take priority
        // over the line-walked Menu/Tabs hits. API v3 (§4): only these declared
        // hits carry an owner, so only they can route a `UiEvent::Action`.
        for hit in &layer.hits {
            if col >= hit.x
                && col < hit.x.saturating_add(hit.width)
                && row >= hit.y
                && row < hit.y.saturating_add(hit.height)
            {
                return Some(UiPluginHit {
                    action_id: Some(hit.action_id.clone()),
                    consumes_mouse: layer.consumes_mouse,
                    declared_hit_owner: Some((
                        entry.plugin_id.clone(),
                        entry.module_id.clone(),
                        entry.negotiated_version,
                    )),
                    surface: layer.surface,
                });
            }
        }
        let mut line_y = area.y;
        for node in &layer.nodes {
            let (action, next_line_y) = ui_plugin_node_action_at(app, area, line_y, node, col, row);
            if action.is_some() || layer.consumes_mouse {
                return Some(UiPluginHit {
                    action_id: action,
                    consumes_mouse: layer.consumes_mouse,
                    declared_hit_owner: None,
                    surface: layer.surface,
                });
            }
            line_y = next_line_y;
            if line_y >= area.y + area.height {
                break;
            }
        }
    }
    None
}

fn render_ui_plugin_layers(app: &AppState, frame: &mut Frame) {
    let usage = app
        .ui_surface_layers
        .iter()
        .find(|entry| entry.layer.surface == UiSurface::UsageStrip);
    tracing::info!(
        layers = app.ui_surface_layers.len(),
        usage_nodes = usage.map(|entry| entry.layer.nodes.len()).unwrap_or(0),
        usage_rect = ?usage.map(|entry| ui_plugin_surface_rect(app, frame.area(), entry.layer.surface)),
        "render_ui_plugin_layers"
    );
    // `ui_surface_layers` is ordered topmost-owner-first (§4:
    // `(z_index desc, plugin_id asc, module_id asc)`); paint bottom-to-top so the
    // topmost layer ends up drawn last (visually on top), so walk it reversed.
    for entry in app.ui_surface_layers.iter().rev() {
        let area = ui_plugin_surface_rect(app, frame.area(), entry.layer.surface);
        if area.width == 0 || area.height == 0 {
            continue;
        }
        render_ui_plugin_layer(app, frame, area, &entry.layer);
    }
}

fn render_ui_plugin_layer(app: &AppState, frame: &mut Frame, area: Rect, layer: &UiLayer) {
    let mut line_y = area.y;
    for node in &layer.nodes {
        match node {
            UiNode::Line { spans } => {
                if line_y >= area.y + area.height {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(plugin_line(app, spans)),
                    Rect::new(area.x, line_y, area.width, 1),
                );
                line_y = line_y.saturating_add(1);
            }
            UiNode::Status { left, right } => {
                if line_y >= area.y + area.height {
                    break;
                }
                let mut combined = left.clone();
                if !right.is_empty() {
                    combined.push(UiTextSpan {
                        text: " ".into(),
                        role: Some("muted".into()),
                        ..Default::default()
                    });
                    combined.extend(right.clone());
                }
                frame.render_widget(
                    Paragraph::new(plugin_line(app, &combined)),
                    Rect::new(area.x, line_y, area.width, 1),
                );
                line_y = line_y.saturating_add(1);
            }
            UiNode::Tabs { items, selected } => {
                if line_y >= area.y + area.height {
                    break;
                }
                let spans = items
                    .iter()
                    .enumerate()
                    .flat_map(|(idx, item)| {
                        let role = if idx == *selected {
                            Some("accent")
                        } else {
                            Some("muted")
                        };
                        [
                            UiTextSpan {
                                text: format!(" {} ", item.label),
                                role: role.map(str::to_string),
                                ..Default::default()
                            },
                            UiTextSpan {
                                text: " ".into(),
                                role: Some("muted".into()),
                                ..Default::default()
                            },
                        ]
                    })
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(plugin_line(app, &spans)),
                    Rect::new(area.x, line_y, area.width, 1),
                );
                line_y = line_y.saturating_add(1);
            }
            UiNode::Menu {
                title,
                items,
                selected,
            } => {
                if line_y < area.y + area.height {
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            title.clone(),
                            plugin_span_style(app, Some("accent")),
                        ))),
                        Rect::new(area.x, line_y, area.width, 1),
                    );
                    line_y = line_y.saturating_add(1);
                }
                for (idx, item) in items.iter().enumerate() {
                    if line_y >= area.y + area.height {
                        break;
                    }
                    let role = if idx == *selected {
                        Some("accent")
                    } else {
                        None
                    };
                    let spans = vec![UiTextSpan {
                        text: item.label.clone(),
                        role: role.map(str::to_string),
                        ..Default::default()
                    }];
                    frame.render_widget(
                        Paragraph::new(plugin_line(app, &spans)),
                        Rect::new(area.x, line_y, area.width, 1),
                    );
                    line_y = line_y.saturating_add(1);
                }
            }
        }
    }
}

fn render_notifications(app: &AppState, frame: &mut Frame, terminal_area: Rect) {
    let has_config_diagnostic = app.config_diagnostic.is_some();
    if let Some(message) = &app.config_diagnostic {
        render_config_diagnostic(frame, terminal_area, message, &app.palette);
    }
    let mut copy_feedback_offset = u16::from(has_config_diagnostic);
    let mut toast_rect = None;
    if let Some(toast) = &app.toast {
        if app.view.layout == ViewLayout::Mobile {
            render_mobile_toast_banner(
                frame,
                frame.area(),
                toast,
                has_config_diagnostic,
                &app.palette,
            );
        } else {
            // feat/native-dashes: keep the toast clear of the native usage strip
            // and goal panel so render matches the hit-area set in compute_view.
            let toast_anchor_area = toast_desktop_anchor_area(
                frame.area(),
                app.view.usage_dash_area,
                app.view.goal_dash_area,
            );
            render_toast_notification(
                frame,
                toast_anchor_area,
                toast,
                has_config_diagnostic,
                toast.position.unwrap_or(app.toast_config.herdr.position),
                &app.palette,
            );
            toast_rect = Some(toast_notification_rect(
                toast_anchor_area,
                toast,
                has_config_diagnostic,
                toast.position.unwrap_or(app.toast_config.herdr.position),
            ));
        }
        if app.view.layout == ViewLayout::Mobile {
            toast_rect = Some(mobile_toast_banner_rect(
                frame.area(),
                has_config_diagnostic,
            ));
        }
    }
    if let Some(feedback) = &app.copy_feedback {
        let area = if app.view.layout == ViewLayout::Mobile {
            frame.area()
        } else {
            terminal_area
        };
        if let Some(toast_rect) = toast_rect {
            copy_feedback_offset = copy_feedback_offset_for_toast(
                area,
                feedback,
                copy_feedback_offset,
                app.toast_config.clipboard.position,
                toast_rect,
            );
        }
        render_copy_feedback(
            frame,
            area,
            feedback,
            copy_feedback_offset,
            app.toast_config.clipboard.position,
            &app.palette,
        );
    }
}

fn copy_feedback_offset_for_toast(
    area: Rect,
    feedback: &crate::app::state::CopyFeedback,
    base_offset: u16,
    position: crate::config::ToastClipboardPosition,
    toast_rect: Rect,
) -> u16 {
    let feedback_rect = copy_feedback_rect(area, feedback, base_offset, position);
    if rects_overlap(feedback_rect, toast_rect) {
        base_offset.saturating_add(toast_rect.height)
    } else {
        base_offset
    }
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.x < b.x.saturating_add(b.width)
        && b.x < a.x.saturating_add(a.width)
        && a.y < b.y.saturating_add(b.height)
        && b.y < a.y.saturating_add(a.height)
}

fn dim_background(frame: &mut Frame, area: Rect) {
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let cell = &mut buf[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::DIM));
        }
    }
}

/// Floating overlay for navigate mode — appears at bottom of terminal area.
fn _build_hints(items: &[(&str, &str)], key_style: Style, dim_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    spans.push(Span::raw(" "));
    for (i, (k, desc)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", dim_style));
        }
        spans.push(Span::styled(k.to_string(), key_style));
        spans.push(Span::styled(format!(" {desc}"), dim_style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::keybind_help::keybind_help_groups;
    use super::scrollbar::scrollbar_thumb;
    use super::*;
    use crate::{app::state::ViewLayout, layout::PaneInfo, workspace::Workspace};
    use ratatui::style::Color;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn copy_feedback_offset_only_increases_when_toast_rect_overlaps() {
        let area = Rect::new(0, 0, 80, 24);
        let feedback = crate::app::state::CopyFeedback {
            message: "copied to clipboard".into(),
        };
        let toast = crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::Finished,
            title: "pi finished".into(),
            context: "workspace · 1".into(),
            position: None,
            target: None,
        };

        let bottom_right_toast = toast_notification_rect(
            area,
            &toast,
            false,
            crate::config::ToastHerdrPosition::BottomRight,
        );
        assert_eq!(
            copy_feedback_offset_for_toast(
                area,
                &feedback,
                0,
                crate::config::ToastClipboardPosition::TopCenter,
                bottom_right_toast,
            ),
            0
        );

        let bottom_center_toast = Rect::new(28, 21, 24, 3);
        assert_eq!(
            copy_feedback_offset_for_toast(
                area,
                &feedback,
                0,
                crate::config::ToastClipboardPosition::BottomCenter,
                bottom_center_toast,
            ),
            bottom_center_toast.height
        );
    }

    #[tokio::test]
    async fn focused_pane_cursor_wins_during_terminal_render() {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("test");
        let first_pane = ws.tabs[0].root_pane;
        let second_pane = ws.test_split(ratatui::layout::Direction::Horizontal);

        ws.insert_test_runtime(
            first_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left"),
        );
        ws.insert_test_runtime(
            second_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"r\r\nb"),
        );
        ws.tabs[0].layout.focus_pane(first_pane);

        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;

        compute_view(&mut app, Rect::new(0, 0, 80, 20));
        let focused = app
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == first_pane)
            .expect("focused pane info");

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();

        terminal
            .backend_mut()
            .assert_cursor_position((focused.inner_rect.x + 4, focused.inner_rect.y));
    }

    #[test]
    fn mobile_width_uses_header_and_full_width_terminal() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;

        compute_view(&mut app, Rect::new(0, 0, 44, 20));

        assert_eq!(app.view.layout, ViewLayout::Mobile);
        assert_eq!(app.view.sidebar_rect, Rect::default());
        assert_eq!(app.view.tab_bar_rect, Rect::default());
        assert_eq!(app.view.mobile_header_rect, Rect::new(0, 0, 44, 2));
        assert_eq!(app.view.terminal_area, Rect::new(0, 2, 44, 18));
        assert_eq!(app.view.mobile_menu_hit_area.height, 2);
        assert_eq!(
            app.view.mobile_menu_hit_area.x + app.view.mobile_menu_hit_area.width,
            44
        );
    }

    #[test]
    fn desktop_toast_hit_area_uses_full_frame_not_terminal_area() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.toast_config.herdr.position = crate::config::ToastHerdrPosition::TopLeft;
        app.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::Finished,
            title: "pi finished".into(),
            context: "one".into(),
            position: None,
            target: None,
        });

        compute_view(&mut app, Rect::new(0, 0, 100, 20));

        assert_eq!(app.view.layout, ViewLayout::Desktop);
        assert!(app.view.terminal_area.x > 0);
        assert_eq!(app.view.toast_hit_area.x, 0);
        assert_eq!(app.view.toast_hit_area.y, 0);
    }

    #[test]
    fn desktop_toast_hit_area_still_offsets_for_config_diagnostic() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.config_diagnostic = Some("config warning".into());
        app.toast_config.herdr.position = crate::config::ToastHerdrPosition::TopLeft;
        app.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::Finished,
            title: "pi finished".into(),
            context: "one".into(),
            position: None,
            target: None,
        });

        compute_view(&mut app, Rect::new(0, 0, 100, 20));

        assert_eq!(app.view.toast_hit_area.x, 0);
        assert_eq!(app.view.toast_hit_area.y, 1);
    }

    #[test]
    fn configured_mobile_width_threshold_controls_layout_switch() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;

        compute_view(&mut app, Rect::new(0, 0, 80, 20));
        assert_eq!(app.view.layout, ViewLayout::Desktop);

        app.mobile_width_threshold = 90;
        compute_view(&mut app, Rect::new(0, 0, 80, 20));
        assert_eq!(app.view.layout, ViewLayout::Mobile);
        assert_eq!(app.view.mobile_header_rect, Rect::new(0, 0, 80, 2));
        assert_eq!(app.view.terminal_area, Rect::new(0, 2, 80, 18));
    }

    #[test]
    fn product_announcement_renders_above_config_diagnostic() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::ProductAnnouncement;
        app.product_announcement = Some(crate::app::state::ProductAnnouncementState {
            version: "0.6.0".into(),
            id: "keybinding-v2".into(),
            title: "Keybinding syntax changed".into(),
            body: "### Update\n- Body".into(),
            scroll: 0,
            preview: false,
        });
        app.config_diagnostic = Some(
            "unsafe direct keybinding: keys.new_workspace = \"n\"\nunsafe direct keybinding: keys.new_tab = \"c\""
                .into(),
        );

        let area = Rect::new(0, 0, 44, 20);
        compute_view(&mut app, area);

        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let buffer = terminal.backend().buffer();

        let popup = centered_popup_rect(
            area,
            PRODUCT_ANNOUNCEMENT_MODAL_SIZE.0,
            PRODUCT_ANNOUNCEMENT_MODAL_SIZE.1,
        )
        .expect("announcement popup");
        let title_row = popup.y + 1;
        let row = buffer_row_text(buffer, Rect::new(0, title_row, area.width, 1), title_row);

        assert!(row.contains("Keybinding syntax changed"));
        assert!(!row.contains("config warning"));
    }

    #[test]
    fn compute_view_clamps_sidebar_width_to_configured_max() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.sidebar_max_width = 30;
        app.sidebar_width = 999;

        compute_view(&mut app, Rect::new(0, 0, 100, 20));

        assert_eq!(app.view.sidebar_rect.width, 30);
    }

    #[test]
    fn compute_view_clamps_sidebar_width_to_configured_min() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.sidebar_min_width = 22;
        app.sidebar_width = 5;

        compute_view(&mut app, Rect::new(0, 0, 100, 20));

        assert_eq!(app.view.sidebar_rect.width, 22);
    }

    #[test]
    fn collapsed_sidebar_keeps_active_workspace_highlight_in_terminal_mode() {
        let mut app = crate::app::state::AppState::test_new();
        app.sidebar_collapsed = true;
        app.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        app.active = Some(1);
        app.selected = 0;
        app.mode = Mode::Terminal;

        compute_view(&mut app, Rect::new(0, 0, 80, 20));

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let buffer = terminal.backend().buffer();

        let (ws_area, _, _) = collapsed_sidebar_sections(app.view.sidebar_rect);
        let active_row = ws_area.y + 1;
        let active_style = buffer[(ws_area.x, active_row)].style();

        assert_eq!(active_style.bg, Some(app.palette.surface_dim));
    }

    #[test]
    fn expanded_sidebar_workspace_rows_show_state_before_name_without_numbers() {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("one");
        let repo = temp_git_repo("main");
        ws.identity_cwd = repo.clone();
        let root_pane = ws.tabs[0].root_pane;
        ws.refresh_git_ahead_behind();

        app.workspaces = vec![ws];
        app.ensure_test_terminals();
        let root_terminal_id = app.workspaces[0].tabs[0].panes[&root_pane]
            .attached_terminal_id
            .clone();
        app.terminals.get_mut(&root_terminal_id).unwrap().cwd = repo.clone();
        app.selected = 0;
        app.mode = Mode::Navigate;

        compute_view(&mut app, Rect::new(0, 0, 80, 20));

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let buffer = terminal.backend().buffer();

        let card = app.view.workspace_card_areas[0].rect;
        let line1 = buffer_row_text(buffer, card, card.y);
        let line2 = buffer_row_text(buffer, card, card.y + 1);

        assert!(line1.starts_with(" · one"));
        assert!(!line1.contains("1 one"));
        assert_eq!(line2, "   main");

        std::fs::remove_dir_all(repo).ok();
    }

    #[test]
    fn tab_bar_dims_auto_named_tabs_and_emphasizes_custom_tabs() {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("test");
        let custom_tab = ws.test_add_tab(Some("logs"));
        ws.switch_tab(custom_tab);

        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;

        compute_view(&mut app, Rect::new(0, 0, 80, 20));

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let buffer = terminal.backend().buffer();

        let auto_rect = app.view.tab_hit_areas[0];
        let custom_rect = app.view.tab_hit_areas[1];
        let auto_style = buffer[(auto_rect.x + 1, auto_rect.y)].style();
        let custom_style = buffer[(custom_rect.x + 1, custom_rect.y)].style();

        assert_eq!(auto_style.fg, Some(app.palette.overlay0));
        assert!(auto_style.add_modifier.contains(Modifier::DIM));
        assert_eq!(custom_style.fg, Some(app.palette.panel_bg));
        assert!(custom_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tab_bar_uses_surface_dim_when_panel_background_resets() {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("test");
        let custom_tab = ws.test_add_tab(Some("logs"));
        ws.switch_tab(custom_tab);

        app.palette.panel_bg = Color::Reset;
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;

        compute_view(&mut app, Rect::new(0, 0, 80, 20));

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let buffer = terminal.backend().buffer();

        let custom_rect = app.view.tab_hit_areas[1];
        let custom_style = buffer[(custom_rect.x + 1, custom_rect.y)].style();

        assert_eq!(custom_style.bg, Some(app.palette.accent));
        assert_eq!(custom_style.fg, Some(app.palette.surface_dim));
        assert!(custom_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn new_tab_button_tracks_rightmost_tab_when_tabs_fit() {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("test");
        ws.test_add_tab(Some("logs"));

        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;

        compute_view(&mut app, Rect::new(0, 0, 80, 20));

        let last_visible = app
            .view
            .tab_hit_areas
            .iter()
            .rev()
            .find(|rect| rect.width > 0)
            .copied()
            .expect("last visible tab");

        assert_eq!(
            app.view.new_tab_hit_area.x,
            last_visible.x + last_visible.width
        );
    }

    #[test]
    fn tab_bar_shows_scroll_controls_when_tabs_overflow() {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("test");
        for name in ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta"] {
            ws.test_add_tab(Some(name));
        }

        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.tab_scroll_follow_active = false;
        app.tab_scroll = 2;

        compute_view(&mut app, Rect::new(0, 0, 65, 20));

        assert!(app.view.tab_scroll_left_hit_area.width > 0);
        assert!(app.view.tab_scroll_right_hit_area.width > 0);
        assert_eq!(app.view.tab_hit_areas[0].width, 0);
        assert_eq!(app.view.tab_hit_areas[1].width, 0);
        assert!(app.view.tab_hit_areas[2].width > 0);
        assert!(app.view.new_tab_hit_area.width > 0);

        let last_visible = app
            .view
            .tab_hit_areas
            .iter()
            .rev()
            .find(|rect| rect.width > 0)
            .copied()
            .expect("last visible tab");

        assert_eq!(
            app.view.tab_scroll_right_hit_area.x,
            last_visible.x + last_visible.width
        );
        assert_eq!(
            app.view.new_tab_hit_area.x,
            app.view.tab_scroll_right_hit_area.x + app.view.tab_scroll_right_hit_area.width
        );
    }

    #[test]
    fn tab_bar_clamps_manual_scroll_at_last_visible_tab() {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("test");
        for name in [
            "one", "two", "three", "four", "five", "six", "seven", "eight",
        ] {
            ws.test_add_tab(Some(name));
        }

        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.tab_scroll_follow_active = false;
        app.tab_scroll = usize::MAX;

        compute_view(&mut app, Rect::new(0, 0, 65, 20));

        let last_idx = app.workspaces[0].tabs.len() - 1;
        assert!(app.view.tab_hit_areas[last_idx].width > 0);
        let clamped_scroll = app.tab_scroll;

        app.scroll_tabs_right();

        assert_eq!(app.tab_scroll, clamped_scroll);
        assert!(app.view.tab_hit_areas[last_idx].width > 0);
    }

    #[test]
    fn pane_scrollbar_rect_uses_reserved_rightmost_column() {
        let info = PaneInfo {
            id: crate::layout::PaneId::from_raw(1),
            rect: Rect::new(0, 0, 12, 8),
            inner_rect: Rect::new(1, 1, 9, 6),
            scrollbar_rect: Some(Rect::new(10, 1, 1, 6)),
            is_focused: true,
        };

        assert_eq!(pane_scrollbar_rect(&info), Some(Rect::new(10, 1, 1, 6)));
    }

    #[tokio::test]
    async fn compute_view_reserves_terminal_column_when_pane_scrollbar_is_visible() {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                12,
                4,
                4096,
                b"000000000000\r\n111111111111\r\n222222222222\r\n333333333333\r\n444444444444\r\n",
            ),
        );

        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;

        compute_view(&mut app, Rect::new(0, 0, 40, 12));

        let info = app.view.pane_infos.first().expect("pane info");
        assert_eq!(info.inner_rect.width + 1, app.view.terminal_area.width);
        assert_eq!(
            info.scrollbar_rect,
            Some(Rect::new(
                info.inner_rect.x + info.inner_rect.width,
                info.inner_rect.y,
                1,
                info.inner_rect.height,
            ))
        );
    }

    #[test]
    fn scrollbar_stays_hidden_without_scrollback() {
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            viewport_rows: 5,
        };

        assert!(!should_show_scrollbar(metrics));
    }

    #[test]
    fn scrollbar_shows_with_scrollback() {
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 20,
            viewport_rows: 5,
        };

        assert!(should_show_scrollbar(metrics));
    }

    #[test]
    fn scrollbar_thumb_reaches_bottom_when_scrolled_to_bottom() {
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 20,
            viewport_rows: 5,
        };
        let track = Rect::new(9, 4, 1, 5);

        let thumb = scrollbar_thumb(metrics, track).expect("thumb");
        assert_eq!(thumb.top + thumb.len, track.y + track.height);
    }

    #[test]
    fn scrollbar_offset_mapping_hits_top_middle_and_bottom() {
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 20,
            viewport_rows: 5,
        };
        let track = Rect::new(9, 4, 1, 5);

        assert_eq!(scrollbar_offset_from_row(metrics, track, 4), 20);
        assert_eq!(scrollbar_offset_from_row(metrics, track, 6), 10);
        assert_eq!(scrollbar_offset_from_row(metrics, track, 8), 0);
    }

    #[test]
    fn dragging_from_current_thumb_row_preserves_offset() {
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: 7,
            max_offset_from_bottom: 20,
            viewport_rows: 5,
        };
        let track = Rect::new(9, 4, 1, 8);
        let thumb = scrollbar_thumb(metrics, track).expect("thumb");
        let row = thumb.top + thumb.len / 2;
        let grab = scrollbar_thumb_grab_offset(metrics, track, row).expect("grab");

        assert_eq!(scrollbar_offset_from_drag_row(metrics, track, row, grab), 7);
    }

    fn buffer_row_text(buffer: &ratatui::buffer::Buffer, area: Rect, row: u16) -> String {
        (area.x..area.x + area.width)
            .map(|x| buffer[(x, row)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn temp_git_repo(branch: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("unix time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-ui-test-{unique}"));
        std::fs::create_dir_all(root.join(".git")).expect("create .git dir");
        std::fs::write(
            root.join(".git/HEAD"),
            format!("ref: refs/heads/{branch}\n"),
        )
        .expect("write HEAD");
        root
    }

    #[test]
    fn ui_plugin_hit_testing_marks_consumed_overlay() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.ui_surface_layers = vec![crate::ui_plugins::UiSurfaceLayer {
            plugin_id: "example.ui-wasm".into(),
            module_id: "overlay".into(),
            negotiated_version: 2,
            layer: UiLayer {
                surface: UiSurface::Overlay,
                z_index: 99,
                consumes_mouse: true,
                nodes: vec![UiNode::Line {
                    spans: vec![UiTextSpan {
                        text: "Consumes".into(),
                        role: Some("accent".into()),
                        bold: false,
                        underline: false,
                        italic: false,
                    }],
                }],
                hits: Vec::new(),
                data: None,
            },
        }];
        compute_view(&mut app, Rect::new(0, 0, 80, 20));
        let frame_area = ui_plugin_frame_area(&app);
        let hit = ui_plugin_hit_at(
            &app,
            frame_area,
            app.view.terminal_area.x,
            app.view.terminal_area.y,
        )
        .expect("plugin hit");
        assert!(hit.consumes_mouse);
        assert!(hit.action_id.is_none());
    }

    #[test]
    fn ui_plugin_action_hit_testing_finds_tab_action() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.ui_surface_layers = vec![crate::ui_plugins::UiSurfaceLayer {
            plugin_id: "example.ui-wasm".into(),
            module_id: "header".into(),
            negotiated_version: 2,
            layer: UiLayer {
                surface: UiSurface::Header,
                z_index: 1,
                consumes_mouse: false,
                nodes: vec![UiNode::Tabs {
                    items: vec![herdr_ui_plugin_api::UiActionRef {
                        id: "plugin:example.ui-wasm:fork".into(),
                        label: "Fork".into(),
                    }],
                    selected: 0,
                }],
                hits: Vec::new(),
                data: None,
            },
        }];
        compute_view(&mut app, Rect::new(0, 0, 80, 20));
        let header = ui_plugin_frame_area(&app);
        let hit = ui_plugin_hit_at(
            &app,
            header,
            app.view.terminal_area.x + 1,
            app.view.terminal_area.y,
        )
        .expect("plugin hit");
        assert_eq!(
            hit.action_id.as_deref(),
            Some("plugin:example.ui-wasm:fork")
        );
    }

    #[test]
    fn ui_plugin_surface_layers_render_into_header_and_footer() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.ui_surface_layers = vec![
            crate::ui_plugins::UiSurfaceLayer {
                plugin_id: "example.ui-wasm".into(),
                module_id: "header".into(),
                negotiated_version: 2,
                layer: UiLayer {
                    surface: UiSurface::Header,
                    z_index: 1,
                    consumes_mouse: false,
                    nodes: vec![UiNode::Line {
                        spans: vec![UiTextSpan {
                            text: "WASM HEADER".into(),
                            role: Some("accent".into()),
                            bold: false,
                            underline: false,
                            italic: false,
                        }],
                    }],
                    hits: Vec::new(),
                    data: None,
                },
            },
            crate::ui_plugins::UiSurfaceLayer {
                plugin_id: "example.ui-wasm".into(),
                module_id: "footer".into(),
                negotiated_version: 2,
                layer: UiLayer {
                    surface: UiSurface::Footer,
                    z_index: 1,
                    consumes_mouse: false,
                    nodes: vec![UiNode::Line {
                        spans: vec![UiTextSpan {
                            text: "WASM FOOTER".into(),
                            role: Some("accent".into()),
                            bold: false,
                            underline: false,
                            italic: false,
                        }],
                    }],
                    hits: Vec::new(),
                    data: None,
                },
            },
        ];

        compute_view(&mut app, Rect::new(0, 0, 80, 20));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20))
            .expect("test terminal");
        terminal.draw(|frame| render(&app, frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("WASM HEADER"));
        assert!(rendered.contains("WASM FOOTER"));
    }

    #[test]
    fn prefix_mode_renders_prefix_indicator() {
        let mut app = crate::app::state::AppState::test_new();
        app.mode = Mode::Prefix;
        app.view.terminal_area = ratatui::layout::Rect::new(0, 0, 60, 4);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 4))
            .expect("test terminal");

        terminal
            .draw(|frame| render_prefix_overlay(&app, frame, app.view.terminal_area))
            .expect("draw prefix overlay");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("PREFIX"));
    }

    #[test]
    fn keybind_help_shows_unset_for_optional_actions() {
        let app = crate::app::state::AppState::test_new();
        let groups = keybind_help_groups(&app);

        let workspace_tab = groups
            .iter()
            .find(|(name, _)| *name == "workspaces / tabs")
            .expect("workspace tab group")
            .1
            .clone();
        let panes = groups
            .iter()
            .find(|(name, _)| *name == "panes")
            .expect("panes group")
            .1
            .clone();

        assert!(workspace_tab
            .iter()
            .any(|(key, label)| key == "unset" && label.as_ref() == "previous workspace"));
        assert!(workspace_tab
            .iter()
            .any(|(key, label)| key == "unset" && label.as_ref() == "next workspace"));
        assert!(workspace_tab
            .iter()
            .any(|(key, label)| key == "unset" && label.as_ref() == "previous agent"));
        assert!(workspace_tab
            .iter()
            .any(|(key, label)| key == "unset" && label.as_ref() == "next agent"));
        assert!(workspace_tab
            .iter()
            .any(|(key, label)| key == "unset" && label.as_ref() == "focus agent 1-9"));
        assert!(workspace_tab
            .iter()
            .any(|(key, label)| key == "unset" && label.as_ref() == "switch workspace 1-9"));
        assert!(panes
            .iter()
            .any(|(key, label)| key == "prefix+h" && label.as_ref() == "focus pane left"));
        assert!(panes
            .iter()
            .any(|(key, label)| key == "prefix+j" && label.as_ref() == "focus pane down"));
        assert!(panes
            .iter()
            .any(|(key, label)| key == "prefix+k" && label.as_ref() == "focus pane up"));
        assert!(panes
            .iter()
            .any(|(key, label)| key == "prefix+l" && label.as_ref() == "focus pane right"));
    }

    #[test]
    fn keybind_help_shows_custom_command_descriptions() {
        let mut app = crate::app::state::AppState::test_new();
        app.keybinds.custom_commands = vec![
            crate::config::CustomCommandKeybind {
                bindings: crate::config::ActionKeybinds::prefix("alt+g"),
                label: "prefix+alt+g".to_string(),
                command: "lazygit".to_string(),
                action: crate::config::CustomCommandAction::Pane,
                description: Some("open lazygit".to_string()),
            },
            crate::config::CustomCommandKeybind {
                bindings: crate::config::ActionKeybinds::prefix("alt+h"),
                label: "prefix+alt+h".to_string(),
                command: "echo hello".to_string(),
                action: crate::config::CustomCommandAction::Shell,
                description: None,
            },
        ];

        let groups = keybind_help_groups(&app);
        let custom = groups
            .iter()
            .find(|(name, _)| *name == "custom")
            .expect("custom group")
            .1
            .clone();
        assert!(custom
            .iter()
            .any(|(key, label)| key == "prefix+alt+g" && label.as_ref() == "open lazygit"));
        assert!(custom
            .iter()
            .any(|(key, label)| key == "prefix+alt+h" && label.as_ref() == "custom command"));

        let rendered_help = keybind_help_lines(&app)
            .into_iter()
            .flat_map(|(_, line)| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<Vec<_>>()
            .join("");
        assert!(rendered_help.contains("open lazygit"));
        assert!(rendered_help.contains("custom command"));
    }
}
