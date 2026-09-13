//! The shell execution-world provider.
//!
//! [`BashLocal`] is the [`Shell`] seam consumed by the command tool: it
//! turns a command line into spawn coordinates and runs them through the
//! [`Subprocess`] seam (normally [`SubprocessLocal`]).
//!
//! The sandbox hook runs on the *outer* argv — `[/bin/bash, -c, <command>]`
//! exactly as it is about to be handed to the subprocess provider — so what
//! the sandbox reports is about the command that actually spawns, not a
//! paraphrase of it. The seam [`Enforced`] carries the verdict, so the shell
//! routes on `enforced.allowed`: a refusal is surfaced to the caller as a
//! `SandboxDenied` seam error whose message carries the enforced mode *and*
//! the enforced reason. It never degrades into a silent pass, and it never
//! guesses at the verdict from `confined`/`mode`.

use harnless_seams::error::{ErrorCode, SeamError, Result};
use harnless_seams::exec::{Enforced, PolicyHome, Sandbox, Shell, Spawn, Subprocess};

use harnless_exec_sandbox::LocalSandbox;
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
        // The sandbox sees the exact argv about to spawn. The verdict is
        // consulted before the subprocess is touched.
        let enforced = self.sandbox.enforce(&argv, policy)?;
        if !enforced.allowed {
            // The refusal is the seam's own verdict — no heuristic reads it
            // off `confined`/`mode`. `SandboxDenied` stays distinct from
            // every kernel/permission failure, and the message carries the
            // full auditable report: the mode that refused plus the reason
            // it gave.
            return Err(SeamError::new(
                ErrorCode::SandboxDenied,
                format!(
                    "sandbox denied command: mode={} confined={} reason={:?} argv={argv:?}",
                    enforced.mode, enforced.confined, enforced.reason
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

/// Convenience: run a command and return the full auditable report the
/// local sandbox would give it, without spawning. Command tools use this to
/// show users what enforcement — verdict and reason — a command is about to
/// get.
pub fn preview_decision(command: &str, policy: &PolicyHome) -> Enforced {
    let argv = vec![
        SHELL_PROGRAM.to_string(),
        "-c".to_string(),
        command.to_string(),
    ];
    LocalSandbox::new().enforce_verdict(&argv, policy)
}
