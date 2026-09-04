//! The settings seam.
//!
//! Namespaces of declared schemas with layered resolution — a shipped
//! composition base beneath a user layer — and a file provider storing the
//! raw document. A provider swap changes storage, never the resolution order.
//! Providers must return redacted, value-free descriptors for anything
//! display-facing.

use serde_json::Value;

use crate::error::Result;

/// A settings namespace, addressed by a stable name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Namespace(pub String);

/// A redacted, value-free descriptor for display-facing output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedDescriptor {
    /// The setting key.
    pub key: String,
    /// Whether the setting has a value in this layer.
    pub present: bool,
    /// A display-safe summary; never the raw value.
    pub summary: String,
}

/// The settings seam.
///
/// Resolution is layered: `layers` are applied lowest-precedence first, so a
/// later layer outranks an earlier one. A provider swap changes how layers
/// are stored, never the resolution order.
pub trait Settings: Send + Sync + 'static {
    /// Resolve the effective value for `key` in `ns` across all layers.
    fn get(&self, ns: &Namespace, key: &str) -> Result<Option<Value>>;

    /// The redacted, value-free descriptor for `key` in `ns`.
    fn describe(&self, ns: &Namespace, key: &str) -> Result<Option<RedactedDescriptor>>;

    /// Whether `ns` has any declared settings.
    fn has_namespace(&self, ns: &Namespace) -> bool;
}
