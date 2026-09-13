//! Result projection: MCP `CallToolResult` → the harness canonical result.
//!
//! Canonical success keeps the *complete, ordered* content blocks plus
//! optional structured content. The rules (issue #10):
//!
//! * text-like runs (adjacent text blocks) join into one text item;
//! * resource links keep name + URI and nothing else;
//! * unsupported content kinds become explicit diagnostic items — they are
//!   never silently dropped;
//! * structured content is validated against the tool's declared output
//!   schema when the schema is in the supported subset; an unsupported
//!   schema falls back to unconstrained JSON (the value passes through);
//! * rich content (images/audio) is admitted only when an attachment store
//!   is mounted **and** the calling route declares image input, with the
//!   whole batch validated before any member is admitted — base64 payload
//!   is handed to the store, never copied into a session record;
//! * a tool-level error (`isError`) surfaces as a structured failure with
//!   the server's own content as the message.

use std::sync::Arc;

use parking_lot::Mutex;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{json, Value};

use harnless_seams::SeamError;

/// A store that accepts image attachments and hands back an opaque
/// reference safe to put in a session record.
///
/// The bridge never copies base64 into a session record: the only thing
/// that survives projection for an admitted image is the reference this
/// store returns.
pub trait AttachmentStore: Send + Sync {
    /// Store `base64` bytes of MIME type `mime`; return an opaque
    /// reference (e.g. a content-addressed id) for the record.
    fn put(&self, mime: &str, base64: &str) -> Result<String, String>;
}

/// What the calling route declared about its input capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RouteCapabilities {
    /// The route accepts image input blocks.
    pub image_input: bool,
}

/// The rich-content gate: both halves must be present before any image or
/// audio block from a server is admitted into the result.
#[derive(Clone, Default)]
pub struct RichContentGate {
    /// Mounted attachment store, if any.
    pub store: Option<Arc<dyn AttachmentStore>>,
    /// What the calling route declares.
    pub route: RouteCapabilities,
}

impl RichContentGate {
    /// A gate with nothing mounted: rich content is never admitted.
    pub fn closed() -> Self {
        Self::default()
    }

    /// Whether rich content may be admitted at all (both halves).
    pub fn admits(&self) -> bool {
        self.store.is_some() && self.route.image_input
    }
}

/// Project an MCP tool result into the harness canonical JSON.
///
/// `output_schema` is the tool's declared output schema (if any);
/// `gate` controls rich-content admission.
pub fn project(
    result: CallToolResult,
    output_schema: Option<&Value>,
    gate: &RichContentGate,
) -> Result<Value, SeamError> {
    let is_error = result.is_error.unwrap_or(false);
    let items = project_blocks(result.content, gate);

    let structured = result.structured_content.map(|value| {
        let (validated, note) = validate_structured(&value, output_schema);
        (value, validated, note)
    });

    if is_error {
        let message = join_text_like(&items);
        return Ok(json!({
            "ok": false,
            "code": "tool-error",
            "message": message,
            "content": items,
        }));
    }

    let mut out = json!({
        "ok": true,
        "content": items,
    });
    if let Some((value, validated, note)) = structured {
        out["structured"] = value;
        out["schemaValidated"] = json!(validated);
        if let Some(note) = note {
            out["schemaNote"] = json!(note);
        }
    }
    Ok(out)
}

/// Project the ordered content blocks, joining text-like runs.
fn project_blocks(blocks: Vec<ContentBlock>, gate: &RichContentGate) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    // Whole-batch validation for rich content: collect first, admit only
    // if every member of the batch stores cleanly.
    let mut pending_rich: Vec<(usize, Value, String, String)> = Vec::new(); // (insert idx, placeholder, mime, b64)

    for block in blocks {
        match block {
            ContentBlock::Text(t) => push_text_like(&mut items, "text", t.text),
            ContentBlock::ResourceLink(link) => {
                // Resource links keep name + URI and nothing else.
                items.push(json!({
                    "type": "resource_link",
                    "name": link.name,
                    "uri": link.uri,
                }));
            }
            ContentBlock::Resource(embedded) => {
                let (uri, mime, text, blob) = match &embedded.resource {
                    rmcp::model::ResourceContents::TextResourceContents {
                        uri,
                        mime_type,
                        text,
                        ..
                    } => (
                        uri.clone(),
                        mime_type.clone().unwrap_or_else(|| "text/plain".into()),
                        Some(text.clone()),
                        None,
                    ),
                    rmcp::model::ResourceContents::BlobResourceContents {
                        uri,
                        mime_type,
                        blob,
                        ..
                    } => (
                        uri.clone(),
                        mime_type
                            .clone()
                            .unwrap_or_else(|| "application/octet-stream".into()),
                        None,
                        Some(blob.clone()),
                    ),
                    _ => (String::new(), "application/octet-stream".into(), None, None),
                };
                match text {
                    Some(text) => items.push(json!({
                        "type": "resource",
                        "uri": uri,
                        "mime": mime,
                        "text": text,
                    })),
                    // An embedded blob is base64: it never enters the
                    // record inline; only the store reference may.
                    None => {
                        let b64 = blob.unwrap_or_default();
                        pending_rich.push((
                            items.len(),
                            json!({ "type": "resource", "uri": uri, "mime": mime }),
                            mime,
                            b64,
                        ));
                    }
                }
            }
            ContentBlock::Image(image) => {
                pending_rich.push((items.len(), Value::Null, image.mime_type, image.data));
            }
            ContentBlock::Audio(audio) => {
                pending_rich.push((items.len(), Value::Null, audio.mime_type, audio.data));
            }
            // Any other kind (future spec kinds) is an explicit diagnostic.
            other => items.push(diagnostic(&format!(
                "unsupported content kind: {}",
                kind_of(&other)
            ))),
        }
    }

    admit_rich(&mut items, pending_rich, gate);
    items
}

/// Insert a text-like item, joining with the immediately preceding
/// text-like item of the same type.
fn push_text_like(items: &mut Vec<Value>, kind: &str, text: String) {
    if let Some(last) = items.last_mut() {
        if last.get("type").and_then(Value::as_str) == Some(kind) {
            if let Some(existing) = last.get("text").and_then(Value::as_str) {
                let joined = format!("{existing}{text}");
                last["text"] = Value::String(joined);
                return;
            }
        }
    }
    items.push(json!({ "type": kind, "text": text }));
}

/// Join all text-like items in order (used for error messages).
fn join_text_like(items: &[Value]) -> String {
    items
        .iter()
        .filter(|i| matches!(i.get("type").and_then(Value::as_str), Some("text")))
        .filter_map(|i| i.get("text").and_then(Value::as_str))
        .collect::<String>()
}

/// Whole-batch rich-content admission.
///
/// If the gate is closed, every rich member becomes an explicit diagnostic
/// (the caller learns *why* it is missing). If open, the batch is validated
/// first: every member must store, or none is admitted (all become
/// diagnostics naming the failure).
fn admit_rich(
    items: &mut Vec<Value>,
    pending: Vec<(usize, Value, String, String)>,
    gate: &RichContentGate,
) {
    if pending.is_empty() {
        return;
    }
    let store = match gate.store.as_ref() {
        Some(s) if gate.route.image_input => s.clone(),
        _ => {
            let reason = if gate.store.is_none() {
                "no attachment store mounted"
            } else {
                "calling route does not declare image input"
            };
            for (idx, placeholder, mime, _) in pending {
                let mut diag = diagnostic(&format!("rich content ({mime}) withheld: {reason}"));
                if placeholder.get("type").and_then(Value::as_str) == Some("resource") {
                    // Keep the link's identity even when the blob is withheld.
                    if let (Some(uri), Some(m)) = (
                        placeholder.get("uri").and_then(Value::as_str),
                        placeholder.get("mime").and_then(Value::as_str),
                    ) {
                        diag["uri"] = json!(uri);
                        diag["mime"] = json!(m);
                    }
                }
                items.insert(idx, diag);
            }
            return;
        }
    };
    // Validate the whole batch before admitting any member.
    let mut stored = Vec::with_capacity(pending.len());
    for (_, _, mime, b64) in &pending {
        match store.put(mime, b64) {
            Ok(reference) => stored.push(reference),
            Err(e) => {
                for (idx, placeholder, mime, _) in pending {
                    let mut diag = diagnostic(&format!("rich content batch rejected: {mime}: {e}"));
                    if let Some(uri) = placeholder.get("uri").and_then(Value::as_str) {
                        diag["uri"] = json!(uri);
                    }
                    items.insert(idx, diag);
                }
                return;
            }
        }
    }
    // Admitted: insert in reverse index order so earlier indices stay valid.
    let mut admitted: Vec<(usize, Value)> = pending
        .iter()
        .zip(stored)
        .map(|((idx, placeholder, mime, _), reference)| {
            let mut item = json!({ "type": "image", "mime": mime, "attachment": reference });
            if let Some(uri) = placeholder.get("uri").and_then(Value::as_str) {
                item["uri"] = json!(uri);
            }
            (*idx, item)
        })
        .collect();
    admitted.sort_by_key(|(idx, _)| std::cmp::Reverse(*idx));
    for (idx, item) in admitted {
        items.insert(idx, item);
    }
}

fn kind_of(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text(_) => "text",
        ContentBlock::Image(_) => "image",
        ContentBlock::Audio(_) => "audio",
        ContentBlock::Resource(_) => "resource",
        ContentBlock::ResourceLink(_) => "resource_link",
        _ => "unknown",
    }
}

/// An explicit diagnostic item for content the bridge cannot represent.
fn diagnostic(reason: &str) -> Value {
    json!({ "type": "diagnostic", "reason": reason })
}

/// Validate `value` against `schema` within the supported subset.
///
/// Returns `(validated, note)`:
/// * no schema → `(false, None)` — unconstrained JSON;
/// * supported-subset schema → `(true, None)` when the value conforms, or
///   `(false, Some(detail))` when it does not (the value still passes
///   through, flagged);
/// * a schema using constructs outside the subset → `(false, Some(note))`
///   — falls back to unconstrained JSON, never a hard failure.
pub fn validate_structured(value: &Value, schema: Option<&Value>) -> (bool, Option<String>) {
    let Some(schema) = schema else {
        return (false, None);
    };
    match schema_check(value, schema) {
        Check::Pass => (true, None),
        Check::Fail(detail) => (false, Some(detail)),
        Check::Unsupported(note) => (false, Some(note)),
    }
}

enum Check {
    Pass,
    Fail(String),
    Unsupported(String),
}

/// The supported output-schema subset: `type` (object/array/string/
/// number/boolean/null + union lists), `properties`, `required`, `items`
/// (single schema), `enum`, `const`. Anything else (`$ref`, `oneOf`,
/// `anyOf`, formats…) is unsupported and falls back.
fn schema_check(value: &Value, schema: &Value) -> Check {
    let Some(map) = schema.as_object() else {
        return Check::Unsupported("output schema is not an object".into());
    };
    for key in map.keys() {
        match key.as_str() {
            "type" | "properties" | "required" | "items" | "enum" | "const" | "title"
            | "description" => {}
            other => return Check::Unsupported(format!("output schema uses `{other}`")),
        }
    }
    check_node(value, schema)
}

fn check_node(value: &Value, schema: &Value) -> Check {
    let Some(map) = schema.as_object() else {
        return Check::Unsupported("nested schema is not an object".into());
    };
    if let Some(c) = map.get("const") {
        if c != value {
            return Check::Fail(format!("value {value} != const {c}"));
        }
    }
    if let Some(e) = map.get("enum").and_then(Value::as_array) {
        if !e.contains(value) {
            return Check::Fail(format!("value {value} not in enum"));
        }
    }
    let types: Vec<&str> = match map.get("type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(list)) => {
            let mut out = Vec::new();
            for t in list {
                match t.as_str() {
                    Some(s) => out.push(s),
                    None => return Check::Unsupported("type list contains non-string".into()),
                }
            }
            out
        }
        Some(_) => return Check::Unsupported("type is not a string or list".into()),
        None => Vec::new(),
    };
    if !types.is_empty() && !types.iter().any(|t| type_matches(value, t)) {
        return Check::Fail(format!(
            "value of type {} violates declared type {types:?}",
            value_kind(value)
        ));
    }
    if value.is_object() && (map.contains_key("properties") || map.contains_key("required")) {
        let obj = value.as_object().unwrap();
        if let Some(req) = map.get("required").and_then(Value::as_array) {
            for r in req {
                if let Some(name) = r.as_str() {
                    if !obj.contains_key(name) {
                        return Check::Fail(format!("missing required property `{name}`"));
                    }
                } else {
                    return Check::Unsupported("required list contains non-string".into());
                }
            }
        }
        if let Some(props) = map.get("properties").and_then(Value::as_object) {
            for (name, sub) in props {
                if let Some(v) = obj.get(name) {
                    match check_node(v, sub) {
                        Check::Pass => {}
                        other => return other,
                    }
                }
            }
        }
    }
    if value.is_array() {
        if let Some(items) = map.get("items") {
            if items.is_object() {
                for v in value.as_array().unwrap() {
                    match check_node(v, items) {
                        Check::Pass => {}
                        other => return other,
                    }
                }
            } else if !items.is_boolean() {
                return Check::Unsupported("items schema is not an object".into());
            }
        }
    }
    Check::Pass
}

pub fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(value: &Value, ty: &str) -> bool {
    match ty {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true, // unknown type names are permissive, not fatal
    }
}

/// A simple in-memory attachment store (content-addressed by digest of the
/// base64 payload), used when the harness mounts a default store.
#[derive(Default)]
pub struct InMemoryAttachmentStore {
    entries: Mutex<std::collections::HashMap<String, (String, String)>>,
}

impl InMemoryAttachmentStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored attachments.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AttachmentStore for InMemoryAttachmentStore {
    fn put(&self, mime: &str, base64: &str) -> Result<String, String> {
        // FNV-1a over the payload bytes: stable, dependency-free addressing.
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x100_0000_01b3;
        let mut hash = OFFSET;
        for byte in mime
            .bytes()
            .chain(std::iter::once(b':'))
            .chain(base64.bytes())
        {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        let reference = format!("attachment-{hash:016x}");
        self.entries
            .lock()
            .insert(reference.clone(), (mime.to_string(), base64.to_string()));
        Ok(reference)
    }
}
