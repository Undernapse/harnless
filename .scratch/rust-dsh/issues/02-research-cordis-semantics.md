# 02 — Codify Cordis semantics the Rust runtime must match

Type: research
Status: resolved

## Question

Extract from `dsh`'s vendored Cordis (`docs/cordis-primer.md`, `docs/cordis-tutorial/`, `vendor/`, `packages/`) a precise reference of the semantics the Rust "Cordis-equivalent" runtime must faithfully reimplement:

- The **plugin** model: a plugin is an object implementing Service — a function with optional `inject` + `apply(ctx)`, or a `Service` subclass mounted into the current context.
- The **context** as a repository of services: a service claims a stable `ctx.<key>`; plugins find services by key, not by importing a concrete impl.
- **`inject`** service-declaration dependency: load order expressed via service requirements.
- **Typed events** and the four dispatch modes — `emit`, `waterfall`, `parallel`, `serial` — their awaitedness, order, and return-value semantics, including the `ctx.waterfall` around-middleware `next()` delegation contract.
- **Reversible effects / disposers**: `ctx.effect()` / `ctx.on()` registrations that unwind on teardown, and teardown ordering.
- **Loader config / profiles / bundles / patching**: how `cordis.patch.yml` and `--dump-config` compose the plugin tree at boot.

Output a reference doc (`docs/cordis-semantics.md` at repo root) that a Rust designer reads next to traits/async code.

## Answer

Semantics reference delivered in [`docs/research/cordis-semantics.md`](../../../docs/research/cordis-semantics.md). Key points the Rust runtime must match:

- **Five** dispatch modes (the primer's table omits `bail`; the source has it): `emit` (sync fire-and-forget), `parallel` (Promise.allSettled), `serial` (in order until one bails), `bail` (stop on first sync bail), `waterfall` (around-middleware with `next()`; not calling `next()` vetoes).
- Plugin = object implementing Service: function/object form `{ inject?, apply(ctx) }`, or a `Service` subclass. `inject` names required services; the registry waits for them before activating (service-requirement load order, not manual boot).
- Context = repository of services found by stable `ctx.<key>`; `ctx.reflect.provide(name, instance, check)`; auto-removal on fiber unload; scoped via `extend()`/`isolate()`/`intercept()`; `resolveConfig` merges ancestor intercept config.
- Reversible effects/disposers via `ctx.effect()`/`ctx.on()`; fiber lifecycle states (PENDING/LOADING legal, UNLOADING rejects new effect creation); dsh's vendored hardening closes reentrant-disposal gaps.
- Loader/config: profiles → bundles → `cordis.patch.yml` → home → `--patch` overlay, applied to an empty entry list; a patch targets a row by id and replaces its whole config; `--dump-config` prints the tree. Loader reconciliation is transactional (dispose-before-apply, restore on failure).
- Five invariants to preserve in Rust listed at the top of the reference doc (service-by-key, reversible effects, inject ordering, five modes + waterfall veto, model-visible==logged).
