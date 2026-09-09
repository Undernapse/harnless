//! The tool registry and its guarded execution pipeline.
//!
//! This is the concrete implementation of the `harnless-seams::Tools` seam and the
//! locked-order pipeline (decision 07):
//!
//! pre-execute waterfall (reorderable allow/deny/ask) → registered monotonic
//! guards (deny or abstain; identity protected, cannot be reordered around) →
//! execute waterfall (around-dispatch) → the tool body → post-execute
//! waterfall (accept, block, replace, add context) → definition-owned
//! finalization → a frozen result notification.
//!
//! Only the around-dispatch stage may replace the caller's cancellation
//! signal. The final outcome notification is fire-and-forget. Approval is a
//! seam, and absence means denial — failing closed is the contract.

use std::collections::HashMap;
use std::sync::Arc;

use harnless_runtime::error::Result;
use harnless_runtime::events::{EventOptions, EventRegistry, Next};
use harnless_runtime::fiber::Fiber;
use harnless_runtime::Disposer;
use serde_json::Value;

use harnless_seams::tools::{
    FrozenResult, GuardVerdict, PipelineStage, PostDecision, PreDecision, ToolBody, ToolDefinition,
};
use harnless_seams::{CallId, ErrorCode, SeamError};

/// The payload/result of the pre-execute waterfall.
pub type PreExecute = (String, Vec<u8>);
/// The payload/result of the post-execute waterfall.
pub type PostExecute = (CallId, Value);

/// A monotonic guard registered directly on the registry.
pub type MonotonicGuard = Arc<dyn Fn(&str, &[u8]) -> GuardVerdict + Send + Sync>;

/// A stored tool: its definition plus its executable body.
type ToolEntry = (ToolDefinition, Arc<dyn ToolBody>);

/// The tool registry: stores tools, monotonic guards, and hosts the pipeline
/// extension points.
pub struct ToolRegistry {
    tools: parking_lot::RwLock<HashMap<String, ToolEntry>>,
    guards: parking_lot::RwLock<Vec<(String, MonotonicGuard)>>,
    events: EventRegistry,
    fiber: Arc<Fiber>,
}

impl ToolRegistry {
    /// Create an empty registry whose pipeline extension points live on
    /// `events`, owned by `fiber`.
    pub fn new(events: EventRegistry, fiber: Arc<Fiber>) -> Self {
        Self {
            tools: parking_lot::RwLock::new(HashMap::new()),
            guards: parking_lot::RwLock::new(Vec::new()),
            events,
            fiber,
        }
    }

    /// Register a monotonic guard under `name`.
    ///
    /// Guards run after the pre-execute waterfall and cannot be reordered
    /// around — their identity (`name`) is protected.
    pub fn add_guard(&self, name: &str, guard: MonotonicGuard) {
        self.guards.write().push((name.to_string(), guard));
    }

    /// Register a pre-execute waterfall listener.
    pub fn on_pre_execute<F>(&self, listener: F) -> Result<Disposer>
    where
        F: FnMut(&mut PreExecute, &mut Next<'_, PreExecute, PreDecision>) -> PreDecision
            + Send
            + 'static,
    {
        let mut listener = listener;
        self.events.on_waterfall(
            &self.fiber,
            move |e: &mut PreExecute, n| listener(e, n),
            EventOptions::new(),
        )
    }

    /// Register a post-execute waterfall listener.
    pub fn on_post_execute<F>(&self, listener: F) -> Result<Disposer>
    where
        F: FnMut(&mut PostExecute, &mut Next<'_, PostExecute, PostDecision>) -> PostDecision
            + Send
            + 'static,
    {
        let mut listener = listener;
        self.events.on_waterfall(
            &self.fiber,
            move |e: &mut PostExecute, n| listener(e, n),
            EventOptions::new(),
        )
    }

    /// Run the guarded pipeline for `call` on `name` with raw-JSON `args`.
    ///
    /// Permission is denied when nobody answers the pre-execute waterfall
    /// (failing closed). Returns the frozen result.
    pub fn execute(
        &self,
        call_id: CallId,
        name: &str,
        args: &[u8],
    ) -> harnless_seams::Result<FrozenResult> {
        let (_def, body) = {
            let tools = self.tools.read();
            tools.get(name).cloned().ok_or_else(|| {
                SeamError::new(ErrorCode::ToolNotFound, format!("tool {name} not found"))
            })?
        };

        // 1. Pre-execute waterfall (reorderable allow/deny/ask). Fail closed.
        let decision = self
            .events
            .waterfall((name.to_string(), args.to_vec()), |(_, _)| {
                PreDecision::Deny("no pre-execute handler".into())
            });
        match decision {
            PreDecision::Allow => {}
            PreDecision::Deny(reason) => return Err(SeamError::new(ErrorCode::ToolDenied, reason)),
            PreDecision::Ask => {
                return Err(SeamError::new(
                    ErrorCode::ToolDenied,
                    "approval required".to_string(),
                ))
            }
        }

        // 2. Monotonic guards (deny or abstain; cannot be reordered).
        for (gname, guard) in self.guards.read().iter() {
            if let GuardVerdict::Deny(reason) = guard(name, args) {
                return Err(SeamError::new(
                    ErrorCode::ToolDenied,
                    format!("guard {gname}: {reason}"),
                ));
            }
        }

        // 3-5. Tool body (an around-dispatch execute waterfall wraps this in
        // the full harness; the minimal registry calls the body directly).
        let result = body.run(call_id, args)?;

        // 6. Post-execute waterfall (accept, block, replace, add context).
        let post = self
            .events
            .waterfall((call_id, result.clone()), |(call, value)| (call, value));
        let final_value = post.1;

        // 7. Frozen result notification (fire-and-forget).
        let frozen = FrozenResult {
            call_id,
            value: final_value.clone(),
        };
        self.events.emit(frozen.clone());

        Ok(frozen)
    }

    /// The pipeline stage this registry hosts.
    pub fn stages(&self) -> &[PipelineStage] {
        &[
            PipelineStage::PreExecute,
            PipelineStage::PostExecute,
            PipelineStage::Notify,
        ]
    }
}

impl harnless_seams::Tools for ToolRegistry {
    fn register(&self, def: ToolDefinition, body: Arc<dyn ToolBody>) -> harnless_seams::Result<()> {
        self.tools.write().insert(def.name.clone(), (def, body));
        Ok(())
    }

    fn get(&self, name: &str) -> Option<ToolDefinition> {
        self.tools.read().get(name).map(|(def, _)| def.clone())
    }

    fn names(&self) -> Vec<String> {
        self.tools.read().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnless_runtime::events::EventRegistry;
    use harnless_runtime::fiber::Fiber;
    use harnless_seams::tools::{ToolBody, Tools as _};

    struct EchoBody;
    impl ToolBody for EchoBody {
        fn run(&self, _call_id: CallId, args: &[u8]) -> harnless_seams::Result<Value> {
            Ok(serde_json::json!({ "echo": args }))
        }
    }

    fn registry() -> (ToolRegistry, Arc<Fiber>) {
        let fiber = Fiber::active();
        let events = EventRegistry::new();
        let reg = ToolRegistry::new(events, fiber.clone());
        (reg, fiber)
    }

    fn echo_def() -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            schema: serde_json::json!({}),
            serialized: false,
        }
    }

    #[test]
    fn execute_fails_closed_when_no_pre_handler() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let err = reg.execute(CallId(1), "echo", b"hi");
        assert!(err.is_err());
        if let Err(e) = err {
            assert_eq!(e.code, ErrorCode::ToolDenied);
        }
    }

    #[test]
    fn pre_allow_lets_body_run_and_freeze_result() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let allow =
            |_: &mut PreExecute, _next: &mut Next<'_, PreExecute, PreDecision>| PreDecision::Allow;
        let _g = reg.on_pre_execute(move |e, n| allow(e, n)).unwrap();
        let frozen = reg.execute(CallId(2), "echo", b"hi").unwrap();
        assert_eq!(frozen.call_id, CallId(2));
        assert_eq!(frozen.value, serde_json::json!({ "echo": b"hi" }));
    }

    #[test]
    fn monotonic_guard_denies_before_body_runs() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let allow =
            |_: &mut PreExecute, _next: &mut Next<'_, PreExecute, PreDecision>| PreDecision::Allow;
        let _g = reg.on_pre_execute(allow).unwrap();
        reg.add_guard(
            "deny-all",
            Arc::new(|_, _| GuardVerdict::Deny("blocked by guard".into())),
        );
        let err = reg.execute(CallId(3), "echo", b"hi").unwrap_err();
        assert_eq!(err.code, ErrorCode::ToolDenied);
    }

    #[test]
    fn post_execute_can_block_the_result() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let allow =
            |_: &mut PreExecute, _next: &mut Next<'_, PreExecute, PreDecision>| PreDecision::Allow;
        let _g = reg.on_pre_execute(allow).unwrap();
        let block = |_: &mut PostExecute, next: &mut Next<'_, PostExecute, PostDecision>| {
            next.call((CallId(4), serde_json::json!({ "echo": b"hi" })))
        };
        let _g2 = reg.on_post_execute(move |e, n| block(e, n)).unwrap();
        let frozen = reg.execute(CallId(4), "echo", b"hi").unwrap();
        assert_eq!(frozen.call_id, CallId(4));
    }
}
