//! System-prompt assembly.
//!
//! The loop assembles the model-visible prompt from the derived history. The
//! base system prompt is caller-supplied (the "shipped composition"), and the
//! assembled prompt folds in the derived conversation surface so the model
//! sees exactly what a projection of the log yields — never a separately
//! stored history.

use crate::events::{ContentBlock, SessionEvent};
use crate::history::History;
use crate::session::SessionLog;

/// The assembled system prompt: a base directive plus the derived
/// conversation.
#[derive(Debug, Clone)]
pub struct SystemPrompt {
    /// The base system directive.
    pub base: String,
    /// The derived conversation lines, in surface order.
    pub conversation: Vec<String>,
}

/// Assemble a system prompt from a base directive and the derived history of
/// `log`.
///
/// The conversation is derived fresh from the committed events each call, so
/// it can never drift from the log.
pub fn assemble(base: &str, log: &SessionLog) -> SystemPrompt {
    let mut history = History::default();
    for record in log.snapshot().records {
        history.apply(&record.event);
        let _ = record;
    }
    let conversation = render_nodes(&history);
    SystemPrompt {
        base: base.to_string(),
        conversation,
    }
}

/// Render the derived surface nodes into display conversation lines.
fn render_nodes(history: &History) -> Vec<String> {
    history
        .nodes()
        .iter()
        .map(|node| {
            let text: Vec<String> = node
                .blocks
                .iter()
                .map(render_block)
                .collect();
            text.join("\n")
        })
        .collect()
}

/// Render one content block to a display line.
fn render_block(block: &ContentBlock) -> String {
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

/// Whether an event participates in system-prompt assembly.
///
/// Reuses the message-producing rule so the prompt never includes structural
/// records.
pub fn in_prompt(event: &SessionEvent) -> bool {
    event.is_message_producing()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnless_seams::{MessageId, SessionId};

    fn log_with_message_and_structure() -> SessionLog {
        let log = SessionLog::new(SessionId(1));
        let msg = crate::events::MessageRecord {
            id: MessageId(1),
            blocks: vec![ContentBlock::Text {
                text: "hello model".into(),
            }],
            provider: None,
            model: None,
        };
        log.append(SessionEvent::TurnOpen); // structural: excluded
        log.append(SessionEvent::UserMessage(msg)); // projects
        log
    }

    #[test]
    fn assemble_renders_only_message_producing_events() {
        let log = log_with_message_and_structure();
        let sys = assemble("you are a harness", &log);
        assert_eq!(sys.base, "you are a harness");
        // Only the user message projects; TurnOpen is excluded.
        assert_eq!(sys.conversation.len(), 1);
        assert_eq!(sys.conversation[0], "hello model");
    }

    #[test]
    fn in_prompt_is_message_producing_only() {
        assert!(in_prompt(&SessionEvent::UserMessage(crate::events::MessageRecord {
            id: MessageId(1),
            blocks: vec![],
            provider: None,
            model: None,
        })));
        assert!(!in_prompt(&SessionEvent::TurnOpen));
        assert!(!in_prompt(&SessionEvent::StepOpen));
    }
}
