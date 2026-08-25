# 02 — Codify Cordis semantics the Rust runtime must match

Type: research
Status: open

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

(blank until resolved)
