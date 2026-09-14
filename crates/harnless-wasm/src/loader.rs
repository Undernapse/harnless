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

/// A prepared mount that has not yet been committed to the live tree.
///
/// [`WasmPluginManager::stage`] produces one — component compiled, guest
/// instantiated under its grants, valid descriptor read — and
/// [`WasmPluginManager::commit`] is the only step that touches anything
/// outside this value: it registers the descriptor's tools on the seam and
/// touches the manager's mounted list. Splitting the mount this way is what
/// lets [`WasmPluginManager::reload_all`] prepare *every* config before any
/// live state changes: registering only at commitment means a config that
/// fails to stage never wrote to the registry at all, so the rollback cannot
/// reach the last-good tree.
pub struct StagedMount {
    fiber: Arc<Fiber>,
    live: Arc<Mutex<LiveInstance>>,
    descriptor: Descriptor,
    config: PluginConfig,
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
    /// The `(name, body)` pairs this mount registered, in registration order.
    /// A single vector so the name and body of one registration can never
    /// drift apart (a `zip` over two independently-locked vectors would
    /// silently drop the tail on any length mismatch).
    pub entries: Arc<Mutex<Vec<(String, Arc<dyn ToolBody>)>>>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        for (name, body) in self.entries.lock().iter() {
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

    /// Mount one plugin on `ctx`, registering its tools on the spine's
    /// [`ToolRegistry`] service: new fiber, tools registered inside it.
    ///
    /// Returns the generation. The registrations unwind when the plugin is
    /// [`Self::unmount`]ed.
    pub fn mount(&self, ctx: &Context, config: &PluginConfig) -> Result<u64, String> {
        let registry = self.registry_from(ctx, config)?;
        self.mount_on(&registry, config)
    }

    /// The [`ToolRegistry`] service `ctx` provides, or the error every
    /// context-driven mount reports when it is missing.
    fn registry_from(
        &self,
        ctx: &Context,
        config: &PluginConfig,
    ) -> Result<Arc<ToolRegistry>, String> {
        ctx.get::<ToolRegistry>()
            .ok_or_else(|| format!("plugin {}: no ToolRegistry service on context", config.id))
    }

    /// Mount one plugin on `ctx`, registering its tools on `registry` — the
    /// [`Tools`] seam the caller already holds, rather than one looked up from
    /// the context by type.
    ///
    /// This is the seam a spine that *owns* the registry (the agent harness
    /// holds `Arc<ToolRegistry>` as a field, not a context service keyed by
    /// `ToolRegistry`) mounts a plugin through: the guarded pipeline, approval,
    /// and recording wrap the guest call exactly as for a native tool, because
    /// it is the same registry the native tools registered on.
    pub fn mount_on(
        &self,
        registry: &Arc<ToolRegistry>,
        config: &PluginConfig,
    ) -> Result<u64, String> {
        self.mount_inner(registry, config)
    }

    fn mount_inner(
        &self,
        registry: &Arc<ToolRegistry>,
        config: &PluginConfig,
    ) -> Result<u64, String> {
        let staged = self.stage(config)?;
        Ok(self.commit(registry, &staged))
    }

    /// Prepare a mount without touching anything outside it: compile the
    /// component, instantiate the guest under its grants, and read its
    /// descriptor. Everything that can fail about a mount fails here;
    /// [`Self::commit`] cannot fail.
    ///
    /// This is the transactional primitive behind [`Self::reload_all`]: staging
    /// every config before committing any means a config that fails to stage
    /// leaves the last-good tree entirely untouched — it never wrote a tool
    /// name to the registry, so unwinding it has nothing to undo.
    pub fn stage(&self, config: &PluginConfig) -> Result<StagedMount, String> {
        let (component, descriptor) = self.prepare(config)?;
        let fiber = Fiber::pending();
        let live = Arc::new(Mutex::new(
            LiveInstance::new(
                &self.engine,
                component,
                config,
                Arc::new(Mutex::new(Vec::new())),
            )
            .map_err(|e| format!("plugin {}: instantiate failed: {e}", config.id))?,
        ));
        fiber.set_state(FiberState::Active);
        Ok(StagedMount {
            fiber,
            live,
            descriptor,
            config: config.clone(),
        })
    }

    /// Commit a staged mount to the live tree: register its tools on the seam,
    /// record its row, and hand the reversible handle's sole strong owner to
    /// the plugin's fiber.
    ///
    /// Registration happens *here*, not in [`Self::stage`], and that ordering is
    /// the transactional guarantee. Once `stage` succeeded — the component
    /// compiled, the guest instantiated under its grants, the descriptor
    /// parsed — registering its declared tools cannot fail: the only names that
    /// could collide are names this tree itself owns (a reload's own new
    /// generation, or another config in the same tree), and the registry's
    /// `register` overwrites those. So `commit` is infallible, which in turn
    /// means the rollback in [`Self::reload_all`] — dispose the staged fibers —
    /// structurally cannot touch a tool the live tree was serving: a staged
    /// mount that never committed registered nothing.
    fn commit(&self, registry: &Arc<ToolRegistry>, staged: &StagedMount) -> u64 {
        // Register each declared tool through the seam (the same
        // `ctx.tools` surface a native tool uses). Each `(name, body)` pair is
        // recorded so the fiber-owned Registration handle removes exactly these
        // registrations — and only those still owned by this mount.
        let entries: Arc<Mutex<Vec<(String, Arc<dyn ToolBody>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        for spec in &staged.descriptor.tools {
            let name = format!("{}.{}", staged.descriptor.name, spec.name);
            let def = ToolDefinition {
                name: name.clone(),
                schema: spec.schema.clone(),
                serialized: spec.serialized,
            };
            let body: Arc<dyn ToolBody> = Arc::new(GuestBody {
                live: staged.live.clone(),
                export: format!("call_{}", spec.name),
                fuel: staged.config.fuel_per_call,
            });
            registry
                .register(def, body.clone())
                .expect("a staged mount's tool registers");
            entries.lock().push((name, body));
        }
        let names: RegisteredTools = Arc::new(Mutex::new(
            entries.lock().iter().map(|(n, _)| n.clone()).collect(),
        ));

        // The reversible handle. Its *single* strong owner becomes an effect on
        // the plugin's fiber, so the mount's tools disappear exactly when that
        // fiber tears down — whether the manager unmounts the row or the fiber
        // is disposed on its own.
        let handle = Arc::new(Mutex::new(Registration {
            registry: registry.clone(),
            entries,
        }));
        self.record_mount(
            &staged.config,
            &staged.fiber,
            &names,
            Some(staged.live.clone()),
            handle,
        )
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
                Some(g) => mounted
                    .iter()
                    .rposition(|m| m.config.id == id && m.generation == g),
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

    /// Reload one plugin: mount the new generation first, then unwind every
    /// older one. The tool set swaps; it never accumulates duplicates and the
    /// new generation always stands mounted afterwards.
    pub fn reload(&self, ctx: &Context, config: &PluginConfig) -> Result<u64, String> {
        let registry = self.registry_from(ctx, config)?;
        self.reload_on(&registry, config)
    }

    /// Reload one plugin registering on a caller-held registry — the
    /// [`Self::mount_on`] counterpart of [`Self::reload`].
    pub fn reload_on(
        &self,
        registry: &Arc<ToolRegistry>,
        config: &PluginConfig,
    ) -> Result<u64, String> {
        let generation = self.commit(registry, &self.stage(config)?);
        self.retire_older(config);
        Ok(generation)
    }

    /// Retire every generation of `config`'s id except the newest, disposing
    /// their fibers so their registrations unwind. Partition by generation
    /// number, not list position: an in-place `remove` loop that re-tests the
    /// shifted element would also drop the just-mounted generation once its
    /// index slides below the scan cursor.
    fn retire_older(&self, config: &PluginConfig) {
        let victims: Vec<MountedPlugin> = {
            let mut mounted = self.mounted.lock();
            let newest = mounted
                .iter()
                .filter(|m| m.config.id == config.id)
                .map(|m| m.generation)
                .max()
                .unwrap_or(0);
            let mut keep = Vec::with_capacity(mounted.len());
            let mut out = Vec::new();
            for m in std::mem::take(&mut *mounted) {
                if m.config.id == config.id && m.generation != newest {
                    out.push(m);
                } else {
                    keep.push(m);
                }
            }
            *mounted = keep;
            out
        };
        for v in victims {
            v.fiber.dispose();
        }
    }

    /// Transactional reload of the whole tree.
    ///
    /// **Every** config is fully staged — compiled, instantiated under its
    /// grants, and validated against its descriptor — *before* any live state is
    /// touched. Nothing is registered during staging: a config that fails to
    /// stage never wrote a tool name to the registry, so the rollback (dispose
    /// the already-staged fibers) has nothing to undo and cannot reach a tool the
    /// last-good tree was serving. Only once every config has staged does the
    /// tree commit — each id's new generation registered and committed, each old
    /// generation retired, and ids dropped from config unmounted.
    ///
    /// Staging is stronger than [`Self::prepare`] validation: a config whose
    /// component compiles but whose guest fails to instantiate (e.g. an `fs`
    /// grant whose directory vanished between validation and mount) fails here,
    /// before the tree moves.
    pub fn reload_all(&self, ctx: &Context, configs: &[PluginConfig]) -> Result<(), String> {
        let registry = self.registry_from(
            ctx,
            configs
                .first()
                .ok_or_else(|| "no configs to reload".to_string())?,
        )?;
        // Phase 1: stage every config. On any failure, unwind the staged
        // mounts and leave the live tree exactly as it was.
        let mut staged = Vec::with_capacity(configs.len());
        for cfg in configs {
            match self.stage(cfg) {
                Ok(s) => staged.push(s),
                Err(e) => {
                    for s in staged {
                        s.fiber.dispose();
                    }
                    return Err(e);
                }
            }
        }
        // Phase 2: commit every staged mount, then retire the superseded
        // generation of each id. Commitment cannot fail, so the tree is never
        // observed half-swapped.
        for s in &staged {
            self.commit(&registry, s);
        }
        for cfg in configs {
            self.retire_older(cfg);
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

    /// The fibers of the currently mounted plugins, in mount order. A test or a
    /// caller observing teardown reads the fiber lifecycle directly: unmounting
    /// disposes the plugin's own fiber and no other.
    pub fn mounted_fibers(&self) -> Vec<Arc<Fiber>> {
        self.mounted
            .lock()
            .iter()
            .map(|m| m.fiber.clone())
            .collect()
    }
}
