//! Machine-routable error taxonomy.
//!
//! Every seam failure carries a stable [`ErrorCode`] consumers route on;
//! branching on message text is prohibited. Messages are for humans only.
//! Codes group by seam prefix but stay distinct across seams so a caller can
//! tell policy refusal from operating-system failure without inspecting text.

use std::fmt;

/// Stable machine-readable error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    // Filesystem.
    NotFound,
    NotADirectory,
    NotText,
    NotARegularFile,
    TooLarge,
    PermissionDenied,
    SandboxDenied,
    IoError,
    StaleVersion,
    NotObserved,
    AmbiguousEdit,
    EditNotFound,
    Aborted,
    // Tool.
    ToolNotFound,
    ToolPanicked,
    ToolTimeout,
    ToolDenied,
    ToolAborted,
    // Model adapter.
    ProviderFailure,
    StreamTerminated,
    EmptyCompletion,
    ContextOverflow,
    // Execution world.
    SpawnFailed,
    ExecCancelled,
}

impl ErrorCode {
    /// The stable string spelling routers match on.
    pub fn as_str(&self) -> &'static str {
        use ErrorCode::*;
        match self {
            NotFound => "not-found",
            NotADirectory => "not-a-directory",
            NotText => "not-text",
            NotARegularFile => "not-a-regular-file",
            TooLarge => "too-large",
            PermissionDenied => "permission-denied",
            SandboxDenied => "sandbox-denied",
            IoError => "io-error",
            StaleVersion => "stale-version",
            NotObserved => "not-observed",
            AmbiguousEdit => "ambiguous-edit",
            EditNotFound => "edit-not-found",
            Aborted => "aborted",
            ToolNotFound => "tool-not-found",
            ToolPanicked => "tool-panicked",
            ToolTimeout => "tool-timeout",
            ToolDenied => "tool-denied",
            ToolAborted => "tool-aborted",
            ProviderFailure => "provider-failure",
            StreamTerminated => "stream-terminated",
            EmptyCompletion => "empty-completion",
            ContextOverflow => "context-overflow",
            SpawnFailed => "spawn-failed",
            ExecCancelled => "exec-cancelled",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A seam error: a stable code plus a human message and an optional
/// provider-internal source. `code` is the only thing consumers route on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeamError {
    /// Stable machine-readable code.
    pub code: ErrorCode,
    /// Human-readable message.
    pub message: String,
}

impl SeamError {
    /// Create a seam error from a code and a message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Convenience for the code-only case.
    pub fn code(code: ErrorCode) -> Self {
        Self {
            code,
            message: String::new(),
        }
    }
}

impl fmt::Display for SeamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SeamError {}

/// Seam-level result: a seam operation either yields `T` or a [`SeamError`].
pub type Result<T> = std::result::Result<T, SeamError>;
