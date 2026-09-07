//! Building an OpenAI chat-completions request from seam `Message`s.
//!
//! The mapping is deliberately faithful to the seam: tool arguments stay raw
//! JSON strings end to end. The adapter reads fields out of a block's raw
//! JSON to route them into the wire shape, but never re-serializes an
//! `arguments` value — the string the provider asked for is the string the
//! provider sees on a subsequent request.

use serde_json::{json, Map, Value};

use harnless_seams::{BlockKind, ContentBlock, Message, Role, ToolSchema};

/// Build the `messages` array for a chat-completions request.
pub fn messages(messages: &[Message]) -> Vec<Value> {
    messages.iter().map(message).collect()
}

/// Build the `tools` array for a chat-completions request. An empty set
/// yields no `tools` key at all, so a bare text model is never asked to
/// declare callables it does not have.
pub fn tools(tools: &[ToolSchema]) -> Option<Value> {
    if tools.is_empty() {
        return None;
    }
    let list = tools
        .iter()
        .map(|t| {
            let mut function = Map::new();
            function.insert("name".into(), json!(t.name));
            function.insert("description".into(), json!(t.description));
            function.insert("parameters".into(), t.parameters.clone());
            if t.strict {
                function.insert("strict".into(), json!(true));
            }
            json!({
                "type": "function",
                "function": function,
            })
        })
        .collect::<Vec<_>>();
    Some(Value::Array(list))
}

/// Build one wire message from a seam [`Message`].
fn message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    // Tool results are a bare `{ role: "tool", tool_call_id, content }`.
    if msg.role == Role::Tool {
        return tool_result(msg);
    }

    // Split blocks into plain content parts and tool calls.
    let mut content_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in &msg.blocks {
        match block.kind {
            BlockKind::Text => content_parts.push(json!({ "type": "text", "text": block.text })),
            BlockKind::Reasoning => {
                // Reasoning is informational for the model's own consumption;
                // it is not sent back as assistant content.
            }
            BlockKind::ToolCall => tool_calls.push(tool_call(block)),
            BlockKind::Image => {
                // Image content is only replayed when the provider declared
                // image input; otherwise it degrades to its text payload so
                // a bad history is loud rather than silently dropped.
                content_parts.push(json!({ "type": "text", "text": block.text }));
            }
            BlockKind::ToolResult => {
                // A tool result reused outside a `Role::Tool` message is an
                // invariant violation — surface it as text.
                content_parts.push(json!({ "type": "text", "text": block.text }));
            }
        }
    }

    let mut wire = Map::new();
    wire.insert("role".into(), json!(role));

    if tool_calls.is_empty() {
        wire.insert("content".into(), content_value(content_parts));
    } else {
        // An assistant message carrying tool calls has no plain content.
        wire.insert("content".into(), Value::Null);
        wire.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    Value::Object(wire)
}

/// Fold plain content parts into the OpenAI `content` field: a single text
/// part becomes a bare string, several become a parts array, none becomes
/// null.
fn content_value(parts: Vec<Value>) -> Value {
    match parts.len() {
        0 => Value::Null,
        1 => parts[0]["text"].clone(),
        _ => Value::Array(parts),
    }
}

/// Build an OpenAI `tool_calls` entry from a raw-JSON tool-call block.
///
/// The block's `text` is the assembled raw JSON the provider emitted for the
/// call — typically `{ "id", "name", "arguments" }`. `arguments` is read
/// verbatim as the string the provider produced, never re-serialized.
fn tool_call(block: &ContentBlock) -> Value {
    let raw: Value = serde_json::from_str(&block.text).unwrap_or_else(|_| json!({}));
    let id = raw.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let name = raw.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    // `arguments` may already be a string; otherwise serialize the object
    // once to recover the raw form.
    let arguments = match raw.get("arguments") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
        None => String::new(),
    };
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        },
    })
}

/// Build a `{ role: "tool" }` result message.
///
/// The block's raw `text` is the tool output. `tool_call_id` is recovered
/// from a `{ "call_id" | "tool_call_id" | "id" }` field the provider-tool
/// correlation stamped into the raw JSON.
fn tool_result(msg: &Message) -> Value {
    let block = msg
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::ToolResult)
        .or_else(|| msg.blocks.first());
    let (call_id, content) = match block {
        Some(b) => (raw_tool_id(&b.text), b.text.clone()),
        None => (String::new(), String::new()),
    };
    json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": content,
    })
}

/// Extract the provider's tool-call correlation id from raw JSON.
fn raw_tool_id(text: &str) -> String {
    if let Ok(raw) = serde_json::from_str::<Value>(text) {
        if let Some(id) = raw
            .get("call_id")
            .or_else(|| raw.get("tool_call_id"))
            .or_else(|| raw.get("id"))
            .and_then(|v| v.as_str())
        {
            return id.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnless_seams::MessageId;

    fn msg(role: Role, blocks: Vec<ContentBlock>) -> Message {
        Message {
            id: MessageId(1),
            role,
            blocks,
            provider: None,
            model: None,
            replay_state: None,
        }
    }

    #[test]
    fn plain_text_user_is_a_string_content() {
        let m = msg(
            Role::User,
            vec![ContentBlock { kind: BlockKind::Text, text: "hi".into() }],
        );
        let wire = message(&m);
        assert_eq!(wire["role"], "user");
        assert_eq!(wire["content"], "hi");
    }

    #[test]
    fn assistant_tool_call_reads_raw_arguments_verbatim() {
        let block = ContentBlock {
            kind: BlockKind::ToolCall,
            text: r#"{"id":"call_1","name":"search","arguments":"{\"q\":\"x\"}"}"#.into(),
        };
        let m = msg(Role::Assistant, vec![block]);
        let wire = message(&m);
        assert_eq!(wire["content"], Value::Null);
        assert_eq!(wire["tool_calls"][0]["id"], "call_1");
        assert_eq!(wire["tool_calls"][0]["function"]["name"], "search");
        // The arguments string is passed through verbatim, not re-serialized.
        assert_eq!(
            wire["tool_calls"][0]["function"]["arguments"],
            r#"{"q":"x"}"#
        );
    }

    #[test]
    fn tool_result_recover_call_id_from_raw() {
        let m = msg(
            Role::Tool,
            vec![ContentBlock {
                kind: BlockKind::ToolResult,
                text: r#"{"call_id":"call_1","content":"42"}"#.into(),
            }],
        );
        let wire = message(&m);
        assert_eq!(wire["role"], "tool");
        assert_eq!(wire["tool_call_id"], "call_1");
        assert_eq!(wire["content"], r#"{"call_id":"call_1","content":"42"}"#);
    }

    #[test]
    fn empty_tool_set_yields_no_tools_key() {
        assert!(tools(&[]).is_none());
    }

    #[test]
    fn tool_schema_emits_function_definition() {
        let schema = ToolSchema::new("search", "Find things", json!({"type":"object"}))
            .strict(true);
        let built = tools(&[schema]).unwrap();
        assert_eq!(built[0]["type"], "function");
        assert_eq!(built[0]["function"]["name"], "search");
        assert_eq!(built[0]["function"]["description"], "Find things");
        assert_eq!(built[0]["function"]["parameters"], json!({"type":"object"}));
        assert_eq!(built[0]["function"]["strict"], true);
    }
}
