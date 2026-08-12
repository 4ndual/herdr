//! fork(host-actions): exclusive keyboard routing for plugin modals.
//!
//! # The problem this solves
//!
//! `UiEvent::Key` is a broadcast COPY — the pane and the host's mode handlers
//! still receive every key (documented at `offer_key_to_ui_plugins`, and the
//! semantics doctor-dash's safety argument depends on). A plugin-drawn form
//! with text fields is impossible under broadcast: typing the prompt would
//! also type into the terminal underneath. The same doc names the fix's cost:
//! "making keys consumable is an ABI change, not a delivery fix, and must not
//! be smuggled in". This module is that ABI change made deliberately —
//! capture is a separate capability (`keyboard_capture`), a separate effect
//! (`SetKeyboardCapture`), and a separate delivery path, so the broadcast
//! semantics every existing plugin was written against are untouched.
//!
//! # Routing
//!
//! While `AppState::ui_keyboard_capture` names a module, both key paths (the
//! interactive `handle_key` and the headless `route_client_events_from`) call
//! [`App::dispatch_key_to_captured_module`] FIRST and stop when it returns
//! true. One inserted guard per path — the same single-door shape as the
//! mouse-side host-action intercept, for the same merge-cost reason.
//!
//! # Failsafes, in order of who they protect
//!
//! - **ctrl+escape force-releases**, handled host-side before the guest sees
//!   the key. A wedged or malicious guest cannot override it; the user always
//!   has a keyboard way out (the mouse never routes through capture at all).
//! - A capture whose module is **no longer registered** (unlink, disable,
//!   rename) releases on the next key press.
//! - A guest whose dispatch **traps** releases immediately: the instance is
//!   invalidated by the manager's crash isolation, and holding capture for a
//!   corpse would starve the terminal until the user noticed.

use crate::app::App;

impl App {
    /// Apply queued `SetKeyboardCapture` requests. Last writer wins, matching
    /// effect-declaration order (§5.1); a release only clears the state if the
    /// releasing module is the current holder, so module A cannot drop module
    /// B's capture.
    pub(crate) fn apply_pending_keyboard_captures(&mut self) {
        for (plugin_id, module_id, captured) in self.ui_plugins.take_pending_keyboard_captures() {
            if captured {
                if self.ui_plugins.is_module_loaded(&plugin_id, &module_id) {
                    self.state.ui_keyboard_capture = Some((plugin_id, module_id));
                }
            } else if self.state.ui_keyboard_capture.as_ref()
                == Some(&(plugin_id.clone(), module_id.clone()))
            {
                self.state.ui_keyboard_capture = None;
            }
        }
    }

    /// The capture router. Returns true when the key was routed to (or eaten
    /// on behalf of) the capturing module and the caller must NOT run its own
    /// key handling; false means no capture is active and the key proceeds
    /// down the normal path.
    pub(crate) fn dispatch_key_to_captured_module(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> bool {
        let Some((plugin_id, module_id)) = self.state.ui_keyboard_capture.clone() else {
            tracing::debug!(?key, "capture: none active");
            return false;
        };
        tracing::debug!(?key, %plugin_id, "capture: routing key to module");
        // Failsafe 1: ctrl+escape always releases, before the guest sees it.
        if key.code == crossterm::event::KeyCode::Esc
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            self.state.ui_keyboard_capture = None;
            self.request_render();
            return true;
        }
        // Failsafe 2: the holder vanished (unlinked / disabled / hot-reload
        // rename). Release and let the key take the normal path.
        if !self.ui_plugins.is_module_loaded(&plugin_id, &module_id) {
            self.state.ui_keyboard_capture = None;
            return false;
        }
        let changed = self
            .ui_plugins
            .dispatch_key_to_module(&plugin_id, &module_id, key);
        if changed {
            self.refresh_ui_plugin_state_from_manager();
            self.apply_ui_plugin_effects();
            self.request_render();
        } else if self
            .ui_plugins
            .last_error_for(&plugin_id, &module_id)
            .is_some()
        {
            // Failsafe 3: the guest trapped on this key. Its instance is being
            // rebuilt by crash isolation; do not hold the keyboard for it.
            self.state.ui_keyboard_capture = None;
            self.request_render();
        }
        // Captured means captured: even a key the guest ignored must not fall
        // through into the pane mid-form.
        true
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn test_app() -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[test]
    fn no_capture_means_normal_routing() {
        let mut app = test_app();
        assert!(!app.dispatch_key_to_captured_module(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
    }

    /// The user-side failsafe. If this ever stops working, a wedged guest owns
    /// the keyboard until the user reaches for the mouse.
    #[test]
    fn ctrl_escape_always_releases_capture() {
        let mut app = test_app();
        app.state.ui_keyboard_capture = Some(("p".into(), "m".into()));
        assert!(
            app.dispatch_key_to_captured_module(KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL))
        );
        assert_eq!(app.state.ui_keyboard_capture, None);
    }

    /// A capture held by a module that is not loaded must self-release and let
    /// the key through — otherwise disabling a plugin mid-form kills the
    /// keyboard.
    #[test]
    fn capture_by_an_unloaded_module_releases_and_falls_through() {
        let mut app = test_app();
        app.state.ui_keyboard_capture = Some(("ghost.plugin".into(), "m".into()));
        assert!(!app.dispatch_key_to_captured_module(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
        assert_eq!(app.state.ui_keyboard_capture, None);
    }
}
