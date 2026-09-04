//! Branded opaque identifiers.
//!
//! Session ids, message ids, call ids, target keys, and version tokens are
//! distinct newtypes. dsh contracts depend on identities being unparseable
//! and non-substitutable — a session id must never be handed where a message
//! id is expected, and a version token must not be strung into a path. Rust's
//! newtypes make that checkable at compile time.

use std::fmt;

macro_rules! branded_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
            serde::Serialize, serde::Deserialize,
        )]
        pub struct $name(pub u64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}-{}", stringify!($name).to_lowercase(), self.0)
            }
        }
    };
}

// A session id: the unit of replay, fork, and persistence.
branded_id!(
    /// Identifies one session log. The session is the unit of replay, fork,
    /// and persistence; ids are opaque and never parsed by consumers.
    SessionId
);

// A message id: identifies an immutable message in the derived history.
branded_id!(
    /// Identifies one message in the derived history. Messages are immutable;
    /// a message id names a single authored message, never a stream segment.
    MessageId
);

// A call id: correlates a tool call with its result across the log.
branded_id!(
    /// Correlates one tool call with its result. The call id links the tool
    /// call event to the tool result event so a reader can pair them.
    CallId
);

// A target key: the opaque identity of a filesystem target resolved by a
// provider. Consumers must never parse it or assume it is a local path.
branded_id!(
    /// The opaque identity of a filesystem target. A path resolves to one of
    /// these; consumers never parse it or assume it names a local path.
    TargetKey
);

// A version token: a backend-owned freshness token for guarded writes/edits.
branded_id!(
    /// A backend-owned freshness token. Write and edit take an optional guard
    /// that carries one of these; a mismatch is a `stale-version` refusal.
    VersionToken
);
