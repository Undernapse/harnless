# Map: Rust reimplementation of the DeepSeek Harness

## Destination

A written architecture/design spec handed off to a separate planning-and-execution effort, for a **standalone greenfield Rust reimplementation of the DeepSeek Harness (`dsh`)** that preserves `dsh`'s plugability: the Cordis-style "everything is a plugin" architecture — a service-context runtime with typed events and reversible effects, an event-sourced session log driving an agent loop, and a core set of swappable capability seams (tools/MCP, llm, fs, subprocess/shell/sandbox, sessions, credentials, settings, storage). The spec's job is to pin down enough decisions that executing it needs no further design.

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

## Not yet specified

Fog: suspected questions, sharper than before but not yet ticketed (config/boot composition now lives in ticket 06, the session-log persistence backend in 04, the MCP protocol surface in 07, and the crate/workspace layout is now decided in 03; the deferred-seam list is already decided in research 01).

- **CLI / execution surface**: the Rust analog of `dsh web` / `dsh --profile web --dump-config` / the `headless` one-shot runner. Headless style is in-scope; the browser web app is out-of-scope, but the CLI verbs are not yet pinned.
- **First-release cut line**: which deferred seams (research 01) must be in the spec's first release vs held for a second pass. Depends on how the core seams (05) and runtime (03) land.
- **"Everything is a plugin" parity target**: how faithful the Rust clone's config-driven plugin swap must be against dsh's (users swap providers via config). May resolve into a stated config-format commitment once 06 lands.

## Out of scope

- **Web UI / web client** (dsh's `apps/web`, `packages/web` browser client, `ConversationNodeDefinition`): the destination is the harness engine + seams, not the browser product.
- **Agent Teams coordination domain** (`agent-team`): experimental private opt-in seam; behind the first release.
- **Subagent providers that shell out to other products** (`subagent-claude-code`, `subagent-codex`, `subagent-acp`): the clone ships an in-process subagent seam or omits it; not porting the external-product bridges.
- **Dynamic Cordis package host runner / HMR / client modules** (`cordis-host-runner`, `modules`, `hmr`): replaceable by the hybrid dynamic-extension decision (WASM tool/MCP plugins), so the JS hot-load machinery is out.
