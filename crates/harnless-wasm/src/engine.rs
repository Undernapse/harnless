//! The wasmtime embedding: one fuel-bounded, capability-scoped instance per
//! plugin, with trap isolation.
//!
//! # Isolation model (measured against wasmtime 46)
//!
//! A wasm *trap* (guest `unreachable`, fuel exhaustion, an escaping host
//! error) poisons the component instance — wasmtime refuses re-entry with
//! "cannot enter component instance" — but **not** the engine or any other
//! store. The host therefore treats a trapping call as a structured tool
//! failure and revives the plugin with a fresh `Store` + instance for the
//! next call. The session never sees a panic; the plugin's own instance is
//! the only thing that died.
//!
//! # Capability model
//!
//! A store is built from [`PluginConfig`] alone:
//!
//! * `consume_fuel(true)` + a per-call budget: a spinning guest is stopped
//!   by the fuel trap, never by wall-clock luck.
//! * a `StoreLimitsBuilder` memory cap: the guest cannot balloon the host.
//! * WASI preview-2 is wired **only** when the config grants `fs`, and the
//!   only preopened directory is the host-provisioned scope at `/scope`.
//!   No env, no args, no network, no stdio inheritance — the guest starts
//!   from nothing and gets exactly what config grants.
//! * the host `log` import is wired **only** when the config grants `log`;
//!   a plugin that imports it without the grant fails to instantiate,
//!   which is the "denied unless granted" boundary.

use std::path::Path;
use std::sync::Arc;

use harnless_seams::error::{ErrorCode, SeamError};
use parking_lot::Mutex;
use wasmtime::component::{Component, Instance, Linker, Val};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::abi::PluginConfig;

/// The guest-visible mount point of a granted `fs` scope.
pub const GUEST_SCOPE: &str = "/scope";

/// The maximum guest memory one plugin instance may commit.
pub const MAX_GUEST_MEMORY: usize = 16 * 1024 * 1024;

/// Store state: WASI context (empty unless granted), the resource table,
/// and the plugin's recorded log lines.
pub struct PluginState {
    wasi: WasiCtx,
    table: wasmtime::component::ResourceTable,
    limits: StoreLimits,
    /// Host-recorded log lines (only filled when `log` is granted).
    pub logged: Arc<Mutex<Vec<String>>>,
}

impl WasiView for PluginState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Build the linker for a plugin config: WASI iff `fs` is granted, host
/// `log` iff `log` is granted. Nothing else is ever wired.
pub fn build_linker(
    engine: &Engine,
    config: &PluginConfig,
) -> wasmtime::Result<Linker<PluginState>> {
    let mut linker = Linker::<PluginState>::new(engine);
    if config.fs_dir().is_some() {
        wasmtime_wasi::p2::add_to_linker_sync::<PluginState>(&mut linker)?;
    }
    if config.can_log() {
        let mut inst = linker.instance(crate::fixture::HOST_INSTANCE)?;
        inst.func_wrap(
            "log",
            |mut cx: wasmtime::StoreContextMut<'_, PluginState>, (s,): (String,)| {
                cx.data_mut().logged.lock().push(s);
                Ok(())
            },
        )?;
    }
    Ok(linker)
}

/// Build a fresh store for `config`: no ambient env/args/stdio, memory
/// capped, fuel available, WASI scoped to the granted dir (if any).
pub fn build_store(
    engine: &Engine,
    config: &PluginConfig,
    logged: Arc<Mutex<Vec<String>>>,
) -> wasmtime::Result<Store<PluginState>> {
    let mut builder = WasiCtxBuilder::new();
    if let Some(dir) = config.fs_dir() {
        let (dir_perms, file_perms) = if config.fs_read_only() {
            (DirPerms::READ, FilePerms::READ)
        } else {
            (DirPerms::all(), FilePerms::all())
        };
        builder.preopened_dir(Path::new(dir), GUEST_SCOPE, dir_perms, file_perms)?;
    }
    // Deliberately: no `.env()`, no `.args()`, no inherited stdio. The guest
    // sees an empty environment and no network regardless of the host's.
    let wasi = builder.build();
    let state = PluginState {
        wasi,
        table: Default::default(),
        limits: StoreLimitsBuilder::new()
            .memory_size(MAX_GUEST_MEMORY)
            .build(),
        logged,
    };
    let mut store = Store::new(engine, state);
    store.limiter(|s| &mut s.limits);
    store.set_fuel(u64::MAX)?;
    Ok(store)
}

/// The engine every plugin in a manager shares: component model + fuel.
pub fn build_engine() -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    cfg.consume_fuel(true);
    Engine::new(&cfg)
}

/// A live plugin instance: store + component instance, revived on trap.
pub struct LiveInstance {
    engine: Engine,
    component: Component,
    config: PluginConfig,
    logged: Arc<Mutex<Vec<String>>>,
    linker: Linker<PluginState>,
    store: Store<PluginState>,
    instance: Instance,
}

impl LiveInstance {
    /// Instantiate `component` under `config`, wiring only granted caps.
    pub fn new(
        engine: &Engine,
        component: Component,
        config: &PluginConfig,
        logged: Arc<Mutex<Vec<String>>>,
    ) -> wasmtime::Result<Self> {
        let linker = build_linker(engine, config)?;
        let mut store = build_store(engine, config, logged.clone())?;
        let instance = linker.instantiate(&mut store, &component)?;
        Ok(Self {
            engine: engine.clone(),
            component,
            config: config.clone(),
            logged,
            linker,
            store,
            instance,
        })
    }

    /// Call an exported `(param string) -> string` function.
    ///
    /// A guest trap is mapped to a structured `ToolPanicked` seam error; the
    /// poisoned instance is replaced (fresh store + instance) so the next
    /// call runs clean — the isolation contract.
    pub fn call_string_fn(
        &mut self,
        name: &str,
        input: &str,
        fuel: u64,
    ) -> harnless_seams::Result<String> {
        self.store.set_fuel(fuel).map_err(seam_wrap)?;
        let func = self
            .instance
            .get_func(&mut self.store, name)
            .ok_or_else(|| {
                SeamError::new(ErrorCode::ToolNotFound, format!("export {name} missing"))
            })?;
        let mut results = [Val::String(String::new())];
        match func.call(&mut self.store, &[Val::String(input.to_string())], &mut results) {
            Ok(()) => match results.into_iter().next() {
                Some(Val::String(s)) => Ok(s),
                _ => Err(SeamError::new(
                    ErrorCode::ToolPanicked,
                    format!("export {name} returned a non-string result"),
                )),
            },
            Err(err) => {
                // The trap poisoned this instance. Revive it now so the
                // failure is scoped to this one call.
                if let Err(revive) = self.revive() {
                    return Err(seam_wrap(revive));
                }
                Err(seam_wrap(err))
            }
        }
    }

    /// Replace the poisoned store/instance with a fresh pair.
    fn revive(&mut self) -> wasmtime::Result<()> {
        let mut store = build_store(&self.engine, &self.config, self.logged.clone())?;
        let instance = self.linker.instantiate(&mut store, &self.component)?;
        self.store = store;
        self.instance = instance;
        Ok(())
    }

    /// The recorded host log lines.
    pub fn logged(&self) -> Vec<String> {
        self.logged.lock().clone()
    }
}

fn seam_wrap(err: wasmtime::Error) -> harnless_seams::SeamError {
    SeamError::new(ErrorCode::ToolPanicked, err.to_string())
}
