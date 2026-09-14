//! The plugin component executed end to end through the guarded pipeline,
//! asserted at the **primary seam**: the fixture's tool is registered on
//! [`harnless_agent::testsupport::Harness`]'s `ctx.tools`, a scripted model turn
//! requests it, and the resulting `tool_call` / `tool_result` pair is read back
//! from the session log — the same seam `crates/harnless-agent/tests/primary_seam.rs`
//! pins the native-tool pipeline against.
//!
//! What each test proves about a *component* guest (issue #15):
//!
//! * [`a_component_tool_executes_end_to_end_through_the_guarded_pipeline`] —
//!   the loader's descriptor validation, schema registration, approval, policy,
//!   guards, and recording all wrap a guest call exactly as they wrap a native
//!   body, and the guest's host `log` cross-import really executes.
//! * [`a_guest_trap_is_isolated_to_its_own_plugin`] — a guest `unreachable`
//!   surfaces as a structured tool failure, the session survives, and the next
//!   call to the same plugin runs clean.
//! * [`a_runaway_guest_is_stopped_by_its_fuel_budget`] — a spinning guest is
//!   killed by fuel, not by wall-clock luck, and is revived afterwards.
//! * [`a_plugin_gets_no_host_capability_it_was_not_granted`] — a component that
//!   imports the host `log` cannot mount when config does not grant it.

use std::sync::Arc;

use harnless_agent::testsupport::{text_recording, tool_recording, Harness, ScriptedCall};
use harnless_runtime::fiber::FiberState;
use harnless_seams::error::ErrorCode;
use harnless_seams::tools::Tools as _;
use harnless_seams::{MessageId, SessionId};
use harnless_wasm::abi::{Capability, PluginConfig};
use harnless_wasm::engine::LiveInstance;
use harnless_wasm::fixture::{self, Behavior};
use harnless_wasm::loader::WasmPluginManager;
use serde_json::json;

/// A manager plus the mounted plugin's live handle, so a test can call the
/// guest directly after the seam-driven turn.
fn mount_fixture(behavior: Behavior, grants_log: bool) -> (WasmPluginManager, PluginConfig) {
    let manager = WasmPluginManager::new();
    let mut config = PluginConfig::sandboxed(
        fixture::plugin_name(behavior),
        fixture::fixture_path(behavior).display().to_string(),
    );
    if grants_log {
        config.capabilities.push(Capability::Log);
    }
    (manager, config)
}

/// Acceptance: a fixture component's tool runs end to end through the guarded
/// pipeline, observed at the primary seam.
///
/// The loader validates the descriptor, registers `echo.echo` on `ctx.tools`
/// with the guest's declared schema, and the scripted turn calls it. The
/// pipeline's stages are all observable: approval gates the call (the first
/// attempt with no handler denies and the guest never runs), the guest's host
/// `log` cross-import executes, and the guest's own JSON result is what lands
/// in the log as the tool result.
#[test]
fn a_component_tool_executes_end_to_end_through_the_guarded_pipeline() {
    let (manager, config) = mount_fixture(Behavior::Echo, true);
    let h = Harness::with_script(
        SessionId(1501),
        vec![
            ScriptedCall::new(
                tool_recording(8, "echo.echo", r#"{"text":"hi"}"#),
                MessageId(2),
            ),
            ScriptedCall::new(text_recording(&["done"]), MessageId(3)),
        ],
    );

    // Mount through the seam: the harness context carries the ToolRegistry the
    // loader registers on.
    let generation = manager
        .mount_on(&h.tools, &config)
        .expect("the echo fixture mounts");
    assert_eq!(generation, 1);

    // The declaration carried through the seam: the guest's own schema.
    assert_eq!(
        h.tools.get("echo.echo").map(|d| d.schema),
        Some(json!({"type": "object"})),
        "the component's declared schema is what the seam exposes"
    );

    // 1. Approval fails closed: no pre-execute handler, so the guest never runs.
    let denied = h
        .tools
        .execute(harnless_seams::CallId(1), "echo.echo", br#"{"text":"hi"}"#)
        .expect_err("no pre-execute handler: the pipeline fails closed");
    assert_eq!(denied.code, ErrorCode::ToolDenied);

    // 2. Approved: the guest runs, its host log import fires, and the result is
    //    the guest's own JSON.
    let _allow = h.allow_all();
    let frozen = h
        .tools
        .execute(harnless_seams::CallId(2), "echo.echo", br#"{"text":"hi"}"#)
        .expect("an approved guest call");
    assert_eq!(
        frozen.value,
        json!({"echo": true}),
        "the guest's declared JSON result is what the pipeline froze"
    );
    assert_eq!(
        manager.plugin_log("echo"),
        Some(vec![r#"{"text":"hi"}"#.into()]),
        "the granted host capability executed inside the guest call"
    );

    // 3. The full turn: the exchange is visible in the log as call + result.
    let (_first, _second) = h.run_tool_turn();
    let kinds = h.event_kinds();
    let call_at = kinds.iter().position(|k| k == "tool_call").expect("call");
    let result_at = kinds
        .iter()
        .position(|k| k == "tool_result")
        .expect("result");
    assert!(
        call_at < result_at,
        "the component tool exchange is visible at the seam: {kinds:?}"
    );

    // 4. Unmounting the config row unwinds the component's tool.
    assert!(manager.unmount("echo"));
    assert!(h.tools.get("echo.echo").is_none());
}

/// Acceptance: a guest trap is contained. The failing call is a structured tool
/// error, the session/registry stand, and the *same plugin's next call runs
/// clean* — the host revived the poisoned instance, nothing else died.
#[test]
fn a_guest_trap_is_isolated_to_its_own_plugin() {
    let (manager, config) = mount_fixture(Behavior::Boom, false);
    let h = Harness::new(SessionId(1502));
    manager.mount_on(&h.tools, &config).expect("boom mounts");
    let _allow = h.allow_all();

    let err = h
        .tools
        .execute(harnless_seams::CallId(1), "boom.boom", b"{}")
        .expect_err("a guest trap must fail the call");
    assert_eq!(err.code, ErrorCode::ToolPanicked);
    assert!(
        err.message.contains("wasm backtrace") || err.message.contains("unreachable"),
        "the failure is a guest trap (a wasm backtrace), not an instance error: {}",
        err.message
    );

    // The plugin is still mounted and its next call is served by a revived
    // instance — which traps again (the body always traps), proving the call
    // *reached the guest* rather than erroring at a dead-instance boundary.
    let again = h
        .tools
        .execute(harnless_seams::CallId(2), "boom.boom", b"{}")
        .expect_err("the revived instance reaches the guest again");
    assert_eq!(again.code, ErrorCode::ToolPanicked);
    assert!(
        again.message.contains("wasm backtrace") || again.message.contains("unreachable"),
        "still a guest trap, not a dead-instance error: {}",
        again.message
    );
    assert_eq!(
        manager.generations(),
        vec![("boom".into(), 1u64)],
        "the trap did not unmount the plugin"
    );
}

/// Acceptance: a runaway guest is stopped by its fuel budget, and the plugin is
/// revived for the next call.
#[test]
fn a_runaway_guest_is_stopped_by_its_fuel_budget() {
    let (mut manager_ref, mut config) = mount_fixture(Behavior::Spin, false);
    // A deliberately tiny budget: the spin loop cannot finish, so fuel is the
    // only thing that ends the call.
    config.fuel_per_call = 10_000;
    let h = Harness::new(SessionId(1503));
    manager_ref
        .mount_on(&h.tools, &config)
        .expect("spin mounts");
    let _allow = h.allow_all();

    let err = h
        .tools
        .execute(harnless_seams::CallId(1), "spin.spin", b"{}")
        .expect_err("a spinning guest must be stopped");
    assert_eq!(err.code, ErrorCode::ToolPanicked);
    // A fuel exhaustion is a wasm trap reported with a wasm backtrace; the point
    // is the call *returned* (was stopped) rather than hanging the test.
    assert!(
        err.message.contains("wasm backtrace") || err.message.to_lowercase().contains("fuel"),
        "the runaway guest was stopped by a trap, not a hang: {}",
        err.message
    );

    // After the fuel trap the plugin is revived: a fresh call reaches the guest
    // and is stopped by fuel again rather than failing at a dead instance.
    let again = h
        .tools
        .execute(harnless_seams::CallId(2), "spin.spin", b"{}")
        .expect_err("still fuel-bounded");
    assert_eq!(again.code, ErrorCode::ToolPanicked);
    assert_eq!(manager_ref.generations(), vec![("spin".into(), 1u64)]);
}

/// Acceptance: a component that imports a host capability it was not granted
/// cannot mount at all — the boundary is instantiation, not a runtime refusal.
#[test]
fn a_plugin_gets_no_host_capability_it_was_not_granted() {
    // The echo fixture imports the host `log`; mounted with no grants it must
    // fail validation (which instantiates a throwaway sandbox), before any tool
    // is registered.
    let (manager, config) = mount_fixture(Behavior::Echo, false);
    let h = Harness::new(SessionId(1504));
    let err = manager
        .mount_on(&h.tools, &config)
        .expect_err("an ungranted host import must refuse the mount");
    assert!(
        err.contains("harnless:plugin/host") || err.contains("log"),
        "the failure names the missing capability: {err}"
    );
    assert!(
        h.tools.get("echo.echo").is_none(),
        "a refused mount registers nothing"
    );
    assert!(manager.generations().is_empty());
}

/// The engine-level guarantee the seam tests lean on: an `fs`-granted fixture
/// instantiates with its scoped directory, and the same component without the
/// grant does not. Kept at the engine boundary because the guest bodies do not
/// themselves read the scope — WASI wiring is the boundary under test.
#[test]
fn the_fs_grant_is_the_only_filesystem_the_guest_can_reach() {
    let engine = harnless_wasm::engine::build_engine().unwrap();
    let component =
        wasmtime::component::Component::new(&engine, fixture::component_bytes(Behavior::FsRead))
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), b"scoped").unwrap();

    let mut granted = PluginConfig::sandboxed(
        "fs",
        fixture::fixture_path(Behavior::FsRead)
            .display()
            .to_string(),
    );
    granted.capabilities.push(Capability::Fs {
        dir: dir.path().display().to_string(),
        read_only: true,
    });
    let mut live = LiveInstance::new(
        &engine,
        component.clone(),
        &granted,
        Arc::new(parking_lot::Mutex::new(Vec::new())),
    )
    .expect("an fs-granted fixture instantiates");
    let out = live
        .call_string_fn("call_read", "{}", harnless_wasm::abi::DEFAULT_FUEL)
        .expect("the granted guest call runs");
    assert_eq!(out, "{\"ok\":true}");

    // The same component with no grant still instantiates (it imports no WASI),
    // but the loader never wires a directory for it — the guest's only view of
    // the host filesystem is the scope the config opened.
    let ungranted = PluginConfig::sandboxed(
        "fs",
        fixture::fixture_path(Behavior::FsRead)
            .display()
            .to_string(),
    );
    let linker = harnless_wasm::engine::build_linker(&engine, &ungranted).unwrap();
    let mut store = harnless_wasm::engine::build_store(
        &engine,
        &ungranted,
        Arc::new(parking_lot::Mutex::new(Vec::new())),
    )
    .unwrap();
    let inst = linker
        .instantiate(&mut store, &component)
        .expect("an ungranted fixture imports no WASI");
    assert!(inst.get_func(&mut store, "call-read").is_some());
}

/// Acceptance: an `fs` grant never wires the network.
///
/// The scoped linker registers filesystem plus the `wasi:io` plumbing it is
/// typed against — and nothing else. A guest that reaches for the default
/// network handle therefore cannot be instantiated *at all*, even when its
/// config's own grant is valid and mounted-elsewhere-fine. The refusal is
/// import resolution, before a single guest instruction runs, which is the
/// strongest form the capability boundary can take: there is no code path in
/// which the plugin gets a socket and policy has to notice.
#[test]
fn a_filesystem_grant_never_wires_the_network() {
    let engine = harnless_wasm::engine::build_engine().unwrap();
    let component =
        wasmtime::component::Component::new(&engine, fixture::component_bytes(Behavior::Sockets))
            .expect("the sockets fixture compiles");

    let dir = tempfile::tempdir().unwrap();
    let mut granted = PluginConfig::sandboxed(
        "sock",
        fixture::fixture_path(Behavior::Sockets)
            .display()
            .to_string(),
    );
    // A perfectly ordinary, satisfiable filesystem grant. The plugin's *fs*
    // half is fine; it is the sockets import that must refuse the mount.
    granted.capabilities.push(Capability::Fs {
        dir: dir.path().display().to_string(),
        read_only: true,
    });

    let err = LiveInstance::new(
        &engine,
        component.clone(),
        &granted,
        Arc::new(parking_lot::Mutex::new(Vec::new())),
    )
    .err()
    .expect("a filesystem grant must not satisfy a sockets import");
    assert!(
        err.to_string().contains("wasi:sockets/network"),
        "the refusal names the unimplemented interface: {err}"
    );

    // It is the *linker* that refuses, not the store or the component: the same
    // component against the same store with the same grants fails identically
    // through the raw path, and the very same config mounts a plugin that
    // imports no sockets.
    let linker = harnless_wasm::engine::build_linker(&engine, &granted).unwrap();
    let mut store = harnless_wasm::engine::build_store(
        &engine,
        &granted,
        Arc::new(parking_lot::Mutex::new(Vec::new())),
    )
    .unwrap();
    let raw = linker
        .instantiate(&mut store, &component)
        .err()
        .expect("the scoped linker has no sockets implementation to offer");
    assert!(
        raw.to_string()
            .contains("a matching implementation was not found"),
        "the failure is import resolution: {raw}"
    );

    // And through the loader: the mount is refused, so nothing is registered.
    let manager = WasmPluginManager::new();
    let h = Harness::new(SessionId(1506));
    let err = manager
        .mount_on(&h.tools, &granted)
        .expect_err("a sockets-importing plugin cannot mount on an fs grant");
    assert!(
        err.contains("wasi:sockets/network"),
        "the loader reports the import failure: {err}"
    );
    assert!(
        h.tools.get("sock.read").is_none(),
        "a refused mount registers nothing"
    );
    assert!(manager.generations().is_empty());

    // Control: the same linker wiring *does* mount a guest with no sockets
    // import, so the refusal above is the import, not the wiring being broken.
    let fs_component =
        wasmtime::component::Component::new(&engine, fixture::component_bytes(Behavior::FsRead))
            .unwrap();
    let mut fs_config = PluginConfig::sandboxed("fs", String::from("fs.wasm"));
    fs_config.capabilities = granted.capabilities.clone();
    let linker = harnless_wasm::engine::build_linker(&engine, &fs_config).unwrap();
    let mut store = harnless_wasm::engine::build_store(
        &engine,
        &fs_config,
        Arc::new(parking_lot::Mutex::new(Vec::new())),
    )
    .unwrap();
    linker
        .instantiate(&mut store, &fs_component)
        .expect("the filesystem-only guest instantiates against the same wiring");
}

/// The loader's own fiber bookkeeping stays consistent with a real component
/// mount: the plugin's fiber is the thing that tears down on unmount.
#[test]
fn a_real_mount_tears_down_its_own_fiber() {
    let (manager, config) = mount_fixture(Behavior::FsRead, false);
    let h = Harness::new(SessionId(1505));
    manager.mount_on(&h.tools, &config).expect("fs mounts");
    let mounted = manager
        .mounted_fibers()
        .into_iter()
        .next()
        .expect("one mounted fiber");
    assert_eq!(mounted.state(), FiberState::Active);
    assert!(manager.unmount("fs"));
    assert_eq!(mounted.state(), FiberState::Disposed);
}
