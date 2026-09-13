//! Run the plugability conformance kit against the real JSONL hub.
//!
//! The kit's contract: adding a provider means passing the suite unchanged.
//! The storage probes are read-your-writes and delete honesty on a scratch
//! key — both fall out of log replay, and this test proves they do.

use harnless_seams::storage::Storage;
use harnless_storage_jsonl::JsonlStorage;

/// A fresh hub over a fresh root directory.
fn make() -> JsonlStorage {
    let dir = tempfile::tempdir().expect("tempdir");
    // The root must outlive the hub's probes; tempdir cleanup is not
    // needed in a short-lived test binary, and dropping it here would
    // delete the store mid-test.
    let root = dir.path().to_path_buf();
    std::mem::forget(dir);
    JsonlStorage::new(root).expect("hub opens its root")
}

#[test]
fn storage_conformance_is_clean() {
    let hub = make();
    let violations = harnless_conformance::check_storage(&hub);
    assert!(violations.is_empty(), "conformance violations: {violations:?}");
}

#[test]
fn storage_conformance_after_compaction_is_clean() {
    // Same contract on a compacted backend — the snapshot publish must not
    // break read-your-writes or delete honesty.
    let hub = make();
    let backend = harnless_seams::storage::BackendName("pre-compaction".to_string());
    hub.set(&backend, "old", serde_json::json!("stale")).expect("set");
    hub.delete(&backend, "old").expect("delete");
    hub.compact(&backend).expect("compact");
    let violations = harnless_conformance::check_storage(&hub);
    assert!(violations.is_empty(), "conformance violations: {violations:?}");
}
