//! # dsh-agent
//!
//! The session log, derived-history projection, agent loop, and core spine
//! services for the dsh harness clone.
//!
//! The session log is event-sourced: it is the single source of truth for
//! what the model sees, and message history is derived from it. The agent
//! loop drives that log and exposes its extension points as waterfalls.
//! Durability is a separate seam ([`SessionPersistence`]).

pub mod events;
pub mod history;
pub mod loop_;
pub mod persistence;
pub mod session;

pub use events::{
    ChunkRecord, CommittedRecord, ContentBlock, LogRecord, MessageRecord, Position,
    SessionEvent, ToolCallRecord, ToolResultRecord, TurnEndReason,
};
pub use history::{History, SurfaceNode};
pub use loop_::{AgentLoop, DerivedTurn, Driver, DriverOutcome};
pub use persistence::{LoadedLog, SessionPersistence};
pub use session::{LogSnapshot, SessionLog};
