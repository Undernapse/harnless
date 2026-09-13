//! The dynamic WASM plugin surface (issue #15).
//!
//! A plugin is a `.wasm` **component** (component model over WASI
//! preview-2) that exports the [`abi`] plugin interface and is mounted into
//! its **own fiber**. The loader registers the component's tools onto
//! `ctx.tools` — the same [`harnless_seams::Tools`] seam a native tool
//! registers through — so policy, approval, recording, and the guarded
//! pipeline apply to a guest tool identically to a native one.
//!
//! The four load-bearing guarantees, each owned by one module:
//!
//! * [`abi`] — the typed plugin ABI: a descriptor plus per-tool execute
//!   functions, plus the host-imported capability surface. A plugin receives
//!   **only** the capabilities its config grants; nothing is ambient.
//! * [`loader`] — mount/unmount/reload: one fiber per plugin, so removing a
//!   config row unwinds exactly its registrations, reload swaps the
//!   generation without duplicates, and a failed reload keeps the last good
//!   tree mounted.
//! * [`engine`] — the wasmtime embedding: fuel-bounded, memory-limited,
//!   env/network-free stores; a trapping or fuel-exhausted plugin poisons
//!   only its own instance, and the host revives it with a fresh store
//!   instead of taking the session down.
//! * [`fixture`] — the reproducible in-repo component corpus: fixture
//!   plugins assembled from a hand-rolled core module plus canonical
//!   lift/lower wrappers (see the module docs for how each byte is derived).

pub mod abi;
pub mod engine;
pub mod fixture;
pub mod loader;

pub use abi::{Capability, Descriptor, PluginConfig, ToolSpec};
pub use loader::{MountedPlugin, Registration, WasmPluginManager};
