//! The execution world seam: subprocess, shell, and sandbox.
//!
//! Shared execution world is the point: retargeting filesystem and
//! subprocess at a remote sandbox moves the command, terminal, and
//! language-server providers with it, with no provider forks. The interesting
//! invariants are the cross-provider ones, so these seams own the spawn
//! coordinates and cancellation, and report exactly what a sandbox enforced.

use crate::error::Result;

/// The shared policy home for default confinement mode and workspace root.
///
/// Both the command executor and the filesystem provider read this, so the
/// two can never confine to different roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyHome {
    /// The workspace root paths are confined to.
    pub workspace_root: String,
    /// Whether commands default to a sandbox.
    pub default_confined: bool,
}

/// Spawn coordinates for a subprocess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    /// The program to run.
    pub argv: Vec<String>,
    /// The working directory for the subprocess.
    pub cwd: Option<String>,
}

/// What a sandbox actually enforced for a spawned command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enforced {
    /// Whether confinement was applied.
    pub confined: bool,
    /// The confinement mode that was enforced.
    pub mode: String,
}

/// A running subprocess handle. Cancellation is best-effort.
pub trait Subprocess: Send + Sync + 'static {
    /// Spawn `spawn` and return a handle.
    fn spawn(&self, spawn: &Spawn) -> Result<Box<dyn SpawnHandle>>;
}

/// A handle to a running subprocess.
pub trait SpawnHandle: Send + Sync + 'static {
    /// The collected stdout + stderr, once the process has exited.
    fn output(&self) -> Result<String>;

    /// Best-effort cancellation.
    fn cancel(&self);
}

/// The shell seam: executors consumed by the command tool.
pub trait Shell: Send + Sync + 'static {
    /// Run a shell command and return its output.
    fn exec(&self, command: &str, policy: &PolicyHome) -> Result<String>;
}

/// The sandbox seam: receives the exact argv about to spawn and reports what
/// it enforced.
pub trait Sandbox: Send + Sync + 'static {
    /// Decide what to enforce for `argv` and report it.
    ///
    /// This is where the sandbox is configured, not where it runs; the
    /// executor consumes the reported enforcement.
    fn enforce(&self, argv: &[String], policy: &PolicyHome) -> Result<Enforced>;
}
