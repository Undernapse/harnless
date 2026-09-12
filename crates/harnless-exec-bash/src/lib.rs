//! The shell execution-world provider.
//!
//! [`BashLocal`] is the [`Shell`] seam consumed by the command tool: it
//! turns a command line into spawn coordinates and runs them through the
//! [`Subprocess`] seam (normally [`SubprocessLocal`]).
//!
//! The sandbox hook runs on the *outer* argv — `[/bin/bash, -c, <command>]`
//! exactly as it is about to be handed to the subprocess provider — so what
//! the sandbox reports is about the command that actually spawns, not a
//! paraphrase of it. A refusal is surfaced to the caller as a `SandboxDenied`
//! seam error carrying the full enforced report; it never degrades into a
//! silent pass.

use harnless_seams::error::{ErrorCode, SeamError, Result};
use harnless_seams::exec::{PolicyHome, Sandbox, Shell, Spawn, Subprocess};

use harnless_exec_sandbox::{LocalSandbox, SandboxDecision, SandboxMode};
use harnless_exec_subprocess::SubprocessLocal;

/// The program the shell seam invokes.
pub const SHELL_PROGRAM: &str = "/bin/bash";

/// The shell seam over the subprocess seam.
pub struct BashLocal {
    /// The subprocess provider the coordinates are handed to.
    pub subprocess: Box<dyn Subprocess>,
    /// The sandbox consulted with the exact argv before spawn.
    pub sandbox: Box<dyn Sandbox>,
}

impl std::fmt::Debug for BashLocal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashLocal").finish_non_exhaustive()
    }
}

impl BashLocal {
    /// Wire a shell over any subprocess and sandbox provider pair.
    pub fn new(subprocess: Box<dyn Subprocess>, sandbox: Box<dyn Sandbox>) -> Self {
        Self { subprocess, sandbox }
    }

    /// The default wiring: subprocess-local execution with the local
    /// sandbox consulted on every command.
    pub fn local() -> Self {
        Self::new(
            Box::new(SubprocessLocal::new()),
            Box::new(LocalSandbox::default()),
        )
    }

    /// The exact argv a command line will be spawned with.
    ///
    /// Public so callers (and conformance tests) can assert the sandbox saw
    /// the same bytes the subprocess receives.
    pub fn argv_for(&self, command: &str) -> Vec<String> {
        vec![
            SHELL_PROGRAM.to_string(),
            "-c".to_string(),
            command.to_string(),
        ]
    }
}

impl Shell for BashLocal {
    fn exec(&self, command: &str, policy: &PolicyHome) -> Result<String> {
        let argv = self.argv_for(command);
        // The sandbox sees the exact argv about to spawn. Enforcement is
        // consulted before the subprocess is touched.
        let enforced = self.sandbox.enforce(&argv, policy)?;
        if refused(&enforced) {
            // The seam `Enforced` has no verdict field, so the refusal
            // travels in the error: `SandboxDenied` is distinct from every
            // kernel/permission failure, and the message carries the mode
            // that refused. LocalSandbox's full decision (verdict + reason)
            // is available via `enforce_decision` for auditors.
            return Err(SeamError::new(
                ErrorCode::SandboxDenied,
                format!(
                    "sandbox denied command: mode={} confined={} argv={argv:?}",
                    enforced.mode, enforced.confined
                ),
            ));
        }
        let spawn = Spawn {
            argv,
            cwd: Some(policy.workspace_root.clone()),
        };
        let handle = self.subprocess.spawn(&spawn)?;
        <dyn harnless_seams::exec::SpawnHandle>::output(&*handle)
    }
}

/// Whether a seam [`Enforced`] reports a refusal.
///
/// [`LocalSandbox`] encodes the verdict in the seam struct as
/// `confined == false` with a confinement mode named on it — the only
/// combination that cannot be a permitted run (a permitted unconfined run
/// has mode `unconfined`).
fn refused(enforced: &harnless_seams::exec::Enforced) -> bool {
    !enforced.confined && enforced.mode != SandboxMode::Unconfined.as_str()
}

/// Convenience: run a command and return the full auditable decision the
/// local sandbox would make, without spawning. Command tools use this to
/// show users what enforcement a command is about to get.
pub fn preview_decision(command: &str, policy: &PolicyHome) -> SandboxDecision {
    let argv = vec![
        SHELL_PROGRAM.to_string(),
        "-c".to_string(),
        command.to_string(),
    ];
    LocalSandbox::new().enforce_decision(&argv, policy)
}
