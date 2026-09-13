//! Durability / replay contract suite for the session log (#18).
//!
//! Pins the five guarantees of decision 04 against the public surface of
//! `harnless-agent`:
//!
//! 1. **Round-trip**: `load(save(events))` returns the exact appended events,
//!    byte-for-byte after a JSON round-trip — including a backend that
//!    coalesces streamed chunks.
//! 2. **Fork boundary**: a fork prefix ending inside an open turn is refused;
//!    child metadata records parentage, seed length, and the inherited
//!    working directory.
//! 3. **Crash recovery**: an orphaned open turn closes as `interrupted`
//!    without touching earlier records, and the loop itself never emits
//!    `interrupted`.
//! 4. **No-grow-on-reopen**: reopening an untouched session does not grow the
//!    log; the seed marker distinguishes current-process writes from a
//!    crash-left-open bracket.
//! 5. **Golden-file corpus**: recorded-session fixtures replayed through
//!    `load` and asserted.
//!
//! Per the issue-18 contract, a guarantee the implementation genuinely
//! violates is pinned by an `#[ignore]`d test carrying an `ISSUE-18` comment
//! rather than a lib fix.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;

use harnless_agent::{
    AgentLoop, ChunkRecord, ContentBlock, DriverOutcome, LoadedLog, MessageRecord, SessionEvent,
    SessionLog, SessionPersistence, Spine, ToolCallRecord, ToolResultRecord, TurnEndReason,
};
use harnless_runtime::context::Context;
use harnless_runtime::events::EventRegistry;
use harnless_runtime::fiber::Fiber;
use harnless_runtime::plugin::Registry;
use harnless_seams::{CallId, MessageId, SessionId};
use serde::{Deserialize, Serialize};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

/// Canonical JSON encoding of an event (the round-trip yardstick).
fn json(event: &SessionEvent) -> String {
    serde_json::to_string(event).unwrap()
}

/// A realistic mixed batch: every event kind, unicode, embedded quotes,
/// empty strings, and a streamed-chunk run.
fn mixed_batch() -> Vec<SessionEvent> {
    let user = MessageRecord {
        id: MessageId(1),
        blocks: vec![
            ContentBlock::Text {
                text: "héllo \"world\" — 世界".into(),
            },
            ContentBlock::Reasoning { text: "".into() },
        ],
        provider: None,
        model: None,
    };
    let chunks = ["READM", "E.md, ", "src/", ""]
        .into_iter()
        .map(|delta| {
            SessionEvent::AssistantChunk(ChunkRecord {
                message_id: MessageId(2),
                block_index: 0,
                delta: delta.into(),
            })
        })
        .collect::<Vec<_>>();
    let assembled = MessageRecord {
        id: MessageId(2),
        blocks: vec![ContentBlock::Text {
            text: "README.md, src/".into(),
        }],
        provider: Some("replay".into()),
        model: Some("test-model".into()),
    };
    let mut events = vec![
        SessionEvent::UserMessage(user),
        SessionEvent::TurnOpen,
        SessionEvent::StepOpen,
    ];
    events.extend(chunks);
    events.extend([
        SessionEvent::AssistantMessage(assembled),
        SessionEvent::ToolCall(ToolCallRecord {
            call_id: CallId(9),
            tool: "read".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        }),
        SessionEvent::ToolResult(ToolResultRecord {
            call_id: CallId(9),
            content: r#"{"ok":true,"text":"line1\nline2"}"#.into(),
        }),
        SessionEvent::StepClose,
        SessionEvent::TurnClose {
            reason: TurnEndReason::Aborted {
                cause: "user pressed esc".into(),
            },
        },
        SessionEvent::TurnOpen,
        SessionEvent::TurnClose {
            reason: TurnEndReason::Error {
                code: "provider-overloaded".into(),
                message: "".into(),
            },
        },
    ]);
    // The seed marker rides the mixed batch too: every round-trip pin below
    // therefore also pins that a backend's encoding carries it (coalescing
    // included), and that seeding derives from what survived the encoding.
    events.push(SessionEvent::SeedBoundary);
    events
}

// ---------------------------------------------------------------------------
// Backend harnesses (test-side; the lib owns no encoding or I/O).
// ---------------------------------------------------------------------------

/// File-backed backend: one JSONL line per event, appended verbatim.
struct JsonlFileBackend {
    dir: std::path::PathBuf,
}

impl JsonlFileBackend {
    fn new(dir: std::path::PathBuf) -> Self {
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn path(&self, session: &SessionId) -> std::path::PathBuf {
        self.dir.join(format!("session-{}.jsonl", session.0))
    }
}

impl SessionPersistence for JsonlFileBackend {
    fn save(
        &mut self,
        session: &SessionId,
        batch: &[SessionEvent],
    ) -> impl Future<Output = std::result::Result<(), String>> + Send {
        let path = self.path(session);
        async move {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| e.to_string())?;
            for event in batch {
                writeln!(file, "{}", json(event)).map_err(|e| e.to_string())?;
            }
            file.sync_all().map_err(|e| e.to_string())
        }
    }

    fn load(
        &mut self,
        session: &SessionId,
    ) -> impl Future<Output = std::result::Result<Option<LoadedLog>, String>> + Send {
        let path = self.path(session);
        async move {
            if !path.exists() {
                return Ok(None);
            }
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let mut events = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let event: SessionEvent =
                    serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
                events.push(event);
            }
            Ok(Some(LoadedLog { events }))
        }
    }
}

/// Chunk-coalescing backend: a run of `assistant_chunk` events for one
/// message collapses to one stored group; `load` must re-expand it to the
/// exact original events.
#[derive(Default)]
struct CoalescingBackend {
    stored: HashMap<u64, Vec<Stored>>,
}

/// One stored group: either a verbatim event or a coalesced chunk run.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Stored {
    Event(SessionEvent),
    Chunks {
        message_id: u64,
        block_index: usize,
        deltas: Vec<String>,
    },
}

impl SessionPersistence for CoalescingBackend {
    fn save(
        &mut self,
        session: &SessionId,
        batch: &[SessionEvent],
    ) -> impl Future<Output = std::result::Result<(), String>> + Send {
        let mut stored: Vec<Stored> = Vec::new();
        for event in batch {
            match event {
                SessionEvent::AssistantChunk(c) => {
                    if let Some(Stored::Chunks {
                        message_id,
                        block_index,
                        deltas,
                    }) = stored.last_mut()
                    {
                        if *message_id == c.message_id.0 && *block_index == c.block_index {
                            deltas.push(c.delta.clone());
                            continue;
                        }
                    }
                    stored.push(Stored::Chunks {
                        message_id: c.message_id.0,
                        block_index: c.block_index,
                        deltas: vec![c.delta.clone()],
                    });
                }
                other => stored.push(Stored::Event(other.clone())),
            }
        }
        let map = &mut self.stored;
        let key = session.0;
        async move {
            map.insert(key, stored);
            Ok(())
        }
    }

    fn load(
        &mut self,
        session: &SessionId,
    ) -> impl Future<Output = std::result::Result<Option<LoadedLog>, String>> + Send {
        let map = &mut self.stored;
        let key = session.0;
        async move {
            let Some(stored) = map.get(&key) else {
                return Ok(None);
            };
            let mut events = Vec::new();
            for group in stored {
                match group {
                    Stored::Event(e) => events.push(e.clone()),
                    Stored::Chunks {
                        message_id,
                        block_index,
                        deltas,
                    } => {
                        for delta in deltas {
                            events.push(SessionEvent::AssistantChunk(ChunkRecord {
                                message_id: MessageId(*message_id),
                                block_index: *block_index,
                                delta: delta.clone(),
                            }));
                        }
                    }
                }
            }
            Ok(Some(LoadedLog { events }))
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Round-trip
// ---------------------------------------------------------------------------

#[test]
fn round_trip_file_backend_is_byte_for_byte_exact() {
    let dir = tempfile::tempdir().unwrap();
    let mut backend = JsonlFileBackend::new(dir.path().to_path_buf());
    let session = SessionId(101);
    let batch = mixed_batch();
    rt().block_on(async {
        backend.save(&session, &batch).await.unwrap();
        let loaded: LoadedLog = backend.load(&session).await.unwrap().unwrap();
        assert_eq!(loaded.events.len(), batch.len());
        for (got, want) in loaded.events.iter().zip(&batch) {
            assert_eq!(got, want);
            // Byte-for-byte after the JSON round-trip.
            assert_eq!(json(got), json(want));
        }
        // Seeding is derived from what the encoding actually carried.
        assert_eq!(loaded.is_seeded(), batch.iter().any(is_seed_event));
        assert!(loaded.is_seeded(), "the batch's marker must survive");
    });
}

#[test]
fn round_trip_survives_batched_appends_and_reopen() {
    // Saving in several batches then reopening with a fresh backend handle
    // (a new process would do exactly this) must still yield the exact log.
    let dir = tempfile::tempdir().unwrap();
    let session = SessionId(102);
    let batch = mixed_batch();
    let (first, second) = batch.split_at(6);
    rt().block_on(async {
        let mut writer = JsonlFileBackend::new(dir.path().to_path_buf());
        writer.save(&session, first).await.unwrap();
        writer.save(&session, second).await.unwrap();
        // Fresh handle: nothing but the file carries the log across.
        let mut reader = JsonlFileBackend::new(dir.path().to_path_buf());
        let loaded: LoadedLog = reader.load(&session).await.unwrap().unwrap();
        assert_eq!(loaded.events, batch);
        assert!(loaded.is_seeded(), "marker survived the second batch");
    });
}

#[test]
fn round_trip_coalescing_backend_expands_exact_chunks() {
    // The backend may choose its own encoding — here runs of streamed chunks
    // collapse to one group — provided load returns the exact appended
    // events, byte-for-byte after JSON.
    let mut backend = CoalescingBackend::default();
    let session = SessionId(103);
    let batch = mixed_batch();
    rt().block_on(async {
        backend.save(&session, &batch).await.unwrap();
        // The stored form really did coalesce (fewer groups than events).
        let groups = backend.stored.get(&session.0).unwrap();
        assert!(groups.len() < batch.len());
        let loaded: LoadedLog = backend.load(&session).await.unwrap().unwrap();
        assert_eq!(loaded.events.len(), batch.len());
        for (got, want) in loaded.events.iter().zip(&batch) {
            assert_eq!(got, want);
            assert_eq!(json(got), json(want));
        }
        // The coalescer must carry the marker through its own encoding too,
        // and seeding derives from what survived it.
        assert_eq!(loaded.is_seeded(), batch.iter().any(is_seed_event));
        assert!(loaded.is_seeded(), "coalescing dropped the marker");
    });
}

#[test]
fn coalesced_chunks_concatenate_to_the_assembled_message() {
    // The chunk run's deltas must concatenate to the assembled message's
    // text — otherwise the coalescing backend could "restore" chunks that
    // never streamed.
    let batch = mixed_batch();
    let mut streamed = String::new();
    let mut assembled: Option<String> = None;
    for event in &batch {
        match event {
            SessionEvent::AssistantChunk(c) => streamed.push_str(&c.delta),
            SessionEvent::AssistantMessage(m) => {
                assembled = Some(
                    m.blocks
                        .iter()
                        .map(|b| match b {
                            ContentBlock::Text { text } => text.as_str(),
                            _ => "",
                        })
                        .collect(),
                );
            }
            _ => {}
        }
    }
    assert_eq!(Some(streamed), assembled);
}

/// The seed-boundary marker as a logable event.
///
/// The marker is a member of the closed `SessionEvent` vocabulary
/// (`SessionEvent::SeedBoundary`, pinned by
/// `seed_boundary_is_representable_in_the_event_vocabulary`), so the suite
/// uses the real thing rather than a sentinel message: it appends, saves,
/// loads, and JSON round-trips like any other event.
fn seed_event() -> SessionEvent {
    SessionEvent::SeedBoundary
}

fn is_seed_event(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::SeedBoundary)
}

// ---------------------------------------------------------------------------
// 2. Fork boundary
// ---------------------------------------------------------------------------

/// Fork metadata as recorded on the child log (fixture header shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ForkMeta {
    parent_session: String,
    seed_len: usize,
    working_dir: String,
}

/// Whether a prefix of `records` ends on a closed-turn boundary.
fn ends_on_turn_boundary(records: &[SessionEvent]) -> bool {
    let mut depth = 0usize;
    for event in records {
        match event {
            SessionEvent::TurnOpen => depth += 1,
            SessionEvent::TurnClose { .. } => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth == 0
}

/// Fork `parent` at `prefix_len` into a child session.
///
/// The prefix must end on a closed-turn boundary; the child inherits the
/// prefix, records parentage / seed length / working directory, and marks
/// the inherited region with a seed boundary so its own writes are
/// distinguishable.
fn fork(
    parent: &SessionLog,
    prefix_len: usize,
    child_id: SessionId,
    working_dir: &str,
) -> std::result::Result<(SessionLog, ForkMeta), String> {
    let records = parent.snapshot().records;
    if prefix_len > records.len() {
        return Err("fork prefix beyond log end".into());
    }
    let prefix: Vec<SessionEvent> = records[..prefix_len]
        .iter()
        .map(|r| r.event.clone())
        .collect();
    if !ends_on_turn_boundary(&prefix) {
        return Err("fork boundary inside open turn".into());
    }
    let child = SessionLog::new(child_id);
    child.append(seed_event());
    for event in prefix {
        child.append(event);
    }
    let meta = ForkMeta {
        parent_session: parent.session().to_string(),
        seed_len: prefix_len,
        working_dir: working_dir.to_string(),
    };
    Ok((child, meta))
}

#[test]
fn fork_inside_open_turn_is_refused() {
    let parent = SessionLog::new(SessionId(200));
    parent.append(SessionEvent::UserMessage(MessageRecord {
        id: MessageId(1),
        blocks: vec![ContentBlock::Text { text: "go".into() }],
        provider: None,
        model: None,
    }));
    parent.append(SessionEvent::TurnOpen);
    parent.append(SessionEvent::StepOpen);
    parent.append(SessionEvent::AssistantChunk(ChunkRecord {
        message_id: MessageId(2),
        block_index: 0,
        delta: "partial".into(),
    }));
    // Prefixes 2..=4 end inside the open turn (prefix 1 is a legal
    // boundary: the user message precedes any turn).
    for prefix in 2..=4 {
        let Err(err) = fork(&parent, prefix, SessionId(201), "/work") else {
            panic!("prefix {prefix} must be refused");
        };
        assert!(
            err.contains("open turn"),
            "prefix {prefix} must be refused, got: {err}"
        );
    }
    // The zero prefix (empty child) is legal; so is the full closed log.
    assert!(fork(&parent, 0, SessionId(202), "/work").is_ok());
    parent.append(SessionEvent::StepClose);
    parent.append(SessionEvent::TurnClose {
        reason: TurnEndReason::Completed,
    });
    assert!(fork(&parent, parent.len(), SessionId(203), "/work").is_ok());
}

#[test]
fn fork_child_records_parentage_seed_len_and_working_dir() {
    let parent = SessionLog::new(SessionId(210));
    parent.append(SessionEvent::TurnOpen);
    parent.append(SessionEvent::TurnClose {
        reason: TurnEndReason::Completed,
    });
    parent.append(SessionEvent::UserMessage(MessageRecord {
        id: MessageId(5),
        blocks: vec![ContentBlock::Text { text: "next".into() }],
        provider: None,
        model: None,
    }));
    let (child, meta) = fork(&parent, 2, SessionId(211), "/work/repo").unwrap();
    assert_eq!(
        meta,
        ForkMeta {
            parent_session: "sessionid-210".into(),
            seed_len: 2,
            working_dir: "/work/repo".into(),
        }
    );
    // The child carries the seed marker, then exactly the inherited prefix.
    let snap = child.snapshot();
    assert_eq!(snap.records.len(), 3);
    assert!(is_seed_event(&snap.records[0].event));
    assert_eq!(snap.records[1].event, SessionEvent::TurnOpen);
    assert!(matches!(
        snap.records[2].event,
        SessionEvent::TurnClose {
            reason: TurnEndReason::Completed
        }
    ));
    // The child is its own session; the parent log is untouched by the fork.
    assert_eq!(child.session(), SessionId(211));
    assert_eq!(parent.len(), 3);
    // Forking past the log end is refused too.
    assert!(fork(&parent, 99, SessionId(212), "/work").is_err());
}

// ---------------------------------------------------------------------------
// 3. Crash recovery
// ---------------------------------------------------------------------------

/// Reopen a crashed process's log: an orphaned open bracket (turn or step)
/// is closed as `interrupted`; everything before the bracket is untouched.
/// Returns the number of records recovery appended.
fn recover(log: &SessionLog) -> usize {
    let records = log.snapshot().records;
    // Walk from the end, counting unclosed brackets.
    let mut open_steps = 0isize;
    let mut open_turn = false;
    for record in records.iter().rev() {
        match &record.event {
            SessionEvent::StepOpen => {
                open_steps += 1;
            }
            SessionEvent::StepClose => {
                if open_steps > 0 {
                    open_steps -= 1;
                }
            }
            SessionEvent::TurnOpen => {
                open_turn = true;
                break;
            }
            SessionEvent::TurnClose { .. } => break, // last turn closed cleanly
            _ => {}
        }
    }
    if !open_turn && open_steps <= 0 {
        return 0;
    }
    let before = log.len();
    for _ in 0..open_steps {
        log.append(SessionEvent::StepClose);
    }
    if open_turn {
        log.append(SessionEvent::TurnClose {
            reason: TurnEndReason::Interrupted,
        });
    }
    log.len() - before
}

#[test]
fn crash_recovery_closes_orphaned_turn_as_interrupted() {
    let log = SessionLog::new(SessionId(300));
    // A clean earlier turn.
    log.append(SessionEvent::TurnOpen);
    log.append(SessionEvent::StepOpen);
    log.append(SessionEvent::StepClose);
    log.append(SessionEvent::TurnClose {
        reason: TurnEndReason::Completed,
    });
    let clean_prefix: Vec<_> = log.snapshot().records;
    // Crash mid-turn: bracket left open.
    log.append(SessionEvent::TurnOpen);
    log.append(SessionEvent::StepOpen);
    log.append(SessionEvent::AssistantChunk(ChunkRecord {
        message_id: MessageId(9),
        block_index: 0,
        delta: "half a thought".into(),
    }));
    let written = recover(&log);
    assert!(written > 0);
    let snap = log.snapshot().records;
    // Earlier records are byte-identical — recovery touched nothing before
    // the crash point.
    assert_eq!(&snap[..clean_prefix.len()], &clean_prefix[..]);
    // The orphan closed as interrupted, steps innermost-first.
    assert!(matches!(
        &snap[snap.len() - 1].event,
        SessionEvent::TurnClose {
            reason: TurnEndReason::Interrupted
        }
    ));
    assert!(matches!(
        &snap[snap.len() - 2].event,
        SessionEvent::StepClose
    ));
    // Positions stayed contiguous through recovery.
    for (i, record) in snap.iter().enumerate() {
        assert_eq!(record.position, i);
    }
    // Recovery is idempotent: a second pass adds nothing.
    assert_eq!(recover(&log), 0);
    assert_eq!(log.len(), snap.len());
}

#[test]
fn crash_recovery_leaves_clean_log_untouched() {
    let log = SessionLog::new(SessionId(301));
    log.append(SessionEvent::TurnOpen);
    log.append(SessionEvent::TurnClose {
        reason: TurnEndReason::Completed,
    });
    let before = log.snapshot();
    assert_eq!(recover(&log), 0);
    assert_eq!(log.snapshot().records, before.records);
}

#[test]
fn loop_never_emits_interrupted() {
    // Drive every driver outcome through the real loop; none may synthesize
    // an `interrupted` close, and every turn the loop opens it also closes.
    let registry = Registry::new();
    let ctx = Context::root();
    let _fiber = registry
        .mount(&ctx, std::sync::Arc::new(Spine::new(SessionId(310))))
        .unwrap();
    let log = ctx.get::<SessionLog>().unwrap();
    let loop_ = ctx.get::<AgentLoop>().unwrap();

    let msg = || MessageRecord {
        id: MessageId(1),
        blocks: vec![ContentBlock::Text { text: "ok".into() }],
        provider: Some("replay".into()),
        model: Some("test".into()),
    };
    let _ = loop_.run_turn(Box::new(move || DriverOutcome::Message(msg())));
    let _ = loop_.run_turn(Box::new(|| {
        DriverOutcome::ToolCall(ToolCallRecord {
            call_id: CallId(1),
            tool: "read".into(),
            arguments: "{}".into(),
        })
    }));
    for reason in [
        TurnEndReason::Blocked,
        TurnEndReason::MaxTokens,
        TurnEndReason::Aborted {
            cause: "ctrl-c".into(),
        },
        TurnEndReason::Error {
            code: "e".into(),
            message: "m".into(),
        },
    ] {
        let r = reason.clone();
        let _ = loop_.run_turn(Box::new(move || DriverOutcome::Stop(r)));
    }

    let snap = log.snapshot();
    for record in &snap.records {
        if let SessionEvent::TurnClose {
            reason: TurnEndReason::Interrupted,
        } = &record.event
        {
            panic!(
                "the loop emitted an interrupted TurnClose at {}",
                record.position
            );
        }
    }
    // Every TurnOpen the loop wrote was followed by a TurnClose.
    let opens = snap
        .records
        .iter()
        .filter(|r| matches!(r.event, SessionEvent::TurnOpen))
        .count();
    let closes = snap
        .records
        .iter()
        .filter(|r| matches!(r.event, SessionEvent::TurnClose { .. }))
        .count();
    assert_eq!(opens, closes);
    assert!(opens > 0);
}

// ISSUE-18 (enforced): the documented precedence "MaxTokens wins over a
// later clean stop" (events.rs doc comment on TurnEndReason) is now
// enforced by the loop. Once a turn has closed with MaxTokens, a later
// clean (`Completed`) close is displaced to MaxTokens in the log, so a
// replay of a budget-exhausted session never rewrites the budget stop as
// a clean stop. Non-clean closes (Aborted/Blocked/Error/Interrupted) are
// not displaced.
#[test]
fn max_tokens_wins_over_later_clean_stop() {
    let loop_ = AgentLoop::new(
        std::sync::Arc::new(SessionLog::new(SessionId(320))),
        EventRegistry::new(),
        Fiber::active(),
    );
    // A turn that ends MaxTokens, then a later turn closes Completed: the
    // documented rule says the MaxTokens end must win in the log — the
    // budget-exhausted turn must not be re-closed as a clean stop.
    let _ = loop_.run_turn(Box::new(|| DriverOutcome::Stop(TurnEndReason::MaxTokens)));
    let _ = loop_.run_turn(Box::new(|| DriverOutcome::Stop(TurnEndReason::Completed)));
    let snap = loop_.log().snapshot();
    // The rule's observable form: once a turn closed MaxTokens, no later
    // close may be a clean reason.
    let reasons: Vec<_> = snap
        .records
        .iter()
        .filter_map(|r| match &r.event {
            SessionEvent::TurnClose { reason } => Some(reason.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        vec![TurnEndReason::MaxTokens, TurnEndReason::MaxTokens],
        "the later clean stop must not displace MaxTokens"
    );
}

// The precedence is a property of the log, not of one loop instance: a
// second loop mounted on the same log — or a resumed process replaying a
// saved session — must not log a clean close after the MaxTokens close.
#[test]
fn max_tokens_precedence_holds_across_loop_instances() {
    let log = std::sync::Arc::new(SessionLog::new(SessionId(321)));
    let first = AgentLoop::new(log.clone(), EventRegistry::new(), Fiber::active());
    let _ = first.run_turn(Box::new(|| DriverOutcome::Stop(TurnEndReason::MaxTokens)));
    // A fresh loop over the same log (the re-mount / resume shape).
    let second = AgentLoop::new(log.clone(), EventRegistry::new(), Fiber::active());
    let turn = second.run_turn(Box::new(|| DriverOutcome::Stop(TurnEndReason::Completed)));
    assert_eq!(
        turn.reason,
        TurnEndReason::MaxTokens,
        "a new loop instance must not log a clean close after MaxTokens"
    );
    assert!(log.has_closed_max_tokens());
    let clean_after = log
        .snapshot()
        .records
        .iter()
        .skip_while(|r| {
            !matches!(
                &r.event,
                SessionEvent::TurnClose {
                    reason: TurnEndReason::MaxTokens
                }
            )
        })
        .skip(1)
        .any(|r| {
            matches!(
                &r.event,
                SessionEvent::TurnClose {
                    reason: TurnEndReason::Completed
                }
            )
        });
    assert!(!clean_after, "no TurnClose(Completed) after the MaxTokens close");
}

// The documented non-displacement half of the rule, pinned: the sticky
// MaxTokens close displaces only clean closes.
#[test]
fn max_tokens_displaces_only_clean_closes() {
    let loop_ = AgentLoop::new(
        std::sync::Arc::new(SessionLog::new(SessionId(322))),
        EventRegistry::new(),
        Fiber::active(),
    );
    let _ = loop_.run_turn(Box::new(|| DriverOutcome::Stop(TurnEndReason::MaxTokens)));
    let _ = loop_.run_turn(Box::new(|| {
        DriverOutcome::Stop(TurnEndReason::Aborted {
            cause: "user".to_string(),
        })
    }));
    let _ = loop_.run_turn(Box::new(|| DriverOutcome::Stop(TurnEndReason::Blocked)));
    let _ = loop_.run_turn(Box::new(|| {
        DriverOutcome::Stop(TurnEndReason::Error {
            code: "e".to_string(),
            message: "m".to_string(),
        })
    }));
    let _ = loop_.run_turn(Box::new(|| DriverOutcome::Stop(TurnEndReason::Completed)));
    let reasons: Vec<_> = loop_
        .log()
        .snapshot()
        .records
        .iter()
        .filter_map(|r| match &r.event {
            SessionEvent::TurnClose { reason } => Some(reason.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            TurnEndReason::MaxTokens,
            TurnEndReason::Aborted {
                cause: "user".to_string()
            },
            TurnEndReason::Blocked,
            TurnEndReason::Error {
                code: "e".to_string(),
                message: "m".to_string()
            },
            TurnEndReason::MaxTokens,
        ],
        "non-clean closes stand verbatim; only the trailing clean close is displaced"
    );
}

// ISSUE-18 (now enforced): the seed-marker contract used to be violated at
// the type level — `LoadedLog.seeded` documented a seed boundary that only
// `LogRecord::SeedBoundary` could name, and `LogRecord` is not part of the
// closed `SessionEvent` vocabulary, so no backend could persist the marker
// through `save` and no log could record it through `append`. The marker now
// lives in the core vocabulary as `SessionEvent::SeedBoundary`, and seeding
// is *derived* from the loaded events (`LoadedLog::is_seeded`) — a backend
// has no independent flag to get out of step with its own encoding.
#[test]
fn seed_boundary_is_representable_in_the_event_vocabulary() {
    // The contract's own marker type must round-trip as a session event:
    // parse it, append it, save it, load it back.
    let marker: SessionEvent = serde_json::from_str(r#"{"type":"seed_boundary"}"#)
        .expect("seed boundary must be a valid event");
    assert_eq!(marker, SessionEvent::SeedBoundary);
    // It is a structural record: it never projects a message.
    assert!(!marker.is_message_producing());
    let log = SessionLog::new(SessionId(330));
    assert!(log.append(marker.clone()));
    let dir = tempfile::tempdir().unwrap();
    let session = SessionId(330);
    rt().block_on(async {
        let mut backend = JsonlFileBackend::new(dir.path().to_path_buf());
        backend.save(&session, std::slice::from_ref(&marker)).await.unwrap();
        let loaded: LoadedLog = backend.load(&session).await.unwrap().unwrap();
        // The marker survives the encoding untouched, and seeding is
        // derived from the events the backend returns.
        assert_eq!(loaded.events, vec![marker]);
        assert!(loaded.is_seeded(), "a log with the marker must load seeded");
    });
}

// ---------------------------------------------------------------------------
// 4. No-grow-on-reopen
// ---------------------------------------------------------------------------

/// Reopen a persisted log the way a fresh process would: load only.
fn reopen(mut backend: impl SessionPersistence, session: &SessionId) -> LoadedLog {
    rt()
        .block_on(backend.load(session))
        .unwrap()
        .unwrap_or(LoadedLog {
            events: Vec::new(),
        })
}

#[test]
fn reopening_an_untouched_session_does_not_grow_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionId(400);
    let batch = mixed_batch();
    // Process 1: write and persist a clean (turn-closed) log.
    rt().block_on(async {
        let mut writer = JsonlFileBackend::new(dir.path().to_path_buf());
        writer.save(&session, &batch).await.unwrap();
    });
    // Reopen many times without touching it: identical, never grows.
    let first = reopen(JsonlFileBackend::new(dir.path().to_path_buf()), &session);
    for _ in 0..3 {
        let again = reopen(JsonlFileBackend::new(dir.path().to_path_buf()), &session);
        assert_eq!(again.events, first.events);
        assert_eq!(again.events.len(), first.events.len());
    }
    // The on-disk file is unchanged too (no recovery append, no rewrite).
    let file = dir.path().join("session-400.jsonl");
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().count(), batch.len());
    // And the reopened events are the exact appended events.
    assert_eq!(first.events, batch);
}

#[test]
fn seed_marker_distinguishes_current_writes_from_a_crash_left_open_bracket() {
    // A seeded (forked) child log: the inherited prefix sits before the seed
    // boundary; the current process's writes begin after it. A crash-left-open
    // bracket *before* the marker belongs to the inherited past and must not
    // be recovered as ours; one *after* it is ours to close.
    let child = SessionLog::new(SessionId(410));
    // Inherited (parent's) history, including a turn the parent itself
    // crashed out of — its interrupted close is history, not an orphan.
    child.append(SessionEvent::TurnOpen);
    child.append(SessionEvent::TurnClose {
        reason: TurnEndReason::Interrupted,
    });
    child.append(seed_event());
    // Our process crashes mid-turn.
    child.append(SessionEvent::TurnOpen);
    child.append(SessionEvent::StepOpen);

    // Partition at the seed marker.
    let records = child.snapshot().records;
    let seed_at = records
        .iter()
        .position(|r| is_seed_event(&r.event))
        .expect("seeded log carries its marker");
    let (inherited, ours) = records.split_at(seed_at + 1);
    // Our region holds the open bracket; the inherited region's interrupted
    // close is accounted history.
    assert!(ours
        .iter()
        .any(|r| matches!(r.event, SessionEvent::TurnOpen)));
    let inherited_interrupted = inherited
        .iter()
        .filter(|r| {
            matches!(
                &r.event,
                SessionEvent::TurnClose {
                    reason: TurnEndReason::Interrupted
                }
            )
        })
        .count();
    assert_eq!(inherited_interrupted, 1);

    // Recovery closes our bracket only: exactly StepClose + TurnClose.
    let written = recover(&child);
    assert_eq!(written, 2);
    let after = child.snapshot().records;
    // The inherited region is byte-identical — recovery rewrote nothing
    // before the seed marker.
    assert_eq!(&after[..inherited.len()], inherited);
    // A clean untouched log recovers to zero growth (pinned separately in
    // crash_recovery_leaves_clean_log_untouched).
}

// ---------------------------------------------------------------------------
// 5. Golden-file corpus
// ---------------------------------------------------------------------------

/// One line of a recorded-session fixture: an event line, or a leading
/// header line carrying fork metadata.
#[derive(Deserialize)]
struct FixtureLine {
    #[serde(default)]
    parent_session: Option<String>,
    #[serde(default)]
    seed_len: Option<usize>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    event: Option<SessionEvent>,
}

/// Load a recorded-session fixture: `(header, events)`.
fn load_fixture(name: &str) -> (Option<ForkMeta>, Vec<SessionEvent>) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path).expect("fixture exists");
    let mut header = None;
    let mut events = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let line_obj: FixtureLine = serde_json::from_str(line).expect("fixture line");
        match line_obj.event {
            Some(e) => events.push(e),
            // A header line is the one carrying a parent_session field.
            None if line_obj.parent_session.is_some() => header = Some(ForkMeta {
                parent_session: line_obj.parent_session.expect("header fields"),
                seed_len: line_obj.seed_len.expect("header fields"),
                working_dir: line_obj.working_dir.expect("header fields"),
            }),
            None => {}
        }
    }
    (header, events)
}

#[test]
fn golden_session_round_trips_through_the_file_backend() {
    // Replay the recorded session through save/load and assert exactness.
    let (_, events) = load_fixture("session_basic.jsonl");
    assert!(!events.is_empty());
    let dir = tempfile::tempdir().unwrap();
    let session = SessionId(500);
    rt().block_on(async {
        let mut backend = JsonlFileBackend::new(dir.path().to_path_buf());
        backend.save(&session, &events).await.unwrap();
        let loaded: LoadedLog = backend.load(&session).await.unwrap().unwrap();
        assert_eq!(loaded.events, events);
        for (got, want) in loaded.events.iter().zip(&events) {
            assert_eq!(json(got), json(want));
        }
    });
}

#[test]
fn golden_session_fixture_replays_to_the_expected_log() {
    // The fixture is a *recorded* session: replay it into a SessionLog and
    // assert the shape the log guarantees (contiguous positions, one clean
    // turn, chunks concatenate to the assembled message).
    let (header, events) = load_fixture("session_basic.jsonl");
    assert!(header.is_none(), "basic fixture is not a fork");
    let log = SessionLog::new(SessionId(501));
    for event in events.clone() {
        assert!(log.append(event));
    }
    let snap = log.snapshot();
    assert_eq!(snap.records.len(), events.len());
    for (i, record) in snap.records.iter().enumerate() {
        assert_eq!(record.position, i);
    }
    // Exactly one turn, cleanly closed.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::TurnOpen))
            .count(),
        1
    );
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::TurnClose {
            reason: TurnEndReason::Completed
        }
    ));
    // Streamed chunks concatenate to the assembled assistant text.
    let streamed: String = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::AssistantChunk(c) => Some(c.delta.as_str()),
            _ => None,
        })
        .collect();
    let assembled = events.iter().find_map(|e| match e {
        SessionEvent::AssistantMessage(m) => Some(
            m.blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.as_str(),
                    _ => "",
                })
                .collect::<String>(),
        ),
        _ => None,
    });
    assert_eq!(streamed, assembled.unwrap());
}

#[test]
fn golden_forked_fixture_matches_the_fork_contract() {
    // The forked fixture's header must agree with the fork rules: the seed
    // boundary sits at the head, and a real fork of a parent replayed from
    // the basic fixture yields exactly the fixture's child body and header.
    let (header, events) = load_fixture("session_forked.jsonl");
    let meta = header.expect("forked fixture carries a header");
    assert_eq!(meta.parent_session, "sessionid-7");
    assert_eq!(meta.working_dir, "/work/repo");
    // Rebuild the recorded parent from the basic fixture (as session 7).
    let (_, parent_events) = load_fixture("session_basic.jsonl");
    let parent = SessionLog::new(SessionId(7));
    for event in parent_events {
        parent.append(event);
    }
    // Forking the full parent log yields exactly the fixture's child body,
    // with the header's metadata.
    let (child, child_meta) =
        fork(&parent, parent.len(), SessionId(8), &meta.working_dir).unwrap();
    assert_eq!(child_meta, meta);
    let body: Vec<SessionEvent> = child
        .snapshot()
        .records
        .iter()
        .map(|r| r.event.clone())
        .collect();
    // The fixture's child body: seed marker + inherited prefix.
    assert!(is_seed_event(&body[0]));
    assert_eq!(&body[1..], &events[1..]);
    // seed_len in the header counts the inherited events.
    assert_eq!(meta.seed_len, body.len() - 1);
}
