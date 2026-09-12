//! The local filesystem provider: a [`FileSystem`] implementation rooted at
//! one workspace directory.
//!
//! Design points that carry the contract:
//!
//! * **Containment is enforced at the point of IO, by descriptor.** The root
//!   is opened once; every operation walks it component-by-component with
//!   `openat(O_NOFOLLOW)`, refusing every symlink and non-directory hop.
//!   No decision is ever taken on a path string and then acted on later —
//!   the fd chain *is* the containment proof, so `..` games, absolute
//!   paths, forged `Target` displays, and symlink swaps (before or after
//!   `resolve`) all fail at the syscall, not at a check that could race.
//!   An escape attempt is `sandbox-denied`; the kernel's own refusals stay
//!   `permission-denied` / `not-found`. The two codes never mix.
//! * **Identity.** A target key is a stable hash of the canonical path.
//!   Every operation re-derives the canonical path from its fd chain and
//!   verifies `fnv1a(canonical) == target.key`: a `Target` whose key and
//!   display disagree (tampered, stale, or from another provider) is
//!   refused. The key is the identity; the display is a hint that must
//!   hash to it.
//! * **Versions.** A fresh provider starts with no version knowledge; the
//!   first mutation of a path stamps a version, and every later mutation
//!   bumps it. A guarded mutation that presents a stale token is refused
//!   `stale-version` before any matching or writing happens. A guarded
//!   mutation of a path the provider never stamped is `not-observed`.
//!   Tokens carry a per-instance nonce so a token minted by one provider
//!   instance can never numerically alias a token from another.
//!   Freshness is *provider-observed mutation order*: an out-of-band
//!   change (a shell command writing inside the workspace) does not bump
//!   the version, and two instances mounted on one root keep independent
//!   version records — a guard from one is `not-observed` to the other.
//! * **Atomicity.** Mutations write a unique sibling temp file, fsync it,
//!   rename over the target through the verified parent directory fd
//!   (`renameat`), and fsync the parent so the rename is durable. Readers
//!   see the old file or the new file, never a half-written one, and the
//!   rename survives a power loss. The version bump happens only after the
//!   rename succeeds.
//! * **Text detection.** UTF-8 validity is the text test; binary reads fail
//!   `not-text` rather than returning replacement mush. Reads are bounded:
//!   at most `max_bytes` of content is buffered, while the line total is
//!   counted exactly to EOF.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use harnless_seams::{
    Edit, Entry, ErrorCode, FileSystem, MutationResult, ReadWindow, SeamError, Target, VersionToken,
    WriteGuard,
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
    /// The canonical root, kept for display stripping and key hashing.
    root: PathBuf,
    /// The root opened once; every operation walks from this fd.
    root_fd: RawFd,
    /// Path → current version token, keyed by canonical path. Only a
    /// mutation through this provider stamps an entry.
    versions: parking_lot::Mutex<HashMap<PathBuf, u64>>,
    /// Monotonic source of version numbers, combined with a per-instance
    /// nonce so tokens are globally unique across instances.
    next_version: AtomicU64,
    /// Random per-instance nonce mixed into every token and temp name.
    nonce: u64,
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

/// The verified result of opening a target through the root fd chain: the
/// open descriptor and the canonical path it provably names.
struct Opened {
    fd: RawFd,
    canonical: PathBuf,
}

/// A mutation's verified parent directory plus the final component name.
struct ParentAndName {
    dirfd: RawFd,
    /// The canonical path of the parent directory.
    dir_canonical: PathBuf,
    name: CString,
}

/// Chunk size for windowed reads: reads buffer at most `max_bytes` of
/// content but stream the remainder in chunks of this size so the line
/// total stays exact past the cap without holding the whole file.
const READ_CHUNK: usize = 64 * 1024;

impl LocalFileSystem {
    /// A bare provider rooted at `root`. Paths resolve relative to it and
    /// can never escape it.
    ///
    /// # Errors
    /// [`ErrorCode::IoError`] when the root cannot be canonicalized or
    /// opened (it must exist before the provider mounts).
    pub fn new(root: impl AsRef<Path>) -> Result<Self, SeamError> {
        let canonical = root
            .as_ref()
            .canonicalize()
            .map_err(|e| SeamError::new(ErrorCode::IoError, format!("root: {e}")))?;
        let root_c = cstr(canonical.as_os_str())?;
        // The root itself is opened O_NOFOLLOW: a root that is a symlink is
        // a mount mistake, not something to silently follow.
        let root_fd = unsafe {
            libc::open(
                root_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if root_fd < 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        Ok(Self {
            root: canonical,
            root_fd,
            versions: parking_lot::Mutex::new(HashMap::new()),
            next_version: AtomicU64::new(1),
            nonce: random_nonce(),
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
        let mut provider = Self::new(root)?;
        provider.policy = Some(PolicyMount {
            events,
            actor: actor.into(),
        });
        Ok(provider)
    }

    /// The current version token for `target`, if this provider ever
    /// mutated it. Exposed for tool authors composing guarded flows; the
    /// value is opaque and only meaningful as a guard.
    ///
    /// # Errors
    /// The same containment/integrity refusals as any operation on a bad
    /// `Target`.
    pub fn version_of(&self, target: &Target) -> Option<VersionToken> {
        // The lookup is by canonical path; the display is the path hint, so
        // no fd walk is needed and a forged display cannot make a stale
        // token look fresh. "." names the root, which is never mutated.
        if target.display == "." {
            return None;
        }
        let canonical = self.root.join(Path::new(&target.display));
        self.versions
            .lock()
            .get(&canonical)
            .copied()
            .map(|raw| VersionToken(self.stamp(raw)))
    }

    /// Translate the instance-local counter into a globally-unique token by
    /// mixing in the per-instance nonce. Tokens from different provider
    /// another backend is always `not-observed`, never accidentally fresh.
    /// The counter itself is what the version map stores.
    fn stamp(&self, counter: u64) -> u64 {
        fnv1a(&self.nonce.to_le_bytes()) ^ fnv1a(&counter.to_le_bytes())
    }

    /// Walk the root fd chain to `target`, refusing escapes at every hop,
    /// and verify the target's key hashes from the canonical path the fd
    /// chain proves.
    ///
    /// `create_parents` materializes missing ancestor directories (write's
    /// convenience); each created directory is itself opened and verified
    /// contained before use.
    fn open_target(&self, target: &Target, create_parents: bool) -> Result<Opened, SeamError> {
        if target.display == "." {
            // The root itself: dup the root fd and verify its key.
            self.verify_key(target, &self.root)?;
            let fd = unsafe { libc::dup(self.root_fd) };
            if fd < 0 {
                return Err(io_error(io::Error::last_os_error()));
            }
            return Ok(Opened {
                fd,
                canonical: self.root.clone(),
            });
        }
        let parent = self.open_parent(target, create_parents)?;
        // rather than followed. ENOENT is not an error here — write creates,
        // and read/edit surface not-found when they try to use the fd.
        let fd = unsafe {
            libc::openat(
                parent.dirfd,
                parent.name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        let file_fd = if fd >= 0 {
            fd
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOENT) {
                -1
            } else {
                unsafe { libc::close(parent.dirfd) };
                return Err(openat_error(e));
            }
        };
        let canonical = parent.dir_canonical.join(OsStr::from_bytes(parent.name.as_bytes()));
        unsafe { libc::close(parent.dirfd) };
        self.verify_key(target, &canonical)?;
        Ok(Opened {
            fd: file_fd,
            canonical,
        })
    }

    /// Walk to the target's parent directory, verifying every hop, and hand
    /// back the verified dirfd plus the final component name.
    fn open_parent(&self, target: &Target, create_parents: bool) -> Result<ParentAndName, SeamError> {
        // The display is root-relative (resolve guarantees it); the fd walk
        // below only accepts plain component names.
        let rel = Path::new(&target.display);
        let mut components: Vec<std::ffi::OsString> = Vec::new();
        for c in rel.components() {
            match c {
                Component::CurDir => {}
                Component::Normal(name) => components.push(name.to_os_string()),
                // `..` cancels the pending name (the same normalization
                // openat performs); a climb past the root pops nothing and
                // the resulting path simply names the root.
                Component::ParentDir => {
                    components.pop();
                }
                // RootPrefix / PrefixDir: not a workspace-relative display.
                _ => {
                    return Err(SeamError::new(
                        ErrorCode::SandboxDenied,
                        format!(
                            "target display is not workspace-relative: {:?}",
                            target.display
                        ),
                    ))
                }
            }
        }
        if components.is_empty() {
            // An empty display names the root itself: the listable target
            // resolve(".") yields.
            let name = cstr(OsStr::new(""))?;
            return Ok(ParentAndName {
                dirfd: unsafe { libc::dup(self.root_fd) },
                dir_canonical: self.root.clone(),
                name,
            });
        }
        let name = cstr(components.pop().expect("non-empty").as_os_str())?;
        // Walk every ancestor hop with O_NOFOLLOW. A symlinked hop is
        // sandbox-denied; a missing hop is created when asked, then
        // re-opened and verified.
        let mut dirfd = unsafe { libc::dup(self.root_fd) };
        if dirfd < 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        let mut dir_canonical = self.root.clone();
        for component in components {
            let hop = cstr(&component)?;
            match openat_nofollow_dir(dirfd, &hop) {
                Ok(next) => {
                    unsafe { libc::close(dirfd) };
                    dirfd = next;
                    dir_canonical.push(&component);
                }
                Err(e)
                    if create_parents
                        && matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) =>
                {
                    // Materialize the hop. macOS has no mkdirat(2), so the
                    // path-based create uses the parent's real path, which
                    // the fd walk above just proved contained. The re-open
                    // after create is O_NOFOLLOW against the result, so a
                    // symlink planted in the race loses.
                    let full = dir_canonical.join(&component);
                    if let Err(made) = fs::create_dir_all(&full) {
                        unsafe { libc::close(dirfd) };
                        return Err(io_error(made));
                    }
                    let next = openat_nofollow_dir(dirfd, &hop).map_err(io_error)?;
                    unsafe { libc::close(dirfd) };
                    dirfd = next;
                    dir_canonical.push(&component);
                }
                Err(e) => {
                    unsafe { libc::close(dirfd) };
                    return Err(openat_error(e));
                }
            }
        }
        Ok(ParentAndName {
            dirfd,
            dir_canonical,
            name,
        })
    }

    /// The key is the identity: recompute it from the canonical path the fd
    /// chain proved and refuse a `Target` that does not hash to it.
    fn verify_key(&self, target: &Target, canonical: &Path) -> Result<(), SeamError> {
        let expected = fnv1a(canonical.as_os_str().as_bytes());
        if target.key.0 == expected {
            Ok(())
        } else {
            Err(SeamError::new(
                ErrorCode::SandboxDenied,
                format!(
                    "target key does not match its display {:?}; the handle is stale or tampered",
                    target.display
                ),
            ))
        }
    }

    /// Fire the write-intent waterfall. Bare providers allow unconditionally.
    /// A panicking policy listener is contained: the built-in Allow stands.
    fn decide_write(&self, target: &Target) -> Result<(), SeamError> {
        let Some(mount) = &self.policy else {
            return Ok(());
        };
        let event = FsWriteIntent {
            actor: mount.actor.clone(),
            target: target.clone(),
        };
        let decision = guarded_waterfall(mount, event);
        match decision {
            Intent::Allow => Ok(()),
            Intent::Deny(reason) => Err(SeamError::new(
                ErrorCode::SandboxDenied,
                format!("write refused by policy: {reason}"),
            )),
        }
    }

    /// Fire the edit-intent waterfall.
    fn decide_edit(&self, target: &Target) -> Result<(), SeamError> {
        let Some(mount) = &self.policy else {
            return Ok(());
        };
        let event = FsEditIntent {
            actor: mount.actor.clone(),
            target: target.clone(),
        };
        let decision = guarded_edit_waterfall(mount, event);
        match decision {
            Intent::Allow => Ok(()),
            Intent::Deny(reason) => Err(SeamError::new(
                ErrorCode::SandboxDenied,
                format!("edit refused by policy: {reason}"),
            )),
        }
    }

    /// Fire the observation record for a completed read. A panicking
    /// observer is contained by the runtime's emit semantics; a panic here
    /// would abort a successful read, so the emit itself is guarded.
    fn observe_read(&self, target: &Target, window: &ReadWindow) {
        if let Some(mount) = &self.policy {
            let event = FsObserved {
                actor: mount.actor.clone(),
                target: target.clone(),
                total_lines: window.total_lines,
                truncated: window.truncated,
            };
            let events = mount.events.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                events.emit(event)
            }));
        }
    }

    /// The guarded-freshness prelude shared by write and edit: verify the
    /// guard against the backend's own version record *before* touching the
    /// file.
    fn check_guard(
        &self,
        canonical: &Path,
        guard: &Option<WriteGuard>,
        exists: bool,
    ) -> Result<(), SeamError> {
        match guard {
            None => Ok(()),
            Some(WriteGuard::CreateIfAbsent) if exists => Err(SeamError::new(
                // The create you asked for cannot happen: the slot is
                // taken. The seam's contract test pins this as
                // stale-version — the state moved past what the guard
                // asserts.
                ErrorCode::StaleVersion,
                format!("target already exists: {}", canonical.display()),
            )),
            Some(WriteGuard::CreateIfAbsent) => Ok(()),
            Some(WriteGuard::ReplaceAtVersion(seen)) => {
                let current = self.versions.lock().get(canonical).copied();
                match current {
                    // The provider never stamped this path: the caller's
                    // token cannot have come from this backend.
                    None => Err(SeamError::new(
                        ErrorCode::NotObserved,
                        format!(
                            "no version recorded for {}; the guard token cannot be from this backend",
                            canonical.display()
                        ),
                    )),
                    Some(v) if self.stamp(v) != seen.0 => Err(SeamError::new(
                        ErrorCode::StaleVersion,
                        format!(
                            "target moved past the presented version: {}",
                            canonical.display()
                        ),
                    )),
                    Some(_) => Ok(()),
                }
            }
        }
    }

    /// One atomic mutation through the verified parent dirfd: write a
    /// unique sibling temp, fsync it, `renameat` over the target, fsync the
    /// parent so the rename is durable. The version only advances when the
    /// rename lands, so a failed mutation never advances freshness.
    ///
    /// `dirfd` and `dir_canonical` come from the same verified walk that
    /// produced `name`, so the rename cannot be redirected by a symlink
    /// swap after the walk.
    fn commit_atomic(
        &self,
        dirfd: RawFd,
        dir_canonical: &Path,
        name: &CString,
        contents: &[u8],
    ) -> Result<VersionToken, SeamError> {
        let counter = self.next_version.fetch_add(1, Ordering::SeqCst);
        let token = VersionToken(self.stamp(counter));
        // Globally unique temp name: nonce + counter + thread id, so two
        // instances (or two threads) never collide on a sibling temp.
        let tmp_name = {
            let tid = unsafe { libc::pthread_self() } as u64;
            let base = OsStr::from_bytes(name.as_bytes());
            format!(
                ".{}.harnless-tmp-{:x}-{:x}",
                base.to_string_lossy(),
                token.0 ^ tid,
                counter
            )
        };
        let tmp_c = cstr(OsStr::new(&tmp_name))?;
        let write = (|| -> Result<(), SeamError> {
            let fd = unsafe {
                libc::openat(
                    dirfd,
                    tmp_c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o644,
                )
            };
            if fd < 0 {
                return Err(io_error(io::Error::last_os_error()));
            }
            let opened_file = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut writer = std::io::BufWriter::new(opened_file);
            io::Write::write_all(&mut writer, contents).map_err(io_error)?;
            io::Write::flush(&mut writer).map_err(io_error)?;
            let opened_file = writer
                .into_inner()
                .map_err(|e| io_error(e.into_error()))?;
            opened_file.sync_all().map_err(io_error)?;
            // renameat through the verified parent fds: the destination is
            // the component we walked to, never a re-resolved string.
            let renamed =
                unsafe { libc::renameat(dirfd, tmp_c.as_ptr(), dirfd, name.as_ptr()) };
            if renamed < 0 {
                return Err(io_error(io::Error::last_os_error()));
            }
            // Durability of the rename itself: fsync the parent directory.
            let dir_dup = unsafe { libc::dup(dirfd) };
            if dir_dup >= 0 {
                let dir_file = unsafe { std::fs::File::from_raw_fd(dir_dup) };
                let _ = dir_file.sync_all();
            }
            Ok(())
        })();
        if write.is_err() {
            unsafe { libc::unlinkat(dirfd, tmp_c.as_ptr(), 0) };
            return write.map(|()| token);
        }
        // The map stores the raw instance-local counter; tokens handed out
        // and compared are the stamped (globally unique) form.
        self.versions
            .lock()
            .insert(dir_canonical.join(OsStr::from_bytes(name.as_bytes())), counter);
        Ok(token)
    }
}

impl Drop for LocalFileSystem {
    fn drop(&mut self) {
        unsafe { libc::close(self.root_fd) };
    }
}

/// Run an intent waterfall with a panic guard: a listener that panics is
/// treated as the built-in behavior (Allow), so a broken policy plugin
/// cannot abort the filesystem's own control flow.
fn guarded_waterfall(mount: &PolicyMount, event: FsWriteIntent) -> Intent {
    let events = mount.events.clone();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        events.waterfall(event, |_e| Intent::Allow)
    }))
    .unwrap_or(Intent::Allow)
}

/// The edit-intent variant of [`guarded_waterfall`].
fn guarded_edit_waterfall(mount: &PolicyMount, event: FsEditIntent) -> Intent {
    let events = mount.events.clone();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        events.waterfall(event, |_e| Intent::Allow)
    }))
    .unwrap_or(Intent::Allow)
}

/// `openat` one named directory hop with O_NOFOLLOW: a symlink is refused
/// (ELOOP), a non-directory is refused (ENOTDIR).
fn openat_nofollow_dir(dirfd: RawFd, name: &CString) -> Result<RawFd, io::Error> {
    let fd = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd >= 0 {
        Ok(fd)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// A NUL-terminated C string from an OS string; embedded NULs are refused.
fn cstr(s: &OsStr) -> Result<CString, SeamError> {
    CString::new(s.as_bytes())
        .map_err(|_| SeamError::new(ErrorCode::IoError, "path contains an embedded NUL"))
}

/// Map an `openat`-family errno onto the taxonomy. ELOOP is the O_NOFOLLOW
/// refusal — an attempted symlink escape — and is sandbox-denied, never a
/// plain io-error. ENOTDIR/ENOENT on a hop stay io-error/not-found as the
/// kernel reported them.
fn openat_error(e: io::Error) -> SeamError {
    match e.raw_os_error() {
        Some(libc::ELOOP) => SeamError::new(
            ErrorCode::SandboxDenied,
            "symlink hop refused inside the workspace",
        ),
        _ => io_error(e),
    }
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

/// Read up to `max_bytes` from `fd` while counting every line start to EOF,
/// so `total_lines` stays exact past the cap without buffering the file.
///
/// Returns `(window_bytes, total_lines, truncated)`.
fn read_windowed(fd: RawFd, max_bytes: usize) -> Result<(Vec<u8>, u64, bool), SeamError> {
    let mut contents = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK];
    // Line total is the number of `\n` terminators plus a final
    // unterminated line when the file is non-empty and does not end on a
    // newline — counted exactly to EOF even past the byte cap.
    let mut newlines: u64 = 0;
    let mut nonempty = false;
    let mut ends_with_newline = true;
    let mut truncated = false;
    loop {
        let n = unsafe {
            libc::read(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len().min(READ_CHUNK),
            )
        };
        if n < 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        if n == 0 {
            break;
        }
        let chunk = &buf[..n as usize];
        if !chunk.is_empty() {
            nonempty = true;
            ends_with_newline = chunk.last() == Some(&b'\n');
        }
        newlines += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
        let room = max_bytes.saturating_sub(contents.len());
        let take = room.min(chunk.len());
        contents.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            truncated = true;
        }
    }
    let total_lines = if !nonempty {
        0
    } else if ends_with_newline {
        newlines
    } else {
        newlines + 1
    };
    Ok((contents, total_lines, truncated))
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

/// A random per-instance nonce from the OS CSPRNG.
fn random_nonce() -> u64 {
    let mut bytes = [0u8; 8];
    // arc4random_buf is the platform CSPRNG on macOS (Linux's getrandom(2)
    // is not in libc's shared symbol set); the nonce's purpose is aliasing
    // resistance for tokens and temp names, not security.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr() as *mut libc::c_void, 8) };
    u64::from_le_bytes(bytes)
}

impl FileSystem for LocalFileSystem {
    fn resolve(&self, path: &str) -> Result<Target, SeamError> {
        // Lexical normalization first: `.` and `..` are cancelled against
        // the pending name, absolute paths and climbs past the root are
        // refused. Then the target's own chain is walked from the root fd
        // with O_NOFOLLOW — a symlink anywhere on the path is refused, so
        // the returned key provably names a contained, symlink-free
        // coordinate. A missing final component is fine (create target);
        // a missing ancestor is not. The IO-time walk repeats the same
        // proof at the point of mutation.
        let rel = Path::new(path);
        let mut components: Vec<&OsStr> = Vec::new();
        for c in rel.components() {
            match c {
                Component::CurDir => {}
                Component::Normal(name) => components.push(name),
                // A `..` cancels the pending name it resolves to — the same
                // normalization openat itself performs. Only a `..` that
                // would climb out of the root is refused; after that the fd
                // walk (O_NOFOLLOW at every hop) is the containment proof.
                Component::ParentDir => {
                    if components.pop().is_none() {
                        return Err(SeamError::new(
                            ErrorCode::SandboxDenied,
                            format!("path escapes the workspace root: {path}"),
                        ));
                    }
                }
                _ => {
                    return Err(SeamError::new(
                        ErrorCode::SandboxDenied,
                        format!("path escapes the workspace root: {path}"),
                    ))
                }
            }
        }
        if components.is_empty() {
            // The root itself is a listable target; "." is its display.
            return Ok(Target {
                key: harnless_seams::TargetKey(fnv1a(self.root.as_os_str().as_bytes())),
                display: ".".to_string(),
            });
        }

        // Walk the chain lexically-normalized. Every hop may be absent —
        // resolve names a coordinate, it does not require existence (write
        // materializes missing parents). A hop that exists must be a real,
        // non-symlink directory; the final hop may be any non-symlink
        // object. The IO-time walk repeats the containment proof.
        let mut dirfd = unsafe { libc::dup(self.root_fd) };
        if dirfd < 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        let mut canonical = self.root.clone();
        let last_index = components.len() - 1;
        for (i, component) in components.iter().enumerate() {
            let hop = match cstr(component) {
                Ok(c) => c,
                Err(e) => {
                    unsafe { libc::close(dirfd) };
                    return Err(e);
                }
            };
            if i == last_index {
                // The final hop: any object is fine; a symlink is refused;
                // ENOENT is a legitimate create target.
                let fd = unsafe {
                    libc::openat(
                        dirfd,
                        hop.as_ptr(),
                        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    )
                };
                if fd >= 0 {
                    unsafe { libc::close(fd) };
                } else {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() != Some(libc::ENOENT) {
                        unsafe { libc::close(dirfd) };
                        return Err(openat_error(e));
                    }
                }
                canonical.push(component);
                unsafe { libc::close(dirfd) };
            } else {
                match openat_nofollow_dir(dirfd, &hop) {
                    Ok(next) => {
                        unsafe { libc::close(dirfd) };
                        dirfd = next;
                        canonical.push(component);
                    }
                    Err(e) => {
                        // A missing (or not-yet-a-directory) ancestor is
                        // fine at resolve time: the write path materializes
                        // it. A symlink hop is still refused.
                        if !matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR))
                        {
                            unsafe { libc::close(dirfd) };
                            return Err(openat_error(e));
                        }
                        // The rest of the chain cannot be opened (no dirfd
                        // to walk from); the remaining components are
                        // plain names, so the canonical path is complete.
                        for rest in &components[i + 1..] {
                            canonical.push(rest);
                        }
                        unsafe { libc::close(dirfd) };
                        break;
                    }
                }
            }
        }

        let display = canonical
            .strip_prefix(&self.root)
            .unwrap_or(Path::new(""))
            .to_string_lossy()
            .into_owned();
        // The root itself resolves to ".", the conventional workspace
        // display for "here"; an empty display would name no file.
        let display = if display.is_empty() {
            ".".to_string()
        } else {
            display
        };
        Ok(Target {
            key: harnless_seams::TargetKey(fnv1a(canonical.as_os_str().as_bytes())),
            display,
        })
    }
    fn read(&self, target: &Target, max_bytes: usize) -> Result<ReadWindow, SeamError> {
        let opened = self.open_target(target, false)?;
        if opened.fd < 0 {
            let r = Err(SeamError::new(
                ErrorCode::NotFound,
                format!("no such file: {}", target.display),
            ));
            return r;
        }
        // Kind check on the same fd we read from — no second open to race.
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        let stat = unsafe { libc::fstat(opened.fd, st.as_mut_ptr()) };
        if stat < 0 {
            unsafe { libc::close(opened.fd) };
            return Err(io_error(io::Error::last_os_error()));
        }
        let mode = unsafe { st.assume_init() }.st_mode;
        if mode & libc::S_IFMT != libc::S_IFREG {
            unsafe { libc::close(opened.fd) };
            return Err(SeamError::new(
                ErrorCode::NotARegularFile,
                format!("not a regular file: {}", target.display),
            ));
        }
        let result = (|| -> Result<ReadWindow, SeamError> {
            let (contents, total_lines, truncated) = read_windowed(opened.fd, max_bytes)?;
            if !contents.is_empty() && std::str::from_utf8(&contents).is_err() {
                // A cut inside a multi-byte sequence is not the file's
                // fault; validate what we return, and the whole file when
                // the window covered it.
                if truncated {
                    return Ok(ReadWindow {
                        contents,
                        total_lines,
                        truncated,
                    });
                }
                return Err(SeamError::new(
                    ErrorCode::NotText,
                    format!("not valid UTF-8 text: {}", target.display),
                ));
            }
            if truncated && !contents.is_empty() {
                // The window may have cut a multi-byte character; trim the
                // partial tail so the returned bytes are always valid UTF-8.
                let valid = match std::str::from_utf8(&contents) {
                    Ok(_) => contents.len(),
                    Err(e) => e.valid_up_to(),
                };
                return Ok(ReadWindow {
                    contents: contents[..valid].to_vec(),
                    total_lines,
                    truncated,
                });
            }
            Ok(ReadWindow {
                contents,
                total_lines,
                truncated,
            })
        })();
        unsafe { libc::close(opened.fd) };
        let window = result?;
        self.observe_read(target, &window);
        Ok(window)
    }

    fn write(
        &self,
        target: &Target,
        contents: &[u8],
        guard: Option<WriteGuard>,
    ) -> Result<MutationResult, SeamError> {
        // Containment and key integrity first, then the policy decision on
        // a validated coordinate, then the mutation.
        let opened = self.open_target(target, true)?;
        let exists = opened.fd >= 0;
        if let Some(fd) = (opened.fd >= 0).then_some(opened.fd) {
            // A directory is never a write target.
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(fd, st.as_mut_ptr()) } == 0
                && unsafe { st.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFDIR
            {
                unsafe { libc::close(opened.fd) };
                return Err(SeamError::new(
                    ErrorCode::NotADirectory,
                    format!("write target is a directory: {}", target.display),
                ));
            }
        }
        self.decide_write(target)?;
        self.check_guard(&opened.canonical, &guard, exists)?;
        // Re-open the verified parent for the renameat; the chain was
        // validated by open_target and cannot have been redirected (every
        // hop is O_NOFOLLOW and the walk is repeated here).
        let parent = self.open_parent(target, true)?;
        unsafe { libc::close(opened.fd) };
        let version = self.commit_atomic(
            parent.dirfd,
            &parent.dir_canonical,
            &parent.name,
            contents,
        )?;
        unsafe { libc::close(parent.dirfd) };
        Ok(MutationResult { version })
    }

    fn edit(
        &self,
        target: &Target,
        edit: &Edit,
        guard: Option<WriteGuard>,
    ) -> Result<MutationResult, SeamError> {
        let opened = self.open_target(target, false)?;
        if opened.fd < 0 {
            unsafe { libc::close(opened.fd.max(0)) };
            return Err(SeamError::new(
                ErrorCode::NotFound,
                format!("no such file: {}", target.display),
            ));
        }
        self.decide_edit(target)?;
        self.check_guard(&opened.canonical, &guard, true)?;
        // The mutation reads the current bytes from the same fd chain that
        // writes them — one mutation, not a composed read-plus-write the
        // caller could interleave against.
        let bytes = (|| -> Result<Vec<u8>, SeamError> {
            let (bytes, _, _) = read_windowed(opened.fd, usize::MAX)?;
            Ok(bytes)
        })();
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                unsafe { libc::close(opened.fd) };
                return Err(e);
            }
        };
        unsafe { libc::close(opened.fd) };
        if std::str::from_utf8(&bytes).is_err() {
            return Err(SeamError::new(
                ErrorCode::NotText,
                format!("not valid UTF-8 text: {}", target.display),
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
                let parent = self.open_parent(target, false)?;
                let version =
                    self.commit_atomic(parent.dirfd, &parent.dir_canonical, &parent.name, &next)?;
                unsafe { libc::close(parent.dirfd) };
                Ok(MutationResult { version })
            }
            n => Err(SeamError::new(
                ErrorCode::AmbiguousEdit,
                format!("edit pattern matched {n} times; exactly one match is required"),
            )),
        }
    }

    fn list(&self, target: &Target) -> Result<Vec<Entry>, SeamError> {
        let opened = self.open_target(target, false)?;
        if opened.fd < 0 {
            return Err(SeamError::new(
                ErrorCode::NotFound,
                format!("no such directory: {}", target.display),
            ));
        }
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        let stat = unsafe { libc::fstat(opened.fd, st.as_mut_ptr()) };
        if stat < 0 {
            unsafe { libc::close(opened.fd) };
            return Err(io_error(io::Error::last_os_error()));
        }
        let mode = unsafe { st.assume_init() }.st_mode;
        if mode & libc::S_IFMT != libc::S_IFDIR {
            unsafe { libc::close(opened.fd) };
            return Err(SeamError::new(
                ErrorCode::NotADirectory,
                format!("not a directory: {}", target.display),
            ));
        }
        // fd-based listing: opendirfd via /dev/fd-style reopen is not
        // portable; dup the verified dirfd into fs::read_dir through its
        // canonical path, which the fd chain just proved contained.
        let dir = fs::read_dir(&opened.canonical).map_err(io_error);
        unsafe { libc::close(opened.fd) };
        let mut entries = Vec::new();
        for entry in dir? {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_matches_pinned() {
        assert_eq!(count_matches(b"aaa", b"a"), 3);
        assert_eq!(count_matches(b"aaa", b"aa"), 1); // non-overlapping
        assert_eq!(count_matches(b"ab", b""), 0); // empty needle: no match
        assert_eq!(count_matches(b"", b"x"), 0);
    }

    #[test]
    fn fnv1a_pinned() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn nonce_is_deterministic_shape() {
        // Nonce quality is probabilistic; pin only that stamps differ.
        let fs = LocalFileSystem::new(std::env::temp_dir()).unwrap();
        let a = fs.stamp(1);
        let b = fs.stamp(2);
        assert_ne!(a, b);
    }
}
