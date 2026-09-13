//! The tools-only bridge: plugin, registry handle, and shared runtime.
//!
//! One [`McpServerPlugin`] per configured server. Activation:
//!
//! 1. a duplicate server name fails the *later* plugin at load;
//! 2. discovery registers the first generation on `ctx.tools` **before the
//!    first turn** — activation is synchronous through the handshake and
//!    the first `tools/list`;
//! 3. a strict server whose handshake fails fails the load; a lenient one
//!    activates tool-less while the supervisor keeps trying;
//! 4. the reconnect supervisor owns the outage/recovery contract; unloading
//!    the plugin cancels it and unregisters the server's tools.
//!
//! Resources and prompts have no harness consumer and are deliberately not
//! bridged (tools-only, matching dsh).
//!
//! # Ownership model
//!
//! The [`Tools`] seam has no unregister, so the bridge keeps its own
//! dispatch table: every MCP public name resolves through
//! [`McpToolBridge::call`], and the entry registered on the underlying
//! registry is one thin forwarder per public name. Generation replacement
//! swaps the table (with full rollback on conflict); unregistering clears
//! the table entry — a disposed forwarder then resolves `tool-not-found`,
//! which is the observable unregister through the seam.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use harnless_runtime::context::Context;
use harnless_runtime::fiber::Fiber;
use harnless_runtime::plugin::Plugin;
use harnless_runtime::{Result as RuntimeResult, RuntimeError};

use harnless_seams::{CallId, ErrorCode, SeamError, ToolBody, ToolDefinition, Tools};

use crate::config::McpServerConfig;
use crate::projection::RichContentGate;
use crate::supervisor::{
    supervise, Generation, GenerationSink, NoopObserver, Registry, RmcpFactory, SupervisorObserver,
    TransportFactory,
};

/// The crate's shared multi-threaded runtime, created on first use.
///
/// `ToolBody::run` is synchronous by seam contract, and rmcp is async;
/// the bridge hops onto this runtime instead of forcing every consumer to
/// own one.
pub fn runtime() -> &'static Arc<Runtime> {
    static RT: std::sync::LazyLock<Arc<Runtime>> = std::sync::LazyLock::new(|| {
        Arc::new(
            Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("harnless-mcp")
                .build()
                .expect("harnless-mcp runtime"),
        )
    });
    &RT
}

/// The set of server names claimed by mounted MCP plugins.
///
/// A duplicate server name fails the *later* plugin at load: the claim is
/// taken at activation and released when the plugin's fiber unloads.
#[derive(Default)]
pub struct ServerClaims {
    claimed: Mutex<HashSet<String>>,
}

impl ServerClaims {
    /// Try to claim `name`; `false` when already claimed.
    pub fn claim(&self, name: &str) -> bool {
        self.claimed.lock().insert(name.to_string())
    }

    /// Release a claim (fiber unload).
    pub fn release(&self, name: &str) {
        self.claimed.lock().remove(name);
    }

    /// Whether `name` is currently claimed.
    pub fn is_claimed(&self, name: &str) -> bool {
        self.claimed.lock().contains(name)
    }
}

/// One bridged tool entry in the dispatch table: raw wire name, public
/// definition, executable body.
type Entry = (String, ToolDefinition, Arc<dyn ToolBody>);

/// The bridge's dispatch table.
#[derive(Default)]
struct BridgeState {
    /// server → (public name → entry)
    generations: HashMap<String, HashMap<String, Entry>>,
    /// public name → live (not yet unregistered).
    live: HashSet<String>,
    /// public name → the definition we registered there. `Tools` has no
    /// unregister, so names that disappear from a generation leave an
    /// orphan forwarder in the registry; we remember the definition so the
    /// name's reappearance re-adopts our own forwarder instead of
    /// conflicting with ourselves.
    registered: HashMap<String, ToolDefinition>,
    /// public names whose forwarder outlived its generation (still ours,
    /// not callable).
    orphans: HashSet<String>,
    /// public names we believe are ours but the registry no longer holds
    /// our definition under — taken out-of-band. Not live, not callable.
    stolen: HashSet<String>,
}

/// Whether two definitions are the same registration we placed.
/// `ToolDefinition` has no `PartialEq` (it is a seam type we must not
/// edit); name + schema + serialized flag identify it.
fn same_definition(a: &ToolDefinition, b: &ToolDefinition) -> bool {
    a.name == b.name && a.schema == b.schema && a.serialized == b.serialized
}

/// The bridge service: owns the MCP-side dispatch table and registers thin
/// forwarders on the underlying tool registry.
pub struct McpToolBridge {
    inner: Arc<dyn Tools>,
    state: RwLock<BridgeState>,
}

impl McpToolBridge {
    /// Wrap the harness tool registry.
    pub fn new(inner: Arc<dyn Tools>) -> Self {
        Self {
            inner,
            state: RwLock::new(BridgeState::default()),
        }
    }

    /// The underlying registry.
    pub fn inner(&self) -> &Arc<dyn Tools> {
        &self.inner
    }

    /// The public names currently bridged for `server`.
    pub fn public_names(&self, server: &str) -> Vec<String> {
        self.state
            .read()
            .generations
            .get(server)
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Resolve a public name to its raw wire name.
    pub fn raw_name(&self, public: &str) -> Option<String> {
        self.state
            .read()
            .generations
            .values()
            .find_map(|g| g.get(public).map(|(raw, _, _)| raw.clone()))
    }

    /// Execute a bridged call by public name.
    pub fn call(
        &self,
        public: &str,
        call_id: CallId,
        args: &[u8],
    ) -> harnless_seams::Result<serde_json::Value> {
        {
            let state = self.state.read();
            if state.stolen.contains(public) {
                return Err(SeamError::new(
                    ErrorCode::ToolNotFound,
                    format!("mcp tool {public} was taken over out-of-band"),
                ));
            }
        }
        let entry = self
            .state
            .read()
            .generations
            .values()
            .find_map(|g| g.get(public).cloned())
            .ok_or_else(|| {
                SeamError::new(
                    ErrorCode::ToolNotFound,
                    format!("mcp tool {public} not bridged"),
                )
            })?;
        entry.2.run(call_id, args)
    }

    /// Unregister every tool belonging to `server` (plugin unload or
    /// supervisor stop): clears the table so the registered forwarders
    /// resolve `tool-not-found` and the definitions disappear from the
    /// bridge's view of the registry.
    pub fn unregister_server(&self, server: &str) {
        let mut state = self.state.write();
        if let Some(table) = state.generations.remove(server) {
            for public in table.keys() {
                state.live.remove(public);
            }
        }
    }

    /// Whether the bridge currently serves `public`.
    pub fn is_live(&self, public: &str) -> bool {
        let state = self.state.read();
        state.live.contains(public) && !state.stolen.contains(public)
    }
}

/// A thin forwarder registered on the underlying registry for one public
/// name; dispatches through the bridge table at call time, so generation
/// swaps never re-register and unregister is a table clear.
struct Forwarder {
    bridge: Arc<McpToolBridge>,
    public: String,
}

impl ToolBody for Forwarder {
    fn run(&self, call_id: CallId, args: &[u8]) -> harnless_seams::Result<serde_json::Value> {
        self.bridge.call(&self.public, call_id, args)
    }
}

/// The [`Registry`] implementation the supervisor drives.
pub struct RegistryHandle {
    bridge: Arc<McpToolBridge>,
    server: String,
    /// Cancelled at unload/dispose: late publishes are dropped.
    disposed: CancellationToken,
}

impl RegistryHandle {
    /// A handle for `server` over `bridge` with its own dispose token
    /// (cancelled at unload; a late publish from an in-flight connection
    /// is then dropped instead of resurrecting tools).
    pub fn new(bridge: Arc<McpToolBridge>, server: impl Into<String>) -> Self {
        Self::with_dispose(bridge, server, CancellationToken::new())
    }

    /// A handle whose dispose token is `disposed`.
    pub fn with_dispose(
        bridge: Arc<McpToolBridge>,
        server: impl Into<String>,
        disposed: CancellationToken,
    ) -> Self {
        Self {
            bridge,
            server: server.into(),
            disposed,
        }
    }
}

impl Registry for RegistryHandle {
    fn replace_generation(&self, server: &str, tools: Generation) -> Result<(), String> {
        // The handle is bound to one server (the plugin's); the supervisor
        // names that server, and it must be the one we serve.
        assert_eq!(
            server, self.server,
            "registry handle is bound to `{}`",
            self.server
        );
        self.publish(tools)
    }

    fn unregister(&self, server: &str) {
        self.bridge.unregister_server(server);
    }
}

impl RegistryHandle {
    /// The public names currently served for this handle's server.
    pub fn served_names(&self) -> Vec<String> {
        self.bridge.public_names(&self.server)
    }

    /// The dispose token: cancelled by unload/supervisor-stop. A publish
    /// after disposal is dropped, so a late in-flight connection cannot
    /// resurrect tools after unload.
    pub fn disposed_token(&self) -> CancellationToken {
        self.disposed.clone()
    }

    /// Publish a generation, honoring the dispose guard. This is the only
    /// publish path; [`Registry::replace_generation`] delegates here.
    pub fn publish(&self, tools: Generation) -> Result<(), String> {
        self.publish_inner(tools)
    }

    /// Test-visible name for the guarded publish (a late publish arriving
    /// after unload must be dropped).
    pub fn publish_after_dispose(&self, tools: Generation) -> Result<(), String> {
        self.publish_inner(tools)
    }

    fn publish_inner(&self, tools: Generation) -> Result<(), String> {
        if self.disposed.is_cancelled() {
            return Err("registry handle disposed; generation dropped".to_string());
        }
        let server = self.server.as_str();
        let mut state = self.bridge.state.write();
        let previous = state.generations.get(server).cloned().unwrap_or_default();
        // Conflict check: every attempted public name must be free, or
        // already owned by *this* server. Anything else — another MCP
        // server's name, a non-MCP tool — rolls the attempted generation
        // back entirely (the previous table stays untouched, nothing from
        // the attempt stays registered).
        for (public, _, _) in &tools {
            for (other, gen) in state.generations.iter() {
                if other != server && gen.contains_key(public) {
                    return Err(format!(
                        "public name `{public}` is owned by server `{other}`"
                    ));
                }
            }
            // The underlying registry is authoritative — never our own
            // bookkeeping: a third party that took our public name
            // out-of-band must conflict, even while we believe the name
            // is ours. A name that is absent from the registry is free.
            if let Some(existing) = self.bridge.inner.get(public) {
                // The registry holds something under this name. It is
                // ours only if we registered that exact definition there
                // (live or orphaned) and it still matches. Anything else —
                // a third party's tool, or our name overwritten out-of-band
                // — conflicts, even if our bookkeeping still claims it.
                let ours_intact = state
                    .registered
                    .get(public)
                    .is_some_and(|ours_def| same_definition(ours_def, &existing));
                if !ours_intact {
                    if state.live.contains(public) || state.orphans.contains(public) {
                        state.stolen.insert(public.clone());
                        state.live.remove(public);
                        state.orphans.remove(public);
                    }
                    return Err(format!("public name `{public}` is taken by a non-MCP tool"));
                }
            }
        }
        // Build the new table.
        let mut table: HashMap<String, Entry> = HashMap::new();
        for (raw, def, body) in tools {
            table.insert(def.name.clone(), (raw, def, body));
        }
        // Register thin forwarders for newly appearing names; roll the
        // whole attempt back if any registration fails.
        let mut added: Vec<String> = Vec::new();
        for public in table.keys() {
            if previous.contains_key(public) || state.live.contains(public) {
                continue;
            }
            // An orphan forwarder of ours is still in the registry —
            // re-adopt it instead of re-registering (Tools has no
            // unregister, and re-registering would mask a theft check).
            if state.orphans.remove(public) {
                state.live.insert(public.clone());
                continue;
            }
            let def = table[public].1.clone();
            let forwarder = Arc::new(Forwarder {
                bridge: self.bridge.clone(),
                public: public.clone(),
            });
            match self.bridge.inner.register(def.clone(), forwarder) {
                Ok(()) => {
                    state.live.insert(public.clone());
                    state.registered.insert(public.clone(), def);
                    added.push(public.clone());
                }
                Err(e) => {
                    // Roll the attempted generation back entirely.
                    for name in added {
                        state.live.remove(&name);
                        state.registered.remove(&name);
                    }
                    return Err(format!("registering `{public}` failed: {}", e.message));
                }
            }
        }
        // Names that disappeared: drop liveness, remember the orphan.
        for public in previous.keys() {
            if !table.contains_key(public) {
                state.live.remove(public);
                if state.registered.contains_key(public) {
                    state.orphans.insert(public.clone());
                }
            }
        }
        state.generations.insert(server.to_string(), table);
        Ok(())
    }
}

/// The per-plugin MCP mount.
pub struct McpMount {
    /// The server's config.
    pub config: McpServerConfig,
    /// Cancellation for the supervisor loop.
    pub stop: CancellationToken,
    /// The rich-content gate for this server's projections.
    pub gate: RichContentGate,
}

/// The MCP server plugin.
pub struct McpServerPlugin {
    config: McpServerConfig,
    observer: Arc<dyn SupervisorObserver>,
    factory_override: Option<Arc<dyn TransportFactory>>,
    gate: RichContentGate,
    keepalive: bool,
    probe_timeout: Duration,
}

impl McpServerPlugin {
    /// The plugin: one MCP server, bridged tools-only.
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            config,
            observer: Arc::new(NoopObserver),
            factory_override: None,
            gate: RichContentGate::closed(),
            keepalive: false,
            probe_timeout: Duration::from_secs(60),
        }
    }

    /// Override how long activation waits for the first handshake +
    /// discovery before declaring the strict start failed (default 60s).
    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// Test-support: the supervisor's stop token is never cancelled until
    /// the plugin unloads, so scripted outages recover while test threads
    /// keep calling through the bridge. Production plugins never need this.
    #[cfg(feature = "test-support")]
    pub fn with_keepalive_probe(mut self) -> Self {
        self.keepalive = true;
        self
    }

    /// Attach an observer for supervisor state changes.
    pub fn with_observer(mut self, observer: Arc<dyn SupervisorObserver>) -> Self {
        self.observer = observer;
        self
    }

    /// Override the transport factory (tests script outages with a fake).
    pub fn with_factory(mut self, factory: Arc<dyn TransportFactory>) -> Self {
        self.factory_override = Some(factory);
        self
    }

    /// Set the rich-content gate.
    pub fn with_gate(mut self, gate: RichContentGate) -> Self {
        self.gate = gate;
        self
    }

    /// Activate against explicit seams (used by tests and by `apply`).
    ///
    /// Returns the mount. Fails on a duplicate server name, or on a
    /// handshake failure when `strict`. The first generation is published
    /// before this call returns.
    pub fn activate(
        &self,
        bridge: Arc<McpToolBridge>,
        claims: Arc<ServerClaims>,
    ) -> Result<McpMount, String> {
        let name = self.config.name.clone();
        if !claims.claim(&name) {
            return Err(format!("mcp server name `{name}` is already claimed"));
        }
        let factory: Arc<dyn TransportFactory> = match &self.factory_override {
            Some(f) => f.clone(),
            None => Arc::new(RmcpFactory::with_gate(
                self.config.clone(),
                self.gate.clone(),
            )),
        };
        let stop = CancellationToken::new();
        // Unload (stop cancelled) disposes the handle: a late publish from
        // an in-flight connection can no longer re-register tools.
        let registry = Arc::new(RegistryHandle::with_dispose(
            bridge.clone(),
            name.clone(),
            stop.clone(),
        ));

        // First discovery must complete before the first turn: probe the
        // initial handshake + discovery synchronously.
        let first = Arc::new(FirstGeneration::default());
        let probe_sink = Arc::new(FirstSink {
            registry: registry.clone(),
            first: first.clone(),
        });
        // Keep-alive test mode: the probe gets its own token so the
        // supervisor's connection is unaffected by the probe handoff.
        let probe_stop = if self.keepalive {
            CancellationToken::new()
        } else {
            stop.clone()
        };
        let probe_stop_run = probe_stop.clone();
        let probe_factory = factory.clone();
        let probe_name = name.clone();
        let probe = std::thread::Builder::new()
            .name(format!("harnless-mcp-probe-{name}"))
            .spawn(move || {
                let result = probe_factory.run(probe_sink, probe_stop_run.clone());
                if let Err(e) = &result {
                    // Surface the real handshake failure: the probe is
                    // the strict start's only diagnostic moment.
                    error!(server = %probe_name, detail = %e,
                        "mcp probe transport failed before publishing a generation");
                }
                result
            })
            .map_err(|e| e.to_string())?;
        // `join` moves the handle, so park it in an Option the closure can
        // take exactly once.
        let probe = std::sync::Mutex::new(Some(probe));
        let outcome = wait_for_first(&first, self.probe_timeout, || {
            // Once the probe thread exits, recover its real error so the
            // strict failure carries the factory's detail, not a generic
            // "factory exited" placeholder.
            let done = probe
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|p| p.is_finished());
            if !done {
                return None;
            }
            let handle = probe.lock().unwrap().take();
            Some(
                handle
                    .map(|p| {
                        p.join()
                            .unwrap_or_else(|_| Err("probe thread panicked".to_string()))
                    })
                    .unwrap_or_else(|| {
                        Err("factory exited before publishing a generation".to_string())
                    }),
            )
        });
        // Either way the probe's job is done: it proved (or failed to
        // prove) the handshake. Cancel it so the supervisor's first `run`
        // owns the only live connection.
        probe_stop.cancel();
        if let Some(handle) = probe.lock().unwrap().take() {
            let _ = handle.join();
        }
        match &outcome {
            Ok(()) => info!(server = %name, "mcp server activated"),
            Err(e) => {
                if self.config.strict {
                    claims.release(&name);
                    stop.cancel();
                    // The detail must reach the log: `RuntimeError`
                    // messages are static, so this line is the only
                    // carrier of the real failure reason.
                    error!(server = %name, detail = %e,
                        "mcp server failed to start (strict); plugin load failed");
                    return Err(format!("mcp server `{name}` failed to start: {e}"));
                }
                warn!(server = %name, reason = %e,
                    "mcp server failed to start (lenient); activating tool-less");
            }
        }

        // The supervisor owns outage/recovery from here. In keep-alive
        // test mode the probe shares the supervisor's stop token, so
        // activation's probe cancellation must not stop the supervisor.
        let cfg = Arc::new(self.config.reconnect.clone());
        let observer = self.observer.clone();
        std::thread::Builder::new()
            .name(format!("harnless-mcp-{name}"))
            .spawn({
                let stop = stop.clone();
                move || {
                    supervise(
                        factory,
                        registry,
                        cfg,
                        observer,
                        Arc::new(std::thread::sleep),
                        stop,
                    );
                }
            })
            .map_err(|e| e.to_string())?;

        Ok(McpMount {
            config: self.config.clone(),
            stop,
            gate: self.gate.clone(),
        })
    }
}

/// Wait for the first generation publication (Ok) or a terminal failure
/// (Err). `factory_exit` reports the probe thread's result once it has
/// exited (so the real error detail survives the thread boundary).
fn wait_for_first(
    first: &Arc<FirstGeneration>,
    timeout: Duration,
    mut factory_exit: impl FnMut() -> Option<Result<(), String>>,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = first.status.lock().clone() {
            return status;
        }
        if let Some(result) = factory_exit() {
            return result;
        }
        if Instant::now() > deadline {
            return Err("timed out waiting for first generation".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Tracks the first generation publication for synchronous activation.
#[derive(Default)]
struct FirstGeneration {
    status: Mutex<Option<Result<(), String>>>,
}

/// The sink used during activation: records the first outcome, delegates
/// the replacement itself to the registry.
struct FirstSink {
    registry: Arc<RegistryHandle>,
    first: Arc<FirstGeneration>,
}

impl GenerationSink for FirstSink {
    fn publish(&self, tools: Generation) {
        let result = self
            .registry
            .replace_generation(&self.registry.server, tools);
        if let Err(e) = &result {
            warn!(detail = %e,
                "mcp generation replacement conflicted; attempted generation rolled back");
        }
        let mut status = self.first.status.lock();
        if status.is_none() {
            *status = Some(result);
        }
    }

    fn outage(&self, reason: String) {
        let mut status = self.first.status.lock();
        if status.is_none() {
            *status = Some(Err(reason));
        }
    }
}

impl Plugin for McpServerPlugin {
    fn name(&self) -> &str {
        "mcp-server"
    }

    fn apply(&self, ctx: &Context) -> RuntimeResult<()> {
        let bridge = ctx.get::<McpToolBridge>().ok_or_else(|| {
            RuntimeError::new(
                "MISSING_SERVICE",
                "mcp plugin requires the McpToolBridge service",
            )
        })?;
        let claims = ctx.get::<ServerClaims>().unwrap_or_default();
        let mount = self.activate(bridge.clone(), claims.clone()).map_err(|e| {
            // The detail is logged here — this is the load boundary — and
            // the RuntimeError keeps a stable code.
            error!(server = %self.config.name, detail = %e,
                "mcp server failed to start; plugin load failed");
            RuntimeError::new("MCP_START_FAILED", "mcp server failed to start (see logs)")
        })?;
        let fiber: Arc<Fiber> = ctx.fiber().expect("mcp plugin runs within a fiber");
        let server = mount.config.name.clone();
        let stop = mount.stop.clone();
        let bridge_for_unload = bridge.clone();
        let claims_for_unload = claims.clone();
        fiber.effect(move || {
            Some(Box::new(move || {
                stop.cancel();
                bridge_for_unload.unregister_server(&server);
                claims_for_unload.release(&server);
                info!(server = %server,
                    "mcp plugin unloaded; supervisor stopped and tools unregistered");
            }) as Box<dyn FnMut() + Send>)
        })?;
        ctx.provide(mount)?;
        Ok(())
    }
}
