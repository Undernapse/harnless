# Cordis Semantics — reference for the Rust runtime

Source of truth: `deepseek-ai/deepseek-harness` vendored Cordis (vendor committed at `vendor/cordis/`, upstream `cordiverse/cordis` `packages/core`, version `4.0.0-rc.7`), plus `docs/cordis-primer.md`. Read this doc next to the Rust traits/async code when designing the "Cordis-equivalent" runtime.

## Plugin model

- A **plugin** is an object that implements Service. It can be a plain function with optional `inject` and `apply(ctx)` fields, or a `Service` subclass whose lifecycle Cordis mounts into the current context.
- **Two plugin forms:**
  - **Function/object plugin**: `{ inject?: string[], apply(ctx) { ... } }`. `apply` is invoked with the context when the plugin activates.
  - **Class plugin**: a `Service` subclass. `super(ctx, name)` registers the instance immediately; `static init` is a method run after construction; `[Service.invoke]` makes the instance callable (e.g. `ctx.logger()`); `[Service.check]` is an availability predicate passed to `ctx.provide()`.
- Plugins are mounted by the registry; registrations attach to the **fiber** that mounted them and unwind when it unloads.

## Context as a repository of services

- A **context** is a repository of services. A service claims a stable `ctx.<key>` (e.g. `ctx.tools`, `ctx.llm`, `ctx.sessions`); other plugins find services **by key**, not by importing a concrete implementation.
- `Service` registers itself via `ctx.reflect.provide(name, instance, check)`. The service is unregistered automatically when its owning fiber unloads.
- Scoped child contexts: `extend()` (copies path, shares), `isolate(name, label?)` (shadows a service in a scope; same label joins scopes), `intercept()` (merges ancestor config).
- `[symbols.resolveConfig]` merges intercept config: entries closer to root apply first; uses `Config.merge` if declared, else shallow `Object.assign`.
- Context has hooks: `effect` (symbol), `filter` (listener filter consulted on every dispatch), `isolate` (isolation map).

## Service dependency via `inject`

- A plugin names required services via `inject`. The **registry** (`ctx.plugin` / `ctx.inject`) waits until those services exist before activating the plugin, delaying load until its service dependencies are present.
- Load order is therefore expressed through **service requirements**, not manual boot sequencing.

## Dispatch modes

The source defines **five** dispatch modes (`DispatchMode = 'emit' | 'parallel' | 'serial' | 'bail' | 'waterfall'`). The primer's table omits `bail`; the code has it, and `serial`/`bail` are closely related.

| Mode | Awaited? | Dispatch order | Returns? | Notes |
| --- | --- | --- | --- | --- |
| `emit` | No | listeners in registration order | No (`void`) | Fire-and-forget observation. `dispatch('emit').map(cb => cb(...))`. |
| `parallel` | Yes | all listeners in parallel | No (`Promise<void>`) | `Promise.allSettled(dispatch('emit', args).map(async cb => cb(...args)))`. |
| `serial` | Yes | in registration order until one bails | `Promisify<ReturnType>` | Iterates listeners in order; stops when a listener bails. |
| `bail` | Yes | stop on first synchronous bail value | — | Stops on the first synchronous bail value. |
| `waterfall` | No | compose around a final `next` callback | `ReturnType<Events[K]>` | Around-middleware. Last dispatch arg is the innermost `next`. Listeners run outermost-first; calling `next()` delegates to the next listener (finally the built-in behavior), not calling it vetoes. |

**Emit semantics (`EventsService`):** `emit` runs listeners synchronously without awaiting; `parallel` awaits all together; `serial` awaits in order until one bails; `bail` stops on first synchronous bail; `waterfall` composes around `next`.

**Waterfall `next()` contract:** a listener receives `(...args, next)`. Call `next()` to delegate the possibly-wrapped result to the next listener; return without `next()` to short-circuit (veto). Values propagate through `next()`'s return value. `prepend: true` runs a listener before ordinary registrations. A policy listener can short-circuit and own the decision; an annotating/observing listener must delegate.

## Reversible effects / disposers

- Registrations are reversible effects. Services, listeners, prompts, tool schemas are installed through `ctx.effect()` / `ctx.on()` and unwind when the owning fiber unloads.
- **Fiber lifecycle** (`fiber.ts`): the fiber owns the collected cleanup/effect registrations. `UNLOADING` state rejects new effect creation (prevents cleanup-time registrations escaping the unload snapshot); `PENDING`/`LOADING` remain legal.
- dsh's vendored hardening closes reentrant disposal gaps: an effect's owner-list wrapper registers before its setup body runs; synchronous setup failure rolls back collected cleanup; async cleanup stays owner-visible until quiescence; repeated disposer calls retain single-shot result.
- Every registration should have a disposer; keep related work in one effect so disposal unwinds in intended sequence.

## Loader / profiles / bundles / patching

- **Profiles** (named composition) list the bundles they stack and hold any out-of-tree plugins plus the user's own `cordis.patch.yml`. `web` and `headless` ship as templates.
- **Bundles** are a distribution format for Cordis config rows + the code they mount; whatever a bundle inserts stays patchable by layers above it.
- Layers apply to an empty entry list in order: each bundle in the profile's listed order → profile `cordis.patch.yml` → home-level one → any `--patch` overlay. A patch targets a row by id and replaces its whole config, or inserts new rows.
- A `dsh` field in each `package.json` declares the role: `dsh.profile` lists a profile's bundles; `dsh.bundle` points at a bundle's patch file.
- `dsh --profile web --dump-config` prints the resulting config tree; any printed row can be replaced by a patch.
- `@deepseek-ai/cordis-plugin-include` parses `!!js` into expression nodes; interpolate `config` (after declared injections activate) and `disabled` (at every mount decision). Other entry metadata stays literal. Overlays select plugins by environment.
- Vendored loader hardening: **transactional reconciliation** — a Loader importing a changed entry name disposes the old before applying; settles service-gated fibers; restores the previous plugin/config on candidate failure; Group updates start candidates concurrently and contain sibling-start failures.

## Key invariants to preserve in Rust

1. **Service-by-key, not by import** — the seam pattern (definition/provider/consumer) is the whole point of "everything is a plugin."
2. **Registrations are reversible effects with disposers** that unwind on unload, in a valid teardown order.
3. **`inject`-driven load ordering** (service-requirement activation), not a manual boot sequence.
4. **The five dispatch modes**, with waterfall's `next()` around-middleware veto contract.
5. **Model-visible == logged** runtime invariant: anything reaching a model request must be reconstructable from the session log.

## Sources

- `vendor/cordis/src/service.ts` (Service base, `provide`, `resolveConfig`)
- `vendor/cordis/src/events.ts` (DispatchMode, EventsService emit/parallel/serial/bail/waterfall)
- `vendor/cordis/src/context.ts` (context interface, isolate/extend/intercept, effect symbols)
- `vendor/cordis/src/fiber.ts` (fiber lifecycle, effect/disposal hardening)
- `vendor/README.md` (vendored package manifest + local-modification log)
- `docs/cordis-primer.md` (five ideas, dispatch table, waterfall semantics)
- `docs/architecture.md` (profiles/bundles/patching, dump-config)
