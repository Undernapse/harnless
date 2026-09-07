//! # harnless-seams
//!
//! The seam *trait definitions* for harnless. This crate carries
//! only service definitions — the three-role discipline's "definition" role —
//! never providers and never consumers. A third-party provider can compile
//! against these interfaces alone: this crate depends on nothing but the
//! runtime's type vocabulary.
//!
//! The core seams are:
//!
//! * [`FileSystem`] — opaque targets, backend-owned freshness guards, atomic
//!   edit, windowed reads, machine-routable errors.
//! * [`ModelAdapter`] — content blocks, immutable messages, the stream
//!   protocol, disjoint usage, the two sanctioned failure paths.
//! * [`Settings`] — namespaces, layered resolution, redacted descriptors.
//! * [`Credentials`] — references resolved per operation.
//! * [`Storage`] — a named key-value hub plus a typed domain layer.
//! * [`Subprocess`] / [`Shell`] / [`Sandbox`] — the shared execution world
//!   with a common [`PolicyHome`] so filesystem and subprocess can never
//!   confine to different roots.
//! * [`Tools`] — the tool registry and its guarded pipeline stages.
//!
//! The approval and session seams complete the core set but belong to
//! `harnless-agent` (approval is a waterfall decision; sessions drive the log).

pub mod credentials;
pub mod error;
pub mod exec;
pub mod fs;
pub mod ids;
pub mod llm;
pub mod settings;
pub mod storage;
pub mod tools;

pub use credentials::{CredentialKind, CredentialRef, Credentials};
pub use error::{ErrorCode, Result, SeamError};
pub use exec::{Enforced, PolicyHome, Sandbox, Shell, Spawn, SpawnHandle, Subprocess};
pub use fs::{
    Edit, Entry, FileSystem, MutationResult, ReadWindow, Target, WriteGuard,
};
pub use ids::{CallId, MessageId, SessionId, TargetKey, VersionToken};
pub use llm::{
    BlockAssembler, BlockKind, BoxStream, ContentBlock, Message, ModelAdapter,
    ProviderFailure, ReplayState, Role, StreamEvent, StreamFrame, ToolSchema,
    Usage,
};
pub use settings::{Namespace, RedactedDescriptor, Settings};
pub use storage::{BackendName, OpaqueUnit, Storage, StorageDomain};
pub use tools::{
    FrozenResult, GuardVerdict, PipelineStage, PostDecision, PreDecision, ToolBody,
    ToolDefinition, Tools,
};
