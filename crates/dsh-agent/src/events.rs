//! The session event vocabulary.
//!
//! The event-sourced session log is the single source of truth for what the
//! model sees. This is the **closed** core vocabulary (decision 04): a tagged
//! serde enum. Plugins contribute log-only event types by name through the
//! extension registry (a separate `dsh-seams` concern), but the core set is
//! closed and lossless-JSON.
//!
//! Every record carries a writer-assigned position (the log length, for
//! contiguity) and an epoch-millisecond time. Position and time are never
//! caller-supplied.
//!
//! Only the three *message-producing* kinds — [`SessionEvent::UserMessage`],
//! [`SessionEvent::AssistantMessage`], and [`SessionEvent::ToolResult`] as
//! consumed by the surface — declare how they join derived history. Structural
//! records never project a message.

use serde::{Deserialize, Serialize};

use dsh_seams::{CallId, MessageId};

/// The position of a record in the log. Contiguity is a contract: position
/// equals the log length at append time (a fresh session starts at 0).
pub type Position = usize;

/// The reason a turn ended.
///
/// `MaxTokens` wins over a later clean stop; `Interrupted` is synthesized
/// only by crash recovery, never by the loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEndReason {
    /// The turn completed normally.
    Completed,
    /// The turn was aborted by a caller-supplied cause.
    Aborted { cause: String },
    /// The turn was blocked by policy.
    Blocked,
    /// The turn ended in a structured error (never a bare string).
    Error { code: String, message: String },
    /// The model's context/token budget was exhausted.
    MaxTokens,
    /// The turn was interrupted by crash recovery; the loop never emits this.
    Interrupted,
}

/// A log-only record: a whole-list snapshot, a request envelope, a route
/// context, or a seed boundary. These never project a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogRecord {
    /// A whole-list snapshot of the derived history at a point in time.
    Snapshot { nodes: Vec<MessageId> },
    /// The request envelope for a turn.
    RequestEnvelope,
    /// The route context a request travelled under.
    RouteContext,
    /// Marks where the current process's writes begin (seed boundary).
    SeedBoundary,
}

/// One session event.
///
/// This is the closed, lossless-JSON core vocabulary. Multiple event kinds
/// are message-producing and join derived history; the rest are structural or
/// log-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A turn opened.
    TurnOpen,
    /// A turn closed, carrying its end reason.
    TurnClose { reason: TurnEndReason },
    /// A step opened within a turn.
    StepOpen,
    /// A step closed.
    StepClose,
    /// A user message (message-producing).
    UserMessage(MessageRecord),
    /// An assistant streamed chunk (replay/presentation data; excluded from
    /// derivation).
    AssistantChunk(ChunkRecord),
    /// An assembled assistant message (message-producing).
    AssistantMessage(MessageRecord),
    /// A tool call.
    ToolCall(ToolCallRecord),
    /// A tool result (message-producing in the derived surface).
    ToolResult(ToolResultRecord),
}

/// The fields of a message record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecord {
    /// Opaque message id.
    pub id: MessageId,
    /// The free-standing message content.
    pub blocks: Vec<ContentBlock>,
    /// Provider that produced an assistant message, if any.
    pub provider: Option<String>,
    /// Model that produced an assistant message, if any.
    pub model: Option<String>,
}

/// A content block in a message record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Reasoning { text: String },
    ToolCall { call_id: CallId, arguments: String },
    ToolResult { call_id: CallId, content: String },
}

/// A streamed chunk of an assistant message (presentation/replay data).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// The message id this chunk belongs to.
    pub message_id: MessageId,
    /// The block index within the message.
    pub block_index: usize,
    /// The delta text.
    pub delta: String,
}

/// A tool call record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Correlates with the result.
    pub call_id: CallId,
    /// The tool name.
    pub tool: String,
    /// Raw-JSON arguments.
    pub arguments: String,
}

/// A tool result record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultRecord {
    /// Correlates with the call.
    pub call_id: CallId,
    /// Raw-JSON canonical result.
    pub content: String,
}

impl SessionEvent {
    /// Whether this event kind is *message-producing* and may declare how it
    /// joins derived history.
    pub fn is_message_producing(&self) -> bool {
        matches!(
            self,
            SessionEvent::UserMessage(_)
                | SessionEvent::AssistantMessage(_)
                | SessionEvent::ToolResult(_)
        )
    }
}

/// A committed log record: the event plus its writer-assigned position and
/// time. Immutable once written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedRecord {
    /// Writer-assigned position (log length at append).
    pub position: Position,
    /// Epoch milliseconds, writer-assigned.
    pub time_ms: u64,
    /// The committed event.
    pub event: SessionEvent,
}
