//! The storage seam.
//!
//! A named key-value hub for non-conversation state, with backends
//! registered side by side under names, and a typed domain layer mounted on
//! top translating operations into opaque units. Session persistence
//! deliberately uses its own seam rather than this hub.

use serde_json::Value;

use crate::error::Result;

/// A named storage backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackendName(pub String);

/// An opaque storage unit produced by a typed domain layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueUnit(pub Vec<u8>);

/// The storage seam: a named key-value hub.
///
/// Backends are registered side by side under names; a consumer addresses a
/// specific backend by name.
pub trait Storage: Send + Sync + 'static {
    /// Get the value for `key` in `backend`.
    fn get(&self, backend: &BackendName, key: &str) -> Result<Option<Value>>;

    /// Set the value for `key` in `backend`.
    fn set(&self, backend: &BackendName, key: &str, value: Value) -> Result<()>;

    /// Delete the value for `key` in `backend`.
    fn delete(&self, backend: &BackendName, key: &str) -> Result<()>;
}

/// A typed domain layer mounted on top of `Storage`, translating operations
/// into [`OpaqueUnit`]s so the domain need not know the backend.
pub trait StorageDomain: Send + Sync + 'static {
    /// Encode a domain value as an opaque unit.
    fn encode(&self, value: Value) -> Result<OpaqueUnit>;

    /// Decode an opaque unit back into a domain value.
    fn decode(&self, unit: &OpaqueUnit) -> Result<Value>;
}
