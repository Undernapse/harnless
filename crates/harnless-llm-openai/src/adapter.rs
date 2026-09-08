//! The OpenAI-compatible [`ModelAdapter`] implementation.
//!
//! Adapter obligations, per the conformance contract (05): a declared
//! identity header on every request; tool arguments raw JSON end to end;
//! exactly two sanctioned failure paths; one adapter call is one provider
//! attempt (no library-internal retries); stalls bounded by a watchdog;
//! context overflow classified to one canonical code; an empty completion
//! is a retryable failure, not a success; usage before finish and nothing
//! after.

use serde_json::{json, Map, Value};

use async_stream::stream;
use futures::StreamExt;

use harnless_seams::{
    BoxStream, CallId, ErrorCode, Message, ModelAdapter, ProviderFailure, ReplayState, Result,
    SeamError, StreamEvent, StreamFrame, ToolSchema,
};

use crate::config::OpenAiConfig;
use crate::request;
use crate::sse::SseDecoder;

/// The replay-ownership marker. Replay state is adapter-private; this key
/// inside the response metadata names the adapter that produced it, so a
/// later request is handed back only to its owner.
const REPLAY_OWNER_KEY: &str = "__harnless_provider";

/// The declared application identity header on every request.
const IDENTITY_HEADER: &str = "x-harnless-identity";

/// The OpenAI-compatible adapter.
pub struct OpenAiAdapter {
    config: OpenAiConfig,
    client: reqwest::Client,
}

impl OpenAiAdapter {
    /// Build an adapter from config.
    pub fn new(config: OpenAiConfig) -> Result<Self> {
        // One adapter call is one provider attempt: library-internal retries
        // stay disabled so the caller owns retry policy. The client bound is
        // connect-only — a full-response timeout would kill legitimate
        // long-running streams; steady emission is guarded by the per-read
        // idle watchdog inside `stream`.
        let client = reqwest::Client::builder()
            .connect_timeout(config.request_timeout)
            .build()
            .map_err(|e| {
                SeamError::new(
                    ErrorCode::ProviderFailure,
                    format!("failed to build HTTP client: {e}"),
                )
            })?;
        Ok(Self { config, client })
    }

    /// Model identifier sent as `model` in each request.
    pub fn model(&self) -> &str {
        &self.config.model
    }
}

/// Providers that speak the OpenAI interface report errors as 4xx/5xx JSON
/// bodies. Classify the message into the canonical codes; context overflow
/// is the one most worth routing on.
fn provider_failure_from(status: reqwest::StatusCode, body: &str) -> ProviderFailure {
    let parsed: std::result::Result<Value, _> = serde_json::from_str(body);
    let text = parsed
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str().map(|s| s.to_string()))
        })
        .unwrap_or_else(|| body.to_string());

    let lower = text.to_lowercase();
    let overflow_markers = [
        "context length",
        "maximum context",
        "context window",
        "too many tokens",
        "reduce the length",
        "token limit",
    ];
    if overflow_markers.iter().any(|m| lower.contains(m)) {
        ProviderFailure::context_overflow()
    } else {
        ProviderFailure {
            code: ErrorCode::ProviderFailure,
            message: if text.is_empty() {
                format!("provider returned {status}")
            } else {
                text
            },
        }
    }
}

/// Whether the endpoint returned an in-band SSE error event instead of
/// content: a JSON object carrying an `error` member.
fn in_band_error(chunk: &Value) -> Option<ProviderFailure> {
    chunk.get("error").map(|e| {
        let text = e
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("provider reported an in-band error")
            .to_string();
        let lower = text.to_lowercase();
        let overflow = ["context length", "maximum context", "context window"]
            .iter()
            .any(|m| lower.contains(m));
        if overflow {
            ProviderFailure::context_overflow()
        } else {
            ProviderFailure {
                code: ErrorCode::ProviderFailure,
                message: text,
            }
        }
    })
}

/// Build the request body from messages, tools, config, and replay state.
fn build_body(
    config: &OpenAiConfig,
    messages: &[Message],
    tools: &[ToolSchema],
    replay: Option<&ReplayState>,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(config.model));
    body.insert("messages".into(), Value::Array(request::messages(messages)));
    body.insert("stream".into(), json!(true));
    // Streaming usage accounting arrives with the final chunk when asked.
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    if let Some(tool_list) = request::tools(tools) {
        body.insert("tools".into(), tool_list);
    }
    // Replay state is adapter-private: only owner-verified response metadata
    // is merged back onto the request; the ownership marker itself never
    // goes on the wire.
    if let Some(state) = replay {
        if let Some(response) = &state.response {
            if let Some(obj) = response.as_object() {
                for (k, v) in obj {
                    if k != REPLAY_OWNER_KEY {
                        body.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }
    Value::Object(body)
}

impl ModelAdapter for OpenAiAdapter {
    fn provider(&self) -> &str {
        &self.config.identity
    }

    fn owns(&self, replay_state: &ReplayState) -> bool {
        replay_state
            .response
            .as_ref()
            .and_then(|r| r.get(REPLAY_OWNER_KEY))
            .and_then(|v| v.as_str())
            .map(|owner| owner == self.provider())
            .unwrap_or(false)
    }

    /// Stream a completion for `messages`. `call_id` correlates the call and
    /// seeds the seam call ids minted for the stream's tool calls, so two
    /// concurrent streams never mint colliding ids.
    fn stream(
        &self,
        call_id: CallId,
        messages: &[Message],
        tools: &[ToolSchema],
        replay: Option<ReplayState>,
    ) -> Result<BoxStream> {
        let body = build_body(&self.config, messages, tools, replay.as_ref());

        let url = self.config.chat_url();
        let client = self.client.clone();
        let identity = self.config.identity.clone();
        let idle_timeout = self.config.idle_timeout;
        let api_key = self.config.api_key.clone();
        let stream = stream! {
            let mut req = client
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(IDENTITY_HEADER, identity)
                .json(&body);
            if let Some(key) = &api_key {
                req = req.bearer_auth(key);
            }

            // A connect failure cannot throw from the entry once the stream
            // exists, so it normalizes to the in-band terminal path. The
            // caller owns retry policy either way.
            let response = match req.send().await {
                Ok(response) => response,
                Err(e) => {
                    yield StreamEvent::Failed(ProviderFailure {
                        code: ErrorCode::ProviderFailure,
                        message: e.to_string(),
                    });
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                // The client timeout is connect-only, so bound this body
                // read by the idle watchdog like any other read.
                let text = match tokio::time::timeout(idle_timeout, response.text()).await {
                    Ok(text) => text.unwrap_or_default(),
                    Err(_) => String::new(),
                };
                // Second sanctioned failure path: an in-band terminal error.
                yield StreamEvent::Failed(provider_failure_from(status, &text));
                return;
            }

            let mut bytes_stream = response.bytes_stream();
            let mut decoder = SseDecoder::new(call_id);
            let mut line_buf = String::new();
            let mut malformed = 0usize;

            loop {
                let next = tokio::time::timeout(idle_timeout, bytes_stream.next()).await;
                let item = match next {
                    Ok(Some(item)) => item,
                    // Clean end of body.
                    Ok(None) => break,
                    // The watchdog expired: the provider stopped sending
                    // bytes mid-stream. A hung provider is a timeout, never
                    // a frozen session.
                    Err(_) => {
                        yield StreamEvent::Failed(ProviderFailure {
                            code: ErrorCode::StreamTerminated,
                            message: format!(
                                "provider stalled for over {idle_timeout:?} mid-stream"
                            ),
                        });
                        return;
                    }
                };

                let bytes = match item {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        // Transport died mid-stream: in-band terminal error.
                        yield StreamEvent::Failed(ProviderFailure {
                            code: ErrorCode::StreamTerminated,
                            message: e.to_string(),
                        });
                        return;
                    }
                };

                // Canonicalize line endings before SSE framing: the protocol
                // terminates lines on \n, \r\n, and bare \r, and a pure-CRLF
                // stream must frame the same as a pure-LF one rather than
                // accumulating into an unbounded buffer.
                let text = String::from_utf8_lossy(&bytes);
                if text.contains('\r') {
                    let mut normalized = String::with_capacity(text.len());
                    let mut chars = text.chars().peekable();
                    while let Some(c) = chars.next() {
                        if c == '\r' {
                            // \r\n and bare \r both fold to \n.
                            if chars.peek() == Some(&'\n') {
                                chars.next();
                            }
                            normalized.push('\n');
                        } else {
                            normalized.push(c);
                        }
                    }
                    line_buf.push_str(&normalized);
                } else {
                    line_buf.push_str(&text);
                }

                // SSE events separate on a blank line; process what is
                // complete and keep the remainder in the buffer. Multiple
                // `data:` lines in one event are legal per the SSE spec and
                // join with \n before parsing.
                while let Some(pos) = line_buf.find("\n\n") {
                    let event = line_buf[..pos].to_string();
                    line_buf.drain(..pos + 2);

                    let mut data_lines: Vec<&str> = Vec::new();
                    for line in event.lines() {
                        if let Some(rest) = line.strip_prefix("data:") {
                            data_lines.push(rest.trim());
                        }
                    }
                    if data_lines.is_empty() {
                        continue;
                    }
                    let data = data_lines.join("\n");
                    let Ok(chunk) = serde_json::from_str::<Value>(&data) else {
                        // Malformed provider output is never silently
                        // dropped; it is counted and surfaced at terminal.
                        malformed += 1;
                        continue;
                    };
                    // An in-band provider error inside the stream.
                    if let Some(failure) = in_band_error(&chunk) {
                        yield StreamEvent::Failed(failure);
                        return;
                    }
                    for frame in decoder.push(&chunk) {
                        yield StreamEvent::Frame(frame);
                    }
                }
            }

            // Terminal: close leftover blocks, then usage before finish and
            // nothing after.
            for frame in decoder.finish() {
                yield StreamEvent::Frame(frame);
            }
            if let Some(usage) = decoder.usage() {
                yield StreamEvent::Frame(StreamFrame::Usage(usage));
            }
            if !decoder.had_content() {
                // An empty completion is a retryable failure, not a success.
                // Malformed data lines are named: content may have been lost
                // to decode failure rather than true provider silence.
                yield StreamEvent::Failed(ProviderFailure {
                    code: ErrorCode::EmptyCompletion,
                    message: if malformed > 0 {
                        format!(
                            "provider completed without emitting any content \
                             ({malformed} malformed data line(s) dropped)"
                        )
                    } else {
                        "provider completed without emitting any content".into()
                    },
                });
                return;
            }
            yield StreamEvent::Frame(StreamFrame::Finish);
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OpenAiConfig;
    use serde_json::json;

    fn test_config() -> OpenAiConfig {
        OpenAiConfig::new("http://127.0.0.1:1/v1", None, "m", "test-identity")
    }

    #[test]
    fn owns_requires_the_stamped_marker() {
        let adapter = OpenAiAdapter::new(test_config()).unwrap();

        let owned = ReplayState {
            response: Some(json!({ REPLAY_OWNER_KEY: "test-identity", "x": 1 })),
            blocks: vec![],
        };
        assert!(adapter.owns(&owned));

        let foreign = ReplayState {
            response: Some(json!({ REPLAY_OWNER_KEY: "someone-else" })),
            blocks: vec![],
        };
        assert!(!adapter.owns(&foreign));

        let unstamped = ReplayState {
            response: Some(json!({ "x": 1 })),
            blocks: vec![],
        };
        assert!(!adapter.owns(&unstamped));

        let none = ReplayState::default();
        assert!(!adapter.owns(&none));
    }

    #[test]
    fn replay_metadata_is_merged_minus_the_owner_marker() {
        let cfg = test_config();
        let replay = ReplayState {
            response: Some(json!({ REPLAY_OWNER_KEY: "id", "temperature": 0.5 })),
            blocks: vec![],
        };
        let body = build_body(&cfg, &[], &[], Some(&replay));
        assert_eq!(body["temperature"], 0.5);
        assert!(body.get(REPLAY_OWNER_KEY).is_none());
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn context_overflow_is_classified_canonically() {
        let f = provider_failure_from(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"This model's maximum context length is 4096 tokens"}}"#,
        );
        assert_eq!(f.code, ErrorCode::ContextOverflow);

        let f = provider_failure_from(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":{"message":"server exploded"}}"#,
        );
        assert_eq!(f.code, ErrorCode::ProviderFailure);
    }

    #[tokio::test]
    async fn dead_endpoint_fails_in_band() {
        // Port 1 refuses connections: the failure is normalized in-band.
        let adapter = OpenAiAdapter::new(test_config()).unwrap();
        let mut stream = adapter
            .stream(CallId(1), &[], &[], None)
            .expect("stream construction succeeds before connect");
        let first = futures::StreamExt::next(&mut stream).await;
        match first {
            Some(StreamEvent::Failed(f)) => assert_eq!(f.code, ErrorCode::ProviderFailure),
            other => panic!("expected in-band Failed, got {other:?}"),
        }
    }

    #[test]
    fn build_body_declares_tools_and_stream_options() {
        let cfg = test_config();
        let tools = vec![ToolSchema::new("t", "d", json!({}))];
        let body = build_body(&cfg, &[], &tools, None);
        assert_eq!(body["tools"][0]["function"]["name"], "t");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["model"], "m");
    }
}
