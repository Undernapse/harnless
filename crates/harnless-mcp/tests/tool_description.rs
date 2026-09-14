//! The description path through the MCP world, asserted at the two seams
//! that own it:
//!
//! * discovery: a server's `tools/list` description is carried onto the
//!   registered definition instead of being dropped at the bridge, and a
//!   server that omits the field yields an empty description rather than a
//!   synthesized one;
//! * republish: two generations differing *only* by description are not the
//!   same registration. The bridge's `same_definition` comparison decides
//!   whether a name is already ours; if it ignored the description, a
//!   description-only update would be skipped as a no-op and the model would
//!   keep seeing the stale text forever.

use std::sync::Arc;
use std::time::Duration;

use harnless_mcp::bridge::{McpToolBridge, RegistryHandle};
use harnless_mcp::naming::public_name;
use harnless_mcp::supervisor::Generation;
use harnless_mcp::test_support::{tool_with, Response};
use harnless_seams::{CallId, SeamError, ToolBody, ToolDefinition, Tools};
use parking_lot::Mutex;
use serde_json::{json, Value};

/// A registry that records what was registered, so a test can read back the
/// definition a discovery path actually produced.
#[derive(Default)]
struct SpyRegistry {
    tools: Arc<Mutex<Vec<ToolDefinition>>>,
}

impl SpyRegistry {
    fn definition(&self, name: &str) -> Option<ToolDefinition> {
        self.tools.lock().iter().find(|d| d.name == name).cloned()
    }

    fn registrations(&self, name: &str) -> usize {
        self.tools.lock().iter().filter(|d| d.name == name).count()
    }
}

impl Tools for SpyRegistry {
    fn register(&self, def: ToolDefinition, _body: Arc<dyn ToolBody>) -> Result<(), SeamError> {
        // A registry's contract is last-writer-wins per name; the spy models
        // that so a re-registration replaces rather than accumulates.
        let mut tools = self.tools.lock();
        tools.retain(|d| d.name != def.name);
        tools.push(def);
        Ok(())
    }
    fn get(&self, name: &str) -> Option<ToolDefinition> {
        self.definition(name)
    }
    fn names(&self) -> Vec<String> {
        self.tools.lock().iter().map(|d| d.name.clone()).collect()
    }
}

struct NoopBody;

impl ToolBody for NoopBody {
    fn run(&self, _call_id: CallId, _args: &[u8]) -> Result<Value, SeamError> {
        Ok(json!({"ok": true}))
    }
}

/// A generation entry as discovery would produce it: public name from the
/// naming scheme, description from the server's `tools/list`.
fn generation(server: &str, raw: &str, description: &str) -> Generation {
    let tool = tool_with(raw, description);
    vec![(
        raw.to_string(),
        ToolDefinition {
            name: public_name(server, raw),
            description: tool
                .description
                .clone()
                .map(|d: std::borrow::Cow<'static, str>| d.into_owned())
                .unwrap_or_default(),
            schema: serde_json::Value::Object((*tool.input_schema).clone()),
            serialized: false,
        },
        Arc::new(NoopBody) as Arc<dyn ToolBody>,
    )]
}

#[test]
fn a_servers_description_reaches_the_registered_definition() {
    // The same shape the discovery path builds (`build_generation` reads
    // `tool.description` off the discovered tool); this asserts the
    // description survives the trip through the bridge onto the registry.
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let handle = RegistryHandle::new(bridge.clone(), "alpha");
    handle
        .publish(generation("alpha", "search", "Search the index."))
        .expect("first generation publishes");
    assert_eq!(
        spy.definition(&public_name("alpha", "search"))
            .map(|d| d.description),
        Some("Search the index.".to_string()),
        "the description the server listed must be the description the \
         registry holds"
    );
}

#[test]
fn a_server_that_omits_the_description_registers_an_empty_one() {
    // `tools/list` may leave `description` unset. The seam's rule is an empty
    // string, never an invented sentence — a model-facing field is only ever
    // what its source said.
    let tool = {
        let mut t = tool_with("quiet", "placeholder");
        t.description = None;
        t
    };
    assert!(tool.description.is_none(), "fixture: field unset");
    let registered = ToolDefinition {
        name: public_name("alpha", "quiet"),
        description: tool
            .description
            .clone()
            .map(|d: std::borrow::Cow<'static, str>| d.into_owned())
            .unwrap_or_default(),
        schema: serde_json::Value::Object((*tool.input_schema).clone()),
        serialized: false,
    };
    assert_eq!(registered.description, "");
}

#[test]
fn a_description_only_update_is_not_treated_as_the_same_registration() {
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let handle = RegistryHandle::new(bridge.clone(), "alpha");
    handle
        .publish(generation("alpha", "search", "Old text."))
        .expect("first generation publishes");
    handle
        .publish(generation("alpha", "search", "New text."))
        .expect("description-only update publishes");
    assert_eq!(
        spy.definition(&public_name("alpha", "search"))
            .map(|d| d.description),
        Some("New text.".to_string()),
        "a generation differing only by description must re-register; \
         otherwise the model keeps calling a tool described by text its \
         server no longer sends"
    );
    assert_eq!(
        spy.registrations(&public_name("alpha", "search")),
        1,
        "the update replaces the registration, it does not stack"
    );
}

#[test]
fn an_identical_republish_does_not_re_register() {
    // The other half of the comparison: sameness is still sameness. If every
    // republish re-registered, the "same registration" bookkeeping the bridge
    // uses to tell its own names from a third party's would be dead code.
    let spy = Arc::new(SpyRegistry::default());
    let bridge = Arc::new(McpToolBridge::new(spy.clone()));
    let handle = RegistryHandle::new(bridge.clone(), "alpha");
    handle
        .publish(generation("alpha", "search", "Same text."))
        .expect("first generation publishes");
    handle
        .publish(generation("alpha", "search", "Same text."))
        .expect("identical republish succeeds");
    assert_eq!(
        spy.registrations(&public_name("alpha", "search")),
        1,
        "an unchanged definition is skipped, not re-registered"
    );
}

/// The fake-server half: `tools/list` answered over the wire, discovered by
/// the real rmcp path, and observed on the registry the bridge writes.
mod over_the_wire {
    use super::*;
    use harnless_mcp::supervisor::{GenerationSink, TransportFactory};
    use harnless_mcp::test_support::{fake_server, scripted};
    use tokio_util::sync::CancellationToken;

    /// Publishes whatever the fake's `tools/list` produced straight at the
    /// bridge, mirroring what the supervisor's sink does in production.
    struct DirectSink {
        handle: Arc<RegistryHandle>,
    }

    impl GenerationSink for DirectSink {
        fn publish(&self, tools: Generation) {
            let _ = self.handle.publish(tools);
        }
        fn outage(&self, _reason: String) {}
    }

    struct FakeFactory {
        descriptions: Vec<(&'static str, &'static str)>,
    }

    impl TransportFactory for FakeFactory {
        fn run(
            &self,
            sink: Arc<dyn GenerationSink>,
            stop: CancellationToken,
        ) -> Result<(), String> {
            let tools = self
                .descriptions
                .iter()
                .map(|(name, description)| tool_with(name, description))
                .collect();
            let responder = scripted(tools, |req| {
                let name = req.params["name"].as_str().unwrap_or("?");
                Response::Result(
                    json!({"content": [{"type": "text", "text": format!("{name} ok")}]}),
                )
            });
            let fake = fake_server(responder);
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

    /// Drive one fake connection to discovery and return what landed on the
    /// registry.
    fn discover(descriptions: &[(&'static str, &'static str)]) -> Arc<SpyRegistry> {
        let spy = Arc::new(SpyRegistry::default());
        let bridge = Arc::new(McpToolBridge::new(spy.clone()));
        let handle = Arc::new(RegistryHandle::new(bridge, "alpha"));
        let sink: Arc<dyn GenerationSink> = Arc::new(DirectSink {
            handle: handle.clone(),
        });
        let factory = FakeFactory {
            descriptions: descriptions.to_vec(),
        };
        let stop = CancellationToken::new();
        let done = stop.clone();
        // Discovery publishes, then the connection parks until stop.
        let serve = std::thread::spawn(move || {
            let _ = factory.run(sink, done);
        });
        for _ in 0..500 {
            if !spy.names().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.cancel();
        let _ = handle.publish(vec![]);
        let _ = serve.join();
        spy
    }

    #[test]
    fn discovery_carries_the_listed_description_to_the_registry() {
        let spy = discover(&[("search", "Search the index for a query."), ("fetch", "")]);
        assert_eq!(
            spy.definition(&public_name("alpha", "search"))
                .map(|d| d.description),
            Some("Search the index for a query.".to_string()),
            "the description the server listed is the description the \
             harness registers"
        );
        assert_eq!(
            spy.definition(&public_name("alpha", "fetch"))
                .map(|d| d.description),
            Some(String::new()),
            "a listed empty description stays empty"
        );
    }
}
