//! The one-shot runner: drive exactly one agent turn and print its answer.
//!
//! The runner is the headless entry: mount the profile, append the user
//! prompt to the session log, drive one [`AgentLoop`] turn through the
//! composed model, and print the assistant message. A profile with no model
//! provider is a named `no-model-provider` failure, never a silent empty
//! turn. A turn that ends in a provider failure is a named `turn-failed`
//! error carrying the structured cause — the session log keeps the failed
//! turn's bracket and prompt, and a later turn on the same composition
//! still runs.
//!
//! A model-requested tool call executes through the mounted pipeline and
//! its frozen result lands in the log; the CLI renders tool blocks as
//! plain text (`render_block`), never as structured output — the tool
//! round-trip is a log fact, not a terminal one.

use std::sync::Arc;

use futures::stream::Stream as _;
use harnless_agent::events::{
    ContentBlock, MessageRecord, SessionEvent, ToolCallRecord, TurnEndReason,
};
use harnless_agent::loop_::{AgentLoop, Driver, DriverOutcome};
use harnless_agent::session::SessionLog;
use harnless_seams::{
    BlockAssembler, BlockKind, CallId, ErrorCode, Message, ProviderFailure, Role, StreamEvent,
};

use crate::boot::{BootComposer, DefaultComposer, Mounted};
use crate::model::ModelHandle;
use crate::CliError;

/// Run one headless turn for profile `profile` against `prompt`.
///
/// Returns the assistant text on success. Errors carry a stable code the
/// shell can route on.
pub fn run_once(
    composer: &dyn BootComposer,
    profile: &str,
    patch: Option<&str>,
    prompt: &str,
) -> Result<String, CliError> {
    let doc = composer.compose(profile, patch)?;
    let mounted = composer.mount(&doc)?;
    drive_turn(&mounted, prompt)
}

/// Run with the built-in composer.
pub fn run_default(profile: &str, patch: Option<&str>, prompt: &str) -> Result<String, CliError> {
    run_once(&DefaultComposer, profile, patch, prompt)
}

/// The model-facing tool schemas for an adapter request.
///
/// The registry's schemas ride on every request the CLI drives — that is the
/// route by which a declared tool reaches the adapter. A composition without
/// tools requests with no schemas, which is today's wire shape.
fn request_schemas(mounted: &Mounted) -> Vec<harnless_seams::ToolSchema> {
    mounted
        .tools
        .as_ref()
        .map(|tools| harnless_seams::Tools::schemas(tools.as_ref()))
        .unwrap_or_default()
}

/// Drive one turn on a live composition and return the assistant text.
///
/// The user prompt is logged first (it is model-visible), then the loop runs
/// a single step against the composed model.
pub fn drive_turn(mounted: &Mounted, prompt: &str) -> Result<String, CliError> {
    let model: ModelHandle = mounted.model.clone().ok_or_else(|| {
        CliError::new(
            "no-model-provider",
            "this profile composes no model provider; run `hrls profile list` \
             for available profiles or patch in a model",
        )
    })?;

    let loop_ = mounted
        .ctx
        .get::<AgentLoop>()
        .ok_or_else(|| CliError::new("mount-failed", "agent loop service is not mounted"))?;

    // The prompt is model-visible, so it belongs in the log before the turn.
    let log = mounted
        .ctx
        .get::<SessionLog>()
        .expect("spine provides the log");
    log.append(SessionEvent::UserMessage(MessageRecord {
        id: mounted.ids.message(),
        blocks: vec![ContentBlock::Text {
            text: prompt.to_string(),
        }],
        provider: None,
        model: None,
    }));

    // One turn, possibly many adapter calls: the loop owns the log, so the
    // driver re-reads the logged surface before every step and the loop
    // commits each assistant message and tool round-trip itself. The CLI
    // never re-implements the loop's commitment logic.
    let turn = loop_.run_turn(make_driver(
        model,
        Arc::clone(&log),
        mounted.ids.clone(),
        request_schemas(mounted),
    ));
    let text = final_assistant_text(&log);
    match turn.reason {
        TurnEndReason::Completed => Ok(text),
        TurnEndReason::Error { code, message } => {
            Err(CliError::new("turn-failed", format!("{code}: {message}")))
        }
        other => Err(CliError::new(
            "turn-failed",
            format!("turn ended with {other:?}"),
        )),
    }
}

/// Project the logged surface into the seam message list the adapter sees.
fn model_messages(log: &SessionLog) -> Vec<Message> {
    log.snapshot()
        .records
        .iter()
        .filter_map(|r| match &r.event {
            SessionEvent::UserMessage(m) => Some(seam_message(m, Role::User)),
            SessionEvent::AssistantMessage(m) => Some(seam_message(m, Role::Assistant)),
            _ => None,
        })
        .collect()
}

/// Convert one logged message record to a seam message.
fn seam_message(record: &MessageRecord, role: Role) -> Message {
    Message {
        id: record.id,
        role,
        blocks: record
            .blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => harnless_seams::ContentBlock {
                    kind: BlockKind::Text,
                    text: text.clone(),
                },
                ContentBlock::Reasoning { text } => harnless_seams::ContentBlock {
                    kind: BlockKind::Reasoning,
                    text: text.clone(),
                },
                ContentBlock::ToolCall { call_id, arguments } => harnless_seams::ContentBlock {
                    kind: BlockKind::ToolCall,
                    text: format!(
                        r#"{{"call_id":{},"arguments":{}}}"#,
                        call_id.0,
                        quote(arguments)
                    ),
                },
                ContentBlock::ToolResult { call_id, content } => harnless_seams::ContentBlock {
                    kind: BlockKind::Text,
                    text: format!("[tool result {call_id}] {content}"),
                },
            })
            .collect(),
        provider: record.provider.clone(),
        model: record.model.clone(),
        replay_state: None,
    }
}

/// JSON-quote a raw string.
fn quote(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Build the loop driver: one adapter call whose stream folds into the
/// assistant message the loop commits.
fn make_driver(
    model: ModelHandle,
    log: Arc<SessionLog>,
    ids: crate::boot::Ids,
    schemas: Vec<harnless_seams::ToolSchema>,
) -> Driver {
    Box::new(move || loop {
        // The model-visible list is re-read per step: the loop commits each
        // assistant message and tool round-trip to the log as the turn
        // proceeds, so the next adapter call sees the whole surface.
        let messages = model_messages(&log);
        let mut stream: harnless_seams::BoxStream =
            match model.stream(ids.call(), &messages, &schemas, None) {
                Ok(stream) => stream,
                Err(err) => return stop_error(err.code, &err.message),
            };
        // The replay stream never blocks; poll it with a noop waker, the
        // same shape the replay crate's seam test uses.
        let mut assembler = BlockAssembler::new();
        let mut failure: Option<ProviderFailure> = None;
        let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
        loop {
            match std::pin::Pin::new(&mut stream).poll_next(&mut cx) {
                std::task::Poll::Ready(Some(StreamEvent::Frame(frame))) => assembler.push(&frame),
                std::task::Poll::Ready(Some(StreamEvent::Failed(f))) => failure = Some(f),
                std::task::Poll::Ready(None) => break,
                std::task::Poll::Pending => {
                    failure = Some(ProviderFailure {
                        code: ErrorCode::StreamTerminated,
                        message: "model stream blocked".into(),
                    });
                    break;
                }
            }
        }
        if let Some(f) = failure {
            return stop_error(f.code, &f.message);
        }
        let mut tool_call: Option<(CallId, String, String)> = None;
        let mut blocks = Vec::new();
        for b in assembler.blocks() {
            match b.kind {
                BlockKind::Reasoning => blocks.push(ContentBlock::Reasoning { text: b.text }),
                BlockKind::ToolCall => {
                    // The assembled tool-call block carries the provider's
                    // raw-JSON envelope; the loop executes the call.
                    let value: serde_json::Value = match serde_json::from_str(&b.text) {
                        Ok(v) => v,
                        Err(e) => {
                            return stop_error(
                                ErrorCode::StreamTerminated,
                                &format!("malformed tool-call block: {e}"),
                            )
                        }
                    };
                    let name = value
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let arguments = value
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let call_id = value
                        .get("id")
                        .and_then(|v| v.as_u64())
                        .map(CallId)
                        .unwrap_or_else(|| ids.call());
                    tool_call = Some((call_id, name, arguments));
                }
                _ => blocks.push(ContentBlock::Text { text: b.text }),
            }
        }
        if let Some((call_id, tool, arguments)) = tool_call {
            return DriverOutcome::ToolCall(ToolCallRecord {
                call_id,
                tool,
                arguments,
            });
        }
        return DriverOutcome::Message(MessageRecord {
            id: ids.message(),
            blocks,
            provider: Some(model.provider().to_string()),
            model: Some("replay".into()),
        });
    })
}

/// A driver stop carrying a normalized provider failure as the turn error.
fn stop_error(code: ErrorCode, message: &str) -> DriverOutcome {
    DriverOutcome::Stop(TurnEndReason::Error {
        code: code.to_string(),
        message: message.to_string(),
    })
}

/// The text of the last assistant message committed to `log`.
fn final_assistant_text(log: &SessionLog) -> String {
    log.snapshot()
        .records
        .iter()
        .rev()
        .find_map(|r| match &r.event {
            SessionEvent::AssistantMessage(m) => Some(
                m.blocks
                    .iter()
                    .map(render_block)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// Render one content block for terminal display.
pub fn render_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Reasoning { text } => format!("[reasoning] {text}"),
        ContentBlock::ToolCall { call_id, arguments } => {
            format!("[tool call {call_id}] {arguments}")
        }
        ContentBlock::ToolResult { call_id, content } => {
            format!("[tool result {call_id}] {content}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_runs_one_turn_with_no_network() {
        let text = run_default("default", None, "hi").expect("run");
        assert!(text.contains("Hello from the harnless replay model."));
    }

    #[test]
    fn patched_none_model_reports_no_provider() {
        let err = run_default("default", Some("model:\n  kind: none\n"), "hi").unwrap_err();
        assert_eq!(err.code, "no-model-provider");
    }

    #[test]
    fn unknown_profile_names_the_error() {
        let err = run_default("missing", None, "hi").unwrap_err();
        assert_eq!(err.code, "unknown-profile");
    }

    #[test]
    fn user_prompt_is_logged_before_the_turn() {
        let composer = DefaultComposer;
        let doc = composer.compose("default", None).unwrap();
        let mounted = composer.mount(&doc).unwrap();
        let _ = drive_turn(&mounted, "question").unwrap();
        let loop_ = mounted.ctx.get::<AgentLoop>().unwrap();
        let kinds: Vec<String> = loop_
            .log()
            .snapshot()
            .records
            .iter()
            .map(|r| match &r.event {
                SessionEvent::UserMessage(_) => "user",
                SessionEvent::TurnOpen => "open",
                SessionEvent::AssistantMessage(_) => "assistant",
                SessionEvent::TurnClose { .. } => "close",
                _ => "other",
            })
            .map(String::from)
            .collect();
        assert_eq!(
            kinds,
            vec!["user", "open", "other", "assistant", "other", "close"]
        );
    }
}
