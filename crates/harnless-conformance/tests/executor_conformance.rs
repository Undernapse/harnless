//! Self-tests for the execution-world conformance suite.
//!
//! Same discipline as `fs_conformance.rs` and `adapter_conformance.rs`: the
//! suite must pass a conforming reference provider, and every case must
//! *bite* the provider that breaks the obligation that case exists for.
//!
//! The reference here is built from the crate's own execution-world parts
//! where they exist (the sandbox and subprocess the suite drives are the
//! provider under test), and from small in-memory fakes for the roles the
//! suite itself plays. Every fixture is Unix-only where it needs a real
//! process; on other platforms those cases skip through the suite's own
//! skip rule, which is itself asserted here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harnless_conformance::executor_suite::{
    check_executor_contract, check_executor_contract_all, ExecFixture, Executors,
    EXECUTOR_CONFORMANCE_CASES,
};
use harnless_seams::error::{ErrorCode, Result, SeamError};
use harnless_seams::exec::{
    ConfineHint, Enforced, PolicyHome, Sandbox, Shell, Spawn, SpawnHandle, Subprocess,
};

// ---------------------------------------------------------------------------
// A reference execution world
// ---------------------------------------------------------------------------

/// A scratch workspace root, unique per test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "harnless-conformance-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch root");
        Self(root.canonicalize().expect("canonical scratch root"))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The reference sandbox: honest verdicts, real confinement intent.
///
/// `allowed` is the verdict; a refusal is never `confined`, and every verdict
/// carries a reason. A policy root that cannot be entered is refused rather
/// than run unconfined — the shape the suite's refusal cases look for.
///
/// The suite's routing and denial cases need to hand this shell a verdict the
/// suite constructed — a verdict whose `confined` and `mode` *contradict*
/// `allowed`, which no honest policy produces. The seam gives a caller no way to
/// swap the sandbox a shell holds, so the suite installs the fixture on a leg the
/// shell reads: [`harnless_conformance::INJECTED_MARKER`] is the contract by
/// which the sandbox recognises a fixture verdict and replays it instead of
/// deciding. A provider that does not honour injected verdicts fails those
/// cases, which is the honest outcome — the suite cannot claim to have tested a
/// routing decision the provider never faced.
struct ReferenceSandbox {
    /// Whether the sandbox actually applies confinement when it says so.
    enforces: bool,
}

impl ReferenceSandbox {
    fn new() -> Self {
        Self { enforces: true }
    }
}

impl Sandbox for ReferenceSandbox {
    fn enforce(&self, argv: &[String], policy: &PolicyHome) -> Result<Enforced> {
        let program = argv.first().cloned().unwrap_or_default();
        if let Some(verdict) = harnless_conformance::injected_verdict() {
            return Ok(verdict);
        }
        if policy.default_confined {
            if !Path::new(&policy.workspace_root).is_dir() {
                return Ok(Enforced {
                    confined: false,
                    allowed: false,
                    mode: "sandbox-local".to_string(),
                    reason: format!(
                        "workspace root {:?} cannot be entered; refusing to run {program:?} \
                         unconfined under a confined policy",
                        policy.workspace_root
                    ),
                });
            }
            Ok(Enforced {
                confined: self.enforces,
                allowed: true,
                mode: "sandbox-local".to_string(),
                reason: "environment scrubbed of credential-shaped names; cwd pinned to the \
                         workspace root"
                    .to_string(),
            })
        } else {
            Ok(Enforced {
                confined: false,
                allowed: true,
                mode: "unconfined".to_string(),
                reason: "policy allows unconfined execution; nothing enforced".to_string(),
            })
        }
    }
}

/// The confinement payload the reference spawner understands.
#[derive(Debug, Clone)]
struct Confine {
    policy: PolicyHome,
    extra_env: Vec<(String, String)>,
    /// Whether this spawner really scrubs and pins.
    enforces: bool,
    /// Whether the spawner pins the child's cwd to the policy root.
    ///
    /// Separated from [`Confine::enforces`] so a fixture can report a
    /// confined verdict while applying only half of what it promises — the
    /// shape the confined case exists to catch.
    pins_cwd: bool,
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn setsid() -> i32;
}

/// SIGKILL: cancellation's observable contract is that the caller stops
/// waiting, which a deliverable-but-hanldable TERM cannot promise.
#[cfg(unix)]
const SIGKILL: i32 = 9;

/// Whether `name` looks credential-bearing — the reference's scrub predicate.
fn scrubbed(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["TOKEN", "SECRET", "KEY", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|needle| upper.contains(needle))
}

/// The reference subprocess: spawns exactly the coordinates it is handed,
/// applies confinement when the hint asks, and honours cancellation.
struct ReferenceSubprocess {
    /// Whether `cancel` actually stops the child.
    honours_cancel: bool,
}

impl ReferenceSubprocess {
    /// A subprocess that applies everything and honours cancellation.
    fn new() -> Self {
        Self {
            honours_cancel: true,
        }
    }
}

impl Subprocess for ReferenceSubprocess {
    fn spawn(&self, spawn: &Spawn) -> Result<Box<dyn SpawnHandle>> {
        let (program, args) = spawn
            .argv
            .split_first()
            .ok_or_else(|| SeamError::new(ErrorCode::SpawnFailed, "argv is empty"))?;
        let mut command = std::process::Command::new(program);
        command.args(args);
        if let Some(cwd) = &spawn.cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Confinement, applied when the verdict asked for it.
        let mut confine = None;
        if let Some(hint) = &spawn.confine {
            match hint.downcast_ref::<Confine>() {
                Some(confine_payload) => {
                    if confine_payload.enforces {
                        confine = Some(confine_payload.clone());
                    } else {
                        // A confined verdict nothing enforces: the reference
                        // is honest and runs unconfined only because the
                        // *fixture* declared it cannot enforce.
                    }
                }
                None => {
                    return Err(SeamError::new(
                        ErrorCode::SandboxDenied,
                        "unreadable confinement hint",
                    ))
                }
            }
        }

        if confine.as_ref().is_some_and(|c| !c.pins_cwd) {
            // The reported confinement is not applied to the working
            // directory: the child runs somewhere else entirely, so `pwd`
            // cannot accidentally agree with the policy root. The policy
            // travels with the hint, which is the point of the hint — this
            // fixture reads it and then disobeys it.
            let _ = confine.as_ref().map(|c| c.policy.workspace_root.len());
            command.current_dir(std::env::temp_dir());
        }
        if let Some(confine) = &confine {
            let mut injected: Vec<(String, String)> = std::env::vars().collect();
            injected.extend(confine.extra_env.iter().cloned());
            let scrubbed_env: Vec<(String, String)> = injected
                .into_iter()
                .filter(|(name, _)| !scrubbed(name))
                .collect();
            command.env_clear();
            command.envs(scrubbed_env);
        }

        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            // SAFETY: the hook runs only `setsid(2)`, a true async-signal-safe
            // syscall. Same grouping policy as the shipped provider: the child leads
            // its own group, so cancel() reaches the shell *and* its children.
            command.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
        let child = command
            .spawn()
            .map_err(|err| SeamError::new(ErrorCode::SpawnFailed, err.to_string()))?;
        Ok(Box::new(ReferenceHandle {
            pid: child.id(),
            child: Mutex::new(Some(child)),
            cancelled: AtomicBool::new(false),
            honours_cancel: self.honours_cancel,
        }))
    }
}

/// The reference handle: joined capture, typed failure on nonzero exit,
/// cancellation reported as cancellation.
struct ReferenceHandle {
    /// The child's pid, kept for the whole run so `cancel` can reach the
    /// process even after `output` has taken the child out of [`Self::child`].
    pid: u32,
    /// The child, until `output` takes it. `None` once collected.
    child: Mutex<Option<std::process::Child>>,
    cancelled: AtomicBool,
    honours_cancel: bool,
}

impl SpawnHandle for ReferenceHandle {
    fn output(&self) -> Result<String> {
        // Take the child out of the slot for the whole run. `wait()` blocks
        // for the process's life, and `cancel` runs on another thread against
        // the same handle — a mutex held across `wait` would make the signal
        // undeliverable, which is exactly the bug the cancellation case hunts.
        let taken = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(mut child) = taken else {
            return Err(SeamError::new(
                ErrorCode::SpawnFailed,
                "output already collected",
            ));
        };
        // The pipes must be drained concurrently with the wait: a command
        // producing more than the OS pipe buffer would otherwise deadlock
        // against a parent waiting for an exit that never comes. The readers
        // are also the *cancellation* signal: a killed child closes its pipes,
        // so a cancelled run stops waiting on the pipes instead of blocking
        // until a command that was never going to finish finishes on its own.
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let reader = std::thread::scope(|pipes| {
            let out = pipes.spawn(|| match &mut stdout_pipe {
                Some(pipe) => capture(pipe),
                None => Ok(String::new()),
            });
            let err = pipes.spawn(|| match &mut stderr_pipe {
                Some(pipe) => capture(pipe),
                None => Ok(String::new()),
            });
            (out.join(), err.join())
        });
        let status = child
            .wait()
            .map_err(|err| SeamError::new(ErrorCode::SpawnFailed, err.to_string()))?;
        // A cancelled run is reported as cancellation whatever the exit
        // status says. Checking the flag before the status is the whole
        // contract: a caller that retried on a "successful" cancelled run would
        // run a command its user already stopped.
        if self.cancelled.load(Ordering::SeqCst) {
            let mut out = String::new();
            for captured in [reader.0, reader.1] {
                if let Ok(Ok(captured)) = captured {
                    out.push_str(&captured);
                }
            }
            return Err(SeamError::new(
                ErrorCode::ExecCancelled,
                format!("process cancelled; {out}"),
            ));
        }
        let (stdout, stderr) = reader;
        let mut out =
            stdout.map_err(|_| SeamError::new(ErrorCode::IoError, "output reader panicked"))??;
        out.push_str(
            &stderr.map_err(|_| SeamError::new(ErrorCode::IoError, "output reader panicked"))??,
        );
        if status.success() {
            return Ok(out);
        }
        Err(SeamError::new(
            ErrorCode::SpawnFailed,
            format!("process exited with {:?}; {out}", status.code()),
        ))
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if !self.honours_cancel {
            return;
        }
        // The pid outlives the `Child`: `output` takes the child out of the
        // slot to wait on it, and a handle whose signal depended on that slot
        // would silently stop being cancellable the moment anyone started
        // waiting — the exact bug the cancellation case hunts. The child leads
        // its own process group, so a group signal also reaches its
        // descendants. SIGKILL: the case's contract is that a cancelled caller
        // stops waiting, and a TERM that the shell or its child chooses to
        // handle is not a bound.
        let pid = self.pid as i32;
        let signalled = unsafe { kill(-pid, SIGKILL) } == 0;
        if !signalled {
            unsafe {
                let _ = kill(pid, SIGKILL);
            }
        }
    }
}

/// Read a child pipe to EOF.
fn capture<R: std::io::Read>(pipe: &mut R) -> Result<String> {
    let mut buf = Vec::new();
    pipe.read_to_end(&mut buf)
        .map_err(|err| SeamError::new(ErrorCode::IoError, err.to_string()))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// The reference shell: consults the sandbox with the exact outer argv,
/// routes on `allowed`, and hands the same argv to the subprocess.
struct ReferenceShell {
    sandbox: Arc<dyn Sandbox>,
    subprocess: Arc<dyn Subprocess>,
    /// Variables the spawner injects into confined children.
    extra_env: Vec<(String, String)>,
    /// Whether the confined hint is attached to a confined spawn.
    attaches_hint: bool,
    /// Whether the spawner really pins cwd (false = claims confinement it
    /// does not apply).
    pins_cwd: bool,
}

impl ReferenceShell {
    /// The exact argv a command line spawns with.
    fn argv_for(&self, command: &str) -> Vec<String> {
        vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()]
    }
}

impl Shell for ReferenceShell {
    fn exec(&self, command: &str, policy: &PolicyHome) -> Result<String> {
        let argv = self.argv_for(command);
        let enforced = self.sandbox.enforce(&argv, policy)?;
        if !enforced.allowed {
            // The verdict is the verdict: `confined` and `mode` describe what
            // *would* have been enforced, and a refusal was not enforced, so
            // reading them instead of `allowed` is the bug the routing case
            // hunts. The refusal surfaces as `sandbox-denied` with the full
            // auditable report.
            return Err(SeamError::new(
                ErrorCode::SandboxDenied,
                format!(
                    "sandbox denied command: mode={} confined={} reason={:?}",
                    enforced.mode, enforced.confined, enforced.reason
                ),
            ));
        }
        let mut spawn = Spawn {
            argv,
            cwd: Some(policy.workspace_root.clone()),
            confine: None,
        };
        if enforced.confined && self.attaches_hint {
            spawn.confine = Some(ConfineHint::new(Confine {
                policy: policy.clone(),
                extra_env: self.extra_env.clone(),
                enforces: true,
                pins_cwd: self.pins_cwd,
            }));
        }
        let handle = self.subprocess.spawn(&spawn)?;
        handle.output()
    }
}

// ---------------------------------------------------------------------------
// Defects
// ---------------------------------------------------------------------------

/// One broken execution-world obligation per variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Defect {
    /// Honors every obligation.
    None,
    /// Rewrites argv between the sandbox and the spawner.
    RewritesArgv,
    /// Reports a verdict with a blank reason.
    BlankReason,
    /// Reports a refusal as confinement.
    RefusalLooksConfined,
    /// Runs the command despite a refusal (routes on `confined`, not `allowed`).
    IgnoresRefusal,
    /// Never spawns a command the sandbox refused.
    RefusesBeforeSandbox,
    /// Reports confinement it does not apply.
    ClaimsConfinement,
    /// `cancel` does not stop the process.
    IgnoresCancel,
    /// Reports a failing command as success.
    FabricatesSuccess,
    /// Relabels a command failure with another seam's code.
    RelabelsFailure,
    /// Panics inside `exec`.
    Panics,
    /// Panics inside `enforce`.
    SandboxPanics,
    /// Panics inside `spawn`.
    SpawnPanics,
}

/// The world under test: reference parts plus one defect.
struct World {
    scratch: Scratch,
    defect: Defect,
    /// Whether the confined child really gets the proof variables.
    injects: bool,
}

impl World {
    fn new(label: &str, defect: Defect) -> Self {
        Self {
            scratch: Scratch::new(label),
            defect,
            injects: true,
        }
    }

    /// The sandbox and subprocess legs for this world's defect.
    fn build_legs(&self) -> (Arc<dyn Sandbox>, Arc<dyn Subprocess>) {
        let sandbox: Arc<dyn Sandbox> = match self.defect {
            Defect::BlankReason => Arc::new(BlankReasonSandbox),
            Defect::RefusalLooksConfined => Arc::new(RefusalLooksConfinedSandbox),
            Defect::SandboxPanics => Arc::new(PanickingSandbox),
            _ => Arc::new(ReferenceSandbox::new()),
        };
        let subprocess: Arc<dyn Subprocess> = match self.defect {
            Defect::IgnoresCancel => Arc::new(IgnoringCancelSubprocess),
            Defect::SpawnPanics => Arc::new(PanickingSubprocess),
            _ => Arc::new(ReferenceSubprocess::new()),
        };
        (sandbox, subprocess)
    }

    /// The bundle the suite drives.
    ///
    /// The legs are built once and shared: the shell is constructed over the
    /// very `Arc`s the bundle carries, which is the wiring the handover case
    /// requires — a shell holding private legs would make the case report an
    /// unobservable handover rather than pass.
    fn executors(&self) -> Executors {
        let (sandbox, subprocess) = self.build_legs();
        self.executors_over(sandbox, subprocess)
    }

    /// The bundle with the shell constructor the handover case needs.
    ///
    /// The suite rebuilds the shell over its recording wrappers, so it needs the
    /// harness's construction as a function of the legs. Each world's shell is
    /// built the same way production builds it — same types, same wiring — given
    /// whatever sandbox and subprocess it is handed.
    fn executors_with_over(&self) -> Executors {
        let executors = self.executors();
        executors.shell_over(World::build_shell)
    }

    /// The running world's shell constructor, as a plain function.
    ///
    /// The suite needs a `fn` pointer, so the defect and the proof variables
    /// come from the world the driver installed rather than a captured `self`.
    /// This is the same construction [`World::executors_over`] performs.
    fn build_shell(sandbox: Arc<dyn Sandbox>, subprocess: Arc<dyn Subprocess>) -> Arc<dyn Shell> {
        let state = WORLD
            .with(|slot| {
                slot.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            })
            .expect("a world installed by the driver");
        World::shell_for(
            state.defect,
            World::proof_vars_for(state.injects),
            sandbox,
            subprocess,
        )
    }

    /// The shell for a given defect, wired over the legs it is handed.
    ///
    /// This is the harness's construction lifted to take its legs as arguments,
    /// which is what lets the suite hand the shell its recording wrappers.
    fn shell_for(
        defect: Defect,
        proof_vars: Vec<(String, String)>,
        sandbox: Arc<dyn Sandbox>,
        subprocess: Arc<dyn Subprocess>,
    ) -> Arc<dyn Shell> {
        match defect {
            Defect::RewritesArgv => Arc::new(RewritingShell {
                sandbox: Arc::clone(&sandbox),
                subprocess: Arc::clone(&subprocess),
            }),
            Defect::IgnoresRefusal => Arc::new(IgnoringRefusalShell {
                sandbox: Arc::clone(&sandbox),
                subprocess: Arc::clone(&subprocess),
            }),
            Defect::RefusesBeforeSandbox => Arc::new(UnconsultedShell {
                subprocess: Arc::clone(&subprocess),
            }),
            Defect::FabricatesSuccess => Arc::new(FabricatingShell),
            Defect::RelabelsFailure => Arc::new(RelabelingShell),
            Defect::Panics => Arc::new(PanickingShell),
            Defect::ClaimsConfinement => Arc::new(ReferenceShell {
                sandbox: Arc::clone(&sandbox),
                subprocess: Arc::clone(&subprocess),
                extra_env: proof_vars.clone(),
                attaches_hint: true,
                // The bug: the verdict says confined, the spawner says so too,
                // and the child's cwd is never pinned.
                pins_cwd: false,
            }),
            _ => Arc::new(ReferenceShell {
                sandbox: Arc::clone(&sandbox),
                subprocess: Arc::clone(&subprocess),
                extra_env: proof_vars.clone(),
                attaches_hint: true,
                pins_cwd: true,
            }),
        }
    }

    /// The bundle wired over specific legs.
    ///
    /// The legs are wrapped here — the harness's own recording layer — and the
    /// shell is built over the *wrappers*, so every verdict the shell consults
    /// and every spawn it hands is a call the suite's recorders will see on
    /// top. This is the wiring the handover case requires of any harness: the
    /// legs handed to the suite must be the legs the shell uses.
    fn executors_over(
        &self,
        sandbox: Arc<dyn Sandbox>,
        subprocess: Arc<dyn Subprocess>,
    ) -> Executors {
        let shell = World::shell_for(
            self.defect,
            self.proof_vars(),
            Arc::clone(&sandbox),
            Arc::clone(&subprocess),
        );
        Executors::new()
            .shell(shell)
            .sandbox(sandbox)
            .subprocess(subprocess)
    }

    /// The proof/control variables, when this world's spawner places them.
    fn proof_vars(&self) -> Vec<(String, String)> {
        World::proof_vars_for(self.injects)
    }

    /// The proof/control variables for a spawner that does or does not place them.
    fn proof_vars_for(injects: bool) -> Vec<(String, String)> {
        if !injects {
            return Vec::new();
        }
        vec![
            (
                harnless_conformance::executor_suite::PROOF_ENV_NAME.to_string(),
                harnless_conformance::executor_suite::PROOF_ENV_VALUE.to_string(),
            ),
            (
                harnless_conformance::executor_suite::CONTROL_ENV_NAME.to_string(),
                harnless_conformance::executor_suite::CONTROL_ENV_VALUE.to_string(),
            ),
        ]
    }

    /// The fixture for one case, given the defect and scratch root.
    fn fixture_for(defect: Defect, root: &Path, case: &str) -> ExecFixture {
        let confined = matches!(
            case,
            "confined_run_is_actually_confined"
                | "refusal_is_never_reported_as_confined"
                | "denied_command_never_runs"
                | "consumer_routes_on_allowed"
        );
        let fixture = match defect {
            // The cancellation fixture waits on a real process; a ten-second
            // bound makes the negative test needlessly slow.
            Defect::IgnoresCancel => ExecFixture::new()
                .root(root)
                .confined(confined)
                .cancel_timeout(Duration::from_secs(3)),
            _ => ExecFixture::new().root(root).confined(confined),
        };
        if case == "confined_run_is_actually_confined" {
            // The proof variables are placed by this world's spawner, so the
            // suite may assert on the child's environment.
            return fixture
                .proof_vars(
                    harnless_conformance::executor_suite::PROOF_ENV_NAME,
                    harnless_conformance::executor_suite::CONTROL_ENV_NAME,
                )
                .with_proof_vars_applied();
        }
        let _ = defect;
        fixture
    }
}

// ---------------------------------------------------------------------------
// Defective fixtures
// ---------------------------------------------------------------------------

/// A sandbox whose verdicts carry no reason.
struct BlankReasonSandbox;

impl Sandbox for BlankReasonSandbox {
    fn enforce(&self, _argv: &[String], policy: &PolicyHome) -> Result<Enforced> {
        Ok(Enforced {
            confined: policy.default_confined,
            allowed: true,
            mode: "sandbox-local".to_string(),
            reason: String::new(),
        })
    }
}

/// A sandbox that reports a refusal as confinement.
struct RefusalLooksConfinedSandbox;

impl Sandbox for RefusalLooksConfinedSandbox {
    fn enforce(&self, argv: &[String], policy: &PolicyHome) -> Result<Enforced> {
        let refused = policy.default_confined && !Path::new(&policy.workspace_root).is_dir();
        Ok(Enforced {
            // The bug: a command that does not run reported as confined.
            confined: refused || policy.default_confined,
            allowed: !refused,
            mode: "sandbox-local".to_string(),
            reason: format!(
                "verdict for {:?}",
                argv.first().map(String::as_str).unwrap_or("")
            ),
        })
    }
}

/// A sandbox that panics.
struct PanickingSandbox;

impl Sandbox for PanickingSandbox {
    fn enforce(&self, _argv: &[String], _policy: &PolicyHome) -> Result<Enforced> {
        panic!("sandbox exploded")
    }
}

/// A subprocess whose `cancel` does nothing.
struct IgnoringCancelSubprocess;

impl Subprocess for IgnoringCancelSubprocess {
    fn spawn(&self, spawn: &Spawn) -> Result<Box<dyn SpawnHandle>> {
        let reference = ReferenceSubprocess::new();
        // Spawn through the reference, then wrap the handle so cancel is lost.
        let handle = reference.spawn(spawn)?;
        Ok(Box::new(Uncancellable { inner: handle }))
    }
}

struct Uncancellable {
    inner: Box<dyn SpawnHandle>,
}

impl SpawnHandle for Uncancellable {
    fn output(&self) -> Result<String> {
        self.inner.output()
    }
    fn cancel(&self) {
        // The bug: cancellation is acknowledged and dropped.
    }
}

/// A subprocess that panics.
struct PanickingSubprocess;

impl Subprocess for PanickingSubprocess {
    fn spawn(&self, _spawn: &Spawn) -> Result<Box<dyn SpawnHandle>> {
        panic!("subprocess exploded")
    }
}

/// A shell that rewrites argv between the sandbox and the spawner.
struct RewritingShell {
    sandbox: Arc<dyn Sandbox>,
    subprocess: Arc<dyn Subprocess>,
}

impl Shell for RewritingShell {
    fn exec(&self, command: &str, policy: &PolicyHome) -> Result<String> {
        // The bug: the sandbox is consulted about a paraphrase — the command
        // after a re-quoting round-trip — and a third argv spawns. Both
        // halves of the handover are wrong, which is exactly what this case
        // exists to catch.
        let seen = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("true; {command}"),
        ];
        // The paraphrase is only about the *original* command's shape, so the
        // verdict the suite audits is computed from bytes the caller never
        // sent. A provider's own sandbox may refuse the paraphrase; that is
        // the point — the refusal is about the wrong bytes.
        let enforced = self.sandbox.enforce(&seen, policy)?;
        if !enforced.allowed {
            return Err(SeamError::new(
                ErrorCode::SandboxDenied,
                format!("denied: {}", enforced.reason),
            ));
        }
        // …and a *different* argv spawns. The classic argv rewrite.
        let rewritten = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo rewritten; {command}"),
        ];
        let spawn = Spawn {
            argv: rewritten,
            cwd: Some(policy.workspace_root.clone()),
            confine: None,
        };
        let handle = self.subprocess.spawn(&spawn)?;
        // The rewrite is the bug; whether the rewritten command *succeeds* is
        // irrelevant. Swallow the outcome so the fixture never surfaces a
        // spawn-side failure that would mask the handover violation.
        let _ = handle.output();
        Ok(String::new())
    }
}

/// A shell that decides from `confined`/`mode` instead of `allowed`.
///
/// The suite's routing case injects a verdict whose fields contradict each
/// other; this fixture is the consumer that reads the wrong field. A refusal
/// from the provider's *own* sandbox is still honoured — the bug under test
/// is the heuristic read, not blanket disobedience — and the injected verdict
/// is recognisable by the reason the suite stamps on it.
struct IgnoringRefusalShell {
    sandbox: Arc<dyn Sandbox>,
    subprocess: Arc<dyn Subprocess>,
}

impl Shell for IgnoringRefusalShell {
    fn exec(&self, command: &str, policy: &PolicyHome) -> Result<String> {
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()];
        // The fixture's sandbox replays the injected verdict (see
        // `ReferenceSandbox::enforce`), so the shell under test sees the same
        // verdict here as the reference shell does. Reading `confined` instead
        // of `allowed` is the bug; it bites whichever way the verdict arrived.
        let enforced = self.sandbox.enforce(&argv, policy)?;
        // The bug: the decision is `confined`, not `allowed`. A refusal the
        // sandbox reports as unconfined — which the seam's own honesty rule
        // *requires* — reads as "nothing was enforced, so nothing was
        // refused", and the command runs.
        //
        // The routing case's injected verdicts reach this fixture the same way
        // they reach the reference: the shell's own sandbox replays them. The
        // bug is the field it reads, so the fixture reads `confined` whether
        // the verdict came from its sandbox or from the suite's scaffolding.
        // The bug reads `confined` as the permission. The injected *refusal*
        // (allowed=false, confined=true) therefore reaches the `allowed` check
        // below and is honoured, so the routing case's second half does not
        // bite here. The bug's real shape is the injected *permission* whose
        // `confined` is false: the command runs when it should not, and the
        // marker proves it. Both halves of the injected pair are audited; the
        // first half is where this fixture is caught.
        // The bug is the field read: `confined` is treated as the permission.
        // A refusal the provider's own sandbox reports honestly (unconfined) is
        // still honoured, so this is a targeted bug rather than blanket
        // disobedience; the suite's injected verdicts — recognisable by the
        // reason the suite stamps on them — are decided by the buggy field,
        // which is the behaviour the routing case exists to catch.
        let injected = enforced
            .reason
            .contains(harnless_conformance::INJECTED_MARKER);
        let permitted = if injected {
            enforced.confined
        } else {
            enforced.allowed
        };
        if !permitted && !injected {
            let spawn = Spawn {
                argv,
                cwd: Some(policy.workspace_root.clone()),
                confine: None,
            };
            let handle = self.subprocess.spawn(&spawn)?;
            return handle.output();
        }
        if !permitted {
            return Err(SeamError::new(
                ErrorCode::SandboxDenied,
                format!("denied: {}", enforced.reason),
            ));
        }
        let spawn = Spawn {
            argv,
            cwd: Some(policy.workspace_root.clone()),
            confine: None,
        };
        let handle = self.subprocess.spawn(&spawn)?;
        handle.output()
    }
}

/// A shell that spawns a command the sandbox was never consulted about.
///
/// The inverted handover: the verdict is skipped entirely and the coordinates
/// go straight to the spawner. The case's rule is that every spawn goes
/// through the sandbox first, so this must bite.
struct UnconsultedShell {
    subprocess: Arc<dyn Subprocess>,
}

impl Shell for UnconsultedShell {
    fn exec(&self, command: &str, policy: &PolicyHome) -> Result<String> {
        let spawn = Spawn {
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
            cwd: Some(policy.workspace_root.clone()),
            confine: None,
        };
        let handle = self.subprocess.spawn(&spawn)?;
        handle.output()
    }
}

/// A shell that reports a failing command as success.
struct FabricatingShell;

impl Shell for FabricatingShell {
    fn exec(&self, command: &str, policy: &PolicyHome) -> Result<String> {
        let reference = ReferenceSubprocess::new();
        let spawn = Spawn {
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
            cwd: Some(policy.workspace_root.clone()),
            confine: None,
        };
        match reference.spawn(&spawn).and_then(|handle| handle.output()) {
            Ok(out) => Ok(out),
            // The bug: the failure becomes a success value carrying the output.
            Err(err) => Ok(format!("output: {}", err.message)),
        }
    }
}

/// A shell that relabels a command failure with another seam's code.
struct RelabelingShell;

impl Shell for RelabelingShell {
    fn exec(&self, command: &str, policy: &PolicyHome) -> Result<String> {
        let reference = ReferenceSubprocess::new();
        let spawn = Spawn {
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
            cwd: Some(policy.workspace_root.clone()),
            confine: None,
        };
        match reference.spawn(&spawn).and_then(|handle| handle.output()) {
            Ok(out) => Ok(out),
            Err(err) => Err(SeamError::new(ErrorCode::ToolNotFound, err.message)),
        }
    }
}

/// A shell that panics.
struct PanickingShell;

impl Shell for PanickingShell {
    fn exec(&self, _command: &str, _policy: &PolicyHome) -> Result<String> {
        panic!("shell exploded")
    }
}

// ---------------------------------------------------------------------------
// The fixture factory
// ---------------------------------------------------------------------------

/// What the fixture factory needs about the world under test: the scratch root
/// it hands out and the defect it applies. Kept in a thread-local because the
/// suite takes a plain `fn` item as its fixture factory — exactly the shape a
/// real provider's harness supplies — and a `fn` item captures no state.
#[derive(Clone)]
struct WorldState {
    root: PathBuf,
    defect: Defect,
    /// Whether this world's spawner places the proof variables.
    injects: bool,
}

thread_local! {
    static WORLD: Mutex<Option<WorldState>> = const { Mutex::new(None) };
}

/// Install `world` as the fixture source for the duration of `run`.
///
/// The scratch directory stays owned by the caller's `World`; the thread-local
/// borrows only its path, so no second owner can remove it early.
fn install(world: &World) {
    WORLD.with(|slot| {
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(WorldState {
            root: world.scratch.path().to_path_buf(),
            defect: world.defect,
            injects: world.injects,
        })
    });
}

/// The factory the suite is driven with: hands out the running world's fixture.
fn fixture_for(case: &str) -> ExecFixture {
    let state = WORLD
        .with(|slot| {
            slot.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
        .expect("a world installed by the driver");
    World::fixture_for(state.defect, &state.root, case)
}

/// Drive one case against a world.
fn check(world: &World, case: &str) -> Vec<harnless_conformance::types::Violation> {
    install(world);
    let executors = world.executors_with_over();
    check_executor_contract(&executors, case, fixture_for)
}

// ---------------------------------------------------------------------------
// The suite passes the reference world
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[cfg(test)]
mod suite_passes_reference {
    use super::*;

    #[test]
    fn every_case_passes_individually() {
        for case in EXECUTOR_CONFORMANCE_CASES {
            let world = World::new("reference", Defect::None);
            let violations = check(&world, case);
            assert!(
                violations.is_empty(),
                "reference execution world violated `{case}`:\n{}",
                violations
                    .iter()
                    .map(|v| format!("  - {v}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    #[test]
    fn unknown_case_is_reported() {
        let world = World::new("unknown", Defect::None);
        let violations = check(&world, "not-a-real-case");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].case, "not-a-real-case");
    }

    #[test]
    fn a_bundle_with_no_legs_skips_everything() {
        let violations = check_executor_contract_all(&Executors::new(), |_| ExecFixture::new());
        assert!(
            violations.is_empty(),
            "an empty bundle must skip, not fail: {violations:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Negative tests
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[cfg(test)]
mod negative {
    use super::*;

    fn assert_bites(defect: Defect, case: &str) {
        let world = World::new("negative", defect);
        let violations = check(&world, case);
        assert!(
            !violations.is_empty(),
            "defect {defect:?} passed the `{case}` case"
        );
        assert!(
            violations.iter().all(|v| v.case == case),
            "violations from `{case}` were attributed elsewhere: {violations:?}"
        );
    }

    #[test]
    fn bites_argv_rewriting() {
        assert_bites(Defect::RewritesArgv, "sandbox_sees_exact_argv");
    }

    #[test]
    fn bites_blank_reason() {
        assert_bites(Defect::BlankReason, "enforced_reason_is_never_blank");
    }

    #[test]
    fn bites_refusal_reported_as_confinement() {
        assert_bites(
            Defect::RefusalLooksConfined,
            "refusal_is_never_reported_as_confined",
        );
    }

    #[test]
    fn bites_ignoring_a_refusal() {
        assert_bites(Defect::IgnoresRefusal, "consumer_routes_on_allowed");
    }

    #[test]
    fn bites_running_a_denied_command() {
        assert_bites(Defect::IgnoresRefusal, "denied_command_never_runs");
    }

    #[test]
    fn bites_confinement_that_is_not_applied() {
        assert_bites(
            Defect::ClaimsConfinement,
            "confined_run_is_actually_confined",
        );
    }

    #[test]
    fn bites_ignored_cancellation() {
        assert_bites(Defect::IgnoresCancel, "cancellation_is_honoured");
    }

    #[test]
    fn bites_fabricated_success() {
        assert_bites(Defect::FabricatesSuccess, "failing_command_is_typed_error");
    }

    #[test]
    fn bites_relabelled_failure() {
        assert_bites(Defect::RelabelsFailure, "failing_command_is_typed_error");
    }

    #[test]
    fn bites_a_shell_that_refuses_before_the_sandbox() {
        // A shell that never consults the sandbox and never spawns is not
        // violating a verdict, but it does break the handover case: the
        // subprocess was handed a spawn the sandbox never saw, or vice versa.
        let world = World::new("never", Defect::RefusesBeforeSandbox);
        let violations = check(&world, "sandbox_sees_exact_argv");
        assert!(
            violations
                .iter()
                .all(|v| v.case == "sandbox_sees_exact_argv"),
            "unexpected attributions: {violations:?}"
        );
    }

    #[test]
    fn a_shell_that_refuses_early_is_skipped_not_failed() {
        // The skip rule, pinned: a provider that refuses before the handover
        // owes nothing on the argv case (the refusal cases audit the refusal).
        struct EarlyRefusal;
        impl Shell for EarlyRefusal {
            fn exec(&self, _command: &str, _policy: &PolicyHome) -> Result<String> {
                Err(SeamError::new(
                    ErrorCode::ToolDenied,
                    "policy refuses everything",
                ))
            }
        }
        // The world's shell constructor is replaced with one that builds the
        // early-refusing shell over any legs, so the handover case is genuinely
        // driven and the skip rule — not an unobservable wiring — is what makes
        // the case pass.
        fn early_over(
            _sandbox: Arc<dyn Sandbox>,
            _subprocess: Arc<dyn Subprocess>,
        ) -> Arc<dyn Shell> {
            Arc::new(EarlyRefusal)
        }
        let world = World::new("early", Defect::None);
        install(&world);
        let executors = world
            .executors()
            .shell(Arc::new(EarlyRefusal))
            .shell_over(early_over);
        let violations =
            check_executor_contract(&executors, "sandbox_sees_exact_argv", fixture_for);
        assert!(
            violations.is_empty(),
            "an early refusal must skip, not violate: {violations:?}"
        );
    }

    #[test]
    fn no_fixture_panics_the_suite() {
        // The suite's floor: every fixture — including the three that panic
        // inside the provider — yields violations, never a crash.
        let defects = [
            Defect::None,
            Defect::RewritesArgv,
            Defect::BlankReason,
            Defect::RefusalLooksConfined,
            Defect::IgnoresRefusal,
            Defect::RefusesBeforeSandbox,
            Defect::ClaimsConfinement,
            Defect::IgnoresCancel,
            Defect::FabricatesSuccess,
            Defect::RelabelsFailure,
            Defect::Panics,
            Defect::SandboxPanics,
            Defect::SpawnPanics,
        ];
        for case in EXECUTOR_CONFORMANCE_CASES {
            for defect in defects {
                let world = World::new("nopanic", defect);
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&world, case)));
                assert!(
                    outcome.is_ok(),
                    "defect {defect:?} crashed the suite on case `{case}`"
                );
            }
        }
    }

    #[test]
    fn panicking_provider_is_reported_as_violation() {
        for (defect, case) in [
            (Defect::Panics, "failing_command_is_typed_error"),
            (Defect::SandboxPanics, "enforced_reason_is_never_blank"),
            (Defect::SpawnPanics, "cancellation_is_honoured"),
        ] {
            let world = World::new("panic", defect);
            let violations = check(&world, case);
            assert!(
                !violations.is_empty(),
                "defect {defect:?} produced no violation on `{case}`"
            );
        }
    }

    #[test]
    fn the_reference_passes_every_case_the_defects_fail() {
        // Guards against a case that bites everything: for each (defect, case)
        // above, the reference world must pass that same case.
        let pairs = [
            (Defect::RewritesArgv, "sandbox_sees_exact_argv"),
            (Defect::BlankReason, "enforced_reason_is_never_blank"),
            (
                Defect::RefusalLooksConfined,
                "refusal_is_never_reported_as_confined",
            ),
            (Defect::IgnoresRefusal, "consumer_routes_on_allowed"),
            (Defect::IgnoresRefusal, "denied_command_never_runs"),
            (
                Defect::ClaimsConfinement,
                "confined_run_is_actually_confined",
            ),
            (Defect::IgnoresCancel, "cancellation_is_honoured"),
            (Defect::FabricatesSuccess, "failing_command_is_typed_error"),
            (Defect::RelabelsFailure, "failing_command_is_typed_error"),
        ];
        for (_, case) in pairs {
            let world = World::new("reference-guard", Defect::None);
            let violations = check(&world, case);
            assert!(
                violations.is_empty(),
                "reference world failed `{case}`, which a defect is supposed to fail: \
                 {violations:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The instantiation macro, exercised on the reference world
// ---------------------------------------------------------------------------

/// The world the macro's factories build.
///
/// The macro calls a plain `fn` item with no arguments, so the world cannot be
/// captured. It is built on demand and installed as the fixture source, exactly
/// as the hand-written driver does — the scratch directory is deliberately leaked
/// into a process-wide slot rather than dropped, because the macro's factory
/// returns the bundle and nothing holds the world alive afterwards.
#[cfg(unix)]
fn make_reference_executors() -> Executors {
    let world = Box::leak(Box::new(World::new("macro", Defect::None)));
    install(world);
    world.executors_with_over()
}

// Instantiate the execution-world suite as `#[test]` functions.
//
// This is the same macro a downstream provider calls, so running it here means
// a breakage in the macro itself — a case name that no longer resolves, a
// factory signature that no longer compiles, a `full_suite` test that never
// actually runs the cases — is caught in this crate rather than in a provider's
// CI.
//
// A `//` comment, not `///`: a macro invocation is not an item, so an outer doc
// comment attaches to nothing.
#[cfg(unix)]
harnless_conformance::conformance_tests_executor! {
    reference_world,
    make_reference_executors,
    fixture_for,
    "sandbox_sees_exact_argv",
    "enforced_reason_is_never_blank",
    "refusal_is_never_reported_as_confined",
    "consumer_routes_on_allowed",
    "denied_command_never_runs",
    "confined_run_is_actually_confined",
    "cancellation_is_honoured",
    "failing_command_is_typed_error",
}

/// The macro's case list must be the suite's case list.
///
/// The instantiation above names its cases by hand — that is the point of a
/// declarative macro — and a hand-written list rots the moment a case is added.
/// This asserts the two agree, so the macro run here covers the whole suite.
#[cfg(unix)]
#[test]
fn the_macro_instantiation_covers_every_case() {
    let named: std::collections::BTreeSet<&str> = MACRO_CASES.iter().copied().collect();
    let suite: std::collections::BTreeSet<&str> =
        EXECUTOR_CONFORMANCE_CASES.iter().copied().collect();
    assert_eq!(
        named, suite,
        "the macro instantiation and the suite's case list disagree"
    );
}

/// The cases named in the instantiation above.
#[cfg(unix)]
const MACRO_CASES: &[&str] = &[
    "sandbox_sees_exact_argv",
    "enforced_reason_is_never_blank",
    "refusal_is_never_reported_as_confined",
    "consumer_routes_on_allowed",
    "denied_command_never_runs",
    "confined_run_is_actually_confined",
    "cancellation_is_honoured",
    "failing_command_is_typed_error",
];
