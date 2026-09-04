//! # dsh-runtime
//!
//! The Cordis-equivalent service-context runtime for the deepseek-harness
//! clone ("everything is a plugin"). This crate provides:
//!
//! * [`context::Context`] — the repository of services, keyed by type.
//! * [`events::EventRegistry`] — typed events with the five dispatch modes
//!   (`emit`, `parallel`, `serial`, `bail`, `waterfall`), the waterfall
//!   `next()` contract included.
//! * [`fiber::Fiber`] — a lifecycle owner of reversible effects (disposers);
//!   one fiber per mounted plugin, LIFO unwind, registration refused while
//!   unloading.
//! * [`plugin::Plugin`] — the plugin trait with `inject`-declared service
//!   requirements; the loader activates a plugin only when all are present.
//!
//! Consumers never import implementations: a service claims a stable key
//! expressed as its Rust type, and lookups go through typed accessors. The
//! type *is* the stable spelling of dsh's `ctx.<key>` string lookup.

pub mod context;
pub mod error;
pub mod events;
pub mod fiber;
pub mod plugin;
pub mod service;

pub use context::Context;
pub use error::{Result, RuntimeError};
pub use events::{EventOptions, EventRegistry};
pub use fiber::{Disposer, Fiber, FiberState};
pub use plugin::{Plugin, Registry};
pub use service::ServiceMap;
