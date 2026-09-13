//! Run the plugability conformance kit against the real file provider.
//!
//! The kit's contract: adding a provider means passing the suite unchanged.
//! The settings probes are descriptor honesty — an absent key's describe
//! must be absent or agree with `get`, and a summary must never leak the
//! raw value.

use harnless_settings_file::{LayerSource, SettingsFile};

/// A fresh provider over a realistic two-layer composition: a shipped
/// base document beneath a user document, plus a file layer, so the probes
/// exercise the resolution paths the provider actually serves.
fn make() -> SettingsFile {
    let base = serde_json::json!({
        "harnless-conformance": {
            "shipped": "base-value",
            "count": 7,
            "flag": true
        }
    });
    let user = serde_json::json!({
        "harnless-conformance": { "shipped": "user-value" }
    });
    SettingsFile::new(vec![LayerSource::doc(base), LayerSource::doc(user)])
}

#[test]
fn settings_conformance_is_clean() {
    let provider = make();
    let violations = harnless_conformance::check_settings(&provider);
    assert!(
        violations.is_empty(),
        "conformance violations: {violations:?}"
    );
}

#[test]
fn settings_conformance_over_file_layers_is_clean() {
    // Same suite against the file-backed composition (the swap changes
    // storage, not the contract).
    // into_tempdir keeps the directory alive for the test's lifetime
    // without leaking to satisfy a borrow.
    let dir = tempfile::tempdir().expect("tempdir");
    let base_path = dir.path().join("base.yml");
    let user_path = dir.path().join("user.yml");
    std::fs::write(
        &base_path,
        "harnless-conformance:\n  shipped: base\n  count: 2\n",
    )
    .unwrap();
    std::fs::write(&user_path, "harnless-conformance:\n  shipped: user\n").unwrap();
    let provider = SettingsFile::new(vec![
        LayerSource::file(&base_path),
        LayerSource::file(&user_path),
    ]);
    let violations = harnless_conformance::check_settings(&provider);
    assert!(
        violations.is_empty(),
        "conformance violations: {violations:?}"
    );
}
