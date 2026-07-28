//! Host side of the resume-search picker overlay (`Mode::ResumePicker`).
//!
//! The overlay hosts a hidden PTY that runs the existing resume-picker TUI
//! byte-for-byte. The picker renders its screen to stderr (painted into the
//! popup by [`crate::ui::resume_picker`]) and, on Enter/click, writes the
//! resolved `exec cx …` command to a result file (its stdout, redirected by the
//! overlay wrapper script). This module owns the lifecycle:
//!
//! - [`App::open_resume_picker_overlay`] spawns the hidden PTY and enters the mode.
//! - Keys/mouse are forwarded into the PTY while the mode is active
//!   ([`App::resume_picker_send_key`], [`App::resume_picker_send_bytes`]).
//! - The picker's `PaneDied` is intercepted by
//!   [`App::handle_resume_picker_pane_died`], which reads the result file and,
//!   for a non-empty command, opens it in a new focused tab.
//! - Esc closes the overlay and kills the picker ([`App::close_resume_picker_overlay`]).

use std::path::PathBuf;

use bytes::Bytes;

use super::App;
use crate::app::state::{Mode, ResumePickerOverlay};
use crate::input::TerminalKey;
use crate::layout::PaneId;
use crate::terminal::{TerminalId, TerminalRuntime};

impl App {
    /// Open the resume-search picker as a floating overlay. `wrapper` is the
    /// absolute path to `resume-picker-overlay.sh`, resolved by the caller (the
    /// button script knows `$HERDR_PLUGIN_ROOT`; the keybind resolves it from the
    /// installed-plugin registry). Idempotent while already open.
    pub(crate) fn open_resume_picker_overlay(&mut self, wrapper: PathBuf) -> std::io::Result<()> {
        if self.state.resume_picker.is_some() {
            return Ok(());
        }
        if !wrapper.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("resume picker wrapper not found: {}", wrapper.display()),
            ));
        }

        let result_file = resume_picker_result_path()?;
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();

        // Initial size is only a seed; the render path resizes the PTY to the
        // popup's inner rect every frame (idempotent when unchanged).
        let (rows, cols) = self.state.estimate_pane_size();
        let ws_idx = self.state.active;
        let cwd = ws_idx
            .and_then(|ws_idx| self.focused_pane_cwd_in_workspace(ws_idx))
            .map(|cwd| self.resolve_new_terminal_cwd(Some(cwd)))
            .unwrap_or_else(|| self.resolve_new_terminal_cwd(None));

        let argv = vec![wrapper.to_string_lossy().into_owned()];
        let launch_env = crate::pane::PaneLaunchEnv::from_extra(vec![(
            "HERDR_RESUME_RESULT_FILE".to_string(),
            result_file.to_string_lossy().into_owned(),
        )]);

        let runtime = TerminalRuntime::spawn_argv_command(
            pane_id,
            rows.max(4),
            cols.max(10),
            cwd,
            &argv,
            &launch_env,
            // fork(0.7.5-merge): upstream 0.7.5 added an `agent_detection` param
            // to `spawn_argv_command`. The resume picker is a short-lived helper
            // UI, not an agent pane, so detection is disabled — matching what
            // upstream's own popup helper passes (`src/app/popup.rs`). This also
            // stops the picker terminal from being misdetected as a live agent.
            crate::pane::AgentDetection::Disabled,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )?;

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.resume_picker = Some(ResumePickerOverlay {
            terminal_id,
            pane_id,
            result_file,
        });
        self.state.mode = Mode::ResumePicker;
        self.render_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.render_notify.notify_one();
        Ok(())
    }

    /// Close the overlay without resuming: kill the picker PTY, delete the
    /// result file, and return to the terminal. Used by Esc.
    pub(crate) fn close_resume_picker_overlay(&mut self) {
        let Some(overlay) = self.state.resume_picker.take() else {
            return;
        };
        if let Some(runtime) = self.terminal_runtimes.remove(&overlay.terminal_id) {
            runtime.shutdown();
        }
        let _ = std::fs::remove_file(&overlay.result_file);
        if self.state.mode == Mode::ResumePicker {
            self.state.mode = Mode::Terminal;
        }
        self.render_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.render_notify.notify_one();
    }

    /// Intercept a `PaneDied` for the overlay's hidden PTY. Returns `true` when
    /// the event belonged to the picker (and was fully handled): the result file
    /// is read and a non-empty command is opened in a new focused tab.
    pub(crate) fn handle_resume_picker_pane_died(&mut self, pane_id: PaneId) -> bool {
        let Some(overlay) = self.state.resume_picker.as_ref() else {
            return false;
        };
        if overlay.pane_id != pane_id {
            return false;
        }
        let overlay = self.state.resume_picker.take().expect("checked Some above");
        if let Some(runtime) = self.terminal_runtimes.remove(&overlay.terminal_id) {
            runtime.shutdown();
        }
        let command = std::fs::read_to_string(&overlay.result_file)
            .ok()
            .map(|raw| raw.trim().to_string())
            .filter(|cmd| !cmd.is_empty());
        let _ = std::fs::remove_file(&overlay.result_file);

        if self.state.mode == Mode::ResumePicker {
            self.state.mode = Mode::Terminal;
        }
        if let Some(command) = command {
            self.open_resume_command_tab(command);
        }
        self.render_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.render_notify.notify_one();
        true
    }

    /// Open a picker-resolved command in a new focused tab. The command is a
    /// shell command line (e.g. `exec cx-resume …`), run via `sh -lc` exactly as
    /// the picker's own `exec sh -c "$CMD"` would, so `exec` replaces the shell.
    fn open_resume_command_tab(&mut self, command: String) {
        let Some(ws_idx) = self.state.active else {
            return;
        };
        let cwd = self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx));
        let (rows, cols) = self.state.estimate_pane_size();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let argv = vec!["sh".to_string(), "-lc".to_string(), command];

        let result = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab_argv_command(
                    rows,
                    cols,
                    cwd,
                    &argv,
                    Vec::new(),
                    scrollback_limit_bytes,
                    host_terminal_theme,
                )
            });
        if let Ok((tab_idx, terminal, runtime)) = result {
            self.terminal_runtimes.insert(terminal.id.clone(), runtime);
            self.state.terminals.insert(terminal.id.clone(), terminal);
            self.state.remove_alias_shadowed_by_new_pane(
                self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
            );
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = Mode::Terminal;
            self.schedule_session_save();
            self.emit_tab_created_events(ws_idx, tab_idx);
        }
    }

    /// Route a key while the overlay is open. Esc closes the overlay and kills
    /// the picker (guaranteeing no orphan even if the picker is wedged); every
    /// other key is forwarded into the hidden PTY.
    pub(crate) fn handle_resume_picker_key(&mut self, key: TerminalKey) {
        if key.code == crossterm::event::KeyCode::Esc && key.modifiers.is_empty() {
            self.close_resume_picker_overlay();
            return;
        }
        self.resume_picker_send_key(key);
    }

    /// Forward a key press into the picker's hidden PTY.
    pub(crate) fn resume_picker_send_key(&mut self, key: TerminalKey) {
        let Some(overlay) = self.state.resume_picker.as_ref() else {
            return;
        };
        let Some(runtime) = self.terminal_runtimes.get(&overlay.terminal_id) else {
            return;
        };
        let bytes = runtime.encode_terminal_key(key);
        if !bytes.is_empty() {
            let _ = runtime.try_send_bytes(Bytes::from(bytes));
        }
    }
}

/// Allocate a unique, private result-file path for one picker run.
fn resume_picker_result_path() -> std::io::Result<PathBuf> {
    let mut dir = std::env::temp_dir();
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    dir.push(format!(
        "herdr-resume-picker-{}-{micros:x}.cmd",
        std::process::id()
    ));
    // Create it empty so an Esc-close (picker writes nothing) reads as cancel.
    std::fs::write(&dir, b"")?;
    Ok(dir)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::app::state::Mode;
    use std::os::unix::fs::PermissionsExt;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let workspace = crate::workspace::Workspace::test_new("resume-picker-test");
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app
    }

    /// A throwaway "wrapper" script that runs long enough to observe the spawned
    /// overlay runtime, then exits. Not a mock of the picker — a real PTY child.
    fn sleep_wrapper() -> PathBuf {
        write_wrapper("#!/bin/sh\nsleep 5\n")
    }

    fn write_wrapper(body: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0);
        path.push(format!(
            "herdr-resume-wrapper-{}-{micros:x}.sh",
            std::process::id()
        ));
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[tokio::test]
    async fn open_enters_mode_and_spawns_hidden_runtime() {
        let mut app = test_app();
        let wrapper = sleep_wrapper();

        app.open_resume_picker_overlay(wrapper.clone()).unwrap();

        assert_eq!(app.state.mode, Mode::ResumePicker);
        let overlay = app.state.resume_picker.clone().expect("overlay present");
        assert!(app.terminal_runtimes.get(&overlay.terminal_id).is_some());
        assert!(overlay.result_file.exists(), "result file seeded");

        app.close_resume_picker_overlay();
        let _ = std::fs::remove_file(wrapper);
    }

    #[tokio::test]
    async fn open_is_idempotent_while_already_open() {
        let mut app = test_app();
        let wrapper = sleep_wrapper();

        app.open_resume_picker_overlay(wrapper.clone()).unwrap();
        let first = app.state.resume_picker.clone().unwrap();
        app.open_resume_picker_overlay(wrapper.clone()).unwrap();
        let second = app.state.resume_picker.clone().unwrap();

        assert_eq!(first.terminal_id, second.terminal_id, "no second spawn");

        app.close_resume_picker_overlay();
        let _ = std::fs::remove_file(wrapper);
    }

    #[tokio::test]
    async fn open_rejects_missing_wrapper() {
        let mut app = test_app();
        let missing = std::env::temp_dir().join("herdr-resume-nope-does-not-exist.sh");
        let err = app.open_resume_picker_overlay(missing).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(app.state.resume_picker.is_none());
        assert_ne!(
            app.state.mode,
            Mode::ResumePicker,
            "mode not switched on failure"
        );
    }

    #[tokio::test]
    async fn close_tears_down_runtime_and_result_file() {
        let mut app = test_app();
        let wrapper = sleep_wrapper();
        app.open_resume_picker_overlay(wrapper.clone()).unwrap();
        let overlay = app.state.resume_picker.clone().unwrap();

        app.close_resume_picker_overlay();

        assert_eq!(app.state.mode, Mode::Terminal);
        assert!(app.state.resume_picker.is_none());
        assert!(app.terminal_runtimes.get(&overlay.terminal_id).is_none());
        assert!(!overlay.result_file.exists(), "result file removed");
        let _ = std::fs::remove_file(wrapper);
    }

    #[tokio::test]
    async fn pane_died_for_other_pane_is_ignored() {
        let mut app = test_app();
        let wrapper = sleep_wrapper();
        app.open_resume_picker_overlay(wrapper.clone()).unwrap();

        let handled = app.handle_resume_picker_pane_died(PaneId::alloc());
        assert!(!handled, "unrelated pane death not claimed");
        assert!(app.state.resume_picker.is_some(), "overlay untouched");

        app.close_resume_picker_overlay();
        let _ = std::fs::remove_file(wrapper);
    }

    #[tokio::test]
    async fn pane_died_with_command_opens_new_tab() {
        let mut app = test_app();
        let wrapper = sleep_wrapper();
        app.open_resume_picker_overlay(wrapper.clone()).unwrap();
        let overlay = app.state.resume_picker.clone().unwrap();
        let tabs_before = app.state.workspaces[0].tabs.len();
        // The picker's stdout line: a resolved shell command.
        std::fs::write(&overlay.result_file, "true\n").unwrap();

        let handled = app.handle_resume_picker_pane_died(overlay.pane_id);

        assert!(handled);
        assert_eq!(app.state.mode, Mode::Terminal);
        assert!(app.state.resume_picker.is_none());
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            tabs_before + 1,
            "resume opened in a new tab"
        );
        let _ = std::fs::remove_file(wrapper);
    }

    #[tokio::test]
    async fn pane_died_with_empty_result_is_a_cancel() {
        let mut app = test_app();
        let wrapper = sleep_wrapper();
        app.open_resume_picker_overlay(wrapper.clone()).unwrap();
        let overlay = app.state.resume_picker.clone().unwrap();
        let tabs_before = app.state.workspaces[0].tabs.len();
        // Esc/Ctrl-C path: the picker exits without writing.

        let handled = app.handle_resume_picker_pane_died(overlay.pane_id);

        assert!(handled);
        assert_eq!(app.state.mode, Mode::Terminal);
        assert!(app.state.resume_picker.is_none());
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            tabs_before,
            "no tab opened on cancel"
        );
        let _ = std::fs::remove_file(wrapper);
    }

    #[tokio::test]
    async fn esc_key_closes_overlay() {
        let mut app = test_app();
        let wrapper = sleep_wrapper();
        app.open_resume_picker_overlay(wrapper.clone()).unwrap();

        app.handle_resume_picker_key(TerminalKey::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::empty(),
        ));

        assert!(app.state.resume_picker.is_none());
        assert_eq!(app.state.mode, Mode::Terminal);
        let _ = std::fs::remove_file(wrapper);
    }

    #[tokio::test]
    async fn non_esc_key_keeps_overlay_open() {
        let mut app = test_app();
        let wrapper = sleep_wrapper();
        app.open_resume_picker_overlay(wrapper.clone()).unwrap();

        // A typed character is forwarded to the picker, not a close.
        app.handle_resume_picker_key(TerminalKey::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::empty(),
        ));

        assert!(app.state.resume_picker.is_some());
        assert_eq!(app.state.mode, Mode::ResumePicker);

        app.close_resume_picker_overlay();
        let _ = std::fs::remove_file(wrapper);
    }
}
