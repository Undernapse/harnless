//! Issue #36: confined spawn must actually be confined — and must be safe
//! under a multi-threaded tokio runtime.
//!
//! Before #36 the sandbox *decided* confinement (`enforce` returned a
//! sandbox-local verdict) but nothing *applied* it: `LocalSandbox::prepare`
//! was never called, so a "confined" command ran with the full parent
//! environment in the parent's cwd. These tests pin the shipped boundary:
//! confined commands are spawned from a dedicated single-threaded spawner
//! thread, and `prepare` (env scrub + cwd pin + rlimit-where-supported) runs
//! in the forked child via `pre_exec` on that thread — where the fork-time
//! single-threadedness `pre_exec` safety requires is guaranteed regardless
//! of how many tokio worker threads the caller runs on.
//!
//! Env-var observation note: Rust 2024 makes `std::env::set_var` unsafe and
//! the test binary is one process with many threads, so instead of mutating
//! this process's environment we *prove* a variable was present in the
//! child's inherited environment by injecting it into the confined child's
//! own spawn env (`ConfinedSpawner::with_extra_env`). The scrub must remove
//! it before the command sees the environment — a var the test can prove was
//! in the child at fork time, then must not appear in `env` output.
//!
//! Every confined command below is capped with `ulimit -t` (CPU seconds) so
//! a broken boundary fails the test instead of hanging it.

use std::sync::Arc;

use harnless_exec_bash::{BashLocal, ConfinedSpawner};
use harnless_exec_sandbox::{LocalSandbox, PolicyHomeExt, SandboxMode};
use harnless_exec_subprocess::SubprocessLocal;
use harnless_seams::error::ErrorCode;
use harnless_seams::exec::{Enforced, PolicyHome, Sandbox, Shell, Spawn, SpawnHandle, Subprocess};

/// Records every spawn it is asked to run.
#[derive(Clone, Default)]
struct RecordingSubprocess {
    spawned: Arc<parking_lot::Mutex<Vec<Spawn>>>,
}

impl Subprocess for RecordingSubprocess {
    fn spawn(&self, spawn: &Spawn) -> harnless_seams::error::Result<Box<dyn SpawnHandle>> {
        self.spawned.lock().push(spawn.clone());
        Ok(Box::new(NullHandle))
    }
}

struct NullHandle;

impl SpawnHandle for NullHandle {
    fn output(&self) -> harnless_seams::error::Result<String> {
        Ok(String::new())
    }
    fn cancel(&self) {}
}

/// A sandbox that records the exact `Enforced` it handed back.
struct RecordingSandbox {
    inner: LocalSandbox,
    seen: Arc<parking_lot::Mutex<Vec<Enforced>>>,
}

impl Sandbox for RecordingSandbox {
    fn enforce(
        &self,
        argv: &[String],
        policy: &PolicyHome,
    ) -> harnless_seams::error::Result<Enforced> {
        let enforced = self.inner.enforce_verdict(argv, policy);
        self.seen.lock().push(enforced.clone());
        Ok(enforced)
    }
}

/// The pinned decision test (issue #36): the confinement boundary is a
/// dedicated single-threaded spawner thread, so confined spawn is safe no
/// matter how multi-threaded the caller's runtime is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confined_spawn_is_safe_under_multithread_runtime() {
    // We are on a 4-worker multi-threaded tokio runtime here.
    let root = std::env::temp_dir().join("harnless-36-multithread");
    std::fs::create_dir_all(&root).expect("create root");
    let policy = PolicyHome::from_root_and_mode(&root, SandboxMode::SandboxLocal);
    let shells: Vec<Arc<BashLocal>> = (0..8)
        .map(|_| {
            Arc::new(BashLocal::new(
                Box::new(ConfinedSpawner::new()),
                Box::new(LocalSandbox::default()),
            ))
        })
        .collect();
    // Drive the confined commands concurrently from dedicated threads (the
    // command tool's real shape: blocking exec off the async workers).
    let outs: Vec<_> = std::thread::scope(|scope| {
        shells
            .into_iter()
            .enumerate()
            .map(|(i, shell)| {
                let policy = policy.clone();
                scope.spawn(move || shell.exec(&format!("ulimit -t 5; printf child-{i}"), &policy))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().expect("spawner thread"))
            .collect()
    });
    for (i, out) in outs.into_iter().enumerate() {
        assert_eq!(
            format!("child-{i}"),
            out.expect("confined command must run under a valid confined policy")
        );
    }
}

/// The confined child must OBSERVE its confinement: scrubbed environment and
/// pinned cwd. The proof var is injected into the child's own spawn env, so
/// we know it was present at fork time; the scrub must remove it before the
/// command sees `env`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confined_child_observes_scrubbed_env_and_pinned_cwd() {
    // A root distinct from the process cwd, so a pinned cwd is observable.
    let root = std::env::temp_dir().join("harnless-36-confine-observe");
    std::fs::create_dir_all(&root).expect("create root");
    let canonical = root.canonicalize().expect("canonical root");
    let policy = PolicyHome::from_root_and_mode(&canonical, SandboxMode::SandboxLocal);

    let spawner = ConfinedSpawner::new()
        // Credential-looking: matches the scrub needles (TOKEN, and the
        // FAKE_ marker keeps the value uniquely identifiable).
        .with_extra_env("HARNLESS_36_FAKE_TOKEN", "hunter2")
        // Non-credential control: the scrub must leave it alone.
        .with_extra_env("HARNLESS_36_SAFE_VAR", "keepme");

    let shell = BashLocal::new(Box::new(spawner), Box::new(LocalSandbox::default()));
    let out = tokio::task::spawn_blocking(move || shell.exec("ulimit -t 5; pwd; env", &policy))
        .await
        .expect("join")
        .expect("confined command must run");

    // cwd pinned: `pwd` reports the canonical workspace root.
    let first_line = out.lines().next().unwrap_or_default();
    assert_eq!(
        canonical.display().to_string(),
        first_line.trim(),
        "confined child did not observe the pinned cwd; output:\n{out}"
    );
    // env scrubbed: the credential-looking var is gone from the child's
    // environment even though it was in the spawn env at fork time.
    assert!(
        !out.contains("HARNLESS_36_FAKE_TOKEN"),
        "credential-looking var survived the scrub; output:\n{out}"
    );
    // ...and the scrub is a scrub, not a blank-out: the control var remains.
    assert!(
        out.contains("HARNLESS_36_SAFE_VAR=keepme"),
        "non-credential var was removed too; output:\n{out}"
    );
}

/// Records the spawn it is asked to run, then defers to the real local
/// subprocess (so the unconfined path really executes).
struct RecordingPassthrough {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    spawned: Arc<parking_lot::Mutex<Vec<Spawn>>>,
}

impl Subprocess for RecordingPassthrough {
    fn spawn(&self, spawn: &Spawn) -> harnless_seams::error::Result<Box<dyn SpawnHandle>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.spawned.lock().push(spawn.clone());
        SubprocessLocal::new().spawn(spawn)
    }
}

/// The unconfined path must stay exactly as it was: same provider, same
/// argv, same cwd coordinate — and the confined spawner must delegate to
/// the plain subprocess directly for an unconfined verdict.
#[test]
fn unconfined_path_never_touches_the_confined_spawner() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let spawned = Arc::new(parking_lot::Mutex::new(Vec::new()));
    // The confined spawner wraps a plain subprocess: on an unconfined verdict
    // it must delegate to it directly (no spawner thread, same argv/cwd).
    let shell = BashLocal::new(
        Box::new(ConfinedSpawner::with_subprocess(Box::new(
            RecordingPassthrough {
                calls: Arc::clone(&calls),
                spawned: Arc::clone(&spawned),
            },
        ))),
        Box::new(LocalSandbox::default()),
    );
    let out = shell
        .exec(
            "printf plain",
            &PolicyHome::from_root_and_mode(".", SandboxMode::Unconfined),
        )
        .expect("run");
    assert_eq!("plain", out);
    // The confined spawner delegated to the plain subprocess (the unconfined
    // path is the plain path) and applied the same cwd coordinate as before.
    assert_eq!(
        1,
        calls.load(std::sync::atomic::Ordering::SeqCst),
        "unconfined verdict must run on the plain subprocess path"
    );
    let spawn = spawned.lock().pop().expect("spawned once");
    assert_eq!(
        vec![
            harnless_exec_bash::SHELL_PROGRAM.to_string(),
            "-c".to_string(),
            "printf plain".to_string()
        ],
        spawn.argv,
        "unconfined argv was rewritten"
    );
    assert_eq!(Some(".".to_string()), spawn.cwd);
    // And the confined hint is absent — the spawn is byte-for-byte what the
    // pre-#36 pipeline produced.
    assert!(
        spawn.confine.is_none(),
        "unconfined spawn carried a confine hint"
    );
}

/// The audit story is unchanged: a confined refusal is still `SandboxDenied`
/// with the enforced mode and reason, and it happens before any spawn.
#[test]
fn confined_refusal_is_sandbox_denied_before_any_spawn() {
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let spawned = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let shell = BashLocal::new(
        Box::new(ConfinedSpawner::with_subprocess(Box::new(
            RecordingSubprocess {
                spawned: Arc::clone(&spawned),
            },
        ))),
        Box::new(RecordingSandbox {
            inner: LocalSandbox::new(),
            seen: Arc::clone(&seen),
        }),
    );
    let err = shell
        .exec(
            "printf nope",
            &PolicyHome::from_root_and_mode(
                "/nonexistent/root-for-confined-spawn",
                SandboxMode::SandboxLocal,
            ),
        )
        .expect_err("refusal must not pass");
    assert_eq!(ErrorCode::SandboxDenied, err.code, "refusal: {err}");
    assert!(err.message.contains("mode=sandbox-local"), "{err}");
    assert!(err.message.contains("cannot be established"), "{err}");
    assert!(spawned.lock().is_empty(), "refused command still spawned");
    // The report the auditor reads is the same struct the executor consumed.
    let enforced = seen.lock().pop().expect("sandbox consulted");
    assert_eq!(
        Enforced {
            confined: false,
            allowed: false,
            mode: "sandbox-local".to_string(),
            reason: enforced.reason.clone(),
        },
        enforced
    );
}

/// A confined command whose workspace root disappears between the verdict
/// and the fork must fail closed: the child self-terminates in `pre_exec`
/// and the seam reports a spawn failure whose message shows the child died
/// by signal — never a successful unconfined run.
#[test]
fn confined_spawn_fails_closed_when_root_vanishes_after_verdict() {
    let root = std::env::temp_dir().join("harnless-36-vanish");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create root");
    let policy = PolicyHome::from_root_and_mode(&root, SandboxMode::SandboxLocal);
    // The verdict is decided while the root exists — the command is
    // allowed and confined.
    let argv = vec![
        harnless_exec_bash::SHELL_PROGRAM.to_string(),
        "-c".to_string(),
        "printf should-not-run".to_string(),
    ];
    let verdict = LocalSandbox::new().enforce(&argv, &policy).expect("decide");
    assert!(verdict.allowed && verdict.confined, "verdict: {verdict:?}");
    // The root vanishes between the verdict and the fork. The spawn's cwd
    // coordinate points at a directory that stays alive, so the failure
    // under test is the child's `prepare` chdir — not the parent's fork.
    std::fs::remove_dir_all(&root).expect("remove root after verdict");
    let spawn = Spawn {
        argv,
        cwd: Some(std::env::temp_dir().display().to_string()),
        confine: Some(harnless_seams::exec::ConfineHint::new(
            harnless_exec_bash::Confinement {
                policy: policy.clone(),
                rlimit_as_bytes: None,
                extra_env: Vec::new(),
            },
        )),
    };
    let spawner = ConfinedSpawner::new();
    let handle = spawner.spawn(&spawn).expect("spawn itself succeeds");
    let err = handle
        .output()
        .expect_err("must fail closed when confinement cannot be applied");
    // The verdict was decided against a root that still existed (the test
    // created it), then the root was removed before the fork — the child's
    // `prepare` fails and the child must die in `pre_exec`.
    assert_eq!(
        ErrorCode::SpawnFailed,
        err.code,
        "must report the child's failed exit, not a successful unconfined run: {err}"
    );
    assert!(
        err.message.contains("signal"),
        "the child must die in pre_exec (signal exit), not run: {err}"
    );
    assert!(
        !err.message.contains("should-not-run"),
        "the command ran despite confinement failing to apply: {err}"
    );
}

/// A confined spawn whose hint cannot be read (wrong payload type) must be
/// refused, never silently routed to the plain unconfined path.
#[test]
fn confined_spawn_refuses_unreadable_hint() {
    let root = std::env::temp_dir().join("harnless-36-bad-hint");
    std::fs::create_dir_all(&root).expect("create root");
    let policy = PolicyHome::from_root_and_mode(&root, SandboxMode::SandboxLocal);
    let spawn = Spawn {
        argv: vec![
            harnless_exec_bash::SHELL_PROGRAM.to_string(),
            "-c".to_string(),
            "printf should-not-run".to_string(),
        ],
        cwd: Some(root.display().to_string()),
        // A confined verdict, but the payload is not a Confinement.
        confine: Some(harnless_seams::exec::ConfineHint::new("not-a-confinement")),
    };
    let spawner = ConfinedSpawner::new();
    let err = match spawner.spawn(&spawn) {
        Ok(_) => panic!("an unreadable confinement hint must refuse the spawn"),
        Err(err) => err,
    };
    assert_eq!(
        ErrorCode::SandboxDenied,
        err.code,
        "unreadable hint must refuse, not degrade to the plain path: {err}"
    );
}

/// The rlimit half of `prepare` is applied where the OS supports it and
/// honestly not-applied on macOS. Linux-only pin: the confined child must
/// observe the `RLIMIT_AS` the confinement asked for.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn confined_child_observes_rlimit_where_supported() {
    let root = std::env::temp_dir().join("harnless-36-rlimit");
    std::fs::create_dir_all(&root).expect("create root");
    let policy = PolicyHome::from_root_and_mode(&root, SandboxMode::SandboxLocal);
    let spawn = Spawn {
        argv: vec![
            harnless_exec_bash::SHELL_PROGRAM.to_string(),
            "-c".to_string(),
            "ulimit -t 5; ulimit -v".to_string(),
        ],
        cwd: Some(root.display().to_string()),
        confine: Some(harnless_seams::exec::ConfineHint::new(
            harnless_exec_bash::Confinement {
                policy: policy.clone(),
                // 512 MiB, in bytes; `ulimit -v` reports KiB.
                rlimit_as_bytes: Some(512 * 1024 * 1024),
                extra_env: Vec::new(),
            },
        )),
    };
    let spawner = ConfinedSpawner::new();
    let out = spawner
        .spawn(&spawn)
        .expect("spawn")
        .output()
        .expect("confined command must run");
    assert_eq!(
        (512 * 1024).to_string(),
        out.trim(),
        "confined child did not observe the requested RLIMIT_AS"
    );
}
