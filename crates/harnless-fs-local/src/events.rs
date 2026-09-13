//! The `fs/*` policy events: two single-slot decision waterfalls and one
//! fire-and-forget observation record.
//!
//! These are *emitter* events. The provider fires `FsWriteIntent` /
//! `FsEditIntent` through the mounted [`EventRegistry`] immediately before a
//! mutation, and fires `FsObserved` after every completed read. A policy
//! plugin registers listeners and decides the waterfalls (veto by not
//! delegating); the provider never registers a service and never imports
//! agent or session types — the actor is an opaque string the emitter
//! supplies.
//!
//! Single-slot means exactly one decision listener is expected in front of
//! the built-in behavior (proceed). Registering more is legal — waterfalls
//! compose — but the *decision* semantics are one owner's.
//!
//! With no registry mounted the provider is bare: every mutation proceeds
//! and nothing is observed, which is the contract for a policy-less mount.

use harnless_seams::Target;

/// The decision a write/edit-intent waterfall returns.
///
/// The built-in behavior is [`Intent::Allow`]; a policy listener overrides
/// by returning without delegating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Proceed with the mutation.
    Allow,
    /// Refuse the mutation; the provider fails the call `sandbox-denied`.
    Deny(String),
}

/// The `fs/write-intent` waterfall payload: a write about to run.
///
/// Waterfall result is [`Intent`]. The provider fires this before touching
/// the filesystem; a `Deny` short-circuits the mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsWriteIntent {
    /// Opaque actor identity supplied by the emitter (never a session type).
    pub actor: String,
    /// The resolved target about to be written.
    pub target: Target,
}

/// The `fs/edit-intent` waterfall payload: an edit about to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEditIntent {
    /// Opaque actor identity supplied by the emitter.
    pub actor: String,
    /// The resolved target about to be edited.
    pub target: Target,
}

/// The `fs/observed` record: a completed read, fire-and-forget.
///
/// Listeners keep their own state (an observation log, a staleness cache);
/// the provider keeps none. A listener failure is contained by the runtime's
/// emit semantics and never affects the reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsObserved {
    /// Opaque actor identity supplied by the emitter.
    pub actor: String,
    /// The target that was read.
    pub target: Target,
    /// Exact line total of the file as read.
    pub total_lines: u64,
    /// Whether the read was truncated at the byte cap.
    pub truncated: bool,
}
