//! # harnless-llm-replay
//!
//! Deterministic replay model adapter for harnless: replays a recorded model
//! stream with no network, so tests and demos reproduce a conversation piece
//! for piece. Implements the [`ModelAdapter`] seam from `harnless-seams` and
//! is the adapter the primary seam test drives.
//!
//! The corpus is the [`Recording`]: a golden-file JSON document capturing the
//! exact [`StreamFrame`] sequence an adapter emitted, plus the response-level
//! metadata that becomes replay state. Deltas are stored verbatim, so a
//! replayed stream streams like the original.
//!
//! Adapter obligations honored here (conformance contract 05): usage before
//! finish and nothing after; tool arguments raw JSON end to end; the two
//! sanctioned failure paths (a malformed corpus throws from the stream entry,
//! a scripted failure arrives in-band); an empty completion is a retryable
//! failure, not a success; disjoint usage accounting; replay-state ownership
//! stamped with the same marker a live adapter uses, so a replayed message's
//! state is returned only to its owner.

mod adapter;
mod recording;
mod script;

pub use adapter::{ReplayAdapter, ReplayAdapterBuilder};
pub use recording::{RecordedBlock, RecordedFrame, RecordedToolCall, RecordedUsage, Recording};
pub use script::Script;
