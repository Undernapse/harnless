//! The reproducible fixture corpus: real plugin components built from WAT.
//!
//! Every fixture is a component-model **component** produced by
//! [`wit_component`] from two inputs this module derives:
//!
//! 1. a **WIT world** ([`world_wit`]) naming the plugin's exports and, for the
//!    log-importing fixtures, the host capability interface; and
//! 2. a **core module** ([`core_wat`]) written as WAT and assembled by [`wat`],
//!    whose exported functions are the canonical-ABI bodies the component's
//!    lifted exports call.
//!
//! [`encode_component`] embeds the component metadata for the world into the
//! core module and runs `ComponentEncoder`, which generates the canonical
//! lift/lower wrappers — the same step `cargo-component` performs for a real
//! plugin, and the reason these fixtures exercise the loader's component path
//! rather than a hand-assembled approximation of it. No external toolchain is
//! involved (`wat` and `wit-component` are Rust crates), so `cargo test`
//! regenerates the corpus deterministically on any machine. The corpus is
//! checked into `fixtures/`; [`regenerate_fixtures`] rewrites it, and the
//! `fixture_corpus_is_byte_reproducible` test asserts the checked-in bytes
//! equal the derived bytes.
//!
//! # The core ABI the bodies are written against
//!
//! A canonical-lifted `(param string) -> string` export calls the core body as
//! `(param i32 i32) -> i32`: the input string arrives as `(ptr, len)` i32
//! locals, and the result is an i32 pointer to a `{ptr, len}` struct the body
//! writes. A `(func() -> string)` export takes no arguments and returns the
//! same `{ptr, len}` pointer. Every body here writes its result struct at
//! [`mem::RESULT_STRUCT`] and returns that address; string parameters are
//! forwarded untouched, so no fixture needs an allocator and
//! `cabi_realloc` is a fixed-address stub.
//!
//! The fixture plugins:
//!
//! * `echo_plugin.wasm` — descriptor declares one tool `echo`; `call_echo`
//!   calls the host `log` with its input and returns `{"echo":<input>}`,
//!   proving the capability cross-import executes end to end.
//! * `boom_plugin.wasm` — `call_boom` executes `unreachable` (a guest trap).
//! * `spin_plugin.wasm` — `call_spin` is a `br 0` loop (fuel-bounded).
//! * `fs_plugin.wasm` — descriptor declares one tool `read`; `call_read`
//!   returns `{"ok":true}`; mounted with an `fs` grant so the scoped WASI
//!   directory is the only filesystem the guest can reach.
//! * `sockets_plugin.wasm` — descriptor declares one tool `read`; its world
//!   imports `wasi:sockets/network` + `instance-network` and `call_read` takes
//!   the default network handle. No grant ever wires sockets, so this fixture
//!   cannot mount: the import-resolution failure *is* the capability boundary.
//!
//! # Naming
//!
//! WIT identifiers are kebab-case, so the component exports are `call-echo`
//! and friends; the loader's `call_<tool>` export lookup accepts either
//! spelling (see [`crate::engine::LiveInstance::call_string_fn`]). The host
//! interface's component-level import name is `harnless:plugin/host` — the
//! world carries no `@version`, and that is the exact string
//! [`crate::engine::build_linker`] wires the `log` implementation under.

use std::path::{Path, PathBuf};

use wit_component::{embed_component_metadata, ComponentEncoder, StringEncoding};
use wit_parser::{Resolve, UnresolvedPackageGroup};

/// Memory layout shared by every fixture core module.
pub mod mem {
    /// The `{ptr, len}` result struct every string-returning function writes.
    pub const RESULT_STRUCT: u32 = 0;
    /// Where fixture string literals live (above the result struct).
    pub const STRING_AREA: u32 = 16;
    /// Where a fixture's scratch area for building an echoed result begins.
    pub const SCRATCH: u32 = 512;
}

/// The WIT-equivalent identity of the host capability interface, as the
/// component's import name reads.
pub const HOST_INSTANCE: &str = "harnless:plugin/host";

/// The behaviour one fixture implements.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Behavior {
    /// `call`: log(input) then return `{"echo":<input>}`.
    Echo,
    /// `call`: trap with `unreachable`.
    Boom,
    /// `call`: loop forever (fuel-bounded).
    Spin,
    /// `call`: return `{"ok":true}`; mounted with an `fs` grant.
    FsRead,
    /// `call`: take the default network handle. Its component imports
    /// `wasi:sockets/*`, which the shipped scoped linker never registers, so
    /// the mount fails at import resolution.
    Sockets,
}

/// Every behaviour in the corpus.
pub const ALL: [Behavior; 5] = [
    Behavior::Echo,
    Behavior::Boom,
    Behavior::Spin,
    Behavior::FsRead,
    Behavior::Sockets,
];

impl Behavior {
    /// Whether this fixture's component imports the host `log`.
    ///
    /// Only the echo fixture does, which is what makes its mount the
    /// end-to-end proof that a granted capability is wired and an ungranted
    /// one refuses instantiation.
    pub fn imports_host_log(&self) -> bool {
        matches!(self, Behavior::Echo)
    }

    /// Whether this fixture's component imports `wasi:sockets/*`.
    ///
    /// The shipped linker registers sockets for no grant whatsoever, so this
    /// fixture can never mount: the point is that the refusal is
    /// import-resolution failure at instantiation, not a runtime policy.
    pub fn imports_sockets(&self) -> bool {
        matches!(self, Behavior::Sockets)
    }
}

/// The fixture file name for a behaviour.
pub fn fixture_name(behavior: Behavior) -> &'static str {
    match behavior {
        Behavior::Echo => "echo_plugin.wasm",
        Behavior::Boom => "boom_plugin.wasm",
        Behavior::Spin => "spin_plugin.wasm",
        Behavior::FsRead => "fs_plugin.wasm",
        Behavior::Sockets => "sockets_plugin.wasm",
    }
}

/// The tool name each fixture's descriptor declares.
pub fn tool_name(behavior: Behavior) -> &'static str {
    match behavior {
        Behavior::Echo => "echo",
        Behavior::Boom => "boom",
        Behavior::Spin => "spin",
        Behavior::FsRead => "read",
        Behavior::Sockets => "read",
    }
}

/// The plugin name each fixture's descriptor declares.
pub fn plugin_name(behavior: Behavior) -> &'static str {
    match behavior {
        Behavior::Echo => "echo",
        Behavior::Boom => "boom",
        Behavior::Spin => "spin",
        Behavior::FsRead => "fs",
        Behavior::Sockets => "sock",
    }
}

/// The descriptor literal each fixture's `descriptor()` returns.
pub fn descriptor_json(behavior: Behavior) -> String {
    let tool = tool_name(behavior);
    let plugin = plugin_name(behavior);
    format!(
        "{{\"name\":\"{plugin}\",\"tools\":[{{\"name\":\"{tool}\",\"schema\":{{\"type\":\"object\"}},\"output\":\"json\",\"serialized\":false}}]}}"
    )
}

/// The WIT world a fixture is encoded against.
///
/// The world declares every export the loader calls plus the canonical
/// allocator (which `ComponentEncoder` expects the core module to export), and
/// for [`Behavior::Echo`] the host interface import.
pub fn world_wit(behavior: Behavior) -> String {
    let tool = tool_name(behavior);
    let import = if behavior.imports_host_log() {
        "  import host;\n"
    } else {
        ""
    };
    let host = if behavior.imports_host_log() {
        "interface host { log: func(s: string); }\n"
    } else {
        ""
    };
    // The sockets fixture's world additionally imports the two WASI interfaces
    // a network-touching guest needs. They are declared in [`SOCKETS_DEP_WIT`],
    // pushed into the `Resolve` before this package.
    let wasi = if behavior.imports_sockets() {
        "  import wasi:sockets/network@0.2.12;\n  import wasi:sockets/instance-network@0.2.12;\n"
    } else {
        ""
    };
    format!(
        r#"package harnless:plugin;
{host}world plugin {{
{import}{wasi}  export descriptor: func() -> string;
  export call-{tool}: func(input: string) -> string;
}}
"#
    )
}

/// The WAT core module a fixture is encoded from.
///
/// See the module docs for the canonical-ABI shape each body obeys.
pub fn core_wat(behavior: Behavior) -> String {
    let tool = tool_name(behavior);
    let descriptor = descriptor_json(behavior);
    let log_import = if behavior.imports_host_log() {
        r#"(import "harnless:plugin/host" "log" (func $log (param i32 i32)))"#
    } else {
        ""
    };
    // The sockets fixture reaches the network through the value-export of the
    // default network handle, exactly as a `wasi:sockets` guest does.
    let net_import = if behavior.imports_sockets() {
        r#"(import "wasi:sockets/instance-network@0.2.12" "instance-network" (func $net (result i32)))"#
    } else {
        ""
    };
    // `descriptor` writes its literal into the data segment and returns a
    // pointer to its {ptr, len} struct, which the data segment also holds.
    let desc_len = descriptor.len();
    let call_body = match behavior {
        // Echo returns a fixed valid JSON object and forwards its raw-JSON
        // arguments to the host `log` — the cross-import is what proves the
        // granted capability executed on the *caller's* payload. The guest does
        // not re-serialize its input (that would need JSON escaping and an
        // allocator; neither is the point of the corpus).
        Behavior::Echo => format!(
            r#"(func (export "call-{tool}") (param $in_ptr i32) (param $in_len i32) (result i32)
    ;; forward the caller's raw JSON to the host log (the granted capability)
    (call $log (local.get $in_ptr) (local.get $in_len))
    ;; result: the fixed literal `{{"echo":true}}` from the data segment
    (i32.store offset={result} (i32.const 0) (i32.const {echo_ptr}))
    (i32.store offset={result_len} (i32.const 0) (i32.const {echo_len}))
    (i32.const {result}))"#,
            result = mem::RESULT_STRUCT,
            result_len = mem::RESULT_STRUCT + 4,
            echo_ptr = mem::STRING_AREA + desc_len as u32 + 8,
            echo_len = ECHO_JSON.len(),
        ),
        Behavior::Boom => format!(
            r#"(func (export "call-{tool}") (param i32) (param i32) (result i32)
    (unreachable))"#
        ),
        Behavior::Spin => format!(
            r#"(func (export "call-{tool}") (param i32) (param i32) (result i32)
    ;; A counter loop: it never returns, so the call's fuel budget is what
    ;; stops it. The `i32.const` after the loop only satisfies the validator's
    ;; declared result type — control never reaches it.
    (local $i i32)
    (loop $again
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $again))
    (i32.const 0))"#
        ),
        Behavior::FsRead => format!(
            r#"(func (export "call-{tool}") (param i32) (param i32) (result i32)
    (i32.store offset={result} (i32.const 0) (i32.const {ok_ptr}))
    (i32.store offset={result_len} (i32.const 0) (i32.const {ok_len}))
    (i32.const {result}))"#,
            result = mem::RESULT_STRUCT,
            result_len = mem::RESULT_STRUCT + 4,
            ok_ptr = mem::STRING_AREA + desc_len as u32 + 8,
            ok_len = OK_JSON.len(),
        ),
        Behavior::Sockets => format!(
            r#"(func (export "call-{tool}") (param i32) (param i32) (result i32)
    ;; Taking the handle is the whole point: it is the call the host must
    ;; never let a filesystem-granted plugin make.
    (drop (call $net))
    (i32.store offset={result} (i32.const 0) (i32.const {ok_ptr}))
    (i32.store offset={result_len} (i32.const 0) (i32.const {ok_len}))
    (i32.const {result}))"#,
            result = mem::RESULT_STRUCT,
            result_len = mem::RESULT_STRUCT + 4,
            ok_ptr = mem::STRING_AREA + desc_len as u32 + 8,
            ok_len = OK_JSON.len(),
        ),
    };
    // The tool-result literal sits just past the descriptor literal in the data
    // segment; each fixture's `call_<tool>` body points its result struct at it.
    let literal = match behavior {
        Behavior::Echo => ECHO_JSON,
        Behavior::FsRead | Behavior::Sockets => OK_JSON,
        _ => "",
    };
    let literal_ptr = mem::STRING_AREA + desc_len as u32 + 8;
    let data = if literal.is_empty() {
        format!(
            r#"(data (i32.const {string_area}) "{descriptor}")"#,
            string_area = mem::STRING_AREA,
            descriptor = wat_escape(&descriptor),
        )
    } else {
        format!(
            r#"(data (i32.const {string_area}) "{descriptor}")
  (data (i32.const {literal_ptr}) "{literal}")"#,
            string_area = mem::STRING_AREA,
            descriptor = wat_escape(&descriptor),
            literal_ptr = literal_ptr,
            literal = wat_escape(literal),
        )
    };
    format!(
        r#"(module
  {log_import}
  {net_import}
  (memory (export "memory") 1)
  (global (export "__data_end") i32 (i32.const 4096))
  (global (export "__heap_base") i32 (i32.const 4096))
  {data}
  (func (export "cabi_realloc") (param i32) (param i32) (param i32) (param i32) (result i32)
    ;; No fixture allocates: the canonical allocator is never called, and a
    ;; call that did would be handed the fixed scratch tail rather than trap.
    (i32.const 4096))
  (func (export "descriptor") (result i32)
    ;; result struct: ptr = the literal's data-segment offset, len = its length
    (i32.store offset={result} (i32.const 0) (i32.const {string_area}))
    (i32.store offset={result_len} (i32.const 0) (i32.const {desc_len}))
    (i32.const {result}))
  {call_body}
)
"#,
        log_import = log_import,
        net_import = net_import,
        data = data,
        result = mem::RESULT_STRUCT,
        result_len = mem::RESULT_STRUCT + 4,
        string_area = mem::STRING_AREA,
        desc_len = desc_len,
    )
}

/// The `fs` fixture's tool result.
const OK_JSON: &str = "{\"ok\":true}";

/// The `echo` fixture's tool result.
const ECHO_JSON: &str = "{\"echo\":true}";

fn wat_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The minimal `wasi:sockets` package the sockets fixture's world imports.
///
/// Only the two interfaces that world names are declared, and only as much of
/// them as the fixture touches: the `network` resource and the
/// `instance-network` value-export. `wit-component` matches the *importing
/// component's* declared interface against the host's, so a stub is enough to
/// produce a component whose import name is the real
/// `wasi:sockets/network@0.2.12` — which is exactly the name the shipped
/// scoped linker does not implement.
pub const SOCKETS_DEP_WIT: &str = r#"package wasi:sockets@0.2.12;
interface network {
  resource network;
}
interface instance-network {
  use network.{network};
  instance-network: func() -> network;
}
"#;

/// Encode a fixture's core module into the component the loader mounts.
///
/// This is the `cargo-component` build step: embed the world's component
/// metadata into the core module, then let `ComponentEncoder` generate the
/// canonical wrappers and the component itself.
pub fn encode_component(behavior: Behavior) -> Vec<u8> {
    let wit = world_wit(behavior);
    let core = core_wat(behavior);
    let deps: &[(&str, &str)] = if behavior.imports_sockets() {
        &[("sockets.wit", SOCKETS_DEP_WIT)]
    } else {
        &[]
    };
    encode(deps, &wit, &core).unwrap_or_else(|e| panic!("{behavior:?}: fixture encode failed: {e}"))
}

/// Encode one plugin package against its dependency packages.
///
/// Every dependency is pushed into the `Resolve` *before* the plugin package:
/// `push_group` requires a package's dependencies to already be present, so
/// this is how a fixture's world gets to say `import wasi:sockets/...`.
fn encode(deps: &[(&str, &str)], wit: &str, core_wat: &str) -> Result<Vec<u8>, String> {
    let core = wat::parse_str(core_wat).map_err(|e| format!("core WAT: {e}"))?;
    let mut resolve = Resolve::default();
    for (path, dep) in deps {
        let group =
            UnresolvedPackageGroup::parse(path, dep).map_err(|e| format!("dep WIT {path}: {e}"))?;
        resolve
            .push_group(group)
            .map_err(|e| format!("dep WIT {path}: {e}"))?;
    }
    let group =
        UnresolvedPackageGroup::parse("plugin.wit", wit).map_err(|e| format!("world WIT: {e}"))?;
    let pkg = resolve
        .push_group(group)
        .map_err(|e| format!("world WIT: {e}"))?;
    let world = resolve
        .select_world(pkg, None)
        .map_err(|e| format!("world WIT: {e}"))?;
    let mut module = core;
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .map_err(|e| format!("embed metadata: {e}"))?;
    ComponentEncoder::default()
        .module(&module)
        .map_err(|e| format!("encoder: {e}"))?
        .validate(true)
        .encode()
        .map_err(|e| format!("encode: {e}"))
}

/// The checked-in component bytes for a behaviour.
pub fn component_bytes(behavior: Behavior) -> Vec<u8> {
    encode_component(behavior)
}

/// The directory the corpus is checked into.
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// The checked-in path for one fixture.
pub fn fixture_path(behavior: Behavior) -> PathBuf {
    fixture_dir().join(fixture_name(behavior))
}

/// Rewrite the checked-in corpus from the derived bytes; returns the paths.
pub fn regenerate_fixtures() -> Vec<PathBuf> {
    ALL.iter()
        .map(|b| {
            let path = fixture_path(*b);
            std::fs::write(&path, component_bytes(*b)).expect("write fixture");
            path
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{Capability, PluginConfig, DEFAULT_FUEL};
    use crate::engine::LiveInstance;
    use std::sync::Arc;

    /// The corpus's own contract: every fixture compiles, instantiates under the
    /// config its behaviour needs, and its `descriptor()` reads back exactly the
    /// descriptor the loader will validate.
    ///
    /// The sockets fixture is the deliberate exception to the instantiate half:
    /// it exists *because* no grant wires `wasi:sockets`, so it is only checked
    /// to compile here (its refusal is pinned by
    /// `tests/guest_behavior.rs::a_filesystem_grant_never_wires_the_network`).
    #[test]
    fn every_fixture_encodes_and_its_descriptor_reads_back() {
        let engine = crate::engine::build_engine().unwrap();
        for behavior in ALL {
            let bytes = component_bytes(behavior);
            let component = wasmtime::component::Component::new(&engine, &bytes)
                .unwrap_or_else(|e| panic!("{behavior:?} did not compile: {e}"));
            if behavior.imports_sockets() {
                continue;
            }
            let mut config = PluginConfig::sandboxed(
                plugin_name(behavior),
                fixture_path(behavior).display().to_string(),
            );
            if behavior.imports_host_log() {
                config.capabilities.push(Capability::Log);
            }
            let mut live = LiveInstance::new(
                &engine,
                component,
                &config,
                Arc::new(parking_lot::Mutex::new(Vec::new())),
            )
            .unwrap_or_else(|e| panic!("{behavior:?} instantiate: {e}"));
            let raw = live
                .call_string_fn("descriptor", "", DEFAULT_FUEL)
                .unwrap_or_else(|e| panic!("{behavior:?} descriptor: {e:?}"));
            assert_eq!(raw, descriptor_json(behavior));
        }
    }

    /// The descriptor literal's length is what the core module hard-codes as its
    /// result length, so a mismatch would silently truncate or overrun it.
    #[test]
    fn descriptor_length_matches_the_data_segment() {
        for behavior in ALL {
            let wat = core_wat(behavior);
            let len = descriptor_json(behavior).len();
            assert!(
                wat.contains(&format!("(i32.const {len}))")),
                "{behavior:?}: core module does not carry descriptor length {len}"
            );
        }
    }
}
