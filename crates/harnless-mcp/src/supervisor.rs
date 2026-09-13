//! The reconnect supervisor: pure policy plus the production transport.
//!
//! All I/O lives behind the [`TransportFactory`] trait so the entire
//! outage/recovery contract — exponential backoff doubling to a ceiling,
//! budget reset after surviving past the ceiling, attempt-limit exhaustion
//! that unregisters and stops — is exercised in-process with a scripted
//! fake, with no clock waiting and no processes.
//!
//! Through an outage the last-good generation stays registered (calls
//! fail); only budget exhaustion (or a disabled supervisor) unregisters.
//! Reconnect state changes are logged at distinct severities: outage and
//! scheduling are informational, rollback is a warning, exhaustion is an
//! error.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
use rmcp::service::{Peer, ServiceError};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, ServiceExt};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use harnless_seams::{CallId, ErrorCode, SeamError, ToolBody, ToolDefinition};

use crate::bridge;
use crate::config::{McpServerConfig, TransportConfig};
use crate::naming;
use crate::projection;

/// One live MCP connection as rmcp models it.
/// Everything needed to (re)establish and use one connection.
///
/// The production implementation wraps `rmcp`; tests script a sequence of
/// outcomes. Both honor the same contract: a dead connection surfaces as a
/// transport-loss error, never a hang.
pub trait TransportFactory: Send + Sync + 'static {
    /// Establish a connection, complete discovery, publish the generation
    /// through `sink`, and return `Err(reason)` when the transport is lost.
    /// Returning `Ok(())` means the supervisor asked to stop (`stop`
    /// cancelled) and the connection shut down cleanly.
    fn run(&self, sink: Arc<dyn GenerationSink>, stop: CancellationToken) -> Result<(), String>;

    /// How long the most recent connection stayed established, if any
    /// connection ever succeeded. The supervisor uses this to decide
    /// whether the budget resets. Called once per transport loss; the
    /// factory may retire the value after reporting it.
    fn last_survival(&self) -> Option<Duration> {
        None
    }

    /// The server name this factory connects to.
    fn server(&self) -> &str;
}

/// Registry-side events a connection reports back.
pub trait GenerationSink: Send + Sync {
    /// A full generation replacement discovered on this connection.
    fn publish(&self, tools: Generation);
    /// Transport loss observed by the connection itself.
    fn outage(&self, reason: String);
}

/// One generation: the server's whole discovered tool set, as
/// `(raw wire name, public definition, body)` triples.
pub type Generation = Vec<(String, ToolDefinition, Arc<dyn ToolBody>)>;

/// A registration handle over the harness tool registry.
pub trait Registry: Send + Sync {
    /// Replace the server's whole generation atomically.
    ///
    /// On a name conflict the *entire* attempted generation is rolled back
    /// (nothing from it stays registered) and the previous generation is
    /// restored; the conflict detail is returned.
    fn replace_generation(&self, server: &str, tools: Generation) -> Result<(), String>;

    /// Remove every tool belonging to `server`.
    fn unregister(&self, server: &str);
}

/// What the supervisor decided at a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorEvent {
    /// A connection established and published its generation.
    Connected {
        /// The server that came up.
        server: String,
    },
    /// Reconnecting after `attempt` consecutive failures, in `delay`.
    Reconnect {
        /// Which consecutive failure this is (1-based).
        attempt: u32,
        /// How long the supervisor waits before the next attempt.
        delay: Duration,
    },
    /// The budget is exhausted (or reconnect is disabled): tools
    /// unregistered, supervisor stopped.
    Stopped {
        /// Consecutive failures observed at stop time.
        attempts: u32,
        /// Why the supervisor stopped.
        reason: String,
    },
}

/// Observer seam for supervisor state changes.
pub trait SupervisorObserver: Send + Sync {
    /// The supervisor transitioned.
    fn on_event(&self, event: SupervisorEvent);
}

/// A no-op observer.
pub struct NoopObserver;

impl SupervisorObserver for NoopObserver {
    fn on_event(&self, _event: SupervisorEvent) {}
}

/// The delay before retry number `attempt` (1-based): doubling from
/// `backoff_initial`, saturating at `backoff_ceiling`.
pub fn backoff_delay(cfg: &crate::config::ReconnectConfig, attempt: u32) -> Duration {
    let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
    cfg.backoff_initial
        .saturating_mul(factor)
        .min(cfg.backoff_ceiling)
}

/// Run the supervisor loop for `factory` against `registry`.
///
/// `sleep` is injectable so tests never wait on real time; production
/// passes a real thread-sleep hopper. The loop is meant to run on a
/// dedicated thread (spawned by the bridge plugin).
pub fn supervise(
    factory: Arc<dyn TransportFactory>,
    registry: Arc<dyn Registry>,
    cfg: Arc<crate::config::ReconnectConfig>,
    observer: Arc<dyn SupervisorObserver>,
    sleep: Arc<dyn Fn(Duration) + Send + Sync>,
    stop: CancellationToken,
) {
    let server = factory.server().to_string();
    let mut failures: u32 = 0;
    loop {
        if stop.is_cancelled() {
            return;
        }
        let sink: Arc<dyn GenerationSink> = Arc::new(SupervisorSink {
            registry: registry.clone(),
            server: server.clone(),
            observer: observer.clone(),
        });
        match factory.run(sink, stop.clone()) {
            Ok(()) => return,
            Err(reason) => {
                if stop.is_cancelled() {
                    return;
                }
                // Survived past the ceiling? A run that stayed up longer
                // than the ceiling resets the budget before this loss
                // counts against it. The factory retires the value as it
                // reports it, so a survival is counted at most once.
                if factory
                    .last_survival()
                    .is_some_and(|s| s > cfg.backoff_ceiling)
                {
                    info!(server = %server,
                        "mcp connection survived past the backoff ceiling; resetting reconnect budget");
                    failures = 0;
                }
                failures += 1;
                // Last-good generation stays registered; calls fail.
                info!(server = %server, reason = %reason, attempt = failures,
                    "mcp transport lost; last-good generation stays registered and failing");
                if !cfg.enabled {
                    registry.unregister(&server);
                    observer.on_event(SupervisorEvent::Stopped {
                        attempts: failures,
                        reason: "reconnect disabled".into(),
                    });
                    warn!(server = %server, "mcp supervisor stopped (reconnect disabled)");
                    return;
                }
                if failures >= cfg.max_attempts {
                    registry.unregister(&server);
                    observer.on_event(SupervisorEvent::Stopped {
                        attempts: failures,
                        reason,
                    });
                    error!(server = %server, attempts = failures,
                        "mcp reconnect budget exhausted; tools unregistered and supervisor stopped");
                    return;
                }
                let delay = backoff_delay(&cfg, failures);
                observer.on_event(SupervisorEvent::Reconnect {
                    attempt: failures,
                    delay,
                });
                info!(server = %server, attempt = failures,
                    delay_ms = delay.as_millis() as u64,
                    "mcp supervisor scheduling reconnect");
                sleep(delay);
                if stop.is_cancelled() {
                    return;
                }
            }
        }
    }
}

/// The sink the supervisor hands each connection attempt.
struct SupervisorSink {
    registry: Arc<dyn Registry>,
    server: String,
    observer: Arc<dyn SupervisorObserver>,
}

impl GenerationSink for SupervisorSink {
    fn publish(&self, tools: Generation) {
        self.observer.on_event(SupervisorEvent::Connected {
            server: self.server.clone(),
        });
        if let Err(detail) = self.registry.replace_generation(&self.server, tools) {
            // Conflict: the attempted generation was rolled back entirely;
            // the previous generation remains registered.
            warn!(server = %self.server, detail = %detail,
                "mcp generation replacement conflicted; attempted generation rolled back");
        }
    }

    fn outage(&self, reason: String) {
        info!(server = %self.server, reason = %reason, "mcp connection outage");
    }
}

/// The production factory: opens the configured transport with `rmcp`,
/// completes the handshake, discovers tools, publishes the generation, and
/// reports loss.
pub struct RmcpFactory {
    config: McpServerConfig,
    gate: crate::projection::RichContentGate,
    /// Instant the current connection became established.
    established: Arc<Mutex<Option<Instant>>>,
}

impl RmcpFactory {
    /// Create a factory for `config` with a closed rich-content gate.
    pub fn new(config: McpServerConfig) -> Self {
        Self::with_gate(config, crate::projection::RichContentGate::closed())
    }

    /// Create a factory whose projections honor `gate`.
    pub fn with_gate(config: McpServerConfig, gate: crate::projection::RichContentGate) -> Self {
        Self {
            config,
            gate,
            established: Arc::new(Mutex::new(None)),
        }
    }
}

impl TransportFactory for RmcpFactory {
    fn run(&self, sink: Arc<dyn GenerationSink>, stop: CancellationToken) -> Result<(), String> {
        let cfg = self.config.clone();
        let gate = self.gate.clone();
        let established = self.established.clone();
        // The rmcp service owns spawned tasks and (for stdio) the child
        // process, so the whole attempt — connect, discover, wait for loss
        // — runs inside one block_on on the shared runtime.
        bridge::runtime().block_on(run_connection(cfg, gate, sink, stop, established))
    }

    fn last_survival(&self) -> Option<Duration> {
        self.established.lock().map(|t| t.elapsed())
    }

    fn server(&self) -> &str {
        &self.config.name
    }
}

/// The client identity announced in every `initialize` handshake.
fn client_info() -> InitializeRequestParams {
    InitializeRequestParams::new(
        ClientCapabilities::default(),
        Implementation::from_build_env(),
    )
}

pub async fn run_connection(
    cfg: McpServerConfig,
    gate: crate::projection::RichContentGate,
    sink: Arc<dyn GenerationSink>,
    stop: CancellationToken,
    established: Arc<Mutex<Option<Instant>>>,
) -> Result<(), String> {
    let handler = BridgeHandler::new(sink.clone(), cfg.clone(), gate.clone());
    let transport = open_transport(&cfg)?;
    let client = handler
        .serve_with_ct(transport, stop.clone())
        .await
        .map_err(|e| e.to_string())?;
    *established.lock() = Some(Instant::now());

    // Discovery: list tools (paginating) and publish the first generation.
    let tools = discover(client.peer(), cfg.call_timeout)
        .await
        .map_err(|e| e.to_string())?;
    let generation = build_generation(
        &cfg.name,
        tools,
        client.peer().clone(),
        cfg.call_timeout,
        &gate,
    );
    sink.publish(generation);

    // Wait until the service dies (transport loss) or we are told to stop.
    tokio::select! {
        quit = client.waiting() => {
            *established.lock() = None;
            sink.outage(format!("mcp connection closed ({quit:?})"));
            Err("mcp connection closed by transport".to_string())
        }
        _ = stop.cancelled() => Ok(()),
    }
}

/// Serve one connection over a caller-provided transport pair (the test
/// fake's channel transport), following the same lifecycle as
/// [`run_connection`]: handshake, discovery, publish, wait-for-loss.
///
/// The fake's responder answers `tools/list` and `tools/call` directly, so
/// the connection runs the bridge's real rmcp client path with no child
/// process and no network.
#[cfg(feature = "test-support")]
pub fn serve_fake(
    transport: crate::test_support::FakeTransport,
    sink: Arc<dyn GenerationSink>,
    stop: CancellationToken,
    server: String,
    call_timeout: Duration,
) -> Result<(), String> {
    serve_fake_keepalive(
        transport,
        sink,
        stop,
        server,
        call_timeout,
        crate::projection::RichContentGate::closed(),
    )
    .0
}

/// Like [`serve_fake`], but hands back the peer clone so tests can call
/// tools after the probe connection (whose own peer dies with it).
///
/// The probe connection's stop token is cancelled the moment activation
/// completes; the keep-alive branch below keeps that connection serving so
/// the test's tool calls keep flowing. Production code never calls this.
#[cfg(feature = "test-support")]
pub fn serve_fake_keepalive(
    transport: crate::test_support::FakeTransport,
    sink: Arc<dyn GenerationSink>,
    stop: CancellationToken,
    server: String,
    call_timeout: Duration,
    gate: crate::projection::RichContentGate,
) -> (
    Result<(), String>,
    Option<rmcp::service::Peer<rmcp::RoleClient>>,
) {
    let cfg = McpServerConfig::new(
        server,
        crate::config::TransportConfig::Stdio {
            program: String::new(),
            args: Vec::new(),
            env: Default::default(),
        },
    )
    .call_timeout(call_timeout);
    let _guard = bridge::runtime().enter();
    tokio::runtime::Handle::current().block_on(async move {
        let handler = BridgeHandler::new(sink.clone(), cfg.clone(), gate.clone());
        let client = match handler.serve_with_ct(transport, stop.clone()).await {
            Ok(c) => c,
            Err(e) => return (Err(e.to_string()), None),
        };
        let tools = match discover(client.peer(), call_timeout).await {
            Ok(t) => t,
            Err(e) => return (Err(e.to_string()), None),
        };
        let peer = client.peer().clone();
        let generation = build_generation(&cfg.name, tools, peer.clone(), call_timeout, &gate);
        sink.publish(generation);
        // With a keep-alive peer the connection must outlive `stop`: the
        // probe's stop token is cancelled the moment activation completes,
        // and the test's calls still need this peer. Otherwise stop ends
        // the connection cleanly.
        if stop.is_cancelled() {
            // Keep-alive probe handoff: the test still needs this peer, so
            // keep serving until the transport itself dies. The connection
            // outliving `stop` is expected and must not report an outage.
            let _quit = client.waiting().await;
            return (Ok(()), Some(peer));
        }
        let outcome = tokio::select! {
            quit = client.waiting() => {
                sink.outage(format!("mcp connection closed ({quit:?})"));
                Err("mcp connection closed by transport".to_string())
            }
            _ = stop.cancelled() => Ok(()),
        };
        (outcome, Some(peer))
    })
}

/// Open the configured transport.
fn open_transport(cfg: &McpServerConfig) -> Result<PumpedTransport, String> {
    match &cfg.transport {
        TransportConfig::Stdio { program, args, env } => {
            let mut command = tokio::process::Command::new(program);
            command.args(args).envs(env);
            let child = TokioChildProcess::new(command).map_err(|e| e.to_string())?;
            Ok(pump_transport(child))
        }
        TransportConfig::Http { url, headers } => {
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
            let mut custom = std::collections::HashMap::new();
            for (name, value) in headers {
                let name = http::HeaderName::try_from(name.as_str()).map_err(|e| e.to_string())?;
                let value =
                    http::HeaderValue::try_from(value.as_str()).map_err(|e| e.to_string())?;
                custom.insert(name, value);
            }
            if !custom.is_empty() {
                config = config.custom_headers(custom);
            }
            let worker = StreamableHttpClientTransport::from_config(config);
            Ok(pump_transport(worker))
        }
    }
}

/// A transport served through a boxed sink/stream pair, so stdio and HTTP
/// share one concrete type at the serve site. rmcp's `IntoTransport` is
/// implemented for `(Sink, Stream)` tuples directly.
type PumpedTransport = (
    std::pin::Pin<
        Box<
            dyn futures::Sink<
                    rmcp::service::TxJsonRpcMessage<rmcp::RoleClient>,
                    Error = TransportPumpError,
                > + Send,
        >,
    >,
    futures::stream::BoxStream<'static, rmcp::service::RxJsonRpcMessage<rmcp::RoleClient>>,
);

/// The pump sink's error type: a concrete `Send + Sync + 'static` error so
/// the boxed sink satisfies rmcp's transport bounds.
#[derive(Debug)]
pub struct TransportPumpError(#[allow(dead_code)] pub String);

impl std::fmt::Display for TransportPumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mcp transport pump: {}", self.0)
    }
}

impl std::error::Error for TransportPumpError {}

/// Wrap any client transport into a boxed sink/stream pair with a pump
/// task shuttling messages in both directions.
///
/// When the sink closes (the service dropped the transport) or the real
/// transport fails in either direction, the pump closes the stream: the
/// rmcp service observes end-of-stream and surfaces transport loss.
pub(crate) fn pump_transport<T>(transport: T) -> PumpedTransport
where
    T: rmcp::transport::Transport<rmcp::RoleClient> + Send + 'static,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    use futures::channel::mpsc;
    use futures::{SinkExt, StreamExt};
    let (out_tx, mut out_rx) =
        mpsc::channel::<rmcp::service::TxJsonRpcMessage<rmcp::RoleClient>>(64);
    let (in_tx, in_rx) = mpsc::channel::<rmcp::service::RxJsonRpcMessage<rmcp::RoleClient>>(64);
    // Pump: client→transport and transport→client in one task. When the
    // outbound channel closes (service dropped) or either direction fails,
    // the pump ends and drops `in_tx`, closing the inbound stream — the
    // rmcp service observes end-of-stream and surfaces transport loss.
    tokio::spawn(async move {
        let mut transport = transport;
        let mut in_tx = in_tx;
        loop {
            tokio::select! {
                msg = out_rx.next() => {
                    let Some(msg) = msg else { break };
                    if transport.send(msg).await.is_err() {
                        break;
                    }
                }
                msg = transport.receive() => {
                    let Some(msg) = msg else { break };
                    if in_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = transport.close().await;
    });
    let sink = futures::sink::unfold(out_tx, |mut tx, msg| async move {
        tx.send(msg)
            .await
            .map(|_| tx)
            .map_err(|e| TransportPumpError(e.to_string()))
    });
    let stream = futures::StreamExt::map(in_rx, |m| m);
    (Box::pin(sink), Box::pin(stream))
}

/// List all tools, following pagination cursors.
///
/// Generic over anything dereferencing to a client `Peer` — a live
/// [`Connection`] or the bare `Peer` a notification handler receives.
async fn discover(
    peer: &Peer<rmcp::RoleClient>,
    timeout: Duration,
) -> Result<Vec<rmcp::model::Tool>, ServiceError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params =
            cursor.map(|c| rmcp::model::PaginatedRequestParams::default().with_cursor(Some(c)));
        let result = tokio::time::timeout(timeout, peer.list_tools(params))
            .await
            .map_err(|_| ServiceError::TransportClosed)??;
        tools.extend(result.tools);
        match result.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => break,
        }
    }
    Ok(tools)
}

/// Build one generation: public-named definitions plus bodies that call
/// the *raw* wire name through the live peer.
pub(crate) fn build_generation(
    server: &str,
    tools: Vec<rmcp::model::Tool>,
    peer: Peer<rmcp::RoleClient>,
    timeout: Duration,
    gate: &crate::projection::RichContentGate,
) -> Generation {
    let raws: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    let by_raw: std::collections::HashMap<String, rmcp::model::Tool> =
        tools.into_iter().map(|t| (t.name.to_string(), t)).collect();
    naming::public_names(server, &raws)
        .into_iter()
        .filter_map(|(raw, public)| {
            let tool = by_raw.get(&raw)?;
            let def = ToolDefinition {
                name: public,
                schema: serde_json::Value::Object((*tool.input_schema).clone()),
                serialized: false,
            };
            let body = Arc::new(CallBody {
                raw: raw.clone(),
                peer: peer.clone(),
                timeout,
                gate: gate.clone(),
                output_schema: tool
                    .output_schema
                    .as_ref()
                    .map(|s| serde_json::Value::Object((**s).clone())),
            });
            Some((raw, def, body as Arc<dyn ToolBody>))
        })
        .collect()
}

/// A tool body that forwards to the MCP server under the raw wire name.
pub(crate) struct CallBody {
    pub raw: String,
    pub peer: Peer<rmcp::RoleClient>,
    pub timeout: Duration,
    pub output_schema: Option<serde_json::Value>,
    /// The server's rich-content gate: images/audio are admitted only when
    /// the mount opened it (store + image-input route).
    pub gate: crate::projection::RichContentGate,
}

impl ToolBody for CallBody {
    fn run(&self, _call_id: CallId, args: &[u8]) -> harnless_seams::Result<serde_json::Value> {
        let parsed: serde_json::Value = serde_json::from_slice(args)
            .map_err(|e| SeamError::new(ErrorCode::IoError, format!("invalid tool args: {e}")))?;
        let arguments = match parsed {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => {
                return Err(SeamError::new(
                    ErrorCode::IoError,
                    format!(
                        "tool arguments must be a JSON object, got {}",
                        crate::projection::value_kind(&other)
                    ),
                ))
            }
        };
        let mut params = rmcp::model::CallToolRequestParams::new(self.raw.clone());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        // `ToolBody::run` is synchronous by seam contract; a worker thread
        // inside the shared runtime cannot re-enter `block_on`, so hop to a
        // fresh thread that owns the runtime context.
        let raw = self.raw.clone();
        let peer = self.peer.clone();
        let timeout = self.timeout;
        let schema = self.output_schema.clone();
        let gate = self.gate.clone();
        let raw_for_err = raw.clone();
        let join = std::thread::scope(move |s| {
            s.spawn(move || {
                let _guard = bridge::runtime().enter();
                tokio::runtime::Handle::current().block_on(async move {
                    let result = tokio::time::timeout(timeout, peer.call_tool(params))
                        .await
                        .map_err(|_| {
                            SeamError::new(
                                ErrorCode::ToolTimeout,
                                format!("mcp call {raw} timed out"),
                            )
                        })?
                        .map_err(|e| SeamError::new(ErrorCode::ProviderFailure, e.to_string()))?;
                    projection::project(result, schema.as_ref(), &gate)
                })
            })
            .join()
        })
        .map_err(|_| {
            SeamError::new(
                ErrorCode::ProviderFailure,
                format!("mcp call {raw_for_err} panicked"),
            )
        })?;
        join
    }
}

/// The bridge handler: receives `notifications/tools/list_changed` and
/// re-discovers, replacing the whole generation through the sink.
pub(crate) struct BridgeHandler {
    sink: Arc<dyn GenerationSink>,
    config: McpServerConfig,
    gate: crate::projection::RichContentGate,
    last_listed: Arc<Mutex<Option<Instant>>>,
}

impl BridgeHandler {
    pub(crate) fn new(
        sink: Arc<dyn GenerationSink>,
        config: McpServerConfig,
        gate: crate::projection::RichContentGate,
    ) -> Self {
        Self {
            sink,
            config,
            gate,
            last_listed: Arc::new(Mutex::new(None)),
        }
    }
}

impl ClientHandler for BridgeHandler {
    fn get_info(&self) -> rmcp::model::ClientInfo {
        client_info()
    }

    fn on_tool_list_changed(
        &self,
        context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        let sink = self.sink.clone();
        let config = self.config.clone();
        let gate = self.gate.clone();
        let peer = context.peer.clone();
        // Debounce: a test may fire list_changed while the initial
        // discovery `tools/list` is still in flight. Wait out a quiet
        // period, then re-discover once.
        let last = self.last_listed.clone();
        async move {
            tracing::info!("mcp list_changed notification received");
            {
                let mut g = last.lock();
                *g = Some(Instant::now());
            }
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let quiet = last
                    .lock()
                    .map(|t| t.elapsed() >= Duration::from_millis(50));
                if quiet.unwrap_or(true) {
                    break;
                }
            }
            tracing::info!("mcp list_changed debounce settled; re-discovering");
            match discover(&peer, config.call_timeout).await {
                Ok(tools) => {
                    let generation =
                        build_generation(&config.name, tools, peer, config.call_timeout, &gate);
                    sink.publish(generation);
                }
                Err(e) => sink.outage(format!("re-discovery failed: {e}")),
            }
        }
    }
}
