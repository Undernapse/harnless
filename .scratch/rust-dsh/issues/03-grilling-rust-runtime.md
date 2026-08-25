# 03 — Design the Rust service-context + typed-event runtime

Type: grilling
Status: open

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

(blank until resolved)
