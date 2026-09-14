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
use rmcp::service::Peer;
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
    /// A connection was lost (or re-discovery failed) while the last-good
    /// generation stays registered and failing.
    Outage {
        /// The server that dropped.
        server: String,
        /// What the connection reported.
        reason: String,
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
                // `max_attempts` is the budget of *reconnects*; the
                // initial connection is not charged against it.
                if failures > cfg.max_attempts {
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
        self.observer.on_event(SupervisorEvent::Outage {
            server: self.server.clone(),
            reason: reason.clone(),
        });
        warn!(server = %self.server, reason = %reason, "mcp connection outage");
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
    // Shared debounce clock: the handler stamps it on every list round;
    // run_connection stamps it around the initial discovery too.
    let listed_clock = handler.listed_clock();
    let transport = open_transport(&cfg)?;
    let client = handler
        .serve_with_ct(transport, stop.clone())
        .await
        .map_err(|e| e.to_string())?;
    *established.lock() = Some(Instant::now());

    // Discovery: list tools (paginating) and publish the first generation.
    // Stamp the shared debounce clock so a list_changed arriving during
    // this discovery waits out its quiet period *after* we finish, never
    // racing an older response over our newer one.
    *listed_clock.lock() = Some(Instant::now());
    let tools = discover(client.peer(), cfg.call_timeout).await?;
    *listed_clock.lock() = Some(Instant::now());
    let generation = build_generation(
        &cfg.name,
        tools,
        client.peer().clone(),
        cfg.call_timeout,
        &gate,
    );
    sink.publish(generation);

    // Wait until the service dies (transport loss) or we are told to stop.
    // Park the service in an Option so the stop leg can consume it for an
    // awaited teardown.
    let mut client = Some(client);
    tokio::select! {
        quit = async { client.take().expect("service present").waiting().await } => {
            *established.lock() = None;
            sink.outage(format!("mcp connection closed ({quit:?})"));
            Err("mcp connection closed by transport".to_string())
        }
        _ = stop.cancelled() => {
            // Await the transport teardown instead of dropping the
            // RunningService: rmcp's drop guard only *requests* the serve
            // task's cancellation, so a stdio child's graceful shutdown
            // (wait, then kill) would run detached and could outlive the
            // runtime. `cancel()` consumes the service and waits for the
            // serve task — and the transport close inside it — to
            // complete, bounded so a wedged child cannot hang the
            // supervisor. The survival stamp is retired with the
            // connection so a later loss cannot count a stale survival.
            *established.lock() = None;
            if let Some(client) = client.take() {
                let _ = tokio::time::timeout(cfg.call_timeout, client.cancel()).await;
            }
            Ok(())
        }
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
    let (_tx, rx) = std::sync::mpsc::channel();
    drop(rx);
    serve_fake_keepalive_handoff(transport, sink, stop, server, call_timeout, gate, _tx)
}

/// The handoff form: after discovery publishes, the live peer is sent on
/// `peer_tx` so a test thread can call through the connection while the
/// serve call is still running.
#[cfg(feature = "test-support")]
pub fn serve_fake_keepalive_handoff(
    transport: crate::test_support::FakeTransport,
    sink: Arc<dyn GenerationSink>,
    stop: CancellationToken,
    server: String,
    call_timeout: Duration,
    gate: crate::projection::RichContentGate,
    peer_tx: std::sync::mpsc::Sender<Peer<rmcp::RoleClient>>,
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
        // Keep-alive mode: `serve_with_ct` tears a freshly-built service
        // down the instant it sees an already-cancelled token, so when the
        // token is pre-cancelled (the activation handoff shape) serve
        // without the token and let the connection run until the fake's
        // transport dies.
        let client = if stop.is_cancelled() {
            match handler.serve(transport).await {
                Ok(c) => c,
                Err(e) => return (Err(format!("keepalive serve: {e}")), None),
            }
        } else {
            match handler.serve_with_ct(transport, stop.clone()).await {
                Ok(c) => c,
                Err(e) => return (Err(e.to_string()), None),
            }
        };

        let tools = match discover(client.peer(), call_timeout).await {
            Ok(t) => t,
            Err(e) => return (Err(e), None),
        };
        let peer = client.peer().clone();
        let generation = build_generation(&cfg.name, tools, peer.clone(), call_timeout, &gate);
        sink.publish(generation);
        let _ = peer_tx.send(peer.clone());

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

/// Open the configured transport (test-visible wrapper over
/// [`open_transport`] so config-knob coverage can assert both variants
/// construct and open without a live endpoint).
pub fn try_open_transport(cfg: &McpServerConfig) -> Result<(), String> {
    open_transport(cfg).map(|_| ())
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
) -> Result<Vec<rmcp::model::Tool>, String> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    // A hostile or buggy server can keep returning the same non-empty
    // cursor forever, pinning the connection and growing memory without
    // bound. Seen cursors are a hard stop; so is the page cap.
    const MAX_PAGES: usize = 256;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..MAX_PAGES {
        let params =
            cursor.map(|c| rmcp::model::PaginatedRequestParams::default().with_cursor(Some(c)));
        let result = tokio::time::timeout(timeout, peer.list_tools(params))
            .await
            .map_err(|_| "tools/list timed out".to_string())?
            .map_err(|e| e.to_string())?;
        tools.extend(result.tools);
        match result.next_cursor {
            Some(next) if !next.is_empty() => {
                if !seen.insert(next.clone()) {
                    return Err("tools/list cursor repeated; aborting pagination".to_string());
                }
                cursor = Some(next);
            }
            _ => return Ok(tools),
        }
    }
    Err("tools/list exceeded the page limit".to_string())
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
    // Keep the first occurrence of each raw name; a server that repeats a
    // raw name loses the later one, and that loss must be visible, not
    // silent.
    let mut by_raw: std::collections::HashMap<String, rmcp::model::Tool> =
        std::collections::HashMap::new();
    let mut raws: Vec<String> = Vec::new();
    for tool in tools {
        let raw = tool.name.to_string();
        if by_raw.contains_key(&raw) {
            warn!(server = %server, raw = %raw,
                "tools/list repeated a raw name; keeping the first definition");
            continue;
        }
        by_raw.insert(raw.clone(), tool);
        raws.push(raw);
    }
    naming::public_names(server, &raws)
        .into_iter()
        .filter_map(|(raw, public)| {
            let tool = match by_raw.get(&raw) {
                Some(tool) => tool,
                None => {
                    warn!(server = %server, raw = %raw,
                        "discovered tool could not be named and was dropped");
                    return None;
                }
            };
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

/// Build a [`CallBody`] for tests: the same body the generation uses, so
/// the timeout/cancellation path is exercised exactly as registered.
/// Serve a fake connection on a helper thread and hand back the live peer
/// the moment discovery published — the exact moment calls can flow. The
/// connection runs until its transport dies.
#[cfg(feature = "test-support")]
pub fn serve_fake_peer_now(
    transport: crate::test_support::FakeTransport,
    server: String,
    call_timeout: Duration,
) -> Peer<rmcp::RoleClient> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stop = CancellationToken::new();
        stop.cancel(); // keep-alive shape (post-handoff)
        let _ = serve_fake_keepalive_handoff(
            transport,
            Arc::new(NoopSink),
            stop,
            server,
            call_timeout,
            crate::projection::RichContentGate::closed(),
            tx,
        );
    });
    rx.recv().expect("handoff sender dropped before publishing")
}

/// A sink that ignores everything (peer-only test connections).
#[cfg(feature = "test-support")]
struct NoopSink;

#[cfg(feature = "test-support")]
impl GenerationSink for NoopSink {
    fn publish(&self, _tools: Generation) {}
    fn outage(&self, _reason: String) {}
}

pub fn call_body_for_test(
    raw: String,
    peer: Peer<rmcp::RoleClient>,
    timeout: Duration,
) -> Arc<dyn harnless_seams::ToolBody> {
    Arc::new(CallBody {
        raw,
        peer,
        timeout,
        output_schema: None,
        gate: crate::projection::RichContentGate::closed(),
    })
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
                    // Cancellable request path with the timeout carried
                    // in the request options: on timeout rmcp's request
                    // handle sends the MCP `notifications/cancelled`
                    // notification for this request id, so the server
                    // stops working on the call instead of the future
                    // simply being dropped server-blind.
                    let request = rmcp::model::ClientRequest::CallToolRequest(
                        rmcp::model::Request::new(params),
                    );
                    // Carry no timeout in the request options (that path
                    // would also fire on progress-reset bookkeeping); drive
                    // the wait with an explicit timeout and cancel the
                    // request ourselves so `notifications/cancelled` is
                    // sent for this exact request id.
                    let options = rmcp::service::PeerRequestOptions::no_options();
                    let handle = peer
                        .send_cancellable_request(request, options)
                        .await
                        .map_err(|e| SeamError::new(ErrorCode::ProviderFailure, e.to_string()))?;
                    // Split the handle: wait on the response channel with
                    // our own timeout; on expiry send the MCP
                    // `notifications/cancelled` for this exact request id
                    // before surfacing the timeout.
                    let rmcp::service::RequestHandle {
                        mut rx, peer, id, ..
                    } = handle;
                    let response = tokio::time::timeout(timeout, &mut rx).await;
                    let response = match response {
                        Err(_elapsed) => {
                            let notification =
                                rmcp::model::ClientNotification::CancelledNotification(
                                    rmcp::model::Notification::new(
                                        rmcp::model::CancelledNotificationParam::new(
                                            Some(id),
                                            Some("call timeout".to_string()),
                                        ),
                                    ),
                                );
                            match peer.send_notification(notification).await {
                                Ok(()) => {
                                    return Err(SeamError::new(
                                        ErrorCode::ToolTimeout,
                                        format!("mcp call {raw} timed out; cancellation sent"),
                                    ))
                                }
                                Err(e) => {
                                    return Err(SeamError::new(
                                        ErrorCode::ToolTimeout,
                                        format!(
                                        "mcp call {raw} timed out; cancellation not delivered: {e}"
                                    ),
                                    ))
                                }
                            }
                        }
                        Ok(Err(_send_err)) => {
                            return Err(SeamError::new(
                                ErrorCode::ProviderFailure,
                                "mcp response channel closed",
                            ))
                        }
                        Ok(Ok(response)) => response,
                    };
                    let result = match response {
                        Ok(rmcp::model::ServerResult::CallToolResult(result)) => result,
                        Ok(other) => {
                            return Err(SeamError::new(
                                ErrorCode::ProviderFailure,
                                format!("unexpected server result for {raw}: {other:?}"),
                            ))
                        }
                        Err(rmcp::service::ServiceError::Timeout { .. }) => {
                            return Err(SeamError::new(
                                ErrorCode::ToolTimeout,
                                format!("mcp call {raw} timed out; cancellation sent"),
                            ))
                        }
                        Err(other) => {
                            return Err(SeamError::new(
                                ErrorCode::ProviderFailure,
                                other.to_string(),
                            ))
                        }
                    };
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
    /// The shared debounce clock handle.
    pub(crate) fn listed_clock(&self) -> Arc<Mutex<Option<Instant>>> {
        self.last_listed.clone()
    }

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
