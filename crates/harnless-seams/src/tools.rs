//! The tools seam: the tool registry and its guarded execution pipeline.
//!
//! The locked order (07): a pre-execute waterfall (reorderable
//! allow/deny/ask) → registered monotonic guards (deny or abstain; identity
//! protected, cannot be reordered around) → an execute waterfall
//! (around-dispatch: timeout, retry, metrics) → the tool body → a post-execute
//! waterfall (accept, block, replace, add context) → definition-owned
//! finalization → a frozen result notification.
//!
//! Only the around-dispatch stage may replace the caller's cancellation
//! signal. The final outcome notification is fire-and-forget. Approval is a
//! seam, and absence means refusal: a one-shot permission decision dispatched
//! as a waterfall; if nobody answers, or the answer seam is unmounted, the
//! call is denied — failing closed is the contract.

use std::sync::Arc;

use serde_json::Value;

use crate::error::Result;
use crate::ids::CallId;

/// A tool: a named operation with a JSON schema and a body.
///
/// Tool arguments are raw JSON strings end to end; the schema describes how
/// to validate them at the boundary.
///
/// Not every field of a definition is model-facing. Only
/// [`ToolDefinition::to_schema`] crosses to the provider, and it carries
/// name, description, and parameters — so the fields an adapter must never
/// see (`serialized`, and anything added later) are excluded by construction,
/// not by review.
#[derive(Clone)]
pub struct ToolDefinition {
    /// The tool name, namespaced under its provider.
    pub name: String,
    /// The model-facing text describing what the tool does.
    ///
    /// This is part of the allowlist: it is one of the three fields
    /// [`ToolDefinition::to_schema`] projects onto the wire, and a provider
    /// decides whether and how to call a tool largely from it. A tool that
    /// arrives from a source with no description (a legacy plugin
    /// descriptor, a server that omitted the field) carries an empty string,
    /// never a fabricated one.
    pub description: String,
    /// The JSON schema for the tool's arguments.
    pub schema: Value,
    /// Whether the tool carries stateful-call serialization requirements.
    ///
    /// Internal scheduling metadata; never model-facing. It has no place in
    /// [`crate::llm::ToolSchema`] and cannot reach one through
    /// [`ToolDefinition::to_schema`].
    pub serialized: bool,
}

impl ToolDefinition {
    /// Project this definition onto the model-facing
    /// [`crate::llm::ToolSchema`] — the *one* place a registry definition
    /// becomes something an adapter may send to a provider.
    ///
    /// The allowlist is a code path, not a comment: the body names exactly
    /// the three fields a provider is permitted to see (name, description,
    /// parameters) and leaves `strict` at its default. `serialized` — and
    /// any internal field added to this struct later — is excluded because
    /// this function never mentions it, so widening [`ToolDefinition`] can
    /// never silently widen the wire shape. Adapters take
    /// [`crate::llm::ToolSchema`], never a definition, so there is no other
    /// route from registry to request.
    ///
    /// [`crate::llm::ToolSchema`]: crate::llm::ToolSchema
    pub fn to_schema(&self) -> crate::llm::ToolSchema {
        crate::llm::ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.schema.clone(),
            strict: false,
        }
    }
}

/// The tool body: the actual executable operation.
pub trait ToolBody: Send + Sync + 'static {
    /// Execute the tool against raw-JSON `args`, returning raw-JSON result.
    fn run(&self, call_id: CallId, args: &[u8]) -> Result<Value>;
}

/// The three policy stages and around-dispatch stage, plus the final
/// notification. Each is a discrete extension point a policy or tool plugin
/// may register on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PipelineStage {
    /// Reorderable allow/deny/ask (waterfall, veto contract).
    PreExecute,
    /// Around-dispatch (waterfall: timeout, retry, metrics). The only stage
    /// that may replace the caller's cancellation signal.
    Execute,
    /// Accept, block, replace, add context (waterfall, veto contract).
    PostExecute,
    /// The frozen result notification (fire-and-forget).
    Notify,
}

/// A decision the pre-execute stage can yield.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreDecision {
    Allow,
    Deny(String),
    Ask,
}

/// A decision the post-execute stage can yield.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostDecision {
    Accept,
    Block(String),
    Replace(Value),
    AddContext(Vec<Value>),
}

/// A monotonic guard: deny or abstain. Its identity is protected and it
/// cannot be reordered around by the pre-execute waterfall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    Deny(String),
    Abstain,
}

/// A frozen tool result, produced once by finalization and then immutable.
#[derive(Debug, Clone, PartialEq)]
pub struct FrozenResult {
    /// The call this result answers.
    pub call_id: CallId,
    /// Raw-JSON canonical result.
    pub value: Value,
}

/// The tools seam — the service definition role only. The concrete registry
/// (the spine) lives in `harnless-agent`; providers implement [`ToolBody`] and
/// register on the registry.
pub trait Tools: Send + Sync + 'static {
    /// Register a tool definition and its body under `name`.
    fn register(&self, def: ToolDefinition, body: Arc<dyn ToolBody>) -> Result<()>;

    /// Look up a tool's definition by name.
    fn get(&self, name: &str) -> Option<ToolDefinition>;

    /// Enumerate tools by name.
    fn names(&self) -> Vec<String>;
}

/// The policy home for the guarded pipeline: default confinement mode and
/// workspace root are shared with the filesystem and execution world so the
/// three can never confine to different roots.
pub use crate::exec::PolicyHome as PipelinePolicy;
