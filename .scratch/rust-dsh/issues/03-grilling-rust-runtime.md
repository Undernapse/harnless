# 03 — Design the Rust service-context + typed-event runtime

Type: grilling
Status: resolved

## Question

How does the Rust "Cordis-equivalent" runtime map the settled foundation? This is the deepest joint of the whole clone — everything else (seams, session log, agent loop, MCP bridge) hangs off it.

Design the Rust runtime for:

- The **context** as a repository of services claimed by stable keys (`ctx.<key>`) — how services are registered, looked up, and named in Rust (trait objects? an any-map? typed service registry?).
- The **typed-event** system with the four dispatch modes — `emit`, `waterfall`, `parallel`, `serial` — including the `waterfall` around-middleware `next()` delegation contract. Which Rust abstraction carries these (trait + event enum? `tracing`-style? `async_trait`? tokio broadcast/oneshot?).
- **Reversible effects / disposers** — `ctx.effect()` / `ctx.on()` registrations that unwind predictably on teardown, matching Cordis's disposal ordering.
- **Plugin lifecycle** — plugins as Services mounted into a context, with `inject`-declared service dependencies ordering load.
- **Concurrency spine** — async runtime (tokio?), Send/Sync guarantees across the service layer, and how dynamic registration plays with Rust's static types.

Consume `02-cordis-semantics` research first. Decide which parts the spec locks and which stay open.

## Answer

Resolved by grilling (2 rounds) with the dev. The Rust "Cordis-equivalent" runtime design the spec must document:

**Round 1 — core runtime (all decisions locked).**

- **Q1 — Service-context representation:** a type-token keyed map. Services are stored as `Arc<dyn Any + Send + Sync>` behind the context; `ctx.get::<S>()` / `ctx.provide::<S>(...)` downcast once at the typed boundary. The Rust type *is* the stable key spelling of `ctx.<key>`. Consumers reference the seam trait (e.g. `Tools`), never a concrete impl → "by key, not by import" holds as a static fact. (Dynamic registration is preserved because the map is keyed by `TypeId` at runtime.)
- **Q2 — Typed-event system:** one `EventRegistry` per event domain with typed payloads. **Five** dispatch modes must be supported (research 02: `emit`, `parallel`, `serial`, `bail`, `waterfall` — the Cordis README table omits `bail`). `emit` = synchronous fire-and-forget in registration order; `parallel` = await all listeners concurrently (Promise.allSettled semantics → all settle, failures contained); `serial` = await in order until one bails; `bail` = stop on first synchronous bail. **Waterfall** is the crux: each listener is `async fn(&mut EventCtx, args, next: Next)`, where `Next` is a chain-callable continuing to the next listener; not calling `next()` vetoes (short-circuits), mirroring Cordis's `next()` delegation contract. `prepend` supported (listener runs before ordinary registrations).
- **Q3 — Reversible effects / disposers:** `ctx.effect()` / `ctx.on()` return RAII `Disposer` guards (drop disposes). Each fiber owns an ordered registry of live effects; unload disposes them **LIFO**. Preserve the Cordis invariant: reject new effect creation while a fiber is `UNLOADING` (legal while `PENDING`/`LOADING`).
- **Q4 — Plugin lifecycle + inject:** `trait Plugin { fn inject() -> &'static [ServiceId]; fn apply(&mut self, ctx: &mut Context) -> Result<()>; }`. The registry stalls a plugin's activation until every injected `ServiceId` is `provided` (service-requirement load order, not manual boot sequencing). The inject list is static, keeping load-ordering checkable. Macro sugar optional later.
- **Q5 — Concurrency spine:** tokio multi-thread runtime throughout; `Context: Send + Sync`; the service map behind an `RwLock`; event dispatch is async (`parallel`/`serial`/`waterfall` fully `await`). Everything the agent loop and seams touch is `Send + Sync`.

**Round 2 — layout + seam boundary (all decisions locked).**

- **Q6 — Crate/workspace layout:** a Cargo workspace: `dsh-runtime` (Cordis-equivalent: Context, events, effects, plugins, fiber), `dsh-seams` (the seam *interfaces* — the trait definitions that back `ctx.get::<Tools>()` etc.), `dsh-agent` (session log + agent loop), `dsh-mcp` (the first-class MCP seam). Concrete providers live in their own crates. Swapping a provider = swapping a feature/crate, not editing core.
- **Q7 — Seam boundary typing:** thin typed accessors over the raw typemap — `ctx.tools()`, `ctx.llm()`, etc. — so seams read by name (`ctx.tools()`) while staying static. The raw `TypeId` typemap stays internal; accessors are hand-written or generated per seam.
- **Q8 — Effect/teardown ownership:** **one fiber per mounted plugin**. Each plugin's `apply()` runs in a fiber that owns its effects; unloading a plugin unwinds exactly its registrations. This is what makes config-driven plugin swap reversible (unload plugin A → its tools/listeners/schemas disappear), upholding "everything is a plugin."
- **Q9 — Async runtime:** commit to tokio multi-thread in the spec; do not abstract the executor. The seam layer publishes async trait methods that happen to be tokio-awaitable.
- **Q10 — `ctx.mcp` seam:** pin the seam's **existence + crate** (`dsh-mcp`, defining `ctx.mcp` with a client transport seam) in the runtime/architecture section now so downstream crates depend on it existing. Its interface/protocol depth (spec versions, stdio/HTTP, JSON-RPC framing, tool-schema mapping) is deferred to ticket 07.

**Spec locks:** Q1–Q10 as above. **Stays open / deferred:** the crate *contents* and per-seam method signatures (05), the agent-loop + session-log internals (04), the MCP protocol surface (07).

Sources consulted: research 02 (`docs/research/cordis-semantics.md`), vendored Cordis source in the dsh repo (`vendor/cordis/src/events.ts` for the five dispatch modes, `service.ts`, `context.ts`, `fiber.ts`).
