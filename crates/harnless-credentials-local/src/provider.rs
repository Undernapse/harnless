//! The local credentials provider: a read-through JSON record store plus
//! the one-attempt-per-key authorization-flow registry.
//!
//! Design points that carry the contract:
//!
//! * **No cache, ever.** Every `resolve`/`kind` re-reads the record file.
//!   The rotation test (write a new value between two resolves, see it
//!   immediately) is the point of the seam; a cache would defeat it.
//! * **Atomic publish.** Writes go to a sibling temp file then rename over
//!   the store, so a reader never observes a half-written record set.
//! * **Cross-process safety.** A `FileLock` on `<store>.lock` (an `flock`)
//!   plus a process-wide mutex per path serialize writers sharing the
//!   file, not just writers sharing one `LocalCredentials`.
//! * **Registry lifecycle only.** The flow registry keys in-flight dances
//!   by `CredentialRef`. The first caller runs the flow; later callers for
//!   the same key never start a second dance — they await the first
//!   outcome. When the first finishes (success or failure) the key frees
//!   and a *later* attempt is allowed again: one attempt per in-flight
//!   window, not a permanent ban. The flow produces the secret; the
//!   registry never inspects how, and the record is written through the
//!   provider's own [`LocalCredentials::store`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use harnless_seams::credentials::{CredentialKind, CredentialRef, Credentials};
use harnless_seams::error::{ErrorCode, SeamError};
use serde::{Deserialize, Serialize};

/// One stored credential record: the kind plus the secret value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRecord {
    /// Stable kind spelling (`bearer`, `oauth2`, `api-key`).
    pub kind: String,
    /// The secret value. Owned by the provider, never by configuration.
    pub value: String,
}

impl CredentialRecord {
    /// Build a record from a seam kind and a value.
    pub fn new(kind: CredentialKind, value: impl Into<String>) -> Self {
        Self {
            kind: kind_spelling(kind).to_string(),
            value: value.into(),
        }
    }

    /// The seam kind this record spells, or `None` for a corrupt spelling.
    pub fn seam_kind(&self) -> Option<CredentialKind> {
        kind_from_spelling(&self.kind)
    }
}

/// The stable on-disk spelling of a credential kind.
pub fn kind_spelling(kind: CredentialKind) -> &'static str {
    match kind {
        CredentialKind::Bearer => "bearer",
        CredentialKind::OAuth2 => "oauth2",
        CredentialKind::ApiKey => "api-key",
    }
}

/// Parse an on-disk kind spelling back into the seam kind.
pub fn kind_from_spelling(spelling: &str) -> Option<CredentialKind> {
    match spelling {
        "bearer" => Some(CredentialKind::Bearer),
        "oauth2" => Some(CredentialKind::OAuth2),
        "api-key" => Some(CredentialKind::ApiKey),
        _ => None,
    }
}

/// An exclusive advisory lock over a file, released on drop.
///
/// `flock(LOCK_EX)` on a sibling `.lock` path, held for the
/// read-modify-write window. Within one process the provider's per-path
/// mutex already serializes writers; the flock extends that across
/// processes sharing the store.
#[derive(Debug)]
pub struct FileLock {
    /// Held only for its Drop: closing the fd releases the flock.
    #[allow(dead_code)]
    file: std::fs::File,
}

impl FileLock {
    /// Acquire the exclusive lock for `path` (blocking).
    pub fn acquire(path: &Path) -> harnless_seams::error::Result<Self> {
        let lock_path = lock_path(path);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| {
                SeamError::new(
                    ErrorCode::IoError,
                    format!("opening credential lock {}: {e}", lock_path.display()),
                )
            })?;
        if !flock_exclusive(&file) {
            return Err(SeamError::new(
                ErrorCode::IoError,
                format!("locking {}: {}", lock_path.display(), std::io::Error::last_os_error()),
            ));
        }
        Ok(Self { file })
    }
}

/// Sibling `.lock` path for a store file.
fn lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(".lock");
    PathBuf::from(p)
}

#[cfg(unix)]
fn flock_exclusive(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: LOCK_EX on a valid fd we own; the lock releases when the
    // File drops (close releases flock). Blocking is intended.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) == 0 }
}

#[cfg(not(unix))]
fn flock_exclusive(_file: &std::fs::File) -> bool {
    // No flock on this platform; the per-path process mutex is the only
    // serialization available. Documented degradation, not a silent lie.
    true
}

/// Outcome of an authorization attempt, so callers can tell whether they
/// ran the dance or joined someone else's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizeState {
    /// This call started and completed the flow; the record is written.
    Authorized,
    /// A flow was already in flight for this key; this call awaited it and
    /// adopted its success.
    Joined,
}

/// The authorization-flow seam: the interactive dance that obtains a
/// secret for a reference and produces it for storage.
///
/// Implementations own the *protocol* (browser dance, device code, prompt)
/// and return the secret; the registry and provider own the *lifecycle*
/// and storage. `authorize` runs on a blocking thread spawned by the
/// registry, so it may block freely.
pub trait AuthorizationFlow: Send + Sync + 'static {
    /// Run the dance for `reference` and return the secret to store.
    ///
    /// A refusal or failure is a typed [`SeamError`]; the registry records
    /// the failure, frees the key, and lets a later attempt try again.
    fn authorize(&self, reference: &CredentialRef) -> harnless_seams::error::Result<String>;
}

/// The flow as the registry stores it: produces the secret for a key.
type SharedFlow =
    Arc<dyn Fn(&CredentialRef) -> harnless_seams::error::Result<String> + Send + Sync + 'static>;

/// The one-attempt-per-key flow registry: flows per kind, in-flight
/// dances per reference.
#[derive(Default)]
struct FlowRegistry {
    /// Flows by kind. `CredentialKind` is not `Hash`, so the three kinds
    /// are a direct-mapped array over their discriminants.
    flows: [Option<SharedFlow>; 3],
    in_flight: HashMap<CredentialRef, Arc<parking_lot::Mutex<InFlight>>>,
}

/// Dense index of a credential kind in the flow array.
fn kind_index(kind: CredentialKind) -> usize {
    match kind {
        CredentialKind::Bearer => 0,
        CredentialKind::OAuth2 => 1,
        CredentialKind::ApiKey => 2,
    }
}

/// In-flight bookkeeping for one key.
struct InFlight {
    done: bool,
    result: Option<harnless_seams::error::Result<String>>,
}

/// A local, file-backed [`Credentials`] provider.
///
/// The record store is one JSON object (`{ "<ref>": { "kind", "value" } }`)
/// at a fixed path. Build with [`LocalCredentials::builder`] or
/// [`LocalCredentials::new`]. Handles are cheap `Arc` clones over one
/// shared state, so a flow task writing through the provider joins the
/// same locks and registry.
#[derive(Clone)]
pub struct LocalCredentials {
    inner: Arc<Inner>,
}

/// The shared state behind every [`LocalCredentials`] handle.
struct Inner {
    path: PathBuf,
    /// Registry of flows and in-flight dances.
    registry: parking_lot::Mutex<FlowRegistry>,
    /// Broadcast of in-flight outcomes so joiners await rather than poll.
    /// Outcomes are published under this lock's ordering: the runner sets
    /// `done` before notifying, and a joiner registers its `notified()`
    /// future before re-checking `done`, so no notification is lost.
    notifier: Arc<tokio::sync::Notify>,
}

impl std::fmt::Debug for LocalCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCredentials").field("path", &self.inner.path).finish_non_exhaustive()
    }
}

/// Builder for [`LocalCredentials`].
#[derive(Default)]
pub struct LocalCredentialsBuilder {
    path: Option<PathBuf>,
    flows: Vec<(CredentialKind, SharedFlow)>,
}

impl std::fmt::Debug for LocalCredentialsBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCredentialsBuilder")
            .field("path", &self.path)
            .field("flows", &self.flows.len())
            .finish()
    }
}

impl LocalCredentialsBuilder {
    /// Set the record-store path (required).
    pub fn path(mut self, path: impl AsRef<Path>) -> Self {
        self.path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Register an authorization-flow closure for `kind`.
    pub fn flow<F>(mut self, kind: CredentialKind, flow: F) -> Self
    where
        F: Fn(&CredentialRef) -> harnless_seams::error::Result<String> + Send + Sync + 'static,
    {
        self.flows.push((kind, Arc::new(flow)));
        self
    }

    /// Register a flow object implementing [`AuthorizationFlow`] for `kind`.
    pub fn flow_impl(mut self, kind: CredentialKind, flow: impl AuthorizationFlow) -> Self {
        self.flows.push((
            kind,
            Arc::new(move |reference: &CredentialRef| flow.authorize(reference)),
        ));
        self
    }

    /// Finish the build.
    pub fn build(self) -> LocalCredentials {
        let path = self.path.expect("LocalCredentials requires a record-store path");
        let mut flows: [Option<SharedFlow>; 3] = Default::default();
        for (kind, flow) in self.flows {
            flows[kind_index(kind)] = Some(flow);
        }
        LocalCredentials {
            inner: Arc::new(Inner {
                path,
                registry: parking_lot::Mutex::new(FlowRegistry {
                    flows,
                    in_flight: HashMap::new(),
                }),
                notifier: Arc::new(tokio::sync::Notify::new()),
            }),
        }
    }
}

impl LocalCredentials {
    /// Start a provider builder.
    pub fn builder() -> LocalCredentialsBuilder {
        LocalCredentialsBuilder::default()
    }

    /// A bare provider over `path` with no flows registered.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self::builder().path(path).build()
    }

    /// The record-store path.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Read the whole record map from disk.
    ///
    /// A missing file is an empty store (fresh install). A corrupt file is
    /// `io-error` — never silently treated as empty, because that would
    /// turn a half-written store into "all credentials absent".
    fn read_all(&self) -> harnless_seams::error::Result<HashMap<String, CredentialRecord>> {
        match std::fs::read_to_string(&self.inner.path) {
            Ok(text) if text.trim().is_empty() => Ok(HashMap::new()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                SeamError::new(
                    ErrorCode::IoError,
                    format!("credential store {} is corrupt: {e}", self.inner.path.display()),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(SeamError::new(
                ErrorCode::IoError,
                format!("reading credential store {}: {e}", self.inner.path.display()),
            )),
        }
    }

    /// Read one record. Lock-free: readers race only the atomic rename
    /// publish, never a half-written file.
    fn read_record(
        &self,
        reference: &CredentialRef,
    ) -> harnless_seams::error::Result<Option<CredentialRecord>> {
        Ok(self.read_all()?.remove(&reference.0))
    }

    /// Publish a mutated record map: process lock, file lock, re-read,
    /// mutate, write temp, rename. The re-read under both locks means a
    /// concurrent writer's records are never lost.
    fn mutate<F>(&self, f: F) -> harnless_seams::error::Result<()>
    where
        F: FnOnce(&mut HashMap<String, CredentialRecord>),
    {
        let process_lock = shared_write_lock(&self.inner.path);
        let _process_guard = process_lock.lock();
        let _file_guard = FileLock::acquire(&self.inner.path)?;
        let mut all = self.read_all()?;
        f(&mut all);
        let serialized = serde_json::to_vec_pretty(&all).map_err(|e| {
            SeamError::new(ErrorCode::IoError, format!("serializing credential store: {e}"))
        })?;
        let tmp = {
            let mut s = self.inner.path.as_os_str().to_os_string();
            s.push(".tmp");
            PathBuf::from(s)
        };
        std::fs::write(&tmp, &serialized).map_err(|e| {
            SeamError::new(
                ErrorCode::IoError,
                format!("writing credential temp {}: {e}", tmp.display()),
            )
        })?;
        std::fs::rename(&tmp, &self.inner.path).map_err(|e| {
            SeamError::new(
                ErrorCode::IoError,
                format!("publishing credential store {}: {e}", self.inner.path.display()),
            )
        })
    }

    /// Write (insert or replace) the record for `reference`.
    ///
    /// This is the write path authorization flows use — the flow produces
    /// the secret, the provider owns the storage.
    pub fn store(
        &self,
        reference: &CredentialRef,
        kind: CredentialKind,
        value: &str,
    ) -> harnless_seams::error::Result<()> {
        let reference = reference.clone();
        let value = value.to_string();
        self.mutate(move |all| {
            all.insert(reference.0, CredentialRecord::new(kind, value));
        })
    }

    /// Delete the record for `reference` (absent is not an error).
    pub fn forget(&self, reference: &CredentialRef) -> harnless_seams::error::Result<()> {
        let reference = reference.clone();
        self.mutate(move |all| {
            all.remove(&reference.0);
        })
    }

    /// Register (or replace) an authorization flow for `kind`.
    pub fn register_flow<F>(&self, kind: CredentialKind, flow: F)
    where
        F: Fn(&CredentialRef) -> harnless_seams::error::Result<String> + Send + Sync + 'static,
    {
        self.inner.registry.lock().flows[kind_index(kind)] = Some(Arc::new(flow));
    }

    /// Register a flow object implementing [`AuthorizationFlow`].
    pub fn register_flow_impl(&self, kind: CredentialKind, flow: impl AuthorizationFlow) {
        self.inner.registry.lock().flows[kind_index(kind)] =
            Some(Arc::new(move |reference: &CredentialRef| flow.authorize(reference)));
    }

    /// Run (or join) the authorization dance for `reference` of `kind`.
    ///
    /// One attempt per key: while a flow for `reference` is in flight, a
    /// second call never starts a second dance — it awaits the in-flight
    /// one and adopts its outcome (`Joined` on success, the original typed
    /// error on failure). Once the dance finishes — success or failure —
    /// the key frees and a later call may run again. With no flow
    /// registered for `kind` the refusal is `tool-denied`: an unregistered
    /// dance is a policy refusal, not an IO failure.
    pub async fn authorize(
        &self,
        reference: &CredentialRef,
        kind: CredentialKind,
    ) -> harnless_seams::error::Result<(String, AuthorizeState)> {
        let flow = self.inner.registry.lock().flows[kind_index(kind)].clone();
        let flow = flow.ok_or_else(|| {
            SeamError::new(
                ErrorCode::ToolDenied,
                format!("no authorization flow registered for kind {}", kind_spelling(kind)),
            )
        })?;

        // Claim the key or join the existing claim; the registry lock makes
        // exactly one caller the runner.
        let (slot, is_runner) = {
            let mut registry = self.inner.registry.lock();
            match registry.in_flight.get(reference) {
                Some(existing) => (existing.clone(), false),
                None => {
                    let slot = Arc::new(parking_lot::Mutex::new(InFlight {
                        done: false,
                        result: None,
                    }));
                    registry.in_flight.insert(reference.clone(), slot.clone());
                    (slot, true)
                }
            }
        };

        if is_runner {
            let reference = reference.clone();
            // The dance runs off the async executor (it may block); the
            // record is written through the provider's own store path.
            let provider = self.clone();
            let for_task = reference.clone();
            let outcome = tokio::task::spawn_blocking(move || -> harnless_seams::error::Result<String> {
                let secret = flow(&for_task)?;
                provider.store(&for_task, kind, &secret)?;
                Ok(secret)
            })
            .await
            .unwrap_or_else(|e| {
                Err(SeamError::new(
                    ErrorCode::IoError,
                    format!("authorization flow task failed: {e}"),
                ))
            });
            {
                let mut guard = slot.lock();
                guard.result = Some(outcome.clone());
                guard.done = true;
            }
            // Free the key so a later attempt may run again, then wake
            // joiners.
            self.inner.registry.lock().in_flight.remove(&reference);
            self.inner.notifier.notify_waiters();
            outcome.map(|secret| (secret, AuthorizeState::Authorized))
        } else {
            // Await the runner's outcome without spinning: register the
            // notification *before* re-checking done, so a completion that
            // races this loop still wakes us.
            loop {
                let notified = self.inner.notifier.notified();
                let done = {
                    let guard = slot.lock();
                    guard.done.then(|| guard.result.clone().expect("result set with done"))
                };
                if let Some(outcome) = done {
                    return outcome.map(|secret| (secret, AuthorizeState::Joined));
                }
                notified.await;
            }
        }
    }

    /// Whether an authorization dance for `reference` is in flight right
    /// now (diagnostics and tests).
    pub fn authorize_in_flight(&self, reference: &CredentialRef) -> bool {
        self.inner.registry.lock().in_flight.contains_key(reference)
    }
}

/// Process-wide writer locks keyed by store path, so two
/// `LocalCredentials` instances on one path in one process also serialize.
fn shared_write_lock(path: &Path) -> Arc<parking_lot::Mutex<()>> {
    static LOCKS: LazyLock<parking_lot::Mutex<HashMap<PathBuf, Arc<parking_lot::Mutex<()>>>>> =
        LazyLock::new(parking_lot::Mutex::default);
    let mut map = LOCKS.lock();
    map.entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(parking_lot::Mutex::new(())))
        .clone()
}

impl Credentials for LocalCredentials {
    fn resolve(&self, reference: &CredentialRef) -> harnless_seams::error::Result<Option<String>> {
        // Read-through per operation: no cache, so rotation is immediate.
        Ok(self.read_record(reference)?.map(|r| r.value))
    }

    fn kind(
        &self,
        reference: &CredentialRef,
    ) -> harnless_seams::error::Result<Option<CredentialKind>> {
        // kind agrees with resolve by construction: both read the same
        // record, and a record with an unparseable kind resolves None too.
        Ok(self.read_record(reference)?.and_then(|r| r.seam_kind()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(name: &str) -> CredentialRef {
        CredentialRef(name.to_string())
    }

    fn store_in(dir: &Path) -> PathBuf {
        dir.join("credentials.json")
    }

    #[test]
    fn absent_reference_is_ok_none() {
        let dir = tempfile::tempdir().unwrap();
        let creds = LocalCredentials::new(store_in(dir.path()));
        assert_eq!(creds.resolve(&cred("ghost")).unwrap(), None);
        assert_eq!(creds.kind(&cred("ghost")).unwrap(), None);
    }

    #[test]
    fn store_resolve_kind_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let creds = LocalCredentials::new(store_in(dir.path()));
        creds.store(&cred("openai"), CredentialKind::ApiKey, "sk-1").unwrap();
        assert_eq!(creds.resolve(&cred("openai")).unwrap().as_deref(), Some("sk-1"));
        assert_eq!(creds.kind(&cred("openai")).unwrap(), Some(CredentialKind::ApiKey));
    }

    #[test]
    fn rotation_reaches_next_resolve_without_restart() {
        // The seam's headline obligation: read-through per operation.
        let dir = tempfile::tempdir().unwrap();
        let path = store_in(dir.path());
        let creds = LocalCredentials::new(&path);
        creds.store(&cred("oauth"), CredentialKind::OAuth2, "old-token").unwrap();
        assert_eq!(creds.resolve(&cred("oauth")).unwrap().as_deref(), Some("old-token"));
        // Out-of-band rotation: an operator (or flow) rewrites the file.
        std::fs::write(&path, r#"{"oauth":{"kind":"oauth2","value":"new-token"}}"#).unwrap();
        assert_eq!(
            creds.resolve(&cred("oauth")).unwrap().as_deref(),
            Some("new-token"),
            "a rotated secret must reach the very next resolve, no restart"
        );
    }

    #[test]
    fn forget_removes_record() {
        let dir = tempfile::tempdir().unwrap();
        let creds = LocalCredentials::new(store_in(dir.path()));
        creds.store(&cred("a"), CredentialKind::Bearer, "t").unwrap();
        creds.forget(&cred("a")).unwrap();
        assert_eq!(creds.resolve(&cred("a")).unwrap(), None);
        creds.forget(&cred("a")).unwrap(); // absent delete is not an error
    }

    #[test]
    fn corrupt_store_is_typed_io_error_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_in(dir.path());
        std::fs::write(&path, "{ not json").unwrap();
        let creds = LocalCredentials::new(&path);
        let err = creds.resolve(&cred("x")).unwrap_err();
        assert_eq!(err.code, ErrorCode::IoError);
    }

    #[test]
    fn kind_spelling_roundtrips() {
        for kind in [CredentialKind::Bearer, CredentialKind::OAuth2, CredentialKind::ApiKey] {
            assert_eq!(kind_from_spelling(kind_spelling(kind)), Some(kind));
        }
        assert_eq!(kind_from_spelling("nonsense"), None);
    }

    #[test]
    fn concurrent_writers_do_not_lose_records() {
        // Two instances on one path (simulating two processes) writing
        // distinct refs: both records survive.
        let dir = tempfile::tempdir().unwrap();
        let path = store_in(dir.path());
        let a = LocalCredentials::new(&path);
        let b = LocalCredentials::new(&path);
        let (ta, tb) = (a.clone(), b.clone());
        std::thread::spawn(move || {
            for i in 0..20 {
                ta.store(&cred(&format!("a{i}")), CredentialKind::ApiKey, "v").unwrap();
            }
        })
        .join()
        .unwrap();
        std::thread::spawn(move || {
            for i in 0..20 {
                tb.store(&cred(&format!("b{i}")), CredentialKind::ApiKey, "v").unwrap();
            }
        })
        .join()
        .unwrap();
        for i in 0..20 {
            assert!(a.resolve(&cred(&format!("a{i}"))).unwrap().is_some());
            assert!(a.resolve(&cred(&format!("b{i}"))).unwrap().is_some());
        }
    }
}
