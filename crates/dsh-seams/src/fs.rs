//! The filesystem seam.
//!
//! Swapping a filesystem provider must be invisible to consumers, so the
//! contract is deliberately provider-neutral:
//!
//! * **Targets are opaque.** A path resolves to a [`Target`] — a stable
//!   identity with a display form — and consumers never parse the identity
//!   key or assume it is a local path. Cross-capability coordinates (a path a
//!   subprocess can open, a file URI, containment tests) come from the
//!   provider, not from string manipulation.
//! * **Freshness is backend-owned.** Write and edit take an *optional*
//!   [`WriteGuard`]; omitting it means unconditional, which is what a bare
//!   provider offers. The guarded behavior is layered by policy, not baked
//!   into the seam.
//! * **Edit is one atomic mutation** that verifies the version before
//!   matching, applies a literal replacement, and writes atomically — never a
//!   read plus write composed elsewhere.
//! * **No timeouts** on file operations: a deadline the seam cannot enforce is
//!   worse than none. Cancellation still propagates best-effort.

use crate::error::Result;
use crate::ids::{TargetKey, VersionToken};

/// A provider-resolved filesystem target.
///
/// The consumer holds this opaque handle; the provider owns the mapping back
/// to a real path. Never construct one except through a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Stable opaque identity.
    pub key: TargetKey,
    /// A display form for humans (diff headers, error messages).
    pub display: String,
}

/// An optional freshness guard for a write or edit.
///
/// `None` means unconditional, which a bare provider offers; the guarded
/// behavior is layered by policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteGuard {
    /// Create only if the target does not yet exist.
    CreateIfAbsent,
    /// Replace only if the target is still at `version`.
    ReplaceAtVersion(VersionToken),
}

/// The result of a successful write or edit, carrying the new freshness token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationResult {
    /// The freshness token a follow-up guarded write must present.
    pub version: VersionToken,
}

/// A windowed read: a byte-capped window over a text file that keeps an exact
/// line total even after the byte cap is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadWindow {
    /// The bytes actually returned, possibly truncated at the cap.
    pub contents: Vec<u8>,
    /// The exact line total of the whole file, regardless of the byte cap.
    pub total_lines: u64,
    /// Whether the window was truncated by the byte cap.
    pub truncated: bool,
}

/// A resolved edit to apply. The replacement is a literal byte range swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The exact bytes to replace.
    pub find: Vec<u8>,
    /// The literal bytes to substitute.
    pub replace: Vec<u8>,
}

/// The filesystem seam — provider implementations only, never a consumer.
///
/// Implementations must be `Send + Sync` because the service layer runs on a
/// multi-thread tokio runtime. File operations never take a deadline.
pub trait FileSystem: Send + Sync + 'static {
    /// Resolve `path` to an opaque [`Target`].
    fn resolve(&self, path: &str) -> Result<Target>;

    /// Read the file at `target`, capped at `max_bytes`.
    ///
    /// The returned [`ReadWindow`] always carries the exact total line count,
    /// even when the byte cap truncates the contents.
    fn read(&self, target: &Target, max_bytes: usize) -> Result<ReadWindow>;

    /// Write `contents` to `target`, honoring an optional freshness [`WriteGuard`].
    ///
    /// This is one atomic mutation. A guard mismatch is a `stale-version`
    /// refusal (for `ReplaceAtVersion`) or a `not-found` error (for
    /// `CreateIfAbsent` when the target already exists).
    fn write(
        &self,
        target: &Target,
        contents: &[u8],
        guard: Option<WriteGuard>,
    ) -> Result<MutationResult>;

    /// Apply one atomic literal edit to `target`, honoring an optional guard.
    ///
    /// The edit verifies the present version (when guarded), matches `find`
    /// exactly once, and writes atomically. It is a single mutation, not a
    /// composed read plus write.
    fn edit(
        &self,
        target: &Target,
        edit: &Edit,
        guard: Option<WriteGuard>,
    ) -> Result<MutationResult>;

    /// List the entries directly under `target`.
    fn list(&self, target: &Target) -> Result<Vec<Entry>>;
}

/// One directory entry under a listed target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Display name of the entry.
    pub name: String,
    /// Whether it is a directory.
    pub is_dir: bool,
}
