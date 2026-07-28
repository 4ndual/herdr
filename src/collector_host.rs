//! Generic out-of-process collector host glue for dashboard plugins OTHER than
//! usage-dash.
//!
//! usage-dash keeps its bespoke path in [`crate::usage_host`] because it
//! deserializes the collector snapshot into a typed `UsageSnapshot`. Every other
//! plugin whose UI module declares BOTH a `collector` and a `ui_input_file` is
//! module-agnostic: the collector's `replace` snapshot is a free-form JSON object
//! the host writes VERBATIM into the module's `ui_input_file`, and the generic
//! §7.1 reader ([`crate::app`]) injects it as `HERDR_PLUGIN_UI_INPUT_JSON` on the
//! next frame. This is the neutral server-side collector data path the
//! runtime/client guardrail calls for — no UI-surface names, no per-plugin host
//! code.

use crate::app::App;
use crate::plugin_collector::CollectorOutcome;

/// Launch the collector for every enabled non-usage plugin module that declares
/// one, once per dashboard tick. Called next to [`crate::usage_host::tick`].
pub fn tick(app: &mut App) {
    let focus = crate::plugin_focus::focus_input(&app.state);

    // Snapshot the (plugin_id, module) launch targets first so we can take
    // `&mut App` for each launch without holding a borrow on `installed_plugins`.
    let targets: Vec<(String, crate::api::schema::PluginManifestUiModule)> = app
        .state
        .installed_plugins
        .values()
        .filter(|plugin| plugin.enabled && plugin.plugin_id != crate::app::USAGE_DASH_PLUGIN_ID)
        .flat_map(|plugin| {
            let plugin_id = plugin.plugin_id.clone();
            plugin
                .ui_modules
                .iter()
                .filter(|module| module.collector.is_some() && module.ui_input_file.is_some())
                .map(move |module| (plugin_id.clone(), module.clone()))
                .collect::<Vec<_>>()
        })
        .collect();

    for (plugin_id, module) in targets {
        crate::plugin_collector::launch_if_idle(app, &plugin_id, &module, &focus);
    }
}

/// Apply a finished collector's sticky outcome for a non-usage module by writing
/// the snapshot verbatim into the module's `ui_input_file`. `replace` writes the
/// snapshot JSON; `clear` writes an empty object (so the guest renders its empty
/// state); `keep` leaves the file untouched. Returns the reason on write failure
/// so the caller can warn once per distinct message.
pub fn apply_outcome(
    app: &App,
    plugin_id: &str,
    module_id: &str,
    outcome: &CollectorOutcome,
) -> Result<(), String> {
    let ui_input_file = app
        .state
        .installed_plugins
        .get(plugin_id)
        .and_then(|plugin| plugin.ui_modules.iter().find(|m| m.id == module_id))
        .and_then(|module| module.ui_input_file.clone());
    let Some(ui_input_file) = ui_input_file else {
        // Module no longer present or dropped its ui_input_file; nothing to write.
        return Ok(());
    };

    match outcome {
        CollectorOutcome::Replace(value) => {
            let contents =
                serde_json::to_string(value).map_err(|err| format!("snapshot serialize: {err}"))?;
            crate::app::write_plugin_ui_input(plugin_id, &ui_input_file, &contents)
                .map_err(|err| format!("write {ui_input_file}: {err}"))
        }
        CollectorOutcome::Keep => Ok(()),
        CollectorOutcome::Clear => crate::app::write_plugin_ui_input(plugin_id, &ui_input_file, "{}")
            .map_err(|err| format!("write {ui_input_file}: {err}")),
    }
}
