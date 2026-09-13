//! The replay [`ModelAdapter`] implementation.
//!
//! Same contract as any provider, minus the network: frames replay in the
//! recorded order with the recorded deltas, usage precedes finish and
//! nothing follows, tool arguments stay raw JSON end to end, and the replay
//! state a recording carries is returned stamped with the same ownership
//! marker a live adapter uses — so only its owning adapter family claims it
//! back.
//!
//! The two sanctioned failure paths map naturally: a corpus that cannot be
//! decoded is a construction mistake and throws from the stream entry; a
//! scripted failure (a recording captured from a failed stream) arrives
//! in-band as a terminal [`StreamEvent::Failed`]. A recording with no
//! content frames is the empty-completion retryable failure, same
//! classification as a silent live provider.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_stream::stream;

use harnless_seams::{
    BoxStream, CallId, ErrorCode, Message, ModelAdapter, ProviderFailure, ReplayState, Result,
    SeamError, StreamEvent, StreamFrame, ToolSchema,
};

use crate::recording::Recording;
use crate::script::Script;

/// The replay-ownership marker, identical to the live adapters' key. Replay
/// state is adapter-private; this key inside the response metadata names the
/// adapter that produced it. Sharing the key across adapter families is
/// deliberate: a recording captured from an OpenAI stream carries that
/// stream's marker, and a replayed message's state must be claimed back by
/// the adapter that can interpret it.
const REPLAY_OWNER_KEY: &str = "__harnless_provider";

/// A deterministic replay adapter serving a [`Script`].
///
/// Each `stream` call consumes the next recording; the call counter is
/// internal, so the adapter is `Send + Sync` and shareable behind an `Arc`.
pub struct ReplayAdapter {
    provider: String,
    script: Script,
    calls: AtomicUsize,
}

impl ReplayAdapter {
    /// Build an adapter replaying `script` under provider identity
    /// `provider`.
    pub fn new(provider: impl Into<String>, script: Script) -> Self {
        Self {
            provider: provider.into(),
            script,
            calls: AtomicUsize::new(0),
        }
    }

    /// Start a builder for a single-recording adapter.
    pub fn builder(recording: Recording) -> ReplayAdapterBuilder {
        ReplayAdapterBuilder {
            provider: "replay".into(),
            recording,
        }
    }

    /// The number of stream calls served so far.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// Whether a recording's frames carry any content: a block boundary or a
/// delta. A recording of only usage/finish frames is as silent as a live
/// provider that completed without emitting.
fn has_content(frames: &[StreamFrame]) -> bool {
    frames.iter().any(|frame| {
        matches!(
            frame,
            StreamFrame::BlockStart { .. }
                | StreamFrame::TextDelta { .. }
                | StreamFrame::ReasoningDelta { .. }
                | StreamFrame::ToolCallDelta { .. }
                | StreamFrame::BlockEnd { .. }
        )
    })
}

impl ModelAdapter for ReplayAdapter {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn owns(&self, replay_state: &ReplayState) -> bool {
        // Exact-name match, mirroring the live adapters: the marker names
        // the adapter family that produced the state. A replay adapter
        // registered under an identity claims only state stamped with that
        // identity — live provider state it cannot interpret is never
        // claimed, so an owns-gated handoff can never silently drop it.
        replay_state
            .response
            .as_ref()
            .and_then(|r| r.get(REPLAY_OWNER_KEY))
            .and_then(|v| v.as_str())
            .map(|owner| owner == self.provider())
            .unwrap_or(false)
    }

    fn stream(
        &self,
        _call_id: CallId,
        _messages: &[Message],
        _tools: &[ToolSchema],
        _replay: Option<ReplayState>,
    ) -> Result<BoxStream> {
        // Decoding happens at the entry: a corpus that fails to restore or
        // violates the stream protocol is a broken fixture, thrown as the
        // first sanctioned failure path — never disguised as a provider
        // failure the caller might retry.
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let recording: &Recording = self.script.get(n);
        recording
            .validate()
            .map_err(|e| SeamError::new(ErrorCode::ProviderFailure, e))?;
        let frames = recording
            .to_frames()
            .map_err(|e| SeamError::new(ErrorCode::ProviderFailure, e))?;
        let failure = recording.failure.clone();
        let empty = !has_content(&frames);

        let stream = stream! {
            for frame in frames {
                yield StreamEvent::Frame(frame);
            }
            // A recording captured from a failed stream ends in-band with
            // the original terminal failure, after whatever it emitted.
            if let Some(failure) = failure {
                yield StreamEvent::Failed(failure);
            } else if empty {
                // An empty completion is a retryable failure, not a success
                // — same classification as a live provider that went silent.
                yield StreamEvent::Failed(ProviderFailure {
                    code: ErrorCode::EmptyCompletion,
                    message: "recording contains no content".into(),
                });
            }
        };

        Ok(Box::pin(stream))
    }
}

/// Convenience builder for the common single-recording replay adapter.
pub struct ReplayAdapterBuilder {
    provider: String,
    recording: Recording,
}

impl ReplayAdapterBuilder {
    /// Set the provider identity reported by the adapter.
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Build the adapter.
    pub fn build(self) -> ReplayAdapter {
        ReplayAdapter::new(self.provider, Script::one(self.recording))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::RecordedFrame;
    use futures::StreamExt;
    use harnless_seams::{BlockKind, ContentBlock, Usage};
    use serde_json::json;

    fn sample_frames() -> Vec<StreamFrame> {
        vec![
            StreamFrame::BlockStart {
                index: 0,
                kind: BlockKind::Text,
            },
            StreamFrame::TextDelta {
                index: 0,
                text: "Hel".into(),
            },
            StreamFrame::TextDelta {
                index: 0,
                text: "lo".into(),
            },
            StreamFrame::BlockEnd {
                index: 0,
                assembled: ContentBlock {
                    kind: BlockKind::Text,
                    text: "Hello".into(),
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

    fn sample_recording() -> Recording {
        let replay = ReplayState {
            response: Some(json!({"id": "resp-1", "__harnless_provider": "openai"})),
            blocks: vec![json!({"i": 0})],
        };
        Recording::capture(&sample_frames(), &replay)
    }

    async fn collect(adapter: &ReplayAdapter) -> Vec<StreamEvent> {
        let mut stream = adapter
            .stream(CallId(1), &[], &[], None)
            .expect("valid corpus streams");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn replays_the_recorded_stream_verbatim() {
        // The expected stream is written independently of the capture path:
        // what a replay must emit, piece for piece.
        let expected = vec![
            StreamFrame::BlockStart {
                index: 0,
                kind: BlockKind::Text,
            },
            StreamFrame::TextDelta {
                index: 0,
                text: "Hel".into(),
            },
            StreamFrame::TextDelta {
                index: 0,
                text: "lo".into(),
            },
            StreamFrame::BlockEnd {
                index: 0,
                assembled: ContentBlock {
                    kind: BlockKind::Text,
                    text: "Hello".into(),
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
        ];
        assert_eq!(expected, sample_frames(), "fixture drift");
        let adapter = ReplayAdapter::builder(sample_recording()).build();
        let events = collect(&adapter).await;
        let frames: Vec<StreamFrame> = events
            .iter()
            .map(|e| match e {
                StreamEvent::Frame(f) => f.clone(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(frames, expected);
    }

    #[tokio::test]
    async fn replays_is_deterministic_across_runs() {
        let json = sample_recording().to_json().unwrap();
        let a = ReplayAdapter::new("openai", Script::from_json_str(&json).unwrap());
        let b = ReplayAdapter::new("openai", Script::from_json_str(&json).unwrap());
        assert_eq!(collect(&a).await, collect(&b).await);
    }

    #[tokio::test]
    async fn usage_precedes_finish_and_nothing_follows() {
        let adapter = ReplayAdapter::builder(sample_recording()).build();
        let events = collect(&adapter).await;
        let usage_at = events
            .iter()
            .position(|e| matches!(e, StreamEvent::Frame(StreamFrame::Usage(_))));
        let finish_at = events
            .iter()
            .position(|e| matches!(e, StreamEvent::Frame(StreamFrame::Finish)));
        let usage_at = usage_at.expect("usage reported");
        let finish_at = finish_at.expect("finished");
        assert_eq!(finish_at, usage_at + 1);
        assert_eq!(events.len(), finish_at + 1);
    }

    #[tokio::test]
    async fn script_serves_recordings_in_order() {
        let first = sample_recording();
        let mut second = sample_recording();
        // Insert before the terminal frame: a delta after Finish would be a
        // protocol violation the entry now rejects.
        let last = second.frames.len() - 1;
        second.frames.insert(
            last,
            RecordedFrame::TextDelta {
                index: 0,
                text: " (second)".into(),
            },
        );
        let adapter = ReplayAdapter::new("openai", Script::new(vec![first.clone(), second]));
        let a = collect(&adapter).await;
        let b = collect(&adapter).await;
        assert_ne!(a, b, "second call replays the second recording");
        assert_eq!(adapter.calls(), 2);
    }

    #[tokio::test]
    async fn scripted_failure_arrives_in_band() {
        let head: Vec<StreamFrame> = sample_frames()
            .into_iter()
            .take_while(|f| !matches!(f, StreamFrame::Finish))
            .collect();
        let failure = ProviderFailure {
            code: ErrorCode::StreamTerminated,
            message: "provider stalled".into(),
        };
        let recording = Recording::capture_failed(&head, &failure);
        let adapter = ReplayAdapter::builder(recording).provider("openai").build();
        let events = collect(&adapter).await;
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Failed(f)) if f == &failure
        ));
        // The failure is terminal: nothing after it.
        assert_eq!(events.len(), head.len() + 1);
    }

    #[tokio::test]
    async fn empty_recording_is_an_empty_completion_failure() {
        let adapter = ReplayAdapter::builder(Recording::default())
            .provider("openai")
            .build();
        let events = collect(&adapter).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Failed(f) => assert_eq!(f.code, ErrorCode::EmptyCompletion),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_end_only_recording_is_content_not_silence() {
        // A stream that only ever emits an assembled block still produced
        // content; classifying it as an empty completion would report a
        // full answer as provider silence.
        let recording = Recording::capture(
            &[
                StreamFrame::BlockEnd {
                    index: 0,
                    assembled: ContentBlock {
                        kind: BlockKind::Text,
                        text: "full answer".into(),
                    },
                },
                StreamFrame::Finish,
            ],
            &ReplayState::default(),
        );
        let adapter = ReplayAdapter::builder(recording).build();
        let events = collect(&adapter).await;
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Failed(_))),
            "BlockEnd-only content must not fail: {events:?}"
        );
    }

    #[test]
    fn broken_corpus_throws_from_the_entry() {
        // A recording naming an unknown block kind cannot restore; that is a
        // fixture mistake, surfaced at the entry, not in-band.
        let bad = Recording {
            frames: vec![RecordedFrame::BlockStart {
                index: 0,
                kind: "smell".into(),
            }],
            ..Recording::default()
        };
        let adapter = ReplayAdapter::builder(bad).build();
        let err = match adapter.stream(CallId(1), &[], &[], None) {
            Err(err) => err,
            Ok(_) => panic!("broken corpus must throw"),
        };
        assert_eq!(err.code, ErrorCode::ProviderFailure);
    }

    #[test]
    fn owns_state_stamped_with_the_provider_marker() {
        let adapter = ReplayAdapter::builder(sample_recording())
            .provider("openai")
            .build();
        let owned = ReplayState {
            response: Some(json!({"__harnless_provider": "openai", "id": "resp-1"})),
            blocks: vec![],
        };
        assert!(adapter.owns(&owned));

        // A foreign marker is another adapter family's private state: never
        // claimed, so an owns-gated handoff cannot silently drop it.
        let foreign = ReplayState {
            response: Some(json!({"__harnless_provider": "deepseek"})),
            blocks: vec![],
        };
        assert!(!adapter.owns(&foreign));

        let unstamped = ReplayState {
            response: Some(json!({"id": "resp-1"})),
            blocks: vec![],
        };
        assert!(!adapter.owns(&unstamped));
        assert!(!adapter.owns(&ReplayState::default()));
    }

    #[test]
    fn provider_identity_is_declared() {
        let adapter = ReplayAdapter::builder(sample_recording())
            .provider("deepseek")
            .build();
        assert_eq!(adapter.provider(), "deepseek");
    }

    #[test]
    fn frames_after_finish_throw_from_the_entry() {
        // The golden file pins "nothing after finish"; a corpus violating it
        // is a broken fixture, not a stream that replays delinquent frames.
        let bad = Recording {
            frames: vec![
                RecordedFrame::Finish,
                RecordedFrame::TextDelta {
                    index: 0,
                    text: "after terminal".into(),
                },
            ],
            ..Recording::default()
        };
        let adapter = ReplayAdapter::builder(bad).build();
        let err = match adapter.stream(CallId(1), &[], &[], None) {
            Err(err) => err,
            Ok(_) => panic!("post-Finish corpus must throw"),
        };
        assert!(err.message.contains("Finish"), "got: {err}");
    }

    #[test]
    fn finish_and_failure_together_throw_from_the_entry() {
        // A stream ends exactly one way; a corpus claiming both is broken.
        let bad = Recording {
            frames: vec![RecordedFrame::Finish],
            failure: Some(ProviderFailure {
                code: ErrorCode::StreamTerminated,
                message: "and also failed".into(),
            }),
            ..Recording::default()
        };
        let adapter = ReplayAdapter::builder(bad).build();
        assert!(adapter.stream(CallId(1), &[], &[], None).is_err());
    }
}
