//! Result projection rules: ordered blocks, text-like joins, resource
//! links, explicit diagnostics, structured-content validation with
//! unsupported-schema fallback, and the rich-content gate.

use std::sync::Arc;

use harnless_mcp::projection::{
    project, AttachmentStore, InMemoryAttachmentStore, RichContentGate, RouteCapabilities,
};
use rmcp::model::{
    CallToolResult, ContentBlock, EmbeddedResource, ImageContent, Resource, ResourceContents,
    TextContent,
};
use serde_json::{json, Value};

fn result(blocks: Vec<ContentBlock>) -> CallToolResult {
    let mut r = CallToolResult::success(blocks);
    r.is_error = Some(false);
    r
}

fn text(t: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(t))
}

#[test]
fn canonical_success_keeps_complete_ordered_content_blocks() {
    let r = result(vec![
        text("one"),
        ContentBlock::ResourceLink(Resource::new("file:///a", "alpha")),
        text("two"),
    ]);
    let out = project(r, None, &RichContentGate::closed()).unwrap();
    assert_eq!(out["ok"], json!(true));
    let items = out["content"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["type"], json!("text"));
    assert_eq!(items[1]["type"], json!("resource_link"));
    assert_eq!(items[2]["text"], json!("two"));
}

#[test]
fn adjacent_text_like_runs_join() {
    let r = result(vec![text("Hello, "), text("world"), text("!")]);
    let out = project(r, None, &RichContentGate::closed()).unwrap();
    let items = out["content"].as_array().unwrap();
    assert_eq!(items.len(), 1, "adjacent text joins into one item");
    assert_eq!(items[0]["text"], json!("Hello, world!"));
}

#[test]
fn resource_links_keep_name_and_uri_only() {
    let link = Resource::new("https://example/x", "the-x").with_title("ignored title");
    let r = result(vec![ContentBlock::ResourceLink(link)]);
    let out = project(r, None, &RichContentGate::closed()).unwrap();
    let item = &out["content"][0];
    assert_eq!(
        item,
        &json!({"type": "resource_link", "name": "the-x", "uri": "https://example/x"})
    );
}

#[test]
fn unsupported_result_kind_surfaces_as_explicit_diagnostic() {
    // rmcp's ContentBlock is non-exhaustive: simulate a future/unsupported
    // kind by feeding an audio block through a closed gate — the gate's
    // refusal is itself an explicit diagnostic naming the reason.
    let r = result(vec![ContentBlock::Audio(rmcp::model::AudioContent::new(
        "aGk=",
        "audio/wav",
    ))]);
    let out = project(r, None, &RichContentGate::closed()).unwrap();
    let item = &out["content"][0];
    assert_eq!(item["type"], json!("diagnostic"));
    assert!(item["reason"]
        .as_str()
        .unwrap()
        .contains("rich content (audio/wav) withheld"));
    assert!(item["reason"]
        .as_str()
        .unwrap()
        .contains("no attachment store mounted"));
}

#[test]
fn structured_content_validates_against_supported_schema() {
    let mut r = result(vec![text("ok")]);
    r.structured_content = Some(json!({"count": 3}));
    let schema = json!({
        "type": "object",
        "properties": { "count": { "type": "integer" } },
        "required": ["count"],
    });
    let out = project(r.clone(), Some(&schema), &RichContentGate::closed()).unwrap();
    assert_eq!(out["schemaValidated"], json!(true));
    assert!(out.get("schemaNote").is_none());

    // A violating value is flagged, never dropped.
    r.structured_content = Some(json!({"count": "three"}));
    let out = project(r, Some(&schema), &RichContentGate::closed()).unwrap();
    assert_eq!(out["schemaValidated"], json!(false));
    assert!(out["schemaNote"]
        .as_str()
        .unwrap()
        .contains("violates declared type"));
    assert_eq!(out["structured"]["count"], json!("three"));
}

#[test]
fn unsupported_schema_falls_back_to_unconstrained_json() {
    let mut r = result(vec![text("ok")]);
    r.structured_content = Some(json!({"anything": [1, 2]}));
    let schema = json!({ "oneOf": [ { "type": "object" }, { "type": "array" } ] });
    let out = project(r, Some(&schema), &RichContentGate::closed()).unwrap();
    assert_eq!(out["schemaValidated"], json!(false));
    assert!(out["schemaNote"].as_str().unwrap().contains("oneOf"));
    assert_eq!(out["structured"], json!({"anything": [1, 2]}));
}

#[test]
fn tool_level_error_surfaces_as_structured_failure() {
    let mut r = result(vec![text("query failed: no rows")]);
    r.is_error = Some(true);
    let out = project(r, None, &RichContentGate::closed()).unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["code"], json!("tool-error"));
    assert_eq!(out["message"], json!("query failed: no rows"));
}

/// A store that records what it was given and can be made to fail.
#[derive(Clone)]
struct FlakyStore {
    fail: Arc<parking_lot::Mutex<bool>>,
    inner: Arc<InMemoryAttachmentStore>,
}

impl AttachmentStore for FlakyStore {
    fn put(&self, mime: &str, base64: &str) -> Result<String, String> {
        if *self.fail.lock() {
            return Err("store full".into());
        }
        self.inner.put(mime, base64)
    }
}

fn open_gate(store: Arc<dyn AttachmentStore>) -> RichContentGate {
    RichContentGate {
        store: Some(store),
        route: RouteCapabilities { image_input: true },
    }
}

#[test]
fn images_admitted_only_with_store_and_image_input_route() {
    let blocks = vec![ContentBlock::Image(ImageContent::new("aW1n", "image/png"))];
    // Store but no image-input route: withheld with the reason.
    let gate = RichContentGate {
        store: Some(Arc::new(InMemoryAttachmentStore::new())),
        route: RouteCapabilities::default(),
    };
    let out = project(result(blocks.clone()), None, &gate).unwrap();
    assert_eq!(out["content"][0]["type"], json!("diagnostic"));
    assert!(out["content"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("does not declare image input"));
    // Both halves present: admitted as a reference.
    let store = Arc::new(InMemoryAttachmentStore::new());
    let out = project(result(blocks), None, &open_gate(store.clone())).unwrap();
    assert_eq!(out["content"][0]["type"], json!("image"));
    assert_eq!(out["content"][0]["mime"], json!("image/png"));
    assert_eq!(store.len(), 1);
}

#[test]
fn rich_batch_validated_wholly_before_any_member_admitted() {
    // Two images, store fails: neither is admitted, both become diagnostics.
    let fail = Arc::new(parking_lot::Mutex::new(true));
    let store = Arc::new(FlakyStore {
        fail: fail.clone(),
        inner: Arc::new(InMemoryAttachmentStore::new()),
    });
    let blocks = vec![
        text("before"),
        ContentBlock::Image(ImageContent::new("aQ==", "image/png")),
        ContentBlock::Image(ImageContent::new("aGk=", "image/png")),
        ContentBlock::ResourceLink(Resource::new("file:///keep", "keep")),
    ];
    let out = project(result(blocks), None, &open_gate(store)).unwrap();
    let items = out["content"].as_array().unwrap();
    assert_eq!(items.len(), 4, "ordering preserved through batch rejection");
    assert_eq!(items[0]["text"], json!("before"));
    assert_eq!(items[1]["type"], json!("diagnostic"));
    assert_eq!(items[2]["type"], json!("diagnostic"));
    assert_eq!(items[3]["name"], json!("keep"));
    assert!(items[1]["reason"]
        .as_str()
        .unwrap()
        .contains("batch rejected"));
}

#[test]
fn base64_never_copied_into_the_projected_record() {
    let store = Arc::new(InMemoryAttachmentStore::new());
    let secret = "SUPERSECRETBASE64PAYLOAD";
    let blocks = vec![ContentBlock::Image(ImageContent::new(secret, "image/png"))];
    let out = project(result(blocks), None, &open_gate(store)).unwrap();
    let serialized = out.to_string();
    assert!(
        !serialized.contains(secret),
        "base64 payload leaked into the projected record"
    );
    assert!(serialized.contains("attachment-"), "reference expected");
}

#[test]
fn embedded_resource_text_and_blob_are_projected() {
    let blocks = vec![
        ContentBlock::Resource(EmbeddedResource::new(
            ResourceContents::TextResourceContents {
                uri: "file:///t".into(),
                mime_type: Some("text/plain".into()),
                text: "hello".into(),
                meta: None,
            },
        )),
        ContentBlock::Resource(EmbeddedResource::new(
            ResourceContents::BlobResourceContents {
                uri: "file:///b".into(),
                mime_type: Some("image/png".into()),
                blob: "YmxvYg==".into(),
                meta: None,
            },
        )),
    ];
    let store = Arc::new(InMemoryAttachmentStore::new());
    let out = project(result(blocks), None, &open_gate(store)).unwrap();
    let items = out["content"].as_array().unwrap();
    assert_eq!(items[0]["type"], json!("resource"));
    assert_eq!(items[0]["text"], json!("hello"));
    // The blob became an admitted attachment with its URI kept, no inline base64.
    assert_eq!(items[1]["type"], json!("image"));
    assert_eq!(items[1]["uri"], json!("file:///b"));
    assert!(!out.to_string().contains("YmxvYg=="));
}

#[test]
fn no_schema_means_unconstrained_passthrough() {
    let mut r = result(vec![]);
    r.structured_content = Some(json!({"x": 1}));
    let out: Value = project(r, None, &RichContentGate::closed()).unwrap();
    assert_eq!(out["structured"], json!({"x": 1}));
    assert_eq!(out["schemaValidated"], json!(false));
}
