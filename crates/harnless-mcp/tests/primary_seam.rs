//! The primary-seam acceptance run: an in-memory fake MCP server over a
//! channel-backed transport, driven end-to-end through the bridge's real
//! production rmcp path (discovery → namespaced registration → raw-name
//! invocation → list-changed generation replacement → conflict rollback →
//! outage → capability-denied).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harnless_mcp::bridge::{McpServerPlugin, McpToolBridge, RegistryHandle, ServerClaims};
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
type NotifyVec = Arc<Mutex<Vec<futures::channel::mpsc::Sender<rmcp::model::ServerNotification>>>>;
type ListedVec = Arc<Mutex<Vec<std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>>>;
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
                description: "test tool".into(),
                schema: json!({"type": "object"}),
                serialized: false,
            },
            Arc::new(NoopBody) as Arc<dyn ToolBody>,
        ),
        (
            "y".into(),
            ToolDefinition {
                name: "mcp__beta__y".into(),
                description: "test tool".into(),
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
        vec![
            backoff_delay(&cfg, 1),
            backoff_delay(&cfg, 2),
            backoff_delay(&cfg, 3),
        ],
        "three reconnects (max_attempts=3) then stop"
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
                description: "test tool".into(),
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
                description: "test tool".into(),
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

// ---------------------------------------------------------------------------
// Review-round regressions (PR #48 independent review).
// ---------------------------------------------------------------------------

/// A registry that overwrites on re-registration (like the real spine): a
/// third party can take a name out-of-band, and re-registering replaces it.
#[derive(Default)]
struct MapRegistry {
    tools: Arc<Mutex<std::collections::HashMap<String, ToolDefinition>>>,
}

impl Tools for MapRegistry {
    fn register(&self, def: ToolDefinition, body: Arc<dyn ToolBody>) -> harnless_seams::Result<()> {
        let _ = body;
        self.tools.lock().insert(def.name.clone(), def);
        Ok(())
    }
    fn get(&self, name: &str) -> Option<ToolDefinition> {
        self.tools.lock().get(name).cloned()
    }
    fn names(&self) -> Vec<String> {
        self.tools.lock().keys().cloned().collect()
    }
}

/// P1a: a third party that took this server's public name out-of-band must
/// conflict on re-sync — the underlying registry is authoritative, not the
/// bridge's own liveness bookkeeping.
#[tokio::test(flavor = "multi_thread")]
async fn out_of_band_name_theft_conflicts_on_resync() {
    let reg = Arc::new(MapRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(reg.clone()));
    let handle = RegistryHandle::new(bridge.clone(), "alpha");
    let gen = |names: &[&'static str]| -> Generation {
        names
            .iter()
            .map(|n| {
                (
                    public_name("alpha", n),
                    ToolDefinition {
                        name: public_name("alpha", n),
                        description: "test tool".into(),
                        schema: json!({"type": "object"}),
                        serialized: false,
                    },
                    Arc::new(NoopBody) as Arc<dyn ToolBody>,
                )
            })
            .collect()
    };
    handle
        .replace_generation("alpha", gen(&["x", "y"]))
        .expect("first generation registers");
    // A third party takes mcp__alpha__y out-of-band (registry overwrite):
    // a *different* definition lands under our public name.
    reg.tools.lock().insert(
        public_name("alpha", "y"),
        ToolDefinition {
            name: public_name("alpha", "y"),
            description: "test tool".into(),
            schema: json!({"type": "string"}),
            serialized: true,
        },
    );
    // Re-sync including y: must conflict and roll back entirely.
    let err = handle
        .replace_generation("alpha", gen(&["x", "y"]))
        .expect_err("out-of-band theft must conflict on re-sync");
    assert!(
        err.contains(&public_name("alpha", "y")),
        "names the stolen tool: {err}"
    );
    // Rolled back: the attempted generation left nothing behind — the
    // registry still shows the thief's definition for y, and the bridge
    // did not silently re-shadow it.
    assert!(
        bridge.is_live("mcp__alpha__x"),
        "previous generation intact"
    );
    assert!(
        !bridge.is_live("mcp__alpha__y"),
        "stolen name must not be silently re-shadowed"
    );
}

/// P1b(i): `max_attempts` is the budget of *reconnects*; the initial
/// connection is not charged against it.
#[tokio::test(flavor = "multi_thread")]
async fn max_attempts_counts_reconnects_not_the_initial_connection() {
    struct AlwaysFail(&'static str);
    impl TransportFactory for AlwaysFail {
        fn run(
            &self,
            _sink: Arc<dyn GenerationSink>,
            _stop: CancellationToken,
        ) -> Result<(), String> {
            Err("down".to_string())
        }
        fn server(&self) -> &str {
            self.0
        }
    }
    #[derive(Default)]
    struct Counter(Mutex<Vec<SupervisorEvent>>);
    impl SupervisorObserver for Counter {
        fn on_event(&self, event: SupervisorEvent) {
            self.0.lock().push(event);
        }
    }
    #[derive(Default)]
    struct UnregSpy(Mutex<Vec<String>>);
    impl Registry for UnregSpy {
        fn replace_generation(&self, _s: &str, _t: Generation) -> Result<(), String> {
            Ok(())
        }
        fn unregister(&self, server: &str) {
            self.0.lock().push(server.to_string());
        }
    }
    let cfg = Arc::new(harnless_mcp::config::ReconnectConfig {
        enabled: true,
        backoff_initial: Duration::from_millis(1),
        backoff_ceiling: Duration::from_millis(1),
        max_attempts: 3,
    });
    let obs = Arc::new(Counter::default());
    let reg = Arc::new(UnregSpy::default());
    supervise(
        Arc::new(AlwaysFail("alpha")),
        reg.clone(),
        cfg,
        obs.clone(),
        Arc::new(|_| {}),
        CancellationToken::new(),
    );
    let events = obs.0.lock();
    let reconnects = events
        .iter()
        .filter(|e| matches!(e, SupervisorEvent::Reconnect { .. }))
        .count();
    assert_eq!(
        reconnects, 3,
        "max_attempts=3 grants 3 reconnects (initial connection is free)"
    );
    assert!(
        matches!(events.last(), Some(SupervisorEvent::Stopped { .. })),
        "supervisor stops with Stopped: {:?}",
        events.last()
    );
    assert_eq!(&*reg.0.lock(), &["alpha".to_string()]);
}

/// P1b(ii): after unload (stop cancelled), a late publish from an in-flight
/// connection must not re-register tools.
#[tokio::test(flavor = "multi_thread")]
async fn late_publish_after_unload_does_not_reregister() {
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let handle = Arc::new(RegistryHandle::new(bridge.clone(), "alpha"));
    let stop = CancellationToken::new();
    // The mount's unload path: stop, then unregister.
    let disposed = handle.disposed_token();
    stop.cancel();
    disposed.cancel();
    bridge.unregister_server("alpha");
    // A late publish from a connection that had not noticed the stop yet.
    handle.publish_after_dispose(vec![(
        "x".to_string(),
        ToolDefinition {
            name: public_name("alpha", "x"),
            description: "test tool".into(),
            schema: json!({"type": "object"}),
            serialized: false,
        },
        Arc::new(NoopBody) as Arc<dyn ToolBody>,
    )]);
    assert!(
        spy.names().is_empty(),
        "disposed handle must not re-register: {:?}",
        spy.names()
    );
}

/// P2a: when a name disappears and later reappears, the server re-adopts
/// its own orphan forwarder instead of conflicting with itself.
#[tokio::test(flavor = "multi_thread")]
async fn orphan_forwarder_reappearance_is_readopted_not_conflict() {
    let reg = Arc::new(MapRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(reg.clone()));
    let handle = RegistryHandle::new(bridge.clone(), "alpha");
    let gen = |names: &[&'static str]| -> Generation {
        names
            .iter()
            .map(|n| {
                (
                    public_name("alpha", n),
                    ToolDefinition {
                        name: public_name("alpha", n),
                        description: "test tool".into(),
                        schema: json!({"type": "object"}),
                        serialized: false,
                    },
                    Arc::new(NoopBody) as Arc<dyn ToolBody>,
                )
            })
            .collect()
    };
    handle
        .replace_generation("alpha", gen(&["x", "y"]))
        .expect("registers");
    // y disappears from the server's list.
    handle
        .replace_generation("alpha", gen(&["x"]))
        .expect("shrink ok");
    // y reappears: the orphan forwarder (still in the registry, ours) must
    // be re-adopted, not treated as a conflict.
    handle
        .replace_generation("alpha", gen(&["x", "y"]))
        .expect("re-adoption must not conflict");
    assert!(bridge.is_live("mcp__alpha__y"));
    // And the call still routes through the bridge.
    assert!(bridge.call("mcp__alpha__y", CallId(9), b"{}").is_ok());
}

/// P2b: a failed re-discovery must reach the supervisor observer as a
/// distinct Outage event, not vanish into a sink call no consumer acts on.
#[tokio::test(flavor = "multi_thread")]
async fn rediscovery_failure_surfaces_as_outage_event() {
    #[derive(Default)]
    struct Events(Mutex<Vec<SupervisorEvent>>);
    impl SupervisorObserver for Events {
        fn on_event(&self, event: SupervisorEvent) {
            if !matches!(event, SupervisorEvent::Connected { .. }) {
                self.0.lock().push(event);
            }
        }
    }
    let notify: NotifyVec = Arc::new(Mutex::new(Vec::new()));
    let listed: ListedVec = Arc::new(Mutex::new(Vec::new()));
    let kills: Arc<Mutex<Vec<tokio::sync::oneshot::Sender<()>>>> = Arc::new(Mutex::new(Vec::new()));
    struct F(
        AtomicUsize,
        NotifyVec,
        ListedVec,
        Arc<Mutex<Vec<tokio::sync::oneshot::Sender<()>>>>,
    );
    impl TransportFactory for F {
        fn run(
            &self,
            sink: Arc<dyn GenerationSink>,
            stop: CancellationToken,
        ) -> Result<(), String> {
            let fake = fake_server(scripted(
                harnless_mcp::test_support::scripted_tools(&["x"]),
                move |req: &Request| {
                    let _ = req;
                    Response::Result(json!({"content": [{"type": "text", "text": "ok"}]}))
                },
            ));
            let listed_rx = fake.listed.lock().unwrap().take();
            self.2.lock().push(std::sync::Mutex::new(listed_rx));
            self.1.lock().push(fake.notify_tx.clone());
            if let Some(k) = fake.kill.lock().take() {
                self.3.lock().push(k);
            }
            let (outcome, _peer) = harnless_mcp::supervisor::serve_fake_keepalive(
                fake.transport,
                sink.clone(),
                stop,
                "alpha".to_string(),
                Duration::from_millis(400),
                harnless_mcp::projection::RichContentGate::closed(),
            );
            outcome
        }
        fn server(&self) -> &str {
            "alpha"
        }
    }
    let factory = Arc::new(F(
        AtomicUsize::new(0),
        notify.clone(),
        listed.clone(),
        kills.clone(),
    ));
    let obs = Arc::new(Events::default());
    let plugin =
        McpServerPlugin::new(stdio_config("alpha").call_timeout(Duration::from_millis(400)))
            .with_factory(factory)
            .with_observer(obs.clone())
            .with_keepalive_probe();
    let (_bridge, _spy, _claims) = mount(plugin);
    // Wait for the supervised connection (second) to answer tools/list.
    let rx: Option<tokio::sync::oneshot::Receiver<()>> = loop {
        let mut guard = listed.lock();
        if guard.len() >= 2 {
            break guard.get_mut(1).and_then(|g| g.lock().unwrap().take());
        }
        drop(guard);
        std::thread::sleep(Duration::from_millis(10));
    };
    if let Some(rx) = rx {
        std::thread::scope(|s| {
            s.spawn(|| {
                let _ = rx.blocking_recv();
            })
            .join()
        })
        .ok();
    }
    // Fire list_changed at the supervised connection...
    {
        let mut tx = notify.lock().get(1).cloned().expect("supervised notify tx");
        let _ = futures::SinkExt::send(
            &mut tx,
            rmcp::model::ServerNotification::ToolListChangedNotification(
                rmcp::model::NotificationNoParam::default(),
            ),
        )
        .await;
    }
    // ...and kill its transport mid-re-discovery: the re-discovery fails,
    // and the failure must be observable, not silent.
    {
        let k = kills.lock().remove(1);
        let _ = k.send(());
    }
    let mut seen = false;
    for _ in 0..300 {
        if obs
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, SupervisorEvent::Outage { .. }))
        {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        seen,
        "re-discovery/transport failure must surface as an Outage observer event: {:?}",
        obs.0
            .lock()
            .iter()
            .map(|e| format!("{e:?}"))
            .collect::<Vec<_>>()
    );
}

/// P2e: the collision suffix is a pure function of (server, raw) — a
/// re-sync where the colliding sibling disappeared must NOT rename the
/// survivor.
#[tokio::test(flavor = "multi_thread")]
async fn collision_suffix_survives_sibling_removal() {
    // Two raws whose normalization collides ("a-b" and "a.b" → "a_b").
    let both = harnless_mcp::naming::public_names("s", &["a-b".into(), "a.b".into()]);
    let (_, name_ab) = &both[0];
    // Re-sync with only the first sibling present.
    let solo = harnless_mcp::naming::public_names("s", &["a-b".into()]);
    assert_eq!(
        solo[0].1, *name_ab,
        "public name must not change when a colliding sibling disappears"
    );
    // And the other sibling keeps its own stable name too.
    let solo2 = harnless_mcp::naming::public_names("s", &["a.b".into()]);
    assert_eq!(solo2[0].1, both[1].1);
}

/// P2c: a call that exceeds the per-call timeout must send the MCP
/// `notifications/cancelled` notification for the pending request id —
/// the server learns the call is abandoned, not just the client future.
#[test]
fn call_timeout_sends_cancelled_notification() {
    let hang = Arc::new(AtomicUsize::new(0));
    let hang2 = hang.clone();
    // The fake answers everything; the FIRST `tools/call` hangs (the
    // timeout leg), later calls answer normally.
    let fake = fake_server(Arc::new(move |req: &Request| match req.method.as_str() {
        "initialize" => Some(Response::initialize(false)),
        "tools/list" => Some(Response::tools(harnless_mcp::test_support::scripted_tools(
            &["slow"],
        ))),
        "tools/call" => {
            if hang2.fetch_add(1, Ordering::SeqCst) == 0 {
                None
            } else {
                Some(Response::Result(json!({"content": []})))
            }
        }
        _ => Some(Response::Result(json!({}))),
    }));
    let peer = harnless_mcp::supervisor::serve_fake_peer_now(
        fake.transport,
        "alpha".to_string(),
        Duration::from_millis(200),
    );
    let body = harnless_mcp::supervisor::call_body_for_test(
        "slow".to_string(),
        peer.clone(),
        Duration::from_millis(200),
    );
    let err = body
        .run(CallId(1), b"{}")
        .expect_err("hanging call must time out");
    assert_eq!(err.code, ErrorCode::ToolTimeout);
    // The fake's own transcript records notifications too.
    let mut saw_cancelled = false;
    for _ in 0..100 {
        if fake
            .seen
            .lock()
            .iter()
            .any(|r| r.method == "notifications/cancelled")
        {
            saw_cancelled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        saw_cancelled,
        "timeout must emit notifications/cancelled; fake saw {:?}",
        fake.seen
            .lock()
            .iter()
            .map(|r| r.method.clone())
            .collect::<Vec<_>>()
    );
    // And the cancellation names the abandoned request.
    let cancelled = fake
        .seen
        .lock()
        .iter()
        .find(|r| r.method == "notifications/cancelled")
        .expect("cancelled notification")
        .params
        .clone();
    assert_eq!(
        cancelled["requestId"],
        json!(2),
        "names the pending call: {cancelled}"
    );
}

/// P3: a strict load failure must surface the real detail, not a
/// constant. `RuntimeError` messages are `&'static str` (runtime
/// contract), so the detail travels through the load boundary's log line.
#[test]
fn strict_failure_detail_is_logged() {
    use tracing_subscriber::layer::{Layer, SubscriberExt};
    struct CapturingLayer(Mutex<Vec<String>>);
    impl<S: tracing::Subscriber> Layer<S> for CapturingLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Vec::new();
            event.record(&mut CollectFields(&mut fields));
            self.0.lock().push(fields.join(" "));
        }
    }
    struct CollectFields<'a>(&'a mut Vec<String>);
    impl tracing::field::Visit for CollectFields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push(format!("{}={value:?}", field.name()));
        }
    }
    let captured = Arc::new(Mutex::new(Vec::new()));
    struct SharedLayer(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> Layer<S> for SharedLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Vec::new();
            event.record(&mut CollectFields(&mut fields));
            self.0.lock().push(fields.join(" "));
        }
    }
    // A process-wide default is required: the probe logs from its own
    // thread, where a thread-local default is invisible.
    let subscriber =
        tracing_subscriber::registry::Registry::default().with(SharedLayer(captured.clone()));
    let _ = tracing::subscriber::set_global_default(subscriber);
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let mut config = stdio_config("detail");
    config.strict = true;
    let plugin = McpServerPlugin::new(config)
        .with_factory(Arc::new(DyingFactory("transport refused by peer")));
    let result = plugin.activate(bridge, Arc::new(ServerClaims::default()));
    assert!(result.is_err());
    let logged = captured.lock().clone();
    assert!(
        logged.iter().any(|line| line.contains("transport refused")),
        "the real failure detail must reach the log: {logged:?}"
    );
}

/// Review P2: a stolen-name poison must not outlive the theft. Once the
/// third party vacates the name, the next re-sync must re-establish our
/// forwarder and calls must flow again — not a permanently-dead tool.
#[tokio::test(flavor = "multi_thread")]
async fn stolen_name_recovers_after_the_thief_vacates() {
    let reg = Arc::new(MapRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(reg.clone()));
    let handle = RegistryHandle::new(bridge.clone(), "alpha");
    let gen = |names: &[&'static str]| -> Generation {
        names
            .iter()
            .map(|n| {
                (
                    public_name("alpha", n),
                    ToolDefinition {
                        name: public_name("alpha", n),
                        description: "test tool".into(),
                        schema: json!({"type": "object"}),
                        serialized: false,
                    },
                    Arc::new(NoopBody) as Arc<dyn ToolBody>,
                )
            })
            .collect()
    };
    // init
    handle
        .replace_generation("alpha", gen(&["x"]))
        .expect("first generation registers");
    // steal: a third party lands a different definition under our name.
    reg.tools.lock().insert(
        public_name("alpha", "x"),
        ToolDefinition {
            name: public_name("alpha", "x"),
            description: "test tool".into(),
            schema: json!({"type": "string"}),
            serialized: true,
        },
    );
    // conflict: re-sync must roll back entirely.
    let err = handle
        .replace_generation("alpha", gen(&["x"]))
        .expect_err("theft must conflict");
    assert!(err.contains("non-MCP tool"), "theft conflict: {err}");
    // unsteal: the third party vacates the name.
    let vacated: Option<ToolDefinition> = reg.tools.lock().remove(&public_name("alpha", "x"));
    assert!(vacated.is_some(), "the thief held the name");
    // re-sync: must succeed and re-establish our forwarder.
    handle
        .replace_generation("alpha", gen(&["x"]))
        .expect("re-sync after the thief vacates must succeed");
    assert!(bridge.is_live("mcp__alpha__x"), "name is live again");
    // calls must flow again.
    let out = bridge
        .call("mcp__alpha__x", CallId(1), b"{}")
        .expect("call must succeed after recovery");
    let _ = out;
}
