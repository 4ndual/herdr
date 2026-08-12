use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, TabCreateParams, TabListParams,
    TabMoveParams, TabRenameParams, TabTarget, TabVisibilityParams,
};
use crate::app::{App, Mode};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_tab_list(&mut self, id: String, params: TabListParams) -> String {
        let tabs = if let Some(workspace_id) = params.workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            let Some(_) = self.state.workspaces.get(ws_idx) else {
                return workspace_not_found(id, &workspace_id);
            };
            self.tab_list_info_filtered(ws_idx, params.hidden)
        } else {
            let mut tabs = Vec::new();
            for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                for tab_idx in 0..ws.tabs.len() {
                    if let Some(tab) = self.tab_info(ws_idx, tab_idx) {
                        if params.hidden.is_none_or(|hidden| tab.hidden == hidden) {
                            tabs.push(tab);
                        }
                    }
                }
            }
            tabs
        };

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_get(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_create(&mut self, id: String, params: TabCreateParams) -> String {
        let TabCreateParams {
            workspace_id,
            cwd,
            focus,
            label,
            env,
        } = params;
        let ws_idx = if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            ws_idx
        } else if let Some(active) = self.state.active {
            active
        } else {
            return encode_error(id, "workspace_not_found", "no active workspace");
        };
        let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        });
        let (rows, cols) = self.state.estimate_pane_size();
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let extra_env = match super::env::normalize_launch_env(env) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab(
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
                    extra_env,
                )
            });
        match result {
            Ok((tab_idx, terminal, runtime)) => {
                self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                self.state.terminals.insert(terminal.id.clone(), terminal);
                self.state.remove_alias_shadowed_by_new_pane(
                    self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
                );
                if let Some(label) = label {
                    let workspace_id = self.state.workspaces[ws_idx].id.clone();
                    let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
                        crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
                    });
                    if let Some(tab) = self
                        .state
                        .workspaces
                        .get_mut(ws_idx)
                        .and_then(|ws| ws.tabs.get_mut(tab_idx))
                    {
                        tab.set_custom_name(label);
                        crate::logging::tab_renamed(&workspace_id, &tab_id);
                    }
                }
                if focus {
                    self.state.switch_workspace_tab(ws_idx, tab_idx);
                    self.state.mode = Mode::Terminal;
                }
                self.schedule_session_save();
                self.emit_tab_created_events(ws_idx, tab_idx);
                encode_success(
                    id,
                    self.tab_created_result(ws_idx, tab_idx)
                        .expect("new tab should produce a complete create response"),
                )
            }
            Err(err) => encode_error(id, "tab_create_failed", err.to_string()),
        }
    }

    /// Drain and apply a pending historial row action (restore / fork / delete)
    /// recorded by the mouse handler. The historial plugin only declares these
    /// hit-rects; the host owns the pane-close log and the resume machinery, so it
    /// resolves the clicked row's key back to its record and acts here. Returns
    /// true if it acted (the caller should re-render). Lives in the api/tabs
    /// module so it can reuse the tab-create helpers and stay a clean tab spawn.
    pub(crate) fn apply_pending_historial_action(&mut self) -> bool {
        let Some(action) = self.state.pending_historial_action.take() else {
            return false;
        };
        use crate::app::state::HistorialActionKind;
        // Resolve the source record by its stable "<ts>:<pane_id>" key. Read a
        // generous window so an older visible row is still found.
        let record = crate::pane_close_history::read_recent(256)
            .into_iter()
            .find(|r| format!("{}:{}", r.ts_unix_ms, r.pane_id) == action.key);
        let Some(record) = record else {
            tracing::warn!(key = %action.key, "historial action: source record not found");
            return false;
        };
        match action.kind {
            HistorialActionKind::Delete => {
                let removed =
                    crate::pane_close_history::delete_record(record.ts_unix_ms, &record.pane_id);
                // Republish immediately so the deleted row disappears this frame
                // instead of waiting for the next dash tick.
                self.write_historial_input_file();
                removed
            }
            HistorialActionKind::Restore | HistorialActionKind::Fork => {
                let argv = if matches!(action.kind, HistorialActionKind::Fork) {
                    record.fork_argv()
                } else {
                    record.resume_plan().map(|plan| plan.argv)
                };
                let Some(argv) = argv else {
                    tracing::warn!(
                        key = %action.key,
                        "historial restore: row is not restorable (no resume plan)"
                    );
                    return false;
                };
                self.spawn_historial_restore_tab(&record, argv)
            }
        }
    }

    /// Spawn a new focused tab in the active workspace running `argv` (a resume /
    /// fork command) in the closed pane's recorded cwd. Mirrors
    /// [`Self::handle_tab_create`]'s registration, but runs an explicit argv (the
    /// resume command) as the tab's process. Returns true on success.
    fn spawn_historial_restore_tab(
        &mut self,
        record: &crate::pane_close_history::PaneCloseRecord,
        argv: Vec<String>,
    ) -> bool {
        let Some(ws_idx) = self.state.active else {
            tracing::warn!("historial restore: no active workspace");
            return false;
        };
        let cwd = if record.cwd.trim().is_empty() {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        } else {
            PathBuf::from(&record.cwd)
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab_argv_command(
                    rows.max(4),
                    cols.max(10),
                    cwd,
                    &argv,
                    Vec::new(),
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                )
            });
        match result {
            Ok((tab_idx, mut terminal, runtime)) => {
                self.terminal_runtimes.insert(terminal.id.clone(), runtime);
                // Carry the closed pane's human name onto the restored pane.
                // Without this the pane has no manual label and its border falls
                // back to the display agent (e.g. "claude"), losing the name it
                // had.
                if let Some(pane_label) = record
                    .label
                    .clone()
                    .filter(|label| !label.trim().is_empty())
                {
                    terminal.set_manual_label(pane_label);
                }
                self.state.terminals.insert(terminal.id.clone(), terminal);
                self.state.remove_alias_shadowed_by_new_pane(
                    self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
                );
                let label = record
                    .label
                    .clone()
                    .filter(|l| !l.trim().is_empty())
                    .or_else(|| record.agent.clone())
                    .unwrap_or_else(|| "restored".to_string());
                if let Some(tab) = self
                    .state
                    .workspaces
                    .get_mut(ws_idx)
                    .and_then(|ws| ws.tabs.get_mut(tab_idx))
                {
                    tab.set_custom_name(label);
                }
                self.state.switch_workspace_tab(ws_idx, tab_idx);
                self.state.mode = Mode::Terminal;
                self.schedule_session_save();
                self.emit_tab_created_events(ws_idx, tab_idx);
                true
            }
            Err(err) => {
                tracing::warn!(error = %err, "historial restore: tab create failed");
                false
            }
        }
    }

    pub(super) fn handle_tab_focus(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        self.state.switch_workspace_tab(ws_idx, tab_idx);
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_rename(&mut self, id: String, params: TabRenameParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
            crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
        });
        let Some(tab) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
        else {
            return tab_not_found(id, &params.tab_id);
        };
        tab.set_custom_name(params.label.clone());
        crate::logging::tab_renamed(&workspace_id, &tab_id);
        if self.state.active == Some(ws_idx) {
            // Reflow the tab bar so the new label width takes effect immediately.
            // The tab bar renders into cached hit areas; without this refresh the
            // old geometry lingers until the next refresh (e.g. a tab switch),
            // leaving the visible label stale. Mirrors handle_tab_move.
            self.state.refresh_tab_bar_view();
        }
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabRenamed,
            data: EventData::TabRenamed {
                tab_id: self.public_tab_id(ws_idx, tab_idx).unwrap(),
                workspace_id: self.public_workspace_id(ws_idx),
                label: params.label,
            },
        });
        let tab = self.tab_info(ws_idx, tab_idx).unwrap();

        encode_success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_move(&mut self, id: String, params: TabMoveParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &params.tab_id);
        };
        if params.insert_index > ws.tabs.len() {
            return encode_error(
                id,
                "tab_move_failed",
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let tab_id = self
            .public_tab_id(ws_idx, tab_idx)
            .unwrap_or_else(|| crate::workspace::public_tab_id_for_number(&ws.id, tab_idx + 1));
        let workspace_id = self.public_workspace_id(ws_idx);
        let insert_index = params.insert_index;
        let moved = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.move_tab(tab_idx, insert_index));
        let tabs = self.tab_list_info(ws_idx);
        if moved {
            self.schedule_session_save();
            if self.state.active == Some(ws_idx) {
                self.state.tab_scroll_follow_active = true;
                self.state.refresh_tab_bar_view();
            }
            self.emit_event(EventEnvelope {
                event: EventKind::TabMoved,
                data: EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index,
                    tabs: tabs.clone(),
                },
            });
        }

        encode_success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_close(&mut self, id: String, target: TabTarget) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let closes_workspace = ws.tabs.len() <= 1;
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        let pane_ids = ws
            .tabs
            .get(tab_idx)
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default();
        // fork(archive-dash): record each pane before it is torn down. Upstream
        // 0.8.0 turned closing the last tab into a workspace close instead of a
        // `tab_close_failed` error, so the panes are destroyed on BOTH paths and
        // the staging has to happen before the branch, not after the old guard.
        // Staged by value here because upstream's workspace-close arm consumes
        // `pane_ids`.
        self.state
            .stage_pane_close_history(ws_idx, pane_ids.iter().copied());

        if closes_workspace {
            if self.state.confirm_implicit_worktree_group_close(ws_idx) {
                return encode_error(
                    id,
                    "confirmation_required",
                    "closing this tab would close a worktree group",
                );
            }
            let workspace = self.workspace_info(ws_idx);
            self.state.selected = ws_idx;
            self.state.close_selected_workspace();
            self.state.remove_plugin_pane_records(pane_ids);
            self.shutdown_detached_terminal_runtimes();
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id: workspace_id.clone(),
                },
            });
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
            return encode_success(id, ResponseResult::Ok {});
        }

        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        if !ws.close_tab(tab_idx) {
            return encode_error(
                id,
                "tab_close_failed",
                format!("tab {} could not be closed", target.tab_id),
            );
        }
        self.state.remove_plugin_pane_records(pane_ids);
        self.state.remove_unattached_terminal_ids(terminal_ids);
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::TabClosed,
            data: EventData::TabClosed {
                tab_id,
                workspace_id,
            },
        });

        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_tab_show(&mut self, id: String, params: TabVisibilityParams) -> String {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let changed = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.set_tab_hidden(tab_idx, false));
        if !changed {
            return tab_not_found(id, &params.tab_id);
        }
        if params.focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = Mode::Terminal;
        }
        self.state.mark_session_dirty();
        if self.state.active == Some(ws_idx) {
            self.state.refresh_tab_bar_view();
        }
        self.schedule_session_save();
        let tab = self.tab_info(ws_idx, tab_idx).expect("shown tab exists");
        encode_success(id, ResponseResult::TabInfo { tab })
    }

    fn tab_list_info_filtered(
        &self,
        ws_idx: usize,
        hidden: Option<bool>,
    ) -> Vec<crate::api::schema::TabInfo> {
        self.state
            .workspaces
            .get(ws_idx)
            .map(|ws| {
                (0..ws.tabs.len())
                    .filter_map(|idx| self.tab_info(ws_idx, idx))
                    .filter(|tab| hidden.is_none_or(|hidden| tab.hidden == hidden))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn tab_list_info(&self, ws_idx: usize) -> Vec<crate::api::schema::TabInfo> {
        self.tab_list_info_filtered(ws_idx, None)
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

fn tab_not_found(id: String, tab_id: &str) -> String {
    encode_error(id, "tab_not_found", format!("tab {tab_id} not found"))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::{
        api::schema::SuccessResponse,
        config::{Config, ShellModeConfig},
        workspace::Workspace,
    };

    #[test]
    fn api_tab_close_last_tab_closes_workspace_and_emits_both_events() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("tabs")];
        app.state.active = Some(0);
        app.state.selected = 0;
        let tab_id = app.public_tab_id(0, 0).unwrap();
        let workspace_id = app.public_workspace_id(0);

        let response = app.handle_tab_close(
            "req".into(),
            TabTarget {
                tab_id: tab_id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.result, ResponseResult::Ok {});
        assert!(app.state.workspaces.is_empty());
        assert!(app.state.active.is_none());
        let events = event_hub.events_after(0);
        assert_eq!(
            events
                .iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            [EventKind::TabClosed, EventKind::WorkspaceClosed]
        );
        assert!(matches!(
            &events[0].1.data,
            EventData::TabClosed {
                tab_id: closed_tab_id,
                workspace_id: closed_workspace_id,
            } if closed_tab_id == &tab_id && closed_workspace_id == &workspace_id
        ));
        assert!(matches!(
            &events[1].1.data,
            EventData::WorkspaceClosed {
                workspace_id: closed_workspace_id,
                workspace: Some(workspace),
            } if closed_workspace_id == &workspace_id
                && workspace.workspace_id == workspace_id
        ));
    }

    #[test]
    fn api_tab_move_reorders_tabs_in_target_workspace() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        let mut workspace = Workspace::test_new("tabs");
        workspace.test_add_tab(Some("two"));
        workspace.test_add_tab(Some("three"));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        let moved_root = app.state.workspaces[0].tabs[0].root_pane;
        let moved_id = app.public_tab_id(0, 0).unwrap();

        let response = app.handle_tab_move(
            "req".into(),
            TabMoveParams {
                tab_id: moved_id.clone(),
                insert_index: 3,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::TabList { tabs } = success.result else {
            panic!("expected tab list");
        };
        assert_eq!(app.state.workspaces[0].tabs[2].root_pane, moved_root);
        assert_eq!(tabs[2].tab_id, app.public_tab_id(0, 2).unwrap());
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index: 3,
                    tabs,
                } if tab_id == &moved_id
                    && workspace_id == &app.public_workspace_id(0)
                    && tabs[2].tab_id == moved_id
            )
        }));
    }

    #[test]
    fn api_tab_rename_reflows_active_tab_bar() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        let workspace = Workspace::test_new("tabs");
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.view.tab_bar_rect = ratatui::layout::Rect::new(0, 0, 60, 1);
        app.state.refresh_tab_bar_view();

        let tab_id = app.public_tab_id(0, 0).unwrap();
        let width_before = app.state.view.tab_hit_areas[0].width;

        app.handle_tab_rename(
            "req".into(),
            TabRenameParams {
                tab_id,
                label: "a much longer custom tab label".into(),
            },
        );

        let width_after = app.state.view.tab_hit_areas[0].width;
        assert!(
            width_after > width_before,
            "tab bar should reflow to the new label width immediately: \
             before={width_before}, after={width_after}"
        );
    }

    #[tokio::test]
    async fn tab_create_follows_cached_focused_pane_cwd_without_runtime() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        let workspace = Workspace::test_new("tabs");
        let focused_pane = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        let cached_cwd = std::env::temp_dir();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(focused_pane)
            .cloned()
            .unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = cached_cwd.clone();

        let response = app.handle_tab_create(
            "req".into(),
            TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::TabCreated { .. }));
        let created = &app.state.workspaces[0].tabs[1];
        let created_terminal_id = created.terminal_id(created.root_pane).unwrap();
        let created_cwd = &app.state.terminals.get(created_terminal_id).unwrap().cwd;
        assert_eq!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&cached_cwd)
        );
        shutdown_test_runtimes(&mut app);
    }

    #[tokio::test]
    async fn historial_restore_keeps_the_closed_pane_name() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        app.state.workspaces = vec![Workspace::test_new("tabs")];
        app.state.active = Some(0);
        app.state.selected = 0;

        let record = crate::pane_close_history::PaneCloseRecord {
            ts_unix_ms: 1,
            pane_id: "w1p1".into(),
            label: Some("codeword".into()),
            agent: Some("claude".into()),
            session_source: None,
            session_kind: None,
            session_value: None,
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            workspace_id: "w1".into(),
            workspace_name: None,
            tab_name: None,
            first_prompt: None,
            last_prompt: None,
        };
        let argv = vec![exiting_test_command().to_string()];

        assert!(app.spawn_historial_restore_tab(&record, argv));

        let restored = &app.state.workspaces[0].tabs[1];
        let terminal_id = restored.terminal_id(restored.root_pane).unwrap();
        let terminal = app.state.terminals.get(terminal_id).unwrap();
        // Without the manual label the border would fall back to the display
        // agent ("claude"), which is what the restore used to show.
        assert_eq!(terminal.border_label(true).as_deref(), Some("codeword"));
        shutdown_test_runtimes(&mut app);
    }
}
