# fsread_plugin.wasm

The one fixture not derived from WAT: a real `cargo-component` guest whose body
performs genuine WASI filesystem I/O, so the `/scope` grant is proven by a guest
that actually reads through it (the WAT fixtures' bodies do not touch WASI).

Rebuild (requires `cargo-component` and the `wasm32-unknown-unknown` target):

    cd crates/harnless-wasm/fixtures/fsread_guest
    cargo component build --release --target wasm32-unknown-unknown
    cp target/wasm32-unknown-unknown/release/echo_fs_read.wasm ../fsread_plugin.wasm

`wit/deps/*.wit` are the `wasi:io` / `wasi:clocks` / `wasi:filesystem`
interfaces at 0.2.12, copied from the `wasmtime-wasi` 46.0.1 crate so the build
is pinned to the same WASI preview2 version the host implements.

The guest is deliberately minimal: no allocator beyond what the canonical ABI
lifts, no serde. `arg_path` hand-parses the `path` field out of the tool's raw
JSON payload, and `json_escape` keeps the result a valid JSON object.

`Cargo.lock` is checked in so a rebuild resolves the same `wit-bindgen-rt`
(the package carries its own `[workspace]` table, so it is standalone from the
host workspace and its own lockfile applies). The generated `src/bindings.rs`
is rebuilt from `wit/` on every build and is not checked in.

The resulting bytes are pinned: `tests/fixture_build.rs::fsread_fixture_bytes_are_pinned`
asserts the checked-in `fsread_plugin.wasm` SHA-256. After a deliberate rebuild,
confirm the guest behaviour tests pass against the new bytes and update that
digest.
