# Spec: Rust reimplementation of the DeepSeek Harness

Status: ready-for-agent
Effort: [rust-dsh map](map.md) — all 7 decision tickets resolved
Date: 2026-08-28

## Problem Statement

The DeepSeek Harness (`dsh`) is an open-source agent harness whose defining property is that **everything is a plugin**: the model adapter, the tool registry, the filesystem, the sandbox, the session log, and the agent loop itself are all swappable services composed from configuration at boot. It is written in TypeScript on a plugin framework (Cordis) that leans hard on JavaScript's dynamic capabilities — services looked up by string key, plugins hot-loaded and hot-swapped, event maps extended by declaration merging, registrations that unwind themselves on unload.

That architecture is attractive but the implementation language imposes costs `dsh` cannot avoid: a Node runtime and a large pnpm workspace, hot-reload machinery that must be hand-hardened against reentrant teardown, dynamic typing that pushes contract violations to runtime, and a single-process execution model. The harness's *design* is worth keeping; the *substrate* is what's in the way.

There is no Rust harness that offers the same guarantee — that any part of the product, from the model adapter to the loop driver, can be replaced from config without touching the code around it.

## Solution

A standalone, greenfield Rust reimplementation of `dsh`'s harness: the same "everything is a plugin" architecture, rebuilt on a Rust-native service-context runtime.

A running system is a **context** holding services that consumers find by stable key rather than by importing an implementation. Each swappable capability is a **seam** with three roles — a service definition (a Rust trait), one or more providers (implementations, independently distributed), and consumers (usually model-facing tools). Capabilities communicate through **typed events** with five dispatch modes, including a waterfall mode that lets a listener wrap or veto an action. Every registration is a **reversible effect** owned by the plugin that made it, so unloading a plugin unwinds exactly its own tools, listeners, and prompt sections.

An **append-only session event log** is the single source of truth for what the model sees: message history is derived from it, never stored separately, so replay, fork, and persistence all read the same stream. A **turn/step agent loop** drives that log and exposes its own extension points as waterfalls.

Because Rust cannot hot-load crates the way Node hot-loads packages, plugability is **hybrid**: core seams are compile-time traits, while model-facing tool and MCP plugins load at runtime as sandboxed WASM components declared in configuration — so third-party capability still arrives without recompiling the engine.

## User Stories

### The plugin runtime

1. As a harness operator, I want every part of the product — model adapter, tools, filesystem, sandbox, session store, even the agent loop — to be replaceable from configuration, so that I can shape a deployment without patching a privileged core.
2. As a plugin author, I want to look up a capability by its stable service key rather than importing a concrete implementation, so that my code keeps working when the deployment swaps the provider underneath me.
3. As a plugin author, I want to declare which services my plugin requires, so that the loader activates my plugin only once they all exist and I never see a half-built context.
4. As a plugin author, I want each of my registrations to carry its own undo, so that when my plugin is removed its tools, listeners, and prompt sections disappear with it and leave nothing dangling.
5. As a plugin author, I want teardown to happen in a predictable order, so that a service my plugin depends on is never disposed while my plugin is still unwinding.
6. As a plugin author, I want the runtime to refuse registrations made during teardown, so that a late-writing plugin cannot leak state past an unload.
7. As a plugin author, I want to observe an event without being able to change it, so that logging and telemetry never alter program behavior.
8. As a policy author, I want to wrap an action and decide whether it proceeds, so that approval, permission, and sandboxing layer onto the pipeline without the pipeline knowing about them.
9. As a policy author, I want to replace a result after the fact, so that pruning, spilling, and redaction apply uniformly across many tools.
10. As a plugin author, I want a failing listener to be contained rather than fatal, so that one broken observer cannot take down a session.
11. As a developer, I want async work in the service layer to be `Send + Sync` on a single committed runtime, so that the harness behaves the same in every build.

### Configuration and boot

12. As a harness operator, I want a named profile that stacks ordered bundles, so that I can reproduce a teammate's composition exactly.
13. As a harness operator, I want to patch a composed entry by its id from my own file, so that I can override one provider's config without editing the bundle that ships it.
14. As a harness operator, I want my patch layer to outrank the shipped bundles, so that local changes win without forking anything.
15. As a harness operator, I want to dump the fully composed configuration, so that I can see what a boot will actually mount and diff two machines.
16. As a harness operator, I want a bad patch to leave the last good configuration running, so that editing config live cannot brick a running harness.
17. As a harness operator, I want a boot failure to name the exact plugin and stage that failed, so that I am not debugging a silent partial mount.
18. As a harness operator, I want partial boot state disposed on failure, so that a failed start cannot leave a process holding terminals or sockets.
19. As a harness operator, I want per-run overlays on top of the profile, so that CI can swap one entry without touching stored profiles.

### The session log

20. As a user, I want the harness to record everything it showed the model, so that a resumed session sees exactly what it saw before.
21. As a developer, I want anything reaching a model request to be reconstructable from the log, so that no hidden state can make a replay diverge from the original run.
22. As a developer, I want the log to be append-only with contiguous positions, so that a replay never has to reason about gaps.
23. As a developer, I want an event rejected at the moment it is appended if it cannot be stored losslessly, so that corruption is never discovered later during a flush.
24. As a developer, I want committed events to be immutable, so that no code path can rewrite history after the fact.
25. As a user, I want a session to record the model's raw streamed pieces, so that a replayed conversation looks like the one I watched happen.
26. As a developer, I want the model's view derived from a recorded surface rather than from raw history, so that a summarizing pass can retire stale turns without rewriting what actually happened.
27. As a developer, I want a projection that retires earlier turns to cite what it retired, so that nothing vanishes from a replay unnoticed.
28. As a user, I want a turn that was cut short to be distinguishable from one that finished cleanly, so that a truncated answer is never silently treated as complete.
29. As a user, I want an interrupted session's unfinished turn closed as interrupted on reload, so that reopening after a crash does not leave the harness mid-thought.
30. As a user, I want to fork a session from any point between turns, so that I can explore a different direction without losing the original.
31. As a user, I want a fork request that lands mid-turn refused rather than quietly trimmed, so that the boundary I asked for is the boundary I get.
32. As a user, I want sessions saved incrementally without the conversation waiting on disk, so that a slow storage device never adds latency to a reply.
33. As a harness operator, I want to choose a storage backend from configuration, so that one deployment keeps flat files and another keeps a database.
34. As a developer, I want a plugin to add its own recorded event types, so that features like summarizing and hook bridges can persist their history without editing the core vocabulary.
35. As a developer, I want an unrecognized record to be readable when it is marked ignorable and to stop a replay when it is not, so that a reader never silently reconstructs from a gutted history.

### The agent loop

36. As a user, I want the harness to keep working through as many model calls and tool rounds as the task needs, so that a single request can do real work.
37. As a user, I want to send a follow-up while the harness is still working, so that I can steer it instead of waiting for it to finish.
38. As a plugin author, I want to decide what the model is shown before a step runs, so that steering, guardrails, and injected context are all one mechanism.
39. As a plugin author, I want to stop a turn at its boundary, so that a policy can end a run without killing the process.
40. As a user, I want a turn that was refused still recorded, so that the log explains why the harness went quiet.
41. As a plugin author, I want to rewrite a failed model request into a retry, so that transient provider failures and context overflow are recoverable without the loop knowing the difference.
42. As a developer, I want the assembled prompt and tool set for each request recorded, so that any past request can be reconstructed exactly.
43. As a developer, I want token accounting stored with the answer it describes, so that usage never needs a separate reconciliation pass.
44. As a user, I want independent tool calls to run together, so that a slow one does not block the others.
45. As a user, I want stateful tool calls to be serialized, so that parallel execution cannot corrupt shared state.

### The tool registry and guarded pipeline

46. As a plugin author, I want to register a model-facing capability with its schema, so that it appears to the model with no change to the loop.
47. As a plugin author, I want my tool's internal metadata kept off the model request, so that the model only ever sees name, description, and parameters.
48. As a plugin author, I want my successful result validated against a declared output schema, so that a malformed result is caught at the boundary instead of confusing the model.
49. As a plugin author, I want one final pass to fix up my model-facing content, so that I can guarantee a bound on it even when the pipeline failed around me.
50. As a tool author, I want invalid arguments reported with a stable machine-readable code, so that the model gets a useful correction and retry logic can branch without parsing prose.
51. As a security-sensitive operator, I want every call to pass policy before it runs, so that no tool implementation has to remember to check permissions itself.
52. As a security-sensitive operator, I want a denied or unanswered approval to fail closed, so that an absent approver can never be mistaken for consent.
53. As a security-sensitive operator, I want policy that must not be reordered to be immune to reordering, so that a later-loaded plugin cannot weaken a hard guard.
54. As a plugin author, I want to set a timeout and have cooperative cancellation delivered, so that a hung call cannot occupy the turn.
55. As a plugin author, I want to hand back context that arrives only after my result, so that a composite tool can teach the model without corrupting its own answer.
56. As a plugin author, I want to end the current turn authoritatively from inside a tool, so that a terminal action does not invite another model round.
57. As a plugin author, I want to narrow which tools a given agent can reach, so that a delegated worker sees a smaller surface than its parent.
58. As a user, I want a capability set granted to one session not to leak into another, so that parallel agents cannot cross-contaminate each other.

### MCP integration

59. As a harness operator, I want to point the harness at an external MCP server from configuration, so that tools I did not write become available to the model.
60. As a harness operator, I want to connect over either a spawned local process or an HTTP endpoint, so that both styles of server are usable.
61. As a model user, I want external tool names namespaced by their server, so that two servers offering a tool called `search` never collide.
62. As a developer, I want tool names to depend only on the server and raw name, so that reconnecting never renames anything or invalidates cached prefixes.
63. As a harness operator, I want a server that publishes a changed tool list to be picked up automatically, so that I do not restart the harness after adding a tool.
64. As a harness operator, I want a server's tools replaced as a whole set rather than merged, so that a removed tool cannot linger.
65. As a harness operator, I want a flaky server retried with a bounded budget, so that a crash-looping server stops spending resources and an intermittent one recovers.
66. As a harness operator, I want the last known tool set kept during an outage, so that a transient failure does not silently strip capability from the model.
67. As a harness operator, I want a server that fails at startup to be a visible warning rather than a silent empty mount, unless I ask for strictness.
68. As a model user, I want external results presented in their original block order with text preserved, so that a server's answer arrives as it was written.
69. As a model user, I want an unsupported result kind surfaced as an explicit diagnostic, so that content never disappears without a trace.
70. As a security-sensitive operator, I want an external server's declared schema to constrain its structured output, so that a misbehaving server cannot inject arbitrary content.

### Dynamic tool and MCP plugins

71. As a plugin author, I want to ship a model-facing tool as a self-contained WASM component, so that users add capability without rebuilding the harness.
72. As a plugin author, I want my plugin's tool declared through the same contract as a native tool, so that policy and recording apply to me identically.
73. As a harness operator, I want a plugin to receive no host access unless my configuration grants it, so that a third-party tool cannot read my files.
74. As a harness operator, I want granting a plugin filesystem access to scope it to a directory I choose, so that least privilege is the default posture.
75. As a harness operator, I want a plugin that panics to be isolated from the session, so that untrusted code cannot take the harness down.
76. As a harness operator, I want to remove a plugin by editing configuration, so that uninstalling is as easy as installing.
77. As a plugin author, I want slow or long-running work to be driven by the host while my code stays synchronous, so that I do not have to model an async runtime inside my plugin.
78. As a developer, I want a reloaded plugin to leave no trace of the previous instance, so that repeated edits do not accumulate duplicate tools.

### Model providers

79. As a harness operator, I want to add a provider by registering an adapter, so that any OpenAI-compatible endpoint works without touching the loop.
80. As a plugin author, I want a replay adapter, so that tests and demos run deterministically without a network.
81. As a developer, I want streamed pieces for text, reasoning, and several concurrent tool calls correlated by index, so that interleaved answers reassemble correctly.
82. As a developer, I want assembled blocks handed to me already complete, so that no consumer re-implements the fold.
83. As a developer, I want failures normalized into one provider-neutral shape with a stable code, so that routing decisions never parse provider text.
84. As a developer, I want context overflow to have a single canonical code, so that recovery logic is written once.
85. As a developer, I want an empty completion treated as a retryable failure, so that a silent provider hiccup is never mistaken for an answer.
86. As a user, I want cached-token accounting kept separate from fresh input, so that reported usage reflects what was billed.
87. As a provider author, I want adapter-private replay state preserved alongside the message that produced it, so that a later request can reuse provider-native context.
88. As a provider author, I want that state only ever returned to the adapter that owns it, so that one provider never misreads another's opaque metadata.
89. As a harness operator, I want a stalled stream bounded by a watchdog, so that a hung provider surfaces as a timeout rather than a frozen session.
90. As a harness operator, I want a declared application identity on every provider request, so that providers can attribute traffic without me leaking anything per-user.

### Filesystem

91. As a user, I want the harness to refuse editing a file it has not read, so that it cannot overwrite work I did outside the session.
92. As a user, I want an edit that conflicts with a newer version of the file refused, so that I am never silently overwritten.
93. As a user, I want a replacement matching more than one place refused unless I ask for all of them, so that a careless edit cannot fan out.
94. As a developer, I want write and edit to be atomic and to report both the prior and the new content, so that diffs are computed from a trustworthy basis.
95. As a developer, I want a backend's file identity treated as opaque, so that moving from local disk to a remote or sandboxed world needs no consumer change.
96. As a user, I want a large read truncated with an exact line total reported, so that I know what I did not see.
97. As a security-sensitive operator, I want filesystem writes confined by the same sandbox policy that confines commands, so that a shell escape and a file escape cannot disagree about the boundary.
98. As a developer, I want no timeout promised on file operations, so that the seam never advertises a guarantee it cannot enforce.

### Execution world

99. As a user, I want commands to run through a swappable executor, so that local, sandboxed, and remote execution are provider choices rather than tool rewrites.
100. As a security-sensitive operator, I want a confined process handed exactly the command line that was about to run, so that nothing can be widened after the decision.
101. As a security-sensitive operator, I want the enforced result reported back, so that a refusal is auditable rather than silent.
102. As a harness operator, I want one place to declare the workspace root and default confinement mode, so that file and process isolation always agree.
103. As a tool author, I want a trusted per-execution environment snapshot, so that facts other plugins contribute arrive without string parsing or ambient leakage.

### Configuration, secrets, and storage

104. As a harness operator, I want settings declared as namespaced schemas with layered resolution, so that shipped defaults and my choices compose predictably.
105. As a plugin author, I want to register my own settings namespace, so that configuration I introduce is validated and discoverable.
106. As a harness operator, I want secrets referenced rather than inlined, so that a config file is safe to share or commit.
107. As a harness operator, I want a rotated credential honored by the very next request, so that rotating a key never requires a restart.
108. As a security-sensitive operator, I want credential values absent from any UI-facing view, so that display cannot leak a secret.
109. As a harness operator, I want interactive authorization flows registered per credential, so that a provider needing a browser consent dance is still config-driven.
110. As a plugin author, I want a general named key-value store, so that non-conversation state persists without inventing a private format.
111. As a plugin author, I want to build a typed store on top of that hub, so that I get durability semantics without choosing a backend.
112. As a harness operator, I want several storage backends to coexist under names, so that a database and a file store can serve different data.

### Cross-cutting behavior

113. As a developer, I want the invariant "the model saw it, therefore it is logged" checked at runtime, so that a violation is loud and immediate rather than a subtle replay bug.
114. As a plugin author, I want to scope registrations to a single agent, so that composition does not require global state.
115. As a plugin author, I want prompt sections and tool schemas assembled per step, so that my contribution appears and disappears with my plugin.
116. As a developer, I want every event's payload constrained to lossless JSON, so that nothing can be logged that cannot be replayed.
117. As a harness operator, I want swapping a provider to leave consumer code untouched, so that the plugability claim is mechanically true and not aspirational.
118. As a user, I want to resume a session in a different harness process, so that my work is not trapped inside one lifetime.



## Implementation Decisions

Decisions below are the resolved outcome of the seven map tickets; each notes its source ticket. This is a greenfield build — no existing modules are modified.

### Crate topology

- **Workspace split** (03): `dsh-runtime` (the Cordis-equivalent: context, events, effects, plugin lifecycle, fibers), `dsh-seams` (seam *trait definitions* only — the service definitions), `dsh-agent` (session log, agent loop, system-prompt assembly, core spine services), `dsh-mcp` (the MCP seam and client bridge). Concrete providers are separate crates or features: `fs-local`, `llm-openai`, `llm-replay`, `credentials-local`, `settings-file`, `storage-jsonl`, `bash-local`, `subprocess-local`, `sandbox-local`, `tool-fs`, `tool-bash`.
- **Rule**: swapping a provider means swapping a crate or a feature, never editing `dsh-runtime` or a consumer. `dsh-seams` depends on nothing but the runtime's type vocabulary, so a third-party provider can compile against interfaces alone.
- **No privileged core**: the agent loop is one registered service, not an entry point. Extensions depend on `agent/*` and `tools/*` events, never on the loop implementation crate — matching how dsh's extension packages avoid depending on the concrete loop.

### Service context runtime

- **Type-token keyed service map** (03 Q1). A service claims a stable key expressed as its Rust type; consumers obtain it via a typed accessor (`ctx.fs()`, `ctx.llm()`, `ctx.tools()`) over an internal map storing `Arc<dyn Any + Send + Sync>`. The accessor layer is the deliberate equivalent of dsh's `ctx.<key>` string lookup: consumers name the capability, never the implementation.
- **Typed accessors, not raw downcasts** (05): the raw map stays internal; every seam gets a named accessor so call sites read like the TypeScript original and downcast failures are unreachable at typed call sites.
- **`Send + Sync` throughout; `Arc<Context>` with an interior `RwLock` over the service map** (03). The map is read-heavy, so reads are concurrent and registration is exclusive.
- **Concurrency runtime is tokio, multi-thread, committed** (03). No executor abstraction — the seam layer publishes async trait methods that happen to be tokio-awaitable. Abstracting the executor was explicitly rejected as a cost with no benefit at this stage.
- **Async traits use return-position `impl Future` with explicit `Send` bounds** (03, 05). Every seam method's future is `Send + Sync`-safe so services can be called across tasks without boxing at the boundary.

### Event system

- **Five dispatch modes, all required** (02, 03): `emit` (synchronous, observed in registration order, no result), `parallel` (awaited, all listeners concurrently, all settle, failures contained), `serial` (awaited, in order, stops when a listener bails), `bail` (stops on the first synchronous bail), `waterfall` (around-middleware). The dsh primer documents four; `bail` exists in the framework source and must be implemented.
- **Waterfall contract** (03): a listener receives the event plus a `next` continuation. Calling `next()` delegates to the next listener and eventually the built-in behavior; returning without calling it **vetoed** the action. Values propagate through `next()`'s return. A listener may replace the result entirely, downstream listeners then see only the replacement. Registration order is dispatch order; a `prepend` option exists for listeners that must run first.
- **One registry per event domain** (03): session, agent, and capability domains each own a typed event vocabulary. Choosing the domain is the first design act of any feature, as it is in dsh.
- **Waterfall implementation** (03): the continuation is modeled as a chain object passed by mutable borrow, avoiding the lifetime tangle of nesting async closures. Veto is expressed by returning without invoking the continuation, not by an error type — an error means "failed," a veto means "I own this decision."
- **Failure containment** (03, 04): a listener failure during a fire-and-forget or fan-out dispatch is logged and isolated; it never changes a committed outcome or blocks sibling listeners.

### Effects, disposers, plugin lifecycle

- **RAII disposer guards** (03): `effect` and `on` return a guard whose drop unwinds the registration. Registrations that must unwind together belong in one effect so ordering is explicit.
- **Fiber-owned, LIFO unwind; creation refused while unloading** (03). A fiber in `Pending` or `Loading` may register; `Unloading` may not. This is a real bug class dsh had to harden against, and it is specified up front rather than discovered.
- **One fiber per mounted plugin** (03): unloading a plugin unwinds exactly its own tools, listeners, schemas, and prompt sections. This is the mechanical guarantee behind "everything is a plugin."
- **`trait Plugin { inject; apply }`** (03): `inject` declares required services as a static list; the loader activates a plugin only when all are present, so load order is expressed as service requirements and never as a hand-maintained boot sequence.
- **Scoped contexts** (03, 05): contexts can be extended, isolated (shadow one service within a scope; sharing a label joins the scopes), and intercept-configured. Isolation is the mechanism behind per-agent capability sets and agent presets.

### Session event log

- **Full-fidelity core vocabulary** (04): turn open/close, step open/close, user message, assistant streamed chunk, assistant message, tool call, tool result, plus the log-only records for whole-list snapshots, request envelope, route context, and seed boundary. Nothing trimmed — the derived-history and replay guarantees depend on the whole set.
- **Turn-end reasons** (04): completed, aborted (with typed cause), blocked, error (structured, never a bare string), max-tokens, interrupted. `max-tokens` wins over a later clean stop, and `interrupted` is synthesized only by crash recovery — the loop never emits it.
- **Position and time on every record** (04): position is the log length (contiguity contract), time is epoch milliseconds. Both writer-assigned, never caller-supplied.
- **Serde closed enum, validated at the append site** (04): payloads must be lossless-JSON; an append carrying something unstorable is rejected before the log changes, so the log can never contain an event a backend cannot reproduce. Committed events are immutable; readers get snapshots.
- **The append path never blocks on I/O** (04): durability is asynchronous; a producer needing a durability barrier requests one explicitly.

### Derived history and the surface

- **Message history is derived, never stored** (04). A projection walks the recorded surface and yields the message list the model sees. Replaying, forking, and persisting all read the same stream.
- **The surface is the sole source of derived history** (04): only the three message-producing event kinds may declare how they join it. Structural records (boundaries, chunks, usage) never project a message. Raw streamed chunks are replay and presentation data, deliberately excluded from derivation — the assembled message is authoritative.
- **Two surface operations** (04): append to the tail, or replace an inclusive range of surface nodes. A replacement must cite every node it retires, so nothing leaves the model's view unaccounted for. Replacement is how a later summarizing pass retires stale turns without rewriting history.
- **Projection is cached per surface node and rebuilt when a replacement lands** (04): deriving costs new nodes, not the whole log. Callers get a fresh array over shared frozen messages, so a held projection cannot be mutated by later appends.
- **A content-less assistant record is skipped in derivation but kept in the log** (04): a truncated step still records usage, provider, and model, while an empty assistant turn never enters a provider transcript.

### Plugin-extensible event vocabulary

- **Core enum plus a plugin extension registry** (04). This is the deliberate answer to the hardest Rust-versus-TypeScript gap: dsh extends its event map by declaration merging, which Rust has no equivalent for. Core events stay a closed typed enum; plugins contribute **log-only** event types registered by name, each supplying its fold, validation, and whether it produces model-visible content.
- **Extension events never enter derived history** (04): a plugin record that reached the model would have to be reconstructable, which would require the core to know how to project it. Keeping extension log-only preserves the invariant without freezing the vocabulary.
- **Unknown records either ignored or fatal, decided by the writer** (04): a record marked ignorable may be skipped by a reader that does not recognize it; an unmarked one must refuse reconstruction. Defaulting to required means a forgotten marker over-refuses rather than silently resuming from a gutted history.
- **No exhaustive-match assertions over event kinds** (04): matching must fall through for unrecognized types, since the set grows after compilation — the direct Rust analogue of dsh's rule against `assertNever` on a merge-extensible map.

### Agent loop

- **Full-fidelity turn/step flow** (04): a turn opens before input is claimed; each step is one model request plus the tools it requested. Between them the loop dispatches the pre-step decision, prompt assembly, request construction, streaming, tool scheduling, and the turn-stopping checkpoint.
- **The pre-step decision is authoritative** (04): a listener may rewrite what is shown or refuse it outright. A refused or emptied first claim still closes a durable turn that spent no step — the attempt is recorded even though nothing ran.
- **Input arrives through one inbox** (04): some messages wake the driver immediately, others wait until something else does. Steering and injected context pass through the same pre-step waterfall as ordinary prompts, so there is one admission path.
- **Extension points and their modes** (04): request interception, model streaming, and the three tool stages are waterfalls; the turn-stopping checkpoint is serial with no continuation. Waterfall listeners must delegate to reach later ones.
- **Tool scheduling honors an explicit per-call mode** (04): parallel calls overlap; exclusive calls form ordering barriers. Classification is re-checked before each start.
- **Model-visible means logged, asserted at runtime** (04): a runtime invariant checks that anything reaching a request can be reconstructed from the log. A new model-visible input requires a new event type — never an unlogged argument.

### Tool registry and schema contract

- **Model-facing schema separated from execution contract** (07): a registered tool carries its public schema (name, description, parameters) plus a canonical output declaration, the execution function, and optional finalization, timeout, concurrency-safety classification, and presentation hooks. The registry builds the request-time schema list through an **allowlist** — output, execute, finalize, timeout, concurrency, and presenters can never reach the model.
- **Schema DSL** (07): a unified value schema — string, number, integer, boolean, null, array, object, raw JSON, and exact-one-of — with explicit object openness, scalar enums and constants, and per-property requiredness. It compiles to JSON Schema for the wire and validates both arguments and canonical output at runtime. Rust infers argument types exactly; dsh's depth-limited inference fallback is a TypeScript artifact and is not reproduced.
- **Two distinct failure kinds** (07): bad arguments versus a bad result, each with a stable machine-readable code, so retry and policy layers branch without parsing messages.

### Guarded execution pipeline

- **The locked order** (07): pre-execute waterfall (reorderable allow/deny/ask) → registered monotonic guards (deny or abstain; identity protected, cannot be reordered around) → execute waterfall (around-dispatch: timeout, retry, metrics) → the tool body → post-execute waterfall (accept, block, replace, add context) → definition-owned finalization → the frozen result notification.
- **Stage-to-mode mapping** (07): the three policy stages and the approval request are waterfalls using the veto contract; the final outcome notification is fire-and-forget. Only the around-dispatch stage may replace the caller's cancellation signal.

- **Approval is a seam, and absence means refusal** (07): a one-shot permission decision dispatched as a waterfall; if nobody answers, or the answer seam is unmounted, the call is denied. Failing closed is the contract, not a default.
- **Result normalization before finalization** (07): the registry snapshots the candidate result losslessly and converts a thrown pipeline failure into an error outcome *before* the definition's finalization pass runs, so finalization sees every outcome including ones that bypassed post-execute. Finalization must be total and may only replace content; error state, canonical value, and metadata remain registry-owned.
- **The final outcome is immutable** (07): the authoritative notification carries the frozen, lossless-JSON outcome, so observers cannot disagree with what was recorded.
- **Two deferred-context channels** (07): a tool may attach context that lands only after its own recorded result — used by composite tools to ferry nested-dispatch findings and by leaf tools to mint plugin-sourced instructions — and may mark a successful result as ending the turn, which only an authoritative success may propagate.
- **Scoped visibility with restrictions** (07): a scope filters the tools it inherits with an allow-list or a deny-list; restrictions from ancestors intersect; a scope's *own* registrations are exempt, so a delegated child always keeps what it was created to answer. Deny-only admits later unlisted inherited tools; an allow-list does not.
- **Presentation hooks defined but unexercised** (07): pending and completed presentation projections are part of the tool contract because they are pure functions of arguments and result and must survive replay, but no consumer ships in this spec — the web client is out of scope.

### MCP client bridge

- **Client direction only** (07): the harness consumes external MCP servers and registers their tools; it does not expose itself as a server.
- **Protocol surface** (07): the MCP specification as implemented by the TypeScript SDK 1.12 generation — JSON-RPC 2.0 framing over **stdio** (spawned child process) and **streamable HTTP** (URL plus headers). Backed by a maintained Rust MCP client rather than a from-scratch protocol implementation.
- **Tools are the only bridged capability** (07): resources and prompts have no harness consumer and are deferred, matching dsh's own limitation.
- **Namespaced, deterministic naming** (07): every external tool is registered as `mcp__<server>__<raw>` and normalized to the provider function-name contract (length and character limits), with a deterministic hash of the server and raw name appended when normalization would collide. Names are pure functions of that pair — connection order, re-syncs, and other servers never rename anything, which keeps prompt prefixes cache-stable.
- **The public name never goes on the wire** (07): invocation sends the server's raw name; the namespaced form is harness-local.
- **Generation replacement, never merge** (07): discovery lists tools and registers them before the first turn; a list-changed notification re-syncs by replacing the whole generation. A registration conflict rolls back the attempted generation entirely rather than leaving a partial set, and a duplicate server name fails the later plugin at load.
- **Outage and recovery semantics** (07): on transport loss the supervisor restarts the same config with exponential backoff, doubling to a ceiling; surviving past the ceiling resets the budget, and exhausting the attempt limit unregisters the tools and stops. Through an outage the last good generation stays registered but failing, so capability is not silently stripped. Reconnect state changes are logged at distinct severities so an operator can tell recovering from finally-failed.
- **Config knobs** (07): per-server transport fields, environment for spawned servers, a per-call timeout, a strictness flag controlling whether startup failure rejects activation, and reconnect toggles.
- **Result projection** (07): canonical success keeps the complete ordered content blocks plus optional structured content; supported output schemas validate structured content while vocabulary outside the supported subset falls back to unconstrained JSON. Text-like runs join, resource links keep name and URI as text, and unsupported kinds become explicit diagnostics rather than vanishing.
- **Rich content is gated on proof** (07): image blocks become durable content only when an attachment store is mounted *and* the exact calling route declares image input; the whole batch is validated before any member is admitted. Audio and embedded resources stay out of model context. Base64 from a server never gets copied into a session record.

### Dynamic plugin surface

- **Hybrid split, deliberately narrow** (06): core seams are compile-time native crates; the dynamic surface covers **model-facing tool and MCP plugins only**. Provider and loop replacement stays a rebuild — declared now so nobody is surprised later.
- **Runtime is wasmtime on the component model with WASI preview 2** (06): chosen for a typed interface boundary and a capability system, over classic core-WASM with manual marshalling.
- **Plugin ABI** (06): a component exports a descriptor declaring its name and its tools, each conforming to the same tool contract as a native tool — schema, output declaration, execution entry. The host registers them onto the tool registry, so policy, recording, approval, and sandboxing apply to a guest tool identically to a native one.
- **Async boundary** (06): wasm functions are synchronous, so long-running work is driven by the host, which polls a structured result or handle. A plugin author never models an async runtime inside the guest.
- **A plugin is a boot entry** (06): configuration rows name plugin id, component path, and config. The loader mounts each into its **own fiber**, so unloading unwinds exactly its registrations, and reload replaces the generation rather than accumulating duplicates.
- **Transactional reload** (06): a plugin or config change is validated against the live tree, and a failure leaves the last good tree mounted — the same safety dsh had to engineer for its hot-reload path.




### Core seam contracts

Each seam is a trait in `dsh-seams`; a provider is an implementation registered onto the context; a consumer is the party that calls it. The three-role discipline is what makes a seam a seam — a lone interface with no provider and no consumer is not plugability.

**Filesystem** — ported faithfully, because its swap-safety contract is the reason providers are interchangeable (05).
- Targets are opaque: a path resolves to a stable identity with a display form, and consumers must never parse the identity key or assume it is a local path. Cross-capability coordinates (a path a subprocess can open, a file URI, containment tests) come from the provider, not from string manipulation.
- Freshness is a backend-owned token. Write and edit take an *optional* guard: create-if-absent, or replace-only-at-version. Omitting the guard means unconditional, which is what a bare provider offers; the guarded behavior is layered by policy, not baked into the seam.
- Edit is one atomic mutation that verifies the version before matching, applies literal replacement, and writes atomically — not a read plus write composed elsewhere, which would reintroduce the race the guard exists to close.
- A machine-routable error taxonomy (not-found, not-a-directory, not-text, not-a-regular-file, too-large, permission-denied, sandbox-denied, io-error, stale-version, not-observed, ambiguous-edit, edit-not-found, aborted). Sandbox refusal is distinct from kernel refusal so a caller can tell policy from operating system.
- Three policy events form the seam's extension gate: two single-slot decision waterfalls for write and edit intent, and one fire-and-forget observation record. A policy plugin registers *no service* — it decides those waterfalls and keeps its own state, so removing it leaves the tool working against the bare provider. The emitter passes an opaque actor rather than an agent or session type, so the filesystem crate never imports them.
- **No timeouts on file operations** (05): a deadline the seam cannot enforce is worse than none, since an in-flight sync or rename cannot be stopped. Cancellation still propagates best-effort.
- The consumer is the file tool: read, write, edit, list — with windowed reads that keep an exact line total even after the byte cap is reached.

**Model adapter** — ported faithfully for the same reason (05).
- Content blocks: text, reasoning, image, tool call, tool result. A new modality is admitted only when adapter, recording, and projection paths all honor it.
- Messages are identified and immutable; an assistant message names the provider and model that produced it and may carry adapter-private replay state. Where a message came from is a separate axis from what kind of information it is, and the two are deliberately independent.
- Stream protocol: block start, text delta, reasoning delta, tool-call delta, block end carrying the assembled block, usage, finish. Indexes correlate interleaved blocks.
- **Adapter obligations, restated as the provider conformance contract** (05): usage before finish and nothing after; tool arguments stay raw JSON strings end to end; exactly two sanctioned failure paths (throw from the stream entry, or an in-band terminal error) normalizing to one provider-neutral failure shape; one adapter call is one provider attempt, with library-internal retries disabled; stalls bounded by a transport watchdog; context overflow classified to one canonical code; an empty completion is a retryable failure, not a success; a declared identity header on every request.
- **Replay state is adapter-owned but its shape is shared**: response-level metadata plus per-block entries aligned to emitted blocks, pruned in lockstep with dropped blocks so stored metadata always describes stored content. On a later request it is returned only to the same adapter instance that registered both the historical and the target provider; other adapters receive provider-neutral content.
- A single shared assembler folds the stream back into blocks and a final message, with one keep-or-drop decision covering content and metadata together — a truncated finish drops tool calls because a partial call is unsafe to execute.
- Token accounting is **disjoint**: uncached input, cached reads, and cached writes sum to billed input; reasoning tokens are informational detail already inside output and must never be added again. Providers that fold cache hits into one total subtract them back out.
- Consumers: the agent loop and any summarizing pass. There is deliberately no model-facing tool for calling a model.
- Providers in scope: an OpenAI-compatible adapter (covering DeepSeek, vLLM, and any endpoint speaking that interface) and a replay adapter for deterministic tests and demos.

**Settings** (05): namespaces of declared schemas with layered resolution — shipped composition base beneath the user layer — and a file provider storing the raw document. A provider swap changes storage, never the resolution order. Providers must return redacted, value-free descriptors for anything display-facing.

**Credentials** (05): configuration holds *references*; providers own the values; consumers resolve **per operation**, so a rotated secret reaches the very next request with no restart. An authorization-flow seam registers the interactive dance per credential kind, keyed by the record it writes, with one attempt per key — the flow seam owns lifecycle, never protocol.

**Storage** (05): a named key-value hub for non-conversation state, backends registered side by side under names, with a typed domain layer mounted on top translating operations into opaque units. Session persistence deliberately uses its own seam rather than this hub.

**Execution world** (05): a subprocess seam owning spawn coordinates and cancellation; a shell seam of executors consumed by the command tool; a sandbox seam that receives the *exact* argv about to spawn and reports what it enforced; one policy home for default confinement mode and workspace root, read by both the command executor and the filesystem provider so the two can never confine to different roots. Shared execution world is the point: retargeting filesystem and subprocess at a remote sandbox moves the command, terminal, and language-server providers with it, with no provider forks. These are specified at definition-plus-provider-plus-consumer depth rather than full contract prose (05), since their interesting invariants are the cross-provider ones already stated.

**Approval** (07) and **sessions** (04) complete the core set; the seams they belong to are covered above.

### Durability, fork, and resume

- **The store is in-memory; durability is a separate seam** (04). Persistence plugins subscribe to the event feed and flush on checkpoint; the store owns neither encoding nor I/O.

- **Every event persists losslessly, streamed chunks included** (04): contiguity forbids filtering records out of the canonical log. A backend may choose its own encoding for a batch provided loading returns the exact appended events.
  ```rust
  // Session-persistence contract (ticket 04):
  trait SessionPersistence {
      fn save(&mut self, session: &SessionId, batch: &[SessionEvent]) -> BoxedFuture<'_, Result<()>>;
      fn load(&mut self, session: &SessionId) -> BoxedFuture<'_, Result<Option<LoadedLog>>>;
  }
  // Invariant under test: load(save(events)) == events, byte-for-byte after
  // JSON round-trip — for any backend, including the chunk-coalescing one.
  ```
- **Backend choice is composition** (04, 05): a flat-file backend ships first; a database backend is deferred. Both persist the same vocabulary, so switching does not invalidate stored sessions.
- **Fork takes an inclusive boundary that must end between turns** (04): a prefix ending inside an open turn is refused rather than silently clipped. Child metadata records parentage, seed length, and inherited working directory.
  ```rust
  enum TurnEndReason { Completed, Aborted { cause }, Blocked, Error { failure }, MaxTokens, Interrupted }
  // Crash recovery is the only producer of Interrupted; the loop never emits it.
  ```
- **Seed and live work are distinguished in the log** (04): a marker records where the current process's writes begin, so a bracket left open by a crash is distinguishable from one being written now. Reopening an untouched session must not grow its log.
- **Session and loop tear down as one ordered chain** (04, 03): the session lifecycle folds into the owning plugin's effect so that unloading commits the loop's final events *before* the store stops publishing. Racing sibling effects would drop them.

### Configuration and boot

- **Layered composition, applied to an empty entry list** (06): each bundle in the profile's order, then the profile's user patch, then the home-level patch, then any per-run overlay. Higher layers outrank lower ones, which is what makes "override without forking" true.
  ```yaml
  # Boot composition, in resolution order (lowest precedence first):
  #   bundle layers (profile order) -> profile patch -> home patch -> --patch overlays
  # A patch targets an entry by id and REPLACES its whole config (no deep merge),
  # or inserts new rows. A patch naming an absent id is a warning, not an error.
  ```
- **Patch semantics: whole-config replacement, not deep merge** (06): an override restates the fields it keeps. Chosen to match dsh — deep merge across layers produces compositions nobody can read.
- **Config is YAML; expressions are declared, not ambient** (06): where dsh interpolates JavaScript expressions, the Rust clone needs a deliberately small, non-Turing-complete substitution surface (environment and home paths). This is the one place the port cannot be faithful, and the escape hatch's shape is a first-class design decision rather than an accident.
- **Dump equals mount** (06): composing base and overlays offline must yield exactly what boot mounts, using the same parser and patch algorithm, so a dumped config is reloadable and diffable.

### Cross-cutting contracts

- **Branded opaque identifiers** (04, 05, 07): session ids, message ids, call ids, target keys, and version tokens are distinct newtypes. Several dsh contracts depend on identities being unparseable and non-substitutable, and Rust's newtypes make that checkable.
- **Stable machine-readable error codes everywhere** (05, 07): filesystem, tool, and provider failures each carry a code consumers route on. Branching on message text is prohibited — messages are for humans.
- **Lossless JSON at every boundary** (04, 07): tool arguments, canonical results, event payloads, and replay state all live inside one lossless-JSON discipline, validated at the boundary rather than at flush time.
- **Runtime invariants are a service** (04): package-owned checks register against a central invariant registry, so "model-visible means logged" is enforced by the runtime and reported with attribution, not left as a comment.

## Testing Decisions

**What makes a good test here:** assert externally observable behavior at a seam — the events a turn produces, the messages derived, the decisions a policy changes, the contract a provider must honor. Never assert internal structure: not map layouts, not lock usage, not which closure ran. The spec's guarantees are behavioral (replay fidelity, veto semantics, fail-closed approval, swap safety), so those are what tests must pin.

**Primary seam — the agent loop, observed through the session log.** This is the single highest seam in the system and it is nearly sufficient on its own: drive a turn with the replay adapter over a recorded model stream and assert the emitted event sequence and the derived message list. Because model-visible means logged, the log *is* the observable behavior — so this one seam exercises the event system and dispatch modes, effects and fibers, tool registration and the full guarded pipeline, approval, prompt assembly, and persistence, with no network, no GPU, and no provider credentials.

**Plugability seam — one parameterized provider conformance kit.** A single reusable contract suite expressed against the seam traits, run unchanged against every implementation of every seam: filesystem providers must honor version guards, atomicity, opaque identities, and the error taxonomy; adapters must honor the streaming order, raw-arguments, two-failure-path, disjoint-usage, and replay-ownership rules; executors must hand over exactly the argv and report enforcement; settings and storage providers must preserve layering and named coexistence. This one kit is the test that the feature works, because "swap a provider without touching consumers" is the feature. Adding a provider means passing the kit; that is also how the guarantee stays true after the first release.

**Dynamic plugins and MCP get no new seams** — both are tested by registering through the tool registry and asserting at the primary seam: a fixture component plugin and a fake in-memory MCP server, exercising discovery, namespaced naming, generation replacement on list-changed, rollback on conflict, outage/backoff/stop-after-budget, and capability-denied access. Deterministic naming and cache stability are asserted by property: the public name is a pure function of server and raw name.

**Durability and replay**: a backend must round-trip the exact log including streamed chunks; fork boundaries inside an open turn must be refused; crash recovery must close an orphaned turn as interrupted without touching earlier records; reopening an untouched session must not grow its log. Golden files of recorded sessions serve as the replay corpus.

**Prior art**: dsh's own suite is the reference for shape and rigor — its behavior tests run against the real loop with a replay adapter rather than mocking internals, its contract catalogs are generated and verified fresh rather than hand-maintained (the same discipline applies to the seam and event tables in this spec), and its snapshot tests pin replayed transcripts. The generated-vs-hand-written rule matters most here: a spec that drifts from its own interface tables is worse than no spec.

**Explicitly not tested**: numerical parity with any model implementation (there is no model compute in this effort), and pixel-level presentation, whose only consumer is out of scope.

## Out of Scope

Ruled out by the map's destination; these return only if the destination is redrawn, as a fresh effort.

- **Web UI and browser client** — the product surface, client module composition, hot module replacement, and conversation-node rendering. The harness engine and its seams are the destination; presentation hooks exist in the tool contract but have no consumer here.
- **The browser-facing HTTP server and API gateway** — host API dispatch, remote descriptor gateways, and the runtime type registry that generates them. A headless or CLI harness has no browser to serve.
- **Agent Teams** — the experimental multi-agent coordination domain (roster, task board, mailbox) is private opt-in upstream and behind the first release.
- **Subagent bridges to other products** — adapters that delegate turns to external agent CLIs. The subagent seam may ship an in-process provider or be omitted; porting third-product bridges is a different effort.
- **JavaScript hot-loading machinery** — the dynamic in-process plugin runner and its inspect bridge, superseded by the WASM tool/MCP surface.
- **Deferred seams** — summarizing/compaction, skills, background jobs, persistent terminals, language-server navigation, code-execution mode, web search and fetch, attachments, spilling, directory picking, session telemetry and querying, plan mode, goals, human commands, workflow engine, workspace and message feedback. Their *interfaces* are inventoried and triaged; none is specified to implementation depth. Compaction is called out especially: the surface replacement it needs is designed and built, but the producer is not.
- **MCP server direction, resources, and prompts** — client-side tools only.
- **Non-tool dynamic plugins** — the runtime surface is tools and MCP only.
- **Model weights, training, and GPU compute** — the harness orchestrates a model endpoint; it is not an inference engine.
- **Windows and non-Unix shell specifics** beyond what the executor seam's provider shape already accommodates.

## Further Notes

- **Where the port cannot be faithful.** Four gaps are deliberate, not oversights: (1) plugin-extensible events — declaration merging has no Rust equivalent, hence core enum plus extension registry; (2) config expressions — a small substitution surface replaces arbitrary code in config; (3) hot reload — Rust cannot hot-load crates, hence the narrow WASM surface and rebuild-instead for providers; (4) presentation — hooks exist without a consumer. Each is recorded so a future reader sees a decision rather than a hole.
- **Two dispatch modes beyond the documentation.** The harness's own primer lists four event modes; the framework has five. The spec locks all five — a gap found while reading framework source rather than docs.
- **MCP was never a seam upstream.** Externally bridged tools ride on the tool registry, so a first-class seam had to be created for them (ticket 01). This is the one place the clone's architecture is deliberately *better* differentiated than the original's.
- **Reference material.** Two research documents back the interface tables and should be read alongside the spec: `docs/research/seam-inventory.md` (every seam with its triage) and `docs/research/cordis-semantics.md` (the semantics the runtime must match, including dispatch-mode mechanics).
- **Upstream is in developer preview with breaking changes** — the reference implementation may move under this spec. Re-clone at execution time rather than trusting cached paths.
- **Sizing.** The runtime, the event-sourced log, the loop, and the guarded pipeline are the load-bearing core; every one of them is specified to behavioral depth. The WASM surface is the largest single risk in the build, and the first implementation milestone should be a tool plugin crossing that boundary end to end rather than breadth across seams.
- **The CLI / execution surface is deliberately unspecified.** The config-boot model is decided (layered profiles, bundles, id-targeted patches, dump-equals-mount), and the browser-facing surface is out of scope — but the exact verb set the binary exposes (`--profile <name>`, `--dump-config`, a one-shot headless runner, an interactive entry point) and the composition shipped as its default profile are left to the planning effort. They are presentation of decisions already made, not new ones.


