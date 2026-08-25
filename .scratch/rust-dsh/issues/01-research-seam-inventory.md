# 01 — Map dsh's seam inventory & triage core scope

Type: research
Status: resolved

## Question

Produce the definitive inventory of `dsh`'s capability seams from its capability graph (`docs/capability-seams.md` + `packages/**/README.md`), and triage each seam as **core** (must have a defined service interface in the Rust spec), **deferred** (in-scope, behind first release), or **out-of-scope** (see map's Out-of-scope). The result feeds the "Define the core seam service interfaces" grilling ticket.

The settled direction: the clone ports the **seam architecture** (service definition + provider + consumer) for the core set — tools/MCP client, llm, fs, subprocess/shell/sandbox, sessions, credentials, settings, storage — not every provider. This ticket makes that list precise and defensible.

## Answer

Full triage delivered in [`docs/research/seam-inventory.md`](../../../docs/research/seam-inventory.md). Summary:

- **Core seams** (settled, must be defined in the Rust spec): `ctx.tools` (+ MCP client bridge onto it), `ctx.llm`, `ctx.fs`, `ctx.subprocess`, `ctx.shell`, `ctx.sandbox`, `ctx.sandboxPolicy`, `ctx.shellEnv`, `ctx.sessions`, `ctx.sessionPersistence`, `ctx.credentials`, `ctx.authorization`, `ctx.settings`, `ctx.storage`, `ctx.storageDomain`.
- **Required core spine** (not seams but needed to hold the core together): `ctx.systemPrompt`, `ctx.agents`, `ctx.agentLoop`, `ctx.approval` (tools guarded pipeline), `ctx.scope`, `ctx.invariants` (enforces "model-visible == logged"), plus a Rust `ctx.mcp` seam must be created because dsh MCP is only a bridge onto `ctx.tools`, not a first-class seam.
- **Deferred** (behind first release): subagents (in-process only), web, skills, sessionQuery, compaction, jobs, terminals, codeRuntime, sessionProjections/cache, sessionTitle, sessionReferenceResolver, userQuestions, planMode, goals, commands, workflowEngine, lsp, attachment(s), fileReferences, tokenMeter/toolResultPruner, spillStore, directoryPicker, sessionTelemetry, messageFeedback, workspaceRegistry, agentDefaultModel, agentPresets, apiProxy.
- **Out-of-scope** (per map): web UI/browser product, agentTeams, external-product subagent bridges (claude-code/codex/acp), dynamic Cordis host runner/HMR/client-modules/webServer.

Open question recorded: Rust spec should make `ctx.mcp` a first-class seam to own the protocol surface + transport.
