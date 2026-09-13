//! # harnless-storage-jsonl
//!
//! JSONL provider for the harnless [`Storage`](harnless_seams::storage::Storage) seam: a named key-value hub
//! where each [`BackendName`](harnless_seams::storage::BackendName) is a side-by-side append-only JSONL log, with
//! a typed [`StorageDomain`](harnless_seams::storage::StorageDomain) layer mounted on top translating operations
//! into [`OpaqueUnit`](harnless_seams::storage::OpaqueUnit)s.
//!
//! Provider obligations, per the seam contract:
//!
//! * **Backends registered side by side under names.** Each backend is its
//!   own file (`<root>/<sanitized-name>.jsonl`) in one root directory.
//!   Creating a backend is creating a file; backends never share state, and
//!   addressing one by name never touches another's bytes.
//! * **Append-only log + replay.** A `set` appends `{"op":"set",…}`; a
//!   `delete` appends a tombstone `{"op":"del",…}`. `get` replays the log
//!   and reports the last operation per key. Read-your-writes is inherent:
//!   the append is durable before the call returns, and replay sees it.
//!   Delete-then-get is `Ok(None)` because the tombstone outranks the set.
//!   A restart (fresh provider on the same root) sees everything: the log
//!   *is* the state.
//! * **Compaction is safe.** [`JsonlStorage::compact`] rewrites a backend
//!   as one snapshot of live keys. It publishes via temp-file + rename, and
//!   every write re-checks the file's length and re-appends any bytes a
//!   racing writer added in the meantime, so a concurrent append is never
//!   lost.
//! * **Typed domain layer.** [`JsonlDomain`] is the reference
//!   [`StorageDomain`](harnless_seams::storage::StorageDomain): it encodes a domain value to an opaque unit (base64
//!   of a versioned JSON envelope) and decodes it back, so the domain
//!   speaks typed records while the hub stores opaque bytes.
//! * **Machine-routable errors.** A backend name that sanitizes to nothing
//!   is `not-found`; a corrupt log line is `io-error` naming the backend
//!   and line — never a silently skipped record.

mod provider;

pub use provider::{BackendNameExt, JsonlDomain, JsonlStorage};
