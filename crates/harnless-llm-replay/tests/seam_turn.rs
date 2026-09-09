//! Integration: the replay adapter driving the agent loop.
//!
//! The shape of the primary seam test (#20): record a model stream, replay
//! it through the `ReplayAdapter` seam, fold the replayed frames exactly as
//! a driver folds a live stream, and drive a turn — asserting the emitted
//! event sequence and the derived message list. No network anywhere.

use futures::stream::Stream as _;
use futures::StreamExt;
use harnless_agent::events::{ContentBlock, MessageRecord, SessionEvent, TurnEndReason};
use harnless_agent::loop_::{AgentLoop, DriverOutcome};
use harnless_llm_replay::{RecordedFrame, Recording, ReplayAdapter, Script};
use harnless_runtime::events::EventRegistry;
use harnless_runtime::fiber::Fiber;
use harnless_seams::{
    BlockAssembler, BlockKind, CallId, Message, MessageId, ModelAdapter, ReplayState, SessionId,
    StreamEvent, StreamFrame, Usage,
};
use serde_json::json;
use std::sync::Arc;

/// A recorded stream: reasoning, a text answer, and a tool call — the
/// shapes a real provider interleaves.
fn recorded_stream() -> Vec<StreamFrame> {
    vec![
        StreamFrame::BlockStart {
            index: 0,
            kind: BlockKind::Reasoning,
        },
        StreamFrame::ReasoningDelta {
            index: 0,
            text: "thin".into(),
        },
        StreamFrame::ReasoningDelta {
            index: 0,
            text: "king".into(),
        },
        StreamFrame::BlockStart {
            index: 1,
            kind: BlockKind::Text,
        },
        StreamFrame::TextDelta {
            index: 1,
            text: "Hel".into(),
        },
        StreamFrame::TextDelta {
            index: 1,
            text: "lo".into(),
        },
        StreamFrame::BlockStart {
            index: 2,
            kind: BlockKind::ToolCall,
        },
        StreamFrame::ToolCallDelta {
            index: 2,
            call_id: CallId(8),
            json: r#"{"id":8,"name":"echo","arguments":"{\"x\":"#.into(),
        },
        StreamFrame::ToolCallDelta {
            index: 2,
            call_id: CallId(8),
            json: r#"1}"#.into(),
        },
        StreamFrame::BlockEnd {
            index: 0,
            assembled: harnless_seams::ContentBlock {
                kind: BlockKind::Reasoning,
                text: "thinking".into(),
            },
        },
        StreamFrame::BlockEnd {
            index: 1,
            assembled: harnless_seams::ContentBlock {
                kind: BlockKind::Text,
                text: "Hello".into(),
            },
        },
        StreamFrame::BlockEnd {
            index: 2,
            assembled: harnless_seams::ContentBlock {
                kind: BlockKind::ToolCall,
                text: r#"{"id":8,"name":"echo","arguments":"{\"x\":1}"}"#.into(),
            },
        },
        StreamFrame::Usage(Usage {
            uncached_input: 10,
            cached_reads: 4,
            cached_writes: 0,
            output: 6,
            reasoning: 2,
        }),
        StreamFrame::Finish,
    ]
}

fn recorded_replay() -> ReplayState {
    ReplayState {
        response: Some(json!({"id": "resp-1", "__harnless_provider": "openai"})),
        blocks: vec![json!({"i": 0}), json!({"i": 1}), json!({"i": 2})],
    }
}

/// Replay a golden recording through the seam and fold it with the shared
/// assembler, exactly as a live-stream driver would.
async fn replay_and_fold(
    recording: &Recording,
) -> (Vec<StreamFrame>, Vec<harnless_seams::ContentBlock>) {
    let adapter = ReplayAdapter::new("openai", Script::one(recording.clone()));
    let mut stream = adapter
        .stream(CallId(1), &[] as &[Message], &[], None)
        .expect("valid corpus");
    let mut frames = Vec::new();
    let mut assembler = BlockAssembler::new();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Frame(frame) => {
                assembler.push(&frame);
                frames.push(frame);
            }
            StreamEvent::Failed(f) => panic!("replay failed in-band: {f:?}"),
        }
    }
    (frames, assembler.blocks())
}

#[tokio::test]
async fn golden_file_replays_frame_for_frame_with_no_network() {
    let recording = Recording::capture(&recorded_stream(), &recorded_replay());
    // Serialize to the golden format, reparse, replay from the reparsed doc.
    let json = recording.to_json().unwrap();
    let from_disk = Recording::from_json(&json).unwrap();

    let (frames, blocks) = replay_and_fold(&from_disk).await;
    assert_eq!(frames, recorded_stream(), "replay is verbatim");
    // Assembled blocks come back in index order, interleaving preserved.
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0].text, "thinking");
    assert_eq!(blocks[1].text, "Hello");
    // Tool arguments stay raw JSON end to end — never re-serialized.
    assert_eq!(
        blocks[2].text,
        r#"{"id":8,"name":"echo","arguments":"{\"x\":1}"}"#
    );
}

#[test]
fn replayed_stream_drives_a_turn_through_the_loop() {
    let recording = Recording::capture(&recorded_stream(), &recorded_replay());
    let adapter = ReplayAdapter::new("openai", Script::one(recording));

    let fiber = Fiber::active();
    let log = Arc::new(harnless_agent::session::SessionLog::new(SessionId(1)));
    let events = EventRegistry::new();
    let loop_ = AgentLoop::new(log, events, fiber);

    // The driver is the seam boundary: it replays via the adapter and folds
    // the stream into the assistant message the loop commits.
    let turn = loop_.run_turn(Box::new(move || {
        let mut stream = adapter
            .stream(CallId(1), &[] as &[Message], &[], None)
            .expect("valid corpus");
        // The stream never blocks (replay, no I/O), so polling it to
        // completion inside the sync driver needs no nested runtime.
        let mut assembler = BlockAssembler::new();
        let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
        loop {
            match std::pin::Pin::new(&mut stream).poll_next(&mut cx) {
                std::task::Poll::Ready(Some(StreamEvent::Frame(frame))) => assembler.push(&frame),
                std::task::Poll::Ready(Some(StreamEvent::Failed(f))) => {
                    panic!("replay failed: {f:?}")
                }
                std::task::Poll::Ready(None) => break,
                std::task::Poll::Pending => panic!("replay stream blocked"),
            }
        }
        let blocks = assembler
            .blocks()
            .into_iter()
            .map(|b| match b.kind {
                BlockKind::Text => ContentBlock::Text { text: b.text },
                BlockKind::Reasoning => ContentBlock::Reasoning { text: b.text },
                _ => ContentBlock::Text { text: b.text },
            })
            .collect();
        DriverOutcome::Message(MessageRecord {
            id: MessageId(1),
            blocks,
            provider: Some("openai".into()),
            model: Some("replay".into()),
        })
    }));

    // Emitted event sequence: the full turn skeleton with the assistant
    // message committed from the replayed stream.
    let snap = loop_.log().snapshot();
    let kinds: Vec<&str> = snap
        .records
        .iter()
        .map(|r| match &r.event {
            SessionEvent::TurnOpen => "TurnOpen",
            SessionEvent::StepOpen => "StepOpen",
            SessionEvent::AssistantMessage(_) => "AssistantMessage",
            SessionEvent::StepClose => "StepClose",
            SessionEvent::TurnClose { .. } => "TurnClose",
            _ => "Other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "TurnOpen",
            "StepOpen",
            "AssistantMessage",
            "StepClose",
            "TurnClose"
        ]
    );
    assert_eq!(turn.reason, TurnEndReason::Completed);
    // Derived history: one assistant message, three blocks (reasoning,
    // text, tool-call payload), matching the recorded stream.
    assert_eq!(turn.history.len(), 1);
    if let SessionEvent::AssistantMessage(msg) = &snap.records[2].event {
        assert_eq!(msg.blocks.len(), 3);
        assert_eq!(
            msg.blocks[1],
            ContentBlock::Text {
                text: "Hello".into()
            }
        );
    } else {
        panic!("expected assistant message");
    }
}

#[tokio::test]
async fn scripted_failure_replays_in_band_after_its_prefix() {
    let head: Vec<StreamFrame> = recorded_stream()
        .into_iter()
        .take_while(|f| !matches!(f, StreamFrame::Finish))
        .collect();
    let recording = Recording::capture_failed(
        &head,
        &harnless_seams::ProviderFailure {
            code: harnless_seams::ErrorCode::StreamTerminated,
            message: "provider stalled".into(),
        },
    );
    let adapter = ReplayAdapter::new("openai", Script::one(recording));
    let mut stream = adapter
        .stream(CallId(1), &[] as &[Message], &[], None)
        .unwrap();
    let mut frames = 0;
    let mut failure = None;
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Frame(_) => frames += 1,
            StreamEvent::Failed(f) => failure = Some(f),
        }
    }
    assert_eq!(frames, head.len());
    let failure = failure.expect("terminal failure");
    assert_eq!(failure.code, harnless_seams::ErrorCode::StreamTerminated);
}

#[test]
fn corpus_is_a_stable_golden_document() {
    // The golden file is the contract: capture from frames, capture again
    // from the replayed frames, and the documents are identical.
    let recording = Recording::capture(&recorded_stream(), &recorded_replay());
    let adapter = ReplayAdapter::new("openai", Script::one(recording.clone()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let replayed = rt.block_on(async {
        let mut stream = adapter
            .stream(CallId(1), &[] as &[Message], &[], None)
            .unwrap();
        let mut frames = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Frame(f) => frames.push(f),
                StreamEvent::Failed(f) => panic!("{f:?}"),
            }
        }
        frames
    });
    let again = Recording::capture(&replayed, &recorded_replay());
    assert_eq!(again.to_json().unwrap(), recording.to_json().unwrap());
    // And the recording's own tool-call inventory survives the round trip.
    assert_eq!(recording.tool_calls.len(), 1);
    assert_eq!(recording.tool_calls[0].name, "echo");
    let _: &RecordedFrame = recording.frames.last().unwrap();
}
