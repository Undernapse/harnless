//! Session persistence.
//!
//! Durability is a separate seam (decision 04): the store is in-memory, and
//! persistence plugins subscribe to the event feed and flush on checkpoint.
//! The store owns neither encoding nor I/O. A backend may choose its own
//! encoding for a batch **provided loading returns the exact appended
//! events** — the invariant under test is `load(save(events)) == events`,
//! byte-for-byte after a JSON round-trip, for any backend including the
//! chunk-coalescing one.

use std::future::Future;

use harnless_seams::SessionId;

use crate::events::SessionEvent;

/// A loaded session log: the exact appended events, from which the seed
/// boundary is derived, so reopening an untouched session does not grow its
/// log.
///
/// The seed boundary is **derivable from `events`**, never an independent
/// fact: a log is seeded iff its events contain
/// [`SessionEvent::SeedBoundary`]. The marker is a member of the core
/// vocabulary precisely so a backend can persist it through `save` and
/// recover it through `load` — there is no separate flag a backend could
/// set out of step with what its encoding actually carries.
#[derive(Debug, Clone)]
pub struct LoadedLog {
    /// The exact events, in position order.
    pub events: Vec<SessionEvent>,
}

impl LoadedLog {
    /// Whether this log carries a seed boundary (writes from this process):
    /// `true` iff `events` contains [`SessionEvent::SeedBoundary`].
    pub fn is_seeded(&self) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e, SessionEvent::SeedBoundary))
    }
}

/// The session-persistence seam.
///
/// A backend's `save` may coalesce chunks, but `load` must return the exact
/// appended events the store expected.
pub trait SessionPersistence: Send + Sync + 'static {
    /// Persist `batch` for `session` (a bounded supplier), awaiting durability.
    fn save(
        &mut self,
        session: &SessionId,
        batch: &[SessionEvent],
    ) -> impl Future<Output = std::result::Result<(), String>> + Send;

    /// Load the persisted log for `session`, if any.
    fn load(
        &mut self,
        session: &SessionId,
    ) -> impl Future<Output = std::result::Result<Option<LoadedLog>, String>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{MessageRecord, TurnEndReason};
    use harnless_seams::MessageId;

    /// In-memory backend recording the exact batch it was given.
    #[derive(Default)]
    struct MemoryBackend {
        stored: std::collections::HashMap<u64, Vec<SessionEvent>>,
    }

    impl SessionPersistence for MemoryBackend {
        async fn save(
            &mut self,
            session: &SessionId,
            batch: &[SessionEvent],
        ) -> std::result::Result<(), String> {
            self.stored.insert(session.0, batch.to_vec());
            Ok(())
        }

        async fn load(
            &mut self,
            session: &SessionId,
        ) -> std::result::Result<Option<LoadedLog>, String> {
            Ok(self
                .stored
                .get(&session.0)
                .map(|e| LoadedLog { events: e.clone() }))
        }
    }

    #[test]
    fn load_save_round_trips_exactly() {
        let mut backend = MemoryBackend::default();
        let session = SessionId(5);
        let batch = vec![
            SessionEvent::TurnOpen,
            SessionEvent::TurnClose {
                reason: TurnEndReason::Completed,
            },
            SessionEvent::UserMessage(MessageRecord {
                id: MessageId(1),
                blocks: vec![],
                provider: None,
                model: None,
            }),
        ];
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            backend.save(&session, &batch).await.unwrap();
            let loaded = backend.load(&session).await.unwrap().unwrap();
            // load(save(events)) == events, byte-for-byte.
            assert_eq!(loaded.events, batch);
        });
    }
}
