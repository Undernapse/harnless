//! File A of the #72 durability contract: the in-process session-log seam.
//!
//! The assertion surface is the **session file** — the store's decoded
//! records — composed through the real mounted session log: a store-mounted
//! composition whose every append mirrors to a temp-dir session file. The
//! composition is the production one (`open_session` + `DefaultComposer`'s
//! seeding mount); the only injection is the scripted model, per the #64
//! corpus harness (`support/mod.rs`) — the spy panics past the corpus, so
//! extra turns can't hide.
//!
//! Zero network, zero flake: every model answer is a corpus replay, every
//! two-process story is a single-process lock-file fixture (#70 §6), and no
//! test sleeps.

mod support;

use std::path::PathBuf;

use harnless_agent::events::{
    ChunkRecord, CommittedRecord, ContentBlock, MessageRecord, SessionEvent, ToolCallRecord,
    ToolResultRecord, TurnEndReason,
};
use harnless_agent::session::SessionLog;
use harnless_cli::boot::DefaultComposer;
use harnless_cli::run::drive_turn;
use harnless_cli::session::{mint_with, open_session, SessionMount};
use harnless_seams::{CallId, ErrorCode, MessageId};
use harnless_storage_jsonl::{FileLock, Header, SessionError, SessionStore};
use support::{failing_recording, text_recording, write_corpus};

/// A fresh temp store dir, collision authority `tempfile` (the workspace's
/// own precedent — no external `mktemp` spawn, no PATH dependency).
fn temp_store(tag: &str) -> PathBuf {
    let root = tempfile::tempdir().expect("tempdir");
    // The dir must outlive the test's explicit `remove_dir_all`, so the
    // guard is forgotten and the cleanup stays the test's own.
    let dir = root.path().join(tag);
    std::fs::create_dir_all(&dir).unwrap();
    std::mem::forget(root);
    dir
}

/// The store file's decoded records, via the tolerant reader (the file is
/// ours; a torn tail here is a test bug, so `load`'s strictness would only
/// mask it — `load_tolerant` surfaces a torn write as a failed assertion).
fn file_records(store: &SessionStore, id: u64) -> Vec<CommittedRecord> {
    let report = store
        .load_tolerant(id)
        .expect("store read")
        .expect("session file exists");
    assert!(!report.torn_tail, "session file has a torn tail");
    report.records
}

fn file_header(store: &SessionStore, id: u64) -> Option<Header> {
    store
        .load_tolerant(id)
        .expect("store read")
        .expect("session file exists")
        .header
}

fn boundary_count(records: &[CommittedRecord]) -> usize {
    records
        .iter()
        .filter(|r| matches!(r.event, SessionEvent::SeedBoundary))
        .count()
}

fn drive_two_turns(mounted: &harnless_cli::boot::Mounted) {
    drive_turn(mounted, "first").unwrap();
    drive_turn(mounted, "second").unwrap();
}

/// End a composition exactly as the binary's process exit does: `shutdown`
/// unwinds the spine fiber (releasing the mirroring writer's session lock),
/// then drop. A bare `drop` leaves the registry-owned fiber — and its
/// mirror's lock — alive; the binary's routes end this way.
fn end_route(route: SessionMount) {
    let mut mounted = route.mounted;
    mounted.shutdown();
    drop(mounted);
}

/// A committed record with a fixed time — the fixture's own appends, so the
/// times are assertable.
fn fixture_record(position: usize, event: SessionEvent) -> CommittedRecord {
    CommittedRecord {
        position,
        time_ms: 1000 + position as u64,
        event,
    }
}

/// A two-record bracket file's lines.
fn file_fixture_lines() -> String {
    let records = vec![
        fixture_record(0, SessionEvent::TurnOpen),
        fixture_record(
            1,
            SessionEvent::TurnClose {
                reason: TurnEndReason::Completed,
            },
        ),
    ];
    records
        .iter()
        .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
        .collect()
}

/// The derived projection's node texts — the #64 oracle shape.
fn texts_of(history: &harnless_agent::history::History) -> Vec<String> {
    history
        .nodes()
        .iter()
        .map(|n| {
            n.blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.clone(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn project(records: &[CommittedRecord]) -> Vec<String> {
    let mut h = harnless_agent::history::History::default();
    for r in records {
        h.apply(&r.event);
    }
    texts_of(&h)
}

#[test]
fn fresh_run_mirrors_the_log_event_for_event() {
    let dir = temp_store("mirror");
    let corpus = write_corpus(
        "mirror",
        &[text_recording("answer one"), text_recording("answer two")],
    );
    let doc = support::seam_profile_store("mirror", Some(&corpus), &[], Some(&dir));
    let route = open_session(&DefaultComposer, &doc, None, None, None).unwrap();
    drive_two_turns(&route.mounted);

    let log = route
        .mounted
        .ctx
        .get::<SessionLog>()
        .expect("spine provides the log");
    let stored = file_records(&SessionStore::new(&dir), route.id);
    // Field-for-field: positions, times, events — the file is the log.
    assert_eq!(stored, log.snapshot().records);
    assert!(!stored.is_empty(), "two turns must have appended");
    // A fresh file has no header and no boundary.
    assert_eq!(file_header(&SessionStore::new(&dir), route.id), None);
    assert_eq!(boundary_count(&stored), 0);
    // Release the fixture's own handle before ending the route: the
    // composition owns the writer's release (its lock goes with the
    // composition), but the test's handle would otherwise keep the *log*
    // alive, and a release assertion must not race the fixture itself.
    drop(log);
    end_route(route);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn resume_seeds_verbatim_and_appends_past_the_seed() {
    let dir = temp_store("resume");
    let store = SessionStore::new(&dir);
    let corpus = write_corpus(
        "resume",
        &[
            text_recording("answer one"),
            text_recording("answer two"),
            text_recording("answer three"),
        ],
    );
    let doc = support::seam_profile_store("resume", Some(&corpus), &[], Some(&dir));

    // Mount, drive, end.
    let route = open_session(&DefaultComposer, &doc, None, None, None).unwrap();
    let id = route.id;
    drive_two_turns(&route.mounted);
    end_route(route);
    let before = std::fs::read(store.session_path(id)).unwrap();

    // Remount as a resume.
    let resumed = open_session(&DefaultComposer, &doc, Some(id), None, None).unwrap();
    assert_eq!(resumed.id, id, "resume keeps the id");
    let log = resumed
        .mounted
        .ctx
        .get::<SessionLog>()
        .expect("spine provides the log");
    // The seed is the stored records exactly — positions, times, events.
    let seeded = file_records(&store, id);
    assert_eq!(log.snapshot().records, seeded);
    // No new header, no boundary: a resume of a fresh file stays plain.
    assert_eq!(file_header(&store, id), None);
    assert_eq!(boundary_count(&log.snapshot().records), 0);

    // The next turn appends at positions continuing from the store, and the
    // file grows only at the tail (prefix bytes identical).
    drop(log);
    drive_turn(&resumed.mounted, "third").unwrap();
    let after = std::fs::read(store.session_path(id)).unwrap();
    assert!(after.starts_with(&before), "resume grows only at the tail");
    let grown = file_records(&store, id);
    let kept = String::from_utf8_lossy(&before)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(&grown[..kept], &seeded[..kept]);
    assert_eq!(kept, seeded.len(), "the prefix held every seeded record");
    assert!(grown.len() > seeded.len(), "the new turn appended");
    // Positions continue: the new records' positions are exactly the old len
    // onward, contiguous.
    let base = seeded.len();
    assert_eq!(
        grown[base..].iter().map(|r| r.position).collect::<Vec<_>>(),
        (base..grown.len()).collect::<Vec<_>>()
    );
    end_route(resumed);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn resume_ids_seed_from_store_max_and_never_reuse() {
    // #68's invariant, asserted with exact counts: ids seed from the store's
    // max (chunk message_ids and call_ids included in the scan) and the
    // post-resume mints are exactly max+1, max+2, … .
    let dir = temp_store("idseed");
    let store = SessionStore::new(&dir);
    let id = 4242u64;

    // A fixture whose max id (55, a chunk's message id) is NOT the last
    // record's id — the scan must see every id kind.
    let records = vec![
        fixture_record(0, SessionEvent::TurnOpen),
        fixture_record(
            1,
            SessionEvent::UserMessage(MessageRecord {
                id: MessageId(10),
                blocks: vec![ContentBlock::Text { text: "u".into() }],
                provider: None,
                model: None,
            }),
        ),
        fixture_record(
            2,
            SessionEvent::ToolCall(ToolCallRecord {
                call_id: CallId(33),
                tool: "echo".into(),
                arguments: "{}".into(),
            }),
        ),
        fixture_record(
            3,
            SessionEvent::ToolResult(ToolResultRecord {
                call_id: CallId(33),
                content: "\"ok\"".into(),
            }),
        ),
        fixture_record(
            4,
            SessionEvent::AssistantChunk(ChunkRecord {
                message_id: MessageId(55),
                block_index: 0,
                delta: "partial".into(),
            }),
        ),
        fixture_record(
            5,
            SessionEvent::AssistantMessage(MessageRecord {
                id: MessageId(20),
                blocks: vec![ContentBlock::Text { text: "a".into() }],
                provider: None,
                model: None,
            }),
        ),
        fixture_record(
            6,
            SessionEvent::TurnClose {
                reason: TurnEndReason::Completed,
            },
        ),
    ];
    let text: String = records
        .iter()
        .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
        .collect();
    std::fs::write(store.session_path(id), text).unwrap();

    let max = harnless_agent::session::max_record_id(&records);
    assert_eq!(max, 55, "the scan sees chunk/call ids, not just messages");

    let resumed = open_session(
        &DefaultComposer,
        &support::seam_profile("idseed", None, &[]),
        Some(id),
        None,
        Some(dir.clone()),
    )
    .unwrap();
    // Exact mints: 56, 57, 58 — never a reuse of 10/20/33/55.
    let minted: Vec<u64> = (0..3).map(|_| resumed.mounted.ids.message().0).collect();
    assert_eq!(minted, vec![56, 57, 58]);
    // Positions and ids are independent counters (#68 §5): the log's next
    // position is 7 while the next id is 59.
    let log = resumed
        .mounted
        .ctx
        .get::<SessionLog>()
        .expect("spine provides the log");
    assert_eq!(log.snapshot().records.len(), 7);
    assert_eq!(resumed.mounted.ids.message().0, 59);
    drop(log);
    end_route(resumed);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn resume_after_errored_turn_replays_history_and_starts_clean() {
    let dir = temp_store("errturn");
    let store = SessionStore::new(&dir);

    // Source session: one turn that ends in a provider error. The failing
    // recording's assembled blocks are empty; the TurnClose carries the
    // structured error.
    let corpus = write_corpus(
        "errturn",
        &[failing_recording(ErrorCode::ProviderFailure, "429")],
    );
    let doc = support::seam_profile_store("errturn", Some(&corpus), &[], Some(&dir));
    let route = open_session(&DefaultComposer, &doc, None, None, None).unwrap();
    let id = route.id;
    let err = drive_turn(&route.mounted, "boom").unwrap_err();
    assert_eq!(err.code, "turn-failed");
    end_route(route);
    let source = file_records(&store, id);
    assert!(
        source.iter().any(|r| matches!(
            &r.event,
            SessionEvent::TurnClose {
                reason: TurnEndReason::Error { .. }
            }
        )),
        "the errored turn's bracket is in the log"
    );

    // Resume: the loop starts clean and the next turn appends past the
    // boundary (there is none — a resume of a plain file is boundary-free).
    let corpus2 = write_corpus("errturn2", &[text_recording("recovered")]);
    let doc2 = support::seam_profile_store("errturn", Some(&corpus2), &[], Some(&dir));
    let resumed = open_session(&DefaultComposer, &doc2, Some(id), None, None).unwrap();
    drive_turn(&resumed.mounted, "again").unwrap();

    // The equality oracle (#64's projection shape): the projection of the
    // resumed process's replayed seed equals the source process's own
    // projection — history replays event-for-event.
    let after = file_records(&store, id);
    assert_eq!(&after[..source.len()], &source[..], "history replays");
    assert!(after.len() > source.len(), "the next turn appended past it");
    assert_eq!(project(&after[..source.len()]), project(&source));

    // The grown log mirrors the file exactly, and the loop started clean:
    // the new turn's bracket closes Completed, appended past the old error
    // close.
    let grown_log = resumed
        .mounted
        .ctx
        .get::<SessionLog>()
        .expect("spine provides the log");
    assert_eq!(grown_log.snapshot().records, after, "log mirrors the file");
    assert!(matches!(
        after.last().unwrap().event,
        SessionEvent::TurnClose {
            reason: TurnEndReason::Completed
        }
    ));
    let texts = project(&after);
    assert!(texts.contains(&"boom".to_string()));
    assert!(texts.contains(&"recovered".to_string()));
    drop(grown_log);
    end_route(resumed);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fork_writes_header_seed_boundary_and_freezes_source() {
    let dir = temp_store("forkseam");
    let store = SessionStore::new(&dir);

    // Source: two turns on a store-mounted composition.
    let corpus = write_corpus(
        "forkseam",
        &[text_recording("s1 one"), text_recording("s1 two")],
    );
    let doc = support::seam_profile_store("forkseam", Some(&corpus), &[], Some(&dir));
    let route = open_session(&DefaultComposer, &doc, None, None, None).unwrap();
    let s1 = route.id;
    drive_two_turns(&route.mounted);
    end_route(route);
    let s1_before = (
        std::fs::read(store.session_path(s1)).unwrap(),
        std::fs::metadata(store.session_path(s1))
            .unwrap()
            .modified()
            .unwrap(),
    );
    let s1_records = file_records(&store, s1);

    // Fork s1 → s2, drive two turns on the fork.
    let corpus2 = write_corpus(
        "forkseam2",
        &[text_recording("s2 one"), text_recording("s2 two")],
    );
    let doc2 = support::seam_profile_store("forkseam", Some(&corpus2), &[], Some(&dir));
    let forked = open_session(&DefaultComposer, &doc2, None, Some(s1), None).unwrap();
    let s2 = forked.id;
    assert_ne!(s1, s2);
    drive_two_turns(&forked.mounted);
    end_route(forked);

    // s2 = header(forked_from=s1) + s1's records verbatim + exactly one
    // boundary + the fork's own turns.
    let s2_records = file_records(&store, s2);
    assert_eq!(file_header(&store, s2), Some(Header { forked_from: s1 }));
    assert_eq!(s2_records[..s1_records.len()], s1_records[..]);
    assert!(matches!(
        s2_records[s1_records.len()].event,
        SessionEvent::SeedBoundary
    ));
    assert_eq!(boundary_count(&s2_records), 1);
    assert!(s2_records.len() > s1_records.len() + 1);

    // s1 bytes and mtime unchanged by the fork and the fork's turns.
    let s1_after = (
        std::fs::read(store.session_path(s1)).unwrap(),
        std::fs::metadata(store.session_path(s1))
            .unwrap()
            .modified()
            .unwrap(),
    );
    assert_eq!(s1_before.0, s1_after.0, "fork freezes the source bytes");
    assert_eq!(s1_before.1, s1_after.1, "fork freezes the source mtime");

    // s2's ids continue from s1's max. #68 §4 anti-pin (lives here on
    // purpose): ids are per-session identity — a cross-session id collision
    // is NOT asserted against anywhere in this suite; the same id value may
    // legitimately appear in two different session files.
    let max1 = harnless_agent::session::max_record_id(&s1_records);
    let max2 = harnless_agent::session::max_record_id(&s2_records[s1_records.len() + 1..]);
    assert!(
        max2 > max1,
        "the fork's own ids continue from the source's max"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn resume_of_resume_and_fork_of_resume_stay_unambiguous() {
    let dir = temp_store("chain");
    let store = SessionStore::new(&dir);
    let doc = support::seam_profile("chain", None, &[]);

    // Fresh session file with a completed bracket.
    let id = 777u64;
    std::fs::write(store.session_path(id), file_fixture_lines()).unwrap();
    let base = file_records(&store, id);

    // resume → the loader returns its own records, 0 boundaries.
    let r1 = open_session(&DefaultComposer, &doc, Some(id), None, Some(dir.clone())).unwrap();
    assert_eq!(boundary_count(&file_records(&store, id)), 0);
    assert_eq!(file_records(&store, id), base);
    end_route(r1);

    // resume-of-a-resume: still 0 boundaries, same records.
    let r2 = open_session(&DefaultComposer, &doc, Some(id), None, Some(dir.clone())).unwrap();
    assert_eq!(file_records(&store, id), base);
    end_route(r2);

    // fork-of-a-resume: the target carries exactly 1 boundary, the source
    // keeps 0, and each file's loader returns its own records.
    let f = open_session(&DefaultComposer, &doc, None, Some(id), Some(dir.clone())).unwrap();
    let fid = f.id;
    end_route(f);
    assert_eq!(boundary_count(&file_records(&store, id)), 0);
    let forked = file_records(&store, fid);
    assert_eq!(boundary_count(&forked), 1);
    assert_eq!(&forked[..base.len()], &base[..]);
    // `is_seeded` stays contains-≥1: the fork file is seeded, the source is
    // not, regardless of boundary multiplicity elsewhere.
    assert!(store.load(fid).unwrap().unwrap().is_seeded());
    assert!(!store.load(id).unwrap().unwrap().is_seeded());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lock_held_fails_loudly() {
    // #70 §6: two handles, one process. The lock fixture is the seam's own
    // FileLock::hold — never a real spawn race.
    let dir = temp_store("lockheld");
    let store = SessionStore::new(&dir);
    let id = 5150u64;
    std::fs::write(store.session_path(id), file_fixture_lines()).unwrap();

    let holder = FileLock::hold(&dir, id).unwrap();
    // A second open of the same session refuses, naming the holder pid.
    let err = store.open_existing(id).unwrap_err();
    assert_eq!(err.code, "session-locked");
    assert!(
        err.message
            .contains(&format!("process {}", std::process::id())),
        "the refusal names the holder: {}",
        err.message
    );
    // Compaction on the same fixture refuses and leaves bytes unchanged.
    let before = std::fs::read(store.session_path(id)).unwrap();
    let err = store.compact(id).unwrap_err();
    assert_eq!(err.code, "session-locked");
    assert_eq!(
        std::fs::read(store.session_path(id)).unwrap(),
        before,
        "a refused compaction leaves the log untouched"
    );
    // Releasing is dropping; then the open succeeds.
    drop(holder);
    drop(store.open_existing(id).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn torn_tail_dropped_then_truncated() {
    let dir = temp_store("torn");
    let store = SessionStore::new(&dir);
    let id = 313u64;
    let good = file_fixture_lines();
    std::fs::write(store.session_path(id), format!("{good}{{\"pos")).unwrap();

    // The tolerant load returns the prefix and names the tear.
    let report = store.load_tolerant(id).unwrap().unwrap();
    assert!(report.torn_tail);
    assert_eq!(report.records.len(), 2);
    // The strict load refuses until repaired.
    assert_eq!(store.load(id).unwrap_err().code, "session-corrupt");

    // The first writer open repairs under its lock: truncate to the good
    // byte boundary.
    let good_len = good.as_bytes().len() as u64;
    let writer = store.open_existing(id).unwrap();
    assert_eq!(
        std::fs::metadata(store.session_path(id)).unwrap().len(),
        good_len
    );
    drop(writer);

    // The file re-loads clean.
    let loaded = store.load(id).unwrap().unwrap();
    assert_eq!(loaded.records.len(), 2);
    assert_eq!(loaded.records[0], fixture_record(0, SessionEvent::TurnOpen));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mid_file_corruption_refuses() {
    let dir = temp_store("midfile");
    let store = SessionStore::new(&dir);
    let id = 404u64;
    let good = file_fixture_lines();
    // Corrupt line BEFORE the last — mid-file, not tail.
    let lines: Vec<&str> = good.lines().collect();
    let broken = format!("{}\nNOT JSON\n{}\n", lines[0], lines[1]);
    std::fs::write(store.session_path(id), broken).unwrap();

    let err = store.load(id).unwrap_err();
    assert_eq!(err.code, "session-corrupt");
    assert!(err.message.contains(&id.to_string()), "{}", err.message);
    // Resume and fork both refuse. `match`, not `unwrap_err`: the Ok arm's
    // SessionMount is not Debug.
    match open_session(
        &DefaultComposer,
        &support::seam_profile("midfile", None, &[]),
        Some(id),
        None,
        Some(dir.clone()),
    ) {
        Ok(route) => {
            end_route(route);
            panic!("resume of a corrupt session must refuse")
        }
        Err(err) => assert_eq!(err.code, "session-corrupt"),
    }
    match open_session(
        &DefaultComposer,
        &support::seam_profile("midfile", None, &[]),
        None,
        Some(id),
        Some(dir.clone()),
    ) {
        Ok(route) => {
            end_route(route);
            panic!("fork of a corrupt session must refuse")
        }
        Err(err) => assert_eq!(err.code, "session-corrupt"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mint_collision_refuses_not_interleaves() {
    let dir = temp_store("mint");
    let store = SessionStore::new(&dir);
    let id = 99u64;

    // A held writer + create_new same id ⇒ session-locked (never an append
    // onto the foreign file). The first writer stays alive across the
    // refusals — it *is* the fixture's "live session".
    let first = store.create_new(id).unwrap();
    let err = store.create_new(id).unwrap_err();
    assert_eq!(err.code, "session-locked");
    let err = store.open_existing(id).unwrap_err();
    assert_eq!(err.code, "session-locked");
    // A mint collision never interleaves: the file is still exactly what the
    // first writer wrote (empty).
    assert_eq!(std::fs::metadata(store.session_path(id)).unwrap().len(), 0);
    drop(first);

    // The file exists after the holder released: a fresh create_new is the
    // O_EXCL refusal, not a silent reopen.
    let err = store.create_new(id).unwrap_err();
    assert_eq!(err.code, "session-locked");

    // An injected-id source that always collides exhausts the retry bound
    // with a named, bounded failure.
    let err =
        mint_with(|_id| Err::<(), _>(SessionError::new("session-locked", "always"))).unwrap_err();
    assert_eq!(err.code, "session-mint-failed");
    assert!(err.message.contains("8 times"), "{}", err.message);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mount_seeded_writer_frees_when_the_mount_ends() {
    // The composition owns the writer's release: ending it frees the lock
    // even while service handles outlive it (boot.rs's rule). The fixture
    // deliberately keeps both `Arc` handles past the composition — the
    // exact shape that used to wedge the session file.
    let dir = temp_store("freedrop");
    let store = SessionStore::new(&dir);
    let id = 606u64;
    std::fs::write(store.session_path(id), file_fixture_lines()).unwrap();

    let route = open_session(
        &DefaultComposer,
        &support::seam_profile("freedrop", None, &[]),
        Some(id),
        None,
        Some(dir.clone()),
    )
    .unwrap();
    // While mounted, a second handle refuses.
    assert_eq!(store.open_existing(id).unwrap_err().code, "session-locked");
    // Hold the log and the loop past the composition's end.
    let log = route
        .mounted
        .ctx
        .get::<SessionLog>()
        .expect("spine provides the log");
    let loop_ = route
        .mounted
        .ctx
        .get::<harnless_agent::loop_::AgentLoop>()
        .expect("spine provides the loop");
    end_route(route);
    // The lock is free with both handles still alive…
    drop(store.open_existing(id).unwrap());
    // …and a surviving handle's append fails loudly, never silently.
    assert!(
        !log.append(SessionEvent::UserMessage(MessageRecord {
            id: MessageId(999),
            blocks: vec![],
            provider: None,
            model: None,
        })),
        "a post-composition append must be refused"
    );
    drop(loop_);
    drop(log);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn failed_seeded_mount_releases_the_lock() {
    // A mount that fails *after* the spine took the session lock must
    // unwind the writer with it — "a failed boot unwinds, never leaks".
    // The injection is the plan itself: a replay script that cannot be
    // read makes `build_adapter` fail right after `mount_spine` succeeded,
    // which is the post-apply window where the writer would otherwise
    // outlive every owner (no `Mounted` ever exists to close it).
    let dir = temp_store("failmount");
    let store = SessionStore::new(&dir);
    let id = 606u64;
    std::fs::write(store.session_path(id), file_fixture_lines()).unwrap();

    let doc = support::seam_profile_store(
        "failmount",
        Some(std::path::Path::new("/nonexistent/missing-script.json")),
        &[],
        Some(&dir),
    );
    let opened = open_session(&DefaultComposer, &doc, Some(id), None, None);
    let err = match opened {
        Ok(_) => panic!("a missing script must refuse the mount"),
        Err(err) => err,
    };
    assert_eq!(err.code, "bad-script", "the mount fails loudly, named");
    // The session is not wedged: the lock went with the failed boot.
    drop(
        store
            .open_existing(id)
            .expect("a failed mount must leave the lock free"),
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn failed_fork_mount_leaves_no_orphan() {
    // #67 §5's no-orphan rule at the mount-failure window: `create_fork`
    // writes the target before the composition mounts, so a mount that
    // fails after it (here: an unreadable replay script) must abandon the
    // target — otherwise `sessions list` renders a fork that never ran.
    let dir = temp_store("forkorphan");
    let store = SessionStore::new(&dir);
    let source = 606u64;
    std::fs::write(store.session_path(source), file_fixture_lines()).unwrap();

    let doc = support::seam_profile_store(
        "forkorphan",
        Some(std::path::Path::new("/nonexistent/missing-script.json")),
        &[],
        Some(&dir),
    );
    let opened = open_session(&DefaultComposer, &doc, None, Some(source), None);
    let err = match opened {
        Ok(_) => panic!("a missing script must refuse the fork mount"),
        Err(err) => err,
    };
    assert_eq!(err.code, "bad-script");
    // The store holds exactly the source: no target file, no lock sibling.
    let ids: Vec<u64> = store.list().iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![source], "the failed fork left no orphan");
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // The lock sibling is shared state, not an orphan: `read_locked` (the
    // fork route's source read) creates it, and it is inert once no
    // process holds the flock — the next open reuses it. The orphan the
    // rule forbids is the *session file* (and a created fork target's
    // pair); the source's lock residue is not one.
    assert_eq!(
        leftovers,
        vec![format!("{source}.jsonl"), format!("{source}.jsonl.lock")],
        "only the source remains (its lock sibling is inert shared state)"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn failed_fresh_mount_abandons_the_mint() {
    // The fresh route's mint creates the file before the mount; a failed
    // mount must leave neither a zero-byte phantom nor a lock sibling.
    let dir = temp_store("freshorphan");

    let doc = support::seam_profile_store(
        "freshorphan",
        Some(std::path::Path::new("/nonexistent/missing-script.json")),
        &[],
        Some(&dir),
    );
    let opened = open_session(&DefaultComposer, &doc, None, None, None);
    assert!(opened.is_err(), "the missing script refuses the mount");
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        leftovers.is_empty(),
        "the abandoned mint left {leftovers:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
