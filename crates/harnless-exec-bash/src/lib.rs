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
//!
//! A `sandbox-local` verdict means the command must run with its environment
//! scrubbed and its cwd pinned. The env scrub is applied **parent-side**:
//! [`ConfinedSpawner`] builds the child's environment explicitly
//! (`Command::env_clear` + the surviving variables) before the fork, so the
//! child never calls a non-async-signal-safe environment mutator
//! (`setenv`/`unsetenv` take an environment lock on macOS and are **not**
//! async-signal-safe there). The cwd pin and rlimit steps run in the child
//! via `chdir(2)`/`setrlimit(2)` — both true async-signal-safe syscalls —
//! inside a `pre_exec` hook. [`ConfinedSpawner`] is the boundary that makes
//! this work under a multi-threaded tokio runtime: confined spawns are
//! funnelled onto a dedicated single-threaded spawner thread, so the fork
//! happens where `pre_exec` is sound no matter how many worker threads the
//! caller runs. Unconfined spawns never touch that thread — they go straight
//! to the plain subprocess provider, unchanged in cost and shape.

use std::sync::{mpsc, Arc};

use harnless_seams::error::{ErrorCode, Result, SeamError};
use harnless_seams::exec::{
    ConfineHint, Enforced, PolicyHome, Sandbox, Shell, Spawn, SpawnHandle, Subprocess,
};

use harnless_exec_sandbox::LocalSandbox;
use harnless_exec_subprocess::SubprocessLocal;

/// The program the shell seam invokes.
pub const SHELL_PROGRAM: &str = "/bin/bash";

/// The confinement a confined verdict asks the spawner to apply.
///
/// A [`Spawn`] carries one as a request-scoped hint when the sandbox
/// returned `confined == true`: the *same* [`PolicyHome`] the verdict was
/// computed from, so a spawner can only ever confine to the root the
/// verdict — and the fs provider — read. A plain [`Subprocess`] that
/// ignores the field spawns exactly the coordinates it always did; a
/// [`ConfinedSpawner`] applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confinement {
    /// The policy home the verdict was decided under (workspace root +
    /// default mode); [`LocalSandbox::prepare`] derives confinement from it.
    pub policy: PolicyHome,
    /// Optional `RLIMIT_AS` bytes to apply in the child where supported
    /// (macOS cannot set `RLIMIT_AS`; there the limit is honestly
    /// not-applied, exactly as the verdict's guarantees text says).
    pub rlimit_as_bytes: Option<u64>,
    /// Extra variables injected into the child's environment *before* the
    /// scrub runs, so they are subject to it. Test/diagnostics-facing: it
    /// lets a test prove a variable was present in the child at spawn time
    /// and then observe the scrub remove it, without mutating the test
    /// process's own environment.
    ///
    /// Entries with an empty name or an embedded NUL are ignored.
    pub extra_env: Vec<(String, String)>,
}

/// The shell seam over the subprocess seam.
pub struct BashLocal {
    /// The subprocess provider the coordinates are handed to. Wire a
    /// [`ConfinedSpawner`] here (the shape [`BashLocal::local`] uses) so a
    /// confined verdict has somewhere to *apply* its confinement.
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
        Self {
            subprocess,
            sandbox,
        }
    }

    /// The default wiring: subprocess-local execution with the local
    /// sandbox consulted on every command. The subprocess slot is a
    /// [`ConfinedSpawner`] over [`SubprocessLocal`], so confined verdicts
    /// actually confine (see its docs) and unconfined verdicts run on the
    /// plain path exactly as before.
    pub fn local() -> Self {
        Self::new(
            Box::new(ConfinedSpawner::new()),
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
        let mut spawn = Spawn {
            argv,
            cwd: Some(policy.workspace_root.clone()),
            confine: None,
        };
        // The verdict travels with the coordinates as a request-scoped
        // hint (see [`Confinement`], boxed as the seam's opaque
        // [`ConfineHint`]): a spawner that can apply confinement routes
        // confined runs through the single-threaded boundary; a plain
        // [`Subprocess`] sees a `Spawn` exactly as before.
        if enforced.confined {
            spawn.confine = Some(ConfineHint::new(Confinement {
                policy: policy.clone(),
                rlimit_as_bytes: None,
                extra_env: Vec::new(),
            }));
        }
        let handle = self.subprocess.spawn(&spawn)?;
        <dyn SpawnHandle>::output(&*handle)
    }
}

/// A request to the spawner thread: spawn these confined coordinates and
/// report the handle (or refusal) back over the reply channel.
type Job = (
    Spawn,
    Confinement,
    mpsc::Sender<Result<Box<dyn SpawnHandle>>>,
);

/// The confined-spawn boundary: a dedicated single-threaded spawner thread.
///
/// # The decision
///
/// Confinement has two halves. The **env scrub** is applied parent-side,
/// before the fork: [`LocalSandbox::prepare_scrubbed_env`] snapshots the
/// environment, filters credential-shaped names, and the child's
/// environment is rebuilt from it via `env_clear` + `envs`. This matters
/// because `setenv`/`unsetenv` are **not** async-signal-safe on macOS
/// (Apple's libsystem takes an environment lock; a signal landing inside
/// it aborts), so a child-side scrub is unsound under any multi-threaded
/// forker. The **cwd pin + rlimit** half runs in the child inside a
/// `pre_exec` hook ([`LocalSandbox::prepare`], `chdir(2)`/`setrlimit(2)` —
/// true async-signal-safe syscalls), and the hook is only sound while the
/// forking process is single-threaded at the fork point. Tokio's
/// multi-threaded runtime violates that on every worker thread. The
/// shipped boundary: confined commands are funnelled over a channel onto
/// one dedicated thread created per [`ConfinedSpawner`] — the only thread
/// that forks confined children — and the fork happens there, so the
/// constraint holds by construction no matter how many threads the
/// caller's runtime has.
///
/// Rejected alternatives:
///
/// - **argv rewriting** (`env -i` / `cd` prefixes): spoofable — the command
///   itself can re-read credentials from `~/.aws` or the keychain and `cd`
///   anywhere, so the verdict would overstate enforcement; it also mutates
///   the exact argv the audit story promises is byte-identical to what the
///   sandbox saw.
/// - **`posix_spawn` / fork-early**: the child is immediately quiescent,
///   which is safe, but there is no pre-exec hook to run `prepare`'s rlimit
///   step (macOS `posix_spawnattr` has no rlimit attribute), so the
///   "rlimit where supported" half of the promised guarantees would
///   silently stop being applied.
/// - **`tokio::task::spawn_blocking`**: moves the fork off the worker but
///   not off multithreadedness — the blocking pool has many threads, so
///   fork-time single-threadedness is still violated.
///
/// The unconfined path is untouched: a verdict with `confined == false`
/// delegates straight to the inner [`Subprocess`] (normally
/// [`SubprocessLocal`]) on the caller's thread — no channel, no thread, no
/// new `pre_exec` beyond the process-group `setsid` that was always there.
///
/// # Failure honesty
///
/// If `prepare` fails in the child (e.g. the workspace root vanished after
/// the verdict), the child self-terminates in `pre_exec` via
/// [`harnless_exec_subprocess::kill_process`] — the hook cannot return an
/// error across the fork boundary — and the parent's
/// [`SpawnHandle::output`] reports the signal exit as `SpawnFailed` with the
/// captured output. A confined command never runs unconfined: either
/// confinement holds or the command does not run.
pub struct ConfinedSpawner {
    inner: Arc<dyn Subprocess>,
    /// Mailbox to the spawner thread. Dropping the last clone closes the
    /// channel and the thread exits; `Drop` joins it.
    queue: mpsc::Sender<Job>,
    /// The spawner thread, taken and joined on drop.
    thread: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Env attached to every confined spawn from this spawner (see
    /// [`Confinement::extra_env`]).
    extra: parking_lot::Mutex<Vec<(String, String)>>,
}

impl std::fmt::Debug for ConfinedSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfinedSpawner").finish_non_exhaustive()
    }
}

impl Default for ConfinedSpawner {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfinedSpawner {
    /// Confined spawns on the dedicated thread; unconfined spawns on a
    /// fresh [`SubprocessLocal`] (the plain path).
    pub fn new() -> Self {
        Self::with_subprocess(Box::new(SubprocessLocal::new()))
    }

    /// Confined spawns on the dedicated thread; unconfined spawns delegate
    /// to `inner` (injectable for tests and for composing capture limits).
    pub fn with_subprocess(inner: Box<dyn Subprocess>) -> Self {
        let (queue, receive) = mpsc::channel::<Job>();
        // The one thread of this process that ever forks for confined
        // work. It lives while jobs keep coming and exits when the last
        // spawner handle drops (see `Drop`, which also joins it).
        let thread = std::thread::Builder::new()
            .name("harnless-confined-spawner".to_string())
            .spawn(move || spawner_loop(receive))
            .expect("spawn confined spawner thread");
        Self {
            inner: Arc::from(inner),
            queue,
            thread: parking_lot::Mutex::new(Some(thread)),
            extra: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Add an environment variable that confined children inherit *before*
    /// the scrub (see [`Confinement::extra_env`]). Builder-style;
    /// test/diagnostics-facing.
    #[must_use]
    pub fn with_extra_env(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra.lock().push((name.into(), value.into()));
        self
    }
}

impl Drop for ConfinedSpawner {
    fn drop(&mut self) {
        // Release the queue so the spawner thread sees the channel close
        // and exits, then join it. Without the join, a leaked or cyclic
        // spawner handle keeps a Sender alive and the parked thread blocks
        // process teardown forever.
        // Swap in a sender to a channel nobody receives on: the previous
        // clone drops, the thread sees the channel close, and it exits.
        let (dead, _guard) = mpsc::channel();
        let _ = std::mem::replace(&mut self.queue, dead);
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
    }
}

impl Subprocess for ConfinedSpawner {
    fn spawn(&self, spawn: &Spawn) -> Result<Box<dyn SpawnHandle>> {
        let confine = match spawn.confine.as_ref() {
            // The plain path: no confined hint at all — same provider,
            // same thread, same everything as before this boundary existed.
            None => return self.inner.spawn(spawn),
            Some(hint) => match hint.downcast_ref::<Confinement>() {
                Some(confine) => confine,
                // A confined verdict whose hint we cannot read must never
                // degrade to the plain path: fail closed, not unconfined.
                None => {
                    return Err(SeamError::new(
                        ErrorCode::SandboxDenied,
                        "confined spawn carries an unreadable confinement hint; \
                         refusing rather than running a confined verdict unconfined",
                    ))
                }
            },
        };
        // Spawner-level extra env is injected before (and therefore subject
        // to) the scrub, then per-request extras win.
        let mut extra_env = self.extra.lock().clone();
        extra_env.extend(confine.extra_env.iter().cloned());
        let confine = Confinement {
            extra_env,
            ..confine.clone()
        };
        let (reply, receipt) = mpsc::channel();
        // A closed channel means the spawner thread already exited (all
        // spawner handles dropped); the reply channel then disconnects and
        // the caller sees `SpawnFailed`.
        self.queue
            .send((spawn.clone(), confine, reply))
            .map_err(|_| SeamError::new(ErrorCode::SpawnFailed, "confined spawner exited"))?;
        receipt
            .recv()
            .map_err(|_| SeamError::new(ErrorCode::SpawnFailed, "confined spawner exited"))?
    }
}

/// The spawner thread body: fork one confined command at a time, alone.
fn spawner_loop(receive: mpsc::Receiver<Job>) {
    while let Ok((spawn, confinement, reply)) = receive.recv() {
        let result = confined_spawn(&spawn, &confinement);
        // A dropped reply channel means the caller gave up; nothing to
        // report to.
        let _ = reply.send(result);
    }
}

/// Fork+exec `spawn` with confinement applied.
///
/// Runs only on the spawner thread — the sole fork site for confined work.
/// The env scrub happens **here, in the parent**, before the fork: the
/// child's environment is rebuilt from the scrubbed snapshot via
/// `env_clear` + `envs`, so the child never touches `setenv`/`unsetenv`
/// (environment-locked, not async-signal-safe on macOS). The child's hook
/// runs only `setsid`/`chdir`/`setrlimit`/`kill` — all async-signal-safe.
#[cfg(unix)]
fn confined_spawn(spawn: &Spawn, confinement: &Confinement) -> Result<Box<dyn SpawnHandle>> {
    use std::os::unix::process::CommandExt;
    let (program, args) = spawn
        .argv
        .split_first()
        .ok_or_else(|| SeamError::new(ErrorCode::SpawnFailed, "spawn argv is empty"))?;
    let mut command = std::process::Command::new(program);
    command.args(args);
    if let Some(cwd) = &spawn.cwd {
        command.current_dir(cwd);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // The env scrub, applied parent-side via the sandbox crate's own
    // predicate — the same one the verdict's guarantees text promises.
    // `std::env::vars()` snapshots the environment into owned strings, so
    // the collect-then-filter pass has none of the live-array hazards a
    // child-side scrub would; extras are merged before the filter, so a
    // credential-shaped extra is scrubbed exactly like an inherited
    // variable (proof-by-injection stays sound).
    let scrubbed = LocalSandbox::new().prepare_scrubbed_env(confinement.extra_env.iter().cloned());
    command.env_clear();
    command.envs(scrubbed);
    // Same process-group policy as the plain provider, so cancel() reaches
    // the child's descendants too. The hook runs only async-signal-safe
    // calls: `setsid`, then the cwd pin and rlimit (true syscalls), and on
    // failure the child kills itself — `pre_exec` cannot report an error
    // across the fork boundary, and a confined command must never exec
    // without its confinement.
    let sandbox = LocalSandbox::new();
    let policy = confinement.policy.clone();
    let rlimit = confinement.rlimit_as_bytes;
    unsafe {
        command.pre_exec(move || {
            // A failed setsid must not kill the spawn (same policy as the
            // plain subprocess provider): the group falls back to the
            // parent's and cancel degrades to a direct-child kill.
            let _ = setsid();
            if sandbox.prepare(&policy, rlimit).is_err() {
                harnless_exec_subprocess::kill_process(
                    std::process::id(),
                    harnless_exec_subprocess::SIGKILL,
                );
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|err| SeamError::new(ErrorCode::SpawnFailed, err.to_string()))?;
    Ok(ConfinedHandle::new(child))
}

/// Non-Unix: confinement application is Unix-only (`prepare` needs fork +
/// rlimit semantics). Refuse rather than run a confined verdict unconfined.
#[cfg(not(unix))]
fn confined_spawn(_spawn: &Spawn, _confinement: &Confinement) -> Result<Box<dyn SpawnHandle>> {
    Err(SeamError::new(
        ErrorCode::SandboxDenied,
        "sandbox-local confinement cannot be applied on this platform; \
         refusing rather than running a confined verdict unconfined",
    ))
}

// `setsid(2)` without a libc crate dependency (same trick the plain
// subprocess provider uses).
#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

/// The confined child's handle: the same observable behaviour as the plain
/// provider's handle — joined capture, `SpawnFailed` on nonzero exit,
/// `ExecCancelled` after cancel, best-effort process-group kill.
struct ConfinedHandle {
    child: Arc<parking_lot::Mutex<std::process::Child>>,
    /// The child's pid, cached at spawn so `cancel` can signal the process group
    /// without taking the child lock (which `output` holds across `wait`).
    child_pid: i32,
    stdout: Reader,
    stderr: Reader,
    cancelled: std::sync::atomic::AtomicBool,
}

impl ConfinedHandle {
    fn new(mut child: std::process::Child) -> Box<dyn SpawnHandle> {
        let stdout = Reader::start(child.stdout.take());
        let stderr = Reader::start(child.stderr.take());
        let child_pid = child.id() as i32;
        Box::new(Self {
            child: Arc::new(parking_lot::Mutex::new(child)),
            child_pid,
            stdout,
            stderr,
            cancelled: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

impl SpawnHandle for ConfinedHandle {
    fn output(&self) -> Result<String> {
        use std::sync::atomic::Ordering;
        let status = {
            let mut child = self.child.lock();
            let status: std::io::Result<std::process::ExitStatus> = child.wait();
            status.map_err(|err| SeamError::new(ErrorCode::SpawnFailed, err.to_string()))?
        };
        let mut out = self.stdout.collect();
        out.push_str(&self.stderr.collect());
        if status.success() {
            return Ok(out);
        }
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(SeamError::new(
                ErrorCode::ExecCancelled,
                format!("process cancelled ({}); {}", exit_label(&status), out),
            ));
        }
        // A non-zero/signal exit is a seam failure the command tool
        // surfaces, with the captured output attached — this is also how a
        // child that self-terminated in `pre_exec` (confinement could not
        // be applied) surfaces: `SpawnFailed`, never a silent pass.
        Err(SeamError::new(
            ErrorCode::SpawnFailed,
            format!("process exited with {}; {}", exit_label(&status), out),
        ))
    }

    fn cancel(&self) {
        use std::sync::atomic::Ordering;
        self.cancelled.store(true, Ordering::SeqCst);
        // Prefer the whole process group (the child leads its own via
        // setsid); fall back to a direct kill.
        #[cfg(unix)]
        {
            // The pid is read from the spawn-time cache, never from the locked
            // child. `output` holds the child across `wait`, and a caller
            // cancelling a run it is blocked waiting on — the shape of any "stop
            // this command" caller — would otherwise deadlock against that wait
            // (or, with a try-lock that gives up, have its cancellation dropped
            // and be handed the completed run as success). Signalling a
            // since-exited group is harmless: the signal simply does not land,
            // and `output` reports the real exit.
            let pid = self.child_pid;
            if unsafe { kill(-pid, harnless_exec_subprocess::SIGTERM) } != 0 {
                // The group is gone or was never led by the child; the direct
                // kill needs the child, so it is best-effort under a try-lock.
                if let Some(mut child) = self.child.try_lock() {
                    let _ = child.kill();
                }
            }
        }
        #[cfg(not(unix))]
        if let Some(mut child) = self.child.try_lock() {
            let _ = child.kill();
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn exit_label(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "signal".to_string(),
    }
}

/// A background stream reader over the confined child's pipes.
struct Reader {
    join: parking_lot::Mutex<Option<std::thread::JoinHandle<Vec<u8>>>>,
}

impl Reader {
    fn start(pipe: Option<impl std::io::Read + Send + 'static>) -> Self {
        let join = pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                buf
            })
        });
        Self {
            join: parking_lot::Mutex::new(join),
        }
    }

    fn collect(&self) -> String {
        let join = self.join.lock().take();
        match join {
            Some(join) => String::from_utf8_lossy(&join.join().unwrap_or_default()).into_owned(),
            None => String::new(),
        }
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

#[cfg(test)]
mod confinement_unit_tests {
    use super::*;

    /// The env scrub is a pure parent-side filter: extras merged before the
    /// needle check are scrubbed, safe names survive, and the sandbox
    /// crate's own predicate is the single source of truth.
    #[test]
    fn prepare_scrubbed_env_filters_extras_by_needle() {
        let scrubbed = LocalSandbox::new().prepare_scrubbed_env([
            (
                "HARNLESS_TEST_FAKE_TOKEN".to_string(),
                "hunter2".to_string(),
            ),
            ("HARNLESS_TEST_SAFE_VAR".to_string(), "keepme".to_string()),
        ]);
        assert!(
            !scrubbed
                .iter()
                .any(|(n, _)| n == "HARNLESS_TEST_FAKE_TOKEN"),
            "credential-shaped extra survived the scrub"
        );
        assert!(
            scrubbed
                .iter()
                .any(|(n, v)| n == "HARNLESS_TEST_SAFE_VAR" && v == "keepme"),
            "safe extra was dropped"
        );
    }
}
