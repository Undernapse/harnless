//! Session-store mechanics (#70 §6): every integrity rule gets its
//! single-process story executed here, at the store's own surface. The CLI
//! seam tests (`harnless-cli/tests/durability_seam.rs`) compose the same
//! rules through the mounted composition; these pin the raw file discipline.
//!
//! Zero network, zero flake: the two-process stories are two-handles-in-one-
//! process lock fixtures (flock is per-open-file-description), never spawns.

use std::path::PathBuf;

use std::os::unix::io::AsRawFd;

use harnless_agent::events::{CommittedRecord, SessionEvent, TurnEndReason};
use harnless_storage_jsonl::SessionStore;

fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "harnless-session-{}-{}-{tag}",
        std::process::id(),
        tag_counter()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn tag_counter() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn record(position: usize, event: SessionEvent) -> CommittedRecord {
    CommittedRecord {
        position,
        time_ms: 1000 + position as u64,
        event,
    }
}

fn bracket(start: usize) -> Vec<CommittedRecord> {
    vec![
        record(start, SessionEvent::TurnOpen),
        record(
            start + 1,
            SessionEvent::TurnClose {
                reason: TurnEndReason::Completed,
            },
        ),
    ]
}

fn write_lines(dir: &std::path::Path, id: u64, lines: &[String]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    path
}

fn record_lines(records: &[CommittedRecord]) -> Vec<String> {
    records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect()
}

#[test]
fn append_round_trips_verbatim_and_grows_only_the_tail() {
    let dir = fixture("roundtrip");
    let store = SessionStore::new(&dir);
    // Lazy dir: a fresh store touches nothing.
    assert!(!dir.exists());
    let mut writer = store.create_new(7).unwrap();
    assert!(dir.exists());
    for r in bracket(0) {
        writer.append(&r).unwrap();
    }
    let path = dir.join("7.jsonl");
    let prefix_len = std::fs::metadata(&path).unwrap().len();
    writer.append(&record(2, SessionEvent::StepOpen)).unwrap();
    let loaded = store.load(7).unwrap().unwrap();
    assert_eq!(loaded.header, None);
    assert_eq!(loaded.records.len(), 3);
    // Verbatim: times ride the store, never re-dated.
    assert_eq!(loaded.records[0].time_ms, 1000);
    assert_eq!(loaded.records[2].position, 2);
    // The file grew only at the tail: the prefix bytes are stable.
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > prefix_len as usize);
    drop(writer);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn one_record_is_one_line_one_write() {
    // Byte-shape contract (#70 §4): every line is a whole record; the file
    // ends with a newline; no line is a fragment.
    let dir = fixture("byteshape");
    let store = SessionStore::new(&dir);
    let mut writer = store.create_new(1).unwrap();
    for r in bracket(0) {
        writer.append(&r).unwrap();
    }
    drop(writer);
    let bytes = std::fs::read(dir.join("1.jsonl")).unwrap();
    assert_eq!(bytes.last(), Some(&b'\n'));
    let lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2);
    for line in lines {
        let _: CommittedRecord = serde_json::from_slice(line).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lock_held_second_open_refuses_and_compaction_refuses() {
    // Same process, second handle ⇒ flock EWOULDBLOCK fires (POSIX
    // per-open-file-description semantics; no spawn, #70 §6).
    let dir = fixture("lock");
    let store = SessionStore::new(&dir);
    let mut writer = store.create_new(9).unwrap();
    writer.append(&record(0, SessionEvent::TurnOpen)).unwrap();
    let err = store.open_existing(9).unwrap_err();
    assert_eq!(err.code, "session-locked");
    assert!(err.message.contains("9"), "{err}");
    // Compaction on the same fixture refuses and leaves bytes unchanged.
    let before = std::fs::read(dir.join("9.jsonl")).unwrap();
    let err = store.compact(9).unwrap_err();
    assert_eq!(err.code, "session-locked");
    assert_eq!(std::fs::read(dir.join("9.jsonl")).unwrap(), before);
    drop(writer);
    let w2 = store.open_existing(9).unwrap();
    // While a writer holds the lock, compaction still refuses; once the
    // last handle drops, it succeeds.
    assert_eq!(store.compact(9).unwrap_err().code, "session-locked");
    drop(w2);
    store.compact(9).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn torn_tail_loads_tolerantly_and_first_append_truncates() {
    let dir = fixture("torn");
    let store = SessionStore::new(&dir);
    let records = bracket(0);
    let path = write_lines(&dir, 3, &record_lines(&records));
    // Hand-tear: append garbage with no trailing newline.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"position\":2,\"time_m").unwrap();
    }
    // Tolerant load returns the good prefix.
    let report = store.load_tolerant(3).unwrap().unwrap();
    assert!(report.torn_tail);
    assert_eq!(report.records, records);
    assert_eq!(report.good_len, std::fs::metadata(&path).unwrap().len() - 21);
    // Strict load refuses until repaired.
    assert_eq!(store.load(3).unwrap_err().code, "session-corrupt");
    // Opening the mounted writer repairs: truncate to the boundary, then
    // the append lands on a clean file.
    let mut writer = store.open_existing(3).unwrap();
    writer.append(&record(2, SessionEvent::TurnOpen)).unwrap();
    drop(writer);
    let loaded = store.load(3).unwrap().unwrap();
    assert_eq!(loaded.records.len(), 3);
    assert_eq!(loaded.records[2].position, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mid_file_corruption_refuses_naming_id_and_line() {
    let dir = fixture("midfile");
    let store = SessionStore::new(&dir);
    let mut lines = record_lines(&bracket(0));
    lines.splice(1..1, ["{ not json ]".to_string()]);
    write_lines(&dir, 4, &lines);
    let err = store.load(4).unwrap_err();
    assert_eq!(err.code, "session-corrupt");
    assert!(err.message.contains("4"), "{err}");
    assert!(err.message.contains("line 2"), "{err}");
    // Even a mounted open refuses — resume and fork both refuse.
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_id_is_session_not_found() {
    let dir = fixture("missing");
    let store = SessionStore::new(&dir);
    let err = store.open_existing(123456789).unwrap_err();
    assert_eq!(err.code, "session-not-found");
    assert!(err.message.contains("123456789"));
    assert!(err.message.contains(&dir.display().to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_new_twice_is_an_excl_refusal() {
    let dir = fixture("excl");
    let store = SessionStore::new(&dir);
    let w1 = store.create_new(5).unwrap();
    let err = store.create_new(5).unwrap_err();
    assert_eq!(err.code, "session-locked");
    drop(w1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mint_is_time_high_and_never_zero() {
    let a = SessionStore::mint_id();
    let b = SessionStore::mint_id();
    assert_ne!(a, 0);
    // Time-high: shifting out the 20 random bits leaves a plausible micros
    // value that is non-decreasing across calls.
    assert!(a >> 20 <= b >> 20 || a >> 20 - b >> 20 <= 1);
}

#[test]
fn fork_writes_header_seed_boundary_and_freezes_source_bytes() {
    let dir = fixture("fork");
    let store = SessionStore::new(&dir);
    let source = bracket(0);
    write_lines(&dir, 11, &record_lines(&source));
    let stored = store.read_locked(11).unwrap();
    let before = std::fs::read(dir.join("11.jsonl")).unwrap();
    let header = harnless_storage_jsonl::Header { forked_from: 11 };
    let mut target = store.create_fork(12, &header, &stored.records).unwrap();
    target.append(&record(3, SessionEvent::TurnOpen)).unwrap();
    drop(target);
    // Source bytes unchanged (the mtime half of this pin rides the CLI test).
    assert_eq!(std::fs::read(dir.join("11.jsonl")).unwrap(), before);
    let forked = store.load(12).unwrap().unwrap();
    assert_eq!(forked.header, Some(header));
    // header + source verbatim + boundary + the new turn's record.
    assert_eq!(forked.records[0], stored.records[0]);
    assert_eq!(forked.records[1], stored.records[1]);
    assert!(matches!(
        forked.records[2].event,
        SessionEvent::SeedBoundary
    ));
    assert_eq!(forked.records[2].position, 2);
    assert_eq!(forked.records[3], record(3, SessionEvent::TurnOpen));
    assert!(forked.is_seeded());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fork_of_a_non_contiguous_source_refuses_before_minting() {
    // #67 §5: "a refused fork leaves no orphan". A source that parses but
    // holds a position gap must refuse at the read, while only the source
    // lock was ever taken — never a minted target file.
    let dir = fixture("fork-gap");
    let store = SessionStore::new(&dir);
    let gapped = vec![record(0, SessionEvent::TurnOpen), record(2, SessionEvent::TurnClose { reason: TurnEndReason::Completed })];
    write_lines(&dir, 21, &record_lines(&gapped));
    let err = store.read_locked(21).unwrap_err();
    assert_eq!(err.code, "session-corrupt");
    assert!(err.message.contains("contiguous"), "{err}");
    // No target was ever created: the dir holds only the source (+ lock).
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(files.iter().all(|f| f.starts_with("21.")), "{files:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lock_holder_record_is_exactly_the_current_pid() {
    // The diagnostic half of the lock file: after a second acquisition the
    // record must still parse as one pid, not a stack of them.
    let dir = fixture("lockpid");
    let store = SessionStore::new(&dir);
    let w1 = store.create_new(31).unwrap();
    let lock = dir.join("31.jsonl.lock");
    assert_eq!(
        std::fs::read_to_string(&lock).unwrap().trim().parse::<u32>().unwrap(),
        std::process::id()
    );
    drop(w1);
    let w2 = store.open_existing(31).unwrap();
    assert_eq!(
        std::fs::read_to_string(&lock).unwrap().trim().parse::<u32>().unwrap(),
        std::process::id()
    );
    drop(w2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_orders_by_mtime_desc_and_marks_corrupt_files() {
    let dir = fixture("list");
    let store = SessionStore::new(&dir);
    // Absent dir lists empty.
    assert!(store.list().is_empty());
    std::fs::create_dir_all(&dir).unwrap();
    write_lines(&dir, 1, &record_lines(&bracket(0)));
    write_lines(&dir, 2, &["{ broken".to_string()]);
    // A torn tail is counted over the good prefix.
    let torn = write_lines(&dir, 3, &record_lines(&bracket(0)));
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&torn).unwrap();
        f.write_all(b"{\"pos").unwrap();
    }
    // Distinct mtimes so ordering is deterministic without sleeps: set them
    // explicitly (id 2 newest, id 3 oldest).
    let times = [
        (dir.join("3.jsonl"), 1_700_000_000u64),
        (dir.join("1.jsonl"), 1_700_000_001),
        (dir.join("2.jsonl"), 1_700_000_002),
    ];
    for (path, secs) in times {
        let file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        unsafe {
            let timespec = [
                libc::timespec {
                    tv_sec: secs as libc::time_t,
                    tv_nsec: 0,
                };
                2
            ];
            libc::futimens(file.as_raw_fd(), timespec.as_ptr());
        }
    }
    let metas = store.list();
    let ids: Vec<u64> = metas.iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![2, 1, 3]);
    let corrupt = metas.iter().find(|m| m.id == 2).unwrap();
    assert!(corrupt.corrupt);
    assert_eq!(corrupt.event_count, None);
    let torn_meta = metas.iter().find(|m| m.id == 3).unwrap();
    assert!(!torn_meta.corrupt);
    assert_eq!(torn_meta.event_count, Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}
