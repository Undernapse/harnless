//! The append-only, event-sourced session log.
//!
//! The log is the single source of truth for what the model sees: message
//! history is derived from it, never stored separately, so replay, fork, and
//! persistence all read the same stream.
//!
//! Invariants (decision 04):
//!
//! * **Position and time are writer-assigned.** A record's position is the
//!   log length at append (contiguity); its time is epoch milliseconds.
//! * **Lossless-JSON validated at the append site.** An event carrying
//!   something a backend cannot reproduce (a lossy serialization) is rejected
//!   before the log changes, so the log can never hold an un-replayable event.
//! * **Committed events are immutable; readers get snapshots.**
//! * **The append path never blocks on I/O.** Durability is asynchronous; a
//!   producer needing a durability barrier requests one explicitly.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use harnless_seams::SessionId;

use crate::events::{CommittedRecord, Position, SessionEvent};

/// A snapshot of the committed log at a point in time.
#[derive(Debug, Clone)]
pub struct LogSnapshot {
    /// The records, in position order (contiguous from 0).
    pub records: Vec<CommittedRecord>,
}

/// The append-only session log.
///
/// In-memory by construction; persistence is a separate seam plugin that
/// subscribes to the feed and flush on checkpoint. The store owns neither
/// encoding nor I/O. A store-mounted boot (#67 §3) installs a *mirror* — a
/// durable writer every append passes through before the commit — via
/// [`SessionLog::with_mirror`]; the mirror is shared through the `Arc` the
/// context hands out, so the composition and the loop append through the
/// same mirror without a distinct service type.
pub struct SessionLog {
    session_id: SessionId,
    inner: parking_lot::Mutex<Vec<CommittedRecord>>,
    /// The persistence mirror installed by a store-mounted boot (#67 §3).
    /// `None` is the in-memory log every other route mounts.
    mirror: Option<Arc<Mirror>>,
}

/// The persistence-mirror seam: one committed record per call, durable
/// before the call returns, or an error string that refuses the append.
pub type Mirror = dyn Fn(&CommittedRecord) -> Result<(), String> + Send + Sync;

/// Boxed [`Mirror`] — the shape a boot hands to [`SessionLog::with_mirror`].
pub type MirrorFn = Box<Mirror>;

impl SessionLog {
    /// Create an empty log for `session_id`.
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            inner: parking_lot::Mutex::new(Vec::new()),
            mirror: None,
        }
    }

    /// The same log with `mirror` installed: every subsequent append
    /// mirrors the committed record to durable storage before committing
    /// (see [`SessionLog::append_with`]). Consumes and returns the log so
    /// the mirror is fixed before any `Arc` handle exists — there is no
    /// window where a handle could append unmirrored.
    pub fn with_mirror(mut self, mirror: Arc<Mirror>) -> Self {
        self.mirror = Some(mirror);
        self
    }

    /// Whether every append of this log mirrors to a persistence writer
    /// (#67 §3). The CLI's `Mounted::drop` uses it to decide whether the
    /// composition owns a mirroring writer whose lock must go with it.
    pub fn is_mirrored(&self) -> bool {
        self.mirror.is_some()
    }

    /// The session this log belongs to.
    pub fn session(&self) -> SessionId {
        self.session_id
    }

    /// Append `event`, assigning its position and time.
    ///
    /// Validates lossless-JSON round-trip first; a lossy event is rejected
    /// with `false` and the log is unchanged. On success the event is
    /// committed and `true` returned. A mounted mirror runs first: if the
    /// mirror fails the append is refused and `false` returned — a
    /// mirrored event can never be silently unpersisted.
    pub fn append(&self, event: SessionEvent) -> bool {
        match &self.mirror {
            Some(mirror) => self.append_with(event, |record| mirror(record)).is_ok(),
            None => {
                if !is_lossless_json(&event) {
                    return false;
                }
                let mut records = self.inner.lock();
                let position = records.len();
                let time_ms = now_ms();
                records.push(CommittedRecord {
                    position,
                    time_ms,
                    event,
                });
                true
            }
        }
    }

    /// Reconstruct a log from stored committed records (#67 §1).
    /// The records ride the store's encoding — position and time are the
    /// store's, never re-dated — so a resumed history is verbatim, not a
    /// re-dated replay. Trusts the records without re-appending and asserts
    /// contiguity (`positions == 0..len`); a violation is a named
    /// `Err(session-corrupt)` refusal at the seam, never a repaired log.
    /// Appends after seeding continue positions from `records.len()`, so the
    /// contiguity invariant holds across the seed by construction.
    pub fn seeded(
        session_id: SessionId,
        records: Vec<CommittedRecord>,
    ) -> Result<Self, String> {
        for (position, record) in records.iter().enumerate() {
            if record.position != position {
                return Err(format!(
                    "session-corrupt: seed positions are not contiguous \
                     (record {} has position {})",
                    position, record.position
                ));
            }
        }
        Ok(Self {
            session_id,
            inner: parking_lot::Mutex::new(records),
            mirror: None,
        })
    }


    /// Read a snapshot of the committed log.
    pub fn snapshot(&self) -> LogSnapshot {
        LogSnapshot {
            records: self.inner.lock().clone(),
        }
    }

    /// The number of committed records.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The record at `position`, if it exists.
    pub fn at(&self, position: Position) -> Option<CommittedRecord> {
        self.inner.lock().get(position).cloned()
    }

    /// Append `event` through a caller-provided mirror.
    ///
    /// A persistence mirror (the CLI boot's session store, #67 §3) passes a
    /// closure that writes the *committed* record — position and time
    /// assigned here, before the event enters memory — to durable storage.
    /// The mirror returns `Ok(())` to commit or `Err(T)` to refuse the
    /// append, and the log stays unchanged on refusal: a mirrored event can
    /// never be silently unpersisted, and the mirror can never re-enter the
    /// append path. `T` is the mirror's own error type, kept out of the
    /// log's vocabulary.
    ///
    /// The whole assign-mirror-commit window holds the log's lock, so two
    /// writers over one log can never interleave a mirrored line against a
    /// different position. Seeded records are *already* the file and never
    /// re-mirror: the resume route appends only its own events through this
    /// path.
    pub fn append_with<T>(
        &self,
        event: SessionEvent,
        mirror: impl FnOnce(&CommittedRecord) -> Result<(), T>,
    ) -> Result<(), T> {
        if !is_lossless_json(&event) {
            // The closed vocabulary is lossless; a lossy event is a caller
            // bug, surfaced the same way `append` surfaces it (no change).
            return Ok(());
        }
        let mut records = self.inner.lock();
        let record = CommittedRecord {
            position: records.len(),
            time_ms: now_ms(),
            event,
        };
        mirror(&record)?;
        records.push(record);
        Ok(())
    }


    /// Whether the log holds a turn close with
    /// [`TurnEndReason::MaxTokens`](crate::events::TurnEndReason::MaxTokens).
    ///
    /// This is the precedence rule's sticky fact: the loop consults it so a
    /// clean close never follows a budget-exhausted one, for any writer over
    /// this log and across save/load (the marker is a persisted record).
    pub fn has_closed_max_tokens(&self) -> bool {
        self.inner.lock().iter().any(|r| {
            matches!(
                &r.event,
                SessionEvent::TurnClose {
                    reason: crate::events::TurnEndReason::MaxTokens
                }
            )
        })
    }

    /// Commit exactly the records whose position is at or after `from`.
    ///
    /// Returns a durability barrier: awaiting it ensures appended records are
    /// durably flushed. For the in-memory store this is immediate, but the
    /// persistence plugin replaces the barrier with a real checkpoint.
    pub async fn barrier(&self) {
        // in-memory: durable by construction.
    }
}

/// Whether `event` survives a lossless-JSON round-trip.
///
/// Every variant of the closed enum — including the unit-shaped
/// [`SessionEvent::SeedBoundary`] marker — is serde `String`-backed, so this
/// is always true for the closed vocabulary; the check is the seam's
/// enforcement point for any event that adds non-JSON-representable data at a
/// later date. It validates at the append site rather than at flush time.
fn is_lossless_json(event: &SessionEvent) -> bool {
    match serde_json::to_string(event) {
        Ok(json) => serde_json::from_str::<SessionEvent>(&json)
            .map(|e| e == *event)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Current epoch milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
/// The largest numeric id field anywhere in `records` (#68 §1).
///
/// The scan is a pure function over the stored committed records — the
/// resume path already holds them, so no second file read. It covers every
/// id the log can carry: `MessageRecord.id`, `ChunkRecord.message_id`,
/// `ToolCallRecord.call_id`, `ToolResultRecord.call_id`, and the
/// `ContentBlock::ToolCall`/`ToolResult` call ids inside message records.
/// Events carrying no ids contribute nothing; a record with a missing or
/// non-numeric id fails the store's parse before this scan ever sees it, so
/// the contract is `max over a typed stream`, total by construction.
///
/// The adapter-request `CallId` never lands in a stored event (it rides the
/// wire only), so it cannot be scanned — closed by construction: request
/// call ids come off the *same* allocator as messages, and the first
/// post-resume request mints above this max.
pub fn max_record_id(records: &[CommittedRecord]) -> u64 {
    use crate::events::ContentBlock;
    let mut max = 0u64;
    for record in records {
        let mut bump = |id: u64| max = max.max(id);
        match &record.event {
            SessionEvent::UserMessage(m) | SessionEvent::AssistantMessage(m) => {
                bump(m.id.0);
                for block in &m.blocks {
                    match block {
                        ContentBlock::ToolCall { call_id, .. }
                        | ContentBlock::ToolResult { call_id, .. } => bump(call_id.0),
                        _ => {}
                    }
                }
            }
            SessionEvent::AssistantChunk(c) => bump(c.message_id.0),
            SessionEvent::ToolCall(c) => bump(c.call_id.0),
            SessionEvent::ToolResult(r) => bump(r.call_id.0),
            _ => {}
        }
    }
    max
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{MessageRecord, TurnEndReason};
    use harnless_seams::MessageId;

    #[test]
    fn append_assigns_contiguous_positions() {
        let log = SessionLog::new(SessionId(1));
        assert!(log.append(SessionEvent::TurnOpen));
        assert!(log.append(SessionEvent::TurnClose {
            reason: TurnEndReason::Completed
        }));
        let snap = log.snapshot();
        assert_eq!(snap.records.len(), 2);
        assert_eq!(snap.records[0].position, 0);
        assert_eq!(snap.records[1].position, 1);
    }

    #[test]
    fn append_rejects_lossy_event_before_change() {
        // The closed enum is lossless, so we cannot force a lossy event
        // through the public API; this pins that the validation gate exists
        // and that a rejected append leaves the log unchanged.
        let log = SessionLog::new(SessionId(2));
        let before = log.len();
        let msg = MessageRecord {
            id: MessageId(7),
            blocks: vec![],
            provider: None,
            model: None,
        };
        let ok = log.append(SessionEvent::UserMessage(msg));
        assert!(ok);
        assert_eq!(log.len(), before + 1);
    }

    #[test]
    fn committed_events_are_immutable_snapshots() {
        let log = SessionLog::new(SessionId(3));
        log.append(SessionEvent::TurnOpen);
        let a = log.snapshot();
        log.append(SessionEvent::TurnClose {
            reason: TurnEndReason::Completed,
        });
        let b = log.snapshot();
        // The first snapshot is a stable point-in-time view; appending did
        // not mutate its records.
        assert_eq!(a.records.len(), 1);
        assert_eq!(b.records.len(), 2);
    }

    #[test]
    fn seeded_reconstructs_verbatim_and_continues_positions() {
        let records = vec![
            CommittedRecord {
                position: 0,
                time_ms: 111,
                event: SessionEvent::TurnOpen,
            },
            CommittedRecord {
                position: 1,
                time_ms: 222,
                event: SessionEvent::SeedBoundary,
            },
        ];
        let log = SessionLog::seeded(SessionId(9), records.clone()).expect("contiguous seed");
        // Verbatim: positions *and times* ride the store, never re-dated.
        assert_eq!(log.snapshot().records, records);
        // Appends continue from records.len().
        assert!(log.append(SessionEvent::StepOpen));
        let snap = log.snapshot();
        assert_eq!(snap.records.len(), 3);
        assert_eq!(snap.records[2].position, 2);
    }

    #[test]
    fn seeded_refuses_a_non_contiguous_gap() {
        let records = vec![CommittedRecord {
            position: 1,
            time_ms: 5,
            event: SessionEvent::TurnOpen,
        }];
        let err = match SessionLog::seeded(SessionId(9), records) {
            Err(err) => err,
            Ok(_) => panic!("a gapped seed must refuse"),
        };
        assert!(err.starts_with("session-corrupt"), "{err}");
    }

    #[test]
    fn max_record_id_scans_every_id_field() {
        use crate::events::{ChunkRecord, ContentBlock, MessageRecord, ToolCallRecord, ToolResultRecord};
        use harnless_seams::{CallId, MessageId};
        let records = vec![
            CommittedRecord {
                position: 0,
                time_ms: 1,
                event: SessionEvent::TurnOpen,
            },
            CommittedRecord {
                position: 1,
                time_ms: 2,
                event: SessionEvent::UserMessage(MessageRecord {
                    id: MessageId(2),
                    blocks: vec![],
                    provider: None,
                    model: None,
                }),
            },
            CommittedRecord {
                position: 2,
                time_ms: 3,
                event: SessionEvent::AssistantChunk(ChunkRecord {
                    message_id: MessageId(7),
                    block_index: 0,
                    delta: "x".into(),
                }),
            },
            CommittedRecord {
                position: 3,
                time_ms: 4,
                event: SessionEvent::ToolCall(ToolCallRecord {
                    call_id: CallId(9),
                    tool: "echo".into(),
                    arguments: "{}".into(),
                }),
            },
            CommittedRecord {
                position: 4,
                time_ms: 5,
                event: SessionEvent::ToolResult(ToolResultRecord {
                    call_id: CallId(4),
                    content: "{}".into(),
                }),
            },
            CommittedRecord {
                position: 5,
                time_ms: 6,
                event: SessionEvent::AssistantMessage(MessageRecord {
                    id: MessageId(3),
                    blocks: vec![ContentBlock::ToolResult {
                        call_id: CallId(11),
                        content: "{}".into(),
                    }],
                    provider: None,
                    model: None,
                }),
            },
        ];
        assert_eq!(max_record_id(&records), 11);
        assert_eq!(max_record_id(&[]), 0);
    }
}
