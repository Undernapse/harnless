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

## Not yet specified

Fog: suspected questions, sharper than before but not yet ticketed (config/boot composition now lives in ticket 06, the session-log persistence backend in 04, and the MCP protocol surface in 07; the deferred-seam list is now decided in research 01).

- **Rust crate/workspace layout**: how the clone is split into crates (core runtime crate, seam crates, WASM plugin host, integration), and how the Cordis-equivalent runtime exposes itself to downstream crates. Still fuzzy — depends on the runtime grilling (03).
- **CLI / execution surface**: the Rust analog of `dsh web` / `dsh --profile web --dump-config` / the `headless` one-shot runner. Headless style is in-scope; the browser web app is out-of-scope, but the CLI verbs are not yet pinned.
- **First-release cut line**: which deferred seams (research 01) must be in the spec's first release vs held for a second pass. Depends on how the core seams (05) and runtime (03) land.
- **"Everything is a plugin" parity target**: how faithful the Rust clone's config-driven plugin swap must be against dsh's (users swap providers via config). May resolve into a stated config-format commitment once 06 lands.

## Out of scope

- **Web UI / web client** (dsh's `apps/web`, `packages/web` browser client, `ConversationNodeDefinition`): the destination is the harness engine + seams, not the browser product.
- **Agent Teams coordination domain** (`agent-team`): experimental private opt-in seam; behind the first release.
- **Subagent providers that shell out to other products** (`subagent-claude-code`, `subagent-codex`, `subagent-acp`): the clone ships an in-process subagent seam or omits it; not porting the external-product bridges.
- **Dynamic Cordis package host runner / HMR / client modules** (`cordis-host-runner`, `modules`, `hmr`): replaceable by the hybrid dynamic-extension decision (WASM tool/MCP plugins), so the JS hot-load machinery is out.
