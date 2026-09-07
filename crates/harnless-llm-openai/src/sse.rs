//! Decoding OpenAI chat-completions SSE chunks into seam [`StreamFrame`]s.
//!
//! The decoder is stateful because deltas arrive as fragments across chunks
//! and a block is only complete when its `finish_reason` arrives. It assigns
//! seam block indices in the order blocks first appear, so interleaved text
//! and several concurrent tool calls reassemble deterministically (spec 81).

use std::collections::BTreeMap;

use serde_json::Value;

use harnless_seams::{BlockKind, CallId, ContentBlock, StreamFrame, Usage};

/// A tool call being accumulated from delta fragments.
#[derive(Clone)]
struct ToolCallBuild {
    /// Assigned seam block index.
    seam_index: usize,
    /// Assigned seam call id.
    call_id: CallId,
    /// The provider's tool-call id (`call_...`).
    id: String,
    /// The function name.
    name: String,
    /// Accumulated raw JSON argument fragments.
    arguments: String,
}

/// A streaming decoder that folds SSE data chunks into stream frames.
#[derive(Default)]
pub struct SseDecoder {
    /// Next seam block index to assign.
    next_index: usize,
    /// Open text block index, if any.
    text_index: Option<usize>,
    /// Text accumulated so far.
    text: String,
    /// Open reasoning block index, if any.
    reasoning_index: Option<usize>,
    /// Reasoning accumulated so far.
    reasoning: String,
    /// Open tool calls keyed by the OpenAI `tool_calls[].index`.
    tool_calls: BTreeMap<usize, ToolCallBuild>,
    /// The next seam call id to assign.
    next_call_id: u64,
    /// Usage reported in a chunk, if any.
    usage: Option<Usage>,
    /// Whether a finish reason has been observed (blocks closed).
    finished: bool,
}

impl SseDecoder {
    /// Create a decoder whose minted seam call ids ascend from `first`.
    /// Namespacing per stream keeps two concurrent streams from minting
    /// colliding tool-call ids.
    pub fn new(first: CallId) -> Self {
        Self {
            next_call_id: first.0,
            ..Self::default()
        }
    }

    /// Push one SSE `data:` JSON value, returning the frames it emits.
    ///
    /// Frames pushed after a finish reason are ignored: a delinquent
    /// provider trailing content past the terminal close must not re-open
    /// blocks or fabricate duplicate tool calls.
    pub fn push(&mut self, chunk: &Value) -> Vec<StreamFrame> {
        let mut frames = Vec::new();
        if self.finished {
            return frames;
        }

        // Usage may arrive on its own or with a choice.
        if let Some(usage) = chunk.get("usage").and_then(usage_from_value) {
            self.usage = Some(usage);
        }

        if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
            // n>1 completions would interleave two choices into one shared
            // index space silently; the harness requests n=1, and a choice
            // other than the first is ignored rather than mis-decoded.
            if let Some(choice) = choices.first() {
                if let Some(delta) = choice.get("delta") {
                    self.apply_delta(delta, &mut frames);
                }
                if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    if !finish.is_empty() && !self.finished {
                        self.close_blocks(&mut frames);
                        self.finished = true;
                    }
                }
            }
        }

        frames
    }

    /// Emit any frames still open at terminal. Idempotent after a finish.
    pub fn finish(&mut self) -> Vec<StreamFrame> {
        let mut frames = Vec::new();
        if !self.finished {
            self.close_blocks(&mut frames);
            self.finished = true;
        }
        frames
    }

    /// The usage gathered so far, if the provider reported it.
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Whether any content block (text, reasoning, or a tool call) was seen.
    pub fn had_content(&self) -> bool {
        !self.text.is_empty() || !self.reasoning.is_empty() || !self.tool_calls.is_empty()
    }

    /// Fold one choice delta.
    fn apply_delta(&mut self, delta: &Value, frames: &mut Vec<StreamFrame>) {
        // Text content.
        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                if self.text_index.is_none() {
                    let index = self.next_index;
                    self.next_index += 1;
                    self.text_index = Some(index);
                    frames.push(StreamFrame::BlockStart { index, kind: BlockKind::Text });
                }
                let index = self.text_index.unwrap();
                self.text.push_str(text);
                frames.push(StreamFrame::TextDelta { index, text: text.to_string() });
            }
        }

        // Reasoning content (DeepSeek-style). Assembles into one block and
        // closes alongside the other blocks at finish.
        if let Some(rev) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
            if !rev.is_empty() {
                if self.reasoning_index.is_none() {
                    let index = self.next_index;
                    self.next_index += 1;
                    self.reasoning_index = Some(index);
                    frames.push(StreamFrame::BlockStart {
                        index,
                        kind: BlockKind::Reasoning,
                    });
                }
                let index = self.reasoning_index.unwrap();
                self.reasoning.push_str(rev);
                frames.push(StreamFrame::ReasoningDelta {
                    index,
                    text: rev.to_string(),
                });
            }
        }

        // Tool calls.
        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for call in calls {
                self.apply_tool_call(call, frames);
            }
        }
    }

    /// Fold one `tool_calls[]` delta element.
    fn apply_tool_call(&mut self, call: &Value, frames: &mut Vec<StreamFrame>) {
        let tool_index = call.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        if !self.tool_calls.contains_key(&tool_index) {
            let seam_index = self.next_index;
            self.next_index += 1;
            self.next_call_id += 1;
            self.tool_calls.insert(
                tool_index,
                ToolCallBuild {
                    seam_index,
                    call_id: CallId(self.next_call_id),
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                },
            );
            frames.push(StreamFrame::BlockStart {
                index: seam_index,
                kind: BlockKind::ToolCall,
            });
        }

        let entry = self.tool_calls.get_mut(&tool_index).unwrap();
        if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
            entry.id.push_str(id);
        }
        if let Some(function) = call.get("function") {
            if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                entry.name.push_str(name);
            }
            if let Some(args) = function.get("arguments").and_then(|v| v.as_str()) {
                entry.arguments.push_str(args);
            }
        }

        // Only a delta that carries an argument fragment is pushed; the
        // id/name-opening frame contributes nothing to fold.
        if let Some(fragment) = call
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
        {
            frames.push(StreamFrame::ToolCallDelta {
                index: entry.seam_index,
                call_id: entry.call_id,
                json: fragment.to_string(),
            });
        }
    }

    /// Close all open blocks, emitting assembled [`StreamFrame::BlockEnd`]s.
    fn close_blocks(&mut self, frames: &mut Vec<StreamFrame>) {
        if let Some(index) = self.text_index {
            frames.push(StreamFrame::BlockEnd {
                index,
                assembled: ContentBlock {
                    kind: BlockKind::Text,
                    text: self.text.clone(),
                },
            });
            self.text_index = None;
        }

        if let Some(index) = self.reasoning_index {
            frames.push(StreamFrame::BlockEnd {
                index,
                assembled: ContentBlock {
                    kind: BlockKind::Reasoning,
                    text: self.reasoning.clone(),
                },
            });
            self.reasoning_index = None;
        }

        for entry in self.tool_calls.values() {
            frames.push(StreamFrame::BlockEnd {
                index: entry.seam_index,
                assembled: ContentBlock {
                    kind: BlockKind::ToolCall,
                    text: assembled_tool_json(&entry.id, &entry.name, &entry.arguments),
                },
            });
        }
        self.tool_calls.clear();
    }
}

/// Assemble the raw JSON for a tool call block: `{ id, name, arguments }`
/// with `arguments` preserved as the raw string the provider produced.
fn assembled_tool_json(id: &str, name: &str, arguments: &str) -> String {
    serde_json::json!({
        "id": id,
        "name": name,
        "arguments": arguments,
    })
    .to_string()
}

/// Map a provider usage object into disjoint seam [`Usage`].
///
/// Two cache-split shapes are honored: DeepSeek's flat
/// `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` and OpenAI's
/// `prompt_tokens_details.cached_tokens` (a read; the remainder of the
/// prompt is uncached input). A provider that reports no split counts the
/// whole prompt as uncached. `cached_writes` stays 0: no chat-completions
/// provider in scope reports cache-write tokens yet. Reasoning tokens are
/// read from the nested `completion_tokens_details.reasoning_tokens` with
/// a flat-key fallback.
pub fn usage_from_value(usage: &Value) -> Option<Usage> {
    let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let completion = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let hit = usage
        .get("prompt_cache_hit_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let miss = usage
        .get("prompt_cache_miss_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let openai_cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // Prefer an explicit miss count; otherwise subtract whatever reads the
    // provider declared from the prompt.
    let (uncached, cached_reads) = if miss > 0 {
        (miss, hit)
    } else if hit > 0 {
        (prompt.saturating_sub(hit), hit)
    } else if openai_cached > 0 {
        (prompt.saturating_sub(openai_cached), openai_cached)
    } else {
        (prompt, 0)
    };
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("reasoning_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0);
    Some(Usage {
        uncached_input: uncached,
        cached_reads,
        cached_writes: 0,
        output: completion,
        reasoning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_content_streams_one_block() {
        let mut d = SseDecoder::new(CallId(0));
        let mut frames = d.push(&json!({"choices":[{"delta":{"content":"hel"}}]}));
        frames.extend(d.push(&json!({
            "choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]
        })));
        assert_eq!(
            frames[0],
            StreamFrame::BlockStart { index: 0, kind: BlockKind::Text }
        );
        assert_eq!(frames[1], StreamFrame::TextDelta { index: 0, text: "hel".into() });
        assert_eq!(frames[2], StreamFrame::TextDelta { index: 0, text: "lo".into() });
        assert_eq!(
            frames[3],
            StreamFrame::BlockEnd {
                index: 0,
                assembled: ContentBlock { kind: BlockKind::Text, text: "hello".into() },
            }
        );
    }

    #[test]
    fn tool_call_fragments_reassemble_raw_json() {
        let mut d = SseDecoder::new(CallId(0));
        let mut frames = d.push(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": "search", "arguments": "{\"q\":"}
            }]}}]
        }));
        frames.extend(d.push(&json!({
            "choices": [{
                "delta": {"tool_calls": [{
                    "index": 0, "function": {"arguments": "\"x\"}"}
                }]},
                "finish_reason": "tool_calls"
            }]
        })));

        // A ToolCallDelta was emitted for the first fragment.
        assert!(frames.iter().any(|f| matches!(
            f,
            StreamFrame::ToolCallDelta { index: 0, call_id, json }
                if json == "{\"q\":" && *call_id == CallId(1)
        )));

        let end = frames.last().unwrap();
        match end {
            StreamFrame::BlockEnd { index, assembled } => {
                assert_eq!(*index, 0);
                assert_eq!(assembled.kind, BlockKind::ToolCall);
                let raw: Value = serde_json::from_str(&assembled.text).unwrap();
                assert_eq!(raw["id"], "call_1");
                assert_eq!(raw["name"], "search");
                assert_eq!(raw["arguments"], "{\"q\":\"x\"}");
            }
            other => panic!("expected BlockEnd, got {other:?}"),
        }
    }

    #[test]
    fn interleaved_text_and_two_tool_calls_get_distinct_indices() {
        let mut d = SseDecoder::new(CallId(0));
        let mut frames = d.push(&json!({
            "choices": [{"delta": {"content": "planning"}}]
        }));
        let empty_args = "{}";
        frames.extend(d.push(&json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_a",
                 "function": {"name": "f1", "arguments": empty_args}},
                {"index": 1, "id": "call_b",
                 "function": {"name": "f2", "arguments": empty_args}}
            ]}}]
        })));
        frames.extend(d.push(&json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}]
        })));

        let starts = frames
            .iter()
            .filter_map(|f| match f {
                StreamFrame::BlockStart { index, kind } => Some((*index, *kind)),
                _ => None,
            })
            .collect::<Vec<_>>();
        // Text at 0, tool call a at 1, tool call b at 2.
        assert_eq!(starts, vec![
            (0, BlockKind::Text),
            (1, BlockKind::ToolCall),
            (2, BlockKind::ToolCall),
        ]);
    }

    #[test]
    fn usage_is_disjoint_and_split_when_provider_splits_cache() {
        let u = usage_from_value(&json!({
            "prompt_tokens": 50,
            "completion_tokens": 7,
            "prompt_cache_hit_tokens": 20,
            "prompt_cache_miss_tokens": 30,
        }))
        .unwrap();
        assert_eq!(u.uncached_input, 30);
        assert_eq!(u.cached_reads, 20);
        assert_eq!(u.cached_writes, 0);
        assert_eq!(u.output, 7);
        assert_eq!(u.billed_input(), 50);
    }

    #[test]
    fn usage_falls_back_to_whole_prompt_when_not_split() {
        let u = usage_from_value(&json!({
            "prompt_tokens": 40,
            "completion_tokens": 5,
        }))
        .unwrap();
        assert_eq!(u.uncached_input, 40);
        assert_eq!(u.cached_reads, 0);
        assert_eq!(u.billed_input(), 40);
    }
    #[test]
    fn deltas_after_finish_never_reopen_blocks() {
        let mut d = SseDecoder::new(CallId(0));
        d.push(&json!({
            "choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]
        }));
        // A delinquent provider trailing content after the terminal close.
        let frames = d.push(&json!({
            "choices":[{"delta":{"content":"ghost"},
                        "tool_calls":[{"index":0,"id":"call_x",
                                       "function":{"arguments":"{}"}}]}]
        }));
        assert!(frames.is_empty());
        assert!(d.had_content());
    }

    #[test]
    fn second_choice_is_ignored_not_interleaved() {
        let mut d = SseDecoder::new(CallId(0));
        let frames = d.push(&json!({
            "choices":[
                {"delta":{"content":"first"}},
                {"delta":{"content":"second"}}
            ]
        }));
        // Only the first choice's text participates: one BlockStart, one TextDelta.
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| match f {
            StreamFrame::BlockStart { index, .. }
            | StreamFrame::TextDelta { index, .. } => *index == 0,
            _ => false,
        }));
    }

    #[test]
    fn openai_cached_tokens_are_read_from_nested_details() {
        let u = usage_from_value(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 60},
            "completion_tokens_details": {"reasoning_tokens": 4},
        }))
        .unwrap();
        assert_eq!(u.cached_reads, 60);
        assert_eq!(u.uncached_input, 40);
        assert_eq!(u.reasoning, 4);
        assert_eq!(u.billed_input(), 100);
    }

    #[test]
    fn flat_reasoning_tokens_still_read() {
        let u = usage_from_value(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "reasoning_tokens": 3,
        }))
        .unwrap();
        assert_eq!(u.reasoning, 3);
    }
}
