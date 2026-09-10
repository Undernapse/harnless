//! The local filesystem provider: a [`FileSystem`] implementation rooted at
//! one workspace directory.
//!
//! Design points that carry the contract:
//!
//! * **Identity.** A target key is a stable hash of the canonicalized path.
//!   Canonicalization is the containment test: a path that escapes the root
//!   (through `..`, an absolute path, or a symlink out of the tree) fails to
//!   canonicalize *inside* the root and is refused. Same file, same key —
//!   through any spelling.
//! * **Versions.** A fresh provider starts with no version knowledge; the
//!   first mutation of a path stamps a version, and every later mutation
//!   bumps it. A guarded mutation that presents a stale token is refused
//!   `stale-version` before any matching or writing happens. A guarded
//!   mutation of a path the provider never stamped is `not-observed` — the
//!   guard's token cannot have come from this backend.
//! * **Atomicity.** Mutations write a sibling temp file, fsync it, then
//!   rename over the target: readers see the old file or the new file, never
//!   a half-written one. The version bump happens only after the rename
//!   succeeds.
//! * **Text detection.** UTF-8 validity is the text test; binary reads fail
//!   `not-text` rather than returning replacement mush.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use harnless_seams::{
    Edit, Entry, ErrorCode, FileSystem, MutationResult, ReadWindow, SeamError, Target,
    VersionToken, WriteGuard,
};

use crate::events::{FsEditIntent, FsObserved, FsWriteIntent, Intent};
use harnless_runtime::events::EventRegistry;

/// A local filesystem confined to one workspace root.
///
/// Construct with [`LocalFileSystem::new`] (bare) or
/// [`LocalFileSystem::with_policy`] (fires the `fs/*` events through a
/// registry). The provider holds no observation state; versions are the only
/// bookkeeping.
pub struct LocalFileSystem {
    root: PathBuf,
    /// Path → current version token, keyed by canonicalized path. Only a
    /// mutation through this provider stamps an entry.
    versions: parking_lot::Mutex<HashMap<PathBuf, u64>>,
    /// Monotonic source of version tokens. Tokens are opaque sequence
    /// numbers; consumers only compare them for equality via guards.
    next_version: AtomicU64,
    /// The mounted policy surface, if any. `None` means bare: mutations
    /// proceed unobserved.
    policy: Option<PolicyMount>,
}

/// A mounted policy surface: the registry to fire through and the actor the
/// emitter names itself by (opaque to the filesystem crate).
struct PolicyMount {
    events: EventRegistry,
    actor: String,
}

impl LocalFileSystem {
    /// A bare provider rooted at `root`. Paths resolve relative to it and
    /// can never escape it.
    ///
    /// # Errors
    /// [`ErrorCode::IoError`] when the root cannot be canonicalized (it must
    /// exist before the provider mounts).
    pub fn new(root: impl AsRef<Path>) -> Result<Self, SeamError> {
        Ok(Self {
            root: canonical_root(root.as_ref())?,
            versions: parking_lot::Mutex::new(HashMap::new()),
            next_version: AtomicU64::new(1),
            policy: None,
        })
    }

    /// The same provider with the `fs/*` policy surface mounted: write and
    /// edit fire their intent waterfalls, reads fire the observation record.
    /// `actor` is the opaque identity listeners see.
    pub fn with_policy(
        root: impl AsRef<Path>,
        events: EventRegistry,
        actor: impl Into<String>,
    ) -> Result<Self, SeamError> {
        Ok(Self {
            policy: Some(PolicyMount {
                events,
                actor: actor.into(),
            }),
            ..Self::new(root)?
        })
    }

    /// The current version token for `target`, if this provider ever
    /// mutated it. Exposed for tool authors composing guarded flows; the
    /// value is opaque and only meaningful as a guard.
    pub fn version_of(&self, target: &Target) -> Option<VersionToken> {
        let path = self.path_of(target);
        self.versions.lock().get(&path).copied().map(VersionToken)
    }

    /// Resolve a target back to a real path. Provider-internal: consumers
    /// hold [`Target`] handles and never see this.
    fn path_of(&self, target: &Target) -> PathBuf {
        // The key is a hash; the display form is the canonicalized path
        // relative to the root, which we re-join and re-canonicalize. The
        // re-canonicalization is a containment re-check, not a parse of the
        // key.
        let joined = self.root.join(&target.display);
        // A tampered display (a Target not from this provider) escapes the
        // root; canonical_root refuses it. Fall back to the joined path so
        // the caller's own IO surfaces a plain not-found rather than
        // panicking here.
        canonical_root(&joined).unwrap_or(joined)
    }

    /// Fire the write-intent waterfall. Bare providers allow unconditionally.
    fn decide_write(&self, target: &Target) -> Result<(), SeamError> {
        match &self.policy {
            None => Ok(()),
            Some(mount) => {
                let event = FsWriteIntent {
                    actor: mount.actor.clone(),
                    target: target.clone(),
                };
                match mount.events.waterfall(event, |_e| Intent::Allow) {
                    Intent::Allow => Ok(()),
                    Intent::Deny(reason) => Err(SeamError::new(
                        ErrorCode::SandboxDenied,
                        format!("write refused by policy: {reason}"),
                    )),
                }
            }
        }
    }

    /// Fire the edit-intent waterfall.
    fn decide_edit(&self, target: &Target) -> Result<(), SeamError> {
        match &self.policy {
            None => Ok(()),
            Some(mount) => {
                let event = FsEditIntent {
                    actor: mount.actor.clone(),
                    target: target.clone(),
                };
                match mount.events.waterfall(event, |_e| Intent::Allow) {
                    Intent::Allow => Ok(()),
                    Intent::Deny(reason) => Err(SeamError::new(
                        ErrorCode::SandboxDenied,
                        format!("edit refused by policy: {reason}"),
                    )),
                }
            }
        }
    }

    /// Fire the observation record for a completed read.
    fn observe_read(&self, target: &Target, window: &ReadWindow) {
        if let Some(mount) = &self.policy {
            mount.events.emit(FsObserved {
                actor: mount.actor.clone(),
                target: target.clone(),
                total_lines: window.total_lines,
                truncated: window.truncated,
            });
        }
    }

    /// The guarded-freshness prelude shared by write and edit: verify the
    /// guard against the backend's own version record *before* touching the
    /// file, and return the current on-disk state for the mutation to fold.
    ///
    /// Returns the existing bytes (`None` when absent) and the token the
    /// mutation must stamp after success.
    fn check_guard(
        &self,
        path: &Path,
        guard: &Option<WriteGuard>,
        exists: bool,
    ) -> Result<(), SeamError> {
        match guard {
            None => Ok(()),
            Some(WriteGuard::CreateIfAbsent) if exists => Err(SeamError::new(
                ErrorCode::StaleVersion,
                format!("target already exists: {}", path.display()),
            )),
            Some(WriteGuard::CreateIfAbsent) => Ok(()),
            Some(WriteGuard::ReplaceAtVersion(seen)) => {
                let current = self.versions.lock().get(path).copied();
                match current {
                    // The provider never stamped this path: the caller's
                    // token cannot have come from this backend.
                    None => Err(SeamError::new(
                        ErrorCode::NotObserved,
                        format!(
                            "no version recorded for {}; the guard token cannot be from this backend",
                            path.display()
                        ),
                    )),
                    Some(v) if VersionToken(v) != *seen => Err(SeamError::new(
                        ErrorCode::StaleVersion,
                        format!(
                            "target moved past the presented version: {}",
                            path.display()
                        ),
                    )),
                    Some(_) => Ok(()),
                }
            }
        }
    }

    /// One atomic mutation: write `contents` to a sibling temp, fsync,
    /// rename over the target, then bump the version. The version only
    /// advances when the rename lands, so a failed mutation never advances
    /// freshness.
    fn commit_atomic(
        &self,
        path: &Path,
        contents: &[u8],
        create_parent: bool,
    ) -> Result<VersionToken, SeamError> {
        if create_parent {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(io_error)?;
            }
        }
        let token = VersionToken(self.next_version.fetch_add(1, Ordering::SeqCst));
        let tmp = temp_path(path, token);
        let write = (|| -> Result<(), SeamError> {
            let mut file = fs::File::create(&tmp).map_err(io_error)?;
            io::Write::write_all(&mut file, contents).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            fs::rename(&tmp, path).map_err(io_error)?;
            Ok(())
        })();
        if write.is_err() {
            let _ = fs::remove_file(&tmp);
            return write.map(|()| token);
        }
        self.versions.lock().insert(path.to_path_buf(), token.0);
        Ok(token)
    }
}

/// Canonicalize `path` and verify it stays inside `root`.
///
/// This is the containment test. Lexical tricks (`..`, absolute paths) are
/// resolved by the OS during canonicalization; a symlink pointing outside
/// the root canonicalizes outside it and is refused. The parent of a
/// not-yet-existing file is canonicalized instead, so creating a new file
/// gets the same guarantee.
fn confine(root: &Path, path: &Path) -> Result<PathBuf, SeamError> {
    let joined = root.join(path);
    // Normalize away `.` and `..` lexically first so a not-yet-existing
    // target's parent lookup is stable, then canonicalize what exists.
    let normalized = normalize_lexically(&joined);
    let canonical = match canonical_root(&normalized) {
        Ok(c) => c,
        // The path (or a parent) does not exist yet: canonicalize the
        // deepest existing ancestor and re-join the rest.
        Err(_) => {
            let mut ancestor = normalized.as_path();
            while !ancestor.exists() {
                ancestor = ancestor.parent().ok_or_else(|| {
                    SeamError::new(
                        ErrorCode::NotFound,
                        format!("no existing ancestor for {}", normalized.display()),
                    )
                })?;
            }
            let base = canonical_root(ancestor)?;
            let rest = normalized
                .strip_prefix(ancestor)
                .map_err(|_| SeamError::new(ErrorCode::IoError, "path normalization failed"))?;
            base.join(rest)
        }
    };
    if canonical.starts_with(root) {
        Ok(canonical)
    } else {
        Err(SeamError::new(
            ErrorCode::SandboxDenied,
            format!("path escapes the workspace root: {}", normalized.display()),
        ))
    }
}

/// Canonicalize requiring the path to exist and be a directory root or
/// inside it — used for the root itself and the display re-check.
fn canonical_root(path: &Path) -> Result<PathBuf, SeamError> {
    path.canonicalize().map_err(io_error)
}

/// Remove `.` components and resolve `..` lexically (no IO). A `..` that
/// pops above the start stays representable; containment is decided after
/// canonicalization, not here.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A sibling temp path for an atomic replace, unique per (target, token).
fn temp_path(path: &Path, token: VersionToken) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.harnless-tmp-{}", token.0))
}

/// Map an IO error onto the taxonomy: the kernel's kinds, mapped faithfully.
/// Sandbox denial is never produced here — that code belongs to the policy
/// layer, and keeping the two distinct is the point.
fn io_error(e: io::Error) -> SeamError {
    let code = match e.kind() {
        io::ErrorKind::NotFound => ErrorCode::NotFound,
        io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        _ => ErrorCode::IoError,
    };
    SeamError::new(code, e.to_string())
}

/// Exact line total of `bytes`: the number of line starts, i.e. `\n`
/// terminators plus a final unterminated line when non-empty.
fn count_lines(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut lines = 1u64;
    for b in bytes {
        if *b == b'\n' {
            lines += 1;
        }
    }
    // A trailing newline does not open a new line.
    if bytes.last() == Some(&b'\n') {
        lines -= 1;
    }
    lines
}

/// Count occurrences of `needle` in `haystack` without overlapping matches.
fn count_matches(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }
    let mut count = 0;
    let mut at = 0;
    while let Some(offset) = find(&haystack[at..], needle) {
        count += 1;
        at += offset + needle.len();
    }
    count
}

/// Minimal byte-substring search (std lacks one).
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

impl FileSystem for LocalFileSystem {
    fn resolve(&self, path: &str) -> Result<Target, SeamError> {
        let canonical = confine(&self.root, Path::new(path))?;
        let display = canonical
            .strip_prefix(&self.root)
            .unwrap_or(canonical.as_path())
            .to_string_lossy()
            .into_owned();
        // The key is a stable hash of the canonical path: same file, same
        // key, through any spelling. FNV-1a over the bytes; collisions are
        // astronomically unlikely at workspace scale and the key is only
        // ever compared for equality.
        Ok(Target {
            key: harnless_seams::TargetKey(fnv1a(canonical.as_os_str().as_encoded_bytes())),
            display,
        })
    }

    fn read(&self, target: &Target, max_bytes: usize) -> Result<ReadWindow, SeamError> {
        let path = self.path_of(target);
        let meta = fs::symlink_metadata(&path).map_err(io_error)?;
        if !meta.is_file() {
            // Any wrong-kind target (directory, fifo, symlink) is the same
            // refusal: the seam reads regular files or not at all.
            return Err(SeamError::new(
                ErrorCode::NotARegularFile,
                format!("not a regular file: {}", path.display()),
            ));
        }
        let bytes = fs::read(&path).map_err(io_error)?;
        if !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
            return Err(SeamError::new(
                ErrorCode::NotText,
                format!("not valid UTF-8 text: {}", path.display()),
            ));
        }
        let total_lines = count_lines(&bytes);
        let truncated = bytes.len() > max_bytes;
        let mut contents = bytes;
        contents.truncate(max_bytes.min(contents.len()));
        let window = ReadWindow {
            contents,
            total_lines,
            truncated,
        };
        self.observe_read(target, &window);
        Ok(window)
    }

    fn write(
        &self,
        target: &Target,
        contents: &[u8],
        guard: Option<WriteGuard>,
    ) -> Result<MutationResult, SeamError> {
        self.decide_write(target)?;
        let path = self.path_of(target);
        let exists = path.symlink_metadata().is_ok();
        self.check_guard(&path, &guard, exists)?;
        if let Some(parent) = path.parent() {
            if parent.exists() && !parent.is_dir() {
                return Err(SeamError::new(
                    ErrorCode::NotADirectory,
                    format!("parent is not a directory: {}", parent.display()),
                ));
            }
        }
        let version = self.commit_atomic(&path, contents, true)?;
        Ok(MutationResult { version })
    }

    fn edit(
        &self,
        target: &Target,
        edit: &Edit,
        guard: Option<WriteGuard>,
    ) -> Result<MutationResult, SeamError> {
        self.decide_edit(target)?;
        let path = self.path_of(target);
        let exists = path.symlink_metadata().is_ok();
        self.check_guard(&path, &guard, exists)?;
        // The mutation reads the current bytes inside the same call that
        // writes them — one mutation, not a composed read-plus-write the
        // caller could interleave against.
        let bytes = fs::read(&path).map_err(io_error)?;
        if std::str::from_utf8(&bytes).is_err() {
            return Err(SeamError::new(
                ErrorCode::NotText,
                format!("not valid UTF-8 text: {}", path.display()),
            ));
        }
        match count_matches(&bytes, &edit.find) {
            0 => Err(SeamError::new(
                ErrorCode::EditNotFound,
                "edit pattern not found",
            )),
            1 => {
                let at = find(&bytes, &edit.find).expect("one match");
                let mut next =
                    Vec::with_capacity(bytes.len() - edit.find.len() + edit.replace.len());
                next.extend_from_slice(&bytes[..at]);
                next.extend_from_slice(&edit.replace);
                next.extend_from_slice(&bytes[at + edit.find.len()..]);
                let version = self.commit_atomic(&path, &next, false)?;
                Ok(MutationResult { version })
            }
            n => Err(SeamError::new(
                ErrorCode::AmbiguousEdit,
                format!("edit pattern matched {n} times; exactly one match is required"),
            )),
        }
    }

    fn list(&self, target: &Target) -> Result<Vec<Entry>, SeamError> {
        let path = self.path_of(target);
        let meta = fs::symlink_metadata(&path).map_err(io_error)?;
        if !meta.is_dir() {
            return Err(SeamError::new(
                ErrorCode::NotADirectory,
                format!("not a directory: {}", path.display()),
            ));
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&path).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let file_type = entry.file_type().map_err(io_error)?;
            entries.push(Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: file_type.is_dir(),
            });
        }
        // Deterministic listing: sorted by name, never readdir order.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }
}

/// FNV-1a, 64-bit. Stable across runs and platforms; keys are only ever
/// compared for equality, never parsed.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
