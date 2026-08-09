//! fork(host-actions): stable public ids for host-drawn controls, plus the
//! claim table that lets a UI plugin observe or replace one.
//!
//! # Why this exists
//!
//! Every native control in the TUI is a coordinate test inlined into
//! `AppState::handle_mouse` with its mutation immediately following it. A button
//! therefore has no name, no id, and no interception point, so a plugin cannot
//! reach one — it can only draw and own its OWN surfaces. Two plugins already
//! wanted the opposite (declare a button, let the HOST run it) and both were
//! served by hardcoding a string prefix in `mouse.rs`: `historial:` and
//! `harness:`. This is the general form of that, so there is never a third.
//!
//! # Why the claim table is pre-published rather than asked at click time
//!
//! `AppState` is pure data and deliberately holds no wasmtime handle, so
//! `handle_mouse` cannot call a guest and wait for an answer — the same
//! constraint that produced `PendingPluginUiAction`. A synchronous plugin veto
//! is therefore impossible without breaking the project's first principle.
//!
//! The inversion that works, and which `ui_context_menu_items` already uses, is
//! to publish the claims INTO `AppState` each time the manager state changes.
//! `handle_mouse` then decides synchronously, from pure data, whether to run its
//! default body or hand the click to a plugin, and the actual guest dispatch
//! happens later in `App::drain_pending_plugin_ui_actions`, which does hold the
//! runtime.
//!
//! # Where the hit-testing lives
//!
//! Not here. `app::input::host_actions` resolves a click to a [`HostAction`] by
//! calling the host's own rect helpers in the host's own order, from ONE
//! interception point at the top of `handle_mouse`. Keeping the vocabulary
//! separate from the resolver is what lets `ui_plugins` build claims without
//! depending on input routing.

use std::collections::HashMap;

use crate::app::state::AppState;

/// A host-drawn control a plugin may claim.
///
/// Wave 1 covers the six controls whose dispatch sites were read line by line.
/// The remaining ~60 mapped controls are deliberately absent rather than
/// guessed at: an id here is a promise that the resolver reproduces the host's
/// routing for it exactly, and that promise cannot be made from a grep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HostAction {
    /// Sidebar collapse/expand chevron (`mouse.rs` sidebar-toggle branch).
    SidebarToggle,
    /// The sidebar footer's `+ new` workspace button.
    WorkspaceNew,
    /// The sidebar footer's global (☰) launcher.
    GlobalMenuOpen,
    /// The tab bar's trailing `+` button.
    TabNew,
    /// The tab bar's `‹` overflow-scroll button.
    TabScrollLeft,
    /// The tab bar's `›` overflow-scroll button.
    TabScrollRight,
}

impl HostAction {
    pub(crate) const ALL: &'static [HostAction] = &[
        HostAction::SidebarToggle,
        HostAction::WorkspaceNew,
        HostAction::GlobalMenuOpen,
        HostAction::TabNew,
        HostAction::TabScrollLeft,
        HostAction::TabScrollRight,
    ];

    /// The manifest-facing id. Neutral by construction (`host.tab.new`, not
    /// `host.tabbar.plus_button`): per the runtime/client guardrail these names
    /// outlive the widget that happens to draw them today.
    pub(crate) const fn id(self) -> &'static str {
        match self {
            HostAction::SidebarToggle => "host.sidebar.toggle",
            HostAction::WorkspaceNew => "host.workspace.new",
            HostAction::GlobalMenuOpen => "host.global_menu.open",
            HostAction::TabNew => "host.tab.new",
            HostAction::TabScrollLeft => "host.tab.scroll_left",
            HostAction::TabScrollRight => "host.tab.scroll_right",
        }
    }

    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|action| action.id() == id)
    }
}

/// What a claim does to the host's own behaviour.
///
/// `Observe` is the default because it is the safe one: a manifest that names an
/// action without saying anything else gets the event and changes nothing.
/// Suppressing a control the user relies on has to be asked for by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HostActionMode {
    /// The host runs its default body AND the plugin receives the event.
    #[default]
    Observe,
    /// The host skips its default body; the plugin owns the outcome.
    Replace,
}

/// One module's claim on one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostActionClaim {
    pub plugin_id: String,
    pub module_id: String,
    pub mode: HostActionMode,
}

/// Claims published from the manager into pure state, keyed by canonical id.
///
/// Keyed by `&'static str` rather than `String` on purpose: only ids that
/// resolved through [`HostAction::from_id`] can ever be inserted, so an
/// unrecognised manifest entry cannot sit in the table looking live.
pub(crate) type HostActionClaims = HashMap<&'static str, HostActionClaim>;

/// A claimed click waiting for `App` to dispatch it to the owning guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingHostAction {
    pub plugin_id: String,
    pub module_id: String,
    pub action_id: &'static str,
    pub mode: HostActionMode,
}

/// What the caller in `handle_mouse` must do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostActionOutcome {
    /// Run the host's default body (no claim, or an `Observe` claim).
    Proceed,
    /// A `Replace` claim took the click; the default body must not run.
    Claimed,
}

impl AppState {
    /// Record `action` for the claiming module, if any, and report whether the
    /// host's own default body should still run.
    ///
    /// Queues rather than dispatches: see the module docs on why the guest call
    /// cannot happen here.
    pub(crate) fn dispatch_host_action(&mut self, action: HostAction) -> HostActionOutcome {
        let Some(claim) = self.ui_host_action_claims.get(action.id()) else {
            return HostActionOutcome::Proceed;
        };
        let pending = PendingHostAction {
            plugin_id: claim.plugin_id.clone(),
            module_id: claim.module_id.clone(),
            action_id: action.id(),
            mode: claim.mode,
        };
        let outcome = match claim.mode {
            HostActionMode::Observe => HostActionOutcome::Proceed,
            HostActionMode::Replace => HostActionOutcome::Claimed,
        };
        self.pending_host_actions.push(pending);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_id_is_unique_and_host_prefixed() {
        let mut seen = std::collections::HashSet::new();
        for action in HostAction::ALL {
            let id = action.id();
            assert!(
                id.starts_with("host."),
                "{id} must be namespaced under `host.` so it can never collide \
                 with a module-private action id"
            );
            assert!(seen.insert(id), "duplicate host action id {id}");
        }
    }

    /// The manifest names actions by string, so the string → variant direction
    /// has to stay total. A typo'd id must be rejected, never silently dropped
    /// into the table where it would look claimed and do nothing.
    #[test]
    fn from_id_round_trips_every_variant_and_rejects_anything_else() {
        for action in HostAction::ALL {
            assert_eq!(HostAction::from_id(action.id()), Some(*action));
        }
        assert_eq!(HostAction::from_id("host.tab.nwe"), None);
        assert_eq!(HostAction::from_id("historial:restore:1"), None);
        assert_eq!(HostAction::from_id(""), None);
    }

    fn claimed(mode: HostActionMode) -> AppState {
        let mut state = AppState::test_new();
        state.ui_host_action_claims.insert(
            HostAction::TabNew.id(),
            HostActionClaim {
                plugin_id: "p".into(),
                module_id: "m".into(),
                mode,
            },
        );
        state
    }

    #[test]
    fn an_unclaimed_action_proceeds_and_queues_nothing() {
        let mut state = AppState::test_new();
        assert_eq!(
            state.dispatch_host_action(HostAction::TabNew),
            HostActionOutcome::Proceed
        );
        assert!(state.pending_host_actions.is_empty());
    }

    #[test]
    fn a_replace_claim_suppresses_the_default_and_queues_the_dispatch() {
        let mut state = claimed(HostActionMode::Replace);
        assert_eq!(
            state.dispatch_host_action(HostAction::TabNew),
            HostActionOutcome::Claimed
        );
        assert_eq!(
            state.pending_host_actions,
            vec![PendingHostAction {
                plugin_id: "p".into(),
                module_id: "m".into(),
                action_id: "host.tab.new",
                mode: HostActionMode::Replace,
            }]
        );
    }

    /// The whole point of the two modes: `Observe` must still let the host run.
    /// If this ever returns `Claimed`, every observing plugin silently breaks the
    /// button it was only watching.
    #[test]
    fn an_observe_claim_queues_the_dispatch_but_still_runs_the_default() {
        let mut state = claimed(HostActionMode::Observe);
        assert_eq!(
            state.dispatch_host_action(HostAction::TabNew),
            HostActionOutcome::Proceed
        );
        assert_eq!(state.pending_host_actions.len(), 1);
    }

    /// A claim on one action must not leak onto its neighbours — the table is
    /// keyed per id, and `host.tab.new` sitting next to `host.tab.scroll_left`
    /// is exactly where an accidental prefix match would show up.
    #[test]
    fn a_claim_applies_only_to_the_action_it_names() {
        let mut state = claimed(HostActionMode::Replace);
        assert_eq!(
            state.dispatch_host_action(HostAction::TabScrollLeft),
            HostActionOutcome::Proceed
        );
        assert!(state.pending_host_actions.is_empty());
    }
}

/// Manager-side claim building. Lives here rather than in `ui_plugins.rs` so the
/// upstream file carries only the code it has to and none of the tests.
#[cfg(test)]
mod claim_table_tests {
    use super::*;
    use crate::api::schema::{
        InstalledPluginInfo, PluginHostActionMode, PluginHotReloadMode, PluginManifestUiModule,
        PluginUiCapability, PluginUiModuleKind, PluginUiSurface,
    };

    fn module(
        id: &str,
        host_actions: &[&str],
        capabilities: Vec<PluginUiCapability>,
    ) -> PluginManifestUiModule {
        PluginManifestUiModule {
            id: id.into(),
            title: id.into(),
            description: None,
            platforms: None,
            kind: PluginUiModuleKind::Wasm,
            api_version: 3,
            module: format!("dist/{id}.wasm"),
            hot_reload: PluginHotReloadMode::On,
            surfaces: vec![PluginUiSurface::Header],
            placement: None,
            capabilities,
            ui_input_file: None,
            image_file: None,
            graphics_roots: Vec::new(),
            max_layers_per_pane: None,
            collector: None,
            host_actions: host_actions.iter().map(|id| (*id).to_string()).collect(),
            host_action_mode: Some(PluginHostActionMode::Replace),
        }
    }

    fn plugin(plugin_id: &str, modules: Vec<PluginManifestUiModule>) -> InstalledPluginInfo {
        InstalledPluginInfo {
            plugin_id: plugin_id.into(),
            name: plugin_id.into(),
            version: "0.0.1".into(),
            min_herdr_version: String::new(),
            description: None,
            manifest_path: format!("/tmp/{plugin_id}/herdr-plugin.toml"),
            plugin_root: format!("/tmp/{plugin_id}"),
            enabled: true,
            platforms: None,
            build: Vec::new(),
            startup: Vec::new(),
            actions: Vec::new(),
            events: Vec::new(),
            panes: Vec::new(),
            ui_modules: modules,
            link_handlers: Vec::new(),
            source: Default::default(),
            warnings: Vec::new(),
        }
    }

    fn claims_for(plugins: Vec<InstalledPluginInfo>) -> HostActionClaims {
        let registry: HashMap<String, InstalledPluginInfo> = plugins
            .into_iter()
            .map(|plugin| (plugin.plugin_id.clone(), plugin))
            .collect();
        crate::ui_plugins::UiPluginManager::from_installed_plugins(&registry).host_action_claims()
    }

    #[test]
    fn a_declared_action_with_the_capability_becomes_a_claim() {
        let claims = claims_for(vec![plugin(
            "example.demo",
            vec![module(
                "demo",
                &["host.tab.new"],
                vec![PluginUiCapability::HostActions],
            )],
        )]);
        let claim = claims
            .get("host.tab.new")
            .expect("a declared action must be claimed");
        assert_eq!(claim.plugin_id, "example.demo");
        assert_eq!(claim.module_id, "demo");
        assert_eq!(claim.mode, HostActionMode::Replace);
    }

    /// The capability is the user-visible grant; the id list only says which
    /// controls it covers. Without the grant a manifest must be able to name
    /// whatever it likes and still change nothing.
    #[test]
    fn declaring_actions_without_the_capability_claims_nothing() {
        let claims = claims_for(vec![plugin(
            "example.demo",
            vec![module(
                "demo",
                &["host.tab.new"],
                vec![PluginUiCapability::MouseAll],
            )],
        )]);
        assert!(claims.is_empty());
    }

    /// `MouseAll` lets a module SEE clicks. It must not also let it SUPPRESS a
    /// native button — if this ever passes, every plugin that asked to watch the
    /// mouse silently gained the power to disable the user's new-tab button.
    #[test]
    fn mouse_all_does_not_confer_host_action_claims() {
        let claims = claims_for(vec![plugin(
            "example.demo",
            vec![module(
                "demo",
                &["host.sidebar.toggle", "host.workspace.new"],
                vec![
                    PluginUiCapability::MouseAll,
                    PluginUiCapability::KeyboardInput,
                    PluginUiCapability::ApiWrite,
                ],
            )],
        )]);
        assert!(claims.is_empty());
    }

    #[test]
    fn an_unknown_action_id_is_dropped_and_the_valid_ones_survive() {
        let claims = claims_for(vec![plugin(
            "example.demo",
            vec![module(
                "demo",
                &["host.tab.nwe", "host.tab.new"],
                vec![PluginUiCapability::HostActions],
            )],
        )]);
        assert_eq!(claims.len(), 1);
        assert!(claims.contains_key("host.tab.new"));
    }

    /// Two plugins fighting over one control resolves by registration order
    /// (plugin_id ascending), not by HashMap iteration order. Asserted directly
    /// because a nondeterministic winner is the kind of bug that only shows up
    /// on someone else's machine.
    #[test]
    fn a_contested_action_goes_to_the_first_module_in_registration_order() {
        let claims = claims_for(vec![
            plugin(
                "b.later",
                vec![module(
                    "demo",
                    &["host.tab.new"],
                    vec![PluginUiCapability::HostActions],
                )],
            ),
            plugin(
                "a.earlier",
                vec![module(
                    "demo",
                    &["host.tab.new"],
                    vec![PluginUiCapability::HostActions],
                )],
            ),
        ]);
        assert_eq!(
            claims
                .get("host.tab.new")
                .map(|claim| claim.plugin_id.as_str()),
            Some("a.earlier")
        );
    }

    #[test]
    fn a_disabled_plugin_claims_nothing() {
        let mut disabled = plugin(
            "example.demo",
            vec![module(
                "demo",
                &["host.tab.new"],
                vec![PluginUiCapability::HostActions],
            )],
        );
        disabled.enabled = false;
        assert!(claims_for(vec![disabled]).is_empty());
    }
}
