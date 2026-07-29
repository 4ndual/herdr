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

/// Plugin ids of the bundled UI dashboards, in the order their roots are
/// resolved. Add an entry here as additional bundled dashboards ship; the
/// resolution and merge logic below is generic.
const BUNDLED_UI_PLUGIN_DIR_NAMES: &[&str] = &[
    "usage-dash-ui",
    "goal-dash-ui",
    "historial-dash-ui",
    "harness-manager-ui",
];

/// Candidate roots holding the bundled UI plugin directories, most authoritative
/// first. Only the first one that exists on disk is used.
///
/// `$HERDR_UI_PLUGIN_DIR` wins when set (packaged / deployment builds point it at
/// the installed plugin tree). The compile-time `CARGO_MANIFEST_DIR` comes next so
/// a developer's `cargo build` of a guest still hot-reloads through the normal
/// manifest path — but it is deliberately *not* the only answer: it is baked at
/// compile time and names a build directory that need not outlive the binary. A
/// binary promoted out of a worktree that was later pruned would otherwise lose
/// every bundled surface at once, with nothing but a per-plugin warning to say
/// why. The executable-relative roots are the durable fallback: they travel with
/// the binary, so bundled provenance never depends on a build dir surviving.
fn bundled_ui_plugin_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(dir) = std::env::var("HERDR_UI_PLUGIN_DIR") {
        if !dir.is_empty() {
            roots.push(PathBuf::from(dir));
        }
    }
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Installed layout: `<prefix>/bin/herdr` alongside `<prefix>/plugins`.
            if let Some(prefix) = dir.parent() {
                roots.push(prefix.join("plugins"));
            }
            // Flat layout: the plugin tree sits next to the binary.
            roots.push(dir.join("plugins"));
        }
    }
    roots
}

/// The bundled plugin root actually in effect, or `None` when no candidate
/// exists on disk.
fn resolved_bundled_ui_plugin_root() -> Option<PathBuf> {
    bundled_ui_plugin_roots().into_iter().find(|root| {
        BUNDLED_UI_PLUGIN_DIR_NAMES
            .iter()
            .any(|name| root.join(name).is_dir())
    })
}

/// Manifest directories of the UI plugins bundled with herdr, under whichever
/// candidate root resolved. Empty when none resolved — the caller warns once
/// rather than emitting one misleading "manifest not found" per plugin.
fn bundled_ui_plugin_dirs() -> Vec<PathBuf> {
    let Some(root) = resolved_bundled_ui_plugin_root() else {
        return Vec::new();
    };
    BUNDLED_UI_PLUGIN_DIR_NAMES
        .iter()
        .map(|name| root.join(name))
        .collect()
}

/// Whether `previous` was linked by the user from somewhere other than the
/// bundled tree, and that somewhere still exists.
///
/// The bundled merge is authoritative for the *shipped* copy of a plugin, but a
/// user who ran `plugin link` against their own checkout of the same plugin id
/// made a deliberate choice, and silently overwriting `plugin_root` on the next
/// boot would shadow their copy with no message and no way to tell from
/// `plugin list` (which reports the registry root, not the effective bundled
/// override). A root that no longer exists carries no such intent — it is stale
/// persisted state, and the bundled copy should reclaim it.
fn is_live_user_link(previous_root: &str, bundled_dir: &std::path::Path) -> bool {
    if previous_root.is_empty() {
        return false;
    }
    let previous = PathBuf::from(previous_root);
    if !previous.is_dir() {
        return false;
    }
    let canonical =
        |path: &std::path::Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    canonical(&previous) != canonical(bundled_dir)
}

/// Auto-link every bundled UI plugin into `installed`, upgrading any stale
/// persisted entry to the shipped manifest while preserving the user's
/// enable/disable choice. Called once at `App::new`, before the UI plugin
/// manager is built, so the bundled surfaces always have a renderer without the
/// user installing anything.
pub(crate) fn merge_bundled_ui_plugins(installed: &mut InstalledPluginRegistry) {
    let dirs = bundled_ui_plugin_dirs();
    if dirs.is_empty() {
        tracing::warn!(
            candidates = ?bundled_ui_plugin_roots(),
            "no bundled ui plugin root found; bundled surfaces fall back to whatever the registry holds"
        );
        return;
    }
    for dir in dirs {
        match load_plugin_manifest(&dir.to_string_lossy(), true) {
            Ok(mut plugin) => {
                if let Some(previous) = installed.get(&plugin.plugin_id) {
                    plugin.enabled = previous.enabled;
                    if is_live_user_link(&previous.plugin_root, &dir) {
                        tracing::debug!(
                            plugin_id = %plugin.plugin_id,
                            user_root = %previous.plugin_root,
                            bundled_dir = %dir.display(),
                            "keeping user-linked plugin root; bundled copy not merged over it"
                        );
                        continue;
                    }
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

    /// A user who linked their own checkout of a bundled plugin id keeps it: the
    /// bundled merge preserves `plugin_root` the way it preserves `enabled`,
    /// instead of silently shadowing their copy on the next boot.
    #[test]
    fn user_linked_plugin_root_survives_the_bundled_merge() {
        let mut installed = InstalledPluginRegistry::new();
        let user_root = std::env::temp_dir();
        assert!(user_root.is_dir(), "test needs an existing foreign root");

        let mut linked = load_plugin_manifest(&bundled_ui_plugin_dirs()[0].to_string_lossy(), true)
            .expect("load bundled usage-dash manifest");
        linked.plugin_root = user_root.to_string_lossy().into_owned();
        linked.version = "9.9.9-user".to_string();
        installed.insert(linked.plugin_id.clone(), linked);

        merge_bundled_ui_plugins(&mut installed);

        let plugin = &installed["builtin.usage-dash"];
        assert_eq!(
            plugin.plugin_root,
            user_root.to_string_lossy(),
            "user-linked root not overwritten by the bundled copy"
        );
        assert_eq!(
            plugin.version, "9.9.9-user",
            "the user's entry is kept whole, not half-merged"
        );
    }

    /// A persisted root that no longer exists carries no user intent — it is
    /// stale state from a build directory that was pruned — so the bundled copy
    /// reclaims it rather than leaving the surface pointed at nothing.
    #[test]
    fn dangling_persisted_root_is_reclaimed_by_the_bundled_copy() {
        let mut installed = InstalledPluginRegistry::new();
        let dangling = std::env::temp_dir().join("herdr-pruned-worktree-that-does-not-exist");
        assert!(!dangling.exists(), "test needs a genuinely absent root");

        let mut stale = load_plugin_manifest(&bundled_ui_plugin_dirs()[0].to_string_lossy(), true)
            .expect("load bundled usage-dash manifest");
        stale.plugin_root = dangling.to_string_lossy().into_owned();
        installed.insert(stale.plugin_id.clone(), stale);

        merge_bundled_ui_plugins(&mut installed);

        let plugin = &installed["builtin.usage-dash"];
        assert_ne!(
            plugin.plugin_root,
            dangling.to_string_lossy(),
            "dangling root replaced by the resolved bundled root"
        );
        assert!(
            std::path::Path::new(&plugin.plugin_root).is_dir(),
            "reclaimed root exists on disk"
        );
    }

    /// Bundled provenance must not depend on the compile-time build directory
    /// surviving: the executable-relative roots are offered as fallbacks so a
    /// promoted binary whose worktree was pruned can still find its plugins.
    #[test]
    fn bundled_roots_include_executable_relative_fallbacks() {
        let roots = bundled_ui_plugin_roots();
        let baked = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins");
        let baked_idx = roots
            .iter()
            .position(|root| root == &baked)
            .expect("baked manifest root offered");
        assert!(
            roots.len() > baked_idx + 1,
            "at least one fallback root is offered after the baked build dir"
        );
        let exe = std::env::current_exe().expect("current exe");
        let exe_dir = exe.parent().expect("exe dir");
        assert!(
            roots.iter().any(|root| root.starts_with(exe_dir)
                || exe_dir.starts_with(root.parent().unwrap_or(root))),
            "a fallback root is resolved relative to the running binary"
        );
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
