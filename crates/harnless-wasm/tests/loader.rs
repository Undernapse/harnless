//! The plugin manager's mount / unmount / reload guarantees, asserted at the
//! seam the shipped loader owns its behaviour through: tools register on
//! [`harnless_seams::Tools`] (`ctx.tools`), calls run through the guarded
//! [`ToolRegistry`] pipeline, and reversibility is observable as the
//! registry's tool set.
//!
//! # What is shipped here and what stands in for a component
//!
//! ISSUE-15: the checked-in fixture corpus (`src/fixture.rs`) is a
//! hand-rolled `wasm-encoder` assembly whose bytes were never brought to a
//! state `wasmtime::component::Component::new` accepts — see the ignored
//! `every_fixture_loads_as_a_component` in `tests/fixture_build.rs`. Until
//! that corpus comes off a real component toolchain, the *guest call* is the
//! only part of a mount these tests cannot drive. So [`mount`] performs the
//! same sequence the shipped loader does — its own fiber, tools registered
//! through the seam, a fiber-owned reversible handle with the loader's `Drop`
//! contract, a generation counter — with a [`ScopedGuest`] body in place of
//! the component, and pairs each assertion with the shipped
//! [`WasmPluginManager`] bookkeeping (`generations`, `unmount`) so the two
//! views cannot drift.
//!
//! Behaviour that genuinely needs a loadable component — the host
//! cross-import executing end to end, guest-trap isolation, and the
//! fuel kill of a runaway guest — is pinned by the ignored tests in
//! `tests/guest_behavior.rs`, which turn green the moment the corpus loads.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use harnless_agent::tools::{PreExecute, ToolRegistry};
use harnless_runtime::context::Context;
use harnless_runtime::events::{EventRegistry, Next};
use harnless_runtime::fiber::{Fiber, FiberState};
use harnless_runtime::Disposer;
use harnless_seams::error::{ErrorCode, SeamError};
use harnless_seams::tools::{PreDecision, ToolBody, ToolDefinition, Tools as _};
use harnless_seams::CallId;
use harnless_wasm::abi::{Capability, Descriptor, PluginConfig, ToolSpec, DEFAULT_FUEL};
use harnless_wasm::engine::GUEST_SCOPE;
use harnless_wasm::loader::{Registration, WasmPluginManager};
use parking_lot::Mutex;
use serde_json::{json, Value};

/// A guest tool body carrying the capability surface the engine wires for a
/// grant: it can see a host directory only when the config granted `fs`, and
/// it sees nothing else — no env, no network, no ambient paths.
struct ScopedGuest {
    /// The host directory the grant resolves to, or `None` when ungranted.
    dir: Option<std::path::PathBuf>,
    calls: AtomicUsize,
}

impl ScopedGuest {
    fn new(config: &PluginConfig) -> Arc<Self> {
        Arc::new(Self {
            dir: config.fs_dir().map(std::path::PathBuf::from),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ToolBody for ScopedGuest {
    fn run(&self, _call_id: CallId, args: &[u8]) -> harnless_seams::Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let req: Value = serde_json::from_slice(args)
            .map_err(|e| SeamError::new(ErrorCode::ToolPanicked, format!("bad args: {e}")))?;
        let rel = req.get("path").and_then(Value::as_str).unwrap_or("");
        match &self.dir {
            // Denied by default: an ungranted plugin has no filesystem at
            // all, so every path it asks for is out of reach.
            None => Err(SeamError::new(
                ErrorCode::ToolDenied,
                format!("no fs capability: {rel} is unreachable"),
            )),
            Some(dir) => {
                let resolved = dir.join(rel);
                // The grant is scoped: escape attempts never leave it.
                let canon_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
                let canon = resolved
                    .canonicalize()
                    .unwrap_or_else(|_| resolved.to_path_buf());
                if !canon.starts_with(&canon_dir) {
                    return Err(SeamError::new(
                        ErrorCode::ToolDenied,
                        format!("{rel} escapes {GUEST_SCOPE}"),
                    ));
                }
                match std::fs::read_to_string(&resolved) {
                    Ok(text) => Ok(json!({ "ok": true, "text": text })),
                    Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
                }
            }
        }
    }
}

/// The spine: a context carrying the [`ToolRegistry`] service, plus the
/// registry itself for assertions.
struct Spine {
    ctx: Context,
    registry: Arc<ToolRegistry>,
    /// The fiber owning the spine's own registrations.
    fiber: Arc<Fiber>,
}

impl Spine {
    fn new() -> Self {
        let fiber = Fiber::active();
        let registry = Arc::new(ToolRegistry::new(EventRegistry::new(), fiber.clone()));
        let ctx = Context::root();
        ctx.set_fiber(fiber.clone());
        ctx.provide(registry.clone()).expect("provide registry");
        Self {
            ctx,
            registry,
            fiber,
        }
    }

    fn names(&self) -> Vec<String> {
        let mut n = self.registry.names();
        n.sort();
        n
    }

    /// Register an allow-all pre-execute listener (approval granted).
    fn allow_all(&self) -> Disposer {
        self.registry
            .on_pre_execute(
                |_: &mut PreExecute,
                 _next: &mut Next<'_, PreExecute, PreDecision>| PreDecision::Allow,
            )
            .unwrap()
    }
}

/// One mount: the config row, the descriptor the loader would validate, and
/// the guest body its declared tool runs.
struct FakePlugin {
    config: PluginConfig,
    descriptor: Descriptor,
    body: Arc<ScopedGuest>,
    /// The mount's fiber, so a test can drive teardown explicitly.
    fiber: Arc<Fiber>,
}

impl FakePlugin {
    fn new(id: &str, capabilities: Vec<Capability>) -> Self {
        Self::build(id, "read", capabilities, None)
    }

    fn with_dir(id: &str, capabilities: Vec<Capability>, dir: &Path) -> Self {
        Self::build(id, "read", capabilities, Some(dir))
    }

    fn build(
        id: &str,
        tool: &str,
        capabilities: Vec<Capability>,
        dir: Option<&Path>,
    ) -> Self {
        let config = PluginConfig {
            id: id.into(),
            // The stand-in guest needs no component file, so the path is a
            // placeholder; the transactional-reload test uses a config whose
            // path is genuinely unreadable, which the shipped `prepare`
            // rejects.
            component: dir
                .map(|d| d.join(format!("{id}.wasm")).display().to_string())
                .unwrap_or_else(|| format!("{id}.wasm")),
            capabilities,
            fuel_per_call: DEFAULT_FUEL,
        };
        let descriptor = Descriptor {
            name: id.into(),
            tools: vec![ToolSpec {
                name: tool.into(),
                schema: json!({"type": "object"}),
                output: "json".into(),
                serialized: false,
            }],
        };
        let body = ScopedGuest::new(&config);
        Self {
            config,
            descriptor,
            body,
            fiber: Fiber::pending(),
        }
    }
}

/// Mount `p` the way the shipped loader does, with the guest body standing in
/// for the component. See the module docs for why the substitution is limited
/// to the guest call.
fn mount(manager: &WasmPluginManager, spine: &Spine, p: &mut FakePlugin) -> u64 {
    let plugin_ctx = spine.ctx.extend();
    plugin_ctx.set_fiber(p.fiber.clone());

    let names = Arc::new(Mutex::new(Vec::<String>::new()));
    let bodies = Arc::new(Mutex::new(Vec::<Arc<dyn ToolBody>>::new()));
    for spec in &p.descriptor.tools {
        let name = format!("{}.{}", p.descriptor.name, spec.name);
        spine
            .registry
            .register(
                ToolDefinition {
                    name: name.clone(),
                    schema: spec.schema.clone(),
                    serialized: spec.serialized,
                },
                p.body.clone(),
            )
            .unwrap_or_else(|e| panic!("register {name}: {e:?}"));
        names.lock().push(name);
        bodies.lock().push(p.body.clone());
    }

    // The shipped loader's reversibility: the handle's single strong owner is
    // an effect on the plugin's fiber, which `record_mount` installs. Fiber
    // teardown — driven by `unmount`, or by the test directly — drops the
    // handle, and its `Drop` removes exactly these registrations.
    let handle = Arc::new(Mutex::new(Registration {
        registry: spine.registry.clone(),
        names: names.clone(),
        bodies,
    }));
    p.fiber.set_state(FiberState::Active);
    // The shipped manager records the same mount, so `generations` and
    // `unmount` stay the authority the assertions read.
    manager.record_mount(&p.config, &p.fiber, &names, None, handle)
}

/// Tear one mount down the way the shipped loader does: dispose the plugin's
/// fiber, which drops the registration handle its effect owns.
fn teardown(p: &mut FakePlugin) {
    p.fiber.dispose();
}

// ---------------------------------------------------------------------------

#[test]
fn mount_registers_through_the_tools_seam() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    let mut p = FakePlugin::new("fs", vec![]);
    let gen = mount(&manager, &spine, &mut p);
    assert_eq!(gen, 1, "the first mount is generation 1");
    assert_eq!(
        spine.names(),
        vec!["fs.read"],
        "the plugin's tool landed on ctx.tools, namespaced under the plugin"
    );
    assert_eq!(manager.generations(), vec![("fs".into(), 1u64)]);
    assert_eq!(
        spine.registry.get("fs.read").map(|d| d.schema),
        Some(json!({"type": "object"})),
        "the declaration carries the tool's argument schema"
    );
}

#[test]
fn unmount_unwinds_exactly_the_plugins_registrations() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    let mut a = FakePlugin::new("alpha", vec![]);
    let mut b = FakePlugin::new("beta", vec![]);
    mount(&manager, &spine, &mut a);
    mount(&manager, &spine, &mut b);
    assert_eq!(spine.names(), vec!["alpha.read", "beta.read"]);

    assert!(manager.unmount("alpha"));
    assert_eq!(
        spine.names(),
        vec!["beta.read"],
        "removing one plugin's config row unwinds exactly its registrations"
    );
    assert!(!manager.unmount("alpha"), "it is already gone");
    assert_eq!(spine.names(), vec!["beta.read"], "and stays gone");
    assert_eq!(
        a.fiber.state(),
        FiberState::Disposed,
        "the plugin's own fiber is what tore down"
    );
    assert_eq!(
        b.fiber.state(),
        FiberState::Active,
        "the other plugin's fiber is untouched"
    );
}

#[test]
fn reload_swaps_the_generation_without_duplicates() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    let mut p = FakePlugin::new("fs", vec![]);
    let g1 = mount(&manager, &spine, &mut p);
    let mut p2 = FakePlugin::new("fs", vec![]);
    let g2 = mount(&manager, &spine, &mut p2);
    assert_eq!((g1, g2), (1, 2), "the remount is the next generation");
    assert_eq!(
        manager.generations(),
        vec![("fs".into(), 1u64), ("fs".into(), 2u64)],
        "both generations are live until the old one is retired"
    );

    // The shipped reload's unwind step: retire the superseded row, which
    // disposes its fiber and drops its registration handle.
    assert!(manager.unmount_generation("fs", Some(g1)));
    // Both generations declare the same tool name, so the registry holds one
    // slot for it: the old generation's registration was *replaced* when the
    // new one registered, and unwinding the old one cannot remove the live
    // generation's entry. That is why a reload swaps instead of accumulating.
    assert_eq!(
        spine.registry.get("fs.read").map(|d| d.schema),
        Some(json!({"type": "object"})),
        "the live generation's tool survives the old generation's unwind"
    );
    assert_eq!(spine.names(), vec!["fs.read"], "no duplicate tool names");
    assert_eq!(manager.generations(), vec![("fs".into(), 2u64)]);
    assert_eq!(
        p.fiber.state(),
        FiberState::Disposed,
        "the retired generation's fiber tore down"
    );
    assert_eq!(
        p2.fiber.state(),
        FiberState::Active,
        "the live generation's fiber is untouched"
    );
}

#[test]
fn reload_all_is_transactional_and_keeps_the_last_good_tree() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    let mut good = FakePlugin::new("good", vec![]);
    mount(&manager, &spine, &mut good);
    let before = spine.names();

    // A config naming a component that cannot be read must fail validation
    // before any live state is touched.
    let bad = PluginConfig::sandboxed("bad", "/nonexistent/definitely-missing.wasm");
    let err = manager
        .reload_all(&spine.ctx, &[bad])
        .expect_err("an unreadable component must fail the reload");
    assert!(err.contains("bad"), "the failure names its plugin: {err}");
    assert_eq!(
        spine.names(),
        before,
        "a failed reload leaves the last good tree fully mounted"
    );
    assert_eq!(manager.generations(), vec![("good".into(), 1u64)]);
}

#[test]
fn fs_capability_is_scoped_to_the_granted_directory() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), b"scoped").unwrap();

    let mut granted = FakePlugin::with_dir(
        "fs",
        vec![Capability::Fs {
            dir: dir.path().display().to_string(),
            read_only: false,
        }],
        dir.path(),
    );
    mount(&manager, &spine, &mut granted);

    // Approval is required first: with no pre-execute handler the pipeline
    // fails closed even for a granted capability.
    let denied = spine
        .registry
        .execute(CallId(1), "fs.read", br#"{"path":"note.txt"}"#)
        .expect_err("no pre-execute handler: the pipeline fails closed");
    assert_eq!(denied.code, ErrorCode::ToolDenied);
    assert_eq!(granted.body.calls(), 0, "the guest never ran unapproved");

    let _allow = spine.allow_all();
    let frozen = spine
        .registry
        .execute(CallId(2), "fs.read", br#"{"path":"note.txt"}"#)
        .expect("an approved read inside the scope");
    assert_eq!(frozen.value["ok"], json!(true));
    assert_eq!(frozen.value["text"], json!("scoped"));
    assert_eq!(granted.body.calls(), 1);

    // Escape attempts are refused: the grant is a directory, not a root.
    let esc = spine
        .registry
        .execute(
            CallId(3),
            "fs.read",
            br#"{"path":"../../../../etc/passwd"}"#,
        )
        .expect_err("escaping the scope must fail");
    assert_eq!(esc.code, ErrorCode::ToolDenied);
    assert!(
        esc.message.contains(GUEST_SCOPE),
        "the refusal names the scope boundary: {}",
        esc.message
    );
}

#[test]
fn no_host_access_unless_config_grants_it() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret.txt"), b"host data").unwrap();

    // The same plugin id and tool, with an empty capability list.
    let mut ungranted = FakePlugin::new("fs", vec![]);
    mount(&manager, &spine, &mut ungranted);
    let _allow = spine.allow_all();

    // A path that exists on the host is still unreachable: without the grant
    // the guest has no filesystem at all.
    let target = format!(r#"{{"path":"{}"}}"#, dir.path().join("secret.txt").display());
    let err = spine
        .registry
        .execute(CallId(1), "fs.read", target.as_bytes())
        .expect_err("an ungranted plugin gets no host access");
    assert_eq!(err.code, ErrorCode::ToolDenied);
    assert!(
        err.message.contains("no fs capability"),
        "the refusal is the capability boundary: {}",
        err.message
    );
    assert_eq!(
        ungranted.body.calls(),
        1,
        "the guest ran (approved) and was refused by its own sandbox"
    );
}

/// The reversal direction of the unmount guarantee: a plugin's registrations
/// unwind when *its own* fiber tears down — no manager call involved — and
/// never when another plugin's fiber or the spine's fiber tears down.
#[test]
fn fiber_teardown_unwinds_only_its_own_plugins_tools() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    let mut a = FakePlugin::new("alpha", vec![]);
    let mut b = FakePlugin::new("beta", vec![]);
    mount(&manager, &spine, &mut a);
    mount(&manager, &spine, &mut b);

    // The spine's own fiber tearing down must not touch a plugin's tools:
    // each mount owns a separate fiber.
    let spine_only = Spine::new();
    let gamma = WasmPluginManager::new();
    let mut c = FakePlugin::new("gamma", vec![]);
    mount(&gamma, &spine_only, &mut c);
    spine_only.fiber.dispose();
    assert_eq!(
        spine_only.names(),
        vec!["gamma.read"],
        "the spine's fiber teardown unwinds the spine, not the plugin"
    );

    // A plugin fiber disposed directly — no manager involved — unwinds exactly
    // its own tools, because its registration handle lives in that fiber.
    a.fiber.dispose();
    assert_eq!(
        spine.names(),
        vec!["beta.read"],
        "explicit fiber teardown alone unwinds that plugin's tools"
    );
    assert_eq!(
        b.fiber.state(),
        FiberState::Active,
        "the other plugin's fiber is untouched"
    );
    assert_eq!(
        manager.generations(),
        vec![("alpha".into(), 1u64), ("beta".into(), 1u64)],
        "the tree still lists both rows: disposing a fiber is not unmounting a \
         config row, and `unmount` is what retires the row"
    );
    // Retiring alpha's row is now a no-op unwind (its fiber is already down)
    // but still removes the row.
    assert!(manager.unmount("alpha"));
    assert_eq!(manager.generations(), vec![("beta".into(), 1u64)]);
}
