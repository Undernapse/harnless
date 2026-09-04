//! The credentials seam.
//!
//! Configuration holds *references*; providers own the values; consumers
//! resolve **per operation**, so a rotated secret reaches the very next
//! request with no restart. An authorization-flow seam registers the
//! interactive dance per credential kind, keyed by the record it writes, with
//! one attempt per key — the flow seam owns lifecycle, never protocol.

use crate::error::Result;

/// A reference to a credential, keyed by name. Consumers resolve per
/// operation; the value is never stored in configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialRef(pub String);

/// The kind of credential, which selects the authorization-flow seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Bearer,
    OAuth2,
    ApiKey,
}

/// The credentials seam.
///
/// Resolution happens per operation, so a rotated secret reaches the next
/// request without a restart.
pub trait Credentials: Send + Sync + 'static {
    /// Resolve the secret for `reference`.
    fn resolve(&self, reference: &CredentialRef) -> Result<Option<String>>;

    /// The kind of credential `reference` denotes.
    fn kind(&self, reference: &CredentialRef) -> Result<Option<CredentialKind>>;
}
