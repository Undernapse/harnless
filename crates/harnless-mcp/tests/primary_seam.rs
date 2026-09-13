//! The primary-seam acceptance run: an in-memory fake MCP server over a
//! channel-backed transport, driven end-to-end through the bridge's real
//! production rmcp path (discovery → namespaced registration → raw-name
//! invocation → list-changed generation replacement → conflict rollback →
//! outage → capability-denied).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harnless_mcp::bridge::{McpServerPlugin, McpToolBridge, ServerClaims};
use harnless_mcp::config::{McpServerConfig, TransportConfig};
use harnless_mcp::naming::public_name;
use harnless_mcp::supervisor::{
    backoff_delay, supervise, Generation, GenerationSink, Registry, SupervisorEvent,
    SupervisorObserver, TransportFactory,
};
use harnless_mcp::test_support::{fake_server, scripted, FakeServer, Request, Responder, Response};
use harnless_seams::{CallId, ErrorCode, SeamError, ToolBody, ToolDefinition, Tools};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// A registry we can inspect: records every registration.
type SpyTools = Arc<Mutex<Vec<(ToolDefinition, Arc<dyn ToolBody>)>>>;

#[derive(Default)]
struct SpyRegistry {
    tools: SpyTools,
}

impl Tools for SpyRegistry {
    fn register(&self, def: ToolDefinition, body: Arc<dyn ToolBody>) -> harnless_seams::Result<()> {
        self.tools.lock().push((def, body));
        Ok(())
    }
    fn get(&self, name: &str) -> Option<ToolDefinition> {
        self.tools
            .lock()
            .iter()
            .find(|(d, _)| d.name == name)
            .map(|(d, _)| d.clone())
    }
    fn names(&self) -> Vec<String> {
        self.tools
            .lock()
            .iter()
            .map(|(d, _)| d.name.clone())
            .collect()
    }
}

fn stdio_config(name: &str) -> McpServerConfig {
    McpServerConfig::new(
        name,
        TransportConfig::Stdio {
            program: "fake".into(),
            args: Vec::new(),
            env: Default::default(),
        },
    )
    .call_timeout(Duration::from_secs(5))
}

/// A factory that serves a fresh connection built from `make` on every
/// `run`, recording each connection's kill switch in `kills` so tests can
/// script outages against the live transport.
type Kills = Arc<Mutex<Vec<tokio::sync::oneshot::Sender<()>>>>;
fn scripted_factory(
    server: &'static str,
    make: impl Fn() -> FakeServer + Send + Sync + 'static,
) -> (Arc<dyn TransportFactory>, Kills) {
    scripted_factory_gated(
        server,
        make,
        harnless_mcp::projection::RichContentGate::closed(),
    )
}

fn scripted_factory_gated(
    server: &'static str,
    make: impl Fn() -> FakeServer + Send + Sync + 'static,
    gate: harnless_mcp::projection::RichContentGate,
) -> (Arc<dyn TransportFactory>, Kills) {
    let kills: Kills = Arc::new(Mutex::new(Vec::new()));
    struct F(
        &'static str,
        Box<dyn Fn() -> FakeServer + Send + Sync>,
        Kills,
        harnless_mcp::projection::RichContentGate,
    );
    impl TransportFactory for F {
        fn run(
            &self,
            sink: Arc<dyn GenerationSink>,
            stop: CancellationToken,
        ) -> Result<(), String> {
            let fake = (self.1)();
            if let Some(k) = fake.kill.lock().take() {
                self.2.lock().push(k);
            }
            let (outcome, _peer) = harnless_mcp::supervisor::serve_fake_keepalive(
                fake.transport,
                sink.clone(),
                stop,
                self.0.into(),
                Duration::from_secs(5),
                self.3.clone(),
            );
            outcome
        }
        fn server(&self) -> &str {
            self.0
        }
    }
    (
        Arc::new(F(server, Box::new(make), kills.clone(), gate)),
        kills,
    )
}

fn tools_responder(
    names: Vec<&'static str>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    errors: Arc<Mutex<Vec<String>>>,
) -> Responder {
    scripted(
        names
            .iter()
            .map(|n| harnless_mcp::test_support::tool(n))
            .collect(),
        move |req: &Request| {
            let tool = req.params["name"].as_str().unwrap_or("?").to_string();
            calls.lock().push((tool.clone(), req.params.clone()));
            if errors.lock().contains(&tool) {
                return Response::Error {
                    code: -32601,
                    message: format!("tool not available: {tool}"),
                };
            }
            Response::Result(json!({"content": [{"type": "text", "text": format!("{tool} ok")}]}))
        },
    )
}

fn mount(plugin: McpServerPlugin) -> (Arc<McpToolBridge>, Arc<SpyRegistry>, Arc<ServerClaims>) {
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let claims = Arc::new(ServerClaims::default());
    plugin
        .activate(bridge.clone(), claims.clone())
        .expect("plugin activates");
    (bridge, spy, claims)
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_registers_namespaced_tools_before_the_first_turn() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let (factory, _kills) = scripted_factory("alpha", {
        let calls = calls.clone();
        let errors = errors.clone();
        move || {
            fake_server(tools_responder(
                vec!["search", "fetch"],
                calls.clone(),
                errors.clone(),
            ))
        }
    });
    let plugin = McpServerPlugin::new(stdio_config("alpha")).with_factory(factory);
    let (bridge, spy, _claims) = mount(plugin);
    // Activation is synchronous through discovery: tools are registered
    // before the first turn could run.
    let mut names = spy.names();
    names.sort();
    assert_eq!(
        names,
        vec![
            public_name("alpha", "fetch"),
            public_name("alpha", "search")
        ]
    );
    // The public name is the pure-function name.
    assert_eq!(public_name("alpha", "search"), "mcp__alpha__search");
    assert!(bridge.is_live("mcp__alpha__search"));
}

#[tokio::test(flavor = "multi_thread")]
async fn invocation_sends_the_raw_wire_name() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let (factory, _kills) = scripted_factory("alpha", {
        let calls = calls.clone();
        let errors = errors.clone();
        move || {
            fake_server(tools_responder(
                vec!["search"],
                calls.clone(),
                errors.clone(),
            ))
        }
    });
    let plugin = McpServerPlugin::new(stdio_config("alpha"))
        .with_factory(factory)
        .with_keepalive_probe();
    let (bridge, _spy, _claims) = mount(plugin);
    // The registered forwarder answers through the bridge.
    let out = bridge
        .inner()
        .get("mcp__alpha__search")
        .expect("registered");
    assert_eq!(out.name, "mcp__alpha__search");
    // The supervised connection takes over after activation; poll until a
    // call flows through it.
    loop {
        match bridge.call("mcp__alpha__search", CallId(1), br#"{"q":"hi"}"#) {
            Ok(v) if v["content"][0]["text"] == json!("search ok") => break,
            Ok(v) => panic!("unexpected result: {v}"),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    // The wire saw the raw name and the raw args, never the public name.
    let seen = calls.lock().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "search");
    assert_eq!(seen[0].1["name"], json!("search"));
    assert_eq!(seen[0].1["arguments"], json!({"q": "hi"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_changed_replaces_the_whole_generation() {
    // Generation 1: {a, b}. After list_changed the server serves {b, c}.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let calls2 = calls.clone();
    let errors2 = errors.clone();
    type NotifyVec =
        Arc<Mutex<Vec<futures::channel::mpsc::Sender<rmcp::model::ServerNotification>>>>;
    type ListedVec = Arc<Mutex<Vec<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>>>;
    let notify: NotifyVec = Arc::new(Mutex::new(Vec::new()));
    let listed: ListedVec = Arc::new(Mutex::new(Vec::new()));
    let factory: Arc<dyn TransportFactory> = {
        struct F(
            AtomicUsize,
            Arc<Mutex<Vec<(String, Value)>>>,
            Arc<Mutex<Vec<String>>>,
            NotifyVec,
            ListedVec,
        );
        impl TransportFactory for F {
            fn run(
                &self,
                sink: Arc<dyn GenerationSink>,
                stop: CancellationToken,
            ) -> Result<(), String> {
                let conn = self.0.fetch_add(1, Ordering::SeqCst);
                // Per-connection: the first `tools/list` (discovery)
                // answers {a, b}; any later list on the same connection
                // (the list-changed re-sync) answers {b, c}.
                // Per-connection responder: discovery answers {a, b}; the
                // list-changed re-sync answers {b, c}.
                let list_count = Arc::new(AtomicUsize::new(0));
                let responder: Responder = {
                    let calls = self.1.clone();
                    let errors = self.2.clone();
                    Arc::new(move |req: &Request| match req.method.as_str() {
                        "initialize" => {
                            Some(harnless_mcp::test_support::Response::initialize(true))
                        }
                        "tools/list" => {
                            let n = list_count.fetch_add(1, Ordering::SeqCst);
                            let names: &[&str] = if n == 0 { &["a", "b"] } else { &["b", "c"] };
                            Some(Response::tools(harnless_mcp::test_support::scripted_tools(
                                names,
                            )))
                        }
                        "tools/call" => {
                            let tool = req.params["name"].as_str().unwrap_or("?").to_string();
                            calls.lock().push((tool.clone(), req.params.clone()));
                            if errors.lock().contains(&tool) {
                                return Some(Response::Error {
                                    code: -32601,
                                    message: format!("tool not available: {tool}"),
                                });
                            }
                            Some(Response::Result(
                                json!({"content": [{"type": "text", "text": format!("{tool} ok")}]}),
                            ))
                        }
                        _ => Some(Response::Error {
                            code: -32601,
                            message: format!("method not found: {}", req.method),
                        }),
                    })
                };
                let fake = fake_server(responder);
                self.3.lock().push(fake.notify_tx.clone());
                let listed_rx = fake.listed.lock().unwrap().take();
                self.4.lock().push(std::sync::Mutex::new(listed_rx));
                let _ = conn;
                harnless_mcp::supervisor::serve_fake(
                    fake.transport,
                    sink,
                    stop,
                    "alpha".into(),
                    Duration::from_secs(5),
                )
            }
            fn server(&self) -> &str {
                "alpha"
            }
        }
        Arc::new(F(
            AtomicUsize::new(0),
            calls2,
            errors2,
            notify.clone(),
            listed.clone(),
        ))
    };
    let plugin = McpServerPlugin::new(stdio_config("alpha"))
        .with_factory(factory.clone())
        .with_keepalive_probe();
    let (bridge, _spy, _claims) = mount(plugin);
    // Push list_changed at the live (keep-alive probe) connection once it
    // has answered its first tools/list.
    let notify_task = notify.clone();
    let listed_task = listed.clone();
    std::thread::spawn(move || {
        loop {
            let ready = {
                let guard = listed_task.lock();
                guard
                    .get(1)
                    .map(|r| r.lock().unwrap().is_some())
                    .unwrap_or(false)
            };
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        {
            let guard = listed_task.lock();
            // The supervised connection (channel 1) is the live one.
            if let Some(rx) = guard.get(1) {
                if let Some(rx) = rx.lock().unwrap().take() {
                    let _ = rx.blocking_recv();
                }
            }
        }
        // Give the supervised connection time to start (it is the live one
        // whose sink drives the registry).
        std::thread::sleep(Duration::from_millis(300));
        // The supervised connection answers the re-sync list with {b, c}.
        {
            let mut guard = notify_task.lock();
            let _ = guard[1].try_send(
                rmcp::model::ServerNotification::ToolListChangedNotification(
                    rmcp::model::NotificationNoParam::default(),
                ),
            );
        }
    });
    // Wait for the replaced generation (the bridge's served-name set is
    // the generation; the registry keeps thin forwarders).
    let mut names = bridge.public_names("alpha");
    for _ in 0..500 {
        names.sort();
        if names.len() == 2 && names.iter().any(|n| n.ends_with("__c")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
        names = bridge.public_names("alpha");
    }
    names.sort();
    assert_eq!(
        names,
        vec![public_name("alpha", "b"), public_name("alpha", "c")],
        "generation replaced wholesale: `a` gone, `b` kept, `c` added"
    );
    let _ = &calls;
}

#[tokio::test(flavor = "multi_thread")]
async fn conflict_rolls_back_the_attempted_generation_entirely() {
    // The spy registry refuses to register `mcp__beta__x` (simulating a
    // taken public name). The whole attempted generation — including
    // `mcp__beta__y`, which would have succeeded — must roll back.
    struct Rejecting;
    impl Tools for Rejecting {
        fn register(
            &self,
            def: ToolDefinition,
            _body: Arc<dyn ToolBody>,
        ) -> harnless_seams::Result<()> {
            if def.name == "mcp__beta__x" {
                return Err(SeamError::new(ErrorCode::ToolNotFound, "taken"));
            }
            Ok(())
        }
        fn get(&self, _name: &str) -> Option<ToolDefinition> {
            None
        }
        fn names(&self) -> Vec<String> {
            Vec::new()
        }
    }
    let bridge = Arc::new(McpToolBridge::new(Arc::new(Rejecting)));
    let handle = Arc::new(harnless_mcp::bridge::RegistryHandle::new(
        bridge.clone(),
        "beta",
    ));
    let gen: Generation = vec![
        (
            "x".into(),
            ToolDefinition {
                name: "mcp__beta__x".into(),
                schema: json!({"type": "object"}),
                serialized: false,
            },
            Arc::new(NoopBody) as Arc<dyn ToolBody>,
        ),
        (
            "y".into(),
            ToolDefinition {
                name: "mcp__beta__y".into(),
                schema: json!({"type": "object"}),
                serialized: false,
            },
            Arc::new(NoopBody) as Arc<dyn ToolBody>,
        ),
    ];
    let err = handle
        .replace_generation("beta", gen)
        .expect_err("conflict must fail the generation");
    assert!(err.contains("mcp__beta__x"), "names the conflict: {err}");
    // Rolled back entirely: neither name is live, and the server has no
    // generation at all.
    assert!(!bridge.is_live("mcp__beta__x"));
    assert!(!bridge.is_live("mcp__beta__y"));
    assert!(bridge.public_names("beta").is_empty());
}

struct NoopBody;

impl ToolBody for NoopBody {
    fn run(&self, _call_id: CallId, _args: &[u8]) -> harnless_seams::Result<Value> {
        Ok(json!({"ok": true}))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn outage_backs_off_and_last_good_generation_stays_registered_failing() {
    // Scripted connection plan: probe (killed by activation handoff), then
    // a supervised connection the test kills mid-flight (the outage), then
    // a recovery connection. Through the outage the last-good generation
    // stays registered and calls fail; after recovery they succeed.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let kills = Arc::new(Mutex::new(Vec::<tokio::sync::oneshot::Sender<()>>::new()));
    let (factory, kills2) = scripted_factory("alpha", {
        let calls = calls.clone();
        let errors = errors.clone();
        let kills = kills.clone();
        move || {
            let fake = fake_server(tools_responder(
                vec!["search"],
                calls.clone(),
                errors.clone(),
            ));
            kills.lock().push(fake.kill.lock().take().unwrap());
            fake
        }
    });
    let plugin = McpServerPlugin::new(stdio_config("alpha").reconnect(
        harnless_mcp::config::ReconnectConfig {
            enabled: true,
            backoff_initial: Duration::from_millis(10),
            backoff_ceiling: Duration::from_millis(40),
            max_attempts: 50,
        },
    ))
    .with_factory(factory)
    .with_keepalive_probe();
    let (bridge, spy, _claims) = mount(plugin);
    // Registered through the whole outage.
    assert_eq!(spy.names(), vec![public_name("alpha", "search")]);
    // Wait for the supervised connection (index 1), then kill it.
    let kills_task = kills.clone();
    std::thread::spawn(move || {
        for _ in 0..500 {
            if kills_task.lock().len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        if let Some(slot) = kills_task.lock().get_mut(1) {
            let tx = std::mem::replace(slot, tokio::sync::oneshot::channel::<()>().0);
            let _ = tx.send(());
        }
    });
    // During the outage calls must fail (transport gone).
    let mut failed = false;
    for _ in 0..500 {
        let r = bridge.call("mcp__alpha__search", CallId(2), b"{}");
        if r.is_err() {
            failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(failed, "calls must fail during the outage");
    assert_eq!(
        spy.names(),
        vec![public_name("alpha", "search")],
        "last-good generation stays registered through the outage"
    );
    // Recovery: the supervisor reconnects (backoff), and calls succeed.
    let mut recovered = false;
    let mut last_err: Option<String> = None;
    for _ in 0..500 {
        match bridge.call("mcp__alpha__search", CallId(3), b"{}") {
            Ok(v) => {
                if v["content"][0]["text"] == json!("search ok") {
                    recovered = true;
                    break;
                }
                last_err = Some(format!("unexpected result: {v}"));
            }
            Err(e) => last_err = Some(format!("{:?}", e)),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        recovered,
        "reconnect must restore working calls; last: {:?}",
        last_err
    );
    let _ = &kills2;
}

#[tokio::test(flavor = "multi_thread")]
async fn capability_denied_surfaces_as_structured_error() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(vec!["secret".to_string()]));
    let (factory, _kills) = scripted_factory("alpha", {
        let calls = calls.clone();
        let errors = errors.clone();
        move || {
            fake_server(tools_responder(
                vec!["secret"],
                calls.clone(),
                errors.clone(),
            ))
        }
    });
    let plugin = McpServerPlugin::new(stdio_config("alpha"))
        .with_factory(factory)
        .with_keepalive_probe();
    let (bridge, _spy, _claims) = mount(plugin);
    let err = loop {
        match bridge.call("mcp__alpha__secret", CallId(4), b"{}") {
            Ok(v) => panic!("capability denial must surface as an error, got {v}"),
            Err(e) if e.message.contains("-32601") => break e,
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    assert!(
        err.message.contains("tool not available"),
        "detail: {}",
        err.message
    );
    assert!(
        err.message.contains("-32601"),
        "wire code preserved: {}",
        err.message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_server_name_fails_the_later_plugin() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let (factory_a, _ka) = scripted_factory("alpha", {
        let calls = calls.clone();
        let errors = errors.clone();
        move || fake_server(tools_responder(vec!["x"], calls.clone(), errors.clone()))
    });
    let (factory_b, _kb) = scripted_factory("alpha", {
        let calls = calls.clone();
        let errors = errors.clone();
        move || fake_server(tools_responder(vec!["y"], calls.clone(), errors.clone()))
    });
    let plugin_a = McpServerPlugin::new(stdio_config("alpha")).with_factory(factory_a);
    let plugin_b = McpServerPlugin::new(stdio_config("alpha")).with_factory(factory_b);
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let claims = Arc::new(ServerClaims::default());
    plugin_a
        .activate(bridge.clone(), claims.clone())
        .expect("first plugin activates");
    let err = match plugin_b.activate(bridge.clone(), claims.clone()) {
        Ok(_) => panic!("second plugin with the same server name must fail"),
        Err(e) => e,
    };
    assert!(err.contains("alpha"), "error names the server: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn supervisor_backoff_budget_unregisters_when_exhausted() {
    // The scripted multi-connection factory above proves recovery; the
    // budget-exhaustion stop is asserted here at the supervisor seam with
    // the same fake transport machinery: every connection dies at once.
    struct Dying;
    impl TransportFactory for Dying {
        fn run(
            &self,
            _sink: Arc<dyn GenerationSink>,
            _stop: CancellationToken,
        ) -> Result<(), String> {
            Err("transport died instantly".into())
        }
        fn server(&self) -> &str {
            "alpha"
        }
    }
    #[derive(Default)]
    struct Rec(Mutex<Vec<SupervisorEvent>>);
    impl SupervisorObserver for Rec {
        fn on_event(&self, e: SupervisorEvent) {
            self.0.lock().push(e);
        }
    }
    struct Unreg(Mutex<Vec<String>>);
    impl Registry for Unreg {
        fn replace_generation(&self, _s: &str, _t: Generation) -> Result<(), String> {
            Ok(())
        }
        fn unregister(&self, s: &str) {
            self.0.lock().push(s.into());
        }
    }
    let reg = Arc::new(Unreg(Mutex::new(Vec::new())));
    let obs = Arc::new(Rec::default());
    let cfg = harnless_mcp::config::ReconnectConfig {
        enabled: true,
        backoff_initial: Duration::from_millis(1),
        backoff_ceiling: Duration::from_millis(4),
        max_attempts: 3,
    };
    supervise(
        Arc::new(Dying),
        reg.clone(),
        Arc::new(cfg.clone()),
        obs.clone(),
        Arc::new(|_| {}),
        CancellationToken::new(),
    );
    let delays: Vec<Duration> = obs
        .0
        .lock()
        .iter()
        .filter_map(|e| match e {
            SupervisorEvent::Reconnect { delay, .. } => Some(*delay),
            _ => None,
        })
        .collect();
    assert_eq!(
        delays,
        vec![backoff_delay(&cfg, 1), backoff_delay(&cfg, 2),],
        "two reconnects then stop at attempt 3"
    );
    assert_eq!(
        &*reg.0.lock(),
        &["alpha".to_string()],
        "unregistered on stop"
    );
}

/// A factory that can never connect: exits immediately without publishing.
struct DyingFactory(&'static str);

impl TransportFactory for DyingFactory {
    fn run(&self, _sink: Arc<dyn GenerationSink>, _stop: CancellationToken) -> Result<(), String> {
        Err("transport refused".to_string())
    }
    fn server(&self) -> &str {
        self.0
    }
}

/// A factory that publishes one tool and blocks until stopped.
struct OkFactory(&'static str);

impl TransportFactory for OkFactory {
    fn run(&self, sink: Arc<dyn GenerationSink>, stop: CancellationToken) -> Result<(), String> {
        sink.publish(vec![(
            "present".to_string(),
            ToolDefinition {
                name: public_name(self.0, "present"),
                schema: json!({"type": "object"}),
                serialized: false,
            },
            Arc::new(NoopBody) as Arc<dyn ToolBody>,
        )]);
        futures::executor::block_on(stop.cancelled());
        Ok(())
    }
    fn server(&self) -> &str {
        self.0
    }
}

#[test]
fn strict_startup_failure_fails_the_load() {
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let claims = Arc::new(ServerClaims::default());
    let plugin = McpServerPlugin::new(stdio_config("strict-one").strict(true))
        .with_factory(Arc::new(DyingFactory("strict-one")));
    let err = match plugin.activate(bridge.clone(), claims.clone()) {
        Ok(_) => panic!("strict plugin must fail activation when the server is down"),
        Err(e) => e,
    };
    assert!(err.contains("strict-one"), "error names the server: {err}");
    assert!(spy.names().is_empty(), "nothing registered on failure");
}

#[test]
fn lenient_startup_failure_activates_tool_less() {
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let claims = Arc::new(ServerClaims::default());
    let plugin = McpServerPlugin::new(stdio_config("lenient-one").strict(false))
        .with_factory(Arc::new(DyingFactory("lenient-one")));
    plugin
        .activate(bridge.clone(), claims.clone())
        .expect("lenient plugin activates tool-less");
    assert!(spy.names().is_empty(), "activated with no tools");
    // The lenient plugin still holds the name: a second plugin for the same
    // server fails even though the first one started tool-less.
    let second = McpServerPlugin::new(stdio_config("lenient-one").strict(true))
        .with_factory(Arc::new(OkFactory("lenient-one")));
    let err = match second.activate(bridge.clone(), claims.clone()) {
        Ok(_) => panic!("claim must survive a lenient start"),
        Err(e) => e,
    };
    assert!(err.contains("lenient-one"), "error names the server: {err}");
}

/// End-to-end gate wiring: the plugin's `with_gate` reaches the call path.
/// Closed gate (default) → image becomes a withheld diagnostic; open gate
/// (store + image-input route) → the image is admitted as an attachment
/// reference and no base64 lands in the projected record.
#[tokio::test(flavor = "multi_thread")]
async fn plugin_gate_reaches_the_call_path() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let b64 = "aW1hZ2UtYnl0ZXM="; // "image-bytes"
    let make_store = || Arc::new(harnless_mcp::projection::InMemoryAttachmentStore::new());

    // Closed gate (the default): diagnostic, nothing stored.
    let (factory, _k) = scripted_factory("alpha", {
        let (calls, errors) = (calls.clone(), errors.clone());
        move || fake_server(tools_responder(vec!["pic"], calls.clone(), errors.clone()))
    });
    let plugin = McpServerPlugin::new(stdio_config("alpha"))
        .with_factory(factory)
        .with_keepalive_probe();
    let (bridge, _spy, _claims) = mount(plugin);
    let out = loop {
        match bridge.call("mcp__alpha__pic", CallId(1), b"{}") {
            Ok(v) => break v,
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // The closed-gate fake answers with text (tools_responder), so this
    // leg only proves the default mount projects text fine.
    assert_eq!(out["content"][0]["text"], json!("pic ok"));

    // Open gate: the fake answers the call with an image block.
    let store = make_store();
    let gate = harnless_mcp::projection::RichContentGate {
        store: Some(store.clone()),
        route: harnless_mcp::projection::RouteCapabilities { image_input: true },
    };
    let (factory2, _k2) = scripted_factory_gated(
        "beta",
        move || {
            fake_server(scripted(
                harnless_mcp::test_support::scripted_tools(&["pic"]),
                move |req: &Request| {
                    let _ = req;
                    Response::Result(json!({"content": [
                        {"type": "image", "mimeType": "image/png", "data": b64},
                    ]}))
                },
            ))
        },
        gate.clone(),
    );
    let plugin2 = McpServerPlugin::new(stdio_config("beta"))
        .with_factory(factory2)
        .with_gate(gate)
        .with_keepalive_probe();
    let bridge2 = Arc::new(McpToolBridge::new(Arc::new(SpyRegistry::default())));
    plugin2
        .activate(bridge2.clone(), Arc::new(ServerClaims::default()))
        .expect("open-gate plugin activates");
    let out = loop {
        match bridge2.call("mcp__beta__pic", CallId(2), b"{}") {
            Ok(v) if v["content"][0]["type"] == json!("image") => break v,
            Ok(v) => panic!("unexpected projection: {v}"),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    assert_eq!(out["content"][0]["mime"], json!("image/png"));
    assert!(out["content"][0]["attachment"]
        .as_str()
        .unwrap()
        .starts_with("attachment-"));
    assert!(
        !out.to_string().contains(b64),
        "base64 must not be projected"
    );
    assert_eq!(store.len(), 1, "payload went to the store");
}

/// A factory that publishes nothing and never returns (the probe hangs).
struct HangingFactory(&'static str);

impl TransportFactory for HangingFactory {
    fn run(&self, _sink: Arc<dyn GenerationSink>, stop: CancellationToken) -> Result<(), String> {
        futures::executor::block_on(stop.cancelled());
        Ok(())
    }
    fn server(&self) -> &str {
        self.0
    }
}

/// A factory that publishes one tool on the probe connection and hangs.
struct PublishThenHang(&'static str);

impl TransportFactory for PublishThenHang {
    fn run(&self, sink: Arc<dyn GenerationSink>, stop: CancellationToken) -> Result<(), String> {
        sink.publish(vec![(
            "t".to_string(),
            ToolDefinition {
                name: public_name(self.0, "t"),
                schema: json!({"type": "object"}),
                serialized: false,
            },
            Arc::new(NoopBody) as Arc<dyn ToolBody>,
        )]);
        futures::executor::block_on(stop.cancelled());
        Ok(())
    }
    fn server(&self) -> &str {
        self.0
    }
}

/// Activation must not block forever when a server never completes the
/// handshake: the probe is bounded, and a strict plugin fails the load.
#[test]
fn activation_probe_is_bounded() {
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let claims = Arc::new(ServerClaims::default());
    let plugin =
        McpServerPlugin::new(stdio_config("hang").call_timeout(Duration::from_millis(200)))
            .with_probe_timeout(Duration::from_millis(300))
            .with_factory(Arc::new(HangingFactory("hang")));
    let started = std::time::Instant::now();
    let result = plugin.activate(bridge, claims);
    let elapsed = started.elapsed();
    assert!(
        result.is_err(),
        "bounded probe must fail a hung strict server"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "probe returned promptly: {elapsed:?}"
    );
}

/// The probe handoff must not drop a generation published on the probe
/// connection: after activation the tools stay callable (the supervised
/// connection republishes; nothing from the probe is silently lost).
#[test]
fn probe_published_generation_survives_handoff() {
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let claims = Arc::new(ServerClaims::default());
    let plugin = McpServerPlugin::new(stdio_config("handoff"))
        .with_factory(Arc::new(PublishThenHang("handoff")));
    plugin
        .activate(bridge.clone(), claims)
        .expect("activation publishes the probe generation");
    assert_eq!(spy.names(), vec![public_name("handoff", "t")]);
    assert!(bridge.call("mcp__handoff__t", CallId(1), b"{}").is_ok());
}
