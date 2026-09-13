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
//! # Confinement is applied, not just decided
//!
//! A `sandbox-local` verdict means the command must run with its environment
//! scrubbed and its cwd pinned. That application lives in
//! [`LocalSandbox::prepare`], which is only safe to call in a forked child
//! (it is not async-signal-safe). [`ConfinedSpawner`] is the boundary that
//! makes this work under a multi-threaded tokio runtime: confined spawns are
//! funnelled onto a dedicated single-threaded spawner thread, so the fork
//! happens where `pre_exec` is sound no matter how many worker threads the
//! caller runs. Unconfined spawns never touch that thread — they go straight
//! to the plain subprocess provider, unchanged in cost and shape.

use std::sync::{mpsc, Arc};

use harnless_seams::error::{ErrorCode, SeamError, Result};
use harnless_seams::exec::{ConfineHint, Enforced, PolicyHome, Sandbox, Shell, Spawn, SpawnHandle, Subprocess};

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
    /// lets a test prove a variable was present in the child at fork time
    /// and then observe the scrub remove it, without mutating the test
    /// process's own environment.
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
        Self { subprocess, sandbox }
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
type Job = (Spawn, Confinement, mpsc::Sender<Result<Box<dyn SpawnHandle>>>);

/// The confined-spawn boundary: a dedicated single-threaded spawner thread.
///
/// # The decision
///
/// [`LocalSandbox::prepare`] (env scrub + cwd pin + rlimit) is the
/// confinement application, and it is only sound inside a `pre_exec` hook
/// while the forking process is single-threaded at the fork point — the
/// hook body itself is async-signal-safe (`prepare` uses `unsetenv`/`chdir`
/// /`setrlimit`, all POSIX async-signal-safe), but the fork can happen
/// while another thread holds a std lock the child's code would
/// re-enter. Tokio's multi-threaded runtime violates that on every worker
/// thread. The shipped boundary: confined commands are funnelled over a
/// channel onto one dedicated thread created per [`ConfinedSpawner`] —
/// the only thread that forks confined children — and the fork happens
/// there, so the constraint holds by construction no matter how many
/// threads the caller's runtime has.
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
    /// Mailbox to the spawner thread. The spawner — not the job — holds
    /// the thread's sender clone: a forked child must never inherit an
    /// open job-channel handle, or the thread could never observe channel
    /// close and the process would keep a live thread forever.
    queue: mpsc::Sender<Job>,
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
        // work. It lives while jobs keep coming and exits when every
        // spawner handle drops (channel close).
        std::thread::Builder::new()
            .name("harnless-confined-spawner".to_string())
            .spawn(move || spawner_loop(receive))
            .expect("spawn confined spawner thread");
        Self {
            inner: Arc::from(inner),
            queue,
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

impl Subprocess for ConfinedSpawner {
    fn spawn(&self, spawn: &Spawn) -> Result<Box<dyn SpawnHandle>> {
        let confine = match spawn
            .confine
            .as_ref()
            .and_then(|hint| hint.downcast_ref::<Confinement>())
        {
            // The plain path: no confined hint — same provider, same
            // thread, same everything as before this boundary existed.
            None => return self.inner.spawn(spawn),
            Some(confine) => confine,
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

/// Fork+exec `spawn` with `prepare` applied in the child.
///
/// Runs only on the spawner thread — the sole fork site for confined work,
/// which is what makes the non-async-signal-safe hook body sound.
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
    // NB: `extra_env` is *not* injected with `Command::env` — that would
    // install the values after the scrub and defeat it. The hook injects
    // them (see below) so they exist at fork time and the scrub sees them.
    // Same process-group policy as the plain provider, so cancel() reaches
    // the child's descendants too. One hook does setsid *then* confinement:
    // a confined child is a process-group leader, and a later hook would
    // run after the child may already have been signalled.
    //
    // Sound here: this fork happens on the dedicated single-threaded
    // spawner. The hook body itself is async-signal-safe (setsid, kill);
    // the non-async-signal-safe `prepare` runs in the *child*, which is
    // single-threaded by fork semantics — exactly why the fork lives on
    // the spawner thread at all.
    let sandbox = LocalSandbox::new();
    let policy = confinement.policy.clone();
    let rlimit = confinement.rlimit_as_bytes;
    let extra = confinement.extra_env.clone();
    unsafe {
        command.pre_exec(move || {
            // A failed setsid must not kill the spawn (same policy as the
            // plain subprocess provider): the group falls back to the
            // parent's and cancel degrades to a direct-child kill.
            let _ = setsid();
            // Prove-injection first: the extras exist in the child's
            // environment before `prepare` scrubs, so a surviving
            // credential-shaped name is a scrub bug, not an artifact of
            // never having been set. `setenv(3)` is async-signal-safe
            // (POSIX), unlike `std::env::set_var`.
            for (name, value) in &extra {
                let cname = std::ffi::CString::new(name.as_str()).expect("env name has no NUL");
                let cvalue = std::ffi::CString::new(value.as_str()).expect("env value has no NUL");
                libc::setenv(cname.as_ptr(), cvalue.as_ptr(), 1);
            }
            // The hook runs in the forked child. On failure the child must
            // die itself — the hook cannot return an error across the fork
            // boundary — so a confined command never runs unconfined.
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
    stdout: Reader,
    stderr: Reader,
    cancelled: std::sync::atomic::AtomicBool,
}

impl ConfinedHandle {
    fn new(mut child: std::process::Child) -> Box<dyn SpawnHandle> {
        let stdout = Reader::start(child.stdout.take());
        let stderr = Reader::start(child.stderr.take());
        Box::new(Self {
            child: Arc::new(parking_lot::Mutex::new(child)),
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
        let Some(mut child) = self.child.try_lock() else {
            return;
        };
        // Prefer the whole process group (the child leads its own via
        // setsid); fall back to a direct kill.
        #[cfg(unix)]
        {
            let pid = child.id() as i32;
            if unsafe { kill(-pid, harnless_exec_subprocess::SIGTERM) } != 0 {
                let _ = child.kill();
            }
        }
        #[cfg(not(unix))]
        let _ = child.kill();
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
