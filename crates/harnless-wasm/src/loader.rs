//! The plugin loader: one fiber per plugin, reversible registrations,
//! generation-swapping reload, transactional failure.
//!
//! [`WasmPluginManager`] owns the mounted tree. Each mount:
//!
//! 1. compiles the component and runs its `descriptor()` (the *validation*
//!    step — a bad component or descriptor fails before anything is
//!    touched);
//! 2. opens a fiber (the plugin's own lifecycle owner);
//! 3. registers each declared tool on the [`Tools`] seam (`ctx.tools`) and
//!    records the names in a fiber-owned [`Registration`] handle, so
//!    unmounting unwinds exactly the plugin's registrations;
//! 4. records the generation.
//!
//! * **Remove** (`unmount`) disposes the fiber → the plugin's tools
//!   disappear, and only those tools.
//! * **Reload** (`reload`) mounts the new generation first and then unwinds
//!   the old one, so the tool set *swaps* instead of accumulating
//!   duplicates.
//! * **Transactional reload** (`reload_all`): every config is validated
//!   *before* any live state is touched; a failure leaves the last good
//!   tree fully mounted.
//!
//! Tool bodies execute the guest through [`LiveInstance`] under the
//! guarded pipeline: the registry runs pre-execute policy, approval
//! (fail-closed), guards, and the frozen-result notification around the
//! guest call exactly as for a native tool.

use std::sync::Arc;

use harnless_agent::tools::ToolRegistry;
use harnless_runtime::context::Context;
use harnless_runtime::fiber::{Fiber, FiberState};
use harnless_seams::error::{ErrorCode, SeamError};
use harnless_seams::tools::{ToolBody, ToolDefinition, Tools};
use harnless_seams::CallId;
use parking_lot::Mutex;
use serde_json::Value;
use wasmtime::component::Component;

use crate::abi::{Descriptor, PluginConfig};
use crate::engine::{build_engine, LiveInstance};

/// The tool names one plugin generation registered, in registration order.
pub type RegisteredTools = Arc<Mutex<Vec<String>>>;

/// A tool body that crosses into the guest: `call_<tool>(raw-json)`.
struct GuestBody {
    live: Arc<Mutex<LiveInstance>>,
    export: String,
    fuel: u64,
}

impl ToolBody for GuestBody {
    fn run(&self, _call_id: CallId, args: &[u8]) -> harnless_seams::Result<Value> {
        let input = std::str::from_utf8(args)
            .map_err(|e| SeamError::new(ErrorCode::ToolPanicked, format!("non-utf8 args: {e}")))?;
        let out = self
            .live
            .lock()
            .call_string_fn(&self.export, input, self.fuel)?;
        serde_json::from_str(&out).map_err(|e| {
            SeamError::new(
                ErrorCode::ToolPanicked,
                format!("guest result is not valid JSON: {e}"),
            )
        })
    }
}

/// The reversible tool registrations one plugin owns.
///
/// Held inside the plugin's fiber: when the fiber unloads, the handle drops
/// and removes exactly this plugin's tools from the registry — the
/// mechanical guarantee behind "removing the plugin by editing config
/// unwinds its registrations".
pub struct Registration {
    registry: Arc<ToolRegistry>,
    names: RegisteredTools,
}

impl Drop for Registration {
    fn drop(&mut self) {
        for name in self.names.lock().iter() {
            self.registry.remove(name);
        }
    }
}

/// One mounted WASM plugin: its fiber, generation, and live instance.
pub struct MountedPlugin {
    /// The plugin's own fiber — unloading it unwinds exactly its tools.
    pub fiber: Arc<Fiber>,
    /// The mount generation (starts at 1, +1 per reload).
    pub generation: u64,
    /// The live guest instance (shared by every tool body of this plugin).
    pub live: Arc<Mutex<LiveInstance>>,
    /// The config this generation mounted with.
    pub config: PluginConfig,
    /// The registered tool names.
    pub names: RegisteredTools,
}

/// The WASM plugin manager: owns the shared engine and the mounted tree.
#[derive(Clone)]
pub struct WasmPluginManager {
    engine: wasmtime::Engine,
    mounted: Arc<Mutex<Vec<MountedPlugin>>>,
}

impl Default for WasmPluginManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmPluginManager {
    /// Create a manager with a fresh shared engine.
    pub fn new() -> Self {
        Self {
            engine: build_engine().expect("wasmtime engine"),
            mounted: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The shared engine (tests compile fixture components against it).
    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// Compile a component from `config.component` and run its
    /// `descriptor()` in a throwaway sandboxed instance.
    ///
    /// This is the validation step transactional reload runs before
    /// touching the live tree: a bad path, bad binary, missing
    /// `descriptor` export, or malformed descriptor fails here.
    pub fn prepare(&self, config: &PluginConfig) -> Result<(Component, Descriptor), String> {
        let bytes = std::fs::read(&config.component)
            .map_err(|e| format!("plugin {}: cannot read component: {e}", config.id))?;
        let component = Component::new(&self.engine, &bytes)
            .map_err(|e| format!("plugin {}: component did not compile: {e}", config.id))?;
        let mut scratch = LiveInstance::new(
            &self.engine,
            component.clone(),
            config,
            Arc::new(Mutex::new(Vec::new())),
        )
        .map_err(|e| format!("plugin {}: instantiate failed: {e}", config.id))?;
        let raw = scratch
            .call_string_fn("descriptor", "", config.fuel_per_call)
            .map_err(|e| format!("plugin {}: descriptor() failed: {e:?}", config.id))?;
        let descriptor: Descriptor = serde_json::from_str(&raw)
            .map_err(|e| format!("plugin {}: descriptor is not valid: {e}", config.id))?;
        Ok((component, descriptor))
    }

    /// Mount one plugin on `ctx` (which must provide the spine's
    /// [`ToolRegistry`]): new fiber, tools registered inside it.
    ///
    /// Returns the generation. The registrations unwind when the plugin is
    /// [`Self::unmount`]ed.
    pub fn mount(&self, ctx: &Context, config: &PluginConfig) -> Result<u64, String> {
        let registry: Arc<ToolRegistry> = ctx
            .get::<ToolRegistry>()
            .ok_or_else(|| format!("plugin {}: no ToolRegistry service on context", config.id))?;
        self.mount_inner(ctx, &registry, config)
    }

    fn mount_inner(
        &self,
        ctx: &Context,
        registry: &Arc<ToolRegistry>,
        config: &PluginConfig,
    ) -> Result<u64, String> {
        let (component, descriptor) = self.prepare(config)?;
        let fiber = Fiber::pending();
        let plugin_ctx = ctx.extend();
        plugin_ctx.set_fiber(fiber.clone());

        let live = Arc::new(Mutex::new(
            LiveInstance::new(&self.engine, component, config, Arc::new(Mutex::new(Vec::new())))
                .map_err(|e| format!("plugin {}: instantiate failed: {e}", config.id))?,
        ));

        // Register each declared tool through the seam (the same
        // `ctx.tools` surface a native tool uses). Names are recorded so
        // the fiber-owned Registration handle removes exactly these.
        let names: RegisteredTools = Arc::new(Mutex::new(Vec::new()));
        for spec in &descriptor.tools {
            let name = format!("{}.{}", descriptor.name, spec.name);
            let def = ToolDefinition {
                name: name.clone(),
                schema: spec.schema.clone(),
                serialized: spec.serialized,
            };
            let body: Arc<dyn ToolBody> = Arc::new(GuestBody {
                live: live.clone(),
                export: format!("call_{}", spec.name),
                fuel: config.fuel_per_call,
            });
            registry
                .register(def, body)
                .map_err(|e| format!("plugin {}: register {name}: {e:?}", config.id))?;
            names.lock().push(name);
        }

        // The reversible handle lives inside the plugin fiber: fiber
        // teardown drops it, and its Drop removes the tools.
        let registration = Registration {
            registry: registry.clone(),
            names: names.clone(),
        };
        if let Err(err) = plugin_ctx.provide(registration) {
            // Roll the registrations back before surfacing the failure.
            for name in names.lock().iter() {
                registry.remove(name);
            }
            return Err(format!("plugin {}: provide registration: {err}", config.id));
        }

        fiber.set_state(FiberState::Active);
        let generation = {
            let mut mounted = self.mounted.lock();
            let g = mounted
                .iter()
                .filter(|m| m.config.id == config.id)
                .map(|m| m.generation)
                .max()
                .unwrap_or(0)
                + 1;
            mounted.push(MountedPlugin {
                fiber: fiber.clone(),
                generation: g,
                live,
                config: config.clone(),
                names,
            });
            g
        };
        Ok(generation)
    }

    /// Unmount a plugin by id: dispose its fiber, unwinding exactly its
    /// registrations. Returns `false` when no such plugin is mounted.
    pub fn unmount(&self, id: &str) -> bool {
        let victim = {
            let mut mounted = self.mounted.lock();
            let pos = mounted.iter().rposition(|m| m.config.id == id);
            pos.map(|pos| mounted.remove(pos))
        };
        match victim {
            Some(plugin) => {
                plugin.fiber.dispose();
                true
            }
            None => false,
        }
    }

    /// Reload one plugin: mount the new generation first, then unwind the
    /// old one. The tool set swaps; it never accumulates duplicates.
    pub fn reload(&self, ctx: &Context, config: &PluginConfig) -> Result<u64, String> {
        let generation = self.mount(ctx, config)?;
        // Unwind every older generation of this id.
        let victims: Vec<MountedPlugin> = {
            let mut mounted = self.mounted.lock();
            let newest = mounted
                .iter()
                .rposition(|m| m.config.id == config.id)
                .unwrap_or(usize::MAX);
            let mut i = 0;
            let mut out = Vec::new();
            while i < mounted.len() {
                if i != newest && mounted[i].config.id == config.id {
                    out.push(mounted.remove(i));
                } else {
                    i += 1;
                }
            }
            out
        };
        for v in victims {
            v.fiber.dispose();
        }
        Ok(generation)
    }

    /// Transactional reload of the whole tree: validate every config first;
    /// on any failure the last good tree stands untouched. Configs absent
    /// from the new list are unmounted (their registrations unwind).
    pub fn reload_all(&self, ctx: &Context, configs: &[PluginConfig]) -> Result<(), String> {
        // Phase 1: validate everything before touching the live tree.
        for cfg in configs {
            self.prepare(cfg)?;
        }
        // Phase 2: swap in the new generation of each config.
        for cfg in configs {
            self.reload(ctx, cfg)?;
        }
        // Phase 3: drop plugins removed from config entirely.
        let keep: Vec<&str> = configs.iter().map(|c| c.id.as_str()).collect();
        loop {
            let next = {
                let mounted = self.mounted.lock();
                mounted
                    .iter()
                    .find(|m| !keep.contains(&m.config.id.as_str()))
                    .map(|m| m.config.id.clone())
            };
            match next {
                Some(id) => {
                    self.unmount(&id);
                }
                None => break,
            }
        }
        Ok(())
    }

    /// The mounted plugin ids with generations.
    pub fn generations(&self) -> Vec<(String, u64)> {
        self.mounted
            .lock()
            .iter()
            .map(|m| (m.config.id.clone(), m.generation))
            .collect()
    }

    /// The host-recorded log lines for one mounted plugin.
    pub fn plugin_log(&self, id: &str) -> Option<Vec<String>> {
        let mounted = self.mounted.lock();
        let m = mounted.iter().rev().find(|m| m.config.id == id)?;
        let log = m.live.lock().logged();
        Some(log)
    }
}
