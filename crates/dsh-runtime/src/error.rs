//! Error types for the dsh runtime.
//!
//! Error codes are stable machine-readable identifiers consumers route on;
//! messages are for humans only.

use core::fmt;

/// A runtime error carrying a stable machine-readable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    /// Stable machine-readable code, e.g. `INACTIVE_EFFECT`.
    pub code: &'static str,
    /// Human-readable message.
    pub message: &'static str,
}

impl RuntimeError {
    /// Create an error from a stable code and a display message.
    pub const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Attempting to create an effect on a context whose fiber is no longer
    /// active (unloading or disposed).
    pub const fn inactive_effect() -> Self {
        Self::new(
            "INACTIVE_EFFECT",
            "cannot create effect on inactive context",
        )
    }

    /// Attempting to run plugin code on a fiber that has been disposed.
    pub const fn inactive_fiber() -> Self {
        Self::new("INACTIVE_FIBER", "fiber is disposed")
    }

    /// Two providers attempted to claim the same key in one scope.
    pub const fn duplicate_service() -> Self {
        Self::new(
            "DUPLICATE_SERVICE",
            "service key already has a provider in this scope",
        )
    }

    /// A listener panicked while being dispatched in a contained mode.
    pub const fn listener_panicked() -> Self {
        Self::new("LISTENER_PANICKED", "event listener panicked")
    }

    /// Two queued or mounted plugins claimed the same operational identity.
    pub const fn duplicate_plugin() -> Self {
        Self::new("DUPLICATE_PLUGIN", "plugin identity is already in use")
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RuntimeError {}

pub type Result<T, E = RuntimeError> = std::result::Result<T, E>;
