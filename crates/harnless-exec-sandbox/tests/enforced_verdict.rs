//! The auditable refusal shape lives on the seam.
//!
//! A refusal is not a shape a consumer has to *infer* from `confined` and
//! `mode` — the seam [`Enforced`] carries the verdict (`allowed`) and the
//! human-auditable `reason` directly, so any consumer routes on
//! `enforced.allowed` and an auditor can read the verdict off the exact
//! struct the executor consumed.

use harnless_exec_sandbox::{LocalSandbox, PolicyHomeExt, SandboxMode};
use harnless_seams::exec::{Enforced, Sandbox};

fn argv() -> Vec<String> {
    vec!["/bin/bash".to_string(), "-c".to_string(), "true".to_string()]
}

#[test]
fn deny_all_enforced_carries_the_refusal_verdict_and_reason() {
    let policy = PolicyHomeExt::from_root_and_mode(std::env::temp_dir(), SandboxMode::SandboxLocal);
    let enforced = LocalSandbox::with_mode(SandboxMode::DenyAll)
        .enforce(&argv(), &policy)
        .expect("decide");
    // The full auditable refusal shape, read straight off the seam struct.
    assert_eq!(
        Enforced {
            confined: false,
            allowed: false,
            mode: "deny-all".to_string(),
            reason: enforced.reason.clone(),
        },
        enforced,
        "a deny-all refusal must be confined=false, allowed=false, mode=\"deny-all\" \
         plus a non-empty reason: {enforced:?}"
    );
    assert!(!enforced.reason.is_empty(), "a refusal must explain itself");
    assert!(
        enforced.reason.contains("refus") || enforced.reason.contains("denied"),
        "reason must state the denial, got: {}",
        enforced.reason
    );
}

#[test]
fn unconfined_enforced_reports_an_allowed_run() {
    let policy = PolicyHomeExt::from_root_and_mode(std::env::temp_dir(), SandboxMode::Unconfined);
    let enforced = LocalSandbox::new()
        .enforce(&argv(), &policy)
        .expect("decide");
    assert!(enforced.allowed, "unconfined policy permits the run");
    assert!(!enforced.confined, "nothing was confined");
    assert_eq!("unconfined", enforced.mode);
    assert!(!enforced.reason.is_empty());
}

#[test]
fn sandbox_local_enforced_reports_a_confined_allowed_run() {
    let policy = PolicyHomeExt::from_root_and_mode(std::env::temp_dir(), SandboxMode::SandboxLocal);
    let enforced = LocalSandbox::new()
        .enforce(&argv(), &policy)
        .expect("decide");
    assert!(enforced.allowed);
    assert!(enforced.confined);
    assert_eq!("sandbox-local", enforced.mode);
    // Honesty survives the move onto the seam: the reason still says what
    // was guaranteed and that it is not kernel enforcement.
    assert!(enforced.reason.contains("best-effort"));
    assert!(enforced.reason.contains("NOT kernel-enforced"));
}

#[test]
fn an_unestablishable_root_denies_on_the_seam_not_in_a_side_report() {
    // The refusal a consumer would previously have had to reverse-engineer
    // from mode+confined now arrives as `allowed == false` with a reason.
    let policy = PolicyHomeExt::from_root_and_mode(
        "/nonexistent/root-for-verdict-test",
        SandboxMode::SandboxLocal,
    );
    let enforced = LocalSandbox::new()
        .enforce(&argv(), &policy)
        .expect("decide");
    assert!(!enforced.allowed, "an unenforceable root must refuse");
    assert!(!enforced.confined, "a refusal must not look confined");
    assert_eq!("sandbox-local", enforced.mode);
    assert!(enforced.reason.contains("cannot be established"), "{enforced:?}");
}
