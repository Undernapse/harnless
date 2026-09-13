//! # harnless-agent
//!
//! The session log, derived-history projection, agent loop, tool pipeline,
//! core spine services, and system-prompt assembly for harnless.
//!
//! The session log is event-sourced: it is the single source of truth for
//! what the model sees, and message history is derived from it. The agent
//! loop drives that log and exposes its extension points as waterfalls. The
//! spine mounts these as services — never as a privileged entry point —
//! per "no privileged core." Durability is a separate seam.

pub mod events;
pub mod history;
pub mod loop_;
pub mod persistence;
pub mod prompt;
pub mod session;
pub mod spine;
pub mod tools;

pub use events::{
    ChunkRecord, CommittedRecord, ContentBlock, LogRecord, MessageRecord, Position, SessionEvent,
    ToolCallRecord, ToolResultRecord, TurnEndReason,
};
pub use history::{History, SurfaceNode};
pub use loop_::{AgentLoop, DerivedTurn, Driver, DriverOutcome};
pub use persistence::{LoadedLog, SessionPersistence};
pub use prompt::{assemble, in_prompt, SystemPrompt};
pub use session::{LogSnapshot, SessionLog};
pub use spine::Spine;
pub use tools::{MonotonicGuard, PostExecute, PreExecute, ToolRegistry};
