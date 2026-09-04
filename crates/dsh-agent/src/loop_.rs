//! The turn/step agent loop.
//!
//! The loop drives the logged conversation and exposes its extension points
//! as waterfalls (decision 04): model streaming is a waterfall whose
//! listeners may transform the assistant message, and the turn-stopping
//! checkpoint is serial with no continuation. A waterfall listener must
//! delegate (call `next`) to reach later ones; returning without calling it
//! vetoes.
//!
//! Because "model-visible means logged," the loop is the single highest seam:
//! drive a turn and assert the emitted event sequence and the derived message
//! list. The log *is* the observable behavior.
//!
//! The loop has no provider dependencies — a caller supplies a [`Driver`]
//! (an in-memory replay adapter for tests, a real model call in production).

use std::sync::Arc;

use dsh_runtime::events::{EventOptions, EventRegistry, Next};
use dsh_runtime::fiber::Fiber;
use dsh_runtime::{Disposer, Result as RtResult};

use crate::events::{
    MessageRecord, SessionEvent, ToolCallRecord, ToolResultRecord,
    TurnEndReason,
};
use crate::history::History;
use crate::session::SessionLog;

/// The model-streaming waterfall payload/result: the assistant message.
type StreamMessage = MessageRecord;

/// A driver produces an assistant response for one step.
///
/// This is the pluggable model boundary; the loop drives it and folds its
/// output into the log. A replay adapter returns a scripted stream.
pub type Driver = Box<dyn FnOnce() -> DriverOutcome + Send>;

/// The outcome of one driver step.
pub enum DriverOutcome {
    /// The model produced a final assistant message.
    Message(MessageRecord),
    /// The model requested a tool call; the loop records it and continues.
    ToolCall(ToolCallRecord),
    /// The turn stopped, with the reason.
    Stop(TurnEndReason),
}

/// The turn/step agent loop.
///
/// Owns a [`SessionLog`] and an [`EventRegistry`] for its waterfall extension
/// points. Driving a turn appends a full event sequence and returns the
/// derived history the model saw.
pub struct AgentLoop {
    log: Arc<SessionLog>,
    events: EventRegistry,
    fiber: Arc<Fiber>,
}

impl AgentLoop {
    /// Create a loop bound to `log` and `fiber` (which owns its effects).
    pub fn new(log: Arc<SessionLog>, events: EventRegistry, fiber: Arc<Fiber>) -> Self {
        Self {
            log,
            events,
            fiber,
        }
    }

    /// The session log this loop drives.
    pub fn log(&self) -> &SessionLog {
        &self.log
    }

    /// The event registry hosting the loop's waterfall extension points.
    pub fn events(&self) -> &EventRegistry {
        &self.events
    }

    /// The fiber owning the loop's registered effects.
    pub fn fiber(&self) -> &Arc<Fiber> {
        &self.fiber
    }

    /// Register a model-streaming waterfall listener.
    ///
    /// The listener may transform the assistant message in place before
    /// committing, and must delegate via `next` to reach the built-in
    /// behavior. Not delegating vetoes the commit. Owned by the loop's fiber.
    pub fn on_stream<F>(&self, listener: F) -> RtResult<Disposer>
    where
        F: FnMut(&mut StreamMessage, &mut Next<'_, StreamMessage, StreamMessage>) -> StreamMessage
            + Send
            + 'static,
    {
        self.events
            .on_waterfall(&self.fiber, listener, EventOptions::new())
    }

    /// Drive one turn with `driver`, appending the event sequence to the log.
    ///
    /// Opens the turn, runs steps until the driver stops or finishes, then
    /// closes the turn with the resulting [`TurnEndReason`].
    pub fn run_turn(&self, driver: Driver) -> DerivedTurn {
        self.log.append(SessionEvent::TurnOpen);
        self.log.append(SessionEvent::StepOpen);

        let mut history = History::default();
        let final_reason = match driver() {
            DriverOutcome::Message(mut msg) => {
                // Model-streaming waterfall: listeners may transform `msg`.
                let committed = self.fire_stream(&mut msg);
                self.log.append(SessionEvent::AssistantMessage(committed.clone()));
                let _ = history.apply(&SessionEvent::AssistantMessage(committed));
                TurnEndReason::Completed
            }
            DriverOutcome::ToolCall(call) => {
                self.log.append(SessionEvent::ToolCall(call.clone()));
                let _ = history.apply(&SessionEvent::ToolCall(call.clone()));
                let result = ToolResultRecord {
                    call_id: call.call_id,
                    content: "{}".into(),
                };
                self.log.append(SessionEvent::ToolResult(result.clone()));
                let _ = history.apply(&SessionEvent::ToolResult(result));
                TurnEndReason::Completed
            }
            DriverOutcome::Stop(reason) => reason,
        };

        self.log.append(SessionEvent::StepClose);
        self.log.append(SessionEvent::TurnClose {
            reason: final_reason.clone(),
        });

        DerivedTurn {
            history,
            reason: final_reason,
        }
    }

    /// Fire the model-streaming waterfall over `msg`.
    ///
    /// The built-in behavior commits the message unchanged; a listener that
    /// delegates sees the message and may return a transformed copy, which the
    /// loop then commits.
    fn fire_stream(&self, msg: &mut StreamMessage) -> StreamMessage {
        self.events
            .waterfall(msg.clone(), |m: StreamMessage| m)
    }
}

/// The result of driving a turn.
#[derive(Debug, Clone)]
pub struct DerivedTurn {
    /// The derived message history the model saw this turn.
    pub history: History,
    /// The reason the turn ended.
    pub reason: TurnEndReason,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ContentBlock, MessageRecord, SessionEvent, TurnEndReason};
    use dsh_runtime::events::EventRegistry;
    use dsh_runtime::fiber::Fiber;
    use dsh_seams::MessageId;

    fn message(text: &str) -> MessageRecord {
        MessageRecord {
            id: MessageId(1),
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            provider: Some("replay".into()),
            model: Some("test".into()),
        }
    }

    #[test]
    fn run_turn_appends_full_sequence_and_derives_history() {
        let fiber = Fiber::active();
        let log = Arc::new(SessionLog::new(dsh_seams::SessionId(1)));
        let events = EventRegistry::new();
        let loop_ = AgentLoop::new(log, events, fiber);
        let text = "hello".to_string();
        let turn = loop_.run_turn(Box::new(move || {
            DriverOutcome::Message(message(&text))
        }));
        let snap = loop_.log().snapshot();
        // TurnOpen, StepOpen, AssistantMessage, StepClose, TurnClose.
        assert_eq!(snap.records.len(), 5);
        assert_eq!(snap.records[0].event, SessionEvent::TurnOpen);
        assert!(matches!(
            snap.records[2].event,
            SessionEvent::AssistantMessage(_)
        ));
        assert!(matches!(
            snap.records[4].event,
            SessionEvent::TurnClose { .. }
        ));
        assert_eq!(turn.reason, TurnEndReason::Completed);
        assert_eq!(turn.history.len(), 1);
    }

    #[test]
    fn on_stream_listener_can_transform_the_committed_message() {
        let fiber = Fiber::active();
        let log = Arc::new(SessionLog::new(dsh_seams::SessionId(2)));
        let events = EventRegistry::new();
        let loop_ = AgentLoop::new(log, events, fiber);
        let _guard = loop_
            .on_stream(|msg: &mut StreamMessage, next: &mut Next<'_, StreamMessage, StreamMessage>| {
                // Transform the first text block before delegating.
                if let Some(ContentBlock::Text { text }) = msg.blocks.first_mut() {
                    text.push_str("!!");
                }
                next.call(msg.clone())
            })
            .unwrap();
        let text = "hi".to_string();
        let turn = loop_.run_turn(Box::new(move || DriverOutcome::Message(message(&text))));
        let snap = loop_.log().snapshot();
        if let SessionEvent::AssistantMessage(m) = &snap.records[2].event {
            if let ContentBlock::Text { text } = &m.blocks[0] {
                assert_eq!(text, "hi!!");
            } else {
                panic!("expected text block");
            }
        } else {
            panic!("expected assistant message");
        }
        assert_eq!(turn.history.len(), 1);
    }

    #[test]
    fn stop_reason_propagates_to_turn_close() {
        let fiber = Fiber::active();
        let log = Arc::new(SessionLog::new(dsh_seams::SessionId(3)));
        let events = EventRegistry::new();
        let loop_ = AgentLoop::new(log, events, fiber);
        let turn = loop_.run_turn(Box::new(|| {
            DriverOutcome::Stop(TurnEndReason::MaxTokens)
        }));
        assert_eq!(turn.reason, TurnEndReason::MaxTokens);
        let snap = loop_.log().snapshot();
        // TurnOpen, StepOpen, StepClose, TurnClose (no assistant message
        // when the driver stops immediately).
        assert_eq!(snap.records.len(), 4);
        if let SessionEvent::TurnClose { reason } = &snap.records[3].event {
            assert_eq!(*reason, TurnEndReason::MaxTokens);
        } else {
            panic!("expected turn close");
        }
    }
}
