//! Derived message history.
//!
//! Message history is **derived, never stored**: a projection walks the
//! recorded surface and yields the message list the model sees. Replaying,
//! forking, and persisting all read the same stream.
//!
//! Only the three message-producing event kinds declare how they join the
//! surface. Structural records (boundaries, chunks, usage) never project a
//! message. Raw streamed chunks are replay and presentation data, excluded
//! from derivation — the assembled message is authoritative.
//!
//! Two surface operations: append to the tail, or replace an inclusive range
//! of surface nodes. A replacement must cite every node it retires.
//!
//! Projection is cached per surface node and rebuilt when a replacement
//! lands — deriving costs new nodes, never the whole log.

use dsh_seams::MessageId;

use crate::events::{ContentBlock, SessionEvent};

/// One node in the derived surface.
///
/// Immutable once placed; caching is per node so a held reference cannot be
/// mutated by a later append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceNode {
    /// The message id this node projects (from a message-producing event).
    pub message_id: MessageId,
    /// The assembled content blocks.
    pub blocks: Vec<ContentBlock>,
}

/// The derived message history, projected from the session log.
///
/// The projection is cached per node; each append adds at most one node (for
/// a message-producing event) and a replacement retires the named nodes then
/// inserts the new ones.
#[derive(Debug, Default, Clone)]
pub struct History {
    nodes: Vec<SurfaceNode>,
    /// Monotonic allocator for synthetic message ids (tool-result nodes).
    next_synthetic: u64,
}

impl History {
    /// The current derived message list, in surface order.
    pub fn nodes(&self) -> &[SurfaceNode] {
        &self.nodes
    }

    /// The number of surface nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the surface is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Fold a committed event into the surface.
    ///
    /// Only message-producing events add a node; everything else is a no-op
    /// for derivation. Returns `Some(node)` when a node was appended.
    pub fn apply(&mut self, event: &SessionEvent) -> Option<&SurfaceNode> {
        let node = match event {
            SessionEvent::UserMessage(m) => SurfaceNode {
                message_id: m.id,
                blocks: m.blocks.clone(),
            },
            SessionEvent::AssistantMessage(m) => SurfaceNode {
                message_id: m.id,
                blocks: m.blocks.clone(),
            },
            SessionEvent::ToolResult(r) => {
                let content = ContentBlock::ToolResult {
                    call_id: r.call_id,
                    content: r.content.clone(),
                };
                // A tool result is a distinct model-visible message; give it
                // a synthetic identity so it is addressable in the surface.
                self.next_synthetic += 1;
                SurfaceNode {
                    message_id: MessageId(self.next_synthetic),
                    blocks: vec![content],
                }
            }
            _ => return None,
        };
        self.nodes.push(node);
        self.nodes.last()
    }

    /// Replace an inclusive range `[start, end]` of nodes with `replacement`.
    ///
    /// `retired` must cite every node being replaced; if it does not match
    /// the node ids in the range, the replacement is refused so nothing
    /// leaves the model's view unaccounted for. Returns `Ok(())` on success
    /// or `Err` when the cited nodes do not match the range.
    pub fn replace(
        &mut self,
        start: usize,
        end: usize,
        retired: &[MessageId],
        replacement: Vec<SurfaceNode>,
    ) -> std::result::Result<(), String> {
        if start > end || end >= self.nodes.len() {
            return Err("range out of bounds".into());
        }
        let cited: Vec<MessageId> = self.nodes[start..=end]
            .iter()
            .map(|n| n.message_id)
            .collect();
        if cited != retired {
            return Err("retired nodes do not match the replaced range".into());
        }
        self.nodes.splice(start..=end, replacement);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{MessageRecord, ToolResultRecord};
    use dsh_seams::{CallId, MessageId};

    fn msg(id: u64) -> MessageRecord {
        MessageRecord {
            id: MessageId(id),
            blocks: vec![ContentBlock::Text {
                text: format!("m{id}"),
            }],
            provider: None,
            model: None,
        }
    }

    #[test]
    fn only_message_producing_kinds_project_nodes() {
        let mut h = History::default();
        assert!(h.apply(&crate::events::SessionEvent::TurnOpen).is_none());
        assert!(h.apply(&crate::events::SessionEvent::StepOpen).is_none());
        assert!(h
            .apply(&crate::events::SessionEvent::UserMessage(msg(1)))
            .is_some());
        assert!(h
            .apply(&crate::events::SessionEvent::AssistantMessage(msg(2)))
            .is_some());
        // Structural and chunk records never project.
        assert!(h
            .apply(&crate::events::SessionEvent::AssistantChunk(
                crate::events::ChunkRecord {
                    message_id: MessageId(2),
                    block_index: 0,
                    delta: "x".into(),
                }
            ))
            .is_none());
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn replace_must_cite_every_retired_node() {
        let mut h = History::default();
        h.apply(&crate::events::SessionEvent::UserMessage(msg(1)));
        h.apply(&crate::events::SessionEvent::UserMessage(msg(2)));
        // Wrong citations -> refused.
        let err = h.replace(0, 1, &[MessageId(1)], vec![]);
        assert!(err.is_err());
        // Correct citations -> succeeds and retires both.
        let ok = h.replace(0, 1, &[MessageId(1), MessageId(2)], vec![SurfaceNode {
            message_id: MessageId(9),
            blocks: vec![],
        }]);
        assert!(ok.is_ok());
        assert_eq!(h.len(), 1);
        assert_eq!(h.nodes()[0].message_id, MessageId(9));
    }

    #[test]
    fn tool_result_projects_a_synthetic_node() {
        let mut h = History::default();
        let node = h.apply(&crate::events::SessionEvent::ToolResult(ToolResultRecord {
            call_id: CallId(4),
            content: "{}".into(),
        }));
        assert!(node.is_some());
        assert_eq!(h.len(), 1);
    }
}
