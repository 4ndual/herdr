use crate::api::schema::{InstalledPluginInfo, PluginHotReloadMode, PluginUiSurface};
use herdr_ui_plugin_api::FocusInput;
use herdr_ui_plugin_api::{
    HostEffect, HostSnapshot, KeyEvent as UiKeyEvent, MouseEvent, MouseEventKind, UiActionKind,
    UiActionRef, UiCapability, UiEvent, UiFrameContext, UiLayer, UiLayoutRect, UiNode,
    UiPluginManifestRef, UiSurface, UiUpdate, UiUpdateScope, API_VERSION,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

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
    /// Manifest `ui_input_file` (spec §7.1): a state-dir filename whose contents
    /// the host surfaces as `HERDR_PLUGIN_UI_INPUT_JSON` each frame. `None` when
    /// the module declares no input file.
    pub ui_input_file: Option<String>,
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

/// How often the shared engine epoch is bumped, and how many bumps a single
/// guest call is granted before it is trapped. 5 * 100ms bounds a runaway guest
/// (`loop {}`) to well under a second without hanging the render thread.
const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(100);
const EPOCH_DEADLINE_TICKS: u64 = 5;

pub(crate) struct UiPluginManager {
    modules: Vec<RegisteredUiModule>,
    by_surface: HashMap<PluginUiSurface, Vec<usize>>,
    mtimes: HashMap<PathBuf, SystemTime>,
    // fork(wave-2): per-`herdr-plugin.toml` mtime, so a manifest edit hot-applies
    // its module metadata (surfaces/capabilities/ui_input_file/collector argv)
    // without a server restart, mirroring the `.wasm` mtime watch in `mtimes`.
    manifest_mtimes: HashMap<PathBuf, SystemTime>,
    loaded_updates: HashMap<(String, String), UiUpdate>,
    last_errors: HashMap<(String, String), String>,
    pending_effects: Vec<HostEffect>,
    pending_notifications: Vec<String>,
    /// Shared wasm runtime holding the live per-module instances. `None` only if
    /// the engine failed to initialize, in which case plugins are inert.
    runtime: Option<PluginRuntime>,
    /// API v3 (§5): effects pending capability-policy check + execution by
    /// `App::apply_ui_plugin_effects`. Includes budget-refused entries so every
    /// request still resolves to exactly one result (§5.4).
    pending_api_effects: Vec<PendingApiEffect>,
}

/// Background thread that increments the shared engine epoch so a guest call
/// that never yields (e.g. `loop {}`) hits its epoch deadline and traps instead
/// of hanging the render thread. Stops and joins on drop.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EpochTicker {
    fn spawn(engine: &Engine) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let engine = engine.clone();
        let stop_flag = Arc::clone(&stop);
        let handle = match std::thread::Builder::new()
            .name("herdr-ui-plugin-epoch".to_string())
            .spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    std::thread::sleep(EPOCH_TICK_INTERVAL);
                    engine.increment_epoch();
                }
            }) {
            Ok(handle) => Some(handle),
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "failed to spawn ui plugin epoch ticker; guest calls will not be time-limited"
                );
                None
            }
        };
        Self { stop, handle }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One epoch-interruptible `Engine` shared by every UI plugin module, plus a
/// cache of live instances keyed by `(plugin_id, module_id)`. Events reuse the
/// cached instance so guest linear memory (statics + heap) persists across
/// events and no recompile/re-instantiate happens per event.
struct PluginRuntime {
    engine: Engine,
    // Held only for its `Drop` (stops the epoch thread); never read directly.
    _ticker: EpochTicker,
    instances: HashMap<(String, String), ModuleInstance>,
}

impl PluginRuntime {
    fn new() -> Result<Self, String> {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        // fork(wave-1): wasmtime's default disk cache. Every server boot (and
        // every test-spawned server) compiles the same bundled dashboard wasm;
        // the cross-process cache turns repeat compiles into a deserialize.
        // With two bundled modules, N concurrent test servers otherwise contend
        // on 2N cranelift compiles and starve past request timeouts. Cache
        // config failure is non-fatal — compiles just stay uncached.
        if let Err(err) = config.cache_config_load_default() {
            tracing::warn!(error = %err, "wasm ui plugin compilation cache disabled");
        }
        let engine =
            Engine::new(&config).map_err(|err| format!("failed to build wasm ui engine: {err}"))?;
        let ticker = EpochTicker::spawn(&engine);
        Ok(Self {
            engine,
            _ticker: ticker,
            instances: HashMap::new(),
        })
    }

    /// Returns the cached instance for `module`, building it (fresh store, fresh
    /// linear memory) when it is not currently cached — first use, or after a
    /// trap / hot-reload dropped it. The returned bool is `true` when this call
    /// built a fresh instance, so the caller can clear the merge cache (§6:
    /// trap-recovery / hot-reload full-replace) before applying its next update.
    fn instance_for(
        &mut self,
        module: &RegisteredUiModule,
    ) -> Result<(&mut ModuleInstance, bool), String> {
        let key = module_key(module);
        let rebuilt = !self.instances.contains_key(&key);
        if rebuilt {
            let instance = ModuleInstance::build(&self.engine, module)?;
            self.instances.insert(key.clone(), instance);
        }
        let instance = self
            .instances
            .get_mut(&key)
            .ok_or_else(|| "ui plugin instance missing after build".to_string())?;
        Ok((instance, rebuilt))
    }

    /// Dispatches `event` to the module's live instance, reporting whether the
    /// instance had to be (re)built for this call (§3 trap-recovery / first use)
    /// so the caller can honor the §6 cache-clear rule.
    fn dispatch(
        &mut self,
        module: &RegisteredUiModule,
        event: &UiEvent,
    ) -> Result<(Option<UiUpdate>, bool), String> {
        let (instance, rebuilt) = self.instance_for(module)?;
        let update = instance.dispatch(event)?;
        Ok((update, rebuilt))
    }

    /// Reads the module's static update and reports the version negotiated with
    /// the freshly-read guest export (§2), so the caller can store it on
    /// `RegisteredUiModule.negotiated_version`.
    fn static_update(&mut self, module: &RegisteredUiModule) -> Result<(UiUpdate, u32), String> {
        let (instance, _rebuilt) = self.instance_for(module)?;
        let update = instance.static_update(module)?;
        Ok((update, instance.negotiated_version))
    }

    /// Drop the cached instance so the next call rebuilds from disk. Used for
    /// crash isolation (a trapped guest) and for hot-reload, where the rebuild
    /// also discards the guest's persisted state.
    fn invalidate(&mut self, module: &RegisteredUiModule) {
        self.instances.remove(&module_key(module));
    }

    // fork(wave-2): drop a cached instance by key alone, for the lifecycle re-sync
    // path where a removed module is no longer held as a `RegisteredUiModule`.
    fn invalidate_key(&mut self, key: &(String, String)) {
        self.instances.remove(key);
    }

    fn reload_static(&mut self, module: &RegisteredUiModule) -> Result<(UiUpdate, u32), String> {
        self.invalidate(module);
        self.static_update(module)
    }
}

/// A live wasm instance for one UI plugin module. Holds its `Store` so guest
/// linear memory persists across events. Every guest entry point re-arms the
/// epoch deadline so a runaway call traps instead of blocking the caller.
struct ModuleInstance {
    store: Store<()>,
    memory: Memory,
    alloc: Option<TypedFunc<i32, i32>>,
    on_event: Option<TypedFunc<(i32, i32), i32>>,
    on_event_len: Option<TypedFunc<(), i32>>,
    static_ptr: TypedFunc<(), i32>,
    static_len: TypedFunc<(), i32>,
    /// The version negotiated with the guest's `herdr_ui_api_version` export at
    /// build time (2 or 3, §2). Surfaced to the manager so it can be stored on
    /// the module and gate v3-only event delivery in `module_accepts_event`.
    negotiated_version: u32,
}

impl ModuleInstance {
    fn build(engine: &Engine, module: &RegisteredUiModule) -> Result<Self, String> {
        let path = module.module_path.display();
        let wasm = std::fs::read(&module.module_path)
            .map_err(|err| format!("failed to read wasm ui module {path}: {err}"))?;
        let compiled = Module::new(engine, &wasm)
            .map_err(|err| format!("failed to compile wasm ui module {path}: {err}"))?;
        let mut store = Store::new(engine, ());
        // Arm a finite deadline so instantiation/start code cannot hang either;
        // every subsequent guest call re-arms before running.
        store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let instance = Instance::new(&mut store, &compiled, &[])
            .map_err(|err| format!("failed to instantiate wasm ui module {path}: {err}"))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| format!("wasm ui module {path} exports no memory"))?;
        let api_fn = instance
            .get_typed_func::<(), i32>(&mut store, "herdr_ui_api_version")
            .map_err(|_| format!("wasm ui module {path} missing export herdr_ui_api_version"))?;
        store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let api_version = api_fn.call(&mut store, ()).map_err(|err| {
            format!("wasm ui module {path} export herdr_ui_api_version failed: {err}")
        })? as u32;
        // §2 version negotiation (replaces the old exact `== API_VERSION` check):
        // the guest export must equal its manifest `api_version` and be one of
        // the host-supported versions {2, 3}. The negotiated value is stored per
        // instance so v3-only events are gated on it downstream.
        if api_version != module.api_version || !matches!(api_version, 2 | 3) {
            return Err(format!(
                "wasm ui module {path} api version mismatch: module exports {api_version}, manifest says {}, host supports 2 or {API_VERSION}",
                module.api_version
            ));
        }
        let negotiated_version = api_version;
        let static_ptr = instance
            .get_typed_func::<(), i32>(&mut store, "herdr_ui_static_update_ptr")
            .map_err(|_| {
                format!("wasm ui module {path} missing export herdr_ui_static_update_ptr")
            })?;
        let static_len = instance
            .get_typed_func::<(), i32>(&mut store, "herdr_ui_static_update_len")
            .map_err(|_| {
                format!("wasm ui module {path} missing export herdr_ui_static_update_len")
            })?;
        // Event entry points are optional: a static-only module omits them and
        // simply produces no update on events.
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "herdr_ui_alloc")
            .ok();
        let on_event = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "herdr_ui_on_event")
            .ok();
        let on_event_len = instance
            .get_typed_func::<(), i32>(&mut store, "herdr_ui_on_event_len")
            .ok();
        Ok(Self {
            store,
            memory,
            alloc,
            on_event,
            on_event_len,
            static_ptr,
            static_len,
            negotiated_version,
        })
    }

    fn static_update(&mut self, module: &RegisteredUiModule) -> Result<UiUpdate, String> {
        let path = module.module_path.display();
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let ptr = self.static_ptr.call(&mut self.store, ()).map_err(|err| {
            format!("wasm ui module {path} export herdr_ui_static_update_ptr failed: {err}")
        })? as usize;
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let len = self.static_len.call(&mut self.store, ()).map_err(|err| {
            format!("wasm ui module {path} export herdr_ui_static_update_len failed: {err}")
        })? as usize;
        let data = self.memory.data(&self.store);
        let end = ptr
            .checked_add(len)
            .ok_or_else(|| "wasm ui module update overflow".to_string())?;
        if end > data.len() {
            return Err(format!(
                "wasm ui module {path} returned out-of-bounds static update slice"
            ));
        }
        serde_json::from_slice::<UiUpdate>(&data[ptr..end])
            .map_err(|err| format!("failed to parse wasm ui module update {path}: {err}"))
    }

    fn dispatch(&mut self, event: &UiEvent) -> Result<Option<UiUpdate>, String> {
        // Borrow the func handles by reference; they live in fields disjoint from
        // `self.store`, so calling them while mutably borrowing the store is fine.
        let (Some(alloc), Some(on_event), Some(on_event_len)) =
            (&self.alloc, &self.on_event, &self.on_event_len)
        else {
            return Ok(None);
        };
        let bytes = serde_json::to_vec(event)
            .map_err(|err| format!("failed to serialize ui event: {err}"))?;
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let ptr = alloc
            .call(&mut self.store, bytes.len() as i32)
            .map_err(|err| format!("ui wasm alloc failed: {err}"))? as usize;
        let data = self.memory.data_mut(&mut self.store);
        let end = ptr
            .checked_add(bytes.len())
            .ok_or_else(|| "ui wasm event buffer overflow".to_string())?;
        if end > data.len() {
            return Err("ui wasm alloc returned out-of-bounds pointer".to_string());
        }
        data[ptr..end].copy_from_slice(&bytes);
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let result_ptr = on_event
            .call(&mut self.store, (ptr as i32, bytes.len() as i32))
            .map_err(|err| format!("ui wasm on_event failed: {err}"))?
            as usize;
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let result_len = on_event_len
            .call(&mut self.store, ())
            .map_err(|err| format!("ui wasm on_event_len failed: {err}"))?
            as usize;
        let data = self.memory.data(&self.store);
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
}

impl UiPluginManager {
    pub(crate) fn from_installed_plugins(plugins: &HashMap<String, InstalledPluginInfo>) -> Self {
        let runtime = match PluginRuntime::new() {
            Ok(runtime) => Some(runtime),
            Err(err) => {
                tracing::error!(error = %err, "failed to init wasm ui plugin runtime; ui plugins disabled");
                None
            }
        };
        let mut manager = Self {
            modules: Vec::new(),
            by_surface: HashMap::new(),
            mtimes: HashMap::new(),
            manifest_mtimes: HashMap::new(),
            loaded_updates: HashMap::new(),
            last_errors: HashMap::new(),
            pending_effects: Vec::new(),
            pending_notifications: Vec::new(),
            pending_api_effects: Vec::new(),
            runtime,
        };
        // Boot is just a sync against an empty registry: every enabled module is
        // "new" and gets instantiated, mtimes seeded, layers cached.
        manager.sync_to_registry(plugins);
        manager
    }

    /// fork(wave-2): reconcile the live module registry against the current
    /// installed-plugins registry after a lifecycle change (enable / disable /
    /// link / unlink) so it applies to a running server without a restart.
    ///
    /// - modules of disabled/unlinked plugins are removed: their cached layers,
    ///   error state, wasm instance, and mtime entries are dropped so overlays
    ///   disappear on the next frame;
    /// - modules of newly enabled/linked plugins are added and instantiated
    ///   lazily (identical to boot);
    /// - kept modules preserve their live instance + guest state, and only their
    ///   manifest-derived metadata (title, surfaces, capabilities, `ui_input_file`,
    ///   hot-reload mode) is refreshed. A kept key whose `module` path changed is
    ///   treated as a fresh module (instance invalidated, re-instantiated).
    ///
    /// Returns the `(plugin_id, module_id)` keys that were removed so the caller
    /// can drop per-module bookkeeping (e.g. `collectors_in_flight`).
    pub(crate) fn sync_to_registry(
        &mut self,
        plugins: &HashMap<String, InstalledPluginInfo>,
    ) -> Vec<(String, String)> {
        // Rebuild `modules`/`by_surface`/`mtimes` from scratch in deterministic
        // order (plugin_id asc, then each plugin's `ui_modules` vec order), reusing
        // the instances + cached updates of modules that survive the change.
        let old_modules = std::mem::take(&mut self.modules);
        let old_mtimes = std::mem::take(&mut self.mtimes);
        self.by_surface.clear();
        self.manifest_mtimes.clear();

        let mut plugin_list = plugins.values().collect::<Vec<_>>();
        plugin_list.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));

        let mut new_modules: Vec<RegisteredUiModule> = Vec::new();
        for plugin in plugin_list {
            if !plugin.enabled {
                continue;
            }
            let root = Path::new(&plugin.plugin_root);
            let manifest_path = PathBuf::from(&plugin.manifest_path);
            self.track_manifest_mtime(&manifest_path);
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
                    ui_input_file: module.ui_input_file.clone(),
                };
                let key = module_key(&registered);
                if let Some(mtime) = old_mtimes.get(&registered.module_path).copied() {
                    self.mtimes.insert(registered.module_path.clone(), mtime);
                } else if let Ok(meta) = std::fs::metadata(&registered.module_path) {
                    if let Ok(mtime) = meta.modified() {
                        self.mtimes.insert(registered.module_path.clone(), mtime);
                    }
                }

                let kept = old_modules
                    .iter()
                    .find(|old| module_key(old) == key)
                    .filter(|old| old.module_path == registered.module_path);
                if let Some(old) = kept {
                    // Preserve the negotiated version + live instance + cached
                    // update (guest state survives a metadata-only refresh).
                    registered.negotiated_version = old.negotiated_version;
                } else {
                    // New module, or a kept key whose wasm path moved: drop any
                    // stale instance/cache for this key and instantiate fresh.
                    self.loaded_updates.remove(&key);
                    if let Some(runtime) = self.runtime.as_mut() {
                        runtime.invalidate_key(&key);
                    }
                    match self.load_module_static(&registered) {
                        Ok((update, negotiated_version)) => {
                            registered.negotiated_version = negotiated_version;
                            self.loaded_updates.insert(key.clone(), update);
                            self.last_errors.remove(&key);
                        }
                        Err(err) => {
                            tracing::warn!(
                                plugin_id = %registered.plugin_id,
                                module_id = %registered.module_id,
                                module_path = %registered.module_path.display(),
                                error = %err,
                                "failed to load ui plugin module"
                            );
                            self.last_errors.insert(key.clone(), err);
                        }
                    }
                }
                new_modules.push(registered);
            }
        }

        // Rebuild the surface index over the final module vec.
        for (idx, module) in new_modules.iter().enumerate() {
            for surface in &module.surfaces {
                self.by_surface.entry(*surface).or_default().push(idx);
            }
        }

        // Drop every trace of modules that are no longer present: their layers
        // (so overlays disappear next frame), error state, and wasm instance.
        let desired: std::collections::HashSet<(String, String)> =
            new_modules.iter().map(module_key).collect();
        let mut removed = Vec::new();
        for old in &old_modules {
            let key = module_key(old);
            if desired.contains(&key) {
                continue;
            }
            removed.push(key.clone());
            self.loaded_updates.remove(&key);
            self.last_errors.remove(&key);
            if let Some(runtime) = self.runtime.as_mut() {
                runtime.invalidate_key(&key);
            }
        }

        self.modules = new_modules;
        removed
    }

    fn track_manifest_mtime(&mut self, manifest_path: &Path) {
        if self.manifest_mtimes.contains_key(manifest_path) {
            return;
        }
        if let Ok(meta) = std::fs::metadata(manifest_path) {
            if let Ok(mtime) = meta.modified() {
                self.manifest_mtimes
                    .insert(manifest_path.to_path_buf(), mtime);
            }
        }
    }

    /// fork(wave-2): manifest paths whose `herdr-plugin.toml` mtime advanced since
    /// the last check, deduplicated (modules of one plugin share a manifest). The
    /// stored mtime is advanced here so one edit is reported exactly once — a
    /// manifest that then fails to parse does not re-fire every tick. The caller
    /// re-parses each path and re-syncs the registry.
    pub(crate) fn detect_changed_manifests(&mut self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        let paths: Vec<PathBuf> = self.manifest_mtimes.keys().cloned().collect();
        for path in paths {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            let old = self.manifest_mtimes.get(&path).copied();
            if old.is_none() || old.is_some_and(|old| old < mtime) {
                self.manifest_mtimes.insert(path.clone(), mtime);
                changed.push(path);
            }
        }
        changed
    }

    /// fork(wave-2): queue a host-side toast (e.g. "manifest reload failed"),
    /// flushed through the same `apply_ui_plugin_notifications` path as guest
    /// reload notices.
    pub(crate) fn push_notification(&mut self, message: String) {
        self.pending_notifications.push(message);
    }

    /// Build (if needed) the live instance for `module` and read its initial
    /// static update plus negotiated version (§2). Used at load time; errors
    /// when the shared runtime failed to initialize.
    fn load_module_static(
        &mut self,
        module: &RegisteredUiModule,
    ) -> Result<(UiUpdate, u32), String> {
        match self.runtime.as_mut() {
            Some(runtime) => runtime.static_update(module),
            None => Err("wasm ui plugin runtime unavailable".to_string()),
        }
    }

    /// Drop and rebuild `module`'s instance from disk (fresh linear memory),
    /// then read its static update plus negotiated version. Used on hot-reload
    /// so guest state resets.
    fn reload_module_static(
        &mut self,
        module: &RegisteredUiModule,
    ) -> Result<(UiUpdate, u32), String> {
        match self.runtime.as_mut() {
            Some(runtime) => runtime.reload_static(module),
            None => Err("wasm ui plugin runtime unavailable".to_string()),
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
        // Iterate by index and clone each module so the reload path can take
        // `&mut self` without aliasing the borrow of `self.modules`, and so the
        // negotiated version can be written back to `self.modules[idx]`.
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
                match self.reload_module_static(&module) {
                    Ok((update, negotiated_version)) => {
                        // §6: hot-reload clears the merge cache — a full replace
                        // (not a partial merge) so a rebuilt module that renders
                        // fewer surfaces doesn't keep stale layers.
                        self.modules[idx].negotiated_version = negotiated_version;
                        self.clear_module_cache(&module.plugin_id, &module.module_id);
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

    /// Drops the merge cache for one module (§6: hot-reload / trap-recovery
    /// clears the cache so the fresh instance's next surfaces fully replace the
    /// stale ones instead of partial-merging into them). Called from the
    /// hot-reload path in `detect_changed_modules` (before re-seeding the fresh
    /// static update) and from `dispatch_event_for_module` when the persistent
    /// runtime had to re-instantiate a trapped module for the current event.
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
        let keys = self
            .modules
            .iter()
            .map(|module| (module.plugin_id.clone(), module.module_id.clone()))
            .collect::<Vec<_>>();
        let mut changed = false;
        for (plugin_id, module_id) in keys {
            // fork(f5-mouse-dedupe): a module whose current layers declare an
            // absolute hit-rect over this cell already receives the click through
            // the hit-rect resolution path (`ui::ui_plugin_hit_at` ->
            // pending_plugin_action_invocation), so suppress the duplicate raw
            // MouseAll broadcast for cells inside its own hit-rects. The raw
            // stream continues for every other cell. Kept general so v3 Action
            // routing can reuse `should_deliver_raw_mouse`.
            if !self.should_deliver_raw_mouse(&plugin_id, &module_id, mouse.column, mouse.row) {
                continue;
            }
            changed |= self.dispatch_event_for_module(&plugin_id, &module_id, &event);
        }
        changed
    }

    /// Single decision point for whether `module` should receive a raw mouse
    /// event through the MouseAll broadcast. Returns `false` when the module's
    /// currently loaded layers declare an absolute hit-rect covering
    /// `(col, row)`, because that cell is already handled by the declarative
    /// hit-rect path (see the `fork(f5-mouse-dedupe)` block in
    /// `dispatch_crossterm_mouse_event`). General enough to be reused by v3
    /// Action routing.
    pub(crate) fn should_deliver_raw_mouse(
        &self,
        plugin_id: &str,
        module_id: &str,
        col: u16,
        row: u16,
    ) -> bool {
        let Some(update) = self
            .loaded_updates
            .get(&(plugin_id.to_string(), module_id.to_string()))
        else {
            return true;
        };
        for layer in &update.layers {
            for hit in &layer.hits {
                if col >= hit.x
                    && col < hit.x.saturating_add(hit.width)
                    && row >= hit.y
                    && row < hit.y.saturating_add(hit.height)
                {
                    return false;
                }
            }
        }
        true
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
        // Clone so the reused live instance can be driven through `&mut self`
        // without aliasing the borrow of `self.modules`.
        let Some(module) = self
            .modules
            .iter()
            .find(|module| module.plugin_id == plugin_id && module.module_id == module_id)
            .cloned()
        else {
            return false;
        };
        if !module_accepts_event(&module, event) {
            return false;
        }
        let key = (plugin_id.to_string(), module_id.to_string());
        let started = Instant::now();
        let result = match self.runtime.as_mut() {
            Some(runtime) => runtime.dispatch(&module, event),
            None => Ok((None, false)),
        };
        let (update, rebuilt) = match result {
            Ok((Some(update), rebuilt)) => (update, rebuilt),
            Ok((None, _)) => return false,
            Err(err) => {
                tracing::warn!(
                    plugin_id = %module.plugin_id,
                    module_id = %module.module_id,
                    module_path = %module.module_path.display(),
                    error = %err,
                    "failed to handle ui plugin event"
                );
                // Crash isolation: drop the poisoned/trapped store so the module
                // is rebuilt from disk on the next event instead of reused.
                if let Some(runtime) = self.runtime.as_mut() {
                    runtime.invalidate(&module);
                }
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
        tracing::debug!(
            plugin_id = %module.plugin_id,
            module_id = %module.module_id,
            elapsed_us = started.elapsed().as_micros(),
            "ui plugin event dispatched"
        );
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
            .extend(filter_effects_for_module(&module, &other_effects));
        queue_api_effects(
            &mut self.pending_api_effects,
            plugin_id,
            module_id,
            api_effects,
        );
        if rebuilt {
            // §6: a freshly (re)built instance (first event after a trap-recovery
            // re-instantiation) must fully replace its surfaces instead of
            // partial-merging into the pre-trap layers kept for last-good
            // rendering. Clearing here makes the merge below see no old layers.
            self.clear_module_cache(plugin_id, module_id);
        }
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

    #[cfg(test)]
    fn test_instance_cached(&self, plugin_id: &str, module_id: &str) -> bool {
        self.runtime.as_ref().is_some_and(|runtime| {
            runtime
                .instances
                .contains_key(&(plugin_id.to_string(), module_id.to_string()))
        })
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
            HostEffect::ShowToast { .. } => {
                module.requests(crate::api::schema::PluginUiCapability::Toasts)
            }
            HostEffect::OpenPluginPane { .. } => {
                module.requests(crate::api::schema::PluginUiCapability::OpenPluginPanes)
            }
            HostEffect::InvokePluginAction { .. } => {
                module.requests(crate::api::schema::PluginUiCapability::PluginActions)
            }
            // §7.2 own-surface guard: a module may only collapse/expand
            // surfaces it declares in its manifest — no capability required.
            HostEffect::SetSurfaceCollapsed { surface, .. } => module
                .surfaces
                .iter()
                .any(|declared| map_surface(*declared) == *surface),
            // Api effects are never routed through this generic capability
            // filter: `dispatch_event_for_module` extracts and queues them
            // separately (via `queue_api_effects`) before this function ever
            // sees `other_effects`, so this arm exists only for exhaustiveness.
            HostEffect::Api { .. } => false,
        })
        .cloned()
        .collect()
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

    #[test]
    fn set_surface_collapsed_requires_declared_surface() {
        let module = RegisteredUiModule {
            plugin_id: "example.demo".into(),
            module_id: "demo".into(),
            title: "Demo".into(),
            module_path: std::path::PathBuf::from("demo.wasm"),
            manifest_path: std::path::PathBuf::from("herdr-plugin.toml"),
            api_version: 3,
            negotiated_version: 3,
            hot_reload: PluginHotReloadMode::On,
            surfaces: vec![PluginUiSurface::RightPanel],
            requested_capabilities: Vec::new(),
            ui_input_file: None,
        };
        let effect = HostEffect::SetSurfaceCollapsed {
            surface: UiSurface::UsageStrip,
            collapsed: true,
        };

        // §7.2 own-surface guard: dropped when the surface is not declared.
        assert!(filter_effects_for_module(&module, std::slice::from_ref(&effect)).is_empty());

        let mut declaring = module.clone();
        declaring.surfaces.push(PluginUiSurface::UsageStrip);
        let kept = filter_effects_for_module(&declaring, std::slice::from_ref(&effect));
        assert_eq!(kept.len(), 1);
        assert!(matches!(
            kept[0],
            HostEffect::SetSurfaceCollapsed {
                surface: UiSurface::UsageStrip,
                collapsed: true,
            }
        ));
    }

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
                ui_input_file: None,
                collector: None,
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

    fn unique_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        root
    }

    fn installed(plugin: InstalledPluginInfo) -> UiPluginManager {
        UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin,
        )]))
    }

    fn plugin_at(root: &std::path::Path) -> InstalledPluginInfo {
        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        plugin
    }

    fn header_text(manager: &UiPluginManager) -> String {
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node, got {:?}", layers.first());
        };
        spans[0].text.clone()
    }

    /// A guest with a persistent mutable counter. Each `on_event` increments the
    /// counter and writes `'0' + counter` into the single `@` placeholder byte of
    /// `template`, returning the whole template. If the instance were recreated
    /// per event the counter would reset, so the rendered digit proves guest
    /// linear memory persists.
    fn write_counter_wasm(path: &std::path::Path, template: &str) {
        write_counter_wasm_versioned(path, template, 2);
    }

    fn write_counter_wasm_versioned(path: &std::path::Path, template: &str, api_version: u32) {
        let digit_offset = template
            .find('@')
            .expect("template needs a '@' placeholder");
        let initial = template.replace('@', "0");
        let len = initial.len();
        let data = initial
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            "(module
                (memory (export \"memory\") 1)
                (global $counter (mut i32) (i32.const 0))
                (data (i32.const 0) \"{data}\")
                (func (export \"herdr_ui_api_version\") (result i32) i32.const {api_version})
                (func (export \"herdr_ui_static_update_ptr\") (result i32) i32.const 0)
                (func (export \"herdr_ui_static_update_len\") (result i32) i32.const {len})
                (func (export \"herdr_ui_alloc\") (param i32) (result i32) i32.const 4096)
                (func (export \"herdr_ui_on_event\") (param i32 i32) (result i32)
                    (global.set $counter (i32.add (global.get $counter) (i32.const 1)))
                    (i32.store8 (i32.const {digit_offset}) (i32.add (i32.const 48) (global.get $counter)))
                    (i32.const 0))
                (func (export \"herdr_ui_on_event_len\") (result i32) i32.const {len}))"
        );
        let wasm = wat::parse_str(&wat).unwrap();
        std::fs::write(path, wasm).unwrap();
    }

    /// Dedicated, frozen **api_version = 2** compat guest for TC7 "old-guest
    /// compat". It renders a single static `usage_strip` layer carrying a legacy
    /// usage action id in a hit-rect and bumps a persistent counter on every
    /// event it is allowed to receive — no v3 features (no Action/ApiResult
    /// handling, no `HostEffect::Api`, no partial-scope updates). Because the
    /// counter digit is observable through the rendered layer, a v3 event that
    /// the host refuses to deliver leaves the digit unchanged. Kept independent
    /// of the production usage-dash guest so TC7 survives that guest moving to
    /// v3 (after which no other v2 guest exists in the tree).
    fn write_v2_usage_compat_guest_wasm(path: &std::path::Path) {
        // `@` is the counter placeholder byte the guest overwrites per event.
        write_counter_wasm_versioned(path, V2_USAGE_COMPAT_TEMPLATE, 2);
    }

    const V2_USAGE_COMPAT_TEMPLATE: &str = r#"{"layers":[{"surface":"usage_strip","hits":[{"x":0,"y":0,"width":5,"height":1,"action_id":"usage.cycle"}],"nodes":[{"kind":"line","spans":[{"text":"@"}]}]}]}"#;

    /// A guest whose static update is valid but whose `on_event` runs
    /// `on_event_body` (used for traps and infinite loops that never return).
    fn write_event_wasm(path: &std::path::Path, static_json: &str, on_event_body: &str) {
        let data = static_json
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            "(module
                (memory (export \"memory\") 1)
                (data (i32.const 0) \"{data}\")
                (func (export \"herdr_ui_api_version\") (result i32) i32.const 2)
                (func (export \"herdr_ui_static_update_ptr\") (result i32) i32.const 0)
                (func (export \"herdr_ui_static_update_len\") (result i32) i32.const {len})
                (func (export \"herdr_ui_alloc\") (param i32) (result i32) i32.const 4096)
                (func (export \"herdr_ui_on_event\") (param i32 i32) (result i32) {on_event_body})
                (func (export \"herdr_ui_on_event_len\") (result i32) i32.const 0))",
            len = static_json.len()
        );
        let wasm = wat::parse_str(&wat).unwrap();
        std::fs::write(path, wasm).unwrap();
    }

    const COUNTER_TEMPLATE: &str =
        r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"@"}]}]}]}"#;

    #[test]
    fn guest_state_persists_across_events() {
        let root = unique_root("state-persist");
        write_counter_wasm(&root.join("dist/goal-ui.wasm"), COUNTER_TEMPLATE);
        let mut manager = installed(plugin_at(&root));

        assert_eq!(header_text(&manager), "0");
        assert!(manager.dispatch_tick());
        assert_eq!(header_text(&manager), "1");
        assert!(manager.dispatch_tick());
        // A per-event rebuild would reset the counter to 1 each time; "2" proves
        // the live instance (and its linear memory) is reused across events.
        assert_eq!(header_text(&manager), "2");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hot_reload_swaps_instance_and_resets_state() {
        let root = unique_root("hot-reload-reset");
        let module_path = root.join("dist/goal-ui.wasm");
        write_counter_wasm(&module_path, COUNTER_TEMPLATE);
        let mut manager = installed(plugin_at(&root));

        assert!(manager.dispatch_tick());
        assert!(manager.dispatch_tick());
        assert_eq!(header_text(&manager), "2");

        // Rewrite the module so its mtime advances; hot-reload must swap the live
        // instance and discard the counter (fresh linear memory).
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_counter_wasm(&module_path, COUNTER_TEMPLATE);
        let changed = manager.detect_changed_modules();
        assert_eq!(changed.len(), 1);
        assert_eq!(header_text(&manager), "0");

        // Counting resumes from the fresh instance's zeroed state.
        assert!(manager.dispatch_tick());
        assert_eq!(header_text(&manager), "1");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn trap_marks_module_failed_then_next_event_recreates() {
        let root = unique_root("trap-recreate");
        let module_path = root.join("dist/goal-ui.wasm");
        let broken_static = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"broken"}]}]}]}"#;
        write_event_wasm(&module_path, broken_static, "unreachable");
        let mut manager = installed(plugin_at(&root));

        // Static loaded fine, so the instance is cached.
        assert_eq!(header_text(&manager), "broken");
        assert!(manager.test_instance_cached("example.goal-ui", "goal-ui"));

        // The event traps: no update applied, a "event failed" toast is queued,
        // and the poisoned store is dropped (crash isolation).
        assert!(!manager.dispatch_tick());
        assert!(!manager.test_instance_cached("example.goal-ui", "goal-ui"));
        let notes = manager.take_pending_notifications();
        assert!(
            notes.iter().any(|note| note.contains("event failed")),
            "expected event-failed toast, got {notes:?}"
        );

        // Replace the module on disk with a working guest WITHOUT a mtime-driven
        // reload; the next event must rebuild the instance from disk and succeed.
        write_counter_wasm(&module_path, COUNTER_TEMPLATE);
        assert!(manager.dispatch_tick());
        assert!(manager.test_instance_cached("example.goal-ui", "goal-ui"));
        assert_eq!(header_text(&manager), "1");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn epoch_deadline_traps_spinning_guest() {
        let root = unique_root("epoch-spin");
        let spin_static = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"spin"}]}]}]}"#;
        write_event_wasm(
            &root.join("dist/goal-ui.wasm"),
            spin_static,
            "(loop $l (br $l)) unreachable",
        );
        let mut manager = installed(plugin_at(&root));

        let started = std::time::Instant::now();
        // The guest never returns; the epoch ticker must trap it. Without epoch
        // interruption this call would hang the test forever.
        assert!(!manager.dispatch_tick());
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "spinning guest was not interrupted in time: {elapsed:?}"
        );
        let notes = manager.take_pending_notifications();
        assert!(
            notes.iter().any(|note| note.contains("event failed")),
            "expected event-failed toast, got {notes:?}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn should_deliver_raw_mouse_suppresses_declared_hit_rects() {
        let root = unique_root("f5-decision");
        write_static_wasm(
            &root.join("dist/goal-ui.wasm"),
            r#"{"layers":[{"surface":"header","hits":[{"x":10,"y":0,"width":5,"height":1,"action_id":"a"}],"nodes":[]}]}"#,
        );
        let manager = installed(plugin_at(&root));

        // Cells covered by the module's own hit-rect are handled by the
        // declarative path, so the raw MouseAll broadcast is suppressed.
        assert!(!manager.should_deliver_raw_mouse("example.goal-ui", "goal-ui", 10, 0));
        assert!(!manager.should_deliver_raw_mouse("example.goal-ui", "goal-ui", 14, 0));
        // Cells outside still get the raw broadcast.
        assert!(manager.should_deliver_raw_mouse("example.goal-ui", "goal-ui", 9, 0));
        assert!(manager.should_deliver_raw_mouse("example.goal-ui", "goal-ui", 15, 0));
        assert!(manager.should_deliver_raw_mouse("example.goal-ui", "goal-ui", 12, 1));
        // Unknown module: nothing to dedupe against.
        assert!(manager.should_deliver_raw_mouse("nope", "nope", 12, 0));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn raw_mouse_broadcast_skips_declared_hit_rects() {
        let root = unique_root("f5-behavioral");
        write_counter_wasm(
            &root.join("dist/goal-ui.wasm"),
            r#"{"layers":[{"surface":"header","hits":[{"x":10,"y":0,"width":5,"height":1,"action_id":"a"}],"nodes":[{"kind":"line","spans":[{"text":"@"}]}]}]}"#,
        );
        let mut manager = installed(plugin_at(&root));

        let moved = |col: u16, row: u16| crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };

        assert_eq!(header_text(&manager), "0");
        // Inside the module's declared hit-rect: raw broadcast suppressed, the
        // guest's on_event is never invoked, so the counter does not advance.
        assert!(!manager.dispatch_crossterm_mouse_event(moved(12, 0)));
        assert_eq!(header_text(&manager), "0");
        // Outside the hit-rect: delivered, counter advances.
        assert!(manager.dispatch_crossterm_mouse_event(moved(0, 0)));
        assert_eq!(header_text(&manager), "1");

        let _ = std::fs::remove_dir_all(root);
    }

    /// Spec §4 dedupe composition: a v3 module that declares a hit-rect owns the
    /// clicks inside it via the `Action` route, so the raw `MouseAll` broadcast
    /// for that same cell must be suppressed — the module must not receive both.
    /// A left-down inside the rect leaves the guest's counter untouched (no raw
    /// delivery), while an explicit `Action` dispatch does advance it, proving
    /// the module stays reachable through the Action path only.
    #[test]
    fn v3_action_click_is_not_also_delivered_as_raw_mouse() {
        let root = unique_root("f5-v3-dedupe");
        write_counter_wasm_versioned(
            &root.join("dist/goal-ui.wasm"),
            r#"{"layers":[{"surface":"header","hits":[{"x":10,"y":0,"width":5,"height":1,"action_id":"a"}],"nodes":[{"kind":"line","spans":[{"text":"@"}]}]}]}"#,
            3,
        );
        let mut plugin = plugin_at(&root);
        plugin.ui_modules[0].api_version = 3;
        let mut manager = installed(plugin);

        assert_eq!(header_text(&manager), "0");
        // Left-down inside the declared hit-rect: the Action route owns it, so the
        // raw MouseAll broadcast is suppressed and the guest counter never moves.
        let left_down = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 12,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        assert!(!manager.dispatch_crossterm_mouse_event(left_down));
        assert_eq!(header_text(&manager), "0");
        // The same v3 module IS still reachable through the Action route.
        assert!(manager.dispatch_action(
            "example.goal-ui",
            "goal-ui",
            "a".into(),
            UiSurface::Header,
            2,
            0,
            UiActionKind::LeftDown,
        ));
        assert_eq!(header_text(&manager), "1");

        let _ = std::fs::remove_dir_all(root);
    }

    /// TC7 old-guest compat: a guaranteed-v2 guest negotiates version 2, renders
    /// its static layer, still receives v2-legal events, and is NOT delivered the
    /// v3-only `Action`/`ApiResult` events — the host refuses them and the guest
    /// state is left untouched.
    #[test]
    fn v2_compat_guest_negotiates_v2_and_refuses_v3_events() {
        let root = unique_root("g-v2-compat");
        write_v2_usage_compat_guest_wasm(&root.join("dist/goal-ui.wasm"));
        let mut plugin = plugin_at(&root);
        // Declare the surface the frozen guest actually paints and pin its
        // capabilities to upstream-safe (pre-v3) ones only, so this fixture never
        // depends on a fork-only capability variant.
        plugin.ui_modules[0].surfaces = vec![PluginUiSurface::UsageStrip];
        plugin.ui_modules[0].capabilities = vec![
            PluginUiCapability::FocusedSession,
            PluginUiCapability::LayoutRead,
        ];

        let mut manager = installed(plugin);

        // Version gating: the guest exports 2 and the manifest says 2, so the
        // negotiated version is 2 (host max is 3).
        assert_eq!(manager.all().len(), 1);
        assert_eq!(manager.all()[0].negotiated_version, 2);
        assert_eq!(manager.all()[0].manifest_ref().api_version, 2);

        // Static layer render: the single usage-strip layer is projected with the
        // guest's initial counter digit.
        let layers = manager.surface_layers();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].layer.surface, UiSurface::UsageStrip);
        assert_eq!(layers[0].negotiated_version, 2);
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!(
                "expected line node, got {:?}",
                layers[0].layer.nodes.first()
            );
        };
        assert_eq!(spans[0].text, "0");

        // v3-only events are refused for a v2 guest: both dispatch calls return
        // false and the observable counter digit stays at "0".
        assert!(!manager.dispatch_action(
            "example.goal-ui",
            "goal-ui",
            "usage.cycle".into(),
            UiSurface::UsageStrip,
            0,
            0,
            UiActionKind::LeftDown,
        ));
        assert!(!manager.dispatch_api_result(
            "example.goal-ui",
            "goal-ui",
            "req-1".into(),
            true,
            None,
            None,
        ));
        assert_eq!(first_layer_text(&manager), "0");

        // Contrast: a v2-legal event (tick) IS delivered, advancing the counter —
        // so the guest is healthy and the refusal above is specifically the v3
        // event gate, not a broken guest.
        assert!(manager.dispatch_tick());
        assert_eq!(first_layer_text(&manager), "1");

        let _ = std::fs::remove_dir_all(root);
    }

    fn first_layer_text(manager: &UiPluginManager) -> String {
        let layers = manager.surface_layers();
        let UiNode::Line { spans } = &layers[0].layer.nodes[0] else {
            panic!("expected line node, got {:?}", layers.first());
        };
        spans[0].text.clone()
    }

    // fork(wave-2): a plugin backed by a real static-update wasm module on disk,
    // rooted at a fresh temp dir, so the lifecycle re-sync tests exercise actual
    // instantiation/teardown (no mocking).
    fn plugin_on_disk(header_text: &str) -> (std::path::PathBuf, InstalledPluginInfo) {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-sync-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        let json = format!(
            r#"{{"layers":[{{"surface":"header","nodes":[{{"kind":"line","spans":[{{"text":"{header_text}","role":"accent"}}]}}]}}]}}"#
        );
        write_static_wasm(&root.join("dist/goal-ui.wasm"), &json);
        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        // The disk module only declares Header (ContextMenu from sample_plugin has
        // no menu layer in this static update), keep surfaces truthful.
        plugin.ui_modules[0].surfaces = vec![PluginUiSurface::Header];
        (root, plugin)
    }

    #[test]
    fn sync_to_registry_adds_removes_and_re_enables_modules() {
        let (root, plugin) = plugin_on_disk("Enabled");
        let key = (plugin.plugin_id.clone(), plugin.ui_modules[0].id.clone());
        let mut registry = HashMap::from([(plugin.plugin_id.clone(), plugin.clone())]);
        let mut manager = UiPluginManager::from_installed_plugins(&registry);

        // Enabled at boot: one module, one rendered header layer, cached update.
        assert_eq!(manager.all().len(), 1);
        assert_eq!(manager.surface_layers().len(), 1);
        assert!(manager.loaded_updates.contains_key(&key));
        assert_eq!(manager.for_surface(PluginUiSurface::Header).len(), 1);

        // Disable -> module removed, layers dropped, cache/instance gone, key
        // reported as removed.
        registry.get_mut(&plugin.plugin_id).unwrap().enabled = false;
        let removed = manager.sync_to_registry(&registry);
        assert_eq!(removed, vec![key.clone()]);
        assert_eq!(manager.all().len(), 0);
        assert_eq!(manager.surface_layers().len(), 0);
        assert!(!manager.loaded_updates.contains_key(&key));
        assert_eq!(manager.for_surface(PluginUiSurface::Header).len(), 0);

        // Re-enable -> module re-instantiated from disk, layer back, nothing
        // reported removed.
        registry.get_mut(&plugin.plugin_id).unwrap().enabled = true;
        let removed = manager.sync_to_registry(&registry);
        assert!(removed.is_empty());
        assert_eq!(manager.all().len(), 1);
        assert_eq!(first_layer_text(&manager), "Enabled");
        assert!(manager.loaded_updates.contains_key(&key));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sync_to_registry_refreshes_kept_module_metadata_without_reinstantiating() {
        let (root, plugin) = plugin_on_disk("Kept");
        let key = (plugin.plugin_id.clone(), plugin.ui_modules[0].id.clone());
        let mut registry = HashMap::from([(plugin.plugin_id.clone(), plugin.clone())]);
        let mut manager = UiPluginManager::from_installed_plugins(&registry);
        assert!(manager.loaded_updates.contains_key(&key));
        assert!(manager.all()[0].ui_input_file.is_none());

        // Change metadata only (same wasm path): add a surface + ui_input_file.
        {
            let module = &mut registry.get_mut(&plugin.plugin_id).unwrap().ui_modules[0];
            module.surfaces = vec![PluginUiSurface::Header, PluginUiSurface::Footer];
            module.ui_input_file = Some("input.json".into());
        }
        let removed = manager.sync_to_registry(&registry);
        assert!(removed.is_empty());
        // Metadata refreshed on the kept module.
        assert_eq!(
            manager.all()[0].surfaces,
            vec![PluginUiSurface::Header, PluginUiSurface::Footer]
        );
        assert_eq!(manager.all()[0].ui_input_file.as_deref(), Some("input.json"));
        assert_eq!(manager.for_surface(PluginUiSurface::Footer).len(), 1);
        // The live instance + cached update survived (guest state preserved).
        assert!(manager.loaded_updates.contains_key(&key));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn detect_changed_manifests_reports_edits_once() {
        let (root, plugin) = plugin_on_disk("Manifest");
        let manifest_path = std::path::PathBuf::from(&plugin.manifest_path);
        // Seed an initial manifest file so an mtime is tracked at sync time.
        std::fs::write(&manifest_path, "id = \"example.goal-ui\"\n").unwrap();
        let registry = HashMap::from([(plugin.plugin_id.clone(), plugin.clone())]);
        let mut manager = UiPluginManager::from_installed_plugins(&registry);

        // No change yet.
        assert!(manager.detect_changed_manifests().is_empty());

        // Advance the manifest mtime (same sleep+rewrite technique as the wasm
        // hot-reload tests); the change is reported exactly once.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&manifest_path, "id = \"example.goal-ui\"\nname = \"x\"\n").unwrap();
        let changed = manager.detect_changed_manifests();
        assert_eq!(changed, vec![manifest_path.clone()]);
        assert!(manager.detect_changed_manifests().is_empty());

        let _ = std::fs::remove_dir_all(root);
    }
}
