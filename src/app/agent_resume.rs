use std::time::Instant;

use bytes::Bytes;
use ratatui::layout::Rect;

use super::App;

struct PendingAgentResumeCandidate {
    pane_id: crate::layout::PaneId,
    terminal_id: crate::terminal::TerminalId,
    cwd: std::path::PathBuf,
    plan: crate::agent_resume::AgentResumePlan,
    rows: u16,
    cols: u16,
}

impl App {
    pub(crate) fn has_pending_agent_resumes(&self) -> bool {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.pending_agent_resume_plan.is_some())
    }

    pub(crate) fn sync_pending_agent_resume_deadline(&mut self, now: Instant) {
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
            return;
        }
        if self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
            return;
        }
        self.pending_agent_resume_deadline
            .get_or_insert(now + super::PENDING_AGENT_RESUME_THEME_WAIT);
    }

    pub(crate) fn pending_agent_resume_due(&self, now: Instant) -> bool {
        self.pending_agent_resume_deadline
            .is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn start_pending_agent_resumes(&mut self, allow_empty_theme: bool) -> bool {
        let pending = self.pending_agent_resume_candidates();
        let mut changed = false;
        for PendingAgentResumeCandidate {
            pane_id,
            terminal_id,
            cwd,
            plan,
            rows,
            cols,
        } in pending
        {
            if self.terminal_runtimes.get(&terminal_id).is_some() {
                continue;
            }
            changed |= self.start_pending_agent_resume(
                pane_id,
                terminal_id,
                cwd,
                plan,
                rows,
                cols,
                allow_empty_theme,
            );
        }

        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() || self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
        }
        changed
    }

    fn pending_agent_resume_candidates(&self) -> Vec<PendingAgentResumeCandidate> {
        let terminal_area = self.state.view.terminal_area;
        if terminal_area.width == 0 || terminal_area.height == 0 {
            return Vec::new();
        };

        let mut pending = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for (tab_idx, tab) in ws.tabs.iter().enumerate() {
                for info in
                    self.pending_agent_resume_pane_infos(ws_idx, tab_idx, tab, terminal_area)
                {
                    let Some(pane) = tab.panes.get(&info.id) else {
                        continue;
                    };
                    if self
                        .terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .is_some()
                    {
                        continue;
                    }
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    let Some(plan) = terminal.pending_agent_resume_plan.clone() else {
                        continue;
                    };
                    pending.push(PendingAgentResumeCandidate {
                        pane_id: info.id,
                        terminal_id: pane.attached_terminal_id.clone(),
                        cwd: terminal.cwd.clone(),
                        plan,
                        rows: info.inner_rect.height,
                        cols: info.inner_rect.width,
                    });
                }
            }
        }
        pending
    }

    fn pending_agent_resume_pane_infos(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        tab: &crate::workspace::Tab,
        terminal_area: Rect,
    ) -> Vec<crate::layout::PaneInfo> {
        let mut pane_infos = derived_pending_agent_resume_pane_infos(
            tab,
            terminal_area,
            self.state.pane_borders,
            self.state.pane_gaps,
        );

        if self.state.active == Some(ws_idx)
            && self
                .state
                .workspaces
                .get(ws_idx)
                .is_some_and(|ws| tab_idx == ws.active_tab_index())
        {
            for visible_info in &self.state.view.pane_infos {
                if let Some(info) = pane_infos
                    .iter_mut()
                    .find(|info| info.id == visible_info.id)
                {
                    *info = visible_info.clone();
                } else {
                    pane_infos.push(visible_info.clone());
                }
            }
        }

        pane_infos
    }

    pub(crate) fn start_pending_agent_resume_for_terminal(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        if self.terminal_runtimes.get(terminal_id).is_some() {
            return false;
        }
        let Some((pane_id, cwd, plan)) = self.state.workspaces.iter().find_map(|ws| {
            ws.tabs.iter().find_map(|tab| {
                tab.layout.pane_ids().into_iter().find_map(|pane_id| {
                    let pane = tab.panes.get(&pane_id)?;
                    if &pane.attached_terminal_id != terminal_id {
                        return None;
                    }
                    let terminal = self.state.terminals.get(terminal_id)?;
                    Some((
                        pane_id,
                        terminal.cwd.clone(),
                        terminal.pending_agent_resume_plan.clone()?,
                    ))
                })
            })
        }) else {
            return false;
        };

        let changed = self.start_pending_agent_resume(
            pane_id,
            terminal_id.clone(),
            cwd,
            plan,
            rows,
            cols,
            allow_empty_theme,
        );
        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
        }
        changed
    }

    fn start_pending_agent_resume(
        &mut self,
        pane_id: crate::layout::PaneId,
        terminal_id: crate::terminal::TerminalId,
        cwd: std::path::PathBuf,
        plan: crate::agent_resume::AgentResumePlan,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        let host_terminal_theme = self.state.host_terminal_theme;
        if host_terminal_theme.is_empty() && !allow_empty_theme {
            return false;
        }

        let Some(resume_command) = shell_command_from_argv(&plan.argv) else {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                "failed to start deferred agent resume with empty argv"
            );
            return false;
        };
        // The pane's OWN launch extras, not an empty vec: a harness pane resumed
        // with only the identity vars looks for its session under ~/.claude and
        // starts a brand-new conversation instead (DONE-E defect B).
        let resume_extra_env = self
            .state
            .terminals
            .get(&terminal_id)
            .map(|terminal| terminal.launch_env.clone())
            .unwrap_or_default();
        let Some(launch_env) = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.pane_launch_env(ws_idx, pane_id, resume_extra_env))
        else {
            return false;
        };

        let should_arm_transcript = auto_continue_should_arm(&plan, &cwd);
        let arm_auto_continue =
            self.auto_continue_on_restore && self.restart_restore_detected && should_arm_transcript;
        tracing::info!(
            terminal = %terminal_id,
            agent = %plan.agent,
            dedupe_key = %plan.dedupe_key,
            cfg_on = self.auto_continue_on_restore,
            restart_detected = self.restart_restore_detected,
            transcript_interrupted = should_arm_transcript,
            arm = arm_auto_continue,
            "auto-continue arm decision"
        );

        let runtime = match crate::terminal::TerminalRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            self.state.pane_scrollback_limit_bytes,
            host_terminal_theme,
            self.state.host_terminal_appearance,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            &launch_env,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent = %plan.agent,
                    err = %err,
                    "failed to start shell for deferred agent resume"
                );
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    terminal.clear_agent_runtime_identity_after_respawn();
                }
                return false;
            }
        };

        let mut input = resume_command;
        input.push('\r');
        if let Err(err) = runtime.try_send_bytes(Bytes::from(input)) {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                err = %err,
                "failed to send deferred agent resume command to shell"
            );
            runtime.shutdown();
            return false;
        }

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.pending_agent_resume_plan = None;
            terminal.respawn_shell_on_exit = false;
            if arm_auto_continue {
                terminal.auto_continue_after_restart = Some(
                    crate::terminal::AutoContinueAfterRestart::armed(std::time::Instant::now()),
                );
            }
        }
        if arm_auto_continue {
            // Wake the loop promptly so the auto-continue sweep starts running.
            self.auto_continue_deadline = Some(std::time::Instant::now());
        }
        true
    }
}

/// Whether a resume plan should arm restart auto-continue: it must be a claude
/// `id`-kind session whose transcript's last turn was interrupted. The session
/// ref is recovered from the plan's dedupe key
/// (`source\0agent\0kind\0value`).
fn auto_continue_should_arm(
    plan: &crate::agent_resume::AgentResumePlan,
    cwd: &std::path::Path,
) -> bool {
    let mut parts = plan.dedupe_key.split('\u{0}');
    let (_source, _agent, kind, value) =
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
            _ => return false,
        };
    // `AgentSessionRefKind` is serialized here via its Debug form ("Id"/"Path").
    if kind != "Id" {
        return false;
    }
    let cwd = cwd.to_string_lossy();
    let Some(path) = crate::pane_close_history::session_transcript_path(
        Some("claude"),
        Some("id"),
        Some(value),
        &cwd,
    ) else {
        return false;
    };
    crate::pane_close_history::transcript_last_turn_interrupted(&path)
}

/// Short, single-line nudge sent to a resumed agent whose turn was interrupted
/// by a restart. Kept short on purpose: long text can be delivered as a
/// bracketed paste whose trailing Enter is swallowed (agent_prompt_stalled),
/// which the bare-Enter retry then recovers.
const AUTO_CONTINUE_NUDGE_TEXT: &str = "Continue the interrupted turn — herdr restarted.";

/// How often the auto-continue sweep runs while any pane is armed.
const AUTO_CONTINUE_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
/// Fallback readiness delay for a resumed claude that boots straight to its idle
/// prompt with no observable non-idle blip. Slow or visible boots take the
/// `saw_activity` path sooner; fast boots are already up well before this, so a
/// generous delay just guarantees claude is genuinely accepting input.
const AUTO_CONTINUE_READY_SETTLE: std::time::Duration = std::time::Duration::from_secs(4);
/// After the nudge, if the agent is still idle this long, send a bare Enter to
/// recover a swallowed submission.
const AUTO_CONTINUE_STALL_RETRY: std::time::Duration = std::time::Duration::from_millis(1500);
/// After the retry, stop watching this pane once this long has passed since the
/// nudge without the agent starting to work.
const AUTO_CONTINUE_GIVE_UP: std::time::Duration = std::time::Duration::from_millis(5000);
/// Overall cap: if a resumed pane never becomes ready, retire it so the sweep
/// stops waking.
const AUTO_CONTINUE_OVERALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

impl App {
    /// Drive restart auto-continue for every armed pane: wait for the resumed
    /// agent to become ready, send a one-line continue nudge, recover a swallowed
    /// submission with a bare Enter, and retire the entry once the agent resumes
    /// working (or a timeout elapses). Returns whether anything changed that
    /// warrants a repaint.
    pub(crate) fn tick_auto_continue_after_restart(&mut self, now: std::time::Instant) -> bool {
        let armed_ids: Vec<crate::terminal::TerminalId> = self
            .state
            .terminals
            .iter()
            .filter(|(_, terminal)| terminal.auto_continue_after_restart.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        if armed_ids.is_empty() {
            self.auto_continue_deadline = None;
            return false;
        }

        let mut changed = false;
        for terminal_id in armed_ids {
            let Some(terminal) = self.state.terminals.get(&terminal_id) else {
                continue;
            };
            let Some(ac) = terminal.auto_continue_after_restart else {
                continue;
            };
            let is_claude = terminal.effective_known_agent() == Some(crate::detect::Agent::Claude);
            let idle = terminal.state == crate::detect::AgentState::Idle;
            let has_runtime = self.terminal_runtimes.get(&terminal_id).is_some();

            if now.saturating_duration_since(ac.armed_at) > AUTO_CONTINUE_OVERALL_TIMEOUT {
                self.clear_auto_continue(&terminal_id);
                changed = true;
                continue;
            }

            // The resumed shell/agent usually passes through a non-idle screen
            // (shell prompt, agent loading) before it is truly ready; observing
            // that clears the idle placeholder seeded at restore time and is the
            // fast path to readiness.
            let saw_activity = ac.saw_activity || !idle;
            if saw_activity && !ac.saw_activity {
                self.mutate_auto_continue(&terminal_id, |ac| ac.saw_activity = true);
            }

            // A resumed claude can also boot straight to its idle prompt without a
            // detectable non-idle blip (a fast, clean resume). In that case fall
            // back to a settle delay: once the agent has had time to come
            // up and is sitting at an idle claude screen, treat it as ready. Slow
            // or visible boots take the saw_activity path well before this fires.
            let settled = now.saturating_duration_since(ac.armed_at) >= AUTO_CONTINUE_READY_SETTLE;
            let ready = is_claude && idle && has_runtime && (saw_activity || settled);

            match ac.nudged_at {
                None => {
                    if ready && self.send_auto_continue_nudge(&terminal_id, false) {
                        self.mutate_auto_continue(&terminal_id, |ac| ac.nudged_at = Some(now));
                        changed = true;
                        tracing::info!(
                            terminal = %terminal_id,
                            saw_activity,
                            settled,
                            "auto-continue nudge sent"
                        );
                    }
                }
                Some(nudged_at) => {
                    if !idle {
                        // The agent picked up the nudge and is working again.
                        self.clear_auto_continue(&terminal_id);
                        changed = true;
                    } else if !ac.retried
                        && now.saturating_duration_since(nudged_at) > AUTO_CONTINUE_STALL_RETRY
                    {
                        self.send_auto_continue_nudge(&terminal_id, true);
                        self.mutate_auto_continue(&terminal_id, |ac| ac.retried = true);
                        changed = true;
                    } else if ac.retried
                        && now.saturating_duration_since(nudged_at) > AUTO_CONTINUE_GIVE_UP
                    {
                        self.clear_auto_continue(&terminal_id);
                        changed = true;
                    }
                }
            }
        }

        if self
            .state
            .terminals
            .values()
            .any(|terminal| terminal.auto_continue_after_restart.is_some())
        {
            self.auto_continue_deadline = Some(now + AUTO_CONTINUE_TICK_INTERVAL);
        } else {
            self.auto_continue_deadline = None;
        }
        changed
    }

    fn clear_auto_continue(&mut self, terminal_id: &crate::terminal::TerminalId) {
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            terminal.auto_continue_after_restart = None;
        }
    }

    fn mutate_auto_continue(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        update: impl FnOnce(&mut crate::terminal::AutoContinueAfterRestart),
    ) {
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            if let Some(ac) = terminal.auto_continue_after_restart.as_mut() {
                update(ac);
            }
        }
    }

    /// Send the continue nudge (`retry == false`) or a bare Enter to recover a
    /// swallowed submission (`retry == true`) to a resumed pane's runtime.
    /// Returns whether the bytes were queued.
    fn send_auto_continue_nudge(
        &self,
        terminal_id: &crate::terminal::TerminalId,
        retry: bool,
    ) -> bool {
        let Some(runtime) = self.terminal_runtimes.get(terminal_id) else {
            return false;
        };
        let bytes = if retry {
            Bytes::from_static(b"\r")
        } else {
            Bytes::from(crate::app::api_helpers::encode_api_submission(
                runtime,
                AUTO_CONTINUE_NUDGE_TEXT,
            ))
        };
        runtime.try_send_bytes(bytes).is_ok()
    }
}

fn derived_pending_agent_resume_pane_infos(
    tab: &crate::workspace::Tab,
    terminal_area: Rect,
    pane_borders: bool,
    pane_gaps: bool,
) -> Vec<crate::layout::PaneInfo> {
    crate::ui::apply_pane_chrome(tab.layout.panes(terminal_area), pane_borders, pane_gaps)
        .into_iter()
        .map(|mut info| {
            let pane_inner = crate::ui::pane_inner_rect(info.rect, info.borders);
            info.inner_rect = stable_terminal_inner_rect(pane_inner);
            info
        })
        .collect()
}

fn stable_terminal_inner_rect(pane_inner: Rect) -> Rect {
    if pane_inner.width <= 4 {
        return pane_inner;
    }

    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

fn shell_command_from_argv(argv: &[String]) -> Option<String> {
    let mut parts = argv.iter();
    let first = shell_quote(parts.next()?);
    let mut command = first;
    for part in parts {
        command.push(' ');
        command.push_str(&shell_quote(part));
    }
    Some(command)
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b'='
            )
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[cfg(unix)]
    fn long_running_test_argv() -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()]
    }

    #[cfg(unix)]
    fn marker_resume_test_argv() -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' 'restored agent: shell quoted | marker'; sleep 5".into(),
        ]
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_waits_for_host_theme_before_launch() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: marker_resume_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(!app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());

        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };

        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
        let terminal = app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal should survive launch");
        assert!(terminal.pending_agent_resume_plan.is_none());
        assert!(!terminal.respawn_shell_on_exit);

        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pending resume should leave a shell runtime");
        let marker = "restored agent: shell quoted | marker";
        for _ in 0..20 {
            if runtime
                .snapshot_history()
                .is_some_and(|text| text.contains(marker))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            runtime
                .snapshot_history()
                .expect("runtime should expose terminal history")
                .contains(marker),
            "deferred restore should inject the resume argv into the restored shell"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_can_launch_after_theme_wait_expires() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(!app.start_pending_agent_resumes(false));
        assert!(app.start_pending_agent_resumes(true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_hidden_panes_with_current_terminal_area() {
        let mut app = test_app();
        let active_workspace = crate::workspace::Workspace::test_new("active");
        let active_pane = active_workspace.tabs[0].root_pane;
        let active_terminal = active_workspace.terminal_id(active_pane).cloned().unwrap();
        let hidden_workspace = crate::workspace::Workspace::test_new("hidden");
        let hidden_pane = hidden_workspace.tabs[0].root_pane;
        let hidden_terminal = hidden_workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = active_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![active_workspace, hidden_workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        for terminal_id in [&active_terminal, &hidden_terminal] {
            app.state
                .terminals
                .get_mut(terminal_id)
                .expect("test terminal should exist")
                .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: long_running_test_argv(),
                dedupe_key: format!("herdr:codex\0codex\0Id\0{terminal_id}"),
            });
        }
        app.pending_agent_resume_deadline =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));

        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&active_terminal).is_some());
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.pending_agent_resume_deadline.is_none(),
            "launched pending resumes should clear the wakeup deadline"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_inactive_tab_panes_with_current_terminal_area() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("tabs");
        let active_pane = workspace.tabs[0].root_pane;
        let inactive_tab = workspace.test_add_tab(Some("agents"));
        let inactive_pane = workspace.tabs[inactive_tab].root_pane;
        let inactive_terminal = workspace.tabs[inactive_tab]
            .terminal_id(inactive_pane)
            .cloned()
            .unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        assert!(app
            .state
            .workspaces
            .first()
            .and_then(|ws| ws.tabs[0].terminal_id(active_pane))
            .is_some());
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&inactive_terminal)
            .expect("inactive tab terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0inactive-tab-session".into(),
        });

        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&inactive_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&inactive_terminal)
                .expect("inactive tab terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "inactive tab restored panes should not wait for tab focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_zoom_hidden_active_tab_panes() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("zoomed");
        let hidden_pane = workspace.tabs[0].root_pane;
        let visible_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].zoomed = true;
        let hidden_terminal = workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: visible_pane,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&hidden_terminal)
            .expect("hidden zoom pane terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0zoom-hidden-session".into(),
        });

        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&hidden_terminal)
                .expect("hidden zoom pane terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "zoom-hidden restored panes should not wait for pane focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_uses_current_terminal_area_for_background_panes() {
        let mut app = test_app();
        let previous_workspace = crate::workspace::Workspace::test_new("previous");
        let previous_pane = previous_workspace.tabs[0].root_pane;
        let previous_terminal = previous_workspace
            .terminal_id(previous_pane)
            .cloned()
            .unwrap();
        let current_workspace = crate::workspace::Workspace::test_new("current");
        app.state.view.pane_infos = previous_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.state.workspaces = vec![previous_workspace, current_workspace];
        app.state.active = Some(1);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&previous_terminal)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(app.pending_agent_resume_deadline.is_some());
        assert!(app.start_pending_agent_resumes(false));
        assert!(app.terminal_runtimes.get(&previous_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&previous_terminal)
                .expect("previous terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "background restored panes should not wait for focus once terminal area is known"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_launches_with_inner_rect_size() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("split");
        let pane_id = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(app.start_pending_agent_resumes(false));
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .expect("pending resume should launch")
                .current_size(),
            (28, 98)
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    fn armed_auto_continue_app() -> (
        App,
        crate::terminal::TerminalId,
        tokio::sync::mpsc::Receiver<Bytes>,
    ) {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("resumed");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        let (runtime, rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.auto_continue_after_restart = Some(
            crate::terminal::AutoContinueAfterRestart::armed(std::time::Instant::now()),
        );
        (app, terminal_id, rx)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_continue_waits_for_activity_before_nudging() {
        let (mut app, terminal_id, mut rx) = armed_auto_continue_app();
        // Seeded idle placeholder with claude already labeled: must NOT nudge yet.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Idle,
            );

        app.tick_auto_continue_after_restart(std::time::Instant::now());

        assert!(rx.try_recv().is_err(), "no nudge before observed activity");
        let ac = app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .auto_continue_after_restart
            .unwrap();
        assert!(!ac.saw_activity);
        assert!(ac.nudged_at.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_continue_nudges_once_resumed_claude_is_ready() {
        let (mut app, terminal_id, mut rx) = armed_auto_continue_app();
        // Live detection first sees the resume shell/agent working (activity).
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Working,
            );
        app.tick_auto_continue_after_restart(std::time::Instant::now());
        assert!(rx.try_recv().is_err(), "working state does not nudge");
        assert!(
            app.state
                .terminals
                .get(&terminal_id)
                .unwrap()
                .auto_continue_after_restart
                .unwrap()
                .saw_activity
        );

        // Then claude becomes ready (idle): nudge fires exactly once.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Idle,
            );
        let nudged_at = std::time::Instant::now();
        app.tick_auto_continue_after_restart(nudged_at);

        let sent = rx.try_recv().expect("nudge should be sent");
        let mut expected = AUTO_CONTINUE_NUDGE_TEXT.as_bytes().to_vec();
        expected.push(b'\r');
        assert_eq!(sent, Bytes::from(expected));
        assert!(rx.try_recv().is_err(), "nudge sent only once");
        assert!(app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .auto_continue_after_restart
            .unwrap()
            .nudged_at
            .is_some());

        // Still idle after the stall window -> bare Enter retry.
        app.tick_auto_continue_after_restart(
            nudged_at + AUTO_CONTINUE_STALL_RETRY + std::time::Duration::from_millis(1),
        );
        assert_eq!(
            rx.try_recv().expect("retry enter"),
            Bytes::from_static(b"\r")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_continue_nudges_via_settle_when_resume_boots_straight_to_idle() {
        let (mut app, terminal_id, mut rx) = armed_auto_continue_app();
        // Fast, clean resume: claude comes up at its idle prompt with no observed
        // non-idle blip. saw_activity never trips, so the settle delay must fire.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Idle,
            );

        let armed_at = app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .auto_continue_after_restart
            .unwrap()
            .armed_at;

        // Before the settle delay: no nudge.
        app.tick_auto_continue_after_restart(
            armed_at + AUTO_CONTINUE_READY_SETTLE - std::time::Duration::from_millis(1),
        );
        assert!(rx.try_recv().is_err(), "no nudge before settle delay");
        assert!(
            !app.state
                .terminals
                .get(&terminal_id)
                .unwrap()
                .auto_continue_after_restart
                .unwrap()
                .saw_activity
        );

        // After the settle delay: nudge fires exactly once.
        app.tick_auto_continue_after_restart(
            armed_at + AUTO_CONTINUE_READY_SETTLE + std::time::Duration::from_millis(1),
        );
        let sent = rx.try_recv().expect("nudge should be sent after settle");
        let mut expected = AUTO_CONTINUE_NUDGE_TEXT.as_bytes().to_vec();
        expected.push(b'\r');
        assert_eq!(sent, Bytes::from(expected));
        assert!(rx.try_recv().is_err(), "nudge sent only once");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_continue_clears_once_agent_starts_working_on_nudge() {
        let (mut app, terminal_id, mut rx) = armed_auto_continue_app();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Working,
            );
        app.tick_auto_continue_after_restart(std::time::Instant::now());
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Idle,
            );
        app.tick_auto_continue_after_restart(std::time::Instant::now());
        assert!(rx.try_recv().is_ok(), "nudge sent");

        // Agent picks up the nudge and starts working -> entry retired.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Working,
            );
        app.tick_auto_continue_after_restart(std::time::Instant::now());
        assert!(app
            .state
            .terminals
            .get(&terminal_id)
            .unwrap()
            .auto_continue_after_restart
            .is_none());
        assert!(app.auto_continue_deadline.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn auto_continue_tick_is_noop_without_armed_panes() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("plain");
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        assert!(!app.tick_auto_continue_after_restart(std::time::Instant::now()));
        assert!(app.auto_continue_deadline.is_none());
    }

    #[test]
    fn shell_command_from_argv_quotes_resume_arguments() {
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session with ' quote".to_string(),
        ];

        assert_eq!(
            shell_command_from_argv(&argv).as_deref(),
            Some("claude --resume 'session with '\\'' quote'")
        );
        assert_eq!(shell_command_from_argv(&[]), None);
    }
}
