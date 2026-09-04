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
    /// Throwing from this entry is the first sanctioned failure path; the
    /// caller normalizes it. In-band terminal errors arrive as a
    /// [`StreamEvent::Failed`]. An empty completion is a retryable failure.
    fn stream(
        &self,
        call_id: CallId,
        messages: &[Message],
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
}

