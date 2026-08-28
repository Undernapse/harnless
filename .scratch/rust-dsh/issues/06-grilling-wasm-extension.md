# 06 — Design the dynamic WASM extension surface

Type: grilling
Status: resolved
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

Resolved by grilling (1 round, all recommendations accepted) with the dev. The dynamic WASM extension surface design the spec must document — the runtime "plug in anything" complement to the compile-time core seams. Consumed research 02 (Cordis loader/config/patch semantics), the runtime (03), the tools pipeline (07), and dsh's boot composition (`@deepseek-ai/dsh-app-boot` README).

- **Q1 — Compile-time vs runtime split.** Core seams are **compile-time native** Rust (llm, fs, shell, sandbox, credentials, settings, storage, sessions, tools pipeline, agent loop, MCP client — in `dsh-seams`/`dsh-runtime`/`dsh-agent`/`dsh-mcp`). The **dynamic WASM surface is limited to model-facing tool + MCP plugins** — the "plug in anything" capability users get without recompiling, while the core engine stays fast and type-safe. This is the settled hybrid dynamism.
- **Q2 — WASM plugin ABI.** Embed **wasmtime** using the **component model / WASI preview-2** for a typed ABI + capability sandboxing. A plugin is a `.wasm` component exporting a descriptor that declares its name and tools (conforming to `ToolDefinition` from 07: name/description/parameters + execute). The host registers those onto `ctx.tools` and calls back into the guest to `execute(args, call_ctx)`. **Async boundary**: WASM functions are synchronous; long-running tool work returns a structured result (or an opaque handle the host polls) while real async runs on the host runtime under the guarded pipeline.
- **Q3 — Config & boot composition (Rust analog).** Reproduce dsh's layered boot model in Rust, config as **YAML** (`serde_yaml`) mirroring `cordis.yml`: a **profile** = ordered bundle list + user patch; **bundles** declare their insert rows; a **patch** file is id-targeted (replaces a row's whole `config`, restating kept fields) or `insert`; `--dump-config` composes base + overlay layers offline (equal to what boot mounts). Home/profile dir analog of `$DSH_HOME/profiles/<name>`. Id-targeted replace (not deep-merge) matches dsh. Layering is what makes "swap a provider via config" work.
- **Q4 — Dynamic plugin mount & unwind.** A WASM plugin is a **boot entry**: the config tree lists plugin rows (id, wasm path, config); a loader (Cordis Loader analog) mounts each into a **per-plugin fiber** (runtime 03 Q8) that owns its registrations — unloading the fiber unwinds exactly that plugin's tool registrations/listeners/schemas (reversible plugin swap). Hot reload = re-mount the fiber on config change, **transactionally**: on failure the last good tree stays registered (mirroring dsh's HMR compose). Config entries mount concurrently; a failed boot disposes the partial tree and rejects loudly.
- **Q5 — Capability / sandbox.** **WASI preview-2 capability model**: a plugin receives only the capabilities its config grants — none by default beyond its own sandbox; a fs-capable plugin gets a host-provisioned scope. The host never leaks ambient env or network. This is the `ctx.sandbox` boundary (03/05) applied to the WASM surface — what makes "plug in anything" safe for third-party tool plugins.

**Spec locks:** Q1–Q5. **Deferred:** hot-reload HMR surface breadth beyond `cordis.patch.yml`-style config watching, wasmer alternative, non-tool WASM plugins (the surface is tools+MCP only by Q1). Sources: research 02, runtime 03, tools pipeline 07, `packages/boot/app-boot/README.md`.
