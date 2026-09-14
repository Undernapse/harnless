//! The plugin manager's mount / unmount / reload guarantees, asserted at the
//! seam the shipped loader owns its behaviour through: tools register on
//! [`harnless_seams::Tools`] (`ctx.tools`), calls run through the guarded
//! [`ToolRegistry`] pipeline, and reversibility is observable as the
//! registry's tool set.
//!
//! # Why these tests use a stand-in guest body
//!
//! The corpus (`src/fixture.rs`) is now real, loadable components, and
//! `tests/guest_behavior.rs` drives the *component* end to end at the primary
//! seam (host cross-import, guest-trap isolation, the fuel kill of a runaway
//! guest, capability denial). Those are the guarantees only a loaded component
//! can prove.
//!
//! What this file owns is narrower and does not need a component: the loader's
//! *mount / unmount / reload / generation bookkeeping*. [`mount`] performs the
//! same sequence the shipped loader does — its own fiber, tools registered
//! through the seam, a fiber-owned reversible handle with the loader's `Drop`
//! contract, a generation counter — with a [`ScopedGuest`] body in place of the
//! component, and pairs each assertion with the shipped [`WasmPluginManager`]
//! bookkeeping (`generations`, `unmount`, `unmount_generation`) so the two views
//! cannot drift. Keeping the lifecycle matrix here (rather than mounting four
//! components per test) isolates reversibility failures from guest behaviour.

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
use harnless_wasm::fixture::{self, Behavior};
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
        // The shipped `mount`/`reload`/`reload_all` look the registry up as
        // `ctx.get::<ToolRegistry>()`, which needs the *shared handle* installed
        // under the `ToolRegistry` key — `provide_shared`, not `provide`.
        ctx.provide_shared(&fiber, registry.clone())
            .expect("provide registry");
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
                |_: &mut PreExecute, _next: &mut Next<'_, PreExecute, PreDecision>| {
                    PreDecision::Allow
                },
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

    fn build(id: &str, tool: &str, capabilities: Vec<Capability>, dir: Option<&Path>) -> Self {
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
                description: format!("the {tool} tool of plugin {id}"),
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

    let entries: Arc<Mutex<Vec<(String, Arc<dyn ToolBody>)>>> = Arc::new(Mutex::new(Vec::new()));
    for spec in &p.descriptor.tools {
        let name = format!("{}.{}", p.descriptor.name, spec.name);
        spine
            .registry
            .register(
                ToolDefinition {
                    name: name.clone(),
                    description: spec.description.clone(),
                    schema: spec.schema.clone(),
                    serialized: spec.serialized,
                },
                p.body.clone(),
            )
            .unwrap_or_else(|e| panic!("register {name}: {e:?}"));
        entries.lock().push((name, p.body.clone()));
    }
    let names = Arc::new(Mutex::new(
        entries
            .lock()
            .iter()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>(),
    ));

    // The shipped loader's reversibility: the handle's single strong owner is
    // an effect on the plugin's fiber, which `record_mount` installs. Fiber
    // teardown — driven by `unmount`, or by the test directly — drops the
    // handle, and its `Drop` removes exactly these registrations.
    let handle = Arc::new(Mutex::new(Registration {
        registry: spine.registry.clone(),
        entries,
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

/// The description path, asserted where the loader owns it: a descriptor
/// that declares model-facing text registers a definition carrying exactly
/// that text, and the loader never invents one for a plugin that declared
/// none.
#[test]
fn a_declared_description_reaches_the_registered_definition() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    let mut p = FakePlugin::new("fs", vec![]);
    mount(&manager, &spine, &mut p);
    assert_eq!(
        spine.registry.get("fs.read").map(|d| d.description),
        Some("the read tool of plugin fs".into()),
        "the description the guest declared is the description the seam holds"
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
    let target = format!(
        r#"{{"path":"{}"}}"#,
        dir.path().join("secret.txt").display()
    );
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

// ---------------------------------------------------------------------------
// The shipped reload paths, driven with real components.
//
// Everything above drives the loader's *bookkeeping* with a stand-in body.
// These two tests drive `WasmPluginManager::reload_on` and `reload_all`
// themselves — the shipped code paths, with the shipped `stage`/`commit`/
// `retire_older` sequence — against the real fixture components, so the
// reload guarantee is pinned on the functions config actually calls.

/// A real component mount through the shipped loader.
fn mount_real(
    manager: &WasmPluginManager,
    registry: &Arc<ToolRegistry>,
    id: &str,
    behavior: Behavior,
) -> u64 {
    let config = PluginConfig::sandboxed(id, fixture::fixture_path(behavior).display().to_string());
    manager
        .mount_on(registry, &config)
        .unwrap_or_else(|e| panic!("{id} mounts: {e}"))
}

/// Acceptance: the shipped `reload_on` leaves the *new* generation mounted.
///
/// A config edit is the only way a plugin changes in production, and `reload`
/// is what applies it. The guarantee reload must give config is: afterwards
/// the plugin is mounted, exactly once, with its tools live. A reload that
/// retired the new generation (the partition-by-position bug) or that left the
/// old row beside the new one both fail here.
#[test]
fn the_shipped_reload_leaves_the_new_generation_mounted() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    let g1 = mount_real(&manager, &spine.registry, "fs", Behavior::FsRead);
    assert_eq!(g1, 1);
    assert_eq!(
        spine.names(),
        vec!["fs.read"],
        "the first generation registered its declared tool"
    );
    let first_fiber = manager
        .mounted_fibers()
        .into_iter()
        .next()
        .expect("one mount");

    // Reload the same config row.
    let config = PluginConfig::sandboxed(
        "fs",
        fixture::fixture_path(Behavior::FsRead)
            .display()
            .to_string(),
    );
    let g2 = manager
        .reload_on(&spine.registry, &config)
        .expect("the shipped reload succeeds");
    assert_eq!(g2, 2, "the reload is the next generation");

    // The tree holds exactly one row for the id, and it is the new one.
    assert_eq!(
        manager.generations(),
        vec![("fs".into(), 2u64)],
        "exactly one row per id, and it is the reloaded generation"
    );
    assert!(
        manager.is_mounted("fs", 2),
        "the new generation stands mounted"
    );
    assert!(
        !manager.is_mounted("fs", 1),
        "the superseded generation is retired"
    );

    // Its tools are live on the seam, and they actually run.
    assert_eq!(
        spine.names(),
        vec!["fs.read"],
        "a reload swaps the tool set, it never accumulates or empties it"
    );
    let _allow = spine.allow_all();
    let frozen = spine
        .registry
        .execute(CallId(1), "fs.read", b"{}")
        .expect("the reloaded generation serves calls");
    assert_eq!(frozen.value, json!({"ok": true}));

    // The old generation's fiber — the thing that owns its registrations — is
    // the one that tore down; the live mount's stands.
    assert_eq!(
        first_fiber.state(),
        FiberState::Disposed,
        "reloading retired the old generation's fiber"
    );
    let live = manager
        .mounted_fibers()
        .into_iter()
        .next()
        .expect("still one mount");
    assert_ne!(
        Arc::as_ptr(&live),
        Arc::as_ptr(&first_fiber),
        "the live row is the new generation's fiber"
    );
    assert_eq!(live.state(), FiberState::Active);
}

/// Acceptance: a successful `reload_all` leaves *every* config mounted, with
/// its tools present.
///
/// The transactional half (`reload_all_is_transactional_...`) proves a bad
/// config changes nothing. This is the other half, and the one config actually
/// depends on: when the whole reload succeeds, each row of the new tree is
/// live — no plugin silently dropped, no generation left unretired.
#[test]
fn a_successful_reload_all_leaves_every_config_mounted() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    // Start from a different tree so the swap is observable in both
    // directions: `spin` is dropped from config, `echo` is new.
    mount_real(&manager, &spine.registry, "spin", Behavior::Spin);
    assert_eq!(spine.names(), vec!["spin.spin"]);

    let configs = vec![
        PluginConfig::sandboxed(
            "echo",
            fixture::fixture_path(Behavior::Echo).display().to_string(),
        ),
        PluginConfig::sandboxed(
            "fs",
            fixture::fixture_path(Behavior::FsRead)
                .display()
                .to_string(),
        ),
    ];
    // `echo` imports the host `log`, so its config must grant it — otherwise
    // this is the transactional-failure test, not this one.
    let mut configs = configs;
    configs[0]
        .capabilities
        .push(harnless_wasm::abi::Capability::Log);

    // `reload_all` is the context-driven entry point: it looks the registry up
    // as a context service, so the spine's context must still own it. `Spine`
    // provides it on the spine's fiber; this is that fiber's context.
    manager
        .reload_all(&spine.ctx, &configs)
        .expect("a good tree reloads");

    // Every config row is mounted, exactly once.
    let mut gens = manager.generations();
    gens.sort();
    assert_eq!(
        gens,
        vec![("echo".into(), 1u64), ("fs".into(), 1u64)],
        "each config of the new tree has exactly one live row"
    );

    // Every config's tools are present on the seam, and the dropped plugin's
    // are gone.
    assert_eq!(
        spine.names(),
        vec!["echo.echo", "fs.read"],
        "the reloaded tree's tools are all registered; the removed plugin's are unwound"
    );

    // They are not decoration: both mounted generations serve calls.
    let _allow = spine.allow_all();
    let fs = spine
        .registry
        .execute(CallId(1), "fs.read", b"{}")
        .expect("the fs row of the reloaded tree runs");
    assert_eq!(fs.value, json!({"ok": true}));
    let echo = spine
        .registry
        .execute(CallId(2), "echo.echo", br#"{"text":"hi"}"#)
        .expect("the echo row of the reloaded tree runs");
    assert_eq!(echo.value, json!({"echo": true}));
    assert_eq!(
        manager.plugin_log("echo"),
        Some(vec![r#"{"text":"hi"}"#.into()]),
        "the reloaded mount carries its own wired capability"
    );
}

/// Acceptance: a *failed* `reload_all` leaves the surviving plugin serving.
///
/// The transactional guarantee config depends on is not "the tool list looks
/// right" — it is "the plugin that was working still answers calls". A bad
/// component in the new tree must not be able to retire the tools of a
/// plugin whose own row is perfectly good.
#[test]
fn failed_reload_all_leaves_the_surviving_plugin_serving() {
    let manager = WasmPluginManager::new();
    let spine = Spine::new();
    mount_real(&manager, &spine.registry, "fs", Behavior::FsRead);
    assert_eq!(manager.generations(), vec![("fs".into(), 1u64)]);
    assert_eq!(spine.names(), vec!["fs.read"]);

    // The new tree: the same good `fs` row plus a component that does not
    // compile at all. `fs` itself is unchanged, so nothing about it moves.
    let fs_good = PluginConfig::sandboxed(
        "fs",
        fixture::fixture_path(Behavior::FsRead)
            .display()
            .to_string(),
    );
    let broken = PluginConfig::sandboxed("broken", "/nonexistent/definitely-missing.wasm");
    manager
        .reload_all(&spine.ctx, &[fs_good, broken])
        .expect_err("an unreadable component fails the reload");

    assert_eq!(
        manager.generations(),
        vec![("fs".into(), 1u64)],
        "a failed reload_all leaves the last-good tree mounted"
    );
    assert_eq!(
        spine.names(),
        vec!["fs.read"],
        "the surviving plugin's tools are still registered"
    );

    // And they are not decoration: the surviving plugin still serves.
    let _allow = spine.allow_all();
    let frozen = spine
        .registry
        .execute(CallId(7), "fs.read", b"{}")
        .unwrap_or_else(|e| panic!("the surviving plugin must still serve calls: {e:?}"));
    assert_eq!(frozen.value, json!({"ok": true}));
}
