//! Cross-provider tests for the execution world: one [`PolicyHome`] must
//! drive both the fs side and the exec side, and the executor contract
//! (exact-argv handover, auditable refusals) is conformed to by hand here
//! until the conformance kit's `check_executor` mirrors these assertions.
//!
//! The fs stand-in below is intentionally the thinnest thing that proves the
//! invariant: when `harnless-fs-local` lands, its provider drops in and this
//! test keeps its shape.

use std::path::{Path, PathBuf};

use harnless_exec_sandbox::{LocalSandbox, PolicyHomeExt, SandboxDecision, SandboxMode};
use harnless_seams::error::Result;
use harnless_seams::exec::{Enforced, PolicyHome, Sandbox};

/// The fs-side consumer: resolves a requested path against the *same*
/// policy home and refuses anything escaping the workspace root.
struct FsConfined {
    policy: PolicyHome,
}

impl FsConfined {
    fn new(policy: &PolicyHome) -> Self {
        Self {
            policy: policy.clone(),
        }
    }

    /// Resolve `requested` or refuse it as outside the confined root.
    fn resolve(&self, requested: &Path) -> Result<PathBuf> {
        use harnless_seams::error::{ErrorCode, SeamError};
        let root = Path::new(&self.policy.workspace_root)
            .canonicalize()
            .map_err(|err| SeamError::new(ErrorCode::IoError, err.to_string()))?;
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            root.join(requested)
        };
        // Lexical containment is enough for the stand-in; the point is the
        // root identity, not path hardening.
        if joined.starts_with(&root) {
            Ok(joined)
        } else {
            Err(SeamError::new(
                ErrorCode::SandboxDenied,
                format!("{:?} escapes workspace root {:?}", joined, root),
            ))
        }
    }
}

fn decisions(sandbox: &LocalSandbox, policy: &PolicyHome) -> SandboxDecision {
    sandbox.enforce_decision(["/bin/bash".to_string()].as_slice(), policy)
}

#[test]
fn one_policy_home_drives_both_fs_and_exec_providers() {
    let root = std::env::temp_dir();
    let root = root.canonicalize().expect("canonical temp dir");
    let policy = PolicyHome::from_root_and_mode(&root, SandboxMode::SandboxLocal);

    // Exec side: the sandbox verdict names *this* root as the pinned cwd.
    let decision = decisions(&LocalSandbox::new(), &policy);
    assert!(decision.allowed);
    assert_eq!("sandbox-local", decision.enforced.mode);
    assert!(
        decision.reason.contains(&root.display().to_string()),
        "exec verdict confined to a different root than the policy declares: {}",
        decision.reason
    );

    // Fs side: the same value confines path resolution to the same root.
    let fs = FsConfined::new(&policy);
    assert!(fs.resolve(Path::new("inside/file.txt")).is_ok());
    let escaped = fs.resolve(Path::new("/etc/passwd"));
    assert!(
        matches!(&escaped, Err(err) if err.code == harnless_seams::error::ErrorCode::SandboxDenied),
        "fs side confined to a different root: {escaped:?}"
    );
}

#[test]
fn policy_home_mode_round_trips_through_the_seam_boolean() {
    let unconfined = PolicyHome::from_root_and_mode("/tmp", SandboxMode::Unconfined);
    assert!(!unconfined.default_confined);
    assert_eq!(SandboxMode::Unconfined, unconfined.default_mode());

    let confined = PolicyHome::from_root_and_mode("/tmp", SandboxMode::SandboxLocal);
    assert!(confined.default_confined);
    assert_eq!(SandboxMode::SandboxLocal, confined.default_mode());

    // deny-all is confinement too — it must not read as "unconfined".
    let denied = PolicyHome::from_root_and_mode("/tmp", SandboxMode::DenyAll);
    assert!(denied.default_confined);
}

#[test]
fn sandbox_local_reports_exactly_what_it_enforced() {
    let policy = PolicyHome::from_root_and_mode(std::env::temp_dir(), SandboxMode::SandboxLocal);
    let decision = decisions(&LocalSandbox::new(), &policy);
    assert!(decision.allowed);
    assert!(decision.enforced.confined);
    // Honest reporting: the reason states the best-effort guarantees AND
    // that they are not kernel enforcement.
    assert!(decision.reason.contains("best-effort"));
    assert!(decision.reason.contains("NOT kernel-enforced"));
}

#[test]
fn refusal_produces_a_distinguishable_enforced_result_never_a_quiet_pass() {
    // The confined home's boolean cannot name deny-all, so the refusal is
    // produced by an explicit deny-all sandbox over it — the combination a
    // host policy uses when it requires a real sandbox this host lacks.
    let confined = PolicyHome::from_root_and_mode(std::env::temp_dir(), SandboxMode::SandboxLocal);
    let unconfined = PolicyHome {
        workspace_root: std::env::temp_dir().display().to_string(),
        default_confined: false,
    };
    let denied = LocalSandbox::with_mode(SandboxMode::DenyAll)
        .enforce_decision(["/bin/bash".to_string()].as_slice(), &confined);
    let passed = LocalSandbox::new()
        .enforce_decision(["/bin/bash".to_string()].as_slice(), &unconfined);

    // The refusal is explicit and auditable.
    assert!(!denied.allowed);
    assert!(!denied.reason.is_empty());
    // And its seam shape is distinguishable from a permitted run: a
    // converter that only sees `Enforced` can still tell them apart.
    let denied_enforced: Enforced = denied.into_enforced();
    let passed_enforced: Enforced = passed.into_enforced();
    assert_ne!(denied_enforced, passed_enforced);
    assert!(!denied_enforced.confined, "a refusal must not look confined");
    assert_eq!("deny-all", denied_enforced.mode);
}

#[test]
fn seam_enforce_and_decision_agree() {
    // The seam conversion is total: enforce() == enforce_decision().into().
    let policy = PolicyHome::from_root_and_mode(std::env::temp_dir(), SandboxMode::SandboxLocal);
    let sandbox = LocalSandbox::new();
    let argv = vec!["/bin/bash".to_string(), "-c".to_string(), "true".to_string()];
    let via_seam: Enforced = sandbox.enforce(&argv, &policy).expect("decide");
    assert_eq!(
        via_seam,
        decisions(&LocalSandbox::new(), &policy).into_enforced()
    );
}
