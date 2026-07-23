//! Auto-linking of the WASM UI plugins that ship bundled inside herdr.
//!
//! Herdr's usage strip is rendered exclusively by a WASM UI plugin — there is
//! no native fallback renderer. Rather than synthesize the plugin's registry
//! entry in core (which forced core to hardcode the module id, surface, and API
//! version of one specific dashboard), the bundled plugin ships as a normal
//! plugin directory with its own manifest, and core auto-links it at startup
//! through the exact same [`crate::app::load_plugin_manifest`] path a user
//! `plugin link` uses. Core stays generic: it knows only that *some* bundled
//! plugin dirs exist, not what any of them render.
//!
//! The bundled manifest is authoritative. Re-linking on every boot upgrades a
//! stale persisted entry (e.g. an older api_version pinned in a prior release)
//! to the shipped manifest, while the user's enable/disable choice is
//! preserved. A missing/broken bundled manifest downgrades to an empty surface
//! (a warning, never a crash) — the same failure mode as a missing `.wasm`.

use std::path::PathBuf;

use crate::app::load_plugin_manifest;
use crate::app::state::InstalledPluginRegistry;

/// Manifest directories of the UI plugins bundled with herdr. `$HERDR_UI_PLUGIN_DIR`
/// wins when set (packaged / deployment builds point it at the installed plugin
/// tree); otherwise each plugin ships under the source tree's `plugins/<name>`,
/// so a `cargo build` of a guest hot-reloads it through the normal manifest path.
fn bundled_ui_plugin_dirs() -> Vec<PathBuf> {
    let root = match std::env::var("HERDR_UI_PLUGIN_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins"),
    };
    // Add more dirs here as additional bundled dashboards ship; the merge
    // logic below is generic.
    vec![
        root.join("usage-dash-ui"),
        root.join("goal-dash-ui"),
        root.join("historial-dash-ui"),
        root.join("harness-manager-ui"),
    ]
}

/// Auto-link every bundled UI plugin into `installed`, upgrading any stale
/// persisted entry to the shipped manifest while preserving the user's
/// enable/disable choice. Called once at `App::new`, before the UI plugin
/// manager is built, so the bundled surfaces always have a renderer without the
/// user installing anything.
pub(crate) fn merge_bundled_ui_plugins(installed: &mut InstalledPluginRegistry) {
    for dir in bundled_ui_plugin_dirs() {
        match load_plugin_manifest(&dir.to_string_lossy(), true) {
            Ok(mut plugin) => {
                if let Some(previous) = installed.get(&plugin.plugin_id) {
                    plugin.enabled = previous.enabled;
                }
                installed.insert(plugin.plugin_id.clone(), plugin);
            }
            Err((code, message)) => {
                tracing::warn!(
                    code,
                    error = %message,
                    dir = %dir.display(),
                    "failed to auto-link bundled ui plugin; its surface will be empty"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh session: the bundled usage-dash manifest auto-links into an empty
    /// registry at the shipped api_version (3) with the generic
    /// `ui_input_file` transport wired — the state that makes the strip render.
    #[test]
    fn bundled_usage_dash_auto_links_into_empty_registry() {
        let mut installed = InstalledPluginRegistry::new();
        merge_bundled_ui_plugins(&mut installed);

        let plugin = installed
            .get("builtin.usage-dash")
            .expect("bundled usage-dash auto-linked");
        assert!(plugin.enabled);
        let module = plugin
            .ui_modules
            .iter()
            .find(|m| m.id == "usage")
            .expect("usage ui module");
        assert_eq!(module.api_version, 3);
        assert_eq!(module.ui_input_file.as_deref(), Some("usage_input.json"));
        assert!(module
            .surfaces
            .contains(&crate::api::schema::PluginUiSurface::UsageStrip));
        // fork(wave-2): the shipped manifest declares the out-of-process
        // collector, so the field flows through the loader into the registry.
        assert_eq!(
            module.collector.as_deref(),
            Some(["../../target/release/usage-dash-collector".to_string()].as_slice())
        );
    }

    /// Fresh session: the bundled goal-dash manifest auto-links at the shipped
    /// api_version (3) on the right-panel surface with the generic
    /// `ui_input_file` transport wired.
    #[test]
    fn bundled_goal_dash_auto_links_into_empty_registry() {
        let mut installed = InstalledPluginRegistry::new();
        merge_bundled_ui_plugins(&mut installed);

        let plugin = installed
            .get("builtin.goal-dash")
            .expect("bundled goal-dash auto-linked");
        assert!(plugin.enabled);
        let module = plugin
            .ui_modules
            .iter()
            .find(|m| m.id == "goal")
            .expect("goal ui module");
        assert_eq!(module.api_version, 3);
        assert_eq!(module.ui_input_file.as_deref(), Some("goal_input.json"));
        assert!(module
            .surfaces
            .contains(&crate::api::schema::PluginUiSurface::RightPanel));
    }

    /// Legacy registry: a persisted pre-purge entry (older api_version, no
    /// input file, user-disabled) is upgraded to the shipped manifest on load,
    /// but the user's disable choice survives the upgrade.
    #[test]
    fn legacy_entry_upgraded_to_shipped_manifest_preserving_enabled() {
        let mut installed = InstalledPluginRegistry::new();

        // Seed a stale persisted entry: same plugin id, but a v2 module with no
        // input file, and disabled by the user.
        let mut stale = load_plugin_manifest(&bundled_ui_plugin_dirs()[0].to_string_lossy(), true)
            .expect("load bundled usage-dash manifest");
        stale.enabled = false;
        stale.ui_modules[0].api_version = 2;
        stale.ui_modules[0].ui_input_file = None;
        installed.insert(stale.plugin_id.clone(), stale);

        merge_bundled_ui_plugins(&mut installed);

        let plugin = &installed["builtin.usage-dash"];
        assert!(
            !plugin.enabled,
            "user disable choice preserved across upgrade"
        );
        assert_eq!(
            plugin.ui_modules[0].api_version, 3,
            "stale v2 entry upgraded to shipped v3 manifest"
        );
        assert_eq!(
            plugin.ui_modules[0].ui_input_file.as_deref(),
            Some("usage_input.json")
        );
    }
}
