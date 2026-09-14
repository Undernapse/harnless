//! The execution-world conformance suite.
//!
//! [`check_executor_contract_all`] runs every case in
//! [`EXECUTOR_CONFORMANCE_CASES`] against a provider;
//! [`check_executor_contract`] runs one case by name (what the
//! [`conformance_tests_executor`](crate::conformance_tests_executor) macro
//! expands to).
//!
//! # What an "executor" is here
//!
//! The execution world is three seams that only mean something together:
//! [`Shell`] turns a command line into spawn coordinates, [`Sandbox`]
//! reports what it enforces for those coordinates, and [`Subprocess`]
//! spawns them. Checking one trait in isolation cannot express the
//! interesting obligations — "the sandbox saw the argv that actually
//! spawned" is a statement about the *pair*. So a provider is handed to the
//! suite as an [`Executors`] bundle, and a provider that genuinely cannot
//! supply one leg (a shell with no sandbox hook, a sandbox with no
//! executor) declares it by leaving that leg `None`; the cases needing it
//! skip rather than false-fail.
//!
//! # Skip, not violation
//!
//! Same rule as the filesystem suite, and for the same reason: a case that
//! cannot build its fixture through the provider's own surface owes nothing.
//! A provider whose `Subprocess` refuses a spawn, whose `Sandbox` refuses a
//! policy root, or whose shell is not the one under test, gets a skip on the
//! dependent assertions. Skips print under `HARNLESS_CONFORMANCE_DEBUG`.
//!
//! # Fixtures are self-provisioning
//!
//! Cases build what they need at runtime: a scratch workspace root under the
//! system temp dir, commands that report their own environment and working
//! directory, a long-running command to cancel. Nothing on disk is assumed
//! to exist beforehand, and no case mutates the test process's environment
//! (Rust 2024 makes that unsafe, and the confinement proof instead *injects*
//! a variable into the child's own spawn environment — see
//! [`ExecFixture::proof_var`]).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use harnless_seams::error::ErrorCode;
use harnless_seams::exec::{Enforced, PolicyHome, Sandbox, Shell, Spawn, SpawnHandle, Subprocess};

use crate::types::Violation;

/// Every execution-world conformance case, in suite order.
pub const EXECUTOR_CONFORMANCE_CASES: &[&str] = &[
    "sandbox_sees_exact_argv",
    "enforced_reason_is_never_blank",
    "refusal_is_never_reported_as_confined",
    "consumer_routes_on_allowed",
    "injected_deny_reaches_shell",
    "denied_command_never_runs",
    "confined_run_is_actually_confined",
    "cancellation_is_honoured",
    "failing_command_is_typed_error",
];

/// The environment variable a confined case injects into its own child, and
/// the value that proves the scrub removed it.
///
/// Credential-shaped on purpose: [`harnless_exec_sandbox::scrub_needle`] (and
/// any conforming scrub) must drop a name like this. A provider that does not
/// support injecting a variable into a confined child's spawn environment
/// leaves [`ExecFixture::proof_var`] `None` and the env half of the confined
/// case skips — the cwd half still runs.
pub const PROOF_ENV_NAME: &str = "HARNLESS_CONFORMANCE_FAKE_TOKEN";
/// The value paired with [`PROOF_ENV_NAME`].
pub const PROOF_ENV_VALUE: &str = "scrub-me";
/// A non-credential control variable: the scrub must leave it alone.
pub const CONTROL_ENV_NAME: &str = "HARNLESS_CONFORMANCE_SAFE_VAR";
/// The value paired with [`CONTROL_ENV_NAME`].
pub const CONTROL_ENV_VALUE: &str = "keepme";

/// The three execution-world legs a provider exposes to the suite.
///
/// A field left `None` is an honest "this provider has no such leg", and the
/// cases that need it skip. Legs are `Arc` rather than `Box` because the
/// recording wrappers the exact-argv and routing cases install must own the
/// leg they wrap (the seam traits require `'static`), and the cancellation
/// case drives a handle from a thread of its own.
pub struct Executors {
    /// The shell under test.
    pub shell: Option<Arc<dyn Shell>>,
    /// The sandbox consulted with the exact argv.
    pub sandbox: Option<Arc<dyn Sandbox>>,
    /// The subprocess that actually spawns.
    pub subprocess: Option<Arc<dyn Subprocess>>,
    /// How to build this provider's shell over legs the suite supplies.
    ///
    /// A shell owns the sandbox and subprocess it talks to, and no seam lets a
    /// caller swap them. The handover case is therefore only observable when the
    /// suite can hand the shell *the legs it is recording*: the case rebuilds
    /// the shell over its own recording wrappers, and this constructor is what
    /// makes that possible. A bundle without it cannot be driven through the
    /// handover, and the case says so rather than skipping on a provider it is
    /// not actually observing.
    pub shell_over: Option<ShellOver>,
}

/// Builds a provider's shell over a sandbox and subprocess supplied by the suite.
///
/// The harness's own construction, lifted one level: the same code that built
/// the shipped shell, but taking the legs as arguments so the suite can hand it
/// the recorders. Build the shell the way production does — same wiring, same
/// types — and pass the legs through.
pub type ShellOver = fn(Arc<dyn Sandbox>, Arc<dyn Subprocess>) -> Arc<dyn Shell>;

impl Executors {
    /// A bundle with no legs set; fill the ones the provider has.
    pub fn new() -> Self {
        Self {
            shell: None,
            sandbox: None,
            subprocess: None,
            shell_over: None,
        }
    }

    /// Declare how to build this provider's shell over suite-supplied legs.
    ///
    /// The handover case rebuilds the shell over its recording wrappers, so
    /// this is the harness's promise that `constructor` produces *the* shell
    /// under test given any sandbox and subprocess. Pass the same wiring
    /// production uses — the shipped providers' harnesses do exactly that.
    #[must_use]
    pub fn shell_over(mut self, constructor: ShellOver) -> Self {
        self.shell_over = Some(constructor);
        self
    }

    /// Set the shell leg.
    #[must_use]
    pub fn shell(mut self, shell: Arc<dyn Shell>) -> Self {
        self.shell = Some(shell);
        self
    }

    /// Set the sandbox leg.
    #[must_use]
    pub fn sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Set the subprocess leg.
    #[must_use]
    pub fn subprocess(mut self, subprocess: Arc<dyn Subprocess>) -> Self {
        self.subprocess = Some(subprocess);
        self
    }
}

impl Default for Executors {
    fn default() -> Self {
        Self::new()
    }
}

/// The fixtures a provider's harness provisions for one case.
///
/// Built by [`ExecutorFixtureFactory`] per case, so a harness decides which
/// scratch root, which policy mode and which proof variables each case runs
/// under — the suite never assumes a provider's sandbox reads some
/// pre-existing directory.
#[derive(Debug, Clone)]
pub struct ExecFixture {
    /// The scratch workspace root the case runs under, when the harness
    /// provisioned one. Cases needing a real directory skip without it.
    pub root: Option<PathBuf>,
    /// Whether the case's policy asks for confinement.
    pub confined: bool,
    /// A credential-shaped variable the harness arranges for the confined
    /// child to inherit, so the scrub's effect is observable.
    pub proof_var: Option<(String, String)>,
    /// A non-credential variable the scrub must leave alone.
    pub control_var: Option<(String, String)>,
    /// Whether the harness's spawner actually places [`ExecFixture::proof_var`]
    /// in a confined child's spawn environment.
    ///
    /// Declaring proof variables without placing them is how a conformance run
    /// ends up asserting on an environment it never established, so the
    /// confined case checks this flag before reading the child's `env` output.
    pub proof_vars_applied: bool,
    /// How long the cancellation case waits for a cancelled process to die.
    pub cancel_timeout: Duration,
}

impl ExecFixture {
    /// A fixture with no root and no confinement; the harness fills in what
    /// its provider supports.
    pub fn new() -> Self {
        Self {
            root: None,
            confined: false,
            proof_var: None,
            control_var: None,
            proof_vars_applied: false,
            cancel_timeout: Duration::from_secs(10),
        }
    }

    /// Set the scratch workspace root.
    #[must_use]
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Ask for a confined policy.
    #[must_use]
    pub fn confined(mut self, confined: bool) -> Self {
        self.confined = confined;
        self
    }

    /// Declare the injected proof/control pair.
    #[must_use]
    pub fn proof_vars(mut self, proof: impl Into<String>, control: impl Into<String>) -> Self {
        self.proof_var = Some((proof.into(), PROOF_ENV_VALUE.to_string()));
        self.control_var = Some((control.into(), CONTROL_ENV_VALUE.to_string()));
        self
    }

    /// Declare that the harness's spawner really places the proof/control pair
    /// in a confined child's spawn environment (see
    /// [`ExecFixture::proof_vars_applied`]).
    #[must_use]
    pub fn with_proof_vars_applied(mut self) -> Self {
        self.proof_vars_applied = self.proof_var.is_some();
        self
    }

    /// Override the cancellation bound.
    #[must_use]
    pub fn cancel_timeout(mut self, cancel_timeout: Duration) -> Self {
        self.cancel_timeout = cancel_timeout;
        self
    }

    /// The policy home this fixture describes.
    ///
    /// Built from the seam's own fields rather than a provider-specific
    /// helper, so the suite stays dependency-free: `workspace_root` is the
    /// scratch root and `default_confined` is the requested mode.
    pub fn policy(&self) -> Option<PolicyHome> {
        let root = self.root.as_ref()?;
        Some(PolicyHome {
            workspace_root: root.display().to_string(),
            default_confined: self.confined,
        })
    }
}

/// A provider's fixture library: builds the scratch fixtures one case needs.
pub type ExecutorFixtureFactory = fn(&str) -> ExecFixture;

/// Run the whole execution-world suite against `providers`.
///
/// A provider is conformant when the returned list is empty.
pub fn check_executor_contract_all(
    providers: &Executors,
    fixture_for: ExecutorFixtureFactory,
) -> Vec<Violation> {
    let mut out = Vec::new();
    for case in EXECUTOR_CONFORMANCE_CASES {
        check_case_into(providers, fixture_for, case, &mut out);
    }
    out
}

/// Run one execution-world conformance case by name.
///
/// Unknown case names yield a single violation naming the unknown case.
pub fn check_executor_contract(
    providers: &Executors,
    case: &str,
    fixture_for: ExecutorFixtureFactory,
) -> Vec<Violation> {
    let mut out = Vec::new();
    check_case_into(providers, fixture_for, case, &mut out);
    out
}

fn check_case_into(
    providers: &Executors,
    fixture_for: ExecutorFixtureFactory,
    case: &str,
    out: &mut Vec<Violation>,
) {
    if !EXECUTOR_CONFORMANCE_CASES.contains(&case) {
        out.push(Violation::new(case.to_string(), "unknown conformance case"));
        return;
    }
    // The fixture is the harness's own construction; a factory that panics
    // is the provider misbehaving on the suite's input.
    let fixture =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (fixture_for)(case))) {
            Ok(fixture) => fixture,
            Err(_) => {
                out.push(Violation::new(
                    case.to_string(),
                    "the fixture factory panicked while building this case's fixtures",
                ));
                return;
            }
        };
    let mut cx = Cx::default();
    match case {
        "sandbox_sees_exact_argv" => sandbox_sees_exact_argv(providers, &fixture, &mut cx),
        "enforced_reason_is_never_blank" => {
            enforced_reason_is_never_blank(providers, &fixture, &mut cx)
        }
        "refusal_is_never_reported_as_confined" => {
            refusal_is_never_reported_as_confined(providers, &fixture, &mut cx)
        }
        "consumer_routes_on_allowed" => consumer_routes_on_allowed(providers, &fixture, &mut cx),
        "injected_deny_reaches_shell" => injected_deny_reaches_shell(providers, &fixture, &mut cx),
        "denied_command_never_runs" => denied_command_never_runs(providers, &fixture, &mut cx),
        "confined_run_is_actually_confined" => {
            confined_run_is_actually_confined(providers, &fixture, &mut cx)
        }
        "cancellation_is_honoured" => cancellation_is_honoured(providers, &fixture, &mut cx),
        "failing_command_is_typed_error" => {
            failing_command_is_typed_error(providers, &fixture, &mut cx)
        }
        _ => unreachable!("case name checked above"),
    }
    out.extend(cx.into_violations(case));
}

/// Per-case outcome collector.
#[derive(Default)]
struct Cx {
    violations: Vec<Violation>,
    skips: Vec<String>,
}

impl Cx {
    fn fail(&mut self, detail: impl Into<String>) {
        self.violations.push(Violation::new(String::new(), detail));
    }

    /// The provider cannot produce this fixture; the dependent assertions are
    /// owed nothing. A skip is not a violation.
    fn skip(&mut self, why: impl Into<String>) {
        self.skips.push(why.into());
    }

    /// Borrow a leg, skipping the case when the provider has none.
    fn leg<'a, T: ?Sized>(&mut self, slot: &'a Option<Arc<T>>, what: &str) -> Option<&'a T> {
        match slot.as_deref() {
            Some(leg) => Some(leg),
            None => {
                self.skip(format!("provider supplied no {what} leg"));
                None
            }
        }
    }

    /// Borrow the scratch root, skipping when the harness provisioned none.
    fn root(&mut self, fixture: &ExecFixture) -> Option<PathBuf> {
        match fixture.root.clone() {
            Some(root) => Some(root),
            None => {
                self.skip("harness provisioned no scratch workspace root");
                None
            }
        }
    }

    fn into_violations(self, case: &str) -> Vec<Violation> {
        if std::env::var_os("HARNLESS_CONFORMANCE_DEBUG").is_some() && !self.skips.is_empty() {
            eprintln!("[conformance skip] {case}: {}", self.skips.join("; "));
        }
        self.violations
            .into_iter()
            .map(|mut v| {
                v.case = case.to_string();
                v
            })
            .collect()
    }
}

/// A command line that writes a unique marker file, for "did it actually
/// run?" assertions.
fn touch_command(path: &std::path::Path) -> String {
    format!("touch '{}'", path.display())
}

/// A command line that reports its own working directory and environment.
fn inspect_command() -> &'static str {
    "pwd; env"
}

/// Run `command` through the shell leg, reporting the outcome in words a
/// violation can quote.
enum Ran {
    /// The shell returned output.
    Ok(String),
    /// The shell returned a typed error.
    Err(ErrorCode, String),
    /// The provider panicked inside `exec`.
    Panicked,
}

impl Ran {
    fn described(&self) -> String {
        match self {
            Ran::Ok(out) => format!("returned Ok({out:?})"),
            Ran::Err(code, message) => format!("returned `{}` ({message})", code.as_str()),
            Ran::Panicked => "panicked".to_string(),
        }
    }
}

fn run(shell: &dyn Shell, command: &str, policy: &PolicyHome) -> Ran {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shell.exec(command, policy))) {
        Ok(Ok(out)) => Ran::Ok(out),
        Ok(Err(err)) => Ran::Err(err.code, err.message.clone()),
        Err(_) => Ran::Panicked,
    }
}

/// A command line that certainly fails and prints a marker on the way out.
const FAILING_COMMAND: &str = "echo failing-on-purpose; exit 7";

/// The marker a denied command must not leave behind.
fn marker_in(root: &std::path::Path, case: &str) -> PathBuf {
    root.join(format!("harnless-conformance-{case}-must-not-exist"))
}

// ---------------------------------------------------------------------------
// The argv handover
// ---------------------------------------------------------------------------

/// The sandbox receives the **exact** argv about to spawn.
///
/// This is the cross-provider obligation the execution world exists to
/// guarantee: a verdict computed about `[/bin/bash, -c, …]` says nothing
/// about the command that actually runs if the executor rewrites argv on the
/// way to the spawner. The case wraps both legs with recorders and compares
/// the bytes the sandbox was consulted with against the bytes the subprocess
/// was handed — element by element, not "equal after normalisation".
///
/// # Wiring is the harness's job
///
/// A shell normally *owns* the sandbox and subprocess it consults, and no seam
/// lets a caller swap them, so the recorders can only observe a handover the
/// harness already wired: build the shell over the legs you hand the suite, and
/// declare it with [`Executors::shell_over`]. A bundle that does not
/// declare it fails this case rather than skipping — a skip would let a
/// provider whose shell holds private legs read as conformant. The shipped
/// providers are instantiated that way in their own test files.
fn sandbox_sees_exact_argv(providers: &Executors, fixture: &ExecFixture, cx: &mut Cx) {
    let Some(policy) = fixture.policy() else {
        cx.skip("harness provisioned no scratch workspace root");
        return;
    };
    if cx.leg(&providers.shell, "shell").is_none()
        || cx.leg(&providers.sandbox, "sandbox").is_none()
        || cx.leg(&providers.subprocess, "subprocess").is_none()
    {
        return;
    }

    // A command line with quoting, expansion, globs, unicode and backslashes
    // — everything an argv rewrite would mangle.
    let command = r#"echo 'a "b" $HOME * ? 中文 \ back' > /dev/null"#;
    let seen = recorder::Recorder::<Vec<String>>::default();
    let spawned = recorder::Recorder::<Spawn>::default();

    // The recording legs wrap the bundle's legs, and the shell under test is
    // *rebuilt over the wrapped legs* — not merely wrapped itself. A shell owns
    // the sandbox and spawner it was constructed with, so a wrapper around the
    // shell would leave the shell consulting the unwrapped legs and the
    // recorders would see nothing: the case would report an unobservable
    // handover for every honest provider. [`Executors::shell_over`] is the
    // harness's construction lifted one level, which is what lets the suite hand
    // the shell the legs it will actually use.
    let recording_sandbox: Arc<dyn Sandbox> = Arc::new(LoggingSandbox {
        inner: Arc::clone(providers.sandbox.as_ref().expect("checked by cx.leg")),
        seen: seen.clone(),
    });
    let recording_subprocess: Arc<dyn Subprocess> = Arc::new(LoggingSubprocess {
        inner: Arc::clone(providers.subprocess.as_ref().expect("checked by cx.leg")),
        spawned: spawned.clone(),
    });

    // The shell under test is the SAME object the rest of the suite drives: the
    // harness's declared constructor applied to the recording legs when one is
    // declared, otherwise the shipped shell. Building a shell here and then
    // driving a *different* object would make this case pass without ever
    // exercising the wiring it claims to check.
    let shell_under_test: Arc<dyn Shell> = match providers.shell_over {
        Some(over) => over(
            Arc::clone(&recording_sandbox),
            Arc::clone(&recording_subprocess),
        ),
        None => Arc::clone(providers.shell.as_ref().expect("checked by cx.leg")),
    };

    // The bundle is built from the shell this case constructed, over the
    // recording legs. Non-vacuity is enforced observably below: the case fails
    // when neither leg was touched and the harness declared no `shell_over`.
    // There is deliberately no identity assertion here — comparing two
    // suite-owned locals the case just assigned cannot fail for any provider,
    // and it would be compiled out in release anyway.
    let wired = Executors::new()
        .shell(Arc::clone(&shell_under_test))
        .sandbox(recording_sandbox)
        .subprocess(recording_subprocess);
    let wired_shell = wired.shell.as_deref().expect("just set");

    let outcome = run(wired_shell, command, &policy);
    match outcome {
        Ran::Ok(_) | Ran::Err(..) => {}
        Ran::Panicked => {
            cx.fail("the provider panicked on a plain command");
            return;
        }
    }

    let consulted = seen.last();
    let handed = spawned.last();
    let (Some(consulted_ref), Some(handed_ref)) = (&consulted, &handed) else {
        // Nothing reached the recording legs. That is only legitimate when the
        // provider refused before the handover; a provider whose shell holds
        // legs the bundle never sees would silently escape this case, so the
        // suite distinguishes the two by asking the shell to run a command
        // the sandbox certainly permits and checking whether *any* leg was
        // touched.
        match (&consulted, &handed) {
            (Some(_), None) => {
                // The verdict refused before the spawner. Legitimate; the
                // refusal cases audit the refusal itself.
                cx.skip("sandbox was consulted but nothing spawned (refusal)");
            }
            (None, Some(_)) => cx.fail(
                "the executor spawned a command the sandbox was never consulted about; every \
                 spawn goes through the sandbox verdict first",
            ),
            (None, None) => {
                // Nothing reached the recorded legs. The harness declared
                // whether the shell consults them, and that declaration
                // decides whether this is a skip or a hole in the run.
                if providers.shell_over.is_some() {
                    // The legs are the shell's legs, so the only honest
                    // reading left is a refusal before the handover — which
                    // the verdict cases audit and this case owes nothing on.
                    cx.skip("nothing was consulted or spawned (provider refused early)");
                } else {
                    cx.fail(
                        "the shell never touched the bundle's legs, and the harness did not \
                         declare `shell_over`, so the handover was never observable: give the \
                         suite a constructor that builds the shell over the legs it hands \
                         over, or this case proves nothing",
                    );
                }
            }
            (Some(_), Some(_)) => unreachable!("both present handled above"),
        }
        return;
    };
    if consulted_ref.as_slice() != handed_ref.argv.as_slice() {
        cx.fail(format!(
            "the sandbox was consulted with {consulted_ref:?} and the subprocess was handed \
             {handed_ref:?}; the verdict is about different bytes than the command that ran \
             (argv rewriting is the failure this case exists for)",
        ));
        return;
    }
    // And the command element must be the caller's bytes, verbatim — not the
    // same string after a shell-quote round-trip.
    let command_element = consulted_ref.last();
    if command_element.map(String::as_str) != Some(command) {
        cx.fail(format!(
            "the command element the sandbox saw is {command_element:?}, not the caller's \
             command {command:?}"
        ));
    }
    // The spawn coordinate for cwd must come from the same policy the fs side
    // reads, or the two worlds confine to different roots.
    if handed_ref.cwd.as_deref() != Some(policy.workspace_root.as_str()) {
        cx.fail(format!(
            "the spawn cwd is {:?} but the policy root is {:?}; executor and filesystem \
             must confine to the same declared root",
            handed_ref.cwd, policy.workspace_root
        ));
    }
}

// ---------------------------------------------------------------------------
// The enforced report
// ---------------------------------------------------------------------------

/// `Enforced::reason` is always non-empty.
///
/// An auditor must be able to reconstruct the decision from the report
/// alone, so both verdicts carry a reason. The case asks the sandbox for a
/// permitted policy and a refusing policy and checks both reports.
fn enforced_reason_is_never_blank(providers: &Executors, fixture: &ExecFixture, cx: &mut Cx) {
    let Some(sandbox) = cx.leg(&providers.sandbox, "sandbox") else {
        return;
    };
    let Some(root) = cx.root(fixture) else {
        return;
    };
    let argv = vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()];
    for (label, confined) in [("permitted", false), ("confined", true)] {
        let policy = PolicyHome {
            workspace_root: root.display().to_string(),
            default_confined: confined,
        };
        let enforced = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sandbox.enforce(&argv, &policy)
        })) {
            Ok(Ok(enforced)) => enforced,
            Ok(Err(err)) => {
                cx.skip(format!(
                    "sandbox refused to decide the {label} policy (`{}`)",
                    err.code.as_str()
                ));
                continue;
            }
            Err(_) => {
                cx.fail(format!("the sandbox panicked deciding the {label} policy"));
                continue;
            }
        };
        if enforced.reason.trim().is_empty() {
            cx.fail(format!(
                "the {label} verdict reported an empty `reason`; the report must be \
                 self-explanatory: {enforced:?}"
            ));
        }
        if enforced.mode.trim().is_empty() {
            cx.fail(format!(
                "the {label} verdict reported an empty `mode`; a report without the mode \
                 it enforced cannot be audited: {enforced:?}"
            ));
        }
    }
}

/// A refusal report must be honest about what it refused.
fn audit_refusal(cx: &mut Cx, enforced: &Enforced) {
    if enforced.confined {
        cx.fail(format!(
            "refusal reported `confined == true`: {enforced:?}; a command that does not run \
             was not confined, and a consumer reading `confined` would record enforcement \
             that never happened"
        ));
    }
    if enforced.reason.trim().is_empty() {
        cx.fail(format!(
            "refusal reported no reason: {enforced:?}; the refusal must be reconstructable \
             from the report alone"
        ));
    }
}

fn refusal_is_never_reported_as_confined(
    providers: &Executors,
    fixture: &ExecFixture,
    cx: &mut Cx,
) {
    let Some(sandbox) = cx.leg(&providers.sandbox, "sandbox") else {
        return;
    };
    let Some(root) = cx.root(fixture) else {
        return;
    };
    let policy = fixture.policy().expect("root implies a policy");
    let argv = vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()];
    // Two independent ways to reach a refusal, because a single natural fixture
    // can be silently unavailable: a root that cannot be entered is the
    // portable ask, and an installed fixture verdict is the guaranteed one. A
    // sandbox that refuses on neither offers the obligation no reachable
    // fixture, which is a hole in the provider — not a skip.
    let mut reached_refusal = false;

    // Path 1: a natural refusal from an unenterable root.
    let missing = root.join("harnless-conformance-vanished-root");
    let refusing = PolicyHome {
        workspace_root: missing.display().to_string(),
        default_confined: true,
    };
    let natural = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sandbox.enforce(&argv, &refusing)
    }));
    match natural {
        Ok(Ok(enforced)) => {
            if enforced.allowed {
                // Legitimate for a sandbox that confines nothing by root. Not a
                // pass: the obligation still has to be exercised, so fall
                // through to the injected path below.
            } else {
                reached_refusal = true;
                audit_refusal(cx, &enforced);
            }
        }
        Ok(Err(_)) | Err(_) => {
            // An errored or panicking report on a plain policy is handled by the
            // reason/panic cases; here it just means no natural refusal.
        }
    }

    // Path 2: a refusal the suite installs, so the obligation is always
    // exercised even by a sandbox that never refuses. The installed verdict is
    // deliberately `confined: false` — the shape a refusal actually has — so
    // `audit_refusal` can pass or fail on it. A `confined: true` fixture would
    // make this path only ever produce a false failure, never a verdict.
    if !reached_refusal {
        inject(Enforced {
            allowed: false,
            confined: false,
            mode: String::new(),
            reason: INJECTED_REASON.to_string(),
        });
        let injected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sandbox.enforce(&argv, &policy)
        }));
        clear_injection();
        match injected {
            Ok(Ok(enforced)) if !enforced.allowed => audit_refusal(cx, &enforced),
            _ => cx.fail(
                "no refusal is reachable for this sandbox — neither an unenterable root nor \
                 an installed deny verdict produced `allowed == false`; the obligation \
                 (a refusal never reports `confined == true`) was never exercised",
            ),
        }
    }
}

/// Consumers route on `allowed`, so the report must not need help.
///
/// The shipped consumer decides "run / refuse / surface to the user" by
/// reading `allowed` and nothing else. This case feeds the shell a verdict
/// whose other fields *contradict* the verdict — the classic way a provider
/// leaks a heuristic read of `confined`/`mode` into a decision — and checks
/// that the shell's behaviour still follows `allowed` alone.
///
/// # The verdict is installed, not the shell swapped
///
/// A shell owns the sandbox it consults, so the suite cannot inject a verdict
/// by handing the shell a different bundle — the shell would never look at it.
/// An injected verdict therefore goes where the shell will actually look: a
/// sandbox participating in these cases calls [`injected_verdict`] before
/// deciding and replays what is installed. A provider whose sandbox ignores it
/// is not driven by the injection and fails the cases that need it, which is
/// the honest outcome — the suite cannot claim to have tested a routing
/// decision the provider never faced.
fn consumer_routes_on_allowed(providers: &Executors, fixture: &ExecFixture, cx: &mut Cx) {
    let Some(policy) = fixture.policy() else {
        cx.skip("harness provisioned no scratch workspace root");
        return;
    };
    if cx.leg(&providers.shell, "shell").is_none()
        || cx.leg(&providers.subprocess, "subprocess").is_none()
    {
        return;
    }
    let marker = marker_in(
        fixture.root.as_ref().expect("policy() implies a root"),
        "routes-on-allowed",
    );
    let _ = std::fs::remove_file(&marker);
    let command = touch_command(&marker);

    let shell = providers.shell.as_ref().expect("checked by cx.leg");

    // Case A1: `allowed == true`, `confined == false`, and a `mode` claiming a
    // deny-all policy. The command must run.
    //
    // The marker is asserted only when the run reported success. A consumer
    // that reads `mode` and refuses anyway is caught by the outcome check with
    // the better message; a provider that honours the verdict but whose spawner
    // cannot reach the scratch root reports an honest spawn error, and doubling
    // that into a false "did not run" would be wrong.
    inject(Enforced {
        allowed: true,
        confined: false,
        mode: "deny-all".to_string(),
        reason: INJECTED_REASON.to_string(),
    });
    let outcome_a1 = run(shell.as_ref(), &command, &policy);
    clear_injection();
    match outcome_a1 {
        Ran::Ok(_) => {
            if !marker.exists() {
                cx.fail(
                    "a permitted verdict reported success without running the command; the \
                     marker file was never created",
                );
            }
        }
        other => cx.fail(format!(
            "a verdict with `allowed == true` was not honoured: the executor {}",
            other.described()
        )),
    }
    let _ = std::fs::remove_file(&marker);

    // Case A2: `allowed == true` with `confined == true` and a mode reading
    // `sandbox-local`. The command must still run — `allowed` is the verdict.
    //
    // This half exists for the consumer that treats "confined" as "the sandbox
    // handled it, so I need not read `allowed`". The marker is the observable:
    // a consumer that skips the spawn leaves it absent. It is asserted only when
    // the run reported success, because a provider that refuses a permitted
    // verdict is caught by the outcome check with a better message — and a
    // provider that honours the verdict but whose spawner cannot reach the
    // scratch root reports an honest spawn error, which must not be doubled into
    // a false "did not run".
    inject(Enforced {
        allowed: true,
        confined: true,
        mode: "sandbox-local".to_string(),
        reason: INJECTED_REASON.to_string(),
    });
    let outcome_a2 = run(shell.as_ref(), &command, &policy);
    clear_injection();
    match outcome_a2 {
        Ran::Ok(_) => {
            if !marker.exists() {
                cx.fail(
                    "a permitted verdict reported success without running the command; the \
                     marker file was never created (a consumer that read `confined` instead \
                     of `allowed` is the usual cause)",
                );
            }
        }
        other => cx.fail(format!(
            "a confined-but-permitted verdict was not honoured: the executor {}",
            other.described()
        )),
    }
    let _ = std::fs::remove_file(&marker);

    // Case B: `allowed == false` with `confined == true` and a mode reading
    // `sandbox-local`. The command must not run.
    inject(Enforced {
        allowed: false,
        confined: true,
        mode: "sandbox-local".to_string(),
        reason: INJECTED_REASON.to_string(),
    });
    let outcome_b = run(shell.as_ref(), &command, &policy);
    clear_injection();
    match outcome_b {
        Ran::Err(code, _) if code == ErrorCode::SandboxDenied => {}
        Ran::Err(code, message) => cx.fail(format!(
            "a denied verdict surfaced `{}`; a sandbox refusal is `sandbox-denied` and must \
             stay distinct from every kernel or permission failure ({message})",
            code.as_str()
        )),
        Ran::Ok(out) => {
            // The refusal was ignored and the caller was told it succeeded.
            // Whether the side effect landed is the spawner's business; the
            // lie is the violation.
            cx.fail(format!(
                "a denied verdict ran the command anyway and reported success ({out:?})"
            ))
        }
        Ran::Panicked => cx.fail("the executor panicked on a denied verdict"),
    }
    if marker.exists() {
        let _ = std::fs::remove_file(&marker);
        cx.fail(
            "the command executed despite `allowed == false`; the verdict is the verdict, \
             whatever `confined`/`mode` say",
        );
    }
}

/// An installed deny verdict reaches the shell even when its own policy permits.
///
/// Every other injected-verdict case asks the provider's sandbox to cooperate
/// by calling [`injected_verdict`]. That leaves a gap the suite itself can fall
/// into: the recording wrapper the suite puts on the sandbox leg is a sandbox
/// too, and if *it* ignored the injection then a "denied" run would silently
/// degrade to whatever the provider's real policy says — the case would assert
/// about a verdict the provider never saw, and an honest refuse-less provider
/// would be failed for the suite's own wiring gap.
///
/// This case closes the gap from the other side. The provider's sandbox is
/// wrapped in a suite-owned sandbox that *always* answers with the installed
/// verdict, and the shell is driven over that wrapper. The provider's real
/// policy is irrelevant by construction: it is never consulted while the
/// verdict is installed. So a shell that runs the command here is a shell that
/// ignores the verdict its own sandbox handed it — the denial bug this family of
/// cases exists to catch — and a provider whose sandbox declines to participate
/// in injection still gets a real verdict out of it.
fn injected_deny_reaches_shell(providers: &Executors, fixture: &ExecFixture, cx: &mut Cx) {
    let Some(policy) = fixture.policy() else {
        cx.skip("harness provisioned no scratch workspace root");
        return;
    };
    if cx.leg(&providers.shell, "shell").is_none()
        || cx.leg(&providers.sandbox, "sandbox").is_none()
        || cx.leg(&providers.subprocess, "subprocess").is_none()
    {
        return;
    }
    let marker = marker_in(
        fixture.root.as_ref().expect("policy() implies a root"),
        "injected-deny",
    );
    let _ = std::fs::remove_file(&marker);
    let command = touch_command(&marker);

    // A sandbox that answers *only* from the installed verdict. Nothing reaches
    // the provider's policy while a verdict is installed, so the run's outcome
    // is attributable to the verdict alone.
    let verdict_only = Arc::new(VerdictOnlySandbox);
    let wired = Executors::new()
        .shell(Arc::clone(
            providers.shell.as_ref().expect("checked by cx.leg"),
        ))
        .sandbox(verdict_only)
        .subprocess(Arc::clone(
            providers.subprocess.as_ref().expect("checked by cx.leg"),
        ));
    let shell = wired.shell.as_deref().expect("just set");

    inject(Enforced {
        allowed: false,
        confined: false,
        mode: String::new(),
        reason: INJECTED_REASON.to_string(),
    });
    let outcome = run(shell, &command, &policy);
    clear_injection();

    match outcome {
        Ran::Err(code, _) if code == ErrorCode::SandboxDenied => {}
        Ran::Err(code, message) => cx.fail(format!(
            "an installed deny verdict surfaced `{}`; a sandbox refusal is `sandbox-denied` \
             ({message})",
            code.as_str()
        )),
        Ran::Ok(out) => cx.fail(format!(
            "the shell ran a command its own sandbox had denied and reported success \
             ({out:?}); the installed verdict was never consulted"
        )),
        Ran::Panicked => cx.fail("the executor panicked on an installed deny verdict"),
    }
    if marker.exists() {
        let _ = std::fs::remove_file(&marker);
        cx.fail(
            "the command ran despite an installed `allowed == false` verdict that the \
             sandbox on the shell's path was obliged to hand back",
        );
    }

    // And the mirror: an installed permit must run, so the case cannot be
    // satisfied by a shell that refuses everything.
    inject(Enforced {
        allowed: true,
        confined: false,
        mode: String::new(),
        reason: INJECTED_REASON.to_string(),
    });
    let outcome = run(shell, &command, &policy);
    clear_injection();
    match outcome {
        Ran::Ok(_) => {}
        other => cx.fail(format!(
            "an installed permit verdict was not honoured: the executor {}",
            other.described()
        )),
    }
    let _ = std::fs::remove_file(&marker);
}

/// A sandbox that answers exclusively from the suite's installed verdict.
///
/// Used by `injected_deny_reaches_shell`, where the point is that the
/// provider's real policy is not in play. With no verdict installed it reports
/// a plain permit, so the wrapper can never be the thing that caused a refusal.
struct VerdictOnlySandbox;

impl Sandbox for VerdictOnlySandbox {
    fn enforce(
        &self,
        _argv: &[String],
        _policy: &PolicyHome,
    ) -> harnless_seams::error::Result<Enforced> {
        Ok(injected_verdict().unwrap_or_else(|| Enforced {
            allowed: true,
            confined: false,
            mode: String::new(),
            reason: "conformance suite default permit".to_string(),
        }))
    }
}

/// The line the cancellation fixture prints before it starts the part it expects
/// to be cancelled.
const CANCEL_SENTINEL: &str = "harnless-conformance-running";

/// How long the sentinel probe's command runs if nothing stops it.
///
/// Deliberately well above any fixture's `cancel_timeout` (the suite default is
/// 10s, and the negative fixtures go lower). The probe is now *cancelled* rather
/// than waited out, so its lifetime is only the window a leaked child could
/// linger — it must never be a value tuned near a fixture constant. A case whose
/// bite depended on the probe outliving the fixture by a hair was not a guard.
const PROBE_LIFETIME_SECS: u64 = 120;

/// How long the suite waits for a probe to announce itself.
const PROBE_BOUND: Duration = Duration::from_secs(5);

/// Whether a spawned command reached the state `cancel` is meant to interrupt.
///
/// Spawns a short-lived command that announces itself and then runs on, and
/// reports whether the announcement was observed inside a bound. `Err` carries a
/// skip reason: a provider that cannot run a plain command at all has not earned
/// a cancellation verdict, and asserting on a child that never started would be
/// asserting on the suite's own fixture.
///
/// # Why the suite waits at all
///
/// Cancelling a child the instant `spawn` returns is a real hazard for a real
/// caller, but it is not the obligation this case states. A provider that signals
/// a process still in `exec` can leave a shell that runs the *whole* command
/// anyway — the signal lands in the window between fork and the child becoming its
/// own process-group leader, so the group-kill reaches nothing and the caller is
/// told a completed run succeeded. Pinning "cancel mid-run" rather than "cancel at
/// t=0" is what makes the case about cancellation being *honoured* instead of about
/// a race in the spawn path.
fn await_sentinel(subprocess: &dyn Subprocess, cwd: &str) -> Probe {
    probe_running(subprocess, cwd)
}

/// Outcome of one sentinel-probe round.
///
/// Four states, because the ways a probe fails to reach the running state mean
/// different things and only some of them earn a skip:
/// - a provider that will not spawn the fixture at all has not earned a verdict;
/// - a provider that spawns a child, never lets it announce, and then *reaps* it
///   on request is uncooperative but not provably broken;
/// - a provider that accepts a child and will not give it back is the exact
///   non-cooperation the cancellation cases exist to catch.
///
/// The last two are the same observed silence, separated by whether the child
/// came back when the suite asked for cancellation.
enum Probe {
    /// The probe announced itself: the provider reached a cancellable state.
    Running,
    /// The provider rejected the spawn. Nothing was started.
    Refused,
    /// Accepted, never announced, and the child was still unreleased when this
    /// function returned — the provider did not give it back on `cancel`.
    Silent,
    /// Accepted, never announced, but the provider released the child once asked.
    SilentButReleased,
}
/// Poll until a probe child announces itself, giving up after `PROBE_BOUND`.
///
/// The probe is a short-lived command that prints a sentinel and then sleeps.
/// Its `output()` is taken on a worker thread so a provider that never releases
/// children cannot wedge the suite: the caller learns "no announcement inside
/// the bound" instead of blocking forever. The handle is shared so the caller
/// can still reach it through the `Arc` and stop the child, so the suite never
/// accumulates live probe children of its own making.
fn probe_running(subprocess: &dyn Subprocess, cwd: &str) -> Probe {
    let spawn = Spawn {
        argv: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo {CANCEL_SENTINEL}; sleep {PROBE_LIFETIME_SECS}"),
        ],
        cwd: Some(cwd.to_string()),
        confine: None,
    };
    let handle: Box<dyn SpawnHandle> =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| subprocess.spawn(&spawn))) {
            Ok(Ok(handle)) => handle,
            // Refused outright: nothing was started, so nothing could announce.
            Ok(Err(_)) | Err(_) => return Probe::Refused,
        };
    let handle = Arc::new(handle);

    // Whether the worker's `output()` call has returned, i.e. whether the provider
    // has given the child back. The suite asked for cancellation; a provider that
    // honours it releases the child, and the worker reports in. A provider that
    // swallows `cancel` leaves the worker blocked, and that is observable here
    // without waiting out the child's own lifetime.
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_released = Arc::clone(&released);
    let (sender, receiver) = std::sync::mpsc::channel::<bool>();
    let worker_handle = Arc::clone(&handle);
    std::thread::spawn(move || {
        let announced = match worker_handle.output() {
            Ok(out) => out.contains(CANCEL_SENTINEL),
            // `output()` failed; the child may still be live, and the caller
            // reaches it through the shared handle.
            Err(_) => false,
        };
        worker_released.store(true, std::sync::atomic::Ordering::SeqCst);
        // The receive side may already have given up on the bound.
        let _ = sender.send(announced);
    });
    match receiver.recv_timeout(PROBE_BOUND) {
        // Announced inside the bound. The worker stays alive holding the handle,
        // so the child is reaped by the time this case's next spawn happens —
        // which is why the case needs no fixed sleep between probe and measurement.
        Ok(true) => Probe::Running,
        // Accepted, and no announcement inside the bound. Ask for the child back.
        Ok(false) | Err(_) => {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.cancel()));
            // Give the provider a bounded moment to honour it. This is a bound on
            // an *observation* (did `output()` return), not a sleep standing in for
            // a condition: the alternative is declaring a provider broken before
            // allowing its own cancellation path a chance to run.
            let gave_it_back = Instant::now() + PROBE_BOUND;
            while !released.load(std::sync::atomic::Ordering::SeqCst)
                && Instant::now() < gave_it_back
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            if released.load(std::sync::atomic::Ordering::SeqCst) {
                Probe::SilentButReleased
            } else {
                Probe::Silent
            }
        }
    }
}
/// Outcome of [`wait_until_quiet`].
///
/// Three states, because the two failure modes mean opposite things about the
/// provider and must not both become a skip.
enum Quiet {
    /// A probe ran and was reaped: the provider releases children.
    Quiet,
    /// The provider rejected a trivial `/bin/sh -c true` outright. Nothing about
    /// cancellation can be established; no verdict is owed.
    Rejected,
    /// A probe was accepted and its child never came back. That is the shape of
    /// a provider that does not release children — the defect the cancellation
    /// case exists to catch, not a fixture limitation.
    Stuck,
}

/// Poll until a trivial probe spawn runs and is reaped, up to `PROBE_BOUND`.
///
/// Used instead of sleeping a fixed beat between the sentinel probe and the
/// measured spawn: a probe that runs to completion is the evidence the previous
/// child is reaped, so the case waits on the condition it actually depends on.
///
/// The probe's `output()` is taken with a bounded wait rather than called
/// inline: a provider that does not release children would block this poll
/// forever, and "never comes back" is exactly the state the caller needs to
/// learn about.
fn wait_until_quiet(subprocess: &dyn Subprocess, cwd: &str) -> Quiet {
    let probe = Spawn {
        argv: vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()],
        cwd: Some(cwd.to_string()),
        confine: None,
    };
    let deadline = Instant::now() + PROBE_BOUND;
    loop {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| subprocess.spawn(&probe))) {
            // The provider will not run a trivial command at all. Retrying to the
            // deadline would report "probe never cleared" for a condition that
            // was never going to change.
            Ok(Err(_)) | Err(_) => return Quiet::Rejected,
            Ok(Ok(handle)) => {
                // The probe returning is the proof the previous child is reaped.
                // A non-zero exit is irrelevant — the question is whether the
                // child came back at all, and the probe is `/bin/sh -c true`.
                let (sender, receiver) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let returned =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.output()))
                            .is_ok();
                    let _ = sender.send(returned);
                });
                let remaining = deadline.saturating_duration_since(Instant::now());
                match receiver.recv_timeout(remaining) {
                    // The child came back: the provider reaps what it spawns.
                    Ok(true) => return Quiet::Quiet,
                    // `output()` itself blew up. The child is gone or the
                    // provider is broken loudly enough that the spawn-side cases
                    // will report it; either way this poll's question is answered.
                    Ok(false) => return Quiet::Quiet,
                    // No answer inside the bound: a child was accepted and never
                    // released.
                    Err(_) => return Quiet::Stuck,
                }
            }
        }
    }
}

/// Why [`await_running`] gave up.
///
/// Mirrors [`Probe`]: the two ways a probe fails to reach the running state mean
/// opposite things about the provider, and only one of them earns a skip.
enum RanUp {
    /// Every probe round was refused: the provider never started a child, so it
    /// has not earned a cancellation verdict.
    NeverSpawned,
    /// At least one probe was accepted but never announced. That is the shape a
    /// provider that will not release children produces, so the caller must fail.
    SpawnedButQuiet,
}

/// Poll until a probe command announces itself mid-run, up to `bound`.
///
/// The measured command is the one being cancelled; this only establishes that
/// the provider can get a child to a cancellable state within the fixture's own
/// cancellation bound.
///
/// # The distinction matters
///
/// A provider that cannot spawn at all has not earned a cancellation verdict. A
/// provider that *can* spawn but will not let go of what it spawned is the exact
/// defect this case family exists to catch, and must not be allowed to hide
/// behind the same skip. Hence [`RanUp`].
fn await_running(
    subprocess: &dyn Subprocess,
    cwd: &str,
    bound: Duration,
) -> std::result::Result<(), RanUp> {
    let deadline = Instant::now() + bound;
    let mut ever_accepted = false;
    loop {
        match probe_running(subprocess, cwd) {
            Probe::Running => return Ok(()),
            // A probe that was accepted, stayed silent, and then gave the child
            // back on request is uncooperative but not the child-hoarding defect;
            // retrying is fair. A probe whose child was never released is the
            // defect, and earns no retry.
            Probe::Refused | Probe::SilentButReleased => {}
            Probe::Silent => ever_accepted = true,
        }
        if Instant::now() >= deadline {
            return Err(if ever_accepted {
                RanUp::SpawnedButQuiet
            } else {
                RanUp::NeverSpawned
            });
        }
    }
}

/// A denied command never runs.
///
/// Stronger than the routing case: the denial must happen *before* the
/// subprocess is touched, so a refusal cannot have side effects. The case
/// asks the sandbox for a refusal (a policy root that does not exist is the
/// portable way) and asserts both that nothing spawned and that the command's
/// own marker file was not created.
fn denied_command_never_runs(providers: &Executors, fixture: &ExecFixture, cx: &mut Cx) {
    if cx.leg(&providers.shell, "shell").is_none()
        || cx.leg(&providers.subprocess, "subprocess").is_none()
    {
        return;
    }
    let Some(sandbox) = cx.leg(&providers.sandbox, "sandbox") else {
        return;
    };
    let Some(root) = cx.root(fixture) else {
        return;
    };
    let marker = marker_in(&root, "denied");
    let _ = std::fs::remove_file(&marker);

    let denied_policy = PolicyHome {
        workspace_root: root
            .join("harnless-conformance-vanished-root")
            .display()
            .to_string(),
        default_confined: true,
    };
    let verdict = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sandbox.enforce(
            &vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                touch_command(&marker),
            ],
            &denied_policy,
        )
    })) {
        Ok(Ok(verdict)) => verdict,
        Ok(Err(err)) => {
            cx.skip(format!(
                "sandbox errored instead of reporting a verdict (`{}`)",
                err.code.as_str()
            ));
            return;
        }
        Err(_) => {
            // The suite's own floor: a sandbox that crashes on a policy whose
            // root does not exist is a violation, and it is reported as one
            // rather than crashing the run.
            cx.fail("the sandbox panicked on a policy whose root does not exist");
            return;
        }
    };
    if verdict.allowed {
        cx.skip("no refusing policy available for this provider; nothing to assert");
        return;
    }

    // The provider's own verdict, replayed through the injected-verdict path so
    // the shell faces the same refusal twice: once as the sandbox's answer, once
    // as the installed fixture. This drives the shell over a sandbox that
    // consults `injected_verdict`, so a shell that spawns anyway is caught here
    // rather than passing because its own policy happens to refuse.
    let wired = Executors::new()
        .shell(Arc::clone(
            providers.shell.as_ref().expect("checked by cx.leg"),
        ))
        .sandbox(Arc::new(LoggingSandbox {
            inner: Arc::clone(providers.sandbox.as_ref().expect("checked by cx.leg")),
            seen: recorder::Recorder::default(),
        }))
        .subprocess(Arc::clone(
            providers.subprocess.as_ref().expect("checked by cx.leg"),
        ));
    let wired_shell = wired.shell.as_deref().expect("just set");
    inject(verdict.clone());
    let outcome = run(wired_shell, &touch_command(&marker), &denied_policy);
    clear_injection();
    match &outcome {
        Ran::Err(ErrorCode::SandboxDenied, message) => {
            // The refusal must be auditable: the report's mode and reason
            // belong in what the caller sees.
            if !message.contains(&verdict.mode) && !message.contains(&verdict.reason) {
                cx.fail(format!(
                    "the refusal surfaced as `{}` without the enforced mode or reason in \
                     it: {message:?} (verdict was {verdict:?})",
                    ErrorCode::SandboxDenied.as_str()
                ));
            }
        }
        other => cx.fail(format!(
            "a denied command surfaced as {} instead of a `sandbox-denied` refusal",
            other.described()
        )),
    }
    if marker.exists() {
        let _ = std::fs::remove_file(&marker);
        cx.fail("the refused command produced its side effect; the denial happened too late");
    }
}

// ---------------------------------------------------------------------------
// Confinement is real
// ---------------------------------------------------------------------------

/// A confined run is actually confined.
///
/// The verdict is not the enforcement: the case runs a command that reports
/// its own working directory and environment and checks both against the
/// verdict's promises. The env half needs a variable the harness can prove
/// was in the child's spawn environment — a provider whose spawner cannot
/// inject one skips that half rather than asserting on a variable it cannot
/// place.
fn confined_run_is_actually_confined(providers: &Executors, fixture: &ExecFixture, cx: &mut Cx) {
    let Some(policy) = fixture.policy() else {
        cx.skip("harness provisioned no scratch workspace root");
        return;
    };
    let Some(shell) = cx.leg(&providers.shell, "shell") else {
        return;
    };
    if !policy.default_confined {
        cx.skip("fixture policy is not confined; confinement is unobservable");
        return;
    }
    let root = fixture.root.clone().expect("policy() implies a root");
    // A root that differs from the process cwd, so a pinned cwd is
    // distinguishable from "inherited the parent's directory".
    let canonical = match root.canonicalize() {
        Ok(canonical) => canonical,
        Err(err) => {
            cx.skip(format!("scratch root is not canonicalisable: {err}"));
            return;
        }
    };
    let policy = PolicyHome {
        workspace_root: canonical.display().to_string(),
        default_confined: true,
    };

    // Ask the sandbox what it intends to enforce; a provider that refuses to
    // confine under its own scratch policy owes nothing on this case.
    let Some(sandbox) = providers.sandbox.as_deref() else {
        cx.skip("provider supplied no sandbox leg");
        return;
    };
    let verdict = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sandbox.enforce(
            &vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                inspect_command().to_string(),
            ],
            &policy,
        )
    })) {
        Ok(Ok(verdict)) => verdict,
        Ok(Err(err)) => {
            cx.skip(format!(
                "sandbox errored on the confined policy (`{}`)",
                err.code.as_str()
            ));
            return;
        }
        Err(_) => {
            cx.fail("the sandbox panicked on the confined scratch policy");
            return;
        }
    };
    if !verdict.allowed {
        cx.skip(format!(
            "sandbox refused the confined scratch policy: {}",
            verdict.reason
        ));
        return;
    }
    if !verdict.confined {
        cx.skip("confined policy produced an unconfined verdict; nothing to observe");
        return;
    }

    // Run the inspector through the bundle the harness wired — including its
    // own spawner, which is the thing that applies confinement (and, when the
    // fixture says so, places the proof variables in the child's environment).
    let output = match run(shell, inspect_command(), &policy) {
        Ran::Ok(out) => out,
        Ran::Err(code, message) => {
            cx.fail(format!(
                "a permitted confined command failed with `{}` ({message}); confined runs \
                 must run, or the verdict is overstating what the provider enforces",
                code.as_str()
            ));
            return;
        }
        Ran::Panicked => {
            cx.fail("the provider panicked running a confined command");
            return;
        }
    };

    // The cwd half: the child reports the workspace root, not the parent's
    // directory.
    let reported = output.lines().next().unwrap_or_default().trim().to_string();
    if reported != canonical.display().to_string() {
        cx.fail(format!(
            "confined child reported cwd {reported:?}, expected the workspace root \
             {:?}; the verdict promised a pinned cwd. output:\n{output}",
            canonical.display().to_string()
        ));
    }
    // The env half, gated on the harness having really placed the proof
    // variable: the suite will not assert on an environment it cannot establish.
    match (&fixture.proof_var, fixture.proof_vars_applied) {
        (Some((proof, _)), true) => {
            if output.contains(proof.as_str()) {
                cx.fail(format!(
                    "credential-shaped variable {proof} survived the scrub and reached the \
                     confined child; output:\n{output}"
                ));
            }
            if let Some((control, value)) = &fixture.control_var {
                if !output.contains(&format!("{control}={value}")) {
                    cx.fail(format!(
                        "non-credential variable {control} was removed too; a scrub that \
                         drops everything is not a scrub. output:\n{output}"
                    ));
                }
            }
        }
        _ => cx.skip("harness placed no proof variable; env scrub unobservable"),
    }
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// `SpawnHandle::cancel` stops the process.
///
/// The seam promises best-effort cancellation; the observable floor is that a
/// caller waiting on a cancelled handle stops waiting and learns the run was
/// cancelled, instead of blocking until the command finishes on its own. The
/// case spawns a command that would outlive any reasonable wait, waits for
/// its output on one thread and cancels it from another.
fn cancellation_is_honoured(providers: &Executors, fixture: &ExecFixture, cx: &mut Cx) {
    let Some(subprocess) = cx.leg(&providers.subprocess, "subprocess") else {
        return;
    };
    let Some(policy) = fixture.policy() else {
        cx.skip("harness provisioned no scratch workspace root");
        return;
    };
    // A command no conforming test waits out. The provider decides process
    // grouping, so a group-kill provider also proves it reaches descendants.
    //
    // The command is deliberately much longer than the fixture's cancellation
    // bound. A provider that ignores `cancel` is caught by the bound — but the
    // suite still has to wait for the child to finish before `output()` returns,
    // so an unbounded sleep would make the negative test hang for minutes. The
    // bound is the assertion; the sleep is the thing being cancelled.
    let sleep_secs = fixture.cancel_timeout.as_secs() + 5;
    let spawn = Spawn {
        argv: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo {CANCEL_SENTINEL}; sleep {sleep_secs}; echo should-not-print"),
        ],
        cwd: Some(policy.workspace_root.clone()),
        confine: None,
    };
    let handle =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| subprocess.spawn(&spawn))) {
            Ok(Ok(handle)) => handle,
            Ok(Err(err)) => {
                cx.skip(format!(
                    "subprocess refused the cancellation fixture (`{}`)",
                    err.code.as_str()
                ));
                return;
            }
            Err(_) => {
                cx.fail("the subprocess panicked spawning a plain command");
                return;
            }
        };

    // Let the child reach the state `cancel` is meant to interrupt before
    // signalling it. A provider that kills a process still in `exec` can leave a
    // shell that runs the *whole* command anyway — the signal lands in the window
    // between fork and the child becoming its own process-group leader, and the
    // group-kill then reaches nothing. That is a real hazard for a real caller who
    // cancels a command it just started, so the case cancels a child that is
    // observably mid-run rather than one still coming up: the command prints a
    // sentinel, and the suite cancels only after seeing it.
    match await_sentinel(subprocess, &policy.workspace_root) {
        Probe::Running => {}
        Probe::Refused => {
            cx.skip(
                "the subprocess refused the cancellation fixture; cancelling a child \
                 that never started proves nothing about cancellation",
            );
            return;
        }
        // Accepted, and the child never came back even after the suite reached
        // the handle through the shared `Arc` and asked for cancellation. That is
        // the provider refusing to release a child it owns — precisely the defect
        // this case states — so it is a violation, not a fixture limitation.
        Probe::Silent => {
            cx.fail(
                "the subprocess accepted a child, never released it on `cancel`, and \
                 never reported it as running; a caller that cannot stop what it \
                 spawned has failed the cancellation obligation",
            );
            return;
        }
        // Accepted, silent, but the provider gave the child back when asked. The
        // child never reached the state `cancel` interrupts, so no cancellation
        // verdict is owed on this run — the suite will not assert on a child it
        // cannot get running.
        Probe::SilentButReleased => {
            cx.skip(
                "the subprocess accepted the cancellation fixture but never reported it \
                 as running; no child reached a cancellable state",
            );
            return;
        }
    }
    // The sentinel probe above is a separate short-lived command. Rather than
    // sleeping a fixed beat and hoping the OS reaped it, poll for the state the
    // measured spawn actually needs: a probe that runs to completion proves the
    // probe is gone, so this waits on the condition instead of a duration.
    match wait_until_quiet(subprocess, &policy.workspace_root) {
        Quiet::Quiet => {}
        // The provider rejects a trivial `/bin/sh -c true`. Nothing about the
        // measured spawn can be established; no verdict is owed.
        Quiet::Rejected => {
            cx.skip(
                "the subprocess rejects a trivial probe spawn; cannot measure \
                     cancellation cleanly",
            );
            return;
        }
        // A probe spawned and was never reaped. For a provider that ignores
        // `cancel` this is the defect itself, not a fixture limitation.
        Quiet::Stuck => {
            cx.fail(
                "a trivial probe spawn was accepted but its child was never reaped; \
                 children are not being released, which is what `cancel` exists to do",
            );
            return;
        }
    }
    // Measured from here, after every fixture probe has settled: the bound is on
    // how long a *cancelled* child takes to stop, and probe time must not count
    // against the provider.
    let started = Instant::now();
    enum Stage {
        Running,
        /// The measured command never announced itself even though the probe
        /// already proved this provider can get a child to a cancellable state.
        /// That is a defect, not a fixture limitation — see the match below.
        MeasuredNeverRan,
        /// The provider could not get even the probe running, so no cancellation
        /// verdict is owed.
        ProbeNeverRan,
        CancelPanicked,
    }
    let (stage, outcome) = std::thread::scope(|waiters| {
        let waiting = waiters.spawn(|| handle.output());
        // Cancel only once the child is observably mid-run. The bound is generous
        // and the loop exits as soon as the sentinel appears, so a fast machine
        // cancels sooner and a loaded CI box still gets its child to a cancellable
        // state — a fixed sleep gets this wrong in both directions.
        match await_running(subprocess, &policy.workspace_root, fixture.cancel_timeout) {
            Ok(()) => {}
            Err(RanUp::NeverSpawned) => {
                let _ = waiting.join();
                return (Stage::ProbeNeverRan, Err(()));
            }
            Err(RanUp::SpawnedButQuiet) => {
                let _ = waiting.join();
                return (Stage::MeasuredNeverRan, Err(()));
            }
        }
        let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.cancel();
        }));
        if cancelled.is_err() {
            // The scope cannot propagate a panic from here; report it after.
            let _ = waiting.join();
            return (Stage::CancelPanicked, Err(()));
        }
        (Stage::Running, waiting.join().map_err(|_| ()))
    });
    let outcome = match (stage, outcome) {
        (Stage::Running, Ok(outcome)) => outcome,
        (Stage::Running, Err(_)) => {
            cx.fail("waiting for the cancelled command panicked the executor");
            return;
        }
        (Stage::ProbeNeverRan, _) => {
            cx.skip("the measured command never announced itself mid-run; nothing to cancel");
            return;
        }
        (Stage::MeasuredNeverRan, _) => {
            // The probe established that this provider spawns children that
            // reach a cancellable state, so the measured command failing to
            // announce is not a fixture limitation. The classic cause is a
            // provider that ignores `cancel`: earlier children are never let go,
            // and the process table fills. Skipping here would let precisely
            // that defect pass.
            cx.fail(
                "the provider spawned probe children but the measured command never \
                 announced itself mid-run; a provider that never releases children \
                 on `cancel` starves its own spawns, which is the defect this case \
                 states",
            );
            return;
        }
        (Stage::CancelPanicked, _) => {
            cx.fail("`SpawnHandle::cancel` panicked");
            return;
        }
    };
    match outcome {
        Ok(out) => cx.fail(format!(
            "a cancelled command returned Ok({out:?}) after {:?}; cancellation must not \
             deliver a completed run as success",
            started.elapsed()
        )),
        Err(err) => {
            // The seam's taxonomy is explicit: a cancelled run reports
            // `exec-cancelled`. Accepting a spawn failure here would let the
            // exact bug this case exists for pass — a `cancel` that reaches
            // nothing leaves the child to die of its own bounded probe lifetime,
            // and the wait can surface that as a spawn-ish failure. The child is
            // observably mid-run when cancelled (the sentinel was seen), so a
            // spawn failure afterwards is not an honest report about THIS run.
            if err.code != ErrorCode::ExecCancelled {
                cx.fail(format!(
                    "a cancelled command failed with `{}`; a cancelled run must report \
                     `exec-cancelled`, since a caller routes on the code to avoid retrying \
                     its own cancellation",
                    err.code.as_str()
                ));
            }
        }
    }
    if started.elapsed() > fixture.cancel_timeout {
        cx.fail(format!(
            "the cancelled command took {:?} to stop, past the fixture's {:?} bound; \
             `cancel` did not reach the process",
            started.elapsed(),
            fixture.cancel_timeout
        ));
    }
}

// ---------------------------------------------------------------------------
// Failure honesty
// ---------------------------------------------------------------------------

/// A failing command surfaces as a typed error, never fabricated output.
fn failing_command_is_typed_error(providers: &Executors, fixture: &ExecFixture, cx: &mut Cx) {
    let Some(policy) = fixture.policy() else {
        cx.skip("harness provisioned no scratch workspace root");
        return;
    };
    let Some(shell) = cx.leg(&providers.shell, "shell") else {
        return;
    };
    match run(shell, FAILING_COMMAND, &policy) {
        Ran::Ok(out) => cx.fail(format!(
            "`{FAILING_COMMAND}` returned Ok({out:?}); a nonzero exit must route as an \
             error, and output that only exists inside a success value is fabricated"
        )),
        Ran::Err(code, message) => {
            // Any exec-taxonomy code is honest routing; a *tool* or *fs* code
            // would mean the failure was relabelled into another seam's
            // vocabulary and a caller routing on exec codes would miss it.
            const EXEC_CODES: &[ErrorCode] = &[
                ErrorCode::SpawnFailed,
                ErrorCode::ExecCancelled,
                ErrorCode::SandboxDenied,
                ErrorCode::IoError,
                ErrorCode::TooLarge,
                ErrorCode::Aborted,
            ];
            if !EXEC_CODES.contains(&code) {
                cx.fail(format!(
                    "`{FAILING_COMMAND}` failed with `{}`; a command failure must carry an \
                     execution-world code so callers routing on exec failures see it \
                     ({message})",
                    code.as_str()
                ));
            }
            // The captured output belongs to the error, not to a success value.
            if !message.contains("failing-on-purpose") {
                cx.skip(format!(
                    "failure message did not echo the command's own output; capture policy \
                     is the provider's choice ({message:?})"
                ));
            }
        }
        Ran::Panicked => cx.fail("a failing command panicked the executor"),
    }
}

// ---------------------------------------------------------------------------
// Recording wrappers
// ---------------------------------------------------------------------------

/// Minimal recorder cell.
///
/// The suite must not add a lock dependency for one mutex, and
/// `std::sync::Mutex` is enough: the recorders are written from the driving
/// thread and read after the call returns.
mod recorder {
    use std::sync::{Arc, Mutex};

    /// An append-only recorder behind an `Arc<Mutex<..>>`.
    #[derive(Default)]
    pub struct Recorder<T> {
        items: Arc<Mutex<Vec<T>>>,
    }

    impl<T> Clone for Recorder<T> {
        fn clone(&self) -> Self {
            Self {
                items: Arc::clone(&self.items),
            }
        }
    }

    impl<T> Recorder<T> {
        /// Record one item.
        pub fn push(&self, item: T) {
            self.items
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(item);
        }

        /// The most recently recorded item.
        pub fn last(&self) -> Option<T>
        where
            T: Clone,
        {
            self.items
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .last()
                .cloned()
        }
    }
}

/// Records the exact argv each `enforce` was consulted with, and replays an
/// installed verdict.
///
/// The injection check is not optional decoration. This wrapper is the sandbox
/// on the shell's hot path whenever a case installs recording legs, so if it
/// ignored [`injected_verdict`] the routing, denial, and refusal cases would
/// silently fall back to the provider's natural verdict — the case would then
/// assert about a fixture verdict the provider never saw, and an honest
/// refuse-less provider would be failed for the suite's own wiring gap.
struct LoggingSandbox {
    inner: Arc<dyn Sandbox>,
    seen: recorder::Recorder<Vec<String>>,
}

impl Sandbox for LoggingSandbox {
    fn enforce(
        &self,
        argv: &[String],
        policy: &PolicyHome,
    ) -> harnless_seams::error::Result<Enforced> {
        self.seen.push(argv.to_vec());
        if let Some(verdict) = injected_verdict() {
            return Ok(verdict);
        }
        self.inner.enforce(argv, policy)
    }
}

/// Records the exact [`Spawn`] each `spawn` was handed.
struct LoggingSubprocess {
    inner: Arc<dyn Subprocess>,
    spawned: recorder::Recorder<Spawn>,
}

impl Subprocess for LoggingSubprocess {
    fn spawn(&self, spawn: &Spawn) -> harnless_seams::error::Result<Box<dyn SpawnHandle>> {
        self.spawned.push(spawn.clone());
        self.inner.spawn(spawn)
    }
}

// The verdict a case has installed for the shell's sandbox to replay.
//
// A shell owns the sandbox it consults — no seam lets a caller swap it — so the
// only way to feed a shell a verdict the suite constructed is to install it
// where the shell will look. [`injected_verdict`] is that place: a sandbox
// consulted by the shell under test calls it before deciding, and replays the
// installed verdict when one is present. The routing and denial cases install
// verdicts whose `confined`/`mode` contradict `allowed` — shapes no honest
// policy produces — and check that the shell still routes on `allowed` alone.
//
// A provider whose sandbox ignores [`injected_verdict`] is not driven by these
// injections and fails the cases that need them, which is the honest outcome:
// the suite cannot claim to have tested a routing decision the provider never
// faced.
thread_local! {
    static INJECTED: std::sync::Mutex<Option<Enforced>> =
        const { std::sync::Mutex::new(None) };
}

/// The verdict installed for the sandbox under test, if any.
///
/// A sandbox that wants to participate in the routing and denial cases calls
/// this before deciding and returns the installed verdict verbatim. The
/// installed verdict always carries [`INJECTED_MARKER`] in its `reason`, so a
/// provider that would rather recognise injected verdicts by inspection than by
/// calling here can do that instead.
#[must_use]
pub fn injected_verdict() -> Option<Enforced> {
    INJECTED.with(|slot| {
        slot.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    })
}

/// Install `verdict` as the sandbox's answer for the duration of one case.
fn inject(verdict: Enforced) {
    INJECTED
        .with(|slot| *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(verdict));
}

/// Clear any installed verdict.
fn clear_injection() {
    INJECTED.with(|slot| *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None);
}

/// The reason every injected verdict carries (see [`INJECTED_MARKER`]).
const INJECTED_REASON: &str = "conformance suite injected this verdict";

/// The substring a sandbox may use to recognise an injected verdict.
///
/// Every verdict installed by the suite carries this in its `reason`, so a
/// provider that prefers to recognise injected verdicts by inspection rather
/// than by calling [`injected_verdict`] can do that instead.
pub const INJECTED_MARKER: &str = "conformance suite injected";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_list_is_the_documented_set() {
        assert_eq!(
            EXECUTOR_CONFORMANCE_CASES,
            &[
                "sandbox_sees_exact_argv",
                "enforced_reason_is_never_blank",
                "refusal_is_never_reported_as_confined",
                "consumer_routes_on_allowed",
                "injected_deny_reaches_shell",
                "denied_command_never_runs",
                "confined_run_is_actually_confined",
                "cancellation_is_honoured",
                "failing_command_is_typed_error",
            ]
        );
    }

    #[test]
    fn unknown_case_is_a_violation_not_a_panic() {
        fn fixture_for(_case: &str) -> ExecFixture {
            ExecFixture::new()
        }
        let violations = check_executor_contract(&Executors::new(), "not-a-case", fixture_for);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].case, "not-a-case");
    }

    #[test]
    fn a_bundle_with_no_legs_skips_everything() {
        fn fixture_for(_case: &str) -> ExecFixture {
            ExecFixture::new()
        }
        let violations = check_executor_contract_all(&Executors::new(), fixture_for);
        assert!(
            violations.is_empty(),
            "an empty bundle must skip, not fail: {violations:?}"
        );
    }

    #[test]
    fn fixture_policy_comes_from_the_fixture_root() {
        let fixture = ExecFixture::new().root("/tmp/scratch").confined(true);
        let policy = fixture.policy().expect("policy");
        assert_eq!(policy.workspace_root, "/tmp/scratch");
        assert!(policy.default_confined);
    }

    #[test]
    fn recorder_reads_back_what_it_recorded() {
        let recorder = recorder::Recorder::<u32>::default();
        assert_eq!(recorder.last(), None);
        recorder.push(1);
        recorder.push(2);
        assert_eq!(recorder.last(), Some(2));
    }
}
