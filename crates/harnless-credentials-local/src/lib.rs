//! # harnless-credentials-local
//!
//! Local provider for the harnless [`Credentials`](harnless_seams::credentials::Credentials) seam: configuration
//! holds *references*, this provider owns the values, and consumers
//! resolve **per operation** — so a secret rotated on disk reaches the
//! very next `resolve` with no restart and no cache.
//!
//! Provider obligations, per the seam contract:
//!
//! * **Read-through resolution.** The record store is a JSON file on disk
//!   (`{ ref: { kind, value } }`). `resolve` and `kind` read it per call;
//!   nothing is cached. A rotation performed by any writer — this provider,
//!   an authorization flow, or an operator with an editor — is visible to
//!   the next operation.
//! * **Reference honesty.** An unknown reference resolves `Ok(None)` and
//!   kinds `Ok(None)`; `kind` always agrees with `resolve`.
//! * **Authorization-flow seam.** [`AuthorizationFlow`] registers the
//!   interactive dance per [`CredentialKind`](harnless_seams::credentials::CredentialKind), keyed by the record it
//!   writes. The registry enforces **one attempt per key**: while a flow is
//!   in flight for a `CredentialRef`, a second `authorize` for the same key
//!   never starts a second dance — it awaits the first and adopts its
//!   result (or reports the first's failure). The flow writes the record
//!   through the provider ([`LocalCredentials::store`]), and the registry
//!   owns the lifecycle (in-flight bookkeeping, completion, cleanup) —
//!   never the protocol (how the secret is obtained is entirely the flow's
//!   business).
//!
//! Writes are serialized by a process-wide lock on the record path and
//! read-modify-written, so concurrent writers — including a second harnless
//! process sharing the file — do not lose records.

mod provider;
pub use provider::{
    AuthorizationFlow, AuthorizeState, CredentialRecord, FileLock, LocalCredentials,
    LocalCredentialsBuilder,
};
