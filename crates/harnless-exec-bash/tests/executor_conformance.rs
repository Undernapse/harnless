//! Executor conformance by hand (contract item 2): the argv handed to the
//! sandbox is byte-identical to the argv spawned, and a denied command
//! surfaces an auditable enforced result — never a silent pass.
//!
//! These assertions are what the conformance kit's `check_executor` will
//! mirror later.

use std::sync::Arc;

use harnless_exec_bash::{BashLocal, SHELL_PROGRAM};
use harnless_exec_sandbox::{LocalSandbox, PolicyHomeExt, SandboxMode};
use harnless_exec_subprocess::SubprocessLocal;
use harnless_seams::error::ErrorCode;
use harnless_seams::error::Result;
use harnless_seams::exec::{Enforced, PolicyHome, Sandbox, Shell, Spawn, SpawnHandle, Subprocess};

/// Records the exact bytes it was asked to spawn.
#[derive(Clone, Default)]
struct RecordingSubprocess {
    spawned: Arc<parking_lot::Mutex<Vec<Spawn>>>,
}

impl Subprocess for RecordingSubprocess {
    fn spawn(&self, spawn: &Spawn) -> Result<Box<dyn SpawnHandle>> {
        self.spawned.lock().push(spawn.clone());
        // Never actually run anything: conformance checks the handover.
        Ok(Box::new(NullHandle))
    }
}

struct NullHandle;

impl SpawnHandle for NullHandle {
    fn output(&self) -> Result<String> {
        Ok(String::new())
    }
    fn cancel(&self) {}
}

/// Records the exact argv the sandbox was consulted with, then defers to
/// `inner`.
struct RecordingSandbox {
    seen: Arc<parking_lot::Mutex<Vec<Vec<String>>>>,
    inner: LocalSandbox,
}

impl Sandbox for RecordingSandbox {
    fn enforce(&self, argv: &[String], policy: &PolicyHome) -> Result<Enforced> {
        self.seen.lock().push(argv.to_vec());
        Ok(self.inner.enforce_decision(argv, policy).into_enforced())
    }
}

fn policy(root: &str, mode: SandboxMode) -> PolicyHome {
    PolicyHome::from_root_and_mode(root, mode)
}

#[test]
fn argv_handed_to_sandbox_is_byte_identical_to_argv_spawned() {
    let spawned = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    // A command line with quoting, globs, unicode, and embedded NUL-ish
    // punctuation — anything argv rewriting would mangle.
    let command = r#"echo 'a "b" $c * ? 中文 \ back' > /dev/null"#;
    let shell = BashLocal::new(
        Box::new(RecordingSubprocess {
            spawned: Arc::clone(&spawned),
        }),
        Box::new(RecordingSandbox {
            seen: Arc::clone(&seen),
            inner: LocalSandbox::default(),
        }),
    );
    shell
        .exec(command, &policy(".", SandboxMode::Unconfined))
        .expect("run");

    let seen = seen.lock().pop().expect("sandbox consulted");
    let spawned = spawned.lock().pop().expect("subprocess spawned");
    assert_eq!(
        seen.as_slice(),
        spawned.argv.as_slice(),
        "sandbox saw a different argv than the one that spawned"
    );
    // And it is the exact outer shell invocation, command verbatim.
    assert_eq!(
        seen.as_slice(),
        [SHELL_PROGRAM.to_string(), "-c".to_string(), command.to_string()].as_slice()
    );
    // The sandbox's bytes are the same bytes, not just equal after
    // normalisation: compare the raw UTF-8 of the command element.
    assert_eq!(
        seen[2].as_bytes(),
        command.as_bytes(),
        "command element was rewritten"
    );
    // The spawn cwd is the policy root — same source the fs side reads.
    assert_eq!(Some(".".to_string()), spawned.cwd);
}

#[test]
fn denied_command_surfaces_an_auditable_enforced_result() {
    let spawned = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let shell = BashLocal::new(
        Box::new(RecordingSubprocess {
            spawned: Arc::clone(&spawned),
        }),
        // Policy says confined; the local sandbox follows with sandbox-local,
        // which refuses because the declared root does not exist.
        Box::new(LocalSandbox::new()),
    );
    let err = shell
        .exec(
            "touch /tmp/must-not-exist-13",
            &policy("/nonexistent/root-for-conformance", SandboxMode::SandboxLocal),
        )
        .expect_err("refusal must not pass");
    assert_eq!(ErrorCode::SandboxDenied, err.code, "refusal must be sandbox-denied: {err}");
    // Auditable: the error names the refusing mode.
    assert!(err.message.contains("mode=sandbox-local"), "unenforceable: {err}");
    // And nothing was spawned — the refusal happened before the subprocess.
    assert!(spawned.lock().is_empty(), "denied command still spawned");
}

#[test]
fn denied_command_never_runs_even_against_a_real_shell() {
    // Same refusal, but wired to the real subprocess provider: proves the
    // gate is in the shell path, not an artifact of the recorder.
    let shell = BashLocal::new(
        Box::new(SubprocessLocal::new()),
        Box::new(LocalSandbox::with_mode(SandboxMode::DenyAll)),
    );
    let marker = std::env::temp_dir().join("harnless-13-should-not-exist");
    let err = shell
        .exec(
            &format!("touch {}", marker.display()),
            &policy(".", SandboxMode::DenyAll),
        )
        .expect_err("deny-all must refuse");
    assert_eq!(ErrorCode::SandboxDenied, err.code);
    assert!(
        !marker.exists(),
        "denied command executed anyway: {:?}",
        marker
    );
}

#[test]
fn allowed_command_runs_through_the_real_pipeline() {
    let shell = BashLocal::local();
    let out = shell
        .exec("printf shell-ok", &policy(".", SandboxMode::Unconfined))
        .expect("run");
    assert_eq!("shell-ok", out);
}
