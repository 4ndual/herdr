use crate::api::schema::{InstalledPluginInfo, PluginHotReloadMode, PluginUiSurface};
use dash_common::FocusInput;
use herdr_ui_plugin_api::{
    HostEffect, HostSnapshot, KeyEvent as UiKeyEvent, MouseEvent, MouseEventKind, UiActionKind,
    UiActionRef, UiCapability, UiEvent, UiFrameContext, UiLayer, UiLayoutRect, UiNode,
    UiPluginManifestRef, UiSurface, UiUpdate, UiUpdateScope,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Spec §5.4: at most this many `HostEffect::Api` effects are executed from one
/// `UiUpdate`; any beyond it are refused (never silently dropped — the caller
/// still gets an `ok:false error:"api_budget_exhausted"` result).
pub(crate) const MAX_API_EFFECTS_PER_UPDATE: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisteredUiModule {
    pub plugin_id: String,
    pub module_id: String,
    pub title: String,
    pub module_path: PathBuf,
    pub manifest_path: PathBuf,
    pub api_version: u32,
    /// The version actually negotiated with the loaded guest export (2 or 3),
    /// recomputed on every successful hot-reload (§2). Gates delivery of the v3
    /// `Action`/`ApiResult` events in `module_accepts_event`.
    pub negotiated_version: u32,
    pub hot_reload: PluginHotReloadMode,
    pub surfaces: Vec<PluginUiSurface>,
    pub requested_capabilities: Vec<crate::api::schema::PluginUiCapability>,
}

/// One `(plugin_id, module_id)`-owned layer, as exposed to rendering/hit-testing
/// (spec §4 identity threading). Ordered by `surface_layers()` using the total
/// order `(z_index desc, plugin_id asc, module_id asc)` reserved for §9 dock
/// arbitration, so the vec is already topmost-hit-owner-first; rendering walks
/// it in reverse to paint bottom-to-top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UiSurfaceLayer {
    pub plugin_id: String,
    pub module_id: String,
    pub negotiated_version: u32,
    pub layer: UiLayer,
}

/// A `HostEffect::Api` effect extracted from a module's update, tagged with the
/// emitting module's identity so `App::apply_ui_plugin_effects` can run the
/// capability policy (§5.3) and deliver the `ApiResult` back to the right
/// module. `budget_exhausted` is set at extraction time for anything past the
/// per-update `MAX_API_EFFECTS_PER_UPDATE`th effect: those are never executed.
#[derive(Debug, Clone)]
pub(crate) struct PendingApiEffect {
    pub plugin_id: String,
    pub module_id: String,
    pub request_id: String,
    pub method: String,
    pub params: serde_json::Value,
    pub budget_exhausted: bool,
}

impl RegisteredUiModule {
    fn requests(&self, capability: crate::api::schema::PluginUiCapability) -> bool {
        self.requested_capabilities.contains(&capability)
    }

    pub(crate) fn manifest_ref(&self) -> UiPluginManifestRef {
        UiPluginManifestRef {
            plugin_id: self.plugin_id.clone(),
            module_id: self.module_id.clone(),
            api_version: self.api_version,
            surfaces: self.surfaces.iter().copied().map(map_surface).collect(),
            requested_capabilities: self
                .requested_capabilities
                .iter()
                .copied()
                .map(map_capability)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct UiPluginManager {
    modules: Vec<RegisteredUiModule>,
    by_surface: HashMap<PluginUiSurface, Vec<usize>>,
    mtimes: HashMap<PathBuf, SystemTime>,
    loaded_updates: HashMap<(String, String), UiUpdate>,
    last_errors: HashMap<(String, String), String>,
    pending_effects: Vec<HostEffect>,
    pending_notifications: Vec<String>,
    /// API v3 (§5): effects pending capability-policy check + execution by
    /// `App::apply_ui_plugin_effects`. Includes budget-refused entries so every
    /// request still resolves to exactly one result (§5.4).
    pending_api_effects: Vec<PendingApiEffect>,
}

impl UiPluginManager {
    pub(crate) fn from_installed_plugins(plugins: &HashMap<String, InstalledPluginInfo>) -> Self {
        let mut modules = Vec::new();
        let mut by_surface: HashMap<PluginUiSurface, Vec<usize>> = HashMap::new();
        let mut mtimes = HashMap::new();
        let mut loaded_updates = HashMap::new();
        let mut last_errors = HashMap::new();

        let mut plugin_list = plugins.values().collect::<Vec<_>>();
        plugin_list.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
        for plugin in plugin_list {
            if !plugin.enabled {
                continue;
            }
            let root = Path::new(&plugin.plugin_root);
            let manifest_path = PathBuf::from(&plugin.manifest_path);
            for module in &plugin.ui_modules {
                let mut registered = RegisteredUiModule {
                    plugin_id: plugin.plugin_id.clone(),
                    module_id: module.id.clone(),
                    title: module.title.clone(),
                    module_path: root.join(&module.module),
                    manifest_path: manifest_path.clone(),
                    api_version: module.api_version,
                    negotiated_version: module.api_version,
                    hot_reload: module.hot_reload,
                    surfaces: module.surfaces.clone(),
                    requested_capabilities: module.capabilities.clone(),
                };
                let idx = modules.len();
                for surface in &registered.surfaces {
                    by_surface.entry(*surface).or_default().push(idx);
                }
                if let Ok(meta) = std::fs::metadata(&registered.module_path) {
                    if let Ok(mtime) = meta.modified() {
                        mtimes.insert(registered.module_path.clone(), mtime);
                    }
                }
                match load_static_update(&registered) {
                    Ok((update, negotiated_version)) => {
                        registered.negotiated_version = negotiated_version;
                        loaded_updates.insert(module_key(&registered), update);
                    }
                    Err(err) => {
                        tracing::warn!(
                            plugin_id = %registered.plugin_id,
                            module_id = %registered.module_id,
                            module_path = %registered.module_path.display(),
                            error = %err,
                            "failed to load ui plugin module"
                        );
                        last_errors.insert(module_key(&registered), err);
                    }
                }
                modules.push(registered);
            }
        }

        Self {
            modules,
            by_surface,
            mtimes,
            loaded_updates,
            last_errors,
            pending_effects: Vec::new(),
            pending_notifications: Vec::new(),
            pending_api_effects: Vec::new(),
        }
    }

    pub(crate) fn all(&self) -> &[RegisteredUiModule] {
        &self.modules
    }

    pub(crate) fn has_modules(&self) -> bool {
        !self.modules.is_empty()
    }

    pub(crate) fn for_surface(&self, surface: PluginUiSurface) -> Vec<&RegisteredUiModule> {
        self.by_surface
            .get(&surface)
            .into_iter()
            .flat_map(|indices| indices.iter().copied())
            .filter_map(|idx| self.modules.get(idx))
            .collect()
    }

    pub(crate) fn detect_changed_modules(&mut self) -> Vec<RegisteredUiModule> {
        let mut changed = Vec::new();
        for idx in 0..self.modules.len() {
            let module = self.modules[idx].clone();
            let Ok(meta) = std::fs::metadata(&module.module_path) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            let old = self.mtimes.get(&module.module_path).copied();
            if old.is_none() || old.is_some_and(|old| old < mtime) {
                self.mtimes.insert(module.module_path.clone(), mtime);
                let key = module_key(&module);
                match load_static_update(&module) {
                    Ok((update, negotiated_version)) => {
                        // §6: hot-reload clears the merge cache — a full replace
                        // (not a partial merge) so a rebuilt module that renders
                        // fewer surfaces doesn't keep stale layers.
                        self.modules[idx].negotiated_version = negotiated_version;
                        self.loaded_updates.insert(key.clone(), update);
                        if self.last_errors.remove(&key).is_some() {
                            self.pending_notifications.push(format!(
                                "ui plugin {}:{} reloaded",
                                module.plugin_id, module.module_id
                            ));
                        }
                        if module.hot_reload == PluginHotReloadMode::On {
                            changed.push(self.modules[idx].clone());
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            plugin_id = %module.plugin_id,
                            module_id = %module.module_id,
                            module_path = %module.module_path.display(),
                            error = %err,
                            "failed to hot-reload ui plugin module"
                        );
                        let notify = self.last_errors.get(&key) != Some(&err);
                        self.last_errors.insert(key, err.clone());
                        if notify {
                            self.pending_notifications.push(format!(
                                "ui plugin {}:{} reload failed: {}",
                                module.plugin_id, module.module_id, err
                            ));
                        }
                    }
                }
            }
        }
        changed
    }

    /// Drops the merge cache for one module (§6: hot-reload / trap-recreate
    /// clears the cache so the fresh instance's next surfaces fully replace the
    /// stale ones instead of partial-merging into them). The static-reload path
    /// above already gets this via `HashMap::insert` full-replace; this is the
    /// named entry point the wasmtime runtime's trap-recreate path (owned by the
    /// parallel runtime agent, §3) should call after re-instantiating a trapped
    /// module.
    ///
    /// Unused within this task's scope (persistent instances/trap-recreate are
    /// §3, out of scope here) — kept `#[allow(dead_code)]` rather than deleted
    /// so the runtime agent has a stable name to wire up instead of
    /// reinventing this cache-clear.
    #[allow(dead_code)]
    pub(crate) fn clear_module_cache(&mut self, plugin_id: &str, module_id: &str) {
        self.loaded_updates
            .remove(&(plugin_id.to_string(), module_id.to_string()));
    }

    pub(crate) fn context_menu_items(&self) -> Vec<UiActionRef> {
        let mut out = Vec::new();
        for module in self.for_surface(PluginUiSurface::ContextMenu) {
            if let Some(update) = self
                .loaded_updates
                .get(&(module.plugin_id.clone(), module.module_id.clone()))
            {
                out.extend(context_menu_items_from_update(update));
            }
        }
        out
    }

    /// Spec §4: deterministic overlap ownership. Iterates `self.modules` (a
    /// `Vec`, already sorted `plugin_id` then `module_id` at registration) so the
    /// input is never HashMap-order-dependent, then applies the exact total sort
    /// `(z_index desc, plugin_id asc, module_id asc)` reserved for §9 dock
    /// arbitration — this is the fix for the old `HashMap`-iteration-order bug
    /// (a stable sort by `z_index` alone was stable over nondeterministic input).
    pub(crate) fn surface_layers(&self) -> Vec<UiSurfaceLayer> {
        let mut layers = Vec::new();
        for module in &self.modules {
            let Some(update) = self.loaded_updates.get(&module_key(module)) else {
                continue;
            };
            for layer in update
                .layers
                .iter()
                .filter(|layer| layer.surface != UiSurface::ContextMenu)
                .cloned()
            {
                layers.push(UiSurfaceLayer {
                    plugin_id: module.plugin_id.clone(),
                    module_id: module.module_id.clone(),
                    negotiated_version: module.negotiated_version,
                    layer,
                });
            }
        }
        layers.sort_by(|a, b| {
            b.layer
                .z_index
                .cmp(&a.layer.z_index)
                .then_with(|| a.plugin_id.cmp(&b.plugin_id))
                .then_with(|| a.module_id.cmp(&b.module_id))
        });
        layers
    }

    pub(crate) fn dispatch_crossterm_mouse_event(
        &mut self,
        mouse: crossterm::event::MouseEvent,
    ) -> bool {
        let Some(kind) = map_crossterm_mouse_kind(mouse.kind) else {
            return false;
        };
        let event = UiEvent::Mouse(MouseEvent {
            kind,
            column: mouse.column,
            row: mouse.row,
        });
        self.dispatch_event(&event)
    }

    pub(crate) fn dispatch_crossterm_key_event(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let Some(event) = map_crossterm_key_event(key) else {
            return false;
        };
        self.dispatch_event(&UiEvent::Key(event))
    }

    pub(crate) fn dispatch_tick(&mut self) -> bool {
        self.dispatch_event(&UiEvent::Tick)
    }

    pub(crate) fn dispatch_reload_requested(&mut self) -> bool {
        self.dispatch_event(&UiEvent::ReloadRequested)
    }

    pub(crate) fn dispatch_focus_changed(&mut self, focus: FocusInput) -> bool {
        self.dispatch_event(&UiEvent::FocusChanged(focus))
    }

    pub(crate) fn dispatch_theme_changed(&mut self) -> bool {
        self.dispatch_event(&UiEvent::ThemeChanged)
    }

    pub(crate) fn dispatch_frame(
        &mut self,
        plugin_id: &str,
        module_id: &str,
        surface: UiSurface,
        rect: UiLayoutRect,
        focused: bool,
        focus: FocusInput,
        env: HashMap<String, String>,
    ) -> bool {
        let event = UiEvent::Frame(UiFrameContext {
            surface,
            rect,
            focused,
            host: HostSnapshot { focus, env },
        });
        self.dispatch_event_for_module(plugin_id, module_id, &event)
    }

    pub(crate) fn take_pending_effects(&mut self) -> Vec<HostEffect> {
        std::mem::take(&mut self.pending_effects)
    }

    pub(crate) fn take_pending_notifications(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_notifications)
    }

    /// True when there are queued notifications or host effects waiting to be
    /// applied. Used by the scheduled tick so a failed hot-reload (which leaves
    /// `changed_ui_modules` empty and may also fail `dispatch_tick`) still
    /// flushes its "reload failed" toast to the user instead of swallowing it.
    /// Also true for parked API bridge calls (§5.4 chained-round budget) so a
    /// parked batch keeps getting retried on the next tick instead of stalling.
    pub(crate) fn has_pending_updates(&self) -> bool {
        !self.pending_notifications.is_empty()
            || !self.pending_effects.is_empty()
            || !self.pending_api_effects.is_empty()
    }

    /// API v3 (§5): effects awaiting policy check + execution by
    /// `App::apply_ui_plugin_effects`.
    pub(crate) fn take_pending_api_effects(&mut self) -> Vec<PendingApiEffect> {
        std::mem::take(&mut self.pending_api_effects)
    }

    /// API v3 (§5.4): re-queue effects that couldn't be processed within the
    /// per-host-turn chained-round budget, so they are retried on the next tick
    /// instead of being dropped.
    pub(crate) fn park_pending_api_effects(&mut self, mut effects: Vec<PendingApiEffect>) {
        self.pending_api_effects.append(&mut effects);
    }

    /// API v3 (§5.3): whether the still-registered module `(plugin_id,
    /// module_id)` declared `capability` in its manifest. Used by the bridge's
    /// capability-policy check before executing a `HostEffect::Api` call; a
    /// module that vanished (unlinked mid-turn) reports `false`, which the
    /// caller treats as a deny.
    pub(crate) fn module_requests(
        &self,
        plugin_id: &str,
        module_id: &str,
        capability: crate::api::schema::PluginUiCapability,
    ) -> bool {
        self.modules
            .iter()
            .find(|module| module.plugin_id == plugin_id && module.module_id == module_id)
            .is_some_and(|module| module.requests(capability))
    }

    /// API v3 (§4): dispatch a resolved click's `Action` event to the one
    /// owning module. `action_id` is module-private and never interpreted here.
    pub(crate) fn dispatch_action(
        &mut self,
        plugin_id: &str,
        module_id: &str,
        action_id: String,
        surface: UiSurface,
        column: u16,
        row: u16,
        kind: UiActionKind,
    ) -> bool {
        let event = UiEvent::Action {
            action_id,
            surface,
            column,
            row,
            kind,
        };
        self.dispatch_event_for_module(plugin_id, module_id, &event)
    }

    /// API v3 (§5.2/§5.4): deliver a bridge call's result. Discarded (not an
    /// error) if the module was unlinked/disabled before dispatch — exactly-one-
    /// result only holds while the module remains registered.
    pub(crate) fn dispatch_api_result(
        &mut self,
        plugin_id: &str,
        module_id: &str,
        request_id: String,
        ok: bool,
        response: Option<serde_json::Value>,
        error: Option<String>,
    ) -> bool {
        if !self
            .modules
            .iter()
            .any(|module| module.plugin_id == plugin_id && module.module_id == module_id)
        {
            return false;
        }
        let event = UiEvent::ApiResult {
            request_id,
            ok,
            response,
            error,
        };
        self.dispatch_event_for_module(plugin_id, module_id, &event)
    }

    fn dispatch_event(&mut self, event: &UiEvent) -> bool {
        let keys = self
            .modules
            .iter()
            .map(|module| (module.plugin_id.clone(), module.module_id.clone()))
            .collect::<Vec<_>>();
        let mut changed = false;
        for (plugin_id, module_id) in keys {
            changed |= self.dispatch_event_for_module(&plugin_id, &module_id, event);
        }
        changed
    }

    fn dispatch_event_for_module(
        &mut self,
        plugin_id: &str,
        module_id: &str,
        event: &UiEvent,
    ) -> bool {
        let Some(module) = self
            .modules
            .iter()
            .find(|module| module.plugin_id == plugin_id && module.module_id == module_id)
        else {
            return false;
        };
        if !module_accepts_event(module, event) {
            return false;
        }
        let key = (plugin_id.to_string(), module_id.to_string());
        let update = match dispatch_event_update(module, event) {
            Ok(Some(update)) => update,
            Ok(None) => return false,
            Err(err) => {
                tracing::warn!(
                    plugin_id = %module.plugin_id,
                    module_id = %module.module_id,
                    module_path = %module.module_path.display(),
                    error = %err,
                    "failed to handle ui plugin event"
                );
                let notify = self.last_errors.get(&key) != Some(&err);
                self.last_errors.insert(key, err.clone());
                if notify {
                    self.pending_notifications.push(format!(
                        "ui plugin {}:{} event failed: {}",
                        module.plugin_id, module.module_id, err
                    ));
                }
                return false;
            }
        };
        if self.last_errors.remove(&key).is_some() {
            self.pending_notifications.push(format!(
                "ui plugin {}:{} recovered",
                module.plugin_id, module.module_id
            ));
        }
        let negotiated_version = module.negotiated_version;
        let (api_effects, other_effects): (Vec<HostEffect>, Vec<HostEffect>) = update
            .effects
            .iter()
            .cloned()
            .partition(|effect| matches!(effect, HostEffect::Api { .. }));
        self.pending_effects
            .extend(filter_effects_for_module(module, &other_effects));
        queue_api_effects(
            &mut self.pending_api_effects,
            plugin_id,
            module_id,
            api_effects,
        );
        let merged_layers = merge_layers(
            self.loaded_updates
                .get(&key)
                .map(|old| old.layers.as_slice()),
            update.layers,
            negotiated_version,
            update.scope,
        );
        self.loaded_updates.insert(
            key,
            UiUpdate {
                layers: merged_layers,
                effects: Vec::new(),
                scope: update.scope,
                consumed_action_ids: Vec::new(),
            },
        );
        true
    }
}

/// Spec §5.4 per-update budget: at most `MAX_API_EFFECTS_PER_UPDATE` `Api`
/// effects from one update are executed; the rest are queued pre-refused with
/// `api_budget_exhausted` so every request still resolves to exactly one result
/// instead of being silently dropped. Takes the queue field directly (rather
/// than being a `&mut self` method) so the borrow checker can see it touches
/// only `pending_api_effects`, not the `modules` field the caller still holds a
/// live reference into.
fn queue_api_effects(
    queue: &mut Vec<PendingApiEffect>,
    plugin_id: &str,
    module_id: &str,
    effects: Vec<HostEffect>,
) {
    for (idx, effect) in effects.into_iter().enumerate() {
        let HostEffect::Api {
            request_id,
            method,
            params,
        } = effect
        else {
            continue;
        };
        let budget_exhausted = idx >= MAX_API_EFFECTS_PER_UPDATE;
        if budget_exhausted {
            tracing::warn!(
                plugin_id,
                module_id,
                request_id = %request_id,
                method = %method,
                "ui plugin api effect refused: per-update budget exhausted"
            );
        }
        queue.push(PendingApiEffect {
            plugin_id: plugin_id.to_string(),
            module_id: module_id.to_string(),
            request_id,
            method,
            params,
            budget_exhausted,
        });
    }
}

/// Spec §6 update merge semantics. v2 modules (and any v3 update with an
/// explicit `"full"` scope) fully replace the cached layer set; v3 modules'
/// default `"partial"` scope merges by surface: every surface named in
/// `incoming` replaces all of its old z-levels at once (there is no per-layer
/// partial update — `UiLayer` has no id), leaving unnamed surfaces untouched. An
/// incoming update naming no surfaces is therefore a pure no-op.
fn merge_layers(
    old: Option<&[UiLayer]>,
    incoming: Vec<UiLayer>,
    negotiated_version: u32,
    scope: UiUpdateScope,
) -> Vec<UiLayer> {
    let effective_scope = if negotiated_version == 3 {
        scope
    } else {
        UiUpdateScope::Full
    };
    match effective_scope {
        UiUpdateScope::Full => incoming,
        UiUpdateScope::Partial => {
            let touched_surfaces: std::collections::HashSet<UiSurface> =
                incoming.iter().map(|layer| layer.surface).collect();
            let mut merged: Vec<UiLayer> = old
                .unwrap_or(&[])
                .iter()
                .filter(|layer| !touched_surfaces.contains(&layer.surface))
                .cloned()
                .collect();
            merged.extend(incoming);
            merged
        }
    }
}

fn module_key(module: &RegisteredUiModule) -> (String, String) {
    (module.plugin_id.clone(), module.module_id.clone())
}

fn module_accepts_event(module: &RegisteredUiModule, event: &UiEvent) -> bool {
    match event {
        UiEvent::Mouse(_) => module.requests(crate::api::schema::PluginUiCapability::MouseAll),
        UiEvent::Key(_) => module.requests(crate::api::schema::PluginUiCapability::KeyboardInput),
        UiEvent::FocusChanged(_) => {
            module.requests(crate::api::schema::PluginUiCapability::FocusedSession)
        }
        UiEvent::ThemeChanged => module.requests(crate::api::schema::PluginUiCapability::ThemeRead),
        UiEvent::Frame(_) => {
            module.requests(crate::api::schema::PluginUiCapability::LayoutRead)
                || module.requests(crate::api::schema::PluginUiCapability::FocusedSession)
        }
        UiEvent::Tick | UiEvent::ReloadRequested => true,
        // Spec §2 enforcement point: v3 event kinds are delivered only to
        // negotiated-v3 modules. No new capability gate — §4 hits and §5
        // requests already imply wanting the corresponding event.
        UiEvent::Action { .. } | UiEvent::ApiResult { .. } => module.negotiated_version == 3,
    }
}

fn filter_effects_for_module(
    module: &RegisteredUiModule,
    effects: &[HostEffect],
) -> Vec<HostEffect> {
    effects
        .iter()
        .filter(|effect| match effect {
            HostEffect::CopyText(_) => {
                module.requests(crate::api::schema::PluginUiCapability::ClipboardWrite)
            }
            HostEffect::SendPaneKeys { .. } => {
                module.requests(crate::api::schema::PluginUiCapability::PaneInput)
            }
            HostEffect::SpawnHeadless { .. } => {
                module.requests(crate::api::schema::PluginUiCapability::HeadlessActions)
            }
            HostEffect::ShowToast { .. } => {
                module.requests(crate::api::schema::PluginUiCapability::Toasts)
            }
            HostEffect::OpenPluginPane { .. } => {
                module.requests(crate::api::schema::PluginUiCapability::OpenPluginPanes)
            }
            HostEffect::InvokePluginAction { .. } => {
                module.requests(crate::api::schema::PluginUiCapability::PluginActions)
            }
            // Api effects are never routed through this generic capability
            // filter: `dispatch_event_for_module` extracts and queues them
            // separately (via `queue_api_effects`) before this function ever
            // sees `other_effects`, so this arm exists only for exhaustiveness.
            HostEffect::Api { .. } => false,
        })
        .cloned()
        .collect()
}

fn dispatch_event_update(
    module: &RegisteredUiModule,
    event: &UiEvent,
) -> Result<Option<UiUpdate>, String> {
    let engine = wasmtime::Engine::default();
    let wasm = std::fs::read(&module.module_path).map_err(|err| {
        format!(
            "failed to read wasm ui module {}: {err}",
            module.module_path.display()
        )
    })?;
    let wasm_module = wasmtime::Module::new(&engine, &wasm).map_err(|err| {
        format!(
            "failed to compile wasm ui module {}: {err}",
            module.module_path.display()
        )
    })?;
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = wasmtime::Instance::new(&mut store, &wasm_module, &[]).map_err(|err| {
        format!(
            "failed to instantiate wasm ui module {}: {err}",
            module.module_path.display()
        )
    })?;
    let Some(memory) = instance.get_memory(&mut store, "memory") else {
        return Ok(None);
    };
    // Compare against the module's already-negotiated version, not the host's
    // max-supported `API_VERSION`: a v2 module (negotiated_version == 2) must
    // keep getting dispatched byte-identically to before v3 existed (§2). If the
    // freshly-read export no longer matches what was negotiated at load/reload
    // time, something changed underneath without a reload — bail out silently,
    // same fail-safe behavior as today.
    if call_i32_export(&instance, &mut store, "herdr_ui_api_version")? as u32
        != module.negotiated_version
    {
        return Ok(None);
    }
    let alloc = match instance.get_typed_func::<i32, i32>(&mut store, "herdr_ui_alloc") {
        Ok(func) => func,
        Err(_) => return Ok(None),
    };
    let on_event = match instance.get_typed_func::<(i32, i32), i32>(&mut store, "herdr_ui_on_event")
    {
        Ok(func) => func,
        Err(_) => return Ok(None),
    };
    let event_len = match instance.get_typed_func::<(), i32>(&mut store, "herdr_ui_on_event_len") {
        Ok(func) => func,
        Err(_) => return Ok(None),
    };
    let bytes =
        serde_json::to_vec(event).map_err(|err| format!("failed to serialize ui event: {err}"))?;
    let ptr = alloc
        .call(&mut store, bytes.len() as i32)
        .map_err(|err| format!("ui wasm alloc failed: {err}"))? as usize;
    let data = memory.data_mut(&mut store);
    let end = ptr
        .checked_add(bytes.len())
        .ok_or_else(|| "ui wasm event buffer overflow".to_string())?;
    if end > data.len() {
        return Err("ui wasm alloc returned out-of-bounds pointer".to_string());
    }
    data[ptr..end].copy_from_slice(&bytes);
    let result_ptr = on_event
        .call(&mut store, (ptr as i32, bytes.len() as i32))
        .map_err(|err| format!("ui wasm on_event failed: {err}"))? as usize;
    let result_len = event_len
        .call(&mut store, ())
        .map_err(|err| format!("ui wasm on_event_len failed: {err}"))?
        as usize;
    let data = memory.data(&store);
    let end = result_ptr
        .checked_add(result_len)
        .ok_or_else(|| "ui wasm result overflow".to_string())?;
    if end > data.len() {
        return Err("ui wasm on_event returned out-of-bounds slice".to_string());
    }
    let update = serde_json::from_slice::<UiUpdate>(&data[result_ptr..end])
        .map_err(|err| format!("failed to parse ui wasm event update: {err}"))?;
    Ok(Some(update))
}

fn map_crossterm_key_event(event: crossterm::event::KeyEvent) -> Option<UiKeyEvent> {
    use crossterm::event::KeyCode;

    let key = match event.code {
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "page_up".to_string(),
        KeyCode::PageDown => "page_down".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "back_tab".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Esc => "escape".to_string(),
        KeyCode::F(n) => format!("f{n}"),
        KeyCode::Char(ch) => ch.to_string(),
        _ => return None,
    };

    Some(UiKeyEvent {
        key,
        shift: event
            .modifiers
            .contains(crossterm::event::KeyModifiers::SHIFT),
        ctrl: event
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL),
        alt: event
            .modifiers
            .contains(crossterm::event::KeyModifiers::ALT),
        meta: event
            .modifiers
            .contains(crossterm::event::KeyModifiers::SUPER),
    })
}

fn map_crossterm_mouse_kind(kind: crossterm::event::MouseEventKind) -> Option<MouseEventKind> {
    use crossterm::event::{MouseButton, MouseEventKind as K};
    Some(match kind {
        K::Moved => MouseEventKind::Move,
        K::Down(MouseButton::Left) => MouseEventKind::LeftDown,
        K::Up(MouseButton::Left) => MouseEventKind::LeftUp,
        K::Down(MouseButton::Right) => MouseEventKind::RightDown,
        K::Up(MouseButton::Right) => MouseEventKind::RightUp,
        K::Down(MouseButton::Middle) => MouseEventKind::MiddleDown,
        K::Up(MouseButton::Middle) => MouseEventKind::MiddleUp,
        K::Drag(_) => MouseEventKind::Drag,
        K::ScrollUp => MouseEventKind::ScrollUp,
        K::ScrollDown => MouseEventKind::ScrollDown,
        _ => return None,
    })
}

fn context_menu_items_from_update(update: &UiUpdate) -> Vec<UiActionRef> {
    let mut out = Vec::new();
    for layer in &update.layers {
        if layer.surface != UiSurface::ContextMenu {
            continue;
        }
        for node in &layer.nodes {
            if let UiNode::Menu { items, .. } | UiNode::Tabs { items, .. } = node {
                out.extend(items.iter().cloned());
            }
        }
    }
    out
}

/// Loads a module's static update and negotiates its API version (§2). Returns
/// the negotiated version alongside the update so the caller can store it on
/// `RegisteredUiModule.negotiated_version`. The manifest is never re-read here
/// (only the `.wasm` mtime is polled), so a module whose fresh export no longer
/// equals its manifest-declared `api_version` fails to load and the caller keeps
/// last-good layers, exactly as it did pre-v3.
fn load_static_update(module: &RegisteredUiModule) -> Result<(UiUpdate, u32), String> {
    let engine = wasmtime::Engine::default();
    let wasm = std::fs::read(&module.module_path).map_err(|err| {
        format!(
            "failed to read wasm ui module {}: {err}",
            module.module_path.display()
        )
    })?;
    let wasm_module = wasmtime::Module::new(&engine, &wasm).map_err(|err| {
        format!(
            "failed to compile wasm ui module {}: {err}",
            module.module_path.display()
        )
    })?;
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = wasmtime::Instance::new(&mut store, &wasm_module, &[]).map_err(|err| {
        format!(
            "failed to instantiate wasm ui module {}: {err}",
            module.module_path.display()
        )
    })?;
    let memory = instance.get_memory(&mut store, "memory").ok_or_else(|| {
        format!(
            "wasm ui module {} exports no memory",
            module.module_path.display()
        )
    })?;
    let api_version = call_i32_export(&instance, &mut store, "herdr_ui_api_version")? as u32;
    if api_version != module.api_version {
        return Err(format!(
            "wasm ui module {} api version mismatch: module exports {}, manifest says {}",
            module.module_path.display(),
            api_version,
            module.api_version,
        ));
    }
    if !(api_version == 2 || api_version == 3) {
        return Err(format!(
            "wasm ui module {} exports unsupported api version {} (host supports 2 or 3)",
            module.module_path.display(),
            api_version,
        ));
    }
    let ptr = call_i32_export(&instance, &mut store, "herdr_ui_static_update_ptr")? as usize;
    let len = call_i32_export(&instance, &mut store, "herdr_ui_static_update_len")? as usize;
    let data = memory.data(&store);
    let end = ptr
        .checked_add(len)
        .ok_or_else(|| "wasm ui module update overflow".to_string())?;
    if end > data.len() {
        return Err(format!(
            "wasm ui module {} returned out-of-bounds static update slice",
            module.module_path.display()
        ));
    }
    let update = serde_json::from_slice::<UiUpdate>(&data[ptr..end]).map_err(|err| {
        format!(
            "failed to parse wasm ui module update {}: {err}",
            module.module_path.display()
        )
    })?;
    Ok((update, api_version))
}

fn call_i32_export(
    instance: &wasmtime::Instance,
    store: &mut wasmtime::Store<()>,
    name: &str,
) -> Result<i32, String> {
    let func = instance
        .get_typed_func::<(), i32>(&mut *store, name)
        .map_err(|_| format!("wasm ui module missing export {name}"))?;
    func.call(&mut *store, ())
        .map_err(|err| format!("wasm ui module export {name} failed: {err}"))
}

fn map_surface(surface: PluginUiSurface) -> UiSurface {
    match surface {
        PluginUiSurface::Header => UiSurface::Header,
        PluginUiSurface::Footer => UiSurface::Footer,
        PluginUiSurface::TabBar => UiSurface::TabBar,
        PluginUiSurface::Sidebar => UiSurface::Sidebar,
        PluginUiSurface::RightPanel => UiSurface::RightPanel,
        PluginUiSurface::Overlay => UiSurface::Overlay,
        PluginUiSurface::ContextMenu => UiSurface::ContextMenu,
        PluginUiSurface::UsageStrip => UiSurface::UsageStrip,
    }
}

fn map_capability(capability: crate::api::schema::PluginUiCapability) -> UiCapability {
    match capability {
        crate::api::schema::PluginUiCapability::FocusedSession => UiCapability::FocusedSession,
        crate::api::schema::PluginUiCapability::ThemeRead => UiCapability::ThemeRead,
        crate::api::schema::PluginUiCapability::LayoutRead => UiCapability::LayoutRead,
        crate::api::schema::PluginUiCapability::MouseAll => UiCapability::MouseAll,
        crate::api::schema::PluginUiCapability::KeyboardInput => UiCapability::KeyboardInput,
        crate::api::schema::PluginUiCapability::ContextMenuAugment => {
            UiCapability::ContextMenuAugment
        }
        crate::api::schema::PluginUiCapability::ClipboardWrite => UiCapability::ClipboardWrite,
        crate::api::schema::PluginUiCapability::PaneInput => UiCapability::PaneInput,
        crate::api::schema::PluginUiCapability::HeadlessActions => UiCapability::HeadlessActions,
        crate::api::schema::PluginUiCapability::Toasts => UiCapability::Toasts,
        crate::api::schema::PluginUiCapability::OpenPluginPanes => UiCapability::OpenPluginPanes,
        crate::api::schema::PluginUiCapability::PluginActions => UiCapability::PluginActions,
        crate::api::schema::PluginUiCapability::ApiRead => UiCapability::ApiRead,
        crate::api::schema::PluginUiCapability::ApiWrite => UiCapability::ApiWrite,
        crate::api::schema::PluginUiCapability::ApiInput => UiCapability::ApiInput,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{
        PluginHotReloadMode, PluginManifestUiModule, PluginUiCapability, PluginUiModuleKind,
        PluginUiSurface,
    };

    fn write_static_wasm(path: &std::path::Path, json: &str) {
        let data = json
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            "(module (memory (export \"memory\") 1) (data (i32.const 0) \"{data}\") (func (export \"herdr_ui_api_version\") (result i32) i32.const 2) (func (export \"herdr_ui_static_update_ptr\") (result i32) i32.const 0) (func (export \"herdr_ui_static_update_len\") (result i32) i32.const {}) )",
            json.len()
        );
        let wasm = wat::parse_str(&wat).unwrap();
        std::fs::write(path, wasm).unwrap();
    }

    fn sample_plugin() -> InstalledPluginInfo {
        InstalledPluginInfo {
            plugin_id: "example.goal-ui".into(),
            name: "Goal UI".into(),
            version: "0.1.0".into(),
            min_herdr_version: crate::build_info::BASE_VERSION.into(),
            description: None,
            manifest_path: "/tmp/example.goal-ui/herdr-plugin.toml".into(),
            plugin_root: "/tmp/example.goal-ui".into(),
            enabled: true,
            platforms: None,
            build: vec![],
            actions: vec![],
            events: vec![],
            panes: vec![],
            ui_modules: vec![PluginManifestUiModule {
                id: "goal-ui".into(),
                title: "Goal UI".into(),
                description: None,
                platforms: None,
                kind: PluginUiModuleKind::Wasm,
                api_version: 2,
                module: "dist/goal-ui.wasm".into(),
                hot_reload: PluginHotReloadMode::On,
                surfaces: vec![PluginUiSurface::Header, PluginUiSurface::ContextMenu],
                capabilities: vec![
                    PluginUiCapability::FocusedSession,
                    PluginUiCapability::MouseAll,
                ],
            }],
            link_handlers: vec![],
            source: Default::default(),
            warnings: vec![],
        }
    }

    #[test]
    fn manager_indexes_modules_by_surface() {
        let plugin = sample_plugin();
        let manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]));
        assert_eq!(manager.all().len(), 1);
        assert_eq!(manager.for_surface(PluginUiSurface::Header).len(), 1);
        assert_eq!(manager.for_surface(PluginUiSurface::Footer).len(), 0);
        let modules = manager.for_surface(PluginUiSurface::ContextMenu);
        assert_eq!(modules.len(), 1);
        let manifest = modules[0].manifest_ref();
        assert_eq!(manifest.api_version, 2);
    }

    #[test]
    fn manager_loads_static_context_menu_items_from_wasm_module() {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let json = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"WASM header","role":"accent"}]}]},{"surface":"context_menu","nodes":[{"kind":"menu","title":"Tab actions","items":[{"id":"plugin:example.goal-ui:fork","label":"Fork"}],"selected":0}]}]}"#;
        write_static_wasm(&root.join("dist/goal-ui.wasm"), json);

        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        let manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]));
        let items = manager.context_menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "Fork");
        assert_eq!(items[0].id, "plugin:example.goal-ui:fork");
        let layers = manager.surface_layers();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].layer.surface, UiSurface::Header);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dispatch_mouse_event_updates_loaded_wasm_layers() {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-event-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let module_path = root.join("dist/goal-ui.wasm");
        let initial = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Initial","role":"accent"}]}]}]}"#;
        let updated = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"After mouse","role":"accent"}]}]},{"surface":"context_menu","nodes":[{"kind":"menu","title":"Tab actions","items":[{"id":"plugin:example.goal-ui:fork","label":"Fork mouse"}],"selected":0}]}]}"#;
        let initial_data = initial
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let updated_data = updated
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            "(module
                (memory (export \"memory\") 1)
                (global $next_len (mut i32) (i32.const {updated_len}))
                (data (i32.const 0) \"{initial_data}\")
                (data (i32.const 2048) \"{updated_data}\")
                (func (export \"herdr_ui_api_version\") (result i32) i32.const 2)
                (func (export \"herdr_ui_static_update_ptr\") (result i32) i32.const 0)
                (func (export \"herdr_ui_static_update_len\") (result i32) i32.const {initial_len})
                (func (export \"herdr_ui_alloc\") (param i32) (result i32) i32.const 1024)
                (func (export \"herdr_ui_on_event\") (param i32 i32) (result i32) i32.const 2048)
                (func (export \"herdr_ui_on_event_len\") (result i32) global.get $next_len)
            )",
            initial_len = initial.len(),
            updated_len = updated.len(),
            initial_data = initial_data,
            updated_data = updated_data,
        );
        let wasm = wat::parse_str(&wat).unwrap();
        std::fs::write(&module_path, wasm).unwrap();

        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        let mut manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]));
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node");
        };
        assert_eq!(spans[0].text, "Initial");

        let changed = manager.dispatch_crossterm_mouse_event(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 1,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });
        assert!(changed);
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node");
        };
        assert_eq!(spans[0].text, "After mouse");
        assert_eq!(manager.context_menu_items()[0].label, "Fork mouse");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn key_event_is_gated_by_keyboard_capability() {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-key-gate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let module_path = root.join("dist/goal-ui.wasm");
        let initial = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Initial","role":"accent"}]}]}]}"#;
        let updated = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Should not apply","role":"accent"}]}]}],"effects":[{"kind":"show_toast","message":"blocked"}]}"#;
        let initial_data = initial
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let updated_data = updated
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            "(module
                (memory (export \"memory\") 1)
                (global $next_len (mut i32) (i32.const {updated_len}))
                (data (i32.const 0) \"{initial_data}\")
                (data (i32.const 2048) \"{updated_data}\")
                (func (export \"herdr_ui_api_version\") (result i32) i32.const 2)
                (func (export \"herdr_ui_static_update_ptr\") (result i32) i32.const 0)
                (func (export \"herdr_ui_static_update_len\") (result i32) i32.const {initial_len})
                (func (export \"herdr_ui_alloc\") (param i32) (result i32) i32.const 1024)
                (func (export \"herdr_ui_on_event\") (param i32 i32) (result i32) i32.const 2048)
                (func (export \"herdr_ui_on_event_len\") (result i32) global.get $next_len)
            )",
            initial_len = initial.len(),
            updated_len = updated.len(),
            initial_data = initial_data,
            updated_data = updated_data,
        );
        let wasm = wat::parse_str(&wat).unwrap();
        std::fs::write(&module_path, wasm).unwrap();

        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        let mut manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]));

        let changed = manager.dispatch_crossterm_key_event(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(!changed);
        assert!(manager.take_pending_effects().is_empty());
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node");
        };
        assert_eq!(spans[0].text, "Initial");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dispatch_frame_event_updates_loaded_wasm_layers() {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-frame-event-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let module_path = root.join("dist/goal-ui.wasm");
        let initial = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Initial","role":"accent"}]}]}]}"#;
        let updated = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"After frame","role":"accent"}]}]}]}"#;
        let initial_data = initial
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let updated_data = updated
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            "(module
                (memory (export \"memory\") 1)
                (global $next_len (mut i32) (i32.const {updated_len}))
                (data (i32.const 0) \"{initial_data}\")
                (data (i32.const 2048) \"{updated_data}\")
                (func (export \"herdr_ui_api_version\") (result i32) i32.const 2)
                (func (export \"herdr_ui_static_update_ptr\") (result i32) i32.const 0)
                (func (export \"herdr_ui_static_update_len\") (result i32) i32.const {initial_len})
                (func (export \"herdr_ui_alloc\") (param i32) (result i32) i32.const 1024)
                (func (export \"herdr_ui_on_event\") (param i32 i32) (result i32) i32.const 2048)
                (func (export \"herdr_ui_on_event_len\") (result i32) global.get $next_len)
            )",
            initial_len = initial.len(),
            updated_len = updated.len(),
            initial_data = initial_data,
            updated_data = updated_data,
        );
        let wasm = wat::parse_str(&wat).unwrap();
        std::fs::write(&module_path, wasm).unwrap();

        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        plugin.ui_modules[0]
            .capabilities
            .push(PluginUiCapability::LayoutRead);
        let mut manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]));

        let changed = manager.dispatch_frame(
            "example.goal-ui",
            "goal-ui",
            UiSurface::Header,
            UiLayoutRect {
                x: 0,
                y: 0,
                width: 80,
                height: 1,
            },
            false,
            FocusInput::default(),
            HashMap::new(),
        );
        assert!(changed);
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node");
        };
        assert_eq!(spans[0].text, "After frame");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invoke_plugin_action_effect_is_gated_by_capability() {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-action-effect-gate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let module_path = root.join("dist/goal-ui.wasm");
        let initial = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Initial","role":"accent"}]}]}]}"#;
        let updated = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"After key","role":"accent"}]}]}],"effects":[{"kind":"invoke_plugin_action","plugin_id":"example.goal-ui","action_id":"fork"}]}"#;
        let initial_data = initial
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let updated_data = updated
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            "(module
                (memory (export \"memory\") 1)
                (global $next_len (mut i32) (i32.const {updated_len}))
                (data (i32.const 0) \"{initial_data}\")
                (data (i32.const 2048) \"{updated_data}\")
                (func (export \"herdr_ui_api_version\") (result i32) i32.const 2)
                (func (export \"herdr_ui_static_update_ptr\") (result i32) i32.const 0)
                (func (export \"herdr_ui_static_update_len\") (result i32) i32.const {initial_len})
                (func (export \"herdr_ui_alloc\") (param i32) (result i32) i32.const 1024)
                (func (export \"herdr_ui_on_event\") (param i32 i32) (result i32) i32.const 2048)
                (func (export \"herdr_ui_on_event_len\") (result i32) global.get $next_len)
            )",
            initial_len = initial.len(),
            updated_len = updated.len(),
            initial_data = initial_data,
            updated_data = updated_data,
        );
        let wasm = wat::parse_str(&wat).unwrap();
        std::fs::write(&module_path, wasm).unwrap();

        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        plugin.ui_modules[0]
            .capabilities
            .push(PluginUiCapability::KeyboardInput);
        let mut manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]));

        let changed = manager.dispatch_crossterm_key_event(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(changed);
        assert!(manager.take_pending_effects().is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dispatch_key_event_queues_host_effects() {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-key-event-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let module_path = root.join("dist/goal-ui.wasm");
        let initial = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Initial","role":"accent"}]}]}]}"#;
        let updated = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"After key","role":"accent"}]}]}],"effects":[{"kind":"show_toast","message":"toast from wasm"},{"kind":"invoke_plugin_action","plugin_id":"example.goal-ui","action_id":"fork"}]}"#;
        let initial_data = initial
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let updated_data = updated
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            "(module
                (memory (export \"memory\") 1)
                (global $next_len (mut i32) (i32.const {updated_len}))
                (data (i32.const 0) \"{initial_data}\")
                (data (i32.const 2048) \"{updated_data}\")
                (func (export \"herdr_ui_api_version\") (result i32) i32.const 2)
                (func (export \"herdr_ui_static_update_ptr\") (result i32) i32.const 0)
                (func (export \"herdr_ui_static_update_len\") (result i32) i32.const {initial_len})
                (func (export \"herdr_ui_alloc\") (param i32) (result i32) i32.const 1024)
                (func (export \"herdr_ui_on_event\") (param i32 i32) (result i32) i32.const 2048)
                (func (export \"herdr_ui_on_event_len\") (result i32) global.get $next_len)
            )",
            initial_len = initial.len(),
            updated_len = updated.len(),
            initial_data = initial_data,
            updated_data = updated_data,
        );
        let wasm = wat::parse_str(&wat).unwrap();
        std::fs::write(&module_path, wasm).unwrap();

        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        plugin.ui_modules[0]
            .capabilities
            .push(PluginUiCapability::KeyboardInput);
        plugin.ui_modules[0]
            .capabilities
            .push(PluginUiCapability::Toasts);
        plugin.ui_modules[0]
            .capabilities
            .push(PluginUiCapability::PluginActions);
        let mut manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]));

        let changed = manager.dispatch_crossterm_key_event(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(changed);
        let effects = manager.take_pending_effects();
        assert_eq!(effects.len(), 2);
        assert!(
            matches!(&effects[0], HostEffect::ShowToast { message } if message == "toast from wasm")
        );
        assert!(
            matches!(&effects[1], HostEffect::InvokePluginAction { plugin_id, action_id, payload_json: None } if plugin_id == "example.goal-ui" && action_id == "fork")
        );
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node");
        };
        assert_eq!(spans[0].text, "After key");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn detect_changed_modules_reloads_static_update_content() {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-reload-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let module_path = root.join("dist/goal-ui.wasm");
        let first = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Version A","role":"accent"}]}]},{"surface":"context_menu","nodes":[{"kind":"menu","title":"Tab actions","items":[{"id":"plugin:example.goal-ui:fork","label":"Fork A"}],"selected":0}]}]}"#;
        write_static_wasm(&module_path, first);

        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        let mut manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]));

        assert_eq!(manager.context_menu_items()[0].label, "Fork A");
        let layers = manager.surface_layers();
        assert_eq!(layers.len(), 1);
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node");
        };
        assert_eq!(spans[0].text, "Version A");

        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Version B","role":"accent"}]}]},{"surface":"context_menu","nodes":[{"kind":"menu","title":"Tab actions","items":[{"id":"plugin:example.goal-ui:fork","label":"Fork B"}],"selected":0}]}]}"#;
        write_static_wasm(&module_path, second);

        let changed = manager.detect_changed_modules();
        assert_eq!(changed.len(), 1);
        assert_eq!(manager.context_menu_items()[0].label, "Fork B");
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node");
        };
        assert_eq!(spans[0].text, "Version B");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn detect_changed_modules_keeps_last_good_update_on_reload_failure() {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-reload-failure-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let module_path = root.join("dist/goal-ui.wasm");
        let first = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Version A","role":"accent"}]}]}]}"#;
        write_static_wasm(&module_path, first);

        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        let mut manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]));

        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&module_path, b"not valid wasm").unwrap();

        let changed = manager.detect_changed_modules();
        assert!(changed.is_empty());
        // The failed reload queues a notification even though no module changed
        // and dispatch may also fail; the scheduled tick relies on this to flush
        // the "reload failed" toast instead of swallowing it.
        assert!(manager.has_pending_updates());
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node");
        };
        assert_eq!(spans[0].text, "Version A");
        let notifications = manager.take_pending_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("reload failed"));
        assert!(!manager.has_pending_updates());

        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"Version B","role":"accent"}]}]}]}"#;
        write_static_wasm(&module_path, second);

        let changed = manager.detect_changed_modules();
        assert_eq!(changed.len(), 1);
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node");
        };
        assert_eq!(spans[0].text, "Version B");
        let notifications = manager.take_pending_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("reloaded"));

        let _ = std::fs::remove_dir_all(root);
    }
}
