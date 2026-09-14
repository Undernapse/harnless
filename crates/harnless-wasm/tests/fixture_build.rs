//! Corpus reproducibility: the checked-in fixture bytes equal the bytes the
//! `fixture` module derives, and every fixture loads as a component.

use harnless_wasm::fixture::{component_bytes, fixture_path, regenerate_fixtures, ALL};

#[test]
fn every_fixture_loads_as_a_component() {
    let engine = harnless_wasm::engine::build_engine().unwrap();
    for behavior in ALL {
        let bytes = component_bytes(behavior);
        wasmtime::component::Component::new(&engine, bytes)
            .unwrap_or_else(|e| panic!("{behavior:?} fixture did not compile: {e}"));
    }
}

#[test]
fn fixture_corpus_is_byte_reproducible() {
    let files = regenerate_fixtures();
    assert_eq!(files.len(), ALL.len());
    for behavior in ALL {
        let path = fixture_path(behavior);
        let on_disk = std::fs::read(&path).expect("fixture on disk");
        assert_eq!(
            on_disk,
            component_bytes(behavior),
            "{} drifted from the derived bytes",
            path.display()
        );
    }
}

/// The `fsread` fixture is the one corpus member *not* derived from WAT: it is
/// a `cargo-component` build whose bytes depend on an external toolchain, so
/// nothing above re-derives it. Its SHA-256 is pinned here — if the checked-in
/// component changes (toolchain upgrade, guest edit, accidental binary churn)
/// this fails loudly instead of quietly changing what the fs-scope tests
/// actually mounted.
///
/// Re-pin deliberately: rebuild per `fixtures/fsread_guest/BUILD.md`, confirm
/// the guest behaviour tests still pass against the new bytes, update the
/// digest.
#[test]
fn fsread_fixture_bytes_are_pinned() {
    const PINNED: &str = "34a3ca6c102336da573662318948a48a609c682f9a1f8aa36e000b9e37af33ff";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("fsread_plugin.wasm");
    let bytes = std::fs::read(&path).expect("fsread fixture on disk");
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        PINNED,
        "{} drifted from the pinned bytes",
        path.display()
    );
}
