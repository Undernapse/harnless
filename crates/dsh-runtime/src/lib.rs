//! Runtime primitives for the Rust DeepSeek Harness.

pub mod context;
pub mod error;
pub mod event;
pub mod fiber;
pub mod plugin;

pub use context::Context;
pub use error::{Result, RuntimeError};
pub use fiber::{Disposer, Fiber, FiberState};
pub use plugin::{MountedPlugin, Plugin, PluginLoader};
pub use service::{service_id, service_name, ServiceId};

mod service;
