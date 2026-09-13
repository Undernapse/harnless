//! The JSONL storage hub: named append-only logs with replay, tombstones,
//! and the reference typed domain layer.
//!
//! Design points that carry the contract:
//!
//! * **One file per backend, side by side.** The root directory holds
//!   `<sanitized>.jsonl` per backend. A name is sanitized to
//!   `[A-Za-z0-9._-]`; a name with nothing left after sanitizing is refused
//!   (`not-found`) rather than mapped to a hidden default. Two backends
//!   whose names sanitize identically collide loudly on the same file —
//!   documented, since the hub's identity *is* the name.
//! * **Log replay is the read path.** `get` streams the file line by line
//!   and keeps the last operation for the key. Tombstones are entries too,
//!   so delete honesty falls out of ordering: last-op-wins.
//! * **Durability before return.** Appends use `O_APPEND` writes and are
//!   flushed+synced before the call returns; the rename-publish compaction
//!   is the only rewrite, and it re-checks for racing appends (compare file
//!   length to the snapshot, re-append the delta) before publishing.
//! * **The domain layer is opaque.** [`JsonlDomain`] wraps values in a
//!   versioned JSON envelope, base64-encoded into [`OpaqueUnit`](harnless_seams::storage::OpaqueUnit). The hub
//!   stores the envelope as a JSON string; nothing below the domain
//!   interprets it.

use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use harnless_seams::error::{ErrorCode, SeamError};
use harnless_seams::storage::{BackendName, OpaqueUnit, Storage, StorageDomain};
use serde_json::Value;

/// Name sanitization and file naming for backends.
pub trait BackendNameExt {
    /// The on-disk file name for this backend.
    fn file_name(&self) -> harnless_seams::error::Result<String>;
}

impl BackendNameExt for BackendName {
    fn file_name(&self) -> harnless_seams::error::Result<String> {
        let sanitized: String = self
            .0
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' })
            .collect();
        if sanitized.is_empty() || sanitized.chars().all(|c| c == '.') {
            // Empty, or nothing but dots (`.`, `..`, `///`) — a name that
            // cannot become a real file without aliasing the directory
            // itself or a hidden entry is refused, not silently remapped.
            return Err(SeamError::new(
                ErrorCode::NotFound,
                format!("backend name {:?} sanitizes to nothing usable", self.0),
            ));
        }
        Ok(format!("{sanitized}.jsonl"))
    }
}

/// One log entry: an operation plus its payload.
///
/// Wire form: `{"op":"set","key":"…","value":…}` or `{"op":"del","key":"…"}`.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Set { key: String, value: Value },
    Delete { key: String },
}

impl Entry {
    fn to_line(&self) -> harnless_seams::error::Result<String> {
        let obj = match self {
            Entry::Set { key, value } => serde_json::json!({ "op": "set", "key": key, "value": value }),
            Entry::Delete { key } => serde_json::json!({ "op": "del", "key": key }),
        };
        serde_json::to_string(&obj).map_err(|e| {
            SeamError::new(ErrorCode::IoError, format!("serializing storage entry: {e}"))
        })
    }

    fn from_line(line: &str, backend: &str, lineno: usize) -> harnless_seams::error::Result<Option<Self>> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let parsed: Value = serde_json::from_str(trimmed).map_err(|e| {
            SeamError::new(
                ErrorCode::IoError,
                format!("backend {backend} line {lineno} is not a valid log entry: {e}"),
            )
        })?;
        let op = parsed.get("op").and_then(Value::as_str).ok_or_else(|| {
            SeamError::new(
                ErrorCode::IoError,
                format!("backend {backend} line {lineno} has no 'op' field"),
            )
        })?;
        let key = parsed.get("key").and_then(Value::as_str).ok_or_else(|| {
            SeamError::new(
                ErrorCode::IoError,
                format!("backend {backend} line {lineno} has no string 'key'"),
            )
        })?;
        match op {
            "set" => {
                let value = parsed.get("value").cloned().unwrap_or(Value::Null);
                Ok(Some(Entry::Set { key: key.to_string(), value }))
            }
            "del" => Ok(Some(Entry::Delete { key: key.to_string() })),
            other => Err(SeamError::new(
                ErrorCode::IoError,
                format!("backend {backend} line {lineno} has unknown op {other:?}"),
            )),
        }
    }
}

/// A JSONL-backed [`Storage`](harnless_seams::storage::Storage) hub.
///
/// Each backend is one append-only file under `root`. Build with
/// [`JsonlStorage::new`]; the root directory must exist (or be creatable).
pub struct JsonlStorage {
    root: PathBuf,
    /// Serializes appends and compactions within this process. The append
    /// itself is atomic at the OS level (`O_APPEND`), but compaction needs
    /// the whole check-copy-publish window.
    lock: parking_lot::Mutex<()>,
}

impl std::fmt::Debug for JsonlStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlStorage").field("root", &self.root).finish_non_exhaustive()
    }
}

impl JsonlStorage {
    /// Open (or create) a hub rooted at `root`.
    pub fn new(root: impl AsRef<Path>) -> harnless_seams::error::Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root).map_err(|e| {
            SeamError::new(
                ErrorCode::IoError,
                format!("creating storage root {}: {e}", root.display()),
            )
        })?;
        Ok(Self { root, lock: parking_lot::Mutex::new(()) })
    }

    /// The hub root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file a backend lives in.
    fn path_for(&self, backend: &BackendName) -> harnless_seams::error::Result<PathBuf> {
        Ok(self.root.join(backend.file_name()?))
    }

    /// Replay a backend's log, returning the live key → value map.
    fn replay(&self, backend: &BackendName) -> harnless_seams::error::Result<HashMap<String, Value>> {
        let path = self.path_for(backend)?;
        let mut live = HashMap::new();
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(live),
            Err(e) => {
                return Err(SeamError::new(
                    ErrorCode::IoError,
                    format!("opening backend {}: {e}", backend.0),
                ))
            }
        };
        let reader = std::io::BufReader::new(file);
        for (index, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| {
                SeamError::new(
                    ErrorCode::IoError,
                    format!("reading backend {}: {e}", backend.0),
                )
            })?;
            if let Some(entry) = Entry::from_line(&line, &backend.0, index + 1)? {
                match entry {
                    Entry::Set { key, value } => {
                        live.insert(key, value);
                    }
                    Entry::Delete { key } => {
                        live.remove(&key);
                    }
                }
            }
        }
        Ok(live)
    }

    /// Append one entry to a backend's log, durable before return.
    fn append(&self, backend: &BackendName, entry: &Entry) -> harnless_seams::error::Result<()> {
        let path = self.path_for(backend)?;
        let line = entry.to_line()?;
        let _guard = self.lock.lock();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                SeamError::new(
                    ErrorCode::IoError,
                    format!("opening backend {} for append: {e}", backend.0),
                )
            })?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
            .map_err(|e| {
                SeamError::new(
                    ErrorCode::IoError,
                    format!("appending to backend {}: {e}", backend.0),
                )
            })
    }

    /// Rewrite a backend's log as one snapshot of its live keys.
    ///
    /// Racing appends are not lost: after building the snapshot, the file's
    /// current length is compared to the snapshot point and any bytes
    /// written in the meantime are re-appended to the compacted file
    /// before the rename publishes.
    pub fn compact(&self, backend: &BackendName) -> harnless_seams::error::Result<()> {
        let path = self.path_for(backend)?;
        let _guard = self.lock.lock();
        let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let live = self.replay(backend)?;
        let mut snapshot = String::new();
        for (key, value) in live {
            snapshot.push_str(
                &Entry::Set { key, value }.to_line().map_err(|e| {
                    SeamError::new(ErrorCode::IoError, format!("compaction: {e}"))
                })?,
            );
            snapshot.push('\n');
        }
        // Anything appended since `before` must survive compaction.
        let mut tail = Vec::new();
        if let Ok(mut file) = std::fs::File::open(&path) {
            if file.seek(SeekFrom::Start(before)).is_ok() {
                let _ = file.read_to_end(&mut tail);
            }
        }
        let tmp = {
            let mut s = path.as_os_str().to_os_string();
            s.push(".compacting");
            PathBuf::from(s)
        };
        {
            let mut out = std::fs::File::create(&tmp).map_err(|e| {
                SeamError::new(
                    ErrorCode::IoError,
                    format!("creating compaction temp {}: {e}", tmp.display()),
                )
            })?;
            out.write_all(snapshot.as_bytes())
                .and_then(|()| out.write_all(&tail))
                .and_then(|()| out.flush())
                .and_then(|()| out.sync_all())
                .map_err(|e| {
                    SeamError::new(
                        ErrorCode::IoError,
                        format!("writing compaction temp: {e}"),
                    )
                })?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| {
            SeamError::new(
                ErrorCode::IoError,
                format!("publishing compacted backend {}: {e}", backend.0),
            )
        })
    }
}

impl Storage for JsonlStorage {
    fn get(&self, backend: &BackendName, key: &str) -> harnless_seams::error::Result<Option<Value>> {
        // Replay sees every append this process made (each is durable
        // before its call returned) — read-your-writes falls out.
        Ok(self.replay(backend)?.remove(key))
    }

    fn set(
        &self,
        backend: &BackendName,
        key: &str,
        value: Value,
    ) -> harnless_seams::error::Result<()> {
        self.append(backend, &Entry::Set { key: key.to_string(), value })
    }

    fn delete(&self, backend: &BackendName, key: &str) -> harnless_seams::error::Result<()> {
        self.append(backend, &Entry::Delete { key: key.to_string() })
    }
}

/// The reference [`StorageDomain`](harnless_seams::storage::StorageDomain): versioned JSON envelopes, base64 into
/// opaque units.
///
/// The envelope (`{"v":1,"payload":…}`) lets the format evolve without
/// reinterpreting old bytes; the hub below stores only the opaque string.
pub struct JsonlDomain {
    /// Envelope format version written by `encode`.
    version: u32,
}

impl std::fmt::Debug for JsonlDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlDomain").field("version", &self.version).finish()
    }
}

impl Default for JsonlDomain {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonlDomain {
    /// The current envelope version (1).
    pub fn new() -> Self {
        Self { version: 1 }
    }
}

impl StorageDomain for JsonlDomain {
    fn encode(&self, value: Value) -> harnless_seams::error::Result<OpaqueUnit> {
        let envelope = serde_json::json!({ "v": self.version, "payload": value });
        let text = serde_json::to_vec(&envelope).map_err(|e| {
            SeamError::new(ErrorCode::IoError, format!("encoding domain unit: {e}"))
        })?;
        Ok(OpaqueUnit(base64::engine::general_purpose::STANDARD.encode(text).into_bytes()))
    }

    fn decode(&self, unit: &OpaqueUnit) -> harnless_seams::error::Result<Value> {
        let text = base64::engine::general_purpose::STANDARD
            .decode(&unit.0)
            .map_err(|e| {
                SeamError::new(ErrorCode::IoError, format!("decoding opaque unit: {e}"))
            })?;
        let envelope: Value = serde_json::from_slice(&text).map_err(|e| {
            SeamError::new(ErrorCode::IoError, format!("opaque unit envelope is corrupt: {e}"))
        })?;
        let version = envelope.get("v").and_then(Value::as_u64).ok_or_else(|| {
            SeamError::new(ErrorCode::IoError, "opaque unit envelope has no version".to_string())
        })?;
        if version > self.version as u64 {
            return Err(SeamError::new(
                ErrorCode::IoError,
                format!("opaque unit is envelope v{version}, this domain reads up to v{}", self.version),
            ));
        }
        envelope.get("payload").cloned().ok_or_else(|| {
            SeamError::new(ErrorCode::IoError, "opaque unit envelope has no payload".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn backend(name: &str) -> BackendName {
        BackendName(name.to_string())
    }

    fn hub_in(dir: &Path) -> JsonlStorage {
        JsonlStorage::new(dir).unwrap()
    }

    #[test]
    fn set_get_delete_read_your_writes() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(dir.path());
        assert_eq!(hub.get(&backend("notes"), "a").unwrap(), None);
        hub.set(&backend("notes"), "a", json!({"n": 1})).unwrap();
        assert_eq!(hub.get(&backend("notes"), "a").unwrap(), Some(json!({"n": 1})));
        hub.delete(&backend("notes"), "a").unwrap();
        assert_eq!(hub.get(&backend("notes"), "a").unwrap(), None, "tombstone must outrank the set");
    }

    #[test]
    fn restart_sees_persisted_log() {
        // Persistence: a fresh provider over the same root replays the log.
        let dir = tempfile::tempdir().unwrap();
        hub_in(dir.path()).set(&backend("kv"), "k", json!("v1")).unwrap();
        let reopened = hub_in(dir.path());
        assert_eq!(reopened.get(&backend("kv"), "k").unwrap(), Some(json!("v1")));
        reopened.delete(&backend("kv"), "k").unwrap();
        assert_eq!(hub_in(dir.path()).get(&backend("kv"), "k").unwrap(), None);
    }

    #[test]
    fn named_backends_coexist_and_are_isolated() {
        // Named-backend coexistence: side-by-side files, no cross-talk.
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(dir.path());
        hub.set(&backend("alpha"), "shared-key", json!("from-alpha")).unwrap();
        hub.set(&backend("beta"), "shared-key", json!("from-beta")).unwrap();
        assert_eq!(hub.get(&backend("alpha"), "shared-key").unwrap(), Some(json!("from-alpha")));
        assert_eq!(hub.get(&backend("beta"), "shared-key").unwrap(), Some(json!("from-beta")));
        hub.delete(&backend("alpha"), "shared-key").unwrap();
        assert_eq!(hub.get(&backend("alpha"), "shared-key").unwrap(), None);
        assert_eq!(
            hub.get(&backend("beta"), "shared-key").unwrap(),
            Some(json!("from-beta")),
            "deleting in one backend must not touch another"
        );
        // Physically side by side.
        assert!(dir.path().join("alpha.jsonl").exists());
        assert!(dir.path().join("beta.jsonl").exists());
    }

    #[test]
    fn log_is_append_only_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(dir.path());
        hub.set(&backend("log"), "a", json!(1)).unwrap();
        hub.set(&backend("log"), "a", json!(2)).unwrap();
        hub.delete(&backend("log"), "a").unwrap();
        let text = std::fs::read_to_string(dir.path().join("log.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "every operation appends; nothing rewrites");
        assert!(lines[0].contains("\"op\":\"set\""));
        assert!(lines[2].contains("\"op\":\"del\""));
    }

    #[test]
    fn compaction_preserves_state_and_racing_appends() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(dir.path());
        for i in 0..10 {
            hub.set(&backend("c"), "old", json!(i)).unwrap();
        }
        hub.set(&backend("c"), "keep", json!("live")).unwrap();
        hub.delete(&backend("c"), "gone").unwrap();
        hub.compact(&backend("c")).unwrap();
        let text = std::fs::read_to_string(dir.path().join("c.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 2, "compacted to live keys only");
        assert_eq!(hub.get(&backend("c"), "old").unwrap(), Some(json!(9)));
        assert_eq!(hub.get(&backend("c"), "keep").unwrap(), Some(json!("live")));
        assert_eq!(hub.get(&backend("c"), "gone").unwrap(), None);
        // Writes after compaction still work and replay.
        hub.set(&backend("c"), "after", json!("post-compact")).unwrap();
        assert_eq!(hub_in(dir.path()).get(&backend("c"), "after").unwrap(), Some(json!("post-compact")));
    }

    #[test]
    fn unusable_backend_name_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(dir.path());
        let err = hub.get(&backend(""), "k").unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        let err = hub.get(&backend("..."), "k").unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound, "a name of only dots is unusable");
    }

    #[test]
    fn corrupt_log_line_is_typed_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_in(dir.path());
        hub.set(&backend("x"), "a", json!(1)).unwrap();
        std::fs::OpenOptions::new().append(true).open(dir.path().join("x.jsonl")).unwrap()
            .write_all(b"i am not json\n").unwrap();
        let err = hub.get(&backend("x"), "a").unwrap_err();
        assert_eq!(err.code, ErrorCode::IoError);
        assert!(err.message.contains("line 2"), "error names the bad line: {}", err.message);
    }

    #[test]
    fn domain_encode_decode_roundtrip() {
        let domain = JsonlDomain::new();
        let value = json!({"record": "note", "tags": ["a", "b"]});
        let unit = domain.encode(value.clone()).unwrap();
        assert_ne!(String::from_utf8_lossy(&unit.0), value.to_string(), "unit is opaque, not raw JSON");
        assert_eq!(domain.decode(&unit).unwrap(), value);
    }

    #[test]
    fn domain_rejects_corrupt_and_future_envelopes() {
        let domain = JsonlDomain::new();
        assert_eq!(domain.decode(&OpaqueUnit(b"not base64 !!".to_vec())).unwrap_err().code, ErrorCode::IoError);
        let future = OpaqueUnit(
            base64::engine::general_purpose::STANDARD
                .encode(br#"{"v":99,"payload":null}"#)
                .into_bytes(),
        );
        let err = domain.decode(&future).unwrap_err();
        assert_eq!(err.code, ErrorCode::IoError);
        assert!(err.message.contains("v99"));
    }
}
