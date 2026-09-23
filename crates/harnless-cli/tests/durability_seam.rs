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
    assert!(matches!(opened, Err(_)), "the missing script refuses the mount");
    // The reference composer's classification hands the *unconsumed
    // writer* back, and `unwind_created` abandons through it — the
    // writer-shaped arm removes both the file and its lock sibling, so
    // the leftover pair below is its observable. The writer arm's own
    // seam is `failed_fork_mount_leaves_no_orphan`, which drives the
    // same `unwind_created` path on the fork route.
    drop(opened);
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

// ---------------------------------------------------------------------------
// The ConfigComposer (the binary's composition root) route: a seeded mount
// that fails *after* its spine mounted must unwind the live composition —
// the registry pins the spine plugin, so the body's Arc drop alone never
// runs `SpineMount::dispose`, and a live mirror would strand the session
// lock for the process's life.
// ---------------------------------------------------------------------------

fn config_store_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hrls-cfgstore-{tag}-{}", std::process::id()))
}

fn config_composer_with_store(
    tag: &str,
    store_dir: &std::path::Path,
) -> harnless_cli::config_boot::ConfigComposer {
    let cfg = std::env::temp_dir().join(format!("hrls-cfg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(cfg.join("profiles")).unwrap();
    std::fs::create_dir_all(cfg.join("bundles")).unwrap();
    // A composed profile whose model row names a corpus that does not
    // exist: the spine mounts (taking the session lock through the seed's
    // writer), and `build_adapter` is the failure *after* it — the exact
    // window the DefaultComposer seam tests pin, on the production route.
    std::fs::write(
        cfg.join("profiles").join("p.yml"),
        format!(
            "name: p\nrows:\n- id: spine\n  plugin: spine\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: scripted\n    script: /nonexistent/missing-script.json\n- id: store\n  plugin: storage-jsonl\n  config:\n    dir: {}\n",
            store_dir.display()
        ),
    )
    .unwrap();
    harnless_cli::config_boot::ConfigComposer::new(
        std::sync::Arc::new(harnless_cli::config_boot::LayeredStore::new(Some(cfg))),
        harnless_cli::config_boot::config::subst::Subst::new(),
    )
}

#[test]
fn config_route_failed_resume_mount_releases_the_lock() {
    let dir = config_store_dir("resumelock");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = SessionStore::new(&dir);
    let source = 707u64;
    std::fs::write(store.session_path(source), file_fixture_lines()).unwrap();

    let composer = config_composer_with_store("resumelock", &dir);
    // `compose` is the trait method (the binary's route); the trait is in
    // scope only for this call.
    use harnless_cli::boot::BootComposer as _;
    let doc = composer.compose("p", None).expect("profile composes");
    let opened = super_support_open(&composer, &doc);
    let err = match opened {
        Ok(_) => panic!("a missing script must refuse the resume mount"),
        Err(err) => err,
    };
    // The mount reaches the model's script through `build_adapter`; the
    // named code is the one the adapter's loader gives an unreadable
    // corpus (the same code the DefaultComposer seam pins).
    assert!(
        matches!(err.code, "bad-script" | "plugin-build-failed"),
        "the mount must refuse at the model, not the lock: {}",
        err.code
    );

    // The lock is gone: a fresh open of the same session gets past the
    // lock and fails at the model again — the point is that taking the
    // lock is possible at all. The first attempt's `open_existing` repaired
    // the fixture's torn tail under its lock, so the second attempt loads
    // cleanly and reaches the model too.
    let again = super_support_open(&composer, &doc);
    match again {
        Ok(_) => panic!("the second mount must fail the same way"),
        Err(err) => assert!(
            matches!(err.code, "bad-script" | "plugin-build-failed"),
            "the second attempt must reach the model too, not the lock: {}",
            err.code
        ),
    }
    // And the file is untouched: a resume never abandons its session.
    let ids: Vec<u64> = store.list().iter().map(|m| m.id).collect();
    assert_eq!(
        ids,
        vec![source],
        "the resume's file survives the failed mount"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn super_support_open(
    composer: &harnless_cli::config_boot::ConfigComposer,
    doc: &harnless_cli::profile::ProfileDoc,
) -> Result<harnless_cli::session::SessionMount, harnless_cli::CliError> {
    harnless_cli::session::open_session(composer, doc, Some(707), None, None)
}

#[test]
fn toolless_seeded_mount_keeps_one_writer_owner() {
    // The guard's disarm rule on the tool-less Ok path: a seeded mount
    // whose profile declares no tools must leave the writer with exactly
    // one owner (the mirror). If the guard rolled it back into the seed
    // cell on the Ok return, the *next* mount's failure classification
    // would find a writer there — and a created-file seed would abandon a
    // file a live mirror is still appending to. Observable shape: mount
    // fresh (no tools), drive a turn, drop it, then resume the same id —
    // the resume must see the first turn's records, which only holds if
    // the first mount's mirror owned the writer alone and wrote them.
    let dir = temp_store("toollessowner");
    let corpus = support::write_corpus("toollessowner", &[text_recording("hello")]);
    let doc = support::seam_profile_store("toollessowner", Some(&corpus), &[], Some(&dir));
    let store = SessionStore::new(&dir);

    let first = open_session(&DefaultComposer, &doc, None, None, None).expect("fresh mounts");
    let id = first.id;
    drive_turn(&first.mounted, "hi").expect("turn runs");
    drop(first);
    // The turn's records are in the file: the mirror wrote them (one owner,
    // no rollback stole the writer).
    let stored = store.load(id).expect("loads").expect("session exists");
    assert!(stored.records.len() >= 2, "the turn persisted");

    // A second fresh mount that fails *after* minting (unreadable script)
    // must abandon its own mint — and must not touch the first session.
    let bad = support::seam_profile_store(
        "toollessowner",
        Some(std::path::Path::new("/nonexistent/missing-script.json")),
        &[],
        Some(&dir),
    );
    // The double-owner shape the disarm rule forbids: the failed mount's
    // `MountSeed::clone` inherits the *shared* writer cell, and a guard
    // that never disarmed leaves the first mount's live writer in it. The
    // mint retry then sees a full cell, and the created-by-mount failure
    // arm abandons the writer — unlinking the first session's file under
    // its live mirror. The first session must survive its sibling's
    // failed mount.
    let opened = open_session(&DefaultComposer, &bad, None, None, None);
    assert!(opened.is_err(), "the missing script refuses the mount");
    let ids: Vec<u64> = store.list().iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![id], "only the first session survives");
    // And it stays loadable: the mirror's file was never unlinked.
    let stored = store.load(id).expect("loads").expect("session survives");
    assert!(
        stored.records.len() >= 2,
        "the first session's records survive"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn panic_after_spine_mount_disposes_the_composition_and_releases_the_lock() {
    // SPINE_UNWIND's rule: the wrapper's Err arm is not the only unwind
    // path. A panic *after* the spine composed (the registry pins the
    // plugin, so the body's `Arc<SpineMount>` drop never disposes) must
    // still tear the composition down and release the session lock —
    // the round-two defect's shape, on the panic path.
    //
    // The panic is injected at the seam the guard exists for: a
    // `BootComposer` whose `mount_seeded` *is* the wrapper — it mounts a
    // real seeded spine (taking the session lock through the mirror) and
    // then panics at the fallible step after the spine, exactly where
    // `config_route_failed_resume_mount_releases_the_lock` pins the model
    // step's Err path. The guard's Drop must run the same dispose the Err
    // arm runs.
    use harnless_cli::boot::{BootComposer, MountSeed};
    let dir = config_store_dir("panicunwind");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = SessionStore::new(&dir);
    let source = 4061u64;
    std::fs::write(store.session_path(source), file_fixture_lines()).unwrap();

    let cfg = config_store_dir("panicunwind-cfg");
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(cfg.join("profiles")).unwrap();
    std::fs::create_dir_all(cfg.join("bundles")).unwrap();
    let corpus = support::write_corpus("panicunwind", &[text_recording("(idle)")]);
    std::fs::write(
        cfg.join("profiles").join("p.yml"),
        format!(
            "name: p\nrows:\n- id: spine\n  plugin: spine\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: scripted\n    script: {}\n- id: store\n  plugin: storage-jsonl\n  config:\n    dir: {}\n",
            corpus.display(),
            dir.display()
        ),
    )
    .unwrap();
    let composer = harnless_cli::config_boot::ConfigComposer::new(
        std::sync::Arc::new(harnless_cli::config_boot::LayeredStore::new(Some(cfg))),
        harnless_cli::config_boot::config::subst::Subst::new(),
    );
    let doc = composer.compose("p", None).expect("profile composes");

    // The seam: the panic stands in for the wrapper body's post-spine
    // fallible step (the model step
    // `config_route_failed_resume_mount_releases_the_lock` pins on its
    // Err path). It is injected at the composer's own seam: a
    // `BootComposer` adapter that mounts a real seeded spine through the
    // production machinery — the spine composes, the session lock is
    // taken through the mirror — and then panics at the post-spine step.
    // The unwind must dispose what mounted and release the flock; the
    // round-two defect's shape, on the panic path.
    struct PanicAtModelStep {
        inner: harnless_cli::config_boot::ConfigComposer,
    }
    impl BootComposer for PanicAtModelStep {
        fn profiles(&self) -> Vec<String> {
            self.inner.profiles()
        }
        fn compose(
            &self,
            name: &str,
            patch: Option<&str>,
        ) -> Result<harnless_cli::profile::ProfileDoc, harnless_cli::CliError> {
            self.inner.compose(name, patch)
        }
        fn dump(&self, doc: &harnless_cli::profile::ProfileDoc) -> String {
            self.inner.dump(doc)
        }
        fn mount(
            &self,
            doc: &harnless_cli::profile::ProfileDoc,
        ) -> Result<harnless_cli::boot::Mounted, harnless_cli::CliError> {
            self.inner.mount(doc)
        }
        fn mount_seeded(
            &self,
            doc: &harnless_cli::profile::ProfileDoc,
            seed: MountSeed,
        ) -> Result<harnless_cli::boot::Mounted, harnless_cli::boot::MountFailure> {
            // The production row-mount machinery composes the spine and
            // takes the session lock through the mirror; the panic is the
            // fallible step *after* the spine. The seeded spine mounts
            // through `mount_spine_for` — the same call the production
            // wrapper's body makes — so the live composition the unwind
            // must tear down is real.
            let config_doc = self
                .inner
                .compose_config(&doc.name, &[])
                .expect("the plan re-composes");
            let wiring = match &doc.tools[..] {
                [] => harnless_cli::boot::ToolsWiring::None,
                declared => harnless_cli::boot::ToolsWiring::AutoAllow {
                    declared: declared.to_vec(),
                },
            };
            let _spine = self
                .inner
                .mount_spine_for_for_test(&config_doc, wiring, seed)
                .expect("the seeded spine mounts");
            panic!("the model step panicked after the spine mounted");
        }
    }

    let stored = store.load(source).expect("loads").expect("session exists");
    let writer = store.open_existing(source).expect("writer");
    let seed = MountSeed {
        session: harnless_seams::SessionId(source),
        records: Some(stored.records),
        id_seed: 0,
        writer: std::sync::Arc::new(std::sync::Mutex::new(Some(writer))),
        created_by_mount: false,
    };
    let wrapper = PanicAtModelStep { inner: composer };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match wrapper.mount_seeded(&doc, seed) {
            Ok(_) => unreachable!("the seam panics"),
            Err(failure) => panic!("the seam must panic, not fail: {}", failure.err.code),
        }
    }))
    .expect_err("the seam's post-spine step panics");
    drop(outcome);

    // The guard's Drop disposed the live spine and closed its mirror:
    // the flock is free for the next open. Without the guard, the
    // registry-pinned spine keeps the mirror's writer — and the flock —
    // for the process's life, and this probe stays `session-locked`.
    FileLock::hold(&dir, source)
        .expect("the panic path must dispose the composition and release the lock");
    assert!(
        store.load(source).expect("load").is_some(),
        "a resume's file is never abandoned"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(config_store_dir("panicunwind-cfg"));
}

#[test]
fn panic_after_spine_mount_abandons_a_created_file() {
    // The guard's classification rule: the panic unwind is not only the
    // spine dispose. A mint/fork route (`created_by_mount == true`) whose
    // post-spine step panics must abandon the file this boot created —
    // the round-twelve defect's shape: the guard disposed the composition
    // and released the lock, but the classification ran only on the Err
    // path, so the panic left a resumable phantom plus lock residue.
    // Same seam as `panic_after_spine_mount_disposes_the_composition_and_releases_the_lock`,
    // with a created file instead of a resume.
    use harnless_cli::boot::{BootComposer, MountSeed};
    let dir = config_store_dir("panicorphan");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = SessionStore::new(&dir);

    let cfg = config_store_dir("panicorphan-cfg");
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(cfg.join("profiles")).unwrap();
    std::fs::create_dir_all(cfg.join("bundles")).unwrap();
    let corpus = support::write_corpus("panicorphan", &[text_recording("(idle)")]);
    std::fs::write(
        cfg.join("profiles").join("p.yml"),
        format!(
            "name: p\nrows:\n- id: spine\n  plugin: spine\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: scripted\n    script: {}\n- id: store\n  plugin: storage-jsonl\n  config:\n    dir: {}\n",
            corpus.display(),
            dir.display()
        ),
    )
    .unwrap();
    let composer = std::sync::Arc::new(harnless_cli::config_boot::ConfigComposer::new(
        std::sync::Arc::new(harnless_cli::config_boot::LayeredStore::new(Some(cfg.clone()))),
        harnless_cli::config_boot::config::subst::Subst::new(),
    ));
    let doc = composer.compose("p", None).expect("profile composes");

    struct PanicAtModelStep {
        inner: std::sync::Arc<harnless_cli::config_boot::ConfigComposer>,
    }
    impl BootComposer for PanicAtModelStep {
        fn profiles(&self) -> Vec<String> {
            self.inner.profiles()
        }
        fn compose(
            &self,
            name: &str,
            patch: Option<&str>,
        ) -> Result<harnless_cli::profile::ProfileDoc, harnless_cli::CliError> {
            self.inner.compose(name, patch)
        }
        fn dump(&self, doc: &harnless_cli::profile::ProfileDoc) -> String {
            self.inner.dump(doc)
        }
        fn mount(
            &self,
            doc: &harnless_cli::profile::ProfileDoc,
        ) -> Result<harnless_cli::boot::Mounted, harnless_cli::CliError> {
            self.inner.mount(doc)
        }
        fn mount_seeded(
            &self,
            doc: &harnless_cli::profile::ProfileDoc,
            seed: MountSeed,
        ) -> Result<harnless_cli::boot::Mounted, harnless_cli::boot::MountFailure> {
            // The seam's panic stands in for the body's fallible step
            // *after* the spine mounted (the model step). The crate's
            // helper drives it through the production wrapper's entry —
            // the wrapper arms the panic guard and records the seed's
            // cell before the body runs, so the panic unwinds past the
            // armed guard (the shape the guard's dispose must reach).
            harnless_cli::config_boot::mount_seeded_panicking_after_spine_for_test(
                &self.inner,
                doc,
                seed,
                harnless_cli::boot::ToolsWiring::None,
            )
        }
    }

    // A created file (the mint shape), not a resume: the panic must leave
    // nothing behind.
    let (id, writer) = mint_with(|id| store.create_new(id)).expect("mint");
    let seed = MountSeed {
        session: harnless_seams::SessionId(id),
        records: Some(vec![]),
        id_seed: 0,
        writer: std::sync::Arc::new(std::sync::Mutex::new(Some(writer))),
        created_by_mount: true,
    };
    let wrapper = PanicAtModelStep { inner: composer };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match wrapper.mount_seeded(&doc, seed) {
            Ok(_) => unreachable!("the seam panics"),
            Err(failure) => panic!("the seam must panic, not fail: {}", failure.err.code),
        }
    }))
    .expect_err("the seam's post-spine step panics");
    drop(outcome);

    // The guard's Drop disposed the composition (lock free) *and*
    // classified the cell: the created file abandoned, no phantom.
    // The leftovers are listed *before* the lock probe: `FileLock::hold`
    // creates the lock sibling it probes with, so a probe-first order
    // would manufacture the residue the assertion forbids.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "the panic path orphaned the created file: {leftovers:?}"
    );
    FileLock::hold(&dir, id)
        .expect("the panic path must dispose the composition and release the lock");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&cfg);
}

#[test]
fn config_route_pre_spine_failure_never_disposes_a_live_sibling() {
    // LAST_MOUNTED_SPINE's staleness rule: a *successful* seeded mount
    // clears the slot, so a later mount that fails *before* composing its
    // own spine cannot upgrade a stale handle and dispose the healthy
    // composition. Shape: mount a seeded spine directly (live, holding
    // the session lock through its mirror); a second seeded mount fails
    // *before* its own spine composes (the source is locked, so the fork
    // route refuses at `read_locked`); the first composition's lock must
    // survive. With a stale slot, the wrapper's failure arm disposes the
    // healthy spine and the lock goes with it.
    use harnless_cli::boot::{BootComposer as _, MountSeed};
    let dir = config_store_dir("stalespine");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = SessionStore::new(&dir);
    let source = 808u64;
    std::fs::write(store.session_path(source), file_fixture_lines()).unwrap();

    let cfg = std::env::temp_dir().join(format!("hrls-cfg-stalespine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(cfg.join("profiles")).unwrap();
    std::fs::create_dir_all(cfg.join("bundles")).unwrap();
    let corpus = support::write_corpus("stalespine", &[text_recording("(idle)")]);
    std::fs::write(
        cfg.join("profiles").join("p.yml"),
        format!(
            "name: p\nrows:\n- id: spine\n  plugin: spine\n- id: model\n  plugin: llm-replay\n  config:\n    kind: replay\n    provider: scripted\n    script: {}\n- id: store\n  plugin: storage-jsonl\n  config:\n    dir: {}\n",
            corpus.display(),
            dir.display()
        ),
    )
    .unwrap();
    let composer = harnless_cli::config_boot::ConfigComposer::new(
        std::sync::Arc::new(harnless_cli::config_boot::LayeredStore::new(Some(cfg))),
        harnless_cli::config_boot::config::subst::Subst::new(),
    );
    let doc = composer.compose("p", None).expect("profile composes");

    // A live seeded mount through the wrapper: the composition holds the
    // session lock. The wrapper records the spine in LAST_MOUNTED_SPINE
    // and clears the slot on success.
    let stored = store.load(source).expect("loads").expect("session exists");
    let writer = store.open_existing(source).expect("writer");
    let seed = MountSeed {
        session: harnless_seams::SessionId(source),
        records: Some(stored.records),
        id_seed: 0,
        writer: std::sync::Arc::new(std::sync::Mutex::new(Some(writer))),
        created_by_mount: false,
    };
    let mounted = match composer.mount_seeded(&doc, seed) {
        Ok(mounted) => mounted,
        Err(failure) => panic!("the seeded mount must succeed: {}", failure.err.code),
    };

    // A second seeded mount fails *pre-spine*: the fork route's source
    // read meets the live lock and refuses before any target exists.
    let opened = harnless_cli::session::open_session(&composer, &doc, None, Some(source), None);
    let err = match opened {
        Ok(_) => panic!("a locked source must refuse the fork"),
        Err(err) => err,
    };
    assert_eq!(err.code, "session-locked");

    // The healthy composition still holds the lock — the failed mount
    // disposed nothing.
    let probe = FileLock::hold(&dir, source);
    match probe {
        Ok(_) => panic!("the failed fork must not release the live mount's lock"),
        Err(e) => assert_eq!(e.code, "session-locked"),
    }
    drop(mounted);
    FileLock::hold(&dir, source)
        .ok()
        .expect("lock free after the mount drops");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_route_pre_spine_failure_abandons_the_created_file() {
    // The wrapper's shared-cell rule: the seed's writer cell outlives the
    // body's seed copy, so a mount that fails BEFORE the spine row mounts
    // still classifies the unconsumed writer and abandons the file this
    // boot created. Shape: a profile with a store row but no spine row —
    // the seed can never reach a spine, so `mount_spine_for` refuses
    // pre-spine. The created file and its lock sibling must both be gone
    // when the mount fails (the round-ten defect: the wrapper took the
    // seed out of its own slot, the body's `?` dropped the writer, and
    // the route's by-id abandon left the orphan).
    use harnless_cli::boot::{BootComposer as _, MountSeed};
    let dir = config_store_dir("presine");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = SessionStore::new(&dir);

    let cfg = config_store_dir("presine-cfg");
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(cfg.join("profiles")).unwrap();
    std::fs::write(
        cfg.join("profiles").join("p.yml"),
        format!(
            "name: p\nrows:\n- id: store\n  plugin: storage-jsonl\n  config:\n    dir: {}\n",
            dir.display()
        ),
    )
    .unwrap();
    let composer = harnless_cli::config_boot::ConfigComposer::new(
        std::sync::Arc::new(harnless_cli::config_boot::LayeredStore::new(Some(
            cfg.clone(),
        ))),
        harnless_cli::config_boot::config::subst::Subst::new(),
    );
    let doc = composer.compose("p", None).expect("profile composes");

    let (id, writer) = mint_with(|id| store.create_new(id)).expect("mint");
    let seed = MountSeed {
        session: harnless_seams::SessionId(id),
        records: Some(vec![]),
        id_seed: 0,
        writer: std::sync::Arc::new(std::sync::Mutex::new(Some(writer))),
        created_by_mount: true,
    };
    let failure = match composer.mount_seeded(&doc, seed) {
        Ok(_) => panic!("a seeded mount with no spine row must refuse"),
        Err(failure) => failure,
    };
    assert_eq!(failure.err.code, "mount-failed");
    // The config route's classification abandons a created file *at the
    // mount failure* (the cell's writer never rides out here): the outcome
    // is Ok, the writer slot is empty, and the file is gone before the
    // error returns. The pre-fix shape lost the cell, fell to the route's
    // by-id abandon, and left the file plus its lock sibling.
    assert!(
        failure.unconsumed_writer.is_none(),
        "the classification owns the created file's writer"
    );
    assert!(
        failure.abandon_outcome.is_ok(),
        "{:?}",
        failure.abandon_outcome
    );
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "pre-spine failure orphaned: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&cfg);
}
