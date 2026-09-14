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
//!
//! # The dispatch key is part of a stage's contract
//!
//! Each policy stage is a waterfall, and the runtime keys waterfall slots by
//! `(TypeId<E>, TypeId<R>)` — the payload type **and** the result type. A
//! stage's dispatch result type is therefore load-bearing: the type
//! [`ToolRegistry::execute`] dispatches under and the type
//! [`ToolRegistry::on_post_execute`] registers under must be one type, or the
//! two never meet, the chain composes empty, and the stage silently freezes the
//! body's value while every registered listener goes unrun. The post-execute
//! stage used to make exactly that mistake: it dispatched the bare
//! `(CallId, Value)` payload as its own result type while the registrar
//! registered under `PostDecision`. The pinned test
//! `post_execute_listener_runs_in_the_locked_order` in `tests/primary_seam.rs`
//! is the regression guard.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

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
/// The payload of the post-execute waterfall.
///
/// The *payload* only. The waterfall's result type is [`PostExecuteOutcome`],
/// the carrier that can express a decision's replacement, added context, and
/// block together — a bare [`PostDecision`] cannot, which is why the stage does
/// not dispatch under it.
pub type PostExecute = (CallId, Value);

/// What the post-execute waterfall actually returns.
///
/// This **is** the stage's dispatch result type, and the type
/// [`ToolRegistry::on_post_execute`] registers its [`PostDecision`] listeners
/// under. The runtime keys waterfall slots by `(TypeId<E>, TypeId<R>)`, so the
/// type [`ToolRegistry::execute`] dispatches under and the type the registrar
/// registers under must be one type, or the two never meet: the chain composes
/// empty, every registered listener is unreachable, and the stage silently
/// freezes the body's value. That mismatch was this stage's bug.
///
/// A bare [`PostDecision`] cannot be the result type, because a decision cannot
/// finalize. [`PostDecision::AddContext`] keeps the body's value frozen *and*
/// asks for extra model-visible context — a decision has nowhere to carry the
/// value it accompanies — and [`PostDecision::Block`] must suppress the
/// frozen-result notification entirely, which a value-only result cannot
/// express. So this carrier holds the value, the added context, and the block in
/// one dispatch.
///
/// Veto needs no field here. A listener that returns without calling `next`
/// *structurally* vetoes the remainder — later listeners and the built-in
/// accept never run — because the stage only advances the chain inside
/// [`BridgeNext::call`]. That is the same contract the pre-execute stage
/// already has, so this carrier deliberately has no "accepted" flag: a stage
/// that recorded delegation would be guessing at something the chain already
/// knows.
#[derive(Debug, Clone, PartialEq)]
pub struct PostExecuteOutcome {
    /// The value finalization freezes, unless `denied` is set: the body's
    /// value, or a listener's replacement.
    pub value: Value,
    /// Extra context requested along the way, outermost-listener first.
    ///
    /// Delivered *after* the frozen result and never merged into it, so the
    /// result stays exactly what the accepted stage produced.
    pub context: Vec<Value>,
    /// A [`PostDecision::Block`] reason: the call is denied, nothing is
    /// frozen, and nothing is notified.
    pub denied: Option<String>,
}

impl PostExecuteOutcome {
    /// The built-in accept: freeze `value`, request no context, deny nothing.
    fn accept(value: Value) -> Self {
        Self {
            value,
            context: Vec::new(),
            denied: None,
        }
    }

    /// Apply one listener's [`PostDecision`] to the value that listener was
    /// handed.
    ///
    /// `Accept` keeps the handed value, `Replace` installs the listener's,
    /// `AddContext` keeps the handed value and carries the context, `Block`
    /// records the reason. The handed value is what the listener was asked to
    /// decide on — which is also what its `next` would have been handed, so a
    /// listener that delegates and one that vetoes start from the same place.
    fn from_decision(decision: &PostDecision, handed: Value) -> Self {
        match decision {
            PostDecision::Accept => Self::accept(handed),
            PostDecision::Replace(value) => Self {
                value: value.clone(),
                context: Vec::new(),
                denied: None,
            },
            PostDecision::AddContext(context) => Self {
                value: handed,
                context: context.clone(),
                denied: None,
            },
            PostDecision::Block(reason) => Self {
                value: handed,
                context: Vec::new(),
                denied: Some(reason.clone()),
            },
        }
    }

    /// Fold one listener's [`PostDecision`] into what the remainder of the
    /// chain answered.
    ///
    /// This is the waterfall's own composition rule: the outermost listener's
    /// value wins, every listener's context is kept (outermost first), and a
    /// `Block` anywhere denies the call whatever the value says.
    fn amend(self, decision: &PostDecision) -> Self {
        match decision {
            PostDecision::Accept => self,
            PostDecision::Replace(value) => Self {
                value: value.clone(),
                ..self
            },
            PostDecision::AddContext(context) => {
                let mut merged = context.clone();
                merged.extend(self.context);
                Self {
                    context: merged,
                    ..self
                }
            }
            PostDecision::Block(reason) => Self {
                denied: Some(reason.clone()),
                ..self
            },
        }
    }

    /// This outcome expressed as the [`PostDecision`] a delegating listener
    /// sees.
    ///
    /// A decision cannot carry a value, so the value the remainder settled on
    /// travels through the *payload* the next listener is handed; the decision
    /// only reports what kind of amendment the remainder asked for. An
    /// outcome whose value differs from the value its registration was handed
    /// is a replacement — that is the only decision shape that says "this
    /// value, not the one you were given" — and the distinction matters:
    /// `Accept` is the identity in [`PostExecuteOutcome::amend`], so mapping a
    /// replaced value to `Accept` would erase the replacement on the way out.
    fn as_decision(&self, handed: Option<&Value>) -> PostDecision {
        match &self.denied {
            Some(reason) => PostDecision::Block(reason.clone()),
            None if !self.context.is_empty() => PostDecision::AddContext(self.context.clone()),
            // The value differs from the one this registration was handed:
            // that is a replacement, and it must survive the fold.
            None if handed.is_some_and(|h| *h != self.value) => {
                PostDecision::Replace(self.value.clone())
            }
            None => PostDecision::Accept,
        }
    }
}

/// One registration's wrapper, as [`ToolRegistry::execute`] assembled it for
/// one dispatch: the listener body plus the cell its `next` answers from.
type StageEntry = (
    Arc<Mutex<StageListener>>,
    Arc<Mutex<Option<PostExecuteOutcome>>>,
);

/// The listener half of a registration, boxed so the chain can hold
/// registrations of different concrete `F` types side by side.
type StageListener = Box<dyn FnMut(&mut PostExecute, &mut BridgeNext) -> PostDecision + Send>;

/// The remainder of the post-execute chain, as installed for one dispatch by
/// [`ToolRegistry::execute`].
///
/// The runtime's own continuation cannot serve here: it is borrow-scoped and
/// typed to [`PostDecision`], the wrong result for the fold each wrapper
/// performs. So the stage carries its own. A wrapper *takes* the chain, keeps
/// the head for itself, and hands the *tail* to its listener's `next` — so the
/// chain strictly shrinks, a listener can never re-enter itself, and a listener
/// that never calls `next` leaves the remainder stranded, which is exactly what
/// veto means.
type StageChain = Arc<Mutex<VecDeque<StageEntry>>>;

/// The `next` a [`ToolRegistry::on_post_execute`] listener is handed.
///
/// It is a distinct type rather than the runtime's own
/// `Next<'_, PostExecute, PostDecision>` because the stage's result type is
/// [`PostExecuteOutcome`], not [`PostDecision`]. Calling it dispatches the
/// remainder of the stage and returns the later listeners and the built-in
/// accept composed into one [`PostDecision`]; not calling it vetoes them — the
/// runtime's own contract, restated for this stage's key.
pub struct BridgeNext {
    chain: StageChain,
    /// The cell this registration's wrapper reads back after the listener
    /// returns. `call` writes the remainder's outcome here.
    answer: Arc<Mutex<Option<PostExecuteOutcome>>>,
}

impl BridgeNext {
    /// Delegate to the later listeners and the built-in accept.
    pub fn call(&mut self, event: PostExecute) -> PostDecision {
        let handed = event.1.clone();
        let outcome = run_chain(&self.chain, event);
        // Hand the wrapper the remainder's *outcome*, not just its decision: a
        // decision cannot carry the value the remainder settled on, and the
        // wrapper's fold needs that value to amend it.
        let decision = outcome.as_decision(Some(&handed));
        *self.answer.lock().unwrap() = Some(outcome);
        decision
    }
}

/// The built-in at the end of the chain is the accept path: nobody amended, so
/// the payload's value is the frozen value and no context was requested. Each
/// registration runs against the value its predecessor handed it, exactly as
/// the runtime's waterfall propagates payloads.
fn run_chain(chain: &StageChain, payload: PostExecute) -> PostExecuteOutcome {
    let head = chain.lock().unwrap().pop_front();
    let Some((listener, answer)) = head else {
        return PostExecuteOutcome::accept(payload.1);
    };
    let handed = payload.1.clone();
    let decision = {
        let mut next = BridgeNext {
            chain: chain.clone(),
            answer: answer.clone(),
        };
        let mut guard = listener.lock().unwrap();
        // The payload travels inward as the *value* the next listener is asked
        // to decide on; the call id is stable for the whole dispatch.
        guard(&mut (payload.0, handed.clone()), &mut next)
    };
    let outcome = match answer.lock().unwrap().take() {
        // Delegated: this listener's decision amends the remainder's answer —
        // the outermost value wins, every context is kept, a block anywhere
        // denies the call.
        Some(inner) => inner.amend(&decision),
        // Vetoed: nothing downstream ran, so this listener's decision against
        // the value it was handed is the whole answer.
        None => PostExecuteOutcome::from_decision(&decision, handed),
    };
    // The outer wrapper reads this cell to compose its own answer. Restoring
    // the outcome (rather than leaving it consumed) is what lets the whole
    // chain unwind without a listener's delegation being mistaken for a veto.
    *answer.lock().unwrap() = Some(outcome.clone());
    outcome
}

/// Where the pipeline delivers post-execute context.
///
/// Model-visible means **logged**: the session log — not a return value — is
/// the source of truth, so the harness and any consumer that owns a log mount
/// it here (see [`ToolRegistry::set_post_execute_context_sink`]). A registry
/// mounted bare (no log, as the wasm host tests do) still runs the whole
/// stage; its context stays observable through
/// [`ToolRegistry::collected_context`].
pub type ContextSink = Arc<dyn Fn(CallId, Value) + Send + Sync>;

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
    /// The post-execute registrations, in registration order. [`Self::execute`]
    /// assembles them into a [`StageChain`] for one dispatch (see
    /// [`Self::on_post_execute`]).
    post: Arc<parking_lot::RwLock<Vec<StageEntry>>>,
    /// Where post-execute context is delivered (see
    /// [`ToolRegistry::set_post_execute_context_sink`]).
    context: parking_lot::RwLock<Option<ContextSink>>,
    /// Post-execute context collected while no sink was mounted.
    pending: parking_lot::RwLock<Vec<(CallId, Value)>>,
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
            post: Arc::new(parking_lot::RwLock::new(Vec::new())),
            context: parking_lot::RwLock::new(None),
            pending: parking_lot::RwLock::new(Vec::new()),
        }
    }

    /// Remove a tool by name, returning whether it was present.
    ///
    /// The reversible half of [`harnless_seams::Tools::register`]: a plugin
    /// fiber's unload
    /// path uses this so unmounting unwinds exactly its registrations.
    pub fn remove(&self, name: &str) -> bool {
        self.tools.write().remove(name).is_some()
    }

    /// Whether `name` is registered with exactly `body` as its body.
    ///
    /// Identity, not definition: two plugins (or one plugin's two generations)
    /// can declare the same name with different bodies, and the reversible
    /// unregister a mount performs must only undo *its own* registration. A
    /// reload registers the new generation's body under the old name before
    /// retiring the old generation, so the old mount's unwind must leave the
    /// live entry standing.
    pub fn body_is(&self, name: &str, body: &Arc<dyn ToolBody>) -> bool {
        self.tools
            .read()
            .get(name)
            .map(|(_, stored)| Arc::ptr_eq(stored, body))
            .unwrap_or(false)
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
    ///
    /// The listener keeps the shape of every other policy stage: it is handed
    /// the `(CallId, Value)` payload plus a `next` continuation, and returns a
    /// [`PostDecision`]. Internally the stage's waterfall result type is
    /// [`PostExecuteOutcome`] — the value, context, and block a decision can
    /// express — so [`Self::execute`] dispatches under *that* key and this
    /// registrar feeds the chain that key's listener runs.
    ///
    /// Composition is the waterfall's own. Listeners run in registration order,
    /// outermost first; each sees what its `next` answered and may amend it —
    /// keep the inner value and add its own context, or replace what the inner
    /// listener produced; the outermost value wins and every context is kept. A
    /// listener that returns without calling `next` vetoes the remainder (later
    /// listeners *and* the built-in accept) structurally, exactly as the
    /// pre-execute stage's veto contract works.
    pub fn on_post_execute<F>(&self, listener: F) -> Result<Disposer>
    where
        F: FnMut(&mut PostExecute, &mut BridgeNext) -> PostDecision + Send + 'static,
    {
        // The registration lives in the stage's own list, which is where
        // `execute` assembles the chain from — registration and dispatch meet
        // in this list, which is the whole point of this stage's design after
        // the dispatch-key fix.
        let entry: StageEntry = (
            Arc::new(Mutex::new(Box::new(listener) as StageListener)),
            Arc::new(Mutex::new(None)),
        );
        let handle = Arc::clone(&entry.0);
        self.post.write().push(entry);
        let post = self.post.clone();
        self.fiber.effect(move || {
            Some(Box::new(move || {
                let mut list = post.write();
                if let Some(at) = list.iter().position(|(l, _)| Arc::ptr_eq(l, &handle)) {
                    list.remove(at);
                }
            }) as harnless_runtime::fiber::DisposeFn)
        })
    }

    /// Install the sink that receives post-execute context, in delivery
    /// order, after the call's frozen result.
    ///
    /// Context handed to a sink while none was installed is **not** lost: it
    /// is held (see [`Self::collected_context`]) and flushed to this sink in
    /// order the moment it is installed, so a spine that wires the log after
    /// a first tool exchange still sees every record.
    pub fn set_post_execute_context_sink(&self, sink: ContextSink) {
        // Install and drain atomically with respect to
        // [`Self::deliver_post_execute_context`]. Both take `pending` and
        // `context` together, so a delivery either lands entirely before this
        // critical section (its value is in `held` and is flushed here) or
        // entirely after it (it sees the mounted sink and delivers directly).
        // No interleaving can leave a value in `pending` while a sink is
        // mounted, which is the silent loss this method's contract forbids.
        //
        // `pending` is taken first and `context` second in BOTH functions, so
        // the order is consistent and cannot deadlock. The sink is cloned out
        // and the locks dropped before the flush loop, so no sink callback
        // ever runs while the registry's own locks are held.
        let held = {
            let mut pending = self.pending.write();
            let mut context = self.context.write();
            *context = Some(sink.clone());
            std::mem::take(&mut *pending)
        };
        for (call_id, value) in held {
            sink(call_id, value);
        }
    }

    /// Post-execute context the pipeline delivered with no sink installed,
    /// in delivery order.
    pub fn collected_context(&self) -> Vec<(CallId, Value)> {
        self.pending.read().clone()
    }

    /// Run the guarded pipeline for `call` on `name` with raw-JSON `args`.
    ///
    /// Permission is denied when nobody answers the pre-execute waterfall
    /// (failing closed). Returns the frozen result.
    ///
    /// The post-execute stage dispatches the `(CallId, Value)` payload under
    /// result type [`PostExecuteOutcome`] — the type [`Self::on_post_execute`]
    /// registers its [`PostDecision`] listeners under — and interprets every
    /// outcome a decision can carry:
    ///
    /// * [`PostDecision::Accept`] freezes the body's value unchanged;
    /// * [`PostDecision::Replace`] freezes the listener's value instead;
    /// * [`PostDecision::Block`] turns the whole call into a
    ///   [`ErrorCode::ToolDenied`] error carrying the reason, and **no**
    ///   frozen-result notification is emitted — a blocked call froze no
    ///   result, so there is nothing to notify and nothing is logged as a
    ///   success;
    /// * [`PostDecision::AddContext`] keeps the frozen result intact and
    ///   delivers the extra context alongside it, never merged into it.
    ///
    /// `AddContext` is the one outcome a bare [`PostDecision`] cannot express
    /// alongside the value it accompanies. The minimal shape that leaves the
    /// result uncorrupted is to carry the context **beside** the result: the
    /// outcome's `context` is handed to the sink installed by
    /// [`Self::set_post_execute_context_sink`] — a consumer that owns a
    /// session log mounts it there, so the context becomes an ordinary,
    /// ordered, model-visible log record — and with no sink mounted it stays
    /// readable through [`Self::collected_context`]. Nothing is merged into
    /// the frozen value: the result the loop logs, the value every notify
    /// listener sees, and the derived history node all stay byte-for-byte what
    /// the accepted stage produced. This is deliberately additive — the seam's
    /// [`FrozenResult`] gained no field, and a consumer that ignores the
    /// context sees exactly the result it saw before.
    ///
    /// A listener that returns without calling `next` vetoes the remainder of
    /// the post waterfall — later listeners *and* the built-in accept never
    /// run — exactly as the pre-execute stage's veto contract works.
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
        //
        // The chain is assembled fresh per dispatch from the registrations in
        // order, then walked: the outermost registration is the head, each
        // registration's `next` runs the strictly-smaller tail, and the built-in
        // accept terminates the chain. Dispatching the payload under any other
        // result type than the one the registrar registers under — which is
        // what this stage used to do — composes an empty chain and silently
        // freezes the body's value.
        let chain: StageChain = Arc::new(Mutex::new(self.post.read().iter().cloned().collect()));
        let post = run_chain(&chain, (call_id, result));
        let PostExecuteOutcome {
            value,
            context,
            denied,
        } = post;

        // 7. Frozen result notification (fire-and-forget). A blocked call
        //    froze nothing, so it notifies nothing, delivers no context, and
        //    nothing is logged as a success: deny before any of it.
        if let Some(reason) = denied {
            return Err(SeamError::new(ErrorCode::ToolDenied, reason));
        }

        // 8. Extra context, delivered before anything downstream of the call
        //    can log its result. The sink is the session log (when one is
        //    mounted), and the log's order *is* the model-visible order — so
        //    context appended here lands ahead of the tool-result record the
        //    loop appends once `execute` returns, and the frozen value itself
        //    is never touched: the result stays byte-for-byte what the
        //    accepted stage produced.
        for extra in context {
            self.deliver_post_execute_context(call_id, extra);
        }
        let frozen = FrozenResult { call_id, value };
        self.events.emit(frozen.clone());
        Ok(frozen)
    }

    /// Hand one piece of post-execute context to the mounted sink.
    ///
    /// With no sink mounted the context is retained rather than dropped: it
    /// was decided, and a pipeline that silently loses a plugin's context is
    /// the same class of bug this stage fixes.
    fn deliver_post_execute_context(&self, call_id: CallId, value: Value) {
        // Both locks, same order as `set_post_execute_context_sink`, and the
        // sink is cloned out so its callback never runs under them. Reading
        // `context` without holding `pending` would let a sink install slip in
        // between the read and the push, stranding this value.
        let mut pending = self.pending.write();
        let sink = self.context.read().clone();
        match sink {
            Some(sink) => {
                drop(pending);
                sink(call_id, value);
            }
            None => pending.push((call_id, value)),
        }
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
    use std::sync::atomic::{AtomicBool, Ordering};

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
            description: "Echo the arguments back.".into(),
            schema: serde_json::json!({}),
            serialized: false,
        }
    }

    fn allow(reg: &ToolRegistry) -> Disposer {
        let allow =
            |_: &mut PreExecute, _next: &mut Next<'_, PreExecute, PreDecision>| PreDecision::Allow;
        reg.on_pre_execute(move |e, n| allow(e, n)).unwrap()
    }

    #[test]
    fn execute_fails_closed_when_no_pre_handler() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let err = reg.execute(CallId(1), "echo", b"hi").unwrap_err();
        assert_eq!(err.code, ErrorCode::ToolDenied);
    }

    #[test]
    fn pre_allow_lets_body_run_and_freeze_result() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        let frozen = reg.execute(CallId(2), "echo", b"hi").unwrap();
        assert_eq!(frozen.call_id, CallId(2));
        assert_eq!(frozen.value, serde_json::json!({ "echo": b"hi" }));
    }

    #[test]
    fn monotonic_guard_denies_before_body_runs() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        reg.add_guard(
            "deny-all",
            Arc::new(|_, _| GuardVerdict::Deny("blocked by guard".into())),
        );
        let err = reg.execute(CallId(3), "echo", b"hi").unwrap_err();
        assert_eq!(err.code, ErrorCode::ToolDenied);
    }

    #[test]
    fn post_execute_accept_keeps_the_body_value() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        let _g2 = reg
            .on_post_execute(|e: &mut PostExecute, next: &mut BridgeNext| {
                next.call((e.0, e.1.clone()))
            })
            .unwrap();
        let frozen = reg.execute(CallId(4), "echo", b"hi").unwrap();
        assert_eq!(frozen.value, serde_json::json!({ "echo": b"hi" }));
    }

    #[test]
    fn post_execute_replace_changes_the_frozen_value() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        let _g2 = reg
            .on_post_execute(|_: &mut PostExecute, _next: &mut BridgeNext| {
                PostDecision::Replace(serde_json::json!({ "replaced": true }))
            })
            .unwrap();
        let frozen = reg.execute(CallId(5), "echo", b"hi").unwrap();
        assert_eq!(frozen.value, serde_json::json!({ "replaced": true }));
    }

    #[test]
    fn post_execute_block_denies_with_the_reason() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        let _g2 = reg
            .on_post_execute(|_: &mut PostExecute, _next: &mut BridgeNext| {
                PostDecision::Block("policy says no".into())
            })
            .unwrap();
        let err = reg.execute(CallId(6), "echo", b"hi").unwrap_err();
        assert_eq!(err.code, ErrorCode::ToolDenied);
        assert_eq!(err.message, "policy says no");
    }

    #[test]
    fn post_execute_block_notifies_nothing() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        let _g2 = reg
            .on_post_execute(|_: &mut PostExecute, _next: &mut BridgeNext| {
                PostDecision::Block("nope".into())
            })
            .unwrap();
        let notified = Arc::new(Mutex::new(Vec::new()));
        let seen = notified.clone();
        let _n = reg
            .events
            .on(
                &reg.fiber,
                move |f: &mut FrozenResult| seen.lock().unwrap().push(f.value.clone()),
                EventOptions::new(),
            )
            .unwrap();
        assert!(reg.execute(CallId(7), "echo", b"hi").is_err());
        assert!(
            notified.lock().unwrap().is_empty(),
            "a blocked call froze nothing"
        );
    }

    #[test]
    fn post_execute_add_context_keeps_the_result_and_collects_context() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        let _g2 = reg
            .on_post_execute(|_: &mut PostExecute, _next: &mut BridgeNext| {
                PostDecision::AddContext(vec![serde_json::json!({"note": "extra"})])
            })
            .unwrap();
        let frozen = reg.execute(CallId(8), "echo", b"hi").unwrap();
        assert_eq!(frozen.value, serde_json::json!({ "echo": b"hi" }));
        assert_eq!(
            reg.collected_context(),
            vec![(CallId(8), serde_json::json!({ "note": "extra" }))]
        );
    }

    #[test]
    fn post_execute_veto_hides_the_remainder_and_the_built_in() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        // Outer (registered first) vetoes — it never calls `next`; the inner
        // listener and the built-in accept must not run.
        let inner_ran = Arc::new(AtomicBool::new(false));
        let _outer = reg
            .on_post_execute(|_: &mut PostExecute, _next: &mut BridgeNext| {
                PostDecision::Replace(serde_json::json!({"vetoed": true}))
            })
            .unwrap();
        let seen = inner_ran.clone();
        let _inner = reg
            .on_post_execute(move |_: &mut PostExecute, _next: &mut BridgeNext| {
                seen.store(true, Ordering::SeqCst);
                PostDecision::Accept
            })
            .unwrap();
        let frozen = reg.execute(CallId(9), "echo", b"hi").unwrap();
        assert_eq!(frozen.value, serde_json::json!({"vetoed": true}));
        assert!(
            !inner_ran.load(Ordering::SeqCst),
            "a veto must not run later listeners"
        );
    }

    #[test]
    fn post_execute_listeners_compose_in_registration_order() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        // Outer delegates and amends the inner decision; inner replaces.
        let _outer = reg
            .on_post_execute(|e: &mut PostExecute, next: &mut BridgeNext| {
                match next.call((e.0, e.1.clone())) {
                    PostDecision::Replace(v) => {
                        let mut obj = v.as_object().cloned().unwrap_or_default();
                        obj.insert("outer".into(), serde_json::json!(true));
                        PostDecision::Replace(serde_json::Value::Object(obj))
                    }
                    other => other,
                }
            })
            .unwrap();
        let _inner = reg
            .on_post_execute(|_: &mut PostExecute, _next: &mut BridgeNext| {
                PostDecision::Replace(serde_json::json!({"inner": true}))
            })
            .unwrap();
        let frozen = reg.execute(CallId(10), "echo", b"hi").unwrap();
        assert_eq!(
            frozen.value,
            serde_json::json!({"inner": true, "outer": true})
        );
    }

    #[test]
    fn disposing_a_post_listener_unregisters_it() {
        let (reg, _f) = registry();
        reg.register(echo_def(), Arc::new(EchoBody)).unwrap();
        let _g = allow(&reg);
        let disposer = reg
            .on_post_execute(|_: &mut PostExecute, _next: &mut BridgeNext| {
                PostDecision::Replace(serde_json::json!({"gone": true}))
            })
            .unwrap();
        disposer.dispose();
        let frozen = reg.execute(CallId(11), "echo", b"hi").unwrap();
        assert_eq!(frozen.value, serde_json::json!({ "echo": b"hi" }));
    }
}
