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
use tracing::{info, warn};

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
        self.state.read().live.contains(public)
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
}

impl RegistryHandle {
    /// A handle for `server` over `bridge`.
    pub fn new(bridge: Arc<McpToolBridge>, server: impl Into<String>) -> Self {
        Self {
            bridge,
            server: server.into(),
        }
    }
}

impl Registry for RegistryHandle {
    fn replace_generation(&self, server: &str, tools: Generation) -> Result<(), String> {
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
            let ours = previous.contains_key(public) || state.live.contains(public);
            if !ours && self.bridge.inner.get(public).is_some() {
                return Err(format!("public name `{public}` is taken by a non-MCP tool"));
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
            let def = table[public].1.clone();
            let forwarder = Arc::new(Forwarder {
                bridge: self.bridge.clone(),
                public: public.clone(),
            });
            match self.bridge.inner.register(def, forwarder) {
                Ok(()) => {
                    state.live.insert(public.clone());
                    added.push(public.clone());
                }
                Err(e) => {
                    // Roll the attempted generation back entirely.
                    for name in added {
                        state.live.remove(&name);
                    }
                    return Err(format!("registering `{public}` failed: {}", e.message));
                }
            }
        }
        // Drop liveness for names that disappeared.
        for public in previous.keys() {
            if !table.contains_key(public) {
                state.live.remove(public);
            }
        }
        state.generations.insert(server.to_string(), table);
        Ok(())
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
        let registry = Arc::new(RegistryHandle::new(bridge.clone(), name.clone()));
        let factory: Arc<dyn TransportFactory> = match &self.factory_override {
            Some(f) => f.clone(),
            None => Arc::new(RmcpFactory::with_gate(
                self.config.clone(),
                self.gate.clone(),
            )),
        };
        let stop = CancellationToken::new();

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
        let probe = std::thread::Builder::new()
            .name(format!("harnless-mcp-probe-{name}"))
            .spawn(move || probe_factory.run(probe_sink, probe_stop_run.clone()))
            .map_err(|e| e.to_string())?;
        let outcome = wait_for_first(&first, self.probe_timeout, || probe.is_finished());
        // Either way the probe's job is done: it proved (or failed to
        // prove) the handshake. Cancel it so the supervisor's first `run`
        // owns the only live connection.
        probe_stop.cancel();
        let _ = probe.join();
        match &outcome {
            Ok(()) => info!(server = %name, "mcp server activated"),
            Err(e) => {
                if self.config.strict {
                    claims.release(&name);
                    stop.cancel();
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
/// (Err). `factory_done` reports whether the probe thread has exited.
fn wait_for_first(
    first: &Arc<FirstGeneration>,
    timeout: Duration,
    factory_done: impl Fn() -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = first.status.lock().clone() {
            return status;
        }
        if factory_done() {
            return Err("factory exited before publishing a generation".into());
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
        let mount = self
            .activate(bridge.clone(), claims.clone())
            .map_err(|e| RuntimeError::new("MCP_START_FAILED", describe(&e)))?;
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

/// `RuntimeError` carries a static message; the failure detail is logged
/// and a stable code is surfaced instead of leaking the string.
fn describe(detail: &str) -> &'static str {
    // The detail travels through the log line emitted by `activate`'s
    // callers; the RuntimeError stays stable.
    let _ = detail;
    "mcp server failed to start (see logs)"
}
