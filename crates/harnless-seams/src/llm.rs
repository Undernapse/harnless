//! The model adapter seam.
//!
//! The adapter is the provider-specific bridge to a model endpoint. Its
//! conformance contract (05) is what makes adapters interchangeable:
//!
//! * usage before finish and nothing after;
//! * tool arguments stay raw JSON strings end to end;
//! * exactly two sanctioned failure paths — throw from the stream entry, or
//!   an in-band terminal error — normalizing to one provider-neutral shape;
//! * one adapter call is one provider attempt (library-internal retries off);
//! * stalls bounded by a transport watchdog;
//! * context overflow classified to one canonical code;
//! * an empty completion is a retryable failure, not a success;
//! * a declared identity header on every request.

use std::pin::Pin;

use futures_lite::Stream;
use serde_json::Value;

use crate::error::{ErrorCode, Result};
use crate::ids::{CallId, MessageId};

/// The kind of a content block in a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Reasoning,
    Image,
    ToolCall,
    ToolResult,
}

/// A single content block.
///
/// Tool arguments are **raw JSON strings end to end** — never parsed or
/// re-serialized by the adapter; lossless JSON is preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentBlock {
    /// What kind of information this block carries.
    pub kind: BlockKind,
    /// The block payload. For `ToolCall` / `ToolResult` this is the raw JSON
    /// string; for `Text` / `Reasoning` it is the literal text.
    pub text: String,
}

/// Message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// The model-facing slice of a tool definition: the allowlisted projection
/// an adapter is permitted to send to a provider.
///
/// Internal tool metadata — output contract, execution body, concurrency
/// classification, presentation hooks — never reaches this type; a tool's
/// public schema is name, description, and parameters and nothing else.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSchema {
    /// The tool's name, namespaced under its provider.
    pub name: String,
    /// A natural-language description of what the tool does.
    pub description: String,
    /// The JSON Schema for the tool's arguments object.
    pub parameters: Value,
    /// Whether the provider should constrain calls to the declared properties
    /// (e.g. OpenAI `strict` mode). Defaults to `false`.
    pub strict: bool,
}

impl ToolSchema {
    /// Build a tool schema from the required fields; `strict` defaults off.
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            strict: false,
        }
    }

    /// Mark whether the provider should enforce the declared property set.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
}

/// One message in the conversation.
///
/// Messages are identified and immutable. An assistant message names the
/// provider and model that produced it and may carry adapter-private replay
/// state; where a message came from is a separate axis from what kind of
/// information it is, and the two are deliberately independent.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Opaque message id.
    pub id: MessageId,
    /// The role that produced the message.
    pub role: Role,
    /// Content blocks, in order.
    pub blocks: Vec<ContentBlock>,
    /// Provider name that produced an assistant message (empty otherwise).
    pub provider: Option<String>,
    /// Model name that produced an assistant message (empty otherwise).
    pub model: Option<String>,
    /// Adapter-private replay state. Carried only for the owning adapter.
    pub replay_state: Option<Value>,
}

/// The stream protocol frames a provider emits, in order.
///
/// Block indices correlate interleaved blocks. The stream ends with a
/// terminal `Finish` carrying usage; nothing may be emitted after it.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamFrame {
    /// A block started.
    BlockStart { index: usize, kind: BlockKind },
    /// A text delta appended to block `index`.
    TextDelta { index: usize, text: String },
    /// A reasoning delta appended to block `index`.
    ReasoningDelta { index: usize, text: String },
    /// A tool-call delta appended to block `index` (raw JSON fragments).
    ToolCallDelta {
        index: usize,
        call_id: CallId,
        json: String,
    },
    /// A block ended, carrying its assembled content.
    BlockEnd {
        index: usize,
        assembled: ContentBlock,
    },
    /// Usage accounting.
    Usage(Usage),
    /// Terminal. Nothing follows this frame.
    Finish,
}

/// Disjoint token accounting: uncached input + cached reads + cached writes
/// sum to billed input; reasoning tokens are already inside output and must
/// never be added again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
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

impl Usage {
    /// Billed input = uncached + cached reads + cached writes (disjoint sum).
    pub fn billed_input(&self) -> u64 {
        self.uncached_input + self.cached_reads + self.cached_writes
    }
}

/// The single provider-neutral failure shape both sanctioned failure paths
/// normalize to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    /// Canonical code.
    pub code: ErrorCode,
    /// Provider-neutral message.
    pub message: String,
}

impl ProviderFailure {
    /// Context-overflow failure, classified to the one canonical code.
    pub fn context_overflow() -> Self {
        Self {
            code: ErrorCode::ContextOverflow,
            message: "model context window exceeded".into(),
        }
    }
}

/// A stream event, normalizing the adapter's two failure paths.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A frame from the stream.
    Frame(StreamFrame),
    /// An in-band terminal error (the second sanctioned failure path).
    Failed(ProviderFailure),
}

/// Replay state is adapter-owned but its shape is shared: response-level
/// metadata plus per-block entries aligned to emitted blocks.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReplayState {
    /// Response-level metadata.
    pub response: Option<Value>,
    /// Per-block entries aligned to emitted blocks, in order.
    pub blocks: Vec<Value>,
}

/// A shared assembler that folds a stream of [`StreamFrame`]s into assembled
/// content blocks, with one keep-or-drop decision covering content and
/// metadata together: a truncated finish drops tool calls, because a partial
/// call is unsafe to execute.
///
/// The assembler tracks blocks by their correlating index. A block that ends
/// with a [`StreamFrame::BlockEnd`] is complete; one still open when
/// [`StreamFrame::Finish`] arrives marks the stream truncated. Callers align
/// provider replay state in lockstep via [`BlockAssembler::align_replay`] so
/// stored metadata always describes stored content.
#[derive(Debug, Default)]
pub struct BlockAssembler {
    /// Completely assembled blocks in index order.
    completed: Vec<(usize, ContentBlock)>,
    /// Blocks started but not yet ended, keyed by index.
    open: Vec<usize>,
    /// Accumulated usage, if the provider reported it.
    usage: Option<Usage>,
    /// Whether an open block remained when `Finish` arrived.
    truncated: bool,
}

impl BlockAssembler {
    /// Create an empty assembler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one stream frame into the assembler state.
    pub fn push(&mut self, frame: &StreamFrame) {
        match frame {
            StreamFrame::BlockStart { index, .. } => self.open.push(*index),
            StreamFrame::BlockEnd { index, assembled } => {
                // A block that had already ended is not reopened.
                if let Some(pos) = self.open.iter().position(|i| i == index) {
                    self.open.remove(pos);
                }
                self.completed.push((*index, assembled.clone()));
            }
            StreamFrame::Usage(usage) => self.usage = Some(*usage),
            StreamFrame::Finish => self.truncated = !self.open.is_empty(),
            // Deltas are presentation data; the assembled block on `BlockEnd`
            // is authoritative for the final message.
            StreamFrame::TextDelta { .. }
            | StreamFrame::ReasoningDelta { .. }
            | StreamFrame::ToolCallDelta { .. } => {}
        }
    }

    /// The completed content blocks, in index order, with keep-or-drop
    /// applied: on a truncated finish, tool-call blocks are dropped.
    pub fn blocks(&self) -> Vec<ContentBlock> {
        // Sort by index to keep interleaved blocks deterministic.
        let mut completed = self.completed.clone();
        completed.sort_by_key(|(index, _)| *index);
        completed
            .into_iter()
            .map(|(_, block)| block)
            .filter(|block| !(self.truncated && block.kind == BlockKind::ToolCall))
            .collect()
    }

    /// Whether the stream was truncated (a block was open at `Finish`).
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The accumulated usage, if reported.
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// emitted blocks in index order. Dropped blocks (from a truncated
    /// finish) have their entries pruned so stored metadata always describes
    /// stored content.
    pub fn align_replay(&self, replay: ReplayState) -> ReplayState {
        // All emitted blocks in index order; the drop filter decides which
        // survive. `replay.blocks[i]` aligns to emitted block `i`.
        let mut emitted = self.completed.clone();
        emitted.sort_by_key(|(index, _)| *index);

        let mut blocks = Vec::with_capacity(emitted.len());
        for (i, (_, block)) in emitted.iter().enumerate() {
            let kept = !(self.truncated && block.kind == BlockKind::ToolCall);
            if kept {
                if let Some(entry) = replay.blocks.get(i) {
                    blocks.push(entry.clone());
                }
            }
        }

        ReplayState {
            response: replay.response,
            blocks,
        }
    }
}

/// A boxed, `Send` stream of events.
pub type BoxStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;

/// The model adapter seam.
///
/// One adapter call is one provider attempt. Implementations return a stream
/// of [`StreamEvent`]; the caller drives it to completion.
pub trait ModelAdapter: Send + Sync + 'static {
    /// The provider identity declared in every request header.
    fn provider(&self) -> &str;

    /// Whether this adapter owns `replay_state` — replay state is returned
    /// only to the same adapter instance that registered both the historical
    /// and the target provider.
    fn owns(&self, replay_state: &ReplayState) -> bool;

    /// Stream a completion for `messages`. `call_id` correlates the call.
    ///
    /// `tools` lists the request-time schema set the model may call; the
    /// adapter declares it to the provider on the wire. Throwing from this
    /// entry is the first sanctioned failure path; the caller normalizes it.
    /// In-band terminal errors arrive as a [`StreamEvent::Failed`]. An empty
    /// completion is a retryable failure.
    fn stream(
        &self,
        call_id: CallId,
        messages: &[Message],
        tools: &[ToolSchema],
        replay: Option<ReplayState>,
    ) -> Result<BoxStream>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billed_input_is_the_disjoint_sum() {
        let usage = Usage {
            uncached_input: 10,
            cached_reads: 20,
            cached_writes: 30,
            output: 5,
            reasoning: 2,
        };
        // Reasoning is inside output and never added to billed input.
        assert_eq!(usage.billed_input(), 60);
        // output includes reasoning; billed input excludes output entirely.
        let _ = usage.reasoning;
    }

    #[test]
    fn context_overflow_is_one_canonical_code() {
        let f = ProviderFailure::context_overflow();
        assert_eq!(f.code, ErrorCode::ContextOverflow);
    }

    fn text_block(index: usize, text: &str) -> StreamFrame {
        StreamFrame::BlockEnd {
            index,
            assembled: ContentBlock {
                kind: BlockKind::Text,
                text: text.into(),
            },
        }
    }

    fn tool_block(index: usize, call_id: u64) -> StreamFrame {
        StreamFrame::BlockEnd {
            index,
            assembled: ContentBlock {
                kind: BlockKind::ToolCall,
                text: format!(r#"{{"id":"{}"}}"#, call_id),
            },
        }
    }

    #[test]
    fn assembler_keeps_completed_blocks_in_index_order() {
        let mut a = BlockAssembler::new();
        // Interleaved indexes: text block 0, tool block 1, text block 2.
        a.push(&StreamFrame::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        });
        a.push(&StreamFrame::BlockStart {
            index: 1,
            kind: BlockKind::ToolCall,
        });
        a.push(&StreamFrame::BlockStart {
            index: 2,
            kind: BlockKind::Text,
        });
        a.push(&text_block(2, "world"));
        a.push(&tool_block(1, 7));
        a.push(&text_block(0, "hello"));
        a.push(&StreamFrame::Finish);

        let blocks = a.blocks();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].text, "hello");
        assert_eq!(blocks[1].kind, BlockKind::ToolCall);
        assert_eq!(blocks[2].text, "world");
        assert!(!a.truncated());
    }

    #[test]
    fn truncated_finish_drops_tool_call_blocks() {
        let mut a = BlockAssembler::new();
        a.push(&StreamFrame::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        });
        // Tool block completes but a later block is still open at Finish.
        a.push(&StreamFrame::BlockStart {
            index: 1,
            kind: BlockKind::ToolCall,
        });
        a.push(&StreamFrame::BlockEnd {
            index: 1,
            assembled: ContentBlock {
                kind: BlockKind::ToolCall,
                text: r#"{"id":"7"}"#.into(),
            },
        });
        a.push(&text_block(0, "hello"));
        // Block 2 opened but never ended -> truncation.
        a.push(&StreamFrame::BlockStart {
            index: 2,
            kind: BlockKind::Text,
        });
        a.push(&StreamFrame::Finish);

        assert!(a.truncated());
        let blocks = a.blocks();
        // Tool-call block dropped; the open text block never completed.
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "hello");
    }

    #[test]
    fn align_replay_prunes_in_lockstep_with_dropped_blocks() {
        let mut a = BlockAssembler::new();
        a.push(&StreamFrame::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        });
        a.push(&StreamFrame::BlockStart {
            index: 1,
            kind: BlockKind::ToolCall,
        });
        a.push(&tool_block(1, 7));
        a.push(&text_block(0, "hello"));
        a.push(&StreamFrame::BlockStart {
            index: 2,
            kind: BlockKind::Text,
        });
        a.push(&StreamFrame::Finish);

        let replay = ReplayState {
            response: Some(serde_json::json!({"id": "r1"})),
            blocks: vec![
                serde_json::json!({"index": 0}),
                serde_json::json!({"index": 1}),
                // Emitted block 2 was opened but never completed and is not
                // in `completed`; the provider supplied no entry for it.
            ],
        };
        let aligned = a.align_replay(replay);
        // response metadata survives; only the kept text block's entry remains.
        assert_eq!(aligned.response, Some(serde_json::json!({"id": "r1"})));
        assert_eq!(aligned.blocks.len(), 1);
        assert_eq!(aligned.blocks[0], serde_json::json!({"index": 0}));
    }
}
