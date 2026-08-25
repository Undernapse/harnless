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

<!-- one line per closed ticket; gist + link. Populated on resolution. -->

## Not yet specified

Fog: suspected questions, sharper than before but not yet ticketed.

- **Config & boot composition**: how profiles/bundles/config-patching (`cordis.patch.yml` analog) and a `--dump-config`-style tree work in Rust. Hangs on the service-context runtime design.
- **Session persistence backend**: append-only log is decided, but storage behind it (rolling file / sqlite / in-memory + durability) and replay/fork mechanics are open. Hangs on the session-log + agent-loop grilling.
- **Deferred seam interfaces**: which interfaces from `dsh`'s capability graph are deferred (out of core spec but still "later"): skill provider registry, web UI / web-access providers, workflow engine, lsp, agent-team, compaction, spill, subagents. Some may be genuinely out of scope (see Out-of-scope); others are in-scope-but-not-core.
- **The MCP client bridge depth**: which MCP spec version(s), transports (stdio/HTTP), JSON-RPC framing, tool-schema mapping onto `ctx.tools` schemas. Breadth is decided (client, consume external) but the protocol surface is not.

## Out of scope

- **Web UI / web client** (dsh's `apps/web`, `packages/web` browser client, `ConversationNodeDefinition`): the destination is the harness engine + seams, not the browser product.
- **Agent Teams coordination domain** (`agent-team`): experimental private opt-in seam; behind the first release.
- **Subagent providers that shell out to other products** (`subagent-claude-code`, `subagent-codex`, `subagent-acp`): the clone ships an in-process subagent seam or omits it; not porting the external-product bridges.
- **Dynamic Cordis package host runner / HMR / client modules** (`cordis-host-runner`, `modules`, `hmr`): replaceable by the hybrid dynamic-extension decision (WASM tool/MCP plugins), so the JS hot-load machinery is out.
