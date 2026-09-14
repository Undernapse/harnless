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

use std::collections::HashMap;
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

/// Weak observers of the live mounts' registration handles, keyed by the
/// plugin fiber's identity.
///
/// The strong owner of a mount's [`Registration`] is a fiber effect, never the
/// manager: this map only reports which mounts are still reversible.
type Mounts = Arc<Mutex<HashMap<usize, std::sync::Weak<Mutex<Registration>>>>>;

/// Identity key for one plugin fiber.
fn fiber_key(fiber: &Arc<Fiber>) -> usize {
    Arc::as_ptr(fiber).addr()
}

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
/// Held by the plugin's fiber: when the fiber unloads, the handle drops and
/// removes exactly this plugin's tools from the registry — the mechanical
/// guarantee behind "removing the plugin by editing config unwinds its
/// registrations".
///
/// Unwind is **identity-checked**: a name is removed only while the registry
/// still holds *this* mount's body under it. [`WasmPluginManager::reload`]
/// registers the new generation's body under the same name before the old
/// generation is retired, so retiring the old one must not delete the live
/// one — that is what makes a reload a swap instead of a disappearance.
pub struct Registration {
    /// The registry the names were registered on.
    pub registry: Arc<ToolRegistry>,
    /// The registered names, in registration order.
    pub names: RegisteredTools,
    /// The bodies this mount registered, in the same order as [`Self::names`].
    pub bodies: Arc<Mutex<Vec<Arc<dyn ToolBody>>>>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        for (name, body) in self.names.lock().iter().zip(self.bodies.lock().iter()) {
            // `remove` reports whether the slot was occupied; the registry
            // entry is superseded when its body is not this mount's.
            if self.registry.body_is(name, body) {
                self.registry.remove(name);
            }
        }
    }
}

/// One mounted WASM plugin: its fiber, generation, and live instance.
pub struct MountedPlugin {
    /// The plugin's own fiber — unloading it unwinds exactly its tools.
    pub fiber: Arc<Fiber>,
    /// The mount generation (starts at 1, +1 per reload).
    pub generation: u64,
    /// The live guest instance (shared by every tool body of this plugin),
    /// or `None` for a stand-in mount recorded by [`Self::record_mount`].
    pub live: Option<Arc<Mutex<LiveInstance>>>,
    /// The config this generation mounted with.
    pub config: PluginConfig,
    /// The registered tool names.
    pub names: RegisteredTools,
    /// A weak observer of the mount's reversible handle. The handle's single
    /// strong owner is an effect on [`Self::fiber`], so this upgrade succeeds
    /// exactly while the mount is live: fiber teardown drops the handle, and
    /// the [`Registration`]'s `Drop` removes exactly this plugin's names.
    pub handle: std::sync::Weak<Mutex<Registration>>,
}

/// The WASM plugin manager: owns the shared engine and the mounted tree.
#[derive(Clone)]
pub struct WasmPluginManager {
    engine: wasmtime::Engine,
    mounted: Arc<Mutex<Vec<MountedPlugin>>>,
    /// Weak observers of the live mounts' handles, keyed by plugin fiber
    /// identity. A mount whose handle has dropped (its fiber tore down) stops
    /// being reported here, which is how the manager observes unwinds it did
    /// not itself trigger.
    handles: Mounts,
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
            handles: Arc::new(Mutex::new(HashMap::new())),
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
        // `ctx.tools` surface a native tool uses). Names *and* bodies are
        // recorded so the fiber-owned Registration handle removes exactly
        // these registrations — and only those still owned by this mount.
        let names: RegisteredTools = Arc::new(Mutex::new(Vec::new()));
        let bodies: Arc<Mutex<Vec<Arc<dyn ToolBody>>>> = Arc::new(Mutex::new(Vec::new()));
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
                .register(def, body.clone())
                .map_err(|e| format!("plugin {}: register {name}: {e:?}", config.id))?;
            names.lock().push(name);
            bodies.lock().push(body);
        }

        // The reversible handle. `record_mount` gives its *single* strong
        // owner to an effect on the plugin's fiber, so the mount's tools
        // disappear exactly when that fiber tears down — whether the manager
        // unmounts the row or the fiber is disposed on its own.
        let handle = Arc::new(Mutex::new(Registration {
            registry: registry.clone(),
            names: names.clone(),
            bodies,
        }));
        fiber.set_state(FiberState::Active);
        Ok(self.record_mount(config, &fiber, &names, Some(live), handle))
    }

    /// The bookkeeping half of a mount, for a mount that carries its own
    /// reversible [`Registration`] handle.
    ///
    /// [`Self::mount`] is this plus compiling the component and wiring the
    /// guest calls through [`LiveInstance`]. Splitting the two lets a test
    /// drive the mount / unmount / reload / generation guarantees against a
    /// guest body that is not a loadable component — the substitution
    /// `tests/loader.rs` documents (ISSUE-15). `live` is `None` exactly for
    /// such a stand-in mount.
    ///
    /// The handle's single strong owner becomes an effect on `fiber`, so the
    /// [`Registration`]'s `Drop` — which removes exactly its names — runs when
    /// the fiber tears down. [`Self::unmount`] and [`Self::reload`] drive that
    /// teardown; a fiber disposed by anyone else drives it too. The manager
    /// additionally keeps a weak observer so [`Self::mounted_handles`] can
    /// report what is live.
    pub fn record_mount(
        &self,
        config: &PluginConfig,
        fiber: &Arc<Fiber>,
        names: &RegisteredTools,
        live: Option<Arc<Mutex<LiveInstance>>>,
        handle: Arc<Mutex<Registration>>,
    ) -> u64 {
        let key = fiber_key(fiber);
        let detached = self.handles.clone();
        let observer = Arc::downgrade(&handle);
        // The handle's only strong owner lives in this effect: fiber teardown
        // drops it, the Registration's Drop removes the plugin's tools, and the
        // weak observer in the manager's map stops reporting the mount.
        let owned = Mutex::new(Some(handle.clone()));
        let reversible = fiber
            .effect(move || {
                let observer = observer.clone();
                detached.lock().insert(key, observer);
                Some(Box::new(move || {
                    drop(owned.lock().take());
                }) as harnless_runtime::fiber::DisposeFn)
            })
            .is_ok();
        debug_assert!(
            reversible,
            "plugin {}: mount recorded on a disposed fiber is not reversible",
            config.id
        );
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
            names: names.clone(),
            // The row observes the handle; the fiber's effect owns it.
            handle: Arc::downgrade(&handle),
        });
        g
    }

    /// Unmount a plugin by id: retire its newest row and dispose its fiber,
    /// unwinding exactly its registrations. Returns `false` when no such
    /// plugin is mounted.
    pub fn unmount(&self, id: &str) -> bool {
        self.unmount_generation(id, None)
    }

    /// Unmount one plugin *generation* by id: retire exactly that row and
    /// dispose its fiber, unwinding exactly that generation's registrations.
    /// `None` means the newest generation, like [`Self::unmount`].
    ///
    /// This is the primitive [`Self::reload`] uses to retire the superseded
    /// generation, and what a caller uses when a config edit replaces one
    /// generation while a newer one is already live.
    pub fn unmount_generation(&self, id: &str, generation: Option<u64>) -> bool {
        let victim = {
            let mut mounted = self.mounted.lock();
            let pos = match generation {
                Some(g) => mounted.iter().rposition(|m| m.config.id == id && m.generation == g),
                None => mounted.iter().rposition(|m| m.config.id == id),
            };
            pos.map(|pos| mounted.remove(pos))
        };
        match victim {
            Some(plugin) => {
                // The fiber's effect owns the registration handle: disposing
                // the fiber drops it, and the handle's Drop removes exactly
                // this plugin's tool names.
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
    ///
    /// A row leaves the tree when the plugin is unmounted or superseded by a
    /// reload, so this is the observable config-tree state: `unmount` and
    /// `reload` are what retire rows, and a row's tools unwind with its fiber.
    pub fn generations(&self) -> Vec<(String, u64)> {
        self.mounted
            .lock()
            .iter()
            .map(|m| (m.config.id.clone(), m.generation))
            .collect()
    }

    /// Whether a mount for `id` at generation `generation` is in the tree.
    pub fn is_mounted(&self, id: &str, generation: u64) -> bool {
        self.mounted
            .lock()
            .iter()
            .any(|m| m.config.id == id && m.generation == generation)
    }

    /// The host-recorded log lines for one mounted plugin.
    pub fn plugin_log(&self, id: &str) -> Option<Vec<String>> {
        let mounted = self.mounted.lock();
        let m = mounted.iter().rev().find(|m| m.config.id == id)?;
        let live = m.live.as_ref()?;
        let log = live.lock().logged();
        Some(log)
    }
}
