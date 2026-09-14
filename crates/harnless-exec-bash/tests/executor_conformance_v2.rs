//! Run the execution-world conformance suite against the shipped execution
//! providers.
//!
//! Same discipline as `harnless-fs-local/tests/conformance.rs`: adding an
//! execution world means passing the suite unchanged. What is under test here is
//! the *real* pipeline — [`BashLocal`] building `[/bin/bash, -c, cmd]`,
//! [`LocalSandbox`] deciding, and [`ConfinedSpawner`] applying the verdict in the
//! forked child — with no fakes substituted for any leg.
//!
//! # Why the legs are shared rather than rebuilt
//!
//! The suite's argv-handover case can only observe what the shell actually hands
//! its sandbox and subprocess, and no seam lets a caller reach inside a shell's
//! legs. So the bundle wires the shell over the *same* `Arc`s the bundle carries,
//! and hands the suite a constructor that rebuilds the shell over legs the suite
//! supplies. A bundle that shipped a shell with private legs would make that case
//! report "unobservable" instead of passing, which is the honest outcome but a
//! strictly worse signal than a pass.
//!
//! # Why the proof variables are declared as applied
//!
//! The confined case reads the child's own `env` output to see whether the scrub
//! removed a credential-shaped variable. That is only evidence if the variable was
//! really in the child at fork time. Rust 2024 makes `std::env::set_var` unsafe
//! and this binary is multi-threaded, so — exactly as in `confined_spawn.rs` — the
//! variable is injected into the confined child's spawn environment through
//! [`ConfinedSpawner::with_extra_env`], which places it before the scrub runs. The
//! fixture then declares `proof_vars_applied`, and the suite asserts the scrub
//! rather than skipping.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use harnless_conformance::executor_suite::{
    ExecFixture, Executors, CONTROL_ENV_NAME, PROOF_ENV_NAME,
};
use harnless_exec_bash::{BashLocal, ConfinedSpawner};
use harnless_exec_sandbox::LocalSandbox;
use harnless_exec_subprocess::SubprocessLocal;
use harnless_seams::exec::{Sandbox, Shell, Subprocess};

/// The cases the instantiation below names.
///
/// Kept beside the macro invocation and asserted against the suite's own list, so
/// a case added to the suite cannot silently fall out of this run.
const CASES: &[&str] = &[
    "sandbox_sees_exact_argv",
    "enforced_reason_is_never_blank",
    "refusal_is_never_reported_as_confined",
    "consumer_routes_on_allowed",
    "denied_command_never_runs",
    "confined_run_is_actually_confined",
    "cancellation_is_honoured",
    "failing_command_is_typed_error",
];

/// The scratch root the running case runs under.
///
/// Built per case (the macro builds a fresh provider per case) and deliberately
/// leaked: the macro's factory returns the bundle and nothing downstream holds the
/// scratch alive, so a `Drop` that removed the directory could delete it while the
/// case is still running commands in it. The OS reclaims the temp entries.
fn scratch_root() -> &'static Path {
    static ROOT: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
        let root =
            std::env::temp_dir().join(format!("harnless-exec-conformance-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create scratch root");
        root.canonicalize().expect("canonical scratch root")
    });
    &ROOT
}

/// The shipped sandbox, wired to honour the suite's injected verdicts.
fn sandbox() -> Arc<dyn Sandbox> {
    // [`LocalSandbox`] is followed exactly — same mode resolution, same verdicts —
    // except that a verdict the suite has installed is replayed verbatim. Without
    // that hook the routing case would be driving a sandbox that never faced the
    // contradicting verdicts, and a pass would say nothing about how this provider
    // routes. The suite documents this as opt-in: a sandbox that ignores
    // `injected_verdict` fails the cases that need it rather than passing them by
    // accident.
    Arc::new(ConformanceSandbox(LocalSandbox::new()))
}

/// [`LocalSandbox`], plus the suite's verdict-injection hook.
struct ConformanceSandbox(LocalSandbox);

impl Sandbox for ConformanceSandbox {
    fn enforce(
        &self,
        argv: &[String],
        policy: &harnless_seams::exec::PolicyHome,
    ) -> harnless_seams::error::Result<harnless_seams::exec::Enforced> {
        if let Some(verdict) = harnless_conformance::executor_suite::injected_verdict() {
            return Ok(verdict);
        }
        self.0.enforce(argv, policy)
    }
}

/// The shipped confined spawner, over the shipped subprocess.
///
/// The proof variables ride in as spawner-level extras so a confined child
/// inherits them *before* the scrub — the only way to observe the scrub without
/// mutating this process's environment.
fn subprocess() -> Arc<dyn Subprocess> {
    let spawner = ConfinedSpawner::with_subprocess(Box::new(SubprocessLocal::new()))
        .with_extra_env(PROOF_ENV_NAME, "scrub-me")
        .with_extra_env(CONTROL_ENV_NAME, "keepme");
    Arc::new(spawner)
}

/// Build the shipped shell over legs supplied by the caller.
///
/// This is the production wiring — `BashLocal` over a confined spawner and the
/// local sandbox — lifted to take the legs as arguments so the suite can hand it
/// its own recorders. It is a plain `fn` item because the suite needs a function
/// pointer.
fn build_shell(sandbox: Arc<dyn Sandbox>, subprocess: Arc<dyn Subprocess>) -> Arc<dyn Shell> {
    let sandbox: Box<dyn Sandbox> = Box::new(ArcSandbox(sandbox));
    let subprocess: Box<dyn Subprocess> = Box::new(ArcSubprocess(subprocess));
    Arc::new(BashLocal::new(subprocess, sandbox))
}

/// The bundle the suite drives: the shipped shell over the shipped legs.
fn executors() -> Executors {
    let sandbox = sandbox();
    let subprocess = subprocess();
    Executors {
        shell: Some(build_shell(Arc::clone(&sandbox), Arc::clone(&subprocess))),
        sandbox: Some(sandbox),
        subprocess: Some(subprocess),
        shell_over: Some(build_shell),
    }
}

/// The fixture for one case: a real scratch root, and confinement where the case
/// asks for it.
fn fixture_for(case: &str) -> ExecFixture {
    let confined = matches!(
        case,
        "confined_run_is_actually_confined"
            | "refusal_is_never_reported_as_confined"
            | "denied_command_never_runs"
            | "consumer_routes_on_allowed"
    );
    let fixture = ExecFixture::new().root(scratch_root()).confined(confined);
    if case == "confined_run_is_actually_confined" {
        // `executors()` really places these two in a confined child's spawn
        // environment, so the suite may read the child's `env` output.
        return fixture
            .proof_vars(PROOF_ENV_NAME, CONTROL_ENV_NAME)
            .with_proof_vars_applied();
    }
    fixture
}

/// A [`Sandbox`] that forwards through an `Arc`, so the shell and the bundle can
/// hold the same provider.
struct ArcSandbox(Arc<dyn Sandbox>);

impl Sandbox for ArcSandbox {
    fn enforce(
        &self,
        argv: &[String],
        policy: &harnless_seams::exec::PolicyHome,
    ) -> harnless_seams::error::Result<harnless_seams::exec::Enforced> {
        self.0.enforce(argv, policy)
    }
}

/// A [`Subprocess`] that forwards through an `Arc`. See [`ArcSandbox`].
struct ArcSubprocess(Arc<dyn Subprocess>);

impl Subprocess for ArcSubprocess {
    fn spawn(
        &self,
        spawn: &harnless_seams::exec::Spawn,
    ) -> harnless_seams::error::Result<Box<dyn harnless_seams::exec::SpawnHandle>> {
        self.0.spawn(spawn)
    }
}

harnless_conformance::conformance_tests_executor! {
    bash_local,
    executors,
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

/// The instantiation above must name every case the suite ships.
#[test]
fn the_instantiation_covers_every_case() {
    let named: std::collections::BTreeSet<&str> = CASES.iter().copied().collect();
    let suite: std::collections::BTreeSet<&str> =
        harnless_conformance::executor_suite::EXECUTOR_CONFORMANCE_CASES
            .iter()
            .copied()
            .collect();
    assert_eq!(
        named, suite,
        "this instantiation and the suite's case list disagree"
    );
}
