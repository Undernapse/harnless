//! The replay corpus: a recorded model stream, serializable as a golden file.
//!
//! A recording captures exactly what an adapter emits — the [`StreamFrame`]
//! sequence plus the response-level metadata that becomes replay state — so
//! replaying it reproduces the original conversation piece for piece. The
//! format is the golden-file format: a recorded session saved to disk is the
//! corpus the replay adapter and the primary seam test both read.
//!
//! Recordings are produced by folding a live stream through
//! [`Recording::capture`], and replayed by the [`ReplayAdapter`]. The same
//! JSON document serves both directions, so `replay(record(x)) == x` holds
//! whenever `x` honors the seam protocol (a terminal frame, owner-stamped
//! replay state) — [`Recording::validate`] is where a corpus that doesn't
//! gets rejected rather than silently replayed.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use harnless_seams::{BlockKind, ContentBlock, ProviderFailure, ReplayState, StreamFrame, Usage};

/// One recorded content block: its kind and assembled payload.
///
/// Mirrors the seam's [`ContentBlock`] with serde so a recording round-trips
/// losslessly through JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedBlock {
    /// What kind of block this is (`"text"`, `"reasoning"`, `"tool_call"`).
    pub kind: String,
    /// The assembled payload: literal text, or raw JSON for a tool call.
    pub text: String,
}

impl RecordedBlock {
    /// Record an assembled seam block.
    pub fn capture(block: &ContentBlock) -> Self {
        Self {
            kind: block.kind.as_str().to_string(),
            text: block.text.clone(),
        }
    }

    /// Restore the seam block.
    pub fn to_block(&self) -> Result<ContentBlock, String> {
        let kind = BlockKind::parse(&self.kind)
            .ok_or_else(|| format!("unknown block kind {:?}", self.kind))?;
        Ok(ContentBlock {
            kind,
            text: self.text.clone(),
        })
    }
}

/// One recorded tool call: the seam call id the stream minted and the tool
/// name.
///
/// The seam call id is recorded so a replayed stream mints the same ids the
/// original did — a replayed conversation correlates with its original tool
/// results. The provider's own call id lives inside the assembled JSON and
/// is never parsed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedToolCall {
    /// The seam call id the original stream minted.
    pub call_id: u64,
    /// The tool name.
    pub name: String,
}

/// A recorded model stream: the frames an adapter emitted, in order, plus
/// the response-level metadata that becomes its replay state.
///
/// Deltas are stored verbatim (they are the "streamed pieces" a replayed
/// conversation must preserve); assembled blocks are stored alongside so a
/// consumer never re-folds. Usage is recorded when the provider reported it.
///
/// A recording may instead capture a *failed* stream: the frames emitted
/// before the failure plus the terminal [`ProviderFailure`]. Replaying it
/// reproduces the failure in-band at the point it originally occurred —
/// the second sanctioned failure path is part of what a golden file pins.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Recording {
    /// The recorded frames, in emission order.
    pub frames: Vec<RecordedFrame>,
    /// Response-level metadata (provider response id, model, …) that becomes
    /// the adapter's replay state on the message this recording produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    /// Per-block metadata entries aligned to the emitted blocks, in order.
    #[serde(default)]
    pub blocks: Vec<Value>,
    /// The tool calls the stream minted, in mint order.
    #[serde(default)]
    pub tool_calls: Vec<RecordedToolCall>,
    /// The terminal failure, if the recorded stream ended in one instead of
    /// a `Finish`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ProviderFailure>,
}

/// One recorded frame.
///
/// The seam's [`StreamFrame`] is not serde because it carries assembled
/// blocks and call ids as Rust types; the recording form is its lossless
/// JSON twin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordedFrame {
    /// A block started.
    BlockStart {
        /// The correlating block index.
        index: usize,
        /// The block kind.
        kind: String,
    },
    /// A text delta appended to block `index`.
    TextDelta {
        /// The block index.
        index: usize,
        /// The delta text.
        text: String,
    },
    /// A reasoning delta appended to block `index`.
    ReasoningDelta {
        /// The block index.
        index: usize,
        /// The delta text.
        text: String,
    },
    /// A tool-call delta appended to block `index` (raw JSON fragments).
    ToolCallDelta {
        /// The block index.
        index: usize,
        /// The call id this delta belongs to.
        call_id: u64,
        /// The raw JSON fragment.
        json: String,
    },
    /// A block ended, carrying its assembled content.
    BlockEnd {
        /// The block index.
        index: usize,
        /// The assembled block.
        assembled: RecordedBlock,
    },
    /// Usage accounting.
    Usage {
        /// Disjoint token accounting.
        usage: RecordedUsage,
    },
    /// Terminal. Nothing follows this frame.
    Finish,
}

/// Recorded disjoint usage, mirroring the seam's [`Usage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RecordedUsage {
    /// Un-cached input tokens.
    pub uncached_input: u64,
    /// Cached-read input tokens.
    pub cached_reads: u64,
    /// Cached-write input tokens.
    pub cached_writes: u64,
    /// Output tokens (already includes reasoning tokens).
    pub output: u64,
    /// Reasoning tokens (informational detail already inside `output`).
    pub reasoning: u64,
}

impl RecordedUsage {
    /// Record a seam usage value.
    pub fn capture(usage: &Usage) -> Self {
        Self {
            uncached_input: usage.uncached_input,
            cached_reads: usage.cached_reads,
            cached_writes: usage.cached_writes,
            output: usage.output,
            reasoning: usage.reasoning,
        }
    }

    /// Restore the seam usage value.
    pub fn to_usage(&self) -> Usage {
        Usage {
            uncached_input: self.uncached_input,
            cached_reads: self.cached_reads,
            cached_writes: self.cached_writes,
            output: self.output,
            reasoning: self.reasoning,
        }
    }
}

impl Recording {
    /// An empty recording.
    pub fn new() -> Self {
        Self::default()
    }

    /// Capture a live frame stream into a recording.
    ///
    /// This is the write side of the corpus: fold the frames an adapter
    /// emitted (in order) plus the replay state it would have stamped on the
    /// message, and the result replays deterministically.
    pub fn capture(frames: &[StreamFrame], replay: &ReplayState) -> Self {
        // The seam call id is taken from the stream's own ToolCallDelta
        // frames (it is a seam id, not the provider's call_... id, which
        // lives inside the assembled JSON untouched); the name from the
        // assembled block on BlockEnd.
        let mut minted: HashMap<usize, u64> = HashMap::new();
        let mut tool_calls = Vec::new();
        let mut recorded = Vec::with_capacity(frames.len());
        for frame in frames {
            recorded.push(RecordedFrame::capture(frame));
            match frame {
                StreamFrame::ToolCallDelta { index, call_id, .. } => {
                    minted.insert(*index, call_id.0);
                }
                StreamFrame::BlockEnd { index, assembled } => {
                    if assembled.kind == BlockKind::ToolCall {
                        let call_id = minted.remove(index).unwrap_or(0);
                        let name = serde_json::from_str::<Value>(&assembled.text)
                            .ok()
                            .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(String::from))
                            .unwrap_or_default();
                        tool_calls.push(RecordedToolCall { call_id, name });
                    }
                }
                _ => {}
            }
        }
        Self {
            frames: recorded,
            response: replay.response.clone(),
            blocks: replay.blocks.clone(),
            tool_calls,
            failure: None,
        }
    }

    /// Capture a failed live stream: the frames emitted before the terminal
    /// failure, plus the failure itself.
    ///
    /// Replaying the result reproduces the original stream piece for piece
    /// and ends in-band with the same [`ProviderFailure`].
    pub fn capture_failed(frames: &[StreamFrame], failure: &ProviderFailure) -> Self {
        let mut recording = Self::capture(frames, &ReplayState::default());
        recording.failure = Some(failure.clone());
        recording
    }

    /// Restore the recorded frames into seam frames.
    ///
    /// Does not check terminal invariants; a corpus whose frames violate the
    /// stream protocol is caught by [`Recording::validate`], which the
    /// replay adapter runs at the stream entry.
    pub fn to_frames(&self) -> Result<Vec<StreamFrame>, String> {
        self.frames.iter().map(|f| f.to_frame()).collect()
    }

    /// Check the golden file's terminal invariants: `Finish` must be the
    /// last frame with nothing after it, and a recording carrying a
    /// terminal failure must not also carry a `Finish` — a stream ends
    /// exactly one way.
    ///
    /// The seam contract ("usage before finish and nothing after") is what
    /// a golden file pins; a document violating it is a broken fixture, not
    /// a stream outcome.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(pos) = self
            .frames
            .iter()
            .position(|f| matches!(f, RecordedFrame::Finish))
        {
            if pos + 1 != self.frames.len() {
                return Err(format!(
                    "frame {} follows Finish; nothing may follow the terminal frame",
                    pos + 1
                ));
            }
            if self.failure.is_some() {
                return Err(
                    "recording carries both a Finish frame and a terminal failure; \
                     a stream ends exactly one way"
                        .into(),
                );
            }
        }
        Ok(())
    }

    /// The replay state this recording carries: response metadata plus the
    /// per-block entries, exactly as captured.
    pub fn to_replay_state(&self) -> ReplayState {
        ReplayState {
            response: self.response.clone(),
            blocks: self.blocks.clone(),
        }
    }

    /// Parse a recording from its golden-file JSON.
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("invalid recording: {e}"))
    }

    /// Serialize to golden-file JSON (pretty; serde_json's default sorted
    /// key order — golden diffs stay stable).
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| format!("cannot serialize: {e}"))
    }
}

impl RecordedFrame {
    /// Record a seam frame.
    pub fn capture(frame: &StreamFrame) -> Self {
        match frame {
            StreamFrame::BlockStart { index, kind } => Self::BlockStart {
                index: *index,
                kind: kind.as_str().to_string(),
            },
            StreamFrame::TextDelta { index, text } => Self::TextDelta {
                index: *index,
                text: text.clone(),
            },
            StreamFrame::ReasoningDelta { index, text } => Self::ReasoningDelta {
                index: *index,
                text: text.clone(),
            },
            StreamFrame::ToolCallDelta {
                index,
                call_id,
                json,
            } => Self::ToolCallDelta {
                index: *index,
                call_id: call_id.0,
                json: json.clone(),
            },
            StreamFrame::BlockEnd { index, assembled } => Self::BlockEnd {
                index: *index,
                assembled: RecordedBlock::capture(assembled),
            },
            StreamFrame::Usage(usage) => Self::Usage {
                usage: RecordedUsage::capture(usage),
            },
            StreamFrame::Finish => Self::Finish,
        }
    }

    /// Restore the seam frame.
    pub fn to_frame(&self) -> Result<StreamFrame, String> {
        Ok(match self {
            Self::BlockStart { index, kind } => StreamFrame::BlockStart {
                index: *index,
                kind: BlockKind::parse(kind)
                    .ok_or_else(|| format!("unknown block kind {kind:?}"))?,
            },
            Self::TextDelta { index, text } => StreamFrame::TextDelta {
                index: *index,
                text: text.clone(),
            },
            Self::ReasoningDelta { index, text } => StreamFrame::ReasoningDelta {
                index: *index,
                text: text.clone(),
            },
            Self::ToolCallDelta {
                index,
                call_id,
                json,
            } => StreamFrame::ToolCallDelta {
                index: *index,
                call_id: harnless_seams::CallId(*call_id),
                json: json.clone(),
            },
            Self::BlockEnd { index, assembled } => StreamFrame::BlockEnd {
                index: *index,
                assembled: assembled.to_block()?,
            },
            Self::Usage { usage } => StreamFrame::Usage(usage.to_usage()),
            Self::Finish => StreamFrame::Finish,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnless_seams::{CallId, ProviderFailure, StreamFrame};
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
            StreamFrame::BlockStart {
                index: 1,
                kind: BlockKind::ToolCall,
            },
            StreamFrame::ToolCallDelta {
                index: 1,
                call_id: CallId(7),
                json: r#"{"id":7,"name":"echo","arguments":"{\"x\":"#.into(),
            },
            StreamFrame::ToolCallDelta {
                index: 1,
                call_id: CallId(7),
                json: r#"1}"#.into(),
            },
            StreamFrame::BlockEnd {
                index: 1,
                assembled: ContentBlock {
                    kind: BlockKind::ToolCall,
                    text: r#"{"id":7,"name":"echo","arguments":"{\"x\":1}"}"#.into(),
                },
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

    #[test]
    fn recording_round_trips_through_json() {
        let replay = ReplayState {
            response: Some(json!({"id": "resp-1", "__harnless_provider": "replay"})),
            blocks: vec![json!({"i": 0}), json!({"i": 1})],
        };
        let recording = Recording::capture(&sample_frames(), &replay);
        let json = recording.to_json().unwrap();
        let parsed = Recording::from_json(&json).unwrap();
        assert_eq!(parsed, recording);

        let frames = parsed.to_frames().unwrap();
        assert_eq!(frames.len(), sample_frames().len());
        // The terminal frame is Finish; usage precedes it.
        assert!(matches!(frames.last(), Some(StreamFrame::Finish)));
        assert!(matches!(&frames[frames.len() - 2], StreamFrame::Usage(_)));
        // Deltas preserved verbatim so the replayed stream looks streamed.
        assert_eq!(
            frames[1],
            StreamFrame::TextDelta {
                index: 0,
                text: "Hel".into()
            }
        );
        // Replay state round-trips.
        let state = parsed.to_replay_state();
        assert_eq!(state.response, replay.response);
        assert_eq!(state.blocks, replay.blocks);
    }

    #[test]
    fn capture_extracts_tool_calls_from_assembled_blocks() {
        let recording = Recording::capture(&sample_frames(), &ReplayState::default());
        assert_eq!(
            recording.tool_calls,
            vec![RecordedToolCall {
                call_id: 7,
                name: "echo".into()
            }]
        );
    }

    #[test]
    fn failed_stream_capture_round_trips_the_failure() {
        let failure = ProviderFailure {
            code: harnless_seams::ErrorCode::StreamTerminated,
            message: "provider stalled".into(),
        };
        let head: Vec<StreamFrame> = sample_frames()
            .into_iter()
            .take_while(|f| !matches!(f, StreamFrame::Finish))
            .collect();
        let recording = Recording::capture_failed(&head, &failure);
        let parsed = Recording::from_json(&recording.to_json().unwrap()).unwrap();
        assert_eq!(parsed, recording);
        assert_eq!(parsed.failure.as_ref(), Some(&failure));
    }

    #[test]
    fn unknown_block_kind_is_an_error_not_a_silent_drop() {
        let bad = Recording {
            frames: vec![RecordedFrame::BlockStart {
                index: 0,
                kind: "smell".into(),
            }],
            ..Recording::default()
        };
        assert!(bad.to_frames().is_err());
    }
}
