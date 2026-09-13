//! Run the plugability conformance kit against the real local provider.
//!
//! The kit's contract: adding a provider means passing the suite unchanged.

use harnless_fs_local::LocalFileSystem;

/// Each case gets a fresh provider over a fresh root. The tempdir is
/// leaked per case — the root must outlive the case's assertions, and
/// leaking in a short-lived test binary is fine.
fn make() -> LocalFileSystem {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    LocalFileSystem::new(dir.path()).expect("mount")
}

harnless_conformance::conformance_tests_fs! {
    fs_local,
    make,
    "write_then_read",
    "stale_version_guard",
    "unobserved_guarded_write",
    "create_if_absent",
    "guarded_edit_stale",
    "atomic_write_no_debris",
    "atomic_edit_no_debris",
    "overwrite_in_place",
    "same_file_same_key",
    "forged_target_refused",
    "windowed_read_past_cap",
    "windowed_read_no_cap",
    "read_missing_is_not_found",
    "list_file_is_not_a_directory",
    "read_binary_is_not_text",
    "read_directory_is_not_a_regular_file",
    "permission_denied_vs_sandbox_denied",
}
