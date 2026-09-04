# Map: Rust reimplementation of the DeepSeek Harness

## Destination

A written architecture/design spec handed off to a separate planning-and-execution effort, for a **standalone greenfield Rust reimplementation of the DeepSeek Harness (`dsh`)** that preserves `dsh`'s plugability: the Cordis-style "everything is a plugin" architecture — a service-context runtime with typed events and reversible effects, an event-sourced session log driving an agent loop, and a core set of swappable capability seams (tools/MCP, llm, fs, subprocess/shell/sandbox, sessions, credentials, settings, storage). The spec's job is to pin down enough decisions that executing it needs no further design.

> **Destination reached.** The spec is published at [spec.md](spec.md), labelled `ready-for-agent` — 118 user stories, implementation decisions sourced from all seven tickets, two test seams. Nothing here is left to decide before execution.

## Notes

- **Domain**: Rust reimplementation of an existing TypeScript agent-harness architecture. The reference codebase is `deepseek-ai/deepseek-harness` (cloned at `/tmp/dsh` for this session; the canonical repo is on GitHub).
- **Skill to consult when resolving HITL tickets**: `grilling` + `domain-modeling` (call the Skill tool for both).
- **Skills for research tickets**: `research`.
- **Reference docs to consult** (in `/tmp/dsh/docs/`): `architecture.md`, `capability-seams.md`, `tool-execution-pipeline.md`, `cordis-primer.md`, `agent-lifecycle.md`, `event-producer-consumer.md`, `module-graph.md`, `config-catalog.md`, plus per-package READMEs (`packages/**/README.md`).
- **User decisions already locked** (from charting): foundation = hand-rolled Rust Cordis-equivalent; dynamism = hybrid (core seams compile-time traits, dynamic WASM extension for MCP/tool plugins); core = event-sourced session log + agent loop; seam scope = core seam architecture, not every provider; MCP = client bridge consuming external servers into `ctx.tools`; llm = OpenAI-compatible streaming + replay adapter.
- Representative directory is `/tmp/dsh` — may not persist across sessions; re-clone from upstream if needed.

## Decisions so far

- [Map dsh's seam inventory & triage core scope](issues/01-research-seam-inventory.md): the definitive seam triage — core set (tools+MCP, llm, fs, subprocess/shell/sandbox, sessions/persistence, credentials/authorization, settings, storage/storageDomain) plus a required core spine (systemPrompt, agents, agentLoop, approval, scope, invariants) and a Rust `ctx.mcp` seam that must exist because dsh's MCP is only a bridge onto `ctx.tools`. Full table: `docs/research/seam-inventory.md`.
- [Codify Cordis semantics the Rust runtime must match](issues/02-research-cordis-semantics.md): pinned the semantics — **five** dispatch modes (emit/parallel/serial/bail/waterfall with the waterfall `next()` veto contract), service-by-key contexts with `inject`-driven load, reversible effects/disposers with fiber lifecycle, and profile/bundle/patch transactional config loading. Reference: `docs/research/cordis-semantics.md`.
- [Design the Rust service-context + typed-event runtime](issues/03-grilling-rust-runtime.md): the runtime design. Type-token service map (`ctx.get::<S>()`, type = the key); one `EventRegistry` per domain with all **five** dispatch modes (waterfall via a `next()`-chain continuation, `prepend` supported); RAII `Disposer` guards with fiber-owned LIFO unwind and reject-while-unloading; `trait Plugin { inject(); apply() }` with service-requirement activation, one fiber per mounted plugin (reversible plugin swap); tokio multi-thread, `Send + Sync`, `RwLock`ed service map. Workspace: `dsh-runtime`, `dsh-seams`, `dsh-agent`, `dsh-mcp`; typed accessors (`ctx.tools()`) over the raw typemap; `ctx.mcp` seam existence + crate pinned (interface deferred to 07).
- [Settle the event-sourced session log + agent loop](issues/04-grilling-session-log.md): full-fidelity `SessionEventMap` (turn/step, user/assistant/tool, raw chunk replay, request/header, end-seed), serde closed enum with source-validated lossless JSON + immutable committed events; `deriveMessages()` surface fold (`append`/`replace`) cached per node; **plugin event extension = core enum + plugin extension registry** for log-only types (the Rust-vs-TS declaration-merging seam); full turn/step loop with `agent/pre-step`, `llm/stream`, `tools/*` waterfalls on the five dispatch modes; in-memory `SessionStore` + `ctx.sessionPersistence` seam (JSONL-first), between-turn `fork`, crash-recovery `interrupted`. Compaction `replace` producers and tools pipeline internals deferred (07).
- [Design the tool registry + MCP client bridge](issues/07-grilling-tools-mcp-bridge.md): full-fidelity `ToolDefinition` (model `ToolSchema` + canonical `output` + `execute` + optional finalize/timeout/concurrency/presenters) with a `schemas()` allowlist keeping internal fields off the wire; scoped registration + `ToolRestriction` allow/deny on the `isolate` scope; the guarded pipeline (pre-execute → monotonic guards → execute → post-execute → finalizeContent → result) with the `ctx.approval` seam failing closed, mapped to five dispatch modes; a Rust JSON-value schema DSL compiling to JSON Schema; MCP bridge targeting SDK-1.12-era MCP (stdio + streamable-http, JSON-RPC 2.0) via a Rust client (`rmcp`), tools-only, `mcp__<server>__<raw>` naming, discovery + re-sync + generation-swap + backoff reconnect.
- [Define the core seam service interfaces](issues/05-grilling-core-seam-interfaces.md): the remaining core seams (llm, fs, credentials, settings, storage, subprocess/shell/sandbox; tools+sessions done in 07/04). Port fs + llm **faithfully** as Rust traits preserving swap-safety contracts (FsTarget/FsVersion/guarded intents/FsErrorCode taxonomy + fs/* policy gate; StreamChunk adapter contract + BlockAssembler + TokenUsage). Each seam = a `trait` on the type-token map via typed accessor (`ctx.fs()`, `ctx.llm()`), Send+Sync futures; seam trait defs in `dsh-seams`, providers as swappable crates/features (fs-local, llm-openai/replay, credentials-local, settings-file, storage-jsonl, bash/subprocess/sandbox-local), consumers in tool crates. fs+llm at full contract depth; the rest lighter (definition+provider+consumer). Named consumers (tool-fs, loop↔llm, tool-bash, providers↔credentials/settings/storage, loop↔sessions). Layered `Settings` + per-operation `Credentials` semantics locked.
- [Design the dynamic WASM extension surface](issues/06-grilling-wasm-extension.md): hybrid dynamism shape. Core seams compile-time native; the dynamic WASM surface is **model-facing tool + MCP plugins only**. Embed **wasmtime** with the **component model / WASI preview-2** (typed ABI + capability sandboxing); a `.wasm` plugin exports a descriptor declaring its tools (conforming to `ToolDefinition`), host registers onto `ctx.tools`, async boundary via host-polled structured results. Config/boot reproduces the layered profiles/bundles/patch/`--dump-config` model in Rust (YAML). WASM plugin = a **boot entry** mounted into a **per-plugin fiber** (reversible unwind, transactional hot-reload keeps last-good-tree on failure). WASI preview-2 capability model: config-granted capabilities only, no ambient host access.

## Not yet specified

Empty. All seven tickets are resolved and the spec is published, so nothing remains to decide before execution.

One item was carried forward as an explicit execution-time choice rather than a map ticket: the **CLI / execution surface** (the analog of `dsh --profile <name> --dump-config` and the `headless` one-shot runner). The config-boot model is decided (06) and the browser app is out of scope; the exact verb set is left to the planning effort and is noted in the spec's Further Notes.

## Implementation tasks

The design is settled and the spec is published as `ready-for-agent`. The build is tracked as `wayfinder:task` sub-issues under this map (GitHub #1). Core runtime, seams, and the agent crate are already started in-repo (`crates/`); the tickets below decompose the remaining work.

- **Providers**
  - [Provider: llm-openai (OpenAI-compatible adapter)](https://github.com/Undernapse/harnless/issues/9) — OpenAI-compatible streaming adapter (DeepSeek/vLLM), disjoint usage, watchdog, two failure paths, replay-state ownership.
  - [Provider: llm-replay (deterministic replay adapter)](https://github.com/Undernapse/harnless/issues/11) — deterministic replay provider for tests/demos; drives the primary seam test.
  - [Provider: fs-local (local filesystem)](https://github.com/Undernapse/harnless/issues/12) — opaque targets, version guards, atomic edit, error taxonomy, `fs/*` policy gate.
  - [Provider: execution world (subprocess/bash/sandbox-local + PolicyHome)](https://github.com/Undernapse/harnless/issues/13) — shared confinement policy home so fs and subprocess never disagree.
  - [Providers: settings-file / credentials-local / storage-jsonl](https://github.com/Undernapse/harnless/issues/14) — layered settings, per-operation credential resolution, named storage backends.
- **Integration**
  - [Implement dsh-mcp crate: MCP seam + client bridge](https://github.com/Undernapse/harnless/issues/10) — tools-only client bridge, namespaced naming, generation replacement, outage/backoff.
  - [Config/boot composition: layered profiles, bundles, patch, --dump-config](https://github.com/Undernapse/harnless/issues/17) — YAML layered boot, id-targeted whole-config patch, dump-equals-mount.
  - [Dynamic WASM plugin surface (wasmtime, component model, per-plugin fiber)](https://github.com/Undernapse/harnless/issues/15) — largest risk; first milestone = one tool plugin end-to-end.
  - [CLI / execution surface (verb set + default profile)](https://github.com/Undernapse/harnless/issues/16) — the deliberately-carried-forward item: profile/dump-config/one-shot runner.
- **Test seams**
  - [Durability/replay tests](https://github.com/Undernapse/harnless/issues/18) — round-trip exact log, fork boundary refusal, crash-recovery interrupted, no-grow-on-reopen.
  - [Primary seam test: agent loop via session log (replay adapter)](https://github.com/Undernapse/harnless/issues/19) — the single highest seam; the log is the observable behavior.
  - [Plugability conformance kit](https://github.com/Undernapse/harnless/issues/20) — one parameterized contract suite run unchanged against every provider.

## Out of scope

- **Web UI / web client** (dsh's `apps/web`, `packages/web` browser client, `ConversationNodeDefinition`): the destination is the harness engine + seams, not the browser product.
- **Agent Teams coordination domain** (`agent-team`): experimental private opt-in seam; behind the first release.
- **Subagent providers that shell out to other products** (`subagent-claude-code`, `subagent-codex`, `subagent-acp`): the clone ships an in-process subagent seam or omits it; not porting the external-product bridges.
- **Dynamic Cordis package host runner / HMR / client modules** (`cordis-host-runner`, `modules`, `hmr`): replaceable by the hybrid dynamic-extension decision (WASM tool/MCP plugins), so the JS hot-load machinery is out.
