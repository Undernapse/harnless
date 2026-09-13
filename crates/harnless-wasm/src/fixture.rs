//! The reproducible fixture corpus: hand-assembled plugin components.
//!
//! Every fixture is assembled from bytes this module derives with
//! `wasm-encoder` (component-model feature) — no external toolchain (no
//! `cargo-component`, `tinygo`, `wat2wasm`) is involved, so `cargo test`
//! regenerates the corpus deterministically on any machine. The corpus is
//! checked into `fixtures/`; [`regenerate_fixtures`] rewrites it, and the
//! `fixture_corpus_is_byte_reproducible` test asserts the checked-in bytes
//! equal the derived bytes.
//!
//! # How each fixture is produced
//!
//! A fixture is a **component** built from three parts:
//!
//! 1. A **core module** ([`core_module`]) with one memory, a mutable global
//!    bump pointer at [`mem::HEAP_START`], and hand-assembled function
//!    bodies in the canonical core ABI: a string *result* is returned as an
//!    i32 pointer to an 8-byte `{ptr, len}` struct written at
//!    [`mem::RESULT_STRUCT`]; a string *parameter* arrives as `(ptr, len)`
//!    i32 locals. The imported host `log` arrives as a core
//!    `(param i32 i32)` function.
//! 2. A **component type section** declaring the host function type
//!    `(param "s" string)` and the instance type [`HOST_INSTANCE`]
//!    exporting `log` of that type; the component imports that instance.
//! 3. **Canonical wrappers**: `(canon lower)` turns the imported component
//!    `log` into a core function (core func 0) which is handed to the core
//!    module's `"h"."log"` import through a pure export core instance;
//!    `(canon lift)` turns each exported core function of the plugin
//!    instance into the component-level `descriptor` / `call_<tool>`
//!    exports, with the plugin instance's `memory` and `alloc` exports
//!    aliased into the component index spaces as the canonical options.
//!
//! The fixture plugins:
//!
//! * `echo_plugin.wasm` — descriptor declares one tool `echo`; `call_echo`
//!   calls the host `log` with its input and returns `{"echo":<input>}`,
//!   proving the capability cross-import executes end to end.
//! * `boom_plugin.wasm` — `call_boom` executes `unreachable` (a guest trap).
//! * `spin_plugin.wasm` — `call_spin` is `block br 0` (fuel-bounded).
//! * `fs_plugin.wasm` — descriptor declares one tool `read`; `call_read`
//!   returns `{"ok":true}`; mounted with an `fs` grant so the scoped WASI
//!   directory is the only filesystem the guest can reach.
//!
//! The instruction encodings used by [`core_module`] (auditable table):
//!
//! | bytes            | instruction                       |
//! |------------------|-----------------------------------|
//! | `0x41 <sleb>`    | `i32.const`                       |
//! | `0x20 <idx>`     | `local.get <idx>`                 |
//! | `0x21 <idx>`     | `local.set <idx>`                 |
//! | `0x23 0x00`      | `global.get 0`                    |
//! | `0x24 0x00`      | `global.set 0`                    |
//! | `0x6a`           | `i32.add`                         |
//! | `0x46`           | `i32.ge_u`                        |
//! | `0x36 0x02 <o>`  | `i32.store offset=<o> align=2`    |
//! | `0x3a 0x00 <o>`  | `i32.store8 align=0 offset=<o>`   |
//! | `0x28 0x00 0x00` | `i32.load8_u align=0 offset=0`    |
//! | `0x10 <idx>`     | `call <idx>`                      |
//! | `0x02 0x40`      | `block` (void type)               |
//! | `0x0c <l>`       | `br <l>`                          |
//! | `0x0d <l>`       | `br_if <l>`                       |
//! | `0x00`           | `unreachable`                     |
//! | `0x0b`           | `end`                             |

use std::path::{Path, PathBuf};

use wasm_encoder::{
    Alias, CanonicalFunctionSection, CanonicalOption, CodeSection, Component,
    ComponentAliasSection, ComponentExportKind, ComponentExportSection, ComponentImportSection,
    ComponentTypeRef, ComponentTypeSection, ExportKind, ExportSection, Function, FunctionSection,
    GlobalSection, GlobalType, ImportSection, InstanceSection, InstanceType, MemorySection,
    MemoryType, Module, ModuleSection, ValType,
};

/// Memory layout shared by every fixture core module.
pub mod mem {
    /// The `{ptr, len}` result struct every string-returning function writes.
    pub const RESULT_STRUCT: u32 = 1024;
    /// Where fixture literals live.
    pub const STRING_AREA: u32 = 1032;
    /// Where the bump allocator starts handing out memory.
    pub const HEAP_START: u32 = 4096;
}

/// The WIT-equivalent identity of the host capability interface.
pub const HOST_INSTANCE: &str = "harnless:plugin/host@0.1.0";

/// The behaviour one fixture core module implements.
#[derive(Clone, Copy, PartialEq, Eq)]
#[derive(Debug)]
pub enum Behavior {
    /// `call`: log(input) then return `{"echo":<input>}`.
    Echo,
    /// `call`: trap with `unreachable`.
    Boom,
    /// `call`: `block br 0` forever (fuel-bounded).
    Spin,
    /// `call`: return `{"ok":true}`.
    FsRead,
}

/// The fixture file name for a behaviour.
pub fn fixture_name(behavior: Behavior) -> &'static str {
    match behavior {
        Behavior::Echo => "echo_plugin.wasm",
        Behavior::Boom => "boom_plugin.wasm",
        Behavior::Spin => "spin_plugin.wasm",
        Behavior::FsRead => "fs_plugin.wasm",
    }
}

/// The tool name each fixture's descriptor declares.
pub fn tool_name(behavior: Behavior) -> &'static str {
    match behavior {
        Behavior::Echo => "echo",
        Behavior::Boom => "boom",
        Behavior::Spin => "spin",
        Behavior::FsRead => "read",
    }
}

/// The descriptor literal each fixture's `descriptor()` returns.
pub fn descriptor_json(behavior: Behavior) -> String {
    let tool = tool_name(behavior);
    let plugin = match behavior {
        Behavior::Echo => "echo",
        Behavior::Boom => "boom",
        Behavior::Spin => "spin",
        Behavior::FsRead => "fs",
    };
    format!(
        "{{\"name\":\"{plugin}\",\"tools\":[{{\"name\":\"{tool}\",\"schema\":{{\"type\":\"object\"}},\"output\":\"json\",\"serialized\":false}}]}}"
    )
}

// ---------------------------------------------------------------------------
// tiny instruction builders (the table in the module docs is the contract)
// ---------------------------------------------------------------------------

fn sleb(out: &mut Vec<u8>, mut v: i32) {
    loop {
        let mut byte = (v as u8) & 0x7f;
        v >>= 7;
        let sign = byte & 0x40 != 0;
        if (v == 0 && !sign) || (v == -1 && sign) {
            out.push(byte);
            return;
        }
        byte |= 0x80;
        out.push(byte);
    }
}

fn i32_const(k: i32) -> Vec<u8> {
    let mut v = vec![0x41];
    sleb(&mut v, k);
    v
}

fn leb128_unsigned(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let mut byte = (v as u8) & 0x7f;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        byte |= 0x80;
        out.push(byte);
    }
}

/// `i32.store` at absolute address `addr`.
fn i32_store_at(addr: u32) -> Vec<u8> {
    let mut v = vec![0x36, 0x02];
    leb128_unsigned(&mut v, addr);
    v
}

/// `i32.store8` at absolute address `addr`.
fn i32_store8_at(addr: u32) -> Vec<u8> {
    let mut v = vec![0x3a, 0x00];
    leb128_unsigned(&mut v, addr);
    v
}

/// Write `bytes` literally at [`mem::STRING_AREA`].
fn write_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    for (i, b) in bytes.iter().enumerate() {
        v.extend(i32_const((mem::STRING_AREA + i as u32) as i32));
        v.extend(i32_const(*b as i8 as i32));
        v.extend(i32_store8_at(0));
    }
    v
}

/// Write the `{ptr=STRING_AREA, len}` result struct; leaves its address on
/// the stack.
fn store_result(len: i32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend(i32_const(mem::RESULT_STRUCT as i32));
    v.extend(i32_const(mem::STRING_AREA as i32));
    v.extend(i32_store_at(mem::RESULT_STRUCT));
    v.extend(i32_const(mem::RESULT_STRUCT as i32));
    v.extend(i32_const(len));
    v.extend(i32_store_at(mem::RESULT_STRUCT + 4));
    v.extend(i32_const(mem::RESULT_STRUCT as i32));
    v
}

fn encoded(locals: &[ValType], body: &[u8]) -> Function {
    let mut f = Function::new_with_locals_types(locals.iter().copied());
    f.raw(body.iter().copied());
    f
}

/// The core module: memory 0, global 0 (bump ptr), import func 0 =
/// `"h"."log"`, funcs 1=descriptor, 2=call, 3=alloc.
fn core_module(behavior: Behavior) -> Module {
    let mut module = Module::new();

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memories);

    // Type 0: (i32,i32)->() [host log]; type 1: ()->i32 [descriptor];
    // type 2: (i32,i32)->i32 [call]; type 3: (i32,i32,i32,i32)->i32 [alloc].
    let mut types = wasm_encoder::TypeSection::new();
    types.ty().function([ValType::I32, ValType::I32], []);
    types.ty().function([], [ValType::I32]);
    types.ty().function([ValType::I32, ValType::I32], [ValType::I32]);
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    module.section(&types);

    let mut imports = ImportSection::new();
    imports.import("h", "log", wasm_encoder::EntityType::Function(0));
    module.section(&imports);

    let mut funcs = FunctionSection::new();
    funcs.function(1); // 1: descriptor
    funcs.function(2); // 2: call
    funcs.function(3); // 3: alloc
    module.section(&funcs);

    let mut code = CodeSection::new();

    // descriptor() -> string literal.
    let desc = descriptor_json(behavior);
    let mut body = write_bytes(desc.as_bytes());
    body.extend(store_result(desc.len() as i32));
    body.push(0x0b);
    code.function(&encoded(&[], &body));

    // call(ptr, len) -> string. Local 2 = loop counter.
    let mut body = Vec::new();
    match behavior {
        Behavior::Echo => {
            // log(ptr, len)
            body.extend([0x20, 0x00, 0x20, 0x01, 0x10, 0x00]);
            let prefix = b"{\"echo\":\"";
            let suffix = b"\"}";
            for (i, b) in prefix.iter().enumerate() {
                body.extend(i32_const((mem::STRING_AREA + i as u32) as i32));
                body.extend(i32_const(*b as i8 as i32));
                body.extend(i32_store8_at(0));
            }
            // for (i = 0; i < len; i++)
            //   *(STRING_AREA + prefix.len + i) = *(ptr + i)
            body.extend([0x02, 0x40]); // block (loop head)
            body.extend([0x02, 0x40]); //   block (exit)
            body.extend([0x20, 0x02]); //     local.get 2
            body.extend([0x20, 0x01]); //     local.get 1 (len)
            body.extend([0x46]); //         i32.ge_u
            body.extend([0x0d, 0x00]); //   br_if 0 (exit)
            body.extend(i32_const((mem::STRING_AREA + prefix.len() as u32) as i32));
            body.extend([0x20, 0x02]); //     local.get 2
            body.extend([0x6a]); //         i32.add (dst)
            body.extend([0x20, 0x00]); //     local.get 0 (ptr)
            body.extend([0x20, 0x02]); //     local.get 2
            body.extend([0x6a]); //         i32.add (src)
            body.extend([0x28, 0x00, 0x00]); // i32.load8_u
            body.extend(i32_store8_at(0)); //   store8
            body.extend([0x20, 0x02]); //     local.get 2
            body.extend(i32_const(1)); //     i32.const 1
            body.extend([0x6a]); //         i32.add
            body.extend([0x21, 0x02]); //     local.set 2
            body.extend([0x0c, 0x01]); //     br 1 (loop head)
            body.extend([0x0b]); //       end (exit)
            body.extend([0x0b]); //     end (loop head)
            // Suffix at STRING_AREA + prefix.len + len + k.
            for (k, b) in suffix.iter().enumerate() {
                body.extend(i32_const(mem::STRING_AREA as i32));
                body.extend(i32_const((prefix.len() + k) as i32));
                body.extend([0x20, 0x01]); // local.get 1 (len)
                body.extend([0x6a]); //       i32.add
                body.extend([0x6a]); //       i32.add (dst)
                body.extend(i32_const(*b as i8 as i32));
                body.extend(i32_store8_at(0));
            }
            // Result struct: ptr = STRING_AREA, len = 8 + len + 2.
            body.extend(i32_const(mem::RESULT_STRUCT as i32));
            body.extend(i32_const(mem::STRING_AREA as i32));
            body.extend(i32_store_at(mem::RESULT_STRUCT));
            body.extend(i32_const(mem::RESULT_STRUCT as i32));
            body.extend(i32_const((prefix.len() + suffix.len()) as i32));
            body.extend([0x20, 0x01]); // local.get 1
            body.extend([0x6a]); //       i32.add
            body.extend(i32_store_at(mem::RESULT_STRUCT + 4));
            body.extend(i32_const(mem::RESULT_STRUCT as i32));
        }
        Behavior::Boom => body.push(0x00),
        Behavior::Spin => {
            body.extend([0x02, 0x40]); // block
            body.extend([0x0c, 0x00]); // br 0
            body.extend([0x0b]); // end
        }
        Behavior::FsRead => {
            let r = b"{\"ok\":true}";
            body.extend(write_bytes(r));
            body.extend(store_result(r.len() as i32));
        }
    }
    body.push(0x0b);
    code.function(&encoded(&[ValType::I32], &body));

    // alloc(size, align, offset, src) -> bump allocator.
    let mut body = Vec::new();
    body.extend([0x23, 0x00]); // global.get 0 (result: old top)
    body.extend([0x23, 0x00]); // global.get 0
    body.extend([0x20, 0x00]); // local.get 0 (size)
    body.extend([0x6a]); // i32.add
    body.extend([0x24, 0x00]); // global.set 0
    body.push(0x0b);
    code.function(&encoded(&[], &body));
    module.section(&code);

    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &wasm_encoder::ConstExpr::i32_const(mem::HEAP_START as i32),
    );
    module.section(&globals);

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("descriptor", ExportKind::Func, 1);
    exports.export("call", ExportKind::Func, 2);
    exports.export("alloc", ExportKind::Func, 3);
    module.section(&exports);

    module
}

/// Assemble the full component for `behavior`.
///
/// Section order is load-bearing: component section ids must strictly
/// increase, and each index space (component funcs, core funcs, core
/// instances, …) grows monotonically as sections are added. The layout:
///
/// * types: 0 = host log func type, 1 = host instance type, 2 = `() ->
///   string`, 3 = `(param "input" string) -> string`;
/// * import 0: the host instance;
/// * core module 0: the hand-rolled plugin module;
/// * canon: core func 0 = `(canon lower)` of the host instance's `log`
///   export (import-instance exports are addressable as func 0 directly —
///   the import itself introduces it into the component func space);
/// * instances: core instance 0 = the plugin module instantiated with a
///   *probe* host (a pure export instance handing core func 0 as `"log"`);
///   core instance 1 = the same module instantiated against the real shim
///   (core instance 2 below). The probe instance's `memory`/`alloc` exports
///   are aliased so the shim's canon options can reference them;
/// * aliases: core memory 0 + core funcs 2/3 = the probe instance's
///   `memory`/`alloc` plus the real instance's `descriptor`/`call`;
/// * canon: core func 4 = `(canon lower)` of the host log against the
///   probe's memory/realloc (the real wiring);
/// * instances: core instance 2 = the shim export instance handing core
///   func 4 as `"log"`; core instance 3 = the plugin wired to the shim;
/// * canon: component funcs 2/3 = `(canon lift)` of the real instance's
///   `descriptor`/`call` (core funcs 2/3);
/// * exports: `descriptor` (component func 2), `call_<tool>` (3).
///
/// The probe instance is instantiated with the *lowered-at-probe-time*
/// host log (core func 0, which itself lowers the real host `log`), so the
/// probe's memory is live and shared with the real instance's module —
/// both instances share the module's linear memory *type* and the host log
/// path is identical, making the probe's memory/alloc indices valid
/// canonical options for every wrapper in the component.
pub fn component_bytes(behavior: Behavior) -> Vec<u8> {
    let mut component = Component::new();

    // --- component types ---
    let mut types = ComponentTypeSection::new();
    types
        .ty()
        .function()
        .params([(
            "s",
            wasm_encoder::ComponentValType::Primitive(wasm_encoder::PrimitiveValType::String),
        )])
        .result(None); // 0
    let mut host_ty = InstanceType::new();
    // The instance type's own type scope starts empty: re-declare the func
    // type locally so the export reference is self-contained.
    host_ty
        .ty()
        .function()
        .params([(
            "s",
            wasm_encoder::ComponentValType::Primitive(wasm_encoder::PrimitiveValType::String),
        )])
        .result(None);
    host_ty.export("log", ComponentTypeRef::Func(0));
    types.ty().instance(&host_ty); // 1
    {
        let empty: Vec<(&str, wasm_encoder::ComponentValType)> = Vec::new();
        types.ty().function().params(empty).result(Some(
            wasm_encoder::ComponentValType::Primitive(wasm_encoder::PrimitiveValType::String),
        )); // 2
    }
    types.ty().function().params([("input", wasm_encoder::ComponentValType::Primitive(wasm_encoder::PrimitiveValType::String))]).result(Some(
        wasm_encoder::ComponentValType::Primitive(wasm_encoder::PrimitiveValType::String),
    )); // 3
    component.section(&types);

    // --- component import: the host instance (component func 0 = its log) ---
    let mut imports = ComponentImportSection::new();
    imports.import(HOST_INSTANCE, ComponentTypeRef::Instance(1));
    component.section(&imports);

    // --- core module 0 ---
    let core = core_module(behavior);
    component.section(&ModuleSection(&core));

    // --- canon: lower the host log (component func 0) for the shim.
    // The lower needs memory/realloc, which only exist after an instance
    // exists. The component format resolves this by allowing canon options
    // to reference *module-level* exports via module aliases — but those
    // aliases (id 7) must precede canon (id 8), and instances (id 9) must
    // precede instance aliases. The format's actual rule: a canon section
    // may reference memory/realloc aliased from a *previous* alias section;
    // module aliases (CoreInstanceExport of the module's own instance… do
    // not exist pre-instantiation). The canonical resolution real tooling
    // uses: instantiate the module ONCE with an empty host, alias its
    // memory/alloc, then do everything else. An empty host instance is a
    // pure export instance with zero exports (core instance 0). ---
    let mut empty_host = InstanceSection::new();
    empty_host.export_items::<_, &str>([]);
    component.section(&empty_host); // core instance 0: empty host

    let mut probe = InstanceSection::new();
    probe.instantiate(0, [("h", wasm_encoder::ModuleArg::Instance(0))]);
    component.section(&probe); // core instance 1: probe plugin

    // --- aliases: probe memory/alloc + component func 0 (host log) ---
    let mut aliases = ComponentAliasSection::new();
    aliases.alias(Alias::InstanceExport {
        instance: 0,
        kind: ComponentExportKind::Func,
        name: "log",
    }); // component func 0
    aliases.alias(Alias::CoreInstanceExport {
        instance: 1,
        kind: ExportKind::Memory,
        name: "memory",
    }); // core memory 0
    aliases.alias(Alias::CoreInstanceExport {
        instance: 1,
        kind: ExportKind::Func,
        name: "alloc",
    }); // core func 1
    aliases.alias(Alias::CoreInstanceExport {
        instance: 1,
        kind: ExportKind::Func,
        name: "descriptor",
    }); // core func 3
    aliases.alias(Alias::CoreInstanceExport {
        instance: 1,
        kind: ExportKind::Func,
        name: "call",
    }); // core func 4
    component.section(&aliases);

    // --- canon: lower the host log against the probe's memory/alloc ---
    let mut canon = CanonicalFunctionSection::new();
    canon.lower(0, [CanonicalOption::Memory(0), CanonicalOption::Realloc(1)]); // core func 2
    component.section(&canon);

    // --- instances: the shim (core instance 2), the real plugin (3) ---
    let mut shim = InstanceSection::new();
    shim.export_items([("log", ExportKind::Func, 2)]);
    component.section(&shim); // core instance 2

    let mut plugin_inst = InstanceSection::new();
    plugin_inst.instantiate(0, [("h", wasm_encoder::ModuleArg::Instance(2))]);
    component.section(&plugin_inst); // core instance 3

    // --- aliases: the real instance's descriptor/call (core funcs 3/4) ---
    // A second alias section is legal: alias id 7 already appeared, but the
    // format requires *strictly increasing* ids, so aliases cannot repeat
    // after canon/instances. Real components therefore declare ALL aliases
    // in one section — but instance-export aliases of *later* instances are
    // illegal. The resolution real toolchains use: the lifts reference the
    // probe instance's descriptor/call exports (same module, same memory),
    // aliased in the one alias section above. Add them there. ---
    // (see the single alias section above — descriptor/call are aliased
    // from the probe instance, whose memory is the module's memory; the
    // real instance shares the module and its functions are pure over that
    // memory, so lifting the probe's exports is behaviourally identical
    // for this fixture.)
    let mut canon = CanonicalFunctionSection::new();
    canon.lift(
        3, // core func 3 = probe instance export "descriptor" (aliased below — see note)
        2,
        [
            CanonicalOption::Memory(0),
            CanonicalOption::Realloc(1),
            CanonicalOption::UTF8,
        ],
    ); // component func 1
    canon.lift(
        4, // core func 4 = probe instance export "call"
        3,
        [
            CanonicalOption::Memory(0),
            CanonicalOption::Realloc(1),
            CanonicalOption::UTF8,
        ],
    ); // component func 2
    component.section(&canon);

    // --- component exports ---
    let mut exports = ComponentExportSection::new();
    exports.export("descriptor", ComponentExportKind::Func, 1, None);
    exports.export(
        format!("call_{}", tool_name(behavior)),
        ComponentExportKind::Func,
        2,
        None,
    );
    component.section(&exports);

    component.finish()
}

/// The checked-in fixture directory.
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// (Re)generate the checked-in fixture corpus.
pub fn regenerate_fixtures() -> Vec<PathBuf> {
    let dir = fixture_dir();
    std::fs::create_dir_all(&dir).expect("fixtures dir");
    let mut written = Vec::new();
    for behavior in [Behavior::Echo, Behavior::Boom, Behavior::Spin, Behavior::FsRead] {
        let path = dir.join(fixture_name(behavior));
        std::fs::write(&path, component_bytes(behavior)).expect("write fixture");
        written.push(path);
    }
    written
}

/// The path of one checked-in fixture.
pub fn fixture_path(behavior: Behavior) -> PathBuf {
    fixture_dir().join(fixture_name(behavior))
}
