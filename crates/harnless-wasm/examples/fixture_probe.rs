use harnless_wasm::fixture::{component_bytes, Behavior};
fn main() {
    let bytes = component_bytes(Behavior::Echo);
    println!("len {}", bytes.len());
    std::fs::write("/tmp/echo_now.wasm", &bytes).unwrap();
    std::fs::write("/tmp/echo_component.wasm", &bytes).unwrap();
    let engine = harnless_wasm::engine::build_engine().unwrap();
    match wasmtime::component::Component::new(&engine, &bytes) {
        Ok(_) => println!("compiled ok"),
        Err(e) => println!("{e:?}"),
    }
}
