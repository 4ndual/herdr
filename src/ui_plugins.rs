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
use wasmtime::{Caller, Engine, Linker, Memory, Module, Store, TypedFunc};

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
    /// Data-driven placement for this module's `Panel` surface (manifest DATA).
    /// The host's generic panel layout reads it; a manifest edit hot-applies a
    /// move/resize with no host rebuild. `None` → free-overlay default.
    pub placement: Option<crate::api::schema::PluginPanelPlacement>,
    pub requested_capabilities: Vec<crate::api::schema::PluginUiCapability>,
    /// Manifest `ui_input_file` (spec §7.1): a state-dir filename whose contents
    /// the host surfaces as `HERDR_PLUGIN_UI_INPUT_JSON` each frame. `None` when
    /// the module declares no input file.
    pub ui_input_file: Option<String>,
    /// Manifest `image_file` (fork): a plugin-directory-relative image filename
    /// the HOST loads and paints over this module's surface `Rect` (kitty path,
    /// half-block fallback). Purely host-consumed — never crosses the wasm ABI.
    /// `None` when the module declares no image.
    pub image_file: Option<String>,
    /// Manifest `graphics_roots` (fork): absolute directory roots this module may
    /// load images from via `pane.graphics.set { source_path }`. Empty means the
    /// module may not use `source_path` at all. Stored unresolved; the host
    /// canonicalises at use time so a root that appears later still works.
    pub graphics_roots: Vec<String>,
    /// fork(+127): manifest `max_layers_per_pane`, or `None` for the host default.
    pub max_layers_per_pane: Option<usize>,
    /// fork(host-actions): manifest `host_actions`, kept as declared. Resolution
    /// to real controls and the capability check both happen in
    /// `host_action_claims`, so an unheld capability is visible here as
    /// "declared but not granted" rather than erased at registration.
    pub host_actions: Vec<String>,
    /// fork(host-actions): manifest `host_action_mode`, defaulted to observe.
    pub host_action_mode: crate::api::schema::PluginHostActionMode,
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
    /// Owning module's `Panel` placement spec, carried onto every layer so the
    /// host's generic panel layout (in `compute_view`) can carve/position the
    /// rect from DATA without reaching back into `UiPluginManager`. `None` for
    /// non-panel layers and for panel modules that declared no placement.
    pub placement: Option<crate::api::schema::PluginPanelPlacement>,
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

/// fork(wave-2): per-module ceiling on the handoff state blob. The exposure is
/// for user intent (toggles, selections), not for shipping a guest's heap
/// through the handoff manifest, which is a single JSON line. A guest over the
/// limit is refused and starts fresh rather than bloating the swap.
const MAX_GUEST_STATE_BYTES: usize = 64 * 1024;

/// fork(wave-2): one guest's opt-in state, carried across a live handoff.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct UiPluginGuestState {
    pub plugin_id: String,
    pub module_id: String,
    pub state: Vec<u8>,
}

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
    /// fork(host-actions): keyboard-capture requests extracted from module
    /// updates, identity-tagged like `pending_api_effects` because the generic
    /// `pending_effects` channel deliberately carries no module identity.
    pending_keyboard_captures: Vec<(String, String, bool)>,
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
    /// fork(+127): whether a compiled+instantiated guest exists for this module.
    fn has_instance(&self, plugin_id: &str, module_id: &str) -> bool {
        self.instances
            .contains_key(&(plugin_id.to_string(), module_id.to_string()))
    }

    /// Bytes of linear memory the guest instance currently holds.
    ///
    /// Guest memory lives inside the wasm instance and is invisible to the
    /// process footprint, so without this a leaking module is indistinguishable
    /// from an idle one from outside.
    fn instance_memory_bytes(&self, plugin_id: &str, module_id: &str) -> Option<usize> {
        let instance = self
            .instances
            .get(&(plugin_id.to_string(), module_id.to_string()))?;
        Some(instance.memory.data_size(&instance.store))
    }

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
        // fork(#49): the `Err` branch above is not the only way this cache can be
        // useless -- and for two years it was useless in the way that does NOT
        // log. Under `debug_assertions` wasmtime names the cache namespace
        // `{compiler}-{GIT_REV}-{exe_mtime_ms}`, so a binary that gets relinked
        // between runs starts cold every time while `cache_config_load_default`
        // returns `Ok` and says nothing. That is what happened here: a
        // `rerun-if-changed` on a path that cannot exist in a git worktree made
        // cargo relink on every build, and the cache accumulated 215 write-only
        // namespaces without one hit. Fixed in build.rs, but the silence was the
        // real defect -- an inert cache must at least be an audible one, so say
        // out loud which namespace a debug build is using. Release builds key on
        // GIT_REV and are stable, so this is debug-only and costs them nothing.
        if cfg!(debug_assertions) {
            let exe_mtime = std::env::current_exe()
                .ok()
                .and_then(|path| path.metadata().ok())
                .and_then(|meta| meta.modified().ok())
                .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|dur| dur.as_millis());
            tracing::debug!(
                ?exe_mtime,
                "wasm ui plugin cache namespace is keyed on this binary's mtime \
                 (debug build); a relink between runs means a cold cache"
            );
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

    /// fork(wave-2): read the opt-in state blob from every LIVE instance. Only
    /// instantiated modules are asked — a module that was never built has no
    /// state to carry by definition.
    fn save_guest_state(&mut self) -> Vec<UiPluginGuestState> {
        let keys = self.instances.keys().cloned().collect::<Vec<_>>();
        let mut saved = Vec::new();
        for key in keys {
            let Some(instance) = self.instances.get_mut(&key) else {
                continue;
            };
            match instance.save_state() {
                Ok(Some(state)) => saved.push(UiPluginGuestState {
                    plugin_id: key.0.clone(),
                    module_id: key.1.clone(),
                    state,
                }),
                Ok(None) => {}
                Err(err) => tracing::warn!(
                    plugin_id = %key.0,
                    module_id = %key.1,
                    error = %err,
                    "ui plugin declined to save handoff state; it will start fresh"
                ),
            }
        }
        saved
    }

    fn restore_guest_state(&mut self, module: &RegisteredUiModule, bytes: &[u8]) -> bool {
        let key = module_key(module);
        match self.instance_for(module) {
            Ok((instance, _rebuilt)) => match instance.restore_state(bytes) {
                Ok(accepted) => accepted,
                Err(err) => {
                    tracing::warn!(
                        plugin_id = %key.0,
                        module_id = %key.1,
                        error = %err,
                        "ui plugin failed to restore handoff state; starting fresh"
                    );
                    false
                }
            },
            Err(err) => {
                tracing::warn!(
                    plugin_id = %key.0,
                    module_id = %key.1,
                    error = %err,
                    "ui plugin instance unavailable for handoff state restore"
                );
                false
            }
        }
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
    /// fork(wave-2): optional guest state-carry exports (§ live handoff). A guest
    /// that omits them keeps today's behaviour exactly — its state resets on
    /// handoff — so this is an opt-in exposure, not a flag day.
    save_state_ptr: Option<TypedFunc<(), i32>>,
    save_state_len: Option<TypedFunc<(), i32>>,
    restore_state: Option<TypedFunc<(i32, i32), i32>>,
    /// The version negotiated with the guest's `herdr_ui_api_version` export at
    /// build time (2 or 3, §2). Surfaced to the manager so it can be stored on
    /// the module and gate v3-only event delivery in `module_accepts_event`.
    negotiated_version: u32,
}

/// Give guests a working `fd_write` that lands in the host log.
///
/// UI modules run freestanding, with no WASI. Anything a guest printed —
/// `println!`, a panic message, `dbg!` left in during development — either
/// failed instantiation with "unknown import" or vanished, so plugin authors
/// had no way at all to see inside their own module and the host had no record
/// of a guest that was screaming. Both stdout (fd 1) and stderr (fd 2) are
/// routed to `tracing` tagged with the module identity; every other fd reports
/// EBADF rather than pretending the write happened.
fn define_guest_stdio(
    linker: &mut Linker<()>,
    plugin_id: &str,
    module_id: &str,
) -> Result<(), String> {
    let plugin_id = plugin_id.to_string();
    let module_id = module_id.to_string();
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            move |mut caller: Caller<'_, ()>,
                  fd: i32,
                  iovs: i32,
                  iovs_len: i32,
                  nwritten: i32|
                  -> i32 {
                const ERRNO_SUCCESS: i32 = 0;
                const ERRNO_BADF: i32 = 8;
                if fd != 1 && fd != 2 {
                    return ERRNO_BADF;
                }
                let Some(wasmtime::Extern::Memory(memory)) = caller.get_export("memory") else {
                    return ERRNO_BADF;
                };
                let data = memory.data(&caller);
                let mut text = String::new();
                let mut written: u32 = 0;
                for i in 0..iovs_len.max(0) {
                    let head = iovs as usize + (i as usize * 8);
                    let Some(iov) = data.get(head..head + 8) else {
                        break;
                    };
                    let ptr = u32::from_le_bytes([iov[0], iov[1], iov[2], iov[3]]) as usize;
                    let len = u32::from_le_bytes([iov[4], iov[5], iov[6], iov[7]]) as usize;
                    // A guest cannot be trusted to log a bounded amount, and this
                    // runs on the render path: take a slice, never the whole heap.
                    let len = len.min(GUEST_LOG_MAX_BYTES);
                    let Some(bytes) = data.get(ptr..ptr.saturating_add(len)) else {
                        break;
                    };
                    text.push_str(&String::from_utf8_lossy(bytes));
                    written = written.saturating_add(len as u32);
                }
                let text = text.trim_end();
                if !text.is_empty() {
                    tracing::warn!(plugin = %plugin_id, module = %module_id, fd, "{text}");
                }
                // Report the byte count back so the guest's writer does not spin
                // retrying a short write.
                let nwritten = nwritten as usize;
                if let Some(slot) = memory.data_mut(&mut caller).get_mut(nwritten..nwritten + 4) {
                    slot.copy_from_slice(&written.to_le_bytes());
                }
                ERRNO_SUCCESS
            },
        )
        .map_err(|err| format!("failed to define guest fd_write: {err}"))?;
    Ok(())
}

/// Per-iovec cap on guest log text. A runaway guest gets truncated, not a host
/// allocation the size of its heap.
const GUEST_LOG_MAX_BYTES: usize = 4096;

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
        let mut linker = Linker::new(engine);
        define_guest_stdio(&mut linker, &module.plugin_id, &module.module_id)?;
        let instance = linker
            .instantiate(&mut store, &compiled)
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
        // Optional, like the event entry points above: absent means this guest
        // does not carry state across a handoff.
        let save_state_ptr = instance
            .get_typed_func::<(), i32>(&mut store, "herdr_ui_save_state_ptr")
            .ok();
        let save_state_len = instance
            .get_typed_func::<(), i32>(&mut store, "herdr_ui_save_state_len")
            .ok();
        let restore_state = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "herdr_ui_restore_state")
            .ok();
        Ok(Self {
            store,
            memory,
            alloc,
            on_event,
            on_event_len,
            static_ptr,
            static_len,
            save_state_ptr,
            save_state_len,
            restore_state,
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

    /// fork(wave-2): read the guest's opt-in handoff state blob, mirroring the
    /// `static_update_ptr`/`_len` pair. `Ok(None)` means the guest does not
    /// implement the exposure — the common case, and not an error.
    fn save_state(&mut self) -> Result<Option<Vec<u8>>, String> {
        let (Some(ptr_fn), Some(len_fn)) = (&self.save_state_ptr, &self.save_state_len) else {
            return Ok(None);
        };
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let ptr = ptr_fn
            .call(&mut self.store, ())
            .map_err(|err| format!("ui wasm save_state_ptr failed: {err}"))?
            as usize;
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let len = len_fn
            .call(&mut self.store, ())
            .map_err(|err| format!("ui wasm save_state_len failed: {err}"))?
            as usize;
        if len == 0 {
            return Ok(None);
        }
        if len > MAX_GUEST_STATE_BYTES {
            return Err(format!(
                "ui wasm state blob is {len} bytes, over the {MAX_GUEST_STATE_BYTES} byte limit"
            ));
        }
        let data = self.memory.data(&self.store);
        let end = ptr
            .checked_add(len)
            .ok_or_else(|| "ui wasm state overflow".to_string())?;
        if end > data.len() {
            return Err("ui wasm save_state returned out-of-bounds slice".to_string());
        }
        Ok(Some(data[ptr..end].to_vec()))
    }

    /// Hand a saved blob back to a freshly built instance. Returns false when the
    /// guest does not implement the exposure, so the caller can tell "declined"
    /// from "restored". A guest that rejects the blob returns 0 and starts fresh.
    fn restore_state(&mut self, bytes: &[u8]) -> Result<bool, String> {
        let (Some(alloc), Some(restore)) = (&self.alloc, &self.restore_state) else {
            return Ok(false);
        };
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let ptr = alloc
            .call(&mut self.store, bytes.len() as i32)
            .map_err(|err| format!("ui wasm alloc failed: {err}"))? as usize;
        let data = self.memory.data_mut(&mut self.store);
        let end = ptr
            .checked_add(bytes.len())
            .ok_or_else(|| "ui wasm state buffer overflow".to_string())?;
        if end > data.len() {
            return Err("ui wasm alloc returned out-of-bounds pointer".to_string());
        }
        data[ptr..end].copy_from_slice(bytes);
        self.store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let accepted = restore
            .call(&mut self.store, (ptr as i32, bytes.len() as i32))
            .map_err(|err| format!("ui wasm restore_state failed: {err}"))?;
        Ok(accepted != 0)
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
            pending_keyboard_captures: Vec::new(),
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
                    placement: module.placement.clone(),
                    requested_capabilities: module.capabilities.clone(),
                    ui_input_file: module.ui_input_file.clone(),
                    image_file: module.image_file.clone(),
                    graphics_roots: module.graphics_roots.clone(),
                    max_layers_per_pane: module.max_layers_per_pane,
                    host_actions: module.host_actions.clone(),
                    host_action_mode: module.host_action_mode.unwrap_or_default(),
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

    /// fork(wave-2): collect the opt-in guest state to carry across a live
    /// handoff. Guests that do not implement the exposure contribute nothing, so
    /// this is empty on an unmodified plugin set.
    pub(crate) fn save_guest_state(&mut self) -> Vec<UiPluginGuestState> {
        match self.runtime.as_mut() {
            Some(runtime) => runtime.save_guest_state(),
            None => Vec::new(),
        }
    }

    /// Push carried state back into freshly instantiated guests, then re-read the
    /// static update so a guest whose static view depends on its state is not
    /// cached at its default for a frame. A guest that declines or errors keeps
    /// its fresh state — cosmetic state is never worth dropping a plugin over.
    pub(crate) fn restore_guest_state(&mut self, states: &[UiPluginGuestState]) -> usize {
        let mut restored = 0;
        for state in states {
            let Some(module) = self
                .modules
                .iter()
                .find(|module| {
                    module.plugin_id == state.plugin_id && module.module_id == state.module_id
                })
                .cloned()
            else {
                continue;
            };
            let Some(runtime) = self.runtime.as_mut() else {
                return restored;
            };
            if !runtime.restore_guest_state(&module, &state.state) {
                continue;
            }
            restored += 1;
            let key = module_key(&module);
            match self.load_module_static(&module) {
                Ok((update, _negotiated)) => {
                    self.loaded_updates.insert(key, update);
                }
                Err(err) => tracing::warn!(
                    plugin_id = %module.plugin_id,
                    module_id = %module.module_id,
                    error = %err,
                    "ui plugin static update failed after handoff state restore"
                ),
            }
        }
        if restored > 0 {
            tracing::info!(restored, "restored ui plugin guest state after handoff");
        }
        restored
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

    /// fork(host-actions): resolve every module's manifest `host_actions` into
    /// the claim table `AppState` consults on each click.
    ///
    /// Two filters, both deliberate:
    ///
    /// - A module that names controls without holding the `host_actions`
    ///   capability contributes nothing. The capability is the user-visible
    ///   grant; the id list is only which controls it applies to.
    /// - An id that does not resolve to a real [`HostAction`] is dropped with a
    ///   warning rather than kept. A claim that can never fire would otherwise
    ///   sit in the table looking live, which is precisely the class of silent
    ///   failure this codebase refuses elsewhere.
    ///
    /// First claimant wins on a contested control, in the registration order
    /// `sync_to_registry` already fixes (plugin_id asc, then manifest order), so
    /// the winner is deterministic rather than HashMap-order-dependent. The
    /// loser is warned about — two plugins fighting over the new-tab button is a
    /// configuration mistake the user needs told about, not resolved silently.
    pub(crate) fn host_action_claims(&self) -> crate::ui_host_actions::HostActionClaims {
        use crate::api::schema::{PluginHostActionMode, PluginUiCapability};
        use crate::ui_host_actions::{HostAction, HostActionClaim, HostActionMode};

        let mut claims = crate::ui_host_actions::HostActionClaims::default();
        for module in &self.modules {
            if module.host_actions.is_empty() {
                continue;
            }
            if !module.requests(PluginUiCapability::HostActions) {
                tracing::warn!(
                    plugin_id = %module.plugin_id,
                    module_id = %module.module_id,
                    "ui module declares host_actions without the host_actions capability; ignoring"
                );
                continue;
            }
            let mode = match module.host_action_mode {
                PluginHostActionMode::Observe => HostActionMode::Observe,
                PluginHostActionMode::Replace => HostActionMode::Replace,
            };
            for declared in &module.host_actions {
                let Some(action) = HostAction::from_id(declared) else {
                    tracing::warn!(
                        plugin_id = %module.plugin_id,
                        module_id = %module.module_id,
                        action_id = %declared,
                        "unknown host action id in manifest host_actions; ignoring"
                    );
                    continue;
                };
                match claims.entry(action.id()) {
                    std::collections::hash_map::Entry::Occupied(existing) => {
                        tracing::warn!(
                            plugin_id = %module.plugin_id,
                            module_id = %module.module_id,
                            action_id = %action.id(),
                            held_by = %existing.get().plugin_id,
                            "host action already claimed by another module; ignoring"
                        );
                    }
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(HostActionClaim {
                            plugin_id: module.plugin_id.clone(),
                            module_id: module.module_id.clone(),
                            mode,
                        });
                    }
                }
            }
        }
        claims
    }

    /// fork(host-actions): deliver a claimed control's activation to its owner.
    pub(crate) fn dispatch_host_action(
        &mut self,
        plugin_id: &str,
        module_id: &str,
        action_id: &'static str,
        mode: crate::ui_host_actions::HostActionMode,
    ) -> bool {
        let event = UiEvent::HostAction {
            action_id: action_id.to_string(),
            mode: match mode {
                crate::ui_host_actions::HostActionMode::Observe => {
                    herdr_ui_plugin_api::UiHostActionMode::Observe
                }
                crate::ui_host_actions::HostActionMode::Replace => {
                    herdr_ui_plugin_api::UiHostActionMode::Replace
                }
            },
        };
        self.dispatch_event_for_module(plugin_id, module_id, &event)
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
                // fork(wave-2): the manifest's `surfaces` list is the gate — a
                // guest layer on an undeclared surface never renders, so a
                // hot-reloaded surfaces edit takes visual effect immediately.
                .filter(|layer| {
                    module
                        .surfaces
                        .iter()
                        .any(|declared| map_surface(*declared) == layer.surface)
                })
                .cloned()
            {
                layers.push(UiSurfaceLayer {
                    plugin_id: module.plugin_id.clone(),
                    module_id: module.module_id.clone(),
                    negotiated_version: module.negotiated_version,
                    placement: module.placement.clone(),
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

    /// Modules that declare a host-consumed `image_file`, resolved to
    /// `(plugin_id, module_id, surface, absolute_path)`. The surface is the
    /// module's first declared surface; the host paints the image over that
    /// surface's screen `Rect`. Only modules with a currently loaded update are
    /// returned, so a disabled/unloaded plugin paints nothing.
    pub(crate) fn surface_images(&self) -> Vec<(String, String, UiSurface, PathBuf)> {
        let mut out = Vec::new();
        for module in &self.modules {
            let Some(image_file) = module.image_file.as_ref() else {
                continue;
            };
            if !self.loaded_updates.contains_key(&module_key(module)) {
                continue;
            }
            let Some(surface) = module
                .surfaces
                .first()
                .map(|declared| map_surface(*declared))
            else {
                continue;
            };
            let Some(path) = module
                .manifest_path
                .parent()
                .map(|dir| dir.join(image_file))
            else {
                continue;
            };
            out.push((
                module.plugin_id.clone(),
                module.module_id.clone(),
                surface,
                path,
            ));
        }
        out
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
        // Move and scroll events have no declarative counterpart (only clicks
        // route through the hit-rect Action path), so the dedupe below is scoped
        // to button events. A MouseAll module therefore still receives hover
        // `Move` AND wheel scroll over its own hit-rects — hover is required for
        // row tooltips, and scroll must reach the guest even when the cursor sits
        // over a full-width row hit-rect (otherwise scrolling stalls the moment
        // the pointer is over a row, e.g. historial-dash / harness-manager).
        let is_move_or_scroll = matches!(
            kind,
            MouseEventKind::Move | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        );
        let mut changed = false;
        for (plugin_id, module_id) in keys {
            // fork(f5-mouse-dedupe): a module whose current layers declare an
            // absolute hit-rect over this cell already receives the click through
            // the hit-rect resolution path (`ui::ui_plugin_hit_at` ->
            // pending_plugin_action_invocation), so suppress the duplicate raw
            // MouseAll broadcast for cells inside its own hit-rects. The raw
            // stream continues for every other cell. Kept general so v3 Action
            // routing can reuse `should_deliver_raw_mouse`.
            if !is_move_or_scroll
                && !self.should_deliver_raw_mouse(&plugin_id, &module_id, mouse.column, mouse.row)
            {
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
            || !self.pending_keyboard_captures.is_empty()
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

    /// fork(host-actions): drain capture requests in emission order (last one
    /// wins at the applier).
    pub(crate) fn take_pending_keyboard_captures(&mut self) -> Vec<(String, String, bool)> {
        std::mem::take(&mut self.pending_keyboard_captures)
    }

    /// fork(host-actions): deliver one key press to ONE module — the capture
    /// path. The broadcast path (`dispatch_crossterm_key_event`) stays
    /// untouched; while capture is held the host calls this instead and skips
    /// its own key handling entirely.
    pub(crate) fn dispatch_key_to_module(
        &mut self,
        plugin_id: &str,
        module_id: &str,
        key: crossterm::event::KeyEvent,
    ) -> bool {
        let Some(event) = map_crossterm_key_event(key) else {
            return false;
        };
        self.dispatch_event_for_module(plugin_id, module_id, &UiEvent::Key(event))
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

    /// fork: every `graphics_roots` entry declared by any still-registered module
    /// of `plugin_id`, as the allowlist for a plugin-owned
    /// `pane.graphics.set { source_path }`.
    ///
    /// Unioned across the plugin's modules on purpose: the layer `owner` tag is
    /// `plugin:<plugin_id>`, not per-module, because `plugin disable` is the
    /// off-switch and it operates on whole plugins. Scoping the roots more
    /// narrowly than the ownership they protect would be false precision — two
    /// modules of one plugin already share a state dir and a process identity.
    ///
    /// An unlinked plugin has no modules and so gets an empty list, which the
    /// caller treats as a refusal.
    /// fork(+127): is a guest instance live for this module right now?
    ///
    /// Distinguishes "registered but never successfully loaded" from "loaded",
    /// which combined with `last_error_for` splits the three states that used to
    /// be indistinguishable from outside the process.
    pub(crate) fn is_module_loaded(&self, plugin_id: &str, module_id: &str) -> bool {
        self.runtime
            .as_ref()
            .is_some_and(|runtime| runtime.has_instance(plugin_id, module_id))
    }

    /// fork(+127): the module's last load / hot-reload / call error, if the most
    /// recent attempt failed.
    /// Linear-memory size of the loaded guest, if it is loaded.
    pub(crate) fn memory_bytes_for(&self, plugin_id: &str, module_id: &str) -> Option<usize> {
        self.runtime
            .as_ref()
            .and_then(|runtime| runtime.instance_memory_bytes(plugin_id, module_id))
    }

    pub(crate) fn last_error_for(&self, plugin_id: &str, module_id: &str) -> Option<&str> {
        self.last_errors
            .get(&(plugin_id.to_string(), module_id.to_string()))
            .map(String::as_str)
    }

    pub(crate) fn graphics_roots_for_plugin(&self, plugin_id: &str) -> Vec<String> {
        let mut roots: Vec<String> = self
            .modules
            .iter()
            .filter(|module| module.plugin_id == plugin_id)
            .flat_map(|module| module.graphics_roots.iter().cloned())
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// fork(+127): the plugin's declared per-pane graphics layer ceiling.
    ///
    /// Plugin-scoped for the same reason `graphics_roots_for_plugin` is: the
    /// off-switch (`plugin disable`) operates on whole plugins, and layer
    /// ownership is stamped with the plugin id, not the module id. When two
    /// modules of one plugin declare different values the LOWEST wins — a
    /// declaration is a promise about resource use, and the strictest promise a
    /// plugin made is the one it should be held to.
    pub(crate) fn max_layers_per_pane_for_plugin(&self, plugin_id: &str) -> Option<usize> {
        self.modules
            .iter()
            .filter(|module| module.plugin_id == plugin_id)
            .filter_map(|module| module.max_layers_per_pane)
            .min()
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
        let had_effects = !update.effects.is_empty();
        let (api_effects, other_effects): (Vec<HostEffect>, Vec<HostEffect>) = update
            .effects
            .iter()
            .cloned()
            .partition(|effect| matches!(effect, HostEffect::Api { .. }));
        // fork(host-actions): peel capture requests off before the identity-less
        // filter. Capability-gated here, where the module is still in hand; an
        // ungranted request degrades to a toast so the author learns why their
        // form is typing into the pane instead of silently misbehaving.
        let mut other_effects = other_effects;
        other_effects.retain(|effect| {
            let HostEffect::SetKeyboardCapture { captured } = effect else {
                return true;
            };
            if module.requests(crate::api::schema::PluginUiCapability::KeyboardCapture) {
                self.pending_keyboard_captures.push((
                    module.plugin_id.clone(),
                    module.module_id.clone(),
                    *captured,
                ));
            } else {
                self.pending_effects.push(HostEffect::ShowToast {
                    message: format!(
                        "Plugin {} requested keyboard capture without the keyboard_capture capability",
                        module.plugin_id
                    ),
                });
            }
            false
        });
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
        // Signal a change only when the module's layers actually differ from
        // what is already loaded (or it emitted effects). Returning `true`
        // unconditionally would force a full state refresh on every event a
        // module accepts — including raw mouse `Move`/click broadcasts to a
        // `mouse_all` module that re-rendered identical content — needlessly
        // reprojecting `ui_surface_layers` each time.
        let layers_changed = self
            .loaded_updates
            .get(&key)
            .map(|old| old.layers != merged_layers)
            .unwrap_or(true);
        self.loaded_updates.insert(
            key,
            UiUpdate {
                layers: merged_layers,
                effects: Vec::new(),
                scope: update.scope,
                consumed_action_ids: Vec::new(),
            },
        );
        layers_changed || had_effects
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
        // fork(host-actions): gated on the capability, not on a version number.
        // The host only ever dispatches this to the module that claimed the
        // control, so the check here is a backstop against a claim outliving the
        // capability that granted it (a manifest hot-reload dropping the grant
        // while the click was already queued).
        UiEvent::HostAction { .. } => {
            module.requests(crate::api::schema::PluginUiCapability::HostActions)
        }
    }
}

fn filter_effects_for_module(
    module: &RegisteredUiModule,
    effects: &[HostEffect],
) -> Vec<HostEffect> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            HostEffect::CopyText { .. } => module
                .requests(crate::api::schema::PluginUiCapability::ClipboardWrite)
                .then(|| effect.clone()),
            HostEffect::SendPaneKeys { .. } => module
                .requests(crate::api::schema::PluginUiCapability::PaneInput)
                .then(|| effect.clone()),
            HostEffect::ShowToast { .. } => module
                .requests(crate::api::schema::PluginUiCapability::Toasts)
                .then(|| effect.clone()),
            HostEffect::OpenPluginPane { entrypoint, .. } => {
                if module.requests(crate::api::schema::PluginUiCapability::OpenPluginPanes) {
                    Some(effect.clone())
                } else {
                    Some(HostEffect::ShowToast {
                        message: format!(
                            "Plugin {} cannot open pane '{}': capability not granted",
                            module.plugin_id, entrypoint
                        ),
                    })
                }
            }
            HostEffect::InvokePluginAction { .. } => module
                .requests(crate::api::schema::PluginUiCapability::PluginActions)
                .then(|| effect.clone()),
            // §7.2 own-surface guard: a module may only collapse/expand
            // surfaces it declares in its manifest — no capability required.
            HostEffect::SetSurfaceCollapsed { surface, .. } => module
                .surfaces
                .iter()
                .any(|declared| map_surface(*declared) == *surface)
                .then(|| effect.clone()),
            // Api effects are never routed through this generic capability
            // filter: `dispatch_event_for_module` extracts and queues them
            // separately (via `queue_api_effects`) before this function ever
            // sees `other_effects`, so this arm exists only for exhaustiveness.
            HostEffect::Api { .. } => None,
            // fork(host-actions): like `Api`, capture requests need the emitting
            // module's identity, which this identity-less filter cannot carry.
            // `dispatch_event_for_module` extracts them into
            // `pending_keyboard_captures` first; this arm is exhaustiveness only.
            HostEffect::SetKeyboardCapture { .. } => None,
        })
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
        PluginUiSurface::Panel => UiSurface::Panel,
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
        crate::api::schema::PluginUiCapability::PaneGraphics => UiCapability::PaneGraphics,
        crate::api::schema::PluginUiCapability::PluginAdmin => UiCapability::PluginAdmin,
        crate::api::schema::PluginUiCapability::HostActions => UiCapability::HostActions,
        crate::api::schema::PluginUiCapability::KeyboardCapture => UiCapability::KeyboardCapture,
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
    fn denied_open_plugin_pane_surfaces_a_toast() {
        let mut module = sample_plugin().ui_modules[0].clone();
        module
            .capabilities
            .retain(|capability| *capability != PluginUiCapability::OpenPluginPanes);
        let registered = RegisteredUiModule {
            plugin_id: "example.demo".into(),
            module_id: "demo".into(),
            title: "Demo".into(),
            module_path: std::path::PathBuf::from("demo.wasm"),
            manifest_path: std::path::PathBuf::from("herdr-plugin.toml"),
            api_version: 3,
            negotiated_version: 3,
            hot_reload: PluginHotReloadMode::On,
            surfaces: module.surfaces.clone(),
            placement: None,
            requested_capabilities: module.capabilities,
            ui_input_file: None,
            image_file: None,
            graphics_roots: Vec::new(),
            max_layers_per_pane: None,
            host_actions: Vec::new(),
            host_action_mode: Default::default(),
        };
        let effects = filter_effects_for_module(
            &registered,
            &[HostEffect::OpenPluginPane {
                plugin_id: "example.demo".into(),
                entrypoint: "update".into(),
                payload_json: None,
            }],
        );
        assert!(matches!(
            effects.as_slice(),
            [HostEffect::ShowToast { message }] if message.contains("capability not granted")
        ));
    }

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
            placement: None,
            requested_capabilities: Vec::new(),
            ui_input_file: None,
            image_file: None,
            graphics_roots: Vec::new(),
            max_layers_per_pane: None,
            host_actions: Vec::new(),
            host_action_mode: Default::default(),
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

    /// A module that implements the optional handoff state-carry exposure: it
    /// parks whatever `restore_state` hands it at offset 2048 and reads that
    /// same span back out of `save_state`. Enough to prove the host's round trip
    /// without depending on a real plugin's serialization.
    fn write_stateful_wasm(path: &std::path::Path, json: &str) {
        let data = json
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            r#"(module
                (memory (export "memory") 1)
                (data (i32.const 0) "{data}")
                (global $saved_len (mut i32) (i32.const 0))
                (func (export "herdr_ui_api_version") (result i32) i32.const 2)
                (func (export "herdr_ui_static_update_ptr") (result i32) i32.const 0)
                (func (export "herdr_ui_static_update_len") (result i32) i32.const {len})
                (func (export "herdr_ui_alloc") (param i32) (result i32) i32.const 1024)
                (func (export "herdr_ui_save_state_ptr") (result i32) i32.const 2048)
                (func (export "herdr_ui_save_state_len") (result i32) global.get $saved_len)
                (func (export "herdr_ui_restore_state") (param i32 i32) (result i32)
                    (memory.copy (i32.const 2048) (local.get 0) (local.get 1))
                    (global.set $saved_len (local.get 1))
                    (i32.const 1))
            )"#,
            len = json.len()
        );
        let wasm = wat::parse_str(&wat).expect("stateful fixture wat parses");
        std::fs::write(path, wasm).expect("write stateful fixture");
    }

    #[test]
    fn guest_state_round_trips_only_for_modules_that_opt_in() {
        // Opted-out module (the plain fixture): contributes nothing, which is what
        // keeps this exposure from being a flag day.
        let (plain_root, plain) = plugin_on_disk("Plain");
        let mut manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plain.plugin_id.clone(),
            plain.clone(),
        )]));
        assert!(
            manager.save_guest_state().is_empty(),
            "a guest without the state exports must carry nothing"
        );
        let _ = std::fs::remove_dir_all(plain_root);

        // Opted-in module: nothing to save before anything is stored, then the
        // exact bytes the host pushed in come back out.
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(root.join("dist")).expect("create fixture dir");
        write_stateful_wasm(
            &root.join("dist/goal-ui.wasm"),
            r#"{"scope":"partial","layers":[]}"#,
        );
        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        plugin.ui_modules[0].surfaces = vec![PluginUiSurface::Header];
        let registry = HashMap::from([(plugin.plugin_id.clone(), plugin.clone())]);
        let mut manager = UiPluginManager::from_installed_plugins(&registry);

        assert!(
            manager.save_guest_state().is_empty(),
            "a guest with nothing stored yet reports no state"
        );

        let carried = vec![UiPluginGuestState {
            plugin_id: plugin.plugin_id.clone(),
            module_id: plugin.ui_modules[0].id.clone(),
            state: b"{\"v\":1,\"counter\":7}".to_vec(),
        }];
        assert_eq!(manager.restore_guest_state(&carried), 1);

        let saved = manager.save_guest_state();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].plugin_id, plugin.plugin_id);
        assert_eq!(saved[0].state, carried[0].state);

        // An entry for a module that is not registered is ignored, not fatal —
        // the exporting server's plugin set need not match the importer's.
        let stranger = vec![UiPluginGuestState {
            plugin_id: "example.not-installed".into(),
            module_id: "nope".into(),
            state: b"{}".to_vec(),
        }];
        assert_eq!(manager.restore_guest_state(&stranger), 0);

        let _ = std::fs::remove_dir_all(root);
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
            startup: Vec::new(),
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
                placement: None,
                capabilities: vec![
                    PluginUiCapability::FocusedSession,
                    PluginUiCapability::MouseAll,
                ],
                ui_input_file: None,
                image_file: None,
                graphics_roots: Vec::new(),
                max_layers_per_pane: None,
                collector: None,
                host_actions: Vec::new(),
                host_action_mode: None,
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
        // Move events are always delivered (they have no hit-rect Action
        // counterpart to dedupe against), including over the module's own
        // declared hit-rect — hover tooltips depend on this.
        assert!(manager.dispatch_crossterm_mouse_event(moved(12, 0)));
        assert_eq!(header_text(&manager), "1");
        // Outside the hit-rect: also delivered, counter advances again.
        assert!(manager.dispatch_crossterm_mouse_event(moved(0, 0)));
        assert_eq!(header_text(&manager), "2");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn raw_mouse_broadcast_delivers_scroll_over_declared_hit_rects() {
        // fork(f5-mouse-dedupe) R5: wheel scroll must reach a MouseAll module even
        // when the cursor sits over one of the module's own full-width row
        // hit-rects. Only button clicks are deduped against the Action path;
        // scroll (like Move) has no Action counterpart, so suppressing it stalled
        // scrolling the instant the pointer was over a row (historial-dash /
        // harness-manager). This locks in that scroll is always delivered.
        let root = unique_root("f5-scroll-over-hit");
        write_counter_wasm(
            &root.join("dist/goal-ui.wasm"),
            r#"{"layers":[{"surface":"header","hits":[{"x":10,"y":0,"width":5,"height":1,"action_id":"a"}],"nodes":[{"kind":"line","spans":[{"text":"@"}]}]}]}"#,
        );
        let mut manager = installed(plugin_at(&root));

        let scroll = |kind: crossterm::event::MouseEventKind, col: u16, row: u16| {
            crossterm::event::MouseEvent {
                kind,
                column: col,
                row,
                modifiers: crossterm::event::KeyModifiers::empty(),
            }
        };
        use crossterm::event::MouseEventKind::{ScrollDown, ScrollUp};

        assert_eq!(header_text(&manager), "0");
        // Scroll INSIDE the declared hit-rect (the bug case): still delivered.
        assert!(manager.dispatch_crossterm_mouse_event(scroll(ScrollDown, 12, 0)));
        assert_eq!(header_text(&manager), "1");
        assert!(manager.dispatch_crossterm_mouse_event(scroll(ScrollUp, 12, 0)));
        assert_eq!(header_text(&manager), "2");
        // Scroll outside the hit-rect: delivered too.
        assert!(manager.dispatch_crossterm_mouse_event(scroll(ScrollDown, 0, 0)));
        assert_eq!(header_text(&manager), "3");

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
        assert_eq!(
            manager.all()[0].ui_input_file.as_deref(),
            Some("input.json")
        );
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

    #[test]
    fn surface_layers_gated_by_declared_manifest_surfaces() {
        let root = std::env::temp_dir().join(format!(
            "herdr-ui-plugin-surface-gate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("dist")).unwrap();
        // The guest emits header + footer layers, but the manifest only
        // declares header: the footer layer must never render.
        let json = r#"{"layers":[{"surface":"header","nodes":[{"kind":"line","spans":[{"text":"H","role":"accent"}]}]},{"surface":"footer","nodes":[{"kind":"line","spans":[{"text":"F","role":"accent"}]}]}]}"#;
        write_static_wasm(&root.join("dist/goal-ui.wasm"), json);

        let mut plugin = sample_plugin();
        plugin.plugin_root = root.display().to_string();
        plugin.manifest_path = root.join("herdr-plugin.toml").display().to_string();
        plugin.ui_modules[0].surfaces = vec![PluginUiSurface::Header];
        let mut manager = UiPluginManager::from_installed_plugins(&HashMap::from([(
            plugin.plugin_id.clone(),
            plugin.clone(),
        )]));
        let layers = manager.surface_layers();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].layer.surface, UiSurface::Header);

        // A re-synced surfaces edit (manifest hot-reload path) takes effect on
        // the kept module without reloading the guest.
        plugin.ui_modules[0].surfaces = vec![PluginUiSurface::Footer];
        let registry = HashMap::from([(plugin.plugin_id.clone(), plugin)]);
        manager.sync_to_registry(&registry);
        let layers = manager.surface_layers();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].layer.surface, UiSurface::Footer);

        let _ = std::fs::remove_dir_all(root);
    }
}
