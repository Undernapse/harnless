//! Run the plugability conformance kit against the real local provider.
//!
//! The kit's contract: adding a provider means passing the suite unchanged.
//! The credentials probes are reference honesty — an unknown reference
//! resolves `Ok(None)` and `kind` agrees with resolvability.

use harnless_credentials_local::LocalCredentials;

/// A fresh provider over a fresh store path, with one registered flow so
/// the probes see a fully-composed provider.
fn make() -> LocalCredentials {
    let dir = tempfile::tempdir().expect("tempdir");
    // Keep the store alive for the test binary: the provider resolves
    // per-operation, so the path must outlive every probe.
    let path = dir.path().join("credentials.json");
    std::mem::forget(dir);
    LocalCredentials::builder()
        .path(path)
        .flow(
            harnless_seams::credentials::CredentialKind::OAuth2,
            |_reference| Ok("conformance-secret".to_string()),
        )
        .build()
}

#[test]
fn credentials_conformance_is_clean() {
    let provider = make();
    let violations = harnless_conformance::check_credentials(&provider);
    assert!(
        violations.is_empty(),
        "conformance violations: {violations:?}"
    );
}

#[test]
fn credentials_conformance_after_a_write_is_clean() {
    // Same probes with a populated store — honesty must hold for both the
    // present and absent paths.
    let provider = make();
    provider
        .store(
            &harnless_seams::credentials::CredentialRef("conformance-present".to_string()),
            harnless_seams::credentials::CredentialKind::Bearer,
            "token",
        )
        .expect("store");
    let violations = harnless_conformance::check_credentials(&provider);
    assert!(
        violations.is_empty(),
        "conformance violations: {violations:?}"
    );
}
