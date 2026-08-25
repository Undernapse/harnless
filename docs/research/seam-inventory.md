# dsh Seam Inventory & Core Scope Triage

Source of truth: `deepseek-ai/deepseek-harness` `docs/capability-seams.md` (the service/capability graph) plus `packages/**/README.md`. This doc lists every capability seam in `dsh`, triaged for the Rust clone: **core** (must have a defined service interface in the spec), **deferred** (in-scope, behind first release), **out-of-scope** (per map's Out-of-scope).

The harness expresses plugability as **seams**: a swappable capability with a Service Definition (the interface), Service Provider(s) (implementations), and Consumers (commonly a model-facing tool). One role alone is not a seam.

## Core seams (settled, must be defined in the spec)

These are the settled focus — the "all of its plugability" core.

| ctx key | Role | Owner | Providers (dsh) | Notes |
| --- | --- | --- | --- | --- |
| `ctx.tools` | core | `tools` | — (registry) | Tool registry + guarded execution pipeline. Owns Code Mode transport, routes calls through pre-policy, monotonic guards, around dispatch, post-policy, final-result observation. **This is where MCP client tools land.** |
| `ctx.llm` | seam | `llm` | `llm-deepseek`, `llm-pi-ai`, `llm-replay` | Provider-neutral streaming service; adapters register provider implementations. |
| `ctx.fs` | seam | `fs` | `fs-local`, `fs-sandbox`, `fs-e2b` | Filesystem provider; tool-fs reads/writes/edits through it; fs-sandbox fences mutations by the shared sandbox mode. |
| `ctx.subprocess` | seam | `subprocess` | `subprocess-local`, `subprocess-e2b` | Process spawn seam; bash executors, PTY shell, LSP host, out-of-process subagent backends all spawn through it. |
| `ctx.shell` | seam | `shell` | `bash-local`, `bash-sandbox`, `pwsh-local` | Bash executor seam; consumed by tool-bash, tool-pwsh, hook bridges. |
| `ctx.sandbox` | seam | `sandbox` | `sandbox-local` | Process-sandbox seam; consumers hand over exact argv, backends wrap it under per-call policy. |
| `ctx.sandboxPolicy` | core | `sandbox-policy` | — | Deployment default mode + workspace root; only enforcing families read it (bash + fs confining to same root). |
| `ctx.shellEnv` | core | `shell-env` | — | Effect-scoped `DSH_*` facts; shell tools collect one trusted snapshot per execution. |
| `ctx.sessions` | core | `session` | — | Append-only Session instances; emits the durable session event feed. |
| `ctx.sessionPersistence` | seam | `session-persistence` | `session-persistence-jsonl`, `session-persistence-sqlite` | Persists the same SessionEvent vocabulary; backend chosen at composition time. |
| `ctx.credentials` | seam | `credentials` | `credentials-local` | Secrets seam; config references secrets, providers own values, consumers resolve per operation so rotated creds reach the next request. |
| `ctx.authorization` | seam | `authorization` | — | Auth flows registered by the plugin that knows how to obtain a credential; owns conversation + one-attempt-per-key lifecycle. |
| `ctx.settings` | seam | `settings` | `settings-file` | Namespace schemas + layered value resolution; LLM adapters register entry config as composition base. |
| `ctx.storage` | seam | `storage` | `storage-json`, `storage-sqlite` | Non-session storage hub; backends register under names; data forms mount on the hub. |
| `ctx.storageDomain` | core | `storage-domain` | — | Waits for every backend, publishes the domain form as one lifecycle-bound service for typed durable state. |

## Required core spine (not "seams" but needed to hold the core together)

| ctx key | Role | Owner | Notes |
| --- | --- | --- | --- |
| `ctx.systemPrompt` | core | `system-prompt` | Collects prompt sections + model-facing tool schemas per step. |
| `ctx.agents` | core | `agent` | Live Agent handles, create/resume factory seam, initiator propagation. |
| `ctx.agentLoop` | bundle | `agent-loop` | The one concrete loop driver; extension packages depend on events/services, not this package. |
| `ctx.approval` | seam | `approval` | One-shot permission decisions over `approval/request` waterfall; absent → fails closed to `unavailable`. Needed by the tools guarded pipeline. |
| `ctx.scope` | library | `scope` | Per-agent scoped-registration primitive (no ctx key). |
| `ctx.invariants` | core | `invariants` | Runtime invariant registry; asserts "model-visible == logged". |
| `ctx.mcp` (proposal) | — | `mcp` | In dsh MCP is NOT its own service in the graph; `packages/mcp/mcp-client` bridges external server tools onto `ctx.tools`. The Rust clone must define an MCP seam to own the client bridge + protocol surface. |

## Deferred (in-scope, behind first release)

Not in the settled core set, but in-scope for the destination's "all of its plugability" — defined after the core lands.

| ctx key | Role | Notes |
| --- | --- | --- |
| `ctx.subagents` | seam | In-process spawn/fork providers first; external-product bridges are out-of-scope. |
| `ctx.web` | seam | Search/fetch providers (exa, perplexity, deepseek, http) + tool-web. |
| `ctx.skills` | seam | Skill providers merge catalogs; tool-skill renders + loads skill bodies. |
| `ctx.sessionQuery` | seam | Exact reads/filters/traces; sqlite backend adds full-text reconciliation. |
| `ctx.compaction` | seam | Post-step pressure + request-error recovery. |
| `ctx.jobs` | seam | Background job registry + tool-jobs controller. |
| `ctx.terminals` | seam | Persistent PTY session registry + tool-terminal. |
| `ctx.codeRuntime` | seam | Model-written program execution (Code Mode). |
| `ctx.sessionProjections` / `ctx.sessionProjectionCache` | core | State-driven fold units + durable checkpoints. |
| `ctx.sessionTitle` | seam | Log-backed titles. |
| `ctx.sessionReferenceResolver` | core | Cross-session snapshot projection. |
| `ctx.userQuestions` | seam | Human q/a seam (ask-user pausing). |
| `ctx.planMode` | core | Plan collaboration state. |
| `ctx.goals` | core | Same-session objective folding. |
| `ctx.commands` | core | Direct human commands without a model turn. |
| `ctx.workflowEngine` | seam | Workflow script engine. |
| `ctx.lsp` | seam | Language-server navigation seam. |
| `ctx.attachment` / `ctx.attachments` | seam | Durable binary attachment storage. |
| `ctx.fileReferences` | seam | Path-only completion candidates. |
| `ctx.tokenMeter` / `ctx.toolResultPruner` | core | Replay token measurement / replayable tool-result pruning. |
| `ctx.spillStore` | seam | Oversized tool-text spill backend. |
| `ctx.directoryPicker` | seam | Workspace-directory picking. |
| `ctx.sessionTelemetry` | seam | Telemetry backend (otel). |
| `ctx.messageFeedback` | core | Per-assistant-message feedback. |
| `ctx.workspaceRegistry` | core | Workspace entity registry. |
| `ctx.agentDefaultModel` / `ctx.agentPresets` | core | Default model selection / per-session preset composition. |
| `ctx.apiProxy` | core | Host API dispatch gateway face. |

## Out-of-scope (per map)

| Seam | Why |
| --- | --- |
| Web UI / web client packages (`apps/web`, `packages/web` browser client, `ConversationNodeDefinition`) | Destination is the harness engine + seams, not the browser product. |
| `ctx.agentTeams` (Agent Teams) | Experimental private opt-in; behind first release. |
| External-product subagent bridges (`subagent-claude-code`, `subagent-codex`, `subagent-acp`) | Clone ships in-process subagent seam or omits; not the external bridges. |
| Dynamic Cordis host runner / HMR / client modules (`cordis-host-runner`, `modules`, `hmr`, `ctx.clientModules`, `ctx.webServer`) | Replaced by the hybrid dynamic-extension decision (WASM tool/MCP plugins). |

## Open questions surfaced

- MCP is a bridge onto `ctx.tools`, not a first-class seam in dsh's graph. The Rust spec must decide whether `ctx.mcp` becomes a first-class seam (recommended: yes, to own protocol surface + transport).
- `ctx.invariants` ("model-visible == logged") is load-bearing for the event-sourced core; recommend keeping it core.
