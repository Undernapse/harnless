//! The session store: one append-only JSONL log per session id.
//!
//! This is the durability provider the CLI's `store` row mounts (#69): a
//! session file is `<dir>/<id>.jsonl`, one committed record per line, and the
//! store is a *mirror* of the live log — the composition never knows whether
//! its log is seeded (#67). The hub [`JsonlStorage`](crate::JsonlStorage) is
//! deliberately not reused: session persistence is its own seam, and the
//! session file's shape (header line, committed records, fork boundary) is
//! not the hub's key/value log.
//!
//! The integrity rules this type owns (#70):
//!
//! * **One committed record = one line = one `write_all(line + "\n")`**, then
//!   flush + `sync_all` before the append returns (durable-before-return).
//!   The session writer never splits line and newline, so a crash yields at
//!   most one torn tail — exactly the case the recovery rule covers.
//! * **Torn final line dropped at load**: a trailing segment after the last
//!   newline that fails to parse (or parses but is unterminated) is a crash
//!   artifact and is ignored. Any line *before* the last newline-terminated
//!   line that fails to parse is `Corrupt` naming the line number — resume
//!   and fork refuse; `list` still shows the file.
//! * **The drop rewrites**: on first append after a tolerant load the writer
//!   truncates the file to the last newline-terminated boundary before
//!   appending, so recovery is idempotent and a torn tail can never become a
//!   mid-file line. `load` alone never touches bytes.
//! * **Advisory lock**: `flock(LOCK_EX)` on the sibling `<id>.jsonl.lock`,
//!   taken by `create_new`/`open_existing` before the writer is returned and
//!   held for the writer's life. The lock file records the holder pid for
//!   diagnostics. A would-block probe on any lock path is `Locked` naming the
//!   id and (when readable) the holder pid. Stale locks need no policy:
//!   flock dies with the process.
//! * **`create_new` uses `O_EXCL`** on the session file, so a mint collision
//!   is a loud refusal, never a silent append onto a foreign log.
//! * **`compact` refuses while locked** (`LOCK_EX|LOCK_NB` ⇒ `Locked`) and is
//!   otherwise temp+rename. The CLI has no call site; it exists for a future
//!   maintenance tool.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use harnless_agent::events::CommittedRecord;

/// The store's typed failure. `code` is the stable string the CLI routes on
/// (it maps 1:1 onto the #71 error table); the CLI wraps it in a `CliError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    /// Stable machine-readable code: `session-not-found`, `session-locked`,
    /// `session-corrupt`, `session-mint-failed`, `io-error`.
    pub code: &'static str,
    /// Human-readable message naming the id (and line/pid when known).
    pub message: String,
}

impl SessionError {
    /// A typed failure with a stable code and a message naming the id.
    /// Public so the CLI's tests can synthesize the store's failure shapes
    /// without touching a real filesystem.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SessionError {}

/// The file-level header a fork file carries as its first line:
/// `{"header":{"forked_from":"<source-id>"}}`.
///
/// The source id is file-level metadata, never an event (#67 §2): the loop,
/// history, and log machinery never read it; the loader exposes it to the
/// CLI and never yields it as a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The session this file was forked from.
    pub forked_from: u64,
}

/// A loaded session log: the header (0 or 1, first line) plus the committed
/// records, verbatim — positions and times are the store's, never re-dated.
#[derive(Debug, Clone)]
pub struct StoredLog {
    /// The fork header, when the file carries one.
    pub header: Option<Header>,
    /// The committed records in position order.
    pub records: Vec<CommittedRecord>,
}

impl StoredLog {
    /// Whether the loaded records carry at least one seed boundary — the
    /// derived seeding fact (#67): contains-≥1, never a counted marker.
    pub fn is_seeded(&self) -> bool {
        self.records.iter().any(|r| {
            matches!(
                r.event,
                harnless_agent::events::SessionEvent::SeedBoundary
            )
        })
    }
}

/// One row of `sessions list`: directory metadata, read tolerantly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    /// The session id (the file stem).
    pub id: u64,
    /// Last modification time as epoch seconds (the sort key).
    pub mtime_secs: u64,
    /// Event count over the good prefix (`None` when the file is corrupt).
    pub event_count: Option<usize>,
    /// First `UserMessage`'s first text block (the `list` excerpt source);
    /// `None` when absent or unreadable.
    pub first_prompt: Option<String>,
    /// Mid-file corruption: the file is shown, resume refuses.
    pub corrupt: bool,
}

/// A tolerant load result: the good prefix, plus whether a torn tail was
/// dropped and whether a mid-file line refused the load.
#[derive(Debug)]
pub struct LoadReport {
    /// The header, when present.
    pub header: Option<Header>,
    /// Records parsed from newline-terminated lines before any failure.
    pub records: Vec<CommittedRecord>,
    /// Byte length of the good (newline-terminated, parseable) prefix — the
    /// truncation boundary a mounted writer restores on first append.
    pub good_len: u64,
    /// A trailing unterminated segment was dropped (crash artifact).
    pub torn_tail: bool,
}

/// The advisory session lock: `flock(LOCK_EX)` on `<file>.lock`, released on
/// drop. The `credentials-local::FileLock` shape, copied not shared.
struct FileLock {
    _file: File,
}

impl FileLock {
    /// Acquire the lock on `<target>.lock`, writing the holder pid after
    /// acquisition. A would-block is `Locked` naming the id and holder pid.
    fn acquire(target: &Path, id: u64, nonblocking: bool) -> Result<Self, SessionError> {
        let mut path = target.as_os_str().to_os_string();
        path.push(".lock");
        let path = PathBuf::from(path);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| {
                SessionError::new("io-error", format!("opening lock file {}: {e}", path.display()))
            })?;
        let would_block = |e: std::io::Error| {
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                SessionError::new(
                    "session-locked",
                    format!(
                        "session {id} is locked by process {}",
                        read_holder(&path)
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "?".to_string())
                    ),
                )
            } else {
                SessionError::new("io-error", format!("locking session {id}: {e}"))
            }
        };
        let rc = if nonblocking {
            // SAFETY: `file` is a valid open descriptor for the duration.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }
        } else {
            // SAFETY: as above; blocking acquisition.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }
        };
        if rc != 0 {
            return Err(would_block(std::io::Error::last_os_error()));
        }
        // The holder record is diagnostic only — flock dies with the process,
        // so a stale lock cannot exist to be misread.
        let mut file = file;
        let _ = file.write_all(format!("{}\n", std::process::id()).as_bytes());
        let _ = file.flush();
        Ok(Self { _file: file })
    }
}

fn read_holder(lock_path: &Path) -> Option<u32> {
    let mut buf = String::new();
    match File::open(lock_path).and_then(|mut f| f.read_to_string(&mut buf)) {
        Ok(_) => {}
        Err(_) => return None,
    }
    buf.trim().parse().ok()
}

/// The session store: pure path state over a directory, created lazily at
/// first write (#69 §5 — a mount of the store row never touches the disk).
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// A store rooted at `dir`. Pure path state: no directory is created.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The root directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    fn create_dir(&self) -> Result<(), SessionError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            SessionError::new(
                "io-error",
                format!("creating session dir {}: {e}", self.dir.display()),
            )
        })
    }

    /// Load a session tolerantly: the good prefix, the truncation boundary,
    /// and whether a torn tail was dropped. Mid-file corruption refuses with
    /// `session-corrupt` naming the id and line. Never touches bytes — the
    /// truncate rides a mounted writer's first append (#70 §3).
    pub fn load_tolerant(&self, id: u64) -> Result<Option<LoadReport>, SessionError> {
        let path = self.path(id);
        let mut buf = Vec::new();
        match File::open(&path).and_then(|mut f| f.read_to_end(&mut buf)) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(SessionError::new(
                    "io-error",
                    format!("opening session {}: {e}", path.display()),
                ))
            }
        }
        Ok(Some(parse_log(&buf, id)?))
    }

    /// Load a session's header and committed records, refusing a torn tail:
    /// the resume/fork route demands a clean file, and a crash artifact must
    /// be repaired by a mounted writer (first append truncates), not silently
    /// hidden from a seed. Mid-file corruption is `session-corrupt`.
    pub fn load(&self, id: u64) -> Result<Option<StoredLog>, SessionError> {
        match self.load_tolerant(id)? {
            None => Ok(None),
            Some(report) => {
                if report.torn_tail {
                    return Err(SessionError::new(
                        "session-corrupt",
                        format!(
                            "session {id} ends with a torn line; resume it once to repair the tail"
                        ),
                    ));
                }
                Ok(Some(StoredLog {
                    header: report.header,
                    records: report.records,
                }))
            }
        }
    }

    /// Create a brand-new session file: `O_EXCL` on the session file (an
    /// existing file is never silently appended-to by a mint), then the
    /// advisory lock. An existing file or a held lock is `session-locked` /
    /// `session-not-found`-free refusal naming the collision.
    pub fn create_new(&self, id: u64) -> Result<SessionWriter, SessionError> {
        self.create_dir()?;
        let path = self.path(id);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    SessionError::new(
                        "session-locked",
                        format!(
                            "session {id} already exists at {}; a mint collision is refused",
                            path.display()
                        ),
                    )
                } else {
                    SessionError::new("io-error", format!("creating session {}: {e}", path.display()))
                }
            })?;
        // The lock is taken after the exclusive create: the create proves
        // ownership, the lock publishes it to other openers.
        let lock = FileLock::acquire(&path, id, true)?;
        Ok(SessionWriter {
            file,
            _lock: lock,
            id,
        })
    }

    /// Open an existing session for writing: take the lock first (a held
    /// lock is `session-locked`), refuse a missing file with
    /// `session-not-found`, and repair a torn tail by truncating to the last
    /// newline-terminated boundary before the writer is returned (#70 §3:
    /// the truncate rides the mounted writer, and opening this writer is
    /// that mounting — torn bytes can never become a mid-file line).
    pub fn open_existing(&self, id: u64) -> Result<SessionWriter, SessionError> {
        let path = self.path(id);
        if !path.exists() {
            return Err(SessionError::new(
                "session-not-found",
                format!("no session {id} in {}", self.dir.display()),
            ));
        }
        let lock = FileLock::acquire(&path, id, true)?;
        // Inspect the file under the lock: a torn tail is repaired *now* —
        // truncate to the good boundary — so the mounted writer's appends
        // land on a clean file and torn bytes can never become a mid-file
        // line (#70 §3: the truncate rides the mounted writer, and opening
        // this writer is that mounting; a resume that never appends leaves
        // the bytes alone because the repair only happens under the lock
        // this open just took, i.e. only for a session being mounted).
        let report = self.load_tolerant(id)?;
        let torn_len = match &report {
            Some(r) if r.torn_tail => Some(r.good_len),
            _ => None,
        };
        let file = OpenOptions::new().append(true).open(&path).map_err(|e| {
            SessionError::new("io-error", format!("opening session {} for append: {e}", path.display()))
        })?;
        if let Some(good_len) = torn_len {
            file.set_len(good_len).map_err(|e| {
                SessionError::new("io-error", format!("repairing session {id}: {e}"))
            })?;
        }
        Ok(SessionWriter {
            file,
            _lock: lock,
            id,
        })
    }

    /// Read a session *while holding its lock* for the copy window — the
    /// fork-source read (#67 §5). A live writer ⇒ `session-locked`; a torn
    /// tail ⇒ `session-corrupt` (fork refuses, per the resume rule).
    pub fn read_locked(&self, id: u64) -> Result<StoredLog, SessionError> {
        let path = self.path(id);
        if !path.exists() {
            return Err(SessionError::new(
                "session-not-found",
                format!("no session {id} in {}", self.dir.display()),
            ));
        }
        let _lock = FileLock::acquire(&path, id, true)?;
        self.load(id)?.ok_or_else(|| {
            SessionError::new(
                "session-not-found",
                format!("no session {id} in {}", self.dir.display()),
            )
        })
    }

    /// Mint a fresh session id: `(unix_micros << 20) | rand(20 bits)` (#70 §5
    /// / #71 §2) — time-high so ids sort, random-low so a collision needs the
    /// same microsecond *and* the same 20-bit draw.
    pub fn mint_id() -> u64 {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let rand20 = own_rand() & ((1u64 << 20) - 1);
        (micros << 20) | rand20
    }

    /// Write a fork file: header + the source's records verbatim + exactly
    /// one `SeedBoundary`, under the *target's* `O_EXCL` create and lock.
    /// The caller holds the source lock for the read (#67 §5); lock order is
    /// always source-then-target, so no cycle is constructible.
    pub fn create_fork(&self, target: u64, header: &Header, records: &[CommittedRecord]) -> Result<SessionWriter, SessionError> {
        let mut writer = self.create_new(target)?;
        // The header line is file-level metadata, not a record.
        let line = format!(
            "{{\"header\":{{\"forked_from\":\"{}\"}}}}",
            header.forked_from
        );
        writer.write_raw_line(&line)?;
        for record in records {
            writer.append(record)?;
        }
        writer.append(&CommittedRecord {
            position: records.len(),
            time_ms: now_ms(),
            event: harnless_agent::events::SessionEvent::SeedBoundary,
        })?;
        Ok(writer)
    }

    /// Rewrite a session's log as its records verbatim (the maintenance hook
    /// #70 §2 leaves in place: the CLI never calls it). Refuses `session-locked`
    /// while the session lock is held, and leaves bytes untouched on refusal.
    pub fn compact(&self, id: u64) -> Result<(), SessionError> {
        let path = self.path(id);
        if !path.exists() {
            return Err(SessionError::new(
                "session-not-found",
                format!("no session {id} in {}", self.dir.display()),
            ));
        }
        // Non-blocking probe: a held lock refuses compaction before any temp
        // write, so a refusal leaves the log untouched by construction.
        let lock = FileLock::acquire(&path, id, true)?;
        let stored = self.load(id)?.ok_or_else(|| {
            SessionError::new("session-not-found", format!("no session {id}"))
        })?;
        let mut snapshot = String::new();
        if let Some(header) = &stored.header {
            snapshot.push_str(&format!(
                "{{\"header\":{{\"forked_from\":\"{}\"}}}}\n",
                header.forked_from
            ));
        }
        for record in &stored.records {
            snapshot.push_str(&record_line(record)?);
            snapshot.push('\n');
        }
        let tmp = {
            let mut s = path.as_os_str().to_os_string();
            s.push(".compacting");
            PathBuf::from(s)
        };
        {
            let mut out = File::create(&tmp).map_err(|e| {
                SessionError::new("io-error", format!("creating compaction temp: {e}"))
            })?;
            out.write_all(snapshot.as_bytes())
                .and_then(|()| out.flush())
                .and_then(|()| out.sync_all())
                .map_err(|e| {
                    SessionError::new("io-error", format!("writing compaction temp: {e}"))
                })?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| {
            SessionError::new("io-error", format!("publishing compacted session {id}: {e}"))
        })?;
        // The lock file is a sibling; the rename never disturbs it, and the
        // guard releases on return.
        drop(lock);
        Ok(())
    }

    /// List sessions: directory metadata only, tolerant of a corrupt file
    /// (shown, never refused) and of a missing directory (empty list).
    /// Ordering is mtime descending (#71 §3).
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(id) = stem.parse::<u64>() else { continue };
            let mtime_secs = path
                .metadata()
                .and_then(|m| {
                    m.modified().map(|t| {
                        t.duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    })
                })
                .unwrap_or(0);
            let (event_count, first_prompt, corrupt) = match self.load_tolerant(id) {
                Ok(Some(report)) => {
                    let first = first_prompt_of(&report.records);
                    (Some(report.records.len()), first, false)
                }
                Ok(None) => (None, None, true),
                Err(e) if e.code == "session-corrupt" => (None, None, true),
                Err(_) => (None, None, true),
            };
            out.push(SessionMeta {
                id,
                mtime_secs,
                event_count,
                first_prompt,
                corrupt,
            });
        }
        out.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs).then(b.id.cmp(&a.id)));
        out
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A tiny xorshift over the clock + pid. The mint's entropy contract (#70 §5)
/// is "time-high + random-low, never a clock-only id"; the OS RNG dependency
/// is not worth a crate edge for 20 bits whose correctness is owned by
/// `O_EXCL` + flock anyway.
fn own_rand() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = micros ^ ((std::process::id() as u64) << 32) ^ 0x9E3779B97F4A7C15;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// The first `UserMessage`'s first text block, if any.
fn first_prompt_of(records: &[CommittedRecord]) -> Option<String> {
    records.iter().find_map(|r| match &r.event {
        harnless_agent::events::SessionEvent::UserMessage(m) => {
            m.blocks.iter().find_map(|b| match b {
                harnless_agent::events::ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        }
        _ => None,
    })
}

/// Serialize one committed record to its JSONL line (no newline).
fn record_line(record: &CommittedRecord) -> Result<String, SessionError> {
    serde_json::to_string(record).map_err(|e| SessionError::new("io-error", format!("encode: {e}")))
}

/// Parse a session file's bytes: header (0 or 1, first line), records,
/// truncation boundary, torn-tail flag. Mid-file parse failure is
/// `session-corrupt` naming the line number (#70 §3).
fn parse_log(bytes: &[u8], id: u64) -> Result<LoadReport, SessionError> {
    let mut header = None;
    let mut records = Vec::new();
    let mut good_len: u64 = 0;
    let mut torn_tail = false;
    // Split on newlines: a trailing segment after the last newline is the
    // only possible torn tail; every newline-terminated line before it must
    // parse.
    let mut offset: u64 = 0;
    let lines = bytes.split_inclusive(|b| *b == b'\n');
    let total = bytes.len() as u64;
    for raw in lines {
        let line_len = raw.len() as u64;
        let terminated = raw.last() == Some(&b'\n');
        let text = String::from_utf8_lossy(raw.strip_suffix(b"\n".as_slice()).unwrap_or(raw));
        if text.trim().is_empty() {
            if terminated {
                good_len += line_len;
                offset += line_len;
            }
            continue;
        }
        if !terminated {
            // Trailing unterminated segment: a crash artifact iff it is the
            // very last segment. Drop it (torn tail), whatever it contains.
            let _ = offset;
            torn_tail = true;
            break;
        }
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
            SessionError::new(
                "session-corrupt",
                format!("session {id}: line {} is not a valid record", records.len() + header_line_offset(&header) + 1),
            )
        })?;
        if let Some(h) = value.get("header") {
            if records.is_empty() && header.is_none() {
                let forked = h
                    .get("forked_from")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| {
                        SessionError::new(
                            "session-corrupt",
                            format!("session {id}: line 1 header is malformed"),
                        )
                    })?;
                header = Some(Header { forked_from: forked });
                good_len += line_len;
                offset += line_len;
                continue;
            }
            return Err(SessionError::new(
                "session-corrupt",
                format!("session {id}: header line must be first and unique"),
            ));
        }
        let record: CommittedRecord = serde_json::from_value(value).map_err(|_| {
            SessionError::new(
                "session-corrupt",
                format!(
                    "session {id}: line {} is not a valid record",
                    records.len() + header_line_offset(&header) + 1
                ),
            )
        })?;
        records.push(record);
        good_len += line_len;
        offset += line_len;
    }
    let _ = total;
    Ok(LoadReport {
        header,
        records,
        good_len,
        torn_tail,
    })
}

fn header_line_offset(header: &Option<Header>) -> usize {
    usize::from(header.is_some())
}

/// A mounted session writer: holds the advisory lock for its life and
/// appends one committed record per line (single `write_all`, durable before
/// return). A torn tail is repaired at open, before this type exists.
pub struct SessionWriter {
    file: File,
    _lock: FileLock,
    id: u64,
}

impl std::fmt::Debug for SessionWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionWriter")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SessionWriter {
    /// The session id this writer appends to.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Append one committed record: a single `write_all(line + "\n")`,
    /// flush, and `sync_all` — durable before the call returns.
    pub fn append(&mut self, record: &CommittedRecord) -> Result<(), SessionError> {
        let line = record_line(record)?;
        self.write_line(&line)
    }

    /// Write a raw line (used for the fork header, which is not a record).
    fn write_raw_line(&mut self, line: &str) -> Result<(), SessionError> {
        self.write_line(line)
    }

    fn write_line(&mut self, line: &str) -> Result<(), SessionError> {
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        // One write: a crash can only ever tear a whole record, never a
        // half-newline artifact (#70 §4).
        self.file
            .write_all(&buf)
            .and_then(|()| self.file.flush())
            .and_then(|()| self.file.sync_all())
            .map_err(|e| {
                SessionError::new("io-error", format!("appending to session {}: {e}", self.id))
            })
    }
}
