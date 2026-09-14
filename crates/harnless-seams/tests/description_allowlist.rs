//! The description allowlist: [`ToolDefinition`] carries model-facing text,
//! and [`ToolDefinition::to_schema`] is the single projection from a registry
//! definition to the shape an adapter may put on the wire.
//!
//! The assertion that matters here is not "the three fields come across" —
//! that is plumbing. It is that the projection's *carried set* is exactly the
//! allowlist: a field that is not name, description, or parameters cannot
//! cross, no matter what it is or what it contains. `serialized` is the
//! witness for the whole internal class (scheduling metadata the model has
//! no business seeing), and the test pins it by value, not by absence from a
//! field list a future refactor could widen.

use harnless_seams::tools::ToolDefinition;
use serde_json::json;

/// A definition whose internal field carries a value distinguishable from
/// every model-facing field, so a leak is unmistakable.
fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "search".into(),
        description: "Search the index for a query.".into(),
        schema: json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
        }),
        serialized: true,
    }
}

#[test]
fn projection_maps_the_allowlisted_fields() {
    let def = definition();
    let schema = def.to_schema();
    assert_eq!(schema.name, "search");
    assert_eq!(schema.description, "Search the index for a query.");
    assert_eq!(
        schema.parameters,
        json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
        })
    );
    // The projection defaults `strict` off: strictness is an adapter-side
    // provider preference, never something a registry definition asserts.
    assert!(!schema.strict);
}

#[test]
fn internal_fields_cannot_cross_the_projection() {
    // `serialized` is the internal field: true on the definition, and its
    // projection must not reflect it. The check is on the projected value —
    // the only artifact an adapter can hold — so no internal state can ride
    // along hidden inside a permitted field either.
    let def = definition();
    let schema = def.to_schema();
    assert_eq!(
        schema,
        harnless_seams::ToolSchema::new(
            "search",
            "Search the index for a query.",
            def.schema.clone(),
        ),
        "the projection must equal the bare allowlisted triple; anything \
         internal (serialized = {}) would make it differ",
        def.serialized,
    );
}

#[test]
fn the_projected_carried_set_is_exactly_the_allowlisted_fields() {
    // `ToolSchema` is deliberately not serializable — an adapter builds the
    // wire object field by field — so the carried set is pinned
    // structurally: the projection's value equals the value built by naming
    // the four permitted fields and nothing else, and each permitted field
    // carries only its own value.
    let def = definition();
    let schema = def.to_schema();
    assert_eq!(
        schema,
        harnless_seams::ToolSchema {
            name: "search".into(),
            description: "Search the index for a query.".into(),
            parameters: def.schema.clone(),
            strict: false,
        }
    );
    // Nothing internal rode along inside a permitted field: the projected
    // parameters are the declared schema, key for key, with no extras.
    let mut keys: Vec<&str> = schema
        .parameters
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["properties", "type"]);
}
