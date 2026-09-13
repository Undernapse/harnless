//! Corpus reproducibility: the checked-in fixture bytes equal the bytes the
//! `fixture` module derives, and every fixture loads as a component.

use harnless_wasm::fixture::{component_bytes, regenerate_fixtures, Behavior};

#[test]
fn fixture_corpus_is_byte_reproducible() {
    let files = regenerate_fixtures();
    assert_eq!(files.len(), 4);
    for behavior in [Behavior::Echo, Behavior::Boom, Behavior::Spin, Behavior::FsRead] {
        let path = harnless_wasm::fixture::fixture_path(behavior);
        let on_disk = std::fs::read(&path).expect("fixture on disk");
        assert_eq!(
            on_disk,
            component_bytes(behavior),
            "{} drifted from the derived bytes",
            path.display()
        );
    }
}

#[test]
fn every_fixture_loads_as_a_component() {
    let engine = harnless_wasm::engine::build_engine().unwrap();
    for behavior in [Behavior::Echo, Behavior::Boom, Behavior::Spin, Behavior::FsRead] {
        let bytes = component_bytes(behavior);
        wasmtime::component::Component::new(&engine, bytes)
            .unwrap_or_else(|e| panic!("{behavior:?} fixture did not compile: {e}"));
    }
}
