//! # harnless-fs-local
//!
//! Local filesystem provider for harnless: the `fs-local` implementation of
//! the [`FileSystem`] seam from `harnless-seams`. Consumers (`tool-fs`:
//! read/write/edit/list) never know where the bytes live.
//!
//! Provider obligations, per the seam contract:
//!
//! * **Targets are opaque.** A path resolves to a stable [`TargetKey`] plus a
//!   display form; the key is a content hash of the canonicalized path, never
//!   the path itself, and consumers never parse it. Two resolves of the same
//!   file — through different spellings, `..` hops, or symlinks — yield the
//!   same key.
//! * **Freshness is backend-owned.** Every mutation bumps a version token;
//!   a guarded write/edit presents the token it saw and gets a
//!   `stale-version` refusal when it is no longer current. An unguarded
//!   mutation is unconditional, exactly what a bare provider offers.
//! * **Edit is one atomic mutation**: version check, exact single match,
//!   literal byte swap, atomic replace — never a read plus write composed by
//!   the caller.
//! * **Windowed reads** return the exact line total of the whole file even
//!   when the byte cap truncates the contents.
//! * **The error taxonomy** is machine-routable end to end: `not-found`,
//!   `not-a-directory`, `not-text`, `not-a-regular-file`, `too-large`,
//!   `permission-denied`, `io-error`, `stale-version`, `not-observed`,
//!   `ambiguous-edit`, `edit-not-found`. `sandbox-denied` is reserved for the
//!   policy layer and never emitted by the bare provider — kernel denial and
//!   sandbox denial stay distinct.
//! * **No timeouts** on file operations; cancellation propagates
//!   best-effort through the caller's own drop.
//!
//! ## Policy surface
//!
//! The `fs/write-intent` and `fs/edit-intent` decision waterfalls and the
//! `fs/observed` record are *emitter* events: the provider fires them through
//! an [`EventRegistry`] when one is mounted, and works bare — no registry,
//! no policy plugin — when none is. The event types live here, the provider
//! owns the emission points, and a policy plugin registers listeners without
//! the filesystem crate ever importing agent or session types.

mod events;
mod provider;
pub use events::{FsEditIntent, FsObserved, FsWriteIntent, Intent};

pub use provider::LocalFileSystem;
