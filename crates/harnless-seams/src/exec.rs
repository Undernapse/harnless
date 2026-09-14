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
///
/// Construct with the field shorthand (`Spawn { argv, cwd, ..Default::default() }`):
/// [`Spawn::confine`] is an additive hint that plain constructors leave
/// `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spawn {
    /// The program to run.
    pub argv: Vec<String>,
    /// The working directory for the subprocess.
    pub cwd: Option<String>,
    /// The confinement a sandbox verdict asks a confinement-capable
    /// spawner to apply to this spawn (env scrub + cwd pin + rlimit,
    /// applied in the forked child).
    ///
    /// The hint is deliberately an opaque, provider-supplied payload: the
    /// seam vocabulary stays dependency-free, and only the exec providers
    /// that understand confinement construct or read it. A [`Subprocess`]
    /// that ignores the field spawns exactly the coordinates it always
    /// did — the field is additive, never a behaviour change for plain
    /// providers.
    pub confine: Option<ConfineHint>,
}

/// An opaque, provider-supplied confinement payload carried on [`Spawn`].
///
/// The seams crate never interprets it; the exec providers define the
/// concrete shape (see `harnless_exec_bash::Confinement`) and share it
/// here, so the spawn coordinates can carry a sandbox verdict through any
/// [`Subprocess`] without the seam vocabulary gaining dependencies.
/// Cloning a hint shares the description — it is immutable once the
/// verdict fixed it.
#[derive(Clone)]
pub struct ConfineHint {
    /// The provider-specific confinement description.
    pub inner: std::sync::Arc<dyn std::any::Any + Send + Sync>,
}

impl ConfineHint {
    /// Wrap a provider's confinement description.
    pub fn new<T: std::any::Any + Send + Sync>(confine: T) -> Self {
        Self {
            inner: std::sync::Arc::new(confine),
        }
    }

    /// Downcast to the provider's concrete description, when it matches.
    pub fn downcast_ref<T: std::any::Any>(&self) -> Option<&T> {
        self.inner.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for ConfineHint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfineHint").finish_non_exhaustive()
    }
}

impl PartialEq for ConfineHint {
    /// Hints compare by identity: they are shared, immutable descriptions.
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for ConfineHint {}

/// What a sandbox actually enforced for a spawned command: the full
/// auditable report, verdict included.
///
/// A refusal is `confined == false`, `allowed == false`, and a `reason`
/// that explains the verdict — nothing is inferred from the mode string.
/// Consumers route on [`Enforced::allowed`] and never on `mode`/`confined`
/// heuristics, so a provider that reports a refusal honestly can never be
/// misread as a permitted run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enforced {
    /// Whether confinement was applied. A refusal is never confinement:
    /// the command does not run, so nothing was confined.
    pub confined: bool,
    /// Whether the command is allowed to spawn. This is the verdict; every
    /// consumer decision (run, refuse, surface to the user) reads it.
    pub allowed: bool,
    /// The confinement mode that was enforced.
    pub mode: String,
    /// Why the verdict is what it is, in human-readable form. Always
    /// non-empty: an auditor must be able to reconstruct the decision from
    /// this struct alone.
    pub reason: String,
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
    ///
    /// The accepted race: a caller that cancels a child which exited on its own
    /// a moment earlier may observe either `exec-cancelled` or the child's own
    /// exit routing from `output` — both are honest about a run whose outcome
    /// was decided before the signal landed. What a provider must never do is
    /// lose a cancellation that reached a *live* child: once `cancel` has
    /// interrupted a running process, `output` reports `exec-cancelled`
    /// whatever the exit status says.
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
