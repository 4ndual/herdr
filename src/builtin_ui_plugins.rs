//! Built-in WASM UI dash plugins (usage strip + goal panel).
//!
//! Herdr's two dashboards are rendered exclusively by WASM UI plugins — there is
//! no native fallback renderer. These builtins register the bundled guest
//! `.wasm` modules so they load automatically at startup, the same way a
//! `plugin link`-ed plugin would, without requiring the user to install anything.
//!
//! The data layer stays native: the host collects the usage/goal data each tick
//! and feeds it to the guest via the plugin frame env. Only the render seam is
//! WASM. Missing `.wasm` ⇒ empty surface (no native re-render) — this is by
//! design (see the dash-wasm-migration plan).
//!
//! The guest modules are built from `plugins/usage-dash-ui` and
//! `plugins/goal-dash-ui` to `target/wasm32-unknown-unknown/release/*.wasm`.
//! They are resolved relative to the herdr source tree (`CARGO_MANIFEST_DIR`),
//! or to `$HERDR_UI_PLUGIN_DIR` when set, so rebuilding a guest crate hot-reloads
//! the panel in a running herdr without restarting `herdrd`.

use std::path::PathBuf;

use crate::api::schema::plugins::{
    InstalledPluginInfo, PluginHotReloadMode, PluginManifestUiModule, PluginSourceInfo,
    PluginUiCapability, PluginUiModuleKind, PluginUiSurface,
};

/// Directory that holds the built guest `.wasm` modules. `$HERDR_UI_PLUGIN_DIR`
/// wins when set (deployment / packaged builds); otherwise the modules are
/// resolved from the source tree's `plugins/.../target` output so a `cargo build`
/// of the guest crate hot-reloads the panel.
fn guest_release_dir(plugin_dir_name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("HERDR_UI_PLUGIN_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("plugins")
        .join(plugin_dir_name)
        .join("target/wasm32-unknown-unknown/release")
}

fn builtin(
    plugin_id: &str,
    name: &str,
    module_id: &str,
    title: &str,
    plugin_dir_name: &str,
    wasm_file: &str,
    surface: PluginUiSurface,
    api_version: u32,
) -> InstalledPluginInfo {
    let root = guest_release_dir(plugin_dir_name);
    InstalledPluginInfo {
        plugin_id: plugin_id.to_string(),
        name: name.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        min_herdr_version: String::new(),
        description: Some(format!("built-in {title} (wasm)")),
        manifest_path: root
            .join("herdr-plugin.toml")
            .to_string_lossy()
            .into_owned(),
        plugin_root: root.to_string_lossy().into_owned(),
        enabled: true,
        platforms: None,
        build: Vec::new(),
        actions: Vec::new(),
        events: Vec::new(),
        panes: Vec::new(),
        ui_modules: vec![PluginManifestUiModule {
            id: module_id.to_string(),
            title: title.to_string(),
            description: None,
            platforms: None,
            kind: PluginUiModuleKind::Wasm,
            api_version,
            module: wasm_file.to_string(),
            hot_reload: PluginHotReloadMode::On,
            surfaces: vec![surface],
            // Frame events (the per-tick render with live data + rect) are only
            // delivered to modules that request LayoutRead or FocusedSession.
            // Both dashes render from the frame rect + focused-session data.
            capabilities: vec![
                PluginUiCapability::LayoutRead,
                PluginUiCapability::FocusedSession,
            ],
        }],
        link_handlers: Vec::new(),
        source: PluginSourceInfo::default(),
        warnings: Vec::new(),
    }
}

/// The built-in dash plugins, keyed by `plugin_id`. Merged into the installed
/// plugin registry at `App::new` so the dash surfaces always have a renderer.
pub(crate) fn builtin_dash_plugins() -> Vec<InstalledPluginInfo> {
    vec![
        builtin(
            "builtin.usage-dash",
            "Usage Dashboard",
            "usage",
            "Usage Dash",
            "usage-dash-ui",
            "usage_dash_ui.wasm",
            PluginUiSurface::UsageStrip,
            // Both builtin guests still hardcode `herdr_ui_api_version() -> 2`
            // (plugins/usage-dash-ui, plugins/goal-dash-ui); the manifest's
            // declared api_version must equal the actual export (§2), so this
            // stays 2 regardless of the host's max-supported API_VERSION.
            2,
        ),
        builtin(
            "builtin.goal-dash",
            "Goal Dashboard",
            "goal",
            "Goal Dash",
            "goal-dash-ui",
            "goal_dash_ui.wasm",
            PluginUiSurface::RightPanel,
            2,
        ),
    ]
}
