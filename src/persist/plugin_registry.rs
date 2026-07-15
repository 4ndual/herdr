use std::path::{Path, PathBuf};

use tracing::warn;

use crate::api::schema::InstalledPluginInfo;

pub const MANIFEST_UNAVAILABLE_WARNING_PREFIX: &str = "manifest unavailable: ";

fn registry_path() -> PathBuf {
    crate::session::data_dir().join("plugins.json")
}

fn save_json_to_path<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value)?;
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &json)?;
    #[cfg(windows)]
    if path.exists() {
        if let Err(err) = std::fs::remove_file(path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
    }
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

/// Atomically write `plugins.json` next to `session.json`.
pub fn save(plugins: &[InstalledPluginInfo]) -> std::io::Result<()> {
    let path = registry_path();
    save_to_path(&path, plugins)
}

pub fn save_to_path(path: &Path, plugins: &[InstalledPluginInfo]) -> std::io::Result<()> {
    save_json_to_path(path, plugins)
}

/// Load `plugins.json`.  Returns an empty vec on any failure so a corrupt or
/// missing file never blocks server startup.
pub fn load() -> Vec<InstalledPluginInfo> {
    load_from_path(&registry_path())
}

pub fn load_from_path(path: &Path) -> Vec<InstalledPluginInfo> {
    if !path.exists() {
        return Vec::new();
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %path.display(), err = %err, "failed to read plugin registry");
            return Vec::new();
        }
    };
    match serde_json::from_str::<Vec<InstalledPluginInfo>>(&content) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(path = %path.display(), err = %err, "failed to parse plugin registry, starting with empty registry");
            Vec::new()
        }
    }
}

/// Re-read each entry's manifest from disk using the provided reload function.
///
/// If the manifest parses successfully, replace cached fields but keep the
/// stored `enabled` flag.  If the file is gone or unparseable, keep the stored
/// entry and append a warning so `plugin.list` surfaces it.
pub fn reload_manifests(
    mut entries: Vec<InstalledPluginInfo>,
    reload_fn: impl Fn(&str, bool) -> Result<InstalledPluginInfo, String>,
) -> Vec<InstalledPluginInfo> {
    for entry in &mut entries {
        entry.warnings.clear();
        match reload_fn(&entry.manifest_path, entry.enabled) {
            Ok(mut fresh) => {
                fresh.enabled = entry.enabled;
                fresh.source = entry.source.clone();
                *entry = fresh;
            }
            Err(warn_msg) => {
                entry
                    .warnings
                    .push(format!("{MANIFEST_UNAVAILABLE_WARNING_PREFIX}{warn_msg}"));
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir()
            .join(format!(
                "herdr-registry-{name}-{}-{nanos}",
                std::process::id()
            ))
            .join("plugins.json")
    }

    fn sample_plugin(id: &str) -> InstalledPluginInfo {
        InstalledPluginInfo {
            plugin_id: id.to_string(),
            name: "Test Plugin".to_string(),
            version: "0.1.0".to_string(),
            min_herdr_version: crate::build_info::BASE_VERSION.to_string(),
            description: None,
            manifest_path: format!("/tmp/{id}/herdr-plugin.toml"),
            plugin_root: format!("/tmp/{id}"),
            enabled: true,
            platforms: None,
            build: vec![],
            actions: vec![],
            events: vec![],
            panes: vec![],
            ui_modules: vec![],
            link_handlers: vec![],
            source: Default::default(),
            warnings: vec![],
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let path = temp_registry_path("roundtrip");
        let plugins = vec![sample_plugin("example.a"), sample_plugin("example.b")];

        save_to_path(&path, &plugins).unwrap();

        let loaded = load_from_path(&path);
        assert_eq!(loaded.len(), 2);
        let ids: Vec<_> = loaded.iter().map(|p| p.plugin_id.as_str()).collect();
        assert!(ids.contains(&"example.a"));
        assert!(ids.contains(&"example.b"));
    }

    #[test]
    fn missing_file_returns_empty() {
        let path = temp_registry_path("missing");
        let loaded = load_from_path(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn corrupt_file_returns_empty_without_panic() {
        let path = temp_registry_path("corrupt");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"this is not valid json {{{{").unwrap();

        let loaded = load_from_path(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn reload_manifests_keeps_entry_with_warning_on_missing_manifest() {
        let entry = sample_plugin("example.missing");
        let entries = vec![entry];

        let result = reload_manifests(entries, |path, _enabled| {
            Err(format!("manifest not found at {path}"))
        });

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].plugin_id, "example.missing");
        assert!(!result[0].warnings.is_empty());
        assert!(result[0].warnings[0].contains("manifest not found"));
    }

    #[test]
    fn reload_manifests_uses_fresh_parse_and_keeps_enabled_flag() {
        let mut entry = sample_plugin("example.reload");
        entry.enabled = false;
        entry.source = crate::api::schema::PluginSourceInfo {
            kind: crate::api::schema::PluginSourceKind::Github,
            owner: Some("ogulcancelik".into()),
            repo: Some("herdr-plugin-examples".into()),
            subdir: Some("worktree-bootstrap".into()),
            requested_ref: Some("main".into()),
            resolved_commit: Some("abc123".into()),
            managed_path: Some("/tmp/herdr/plugins/github/example.reload".into()),
            installed_unix_ms: Some(42),
        };

        let result = reload_manifests(vec![entry], |_path, _enabled| {
            Ok(InstalledPluginInfo {
                plugin_id: "example.reload".to_string(),
                name: "Fresh Name".to_string(),
                version: "0.2.0".to_string(),
                min_herdr_version: crate::build_info::BASE_VERSION.to_string(),
                description: Some("refreshed".to_string()),
                manifest_path: "/tmp/example.reload/herdr-plugin.toml".to_string(),
                plugin_root: "/tmp/example.reload".to_string(),
                enabled: true, // caller would pass stored enabled; fresh parse returns true
                platforms: None,
                build: vec![],
                actions: vec![],
                events: vec![],
                panes: vec![],
                ui_modules: vec![],
                link_handlers: vec![],
                source: Default::default(),
                warnings: vec![],
            })
        });

        assert_eq!(result[0].name, "Fresh Name");
        assert_eq!(result[0].version, "0.2.0");
        // enabled preserved from stored entry
        assert!(!result[0].enabled);
        assert_eq!(
            result[0].source.kind,
            crate::api::schema::PluginSourceKind::Github
        );
        assert_eq!(result[0].source.owner.as_deref(), Some("ogulcancelik"));
        assert!(result[0].warnings.is_empty());
    }

    #[test]
    fn atomic_write_temp_file_is_cleaned_up_on_rename_failure() {
        // Write to a path whose parent does not yet exist, then verify the
        // tmp file is removed when the write fails mid-way.  Here we just
        // confirm a successful write leaves no .tmp file behind.
        let path = temp_registry_path("cleanup");
        save_to_path(&path, &[sample_plugin("example.cleanup")]).unwrap();

        let tmp = path.with_extension("json.tmp");
        assert!(
            !tmp.exists(),
            "tmp file should be cleaned up after successful rename"
        );
        assert!(path.exists());
    }

    #[test]
    fn save_replaces_existing_registry_file() {
        let path = temp_registry_path("replace-existing");
        save_to_path(&path, &[sample_plugin("example.first")]).unwrap();
        save_to_path(&path, &[sample_plugin("example.second")]).unwrap();

        let loaded = load_from_path(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].plugin_id, "example.second");
    }

    /// A fully-populated API v3 `ui_modules` entry with every field the fork adds
    /// set to a non-default value, so a dropped field is caught by equality.
    fn sample_plugin_with_v3_ui_module(id: &str) -> InstalledPluginInfo {
        use crate::api::schema::{
            PluginHotReloadMode, PluginManifestUiModule, PluginPlatform, PluginUiCapability,
            PluginUiModuleKind, PluginUiSurface,
        };

        let mut plugin = sample_plugin(id);
        plugin.ui_modules = vec![PluginManifestUiModule {
            id: format!("{id}.dash"),
            title: "Dash".to_string(),
            description: Some("v3 ui module".to_string()),
            platforms: Some(vec![PluginPlatform::Macos, PluginPlatform::Linux]),
            kind: PluginUiModuleKind::Wasm,
            api_version: 3,
            module: "dash.wasm".to_string(),
            hot_reload: PluginHotReloadMode::Off,
            surfaces: vec![PluginUiSurface::UsageStrip, PluginUiSurface::Header],
            capabilities: vec![
                PluginUiCapability::ApiRead,
                PluginUiCapability::ApiWrite,
                PluginUiCapability::ApiInput,
                PluginUiCapability::FocusedSession,
            ],
            ui_input_file: Some("input.json".to_string()),
            collector: None,
        }];
        plugin
    }

    /// Every field of a v3 `ui_modules` entry (api_version 3, api_read/api_write/
    /// api_input capabilities, ui_input_file, usage_strip surface) must survive a
    /// `save`/`load` round-trip through `plugins.json` unchanged. Guards against a
    /// future fork refactor silently dropping fork-only fields on rewrite.
    #[test]
    fn v3_ui_modules_survive_save_load_roundtrip() {
        let path = temp_registry_path("v3-ui-modules");
        let original = sample_plugin_with_v3_ui_module("example.v3");

        save_to_path(&path, std::slice::from_ref(&original)).unwrap();
        let loaded = load_from_path(&path);

        assert_eq!(loaded.len(), 1);
        // Full-struct equality: any dropped or mutated field fails here.
        assert_eq!(loaded[0], original);

        // Spell out the fork-only fields so a regression names the culprit.
        let module = &loaded[0].ui_modules[0];
        assert_eq!(module.api_version, 3);
        assert_eq!(module.ui_input_file.as_deref(), Some("input.json"));
        assert!(module
            .capabilities
            .contains(&crate::api::schema::PluginUiCapability::ApiRead));
        assert!(module
            .capabilities
            .contains(&crate::api::schema::PluginUiCapability::ApiWrite));
        assert!(module
            .capabilities
            .contains(&crate::api::schema::PluginUiCapability::ApiInput));
        assert!(module
            .surfaces
            .contains(&crate::api::schema::PluginUiSurface::UsageStrip));
    }

    /// Characterizes the forward-compat behavior of `InstalledPluginInfo` today:
    /// the type does NOT use `#[serde(deny_unknown_fields)]`, so loading a
    /// registry written by a *newer* binary (extra keys) does not error — but a
    /// typed round-trip DROPS any key the running binary's struct does not know.
    ///
    /// This is the real forward-compat hazard from the fork-update-safety audit
    /// (Finding 3): an older `herdr` binary whose `InstalledPluginInfo` lacks
    /// `ui_modules` loads fork state fine, then rewrites `plugins.json` WITHOUT
    /// the fork's ui modules on the next save. Here the same relationship is
    /// exercised with a hypothetical future field standing in for `ui_modules`.
    /// If this assertion ever flips (unknown fields start erroring or surviving),
    /// the forward-compat contract changed and the audit must be revisited.
    #[test]
    fn unknown_fields_are_ignored_on_load_but_dropped_on_rewrite() {
        let with_unknown_field = r#"[
            {
                "plugin_id": "example.future",
                "name": "Future Plugin",
                "version": "9.9.9",
                "manifest_path": "/tmp/example.future/herdr-plugin.toml",
                "plugin_root": "/tmp/example.future",
                "enabled": true,
                "ui_modules": [
                    {
                        "id": "example.future.dash",
                        "title": "Dash",
                        "api_version": 4,
                        "module": "dash.wasm",
                        "ui_input_file": "input.json"
                    }
                ],
                "future_field_unknown_to_this_binary": {
                    "some": "value",
                    "count": 7
                }
            }
        ]"#;

        // No deny_unknown_fields: the load succeeds despite the unknown key.
        let loaded: Vec<InstalledPluginInfo> = serde_json::from_str(with_unknown_field)
            .expect("unknown fields must not cause a load error");
        assert_eq!(loaded.len(), 1);
        // Fields the struct DOES know are preserved.
        assert_eq!(loaded[0].plugin_id, "example.future");
        assert_eq!(loaded[0].ui_modules[0].api_version, 4);
        assert_eq!(
            loaded[0].ui_modules[0].ui_input_file.as_deref(),
            Some("input.json")
        );

        // Rewriting through the typed struct DROPS the unknown key — the strip
        // hazard the audit documents. `ui_modules` itself survives only because
        // this binary's struct knows it; a binary lacking the field would drop it
        // identically.
        let rewritten = serde_json::to_string(&loaded).unwrap();
        assert!(
            !rewritten.contains("future_field_unknown_to_this_binary"),
            "unknown field unexpectedly preserved on rewrite; forward-compat behavior changed"
        );
        assert!(
            rewritten.contains("ui_modules"),
            "ui_modules should survive a rewrite by a binary that knows the field"
        );
    }
}
