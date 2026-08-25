# 06 — Design the dynamic WASM extension surface

Type: grilling
Status: open
Blocked by: 03

## Question

Design the **hybrid dynamism** the user locked: core seams are compile-time Rust traits; a dynamic extension surface (WASM MCP/tool plugins) provides the runtime "plug in anything" feel that `dsh` gets from hot-loaded Cordis plugins.

Decide:

- **What's compile-time vs runtime** — which registrations are native Rust traits at build time, which load as WASM at runtime.
- **The WASM plugin ABI** — host↔plugin interface for tool and MCP plugins: how a WASM plugin declares itself, registers onto `ctx.tools`, and executes under the guarded pipeline. Which wasm runtime (wasmtime / wasmer), capability/sandbox model, async boundary.
- **Config & boot composition** — the Rust analog of profiles / bundles / `cordis.patch.yml` / `--dump-config`: how the plugin tree and config patching are expressed and composed at boot.
- **Teardown/unwind of dynamic plugins** — how a reloaded/unloaded WASM plugin unwinds its registrations given Rust's static types.

Consume `02-cordis-semantics` research (loader/config semantics) and `03-rust-runtime` (the registration model dynamic loading must hook into).

## Answer

(blank until resolved)
