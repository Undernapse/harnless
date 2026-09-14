//! Reusable test-support for the **primary seam**: the agent loop observed
//! through the session log, driven by the replay adapter over a recorded
//! model stream (no network, no provider credentials).
//!
//! Compiled into the normal crate build (no `cfg(test)`) so *other* crates'
//! integration tests can reuse it: build a [`Harness`], register tools and
//! pipeline listeners on it, drive turns, and assert the emitted event
//! sequence, the derived message list, the assembled prompt, and the
//! persisted log — all through the log, the single highest seam.
//!
//! The driving machinery mirrors the production driver shape (the CLI's
//! `make_driver`): the [`ReplayAdapter`] stream is polled to completion with
//! a noop waker (replay never blocks) and folded with the shared
//! [`BlockAssembler`]. A `tool_call` block in the replayed stream makes the
//! driver return [`DriverOutcome::ToolCall`]; the loop logs the call and
//! answers it with its pipeline result. The guarded [`ToolRegistry`]
//! pipeline is driven explicitly via [`Harness::execute_tool`] so tests
//! observe each stage — pre-execute → monotonic guards → execute →
//! post-execute → finalize → notify. Approval fails closed: with no
//! pre-execute listener registered, the pipeline denies.
//!
//! Corpus: golden [`Recording`] JSON documents (see
//! `crates/harnless-agent/tests/corpus/`), one document per scripted model
//! call; a [`Harness`] replays them in order.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::stream::Stream as _;
use harnless_llm_replay::{Recording, ReplayAdapter, Script};
use harnless_runtime::events::{EventRegistry, Next};
use harnless_runtime::fiber::Fiber;
use harnless_runtime::{Disposer, Result as RtResult};
use harnless_seams::tools::{FrozenResult, PreDecision, ToolBody, ToolDefinition};
use harnless_seams::{
    BlockAssembler, BlockKind, CallId, ErrorCode, Message, MessageId, ModelAdapter,
    ProviderFailure, ReplayState, SeamError, SessionId, StreamEvent, StreamFrame,
};

use crate::events::{
    ContentBlock, MessageRecord, SessionEvent, ToolCallRecord, ToolResultRecord, TurnEndReason,
};
use crate::history::{History, SurfaceNode};
use crate::loop_::{AgentLoop, DerivedTurn, Driver, DriverOutcome};
use crate::persistence::{LoadedLog, SessionPersistence};
use crate::prompt::{assemble, SystemPrompt};
use crate::session::SessionLog;
use crate::tools::{PreExecute, ToolRegistry};

/// One scripted model call: a recording plus the assistant message id the
/// driver stamps the committed message.
pub struct ScriptedCall {
    /// The golden recording replayed for this call.
    pub recording: Recording,
    /// The message id for the message this call commits.
    pub message_id: MessageId,
}

impl ScriptedCall {
    /// Pair a recording with the message id it commits.
    pub fn new(recording: Recording, message_id: MessageId) -> Self {
        Self {
            recording,
            message_id,
        }
    }
}

/// A recorded tool: a [`ToolBody`] counting invocations, so a test can pin
/// that the guarded pipeline ran — or never ran — the body.
pub struct CountingTool {
    calls: AtomicUsize,
    /// The raw-JSON result the body returns.
    pub result: String,
}

impl CountingTool {
    /// A tool returning raw-JSON `result`.
    pub fn new(result: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            result: result.into(),
        })
    }

    /// How many times the body has run.
    pub fn invocations(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ToolBody for CountingTool {
    fn run(&self, _call_id: CallId, _args: &[u8]) -> harnless_seams::Result<serde_json::Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        serde_json::from_str(&self.result).map_err(|e| {
            SeamError::new(
                ErrorCode::ToolPanicked,
                format!("tool result is not JSON: {e}"),
            )
        })
    }
}

/// A tool call the replayed stream requested, parsed from its recorded
/// assembled payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedToolCall {
    /// The seam call id the stream minted.
    pub call_id: CallId,
    /// The tool name.
    pub tool: String,
    /// Raw-JSON arguments.
    pub arguments: String,
}

/// Parse a recorded assembled tool-call payload
/// (`{"id":N,"name":"...","arguments":"..."}`) into a [`RequestedToolCall`].
pub fn parse_requested_tool_call(text: &str) -> Option<RequestedToolCall> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(RequestedToolCall {
        call_id: CallId(value.get("id")?.as_u64()?),
        tool: value.get("name")?.as_str()?.to_string(),
        arguments: value.get("arguments")?.as_str()?.to_string(),
    })
}

/// The primary-seam harness: spine services (log, tool registry, loop) plus
/// one replay adapter, wired the way the spine mounts them.
pub struct Harness {
    /// The session log — the observable behavior.
    pub log: Arc<SessionLog>,
    /// The guarded tool pipeline.
    pub tools: Arc<ToolRegistry>,
    /// The agent loop driving the log.
    pub loop_: Arc<AgentLoop>,
    /// The loop's event domain (waterfalls: `on_stream`).
    pub events: EventRegistry,
    /// The fiber owning every registration made through the harness.
    pub fiber: Arc<Fiber>,
    /// The scripted calls the replay adapter serves, in order.
    pub script: Vec<ScriptedCall>,
    /// Adapter identity committed with each assistant message.
    pub provider: String,
    /// Model name committed with each assistant message.
    pub model: String,
    /// Adapter call counter, shared by every driver built from this harness
    /// (the script position across turns).
    calls: Arc<AtomicUsize>,
    /// One replay adapter shared by every driver, so the recorded stream
    /// sequence continues across turns exactly as one adapter serves one
    /// conversation.
    adapter: Arc<ReplayAdapter>,
}

impl Harness {
    /// A harness with no scripted calls; prefer [`Harness::with_script`].
    pub fn new(session: SessionId) -> Self {
        Self::build(session, Vec::new())
    }

    /// A harness scripted with `script`, one recording per model call.
    pub fn with_script(session: SessionId, script: Vec<ScriptedCall>) -> Self {
        Self::build(session, script)
    }

    fn build(session: SessionId, script: Vec<ScriptedCall>) -> Self {
        let fiber = Fiber::active();
        let log = Arc::new(SessionLog::new(session));
        let events = EventRegistry::new();
        let tools = Arc::new(ToolRegistry::new(events.clone(), fiber.clone()));
        let loop_ = Arc::new(AgentLoop::with_tools(
            log.clone(),
            events.clone(),
            fiber.clone(),
            tools.clone(),
        ));
        // The log is the observable behavior, so it is also where the
        // pipeline delivers post-execute context: an `AddContext` decision
        // becomes an ordinary log record, model-visible and ordered, exactly
        // the way a consumer that owns a log is meant to mount the sink.
        let sink_log = log.clone();
        tools.set_post_execute_context_sink(std::sync::Arc::new(move |call_id, value| {
            sink_log.append(SessionEvent::ToolResult(ToolResultRecord {
                call_id,
                content: format!("[context] {value}"),
            }));
        }));
        // The adapter needs at least one recording; an unscripted harness
        // gets a trivial placeholder so construction never panics, and the
        // driver's id list keeps the same length so indexing can never
        // underflow (the adapter's repeat-last semantics then apply).
        let mut recordings: Vec<Recording> = script.iter().map(|c| c.recording.clone()).collect();
        if recordings.is_empty() {
            recordings.push(text_recording(&["no script"]));
        }
        let adapter = Arc::new(ReplayAdapter::new("replay", Script::new(recordings)));
        Self {
            log,
            tools,
            loop_,
            events,
            fiber,
            script: if script.is_empty() {
                // Mirror the placeholder recording with the id it commits.
                vec![ScriptedCall::new(
                    text_recording(&["no script"]),
                    MessageId(0),
                )]
            } else {
                script
            },
            provider: "replay".into(),
            model: "golden".into(),
            calls: Arc::new(AtomicUsize::new(0)),
            adapter,
        }
    }

    /// Set the model name committed with assistant messages (default `golden`).
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Register a tool (definition + body) on the guarded registry.
    ///
    /// `description` is the model-facing text the definition carries: it is
    /// what [`harnless_seams::ToolDefinition::to_schema`] would hand an
    /// adapter, so a test that cares about what a provider would see has to
    /// state it explicitly — there is no defaulted description that can
    /// silently vanish from a registration.
    pub fn register_tool(
        &self,
        name: &str,
        description: &str,
        schema: serde_json::Value,
        body: Arc<dyn ToolBody>,
    ) -> harnless_seams::Result<()> {
        use harnless_seams::Tools as _;
        self.tools.register(
            ToolDefinition {
                name: name.to_string(),
                description: description.to_string(),
                schema,
                serialized: false,
            },
            body,
        )
    }

    /// Register an allow-all pre-execute listener (approval granted).
    pub fn allow_all(&self) -> RtResult<Disposer> {
        self.tools.on_pre_execute(
            |_: &mut PreExecute, _next: &mut Next<'_, PreExecute, PreDecision>| PreDecision::Allow,
        )
    }

    /// Append a user message to the log (model-visible, so it is logged
    /// before the turn, exactly as the CLI runner does).
    pub fn user(&self, id: MessageId, text: &str) {
        self.log.append(SessionEvent::UserMessage(MessageRecord {
            id,
            blocks: vec![ContentBlock::Text { text: text.into() }],
            provider: None,
            model: None,
        }));
    }
    /// Register an accept-all post-execute listener: observe the body's
    /// value and delegate it onward (the post waterfall's accept path).
    ///
    /// Post-execute listeners are dispatched under the registry's outcome
    /// result type, so a listener registered here really does run — see
    /// `post_execute_listener_runs_in_the_locked_order` in
    /// `tests/primary_seam.rs`.
    pub fn accept_post(&self) -> RtResult<Disposer> {
        self.tools.on_post_execute(
            |e: &mut crate::tools::PostExecute, next: &mut crate::tools::BridgeNext| {
                let (call_id, value) = e.clone();
                next.call((call_id, value))
            },
        )
    }

    /// Append a tool result to the log (the message-producing half of a
    /// tool exchange).
    pub fn log_tool_result(&self, call_id: CallId, content: impl Into<String>) {
        self.log.append(SessionEvent::ToolResult(ToolResultRecord {
            call_id,
            content: content.into(),
        }));
    }

    /// Build the loop driver: one adapter call whose replayed stream folds
    /// into the committed assistant message, or a tool-call outcome.
    ///
    /// The shared call counter names the script position, so drivers built
    /// across turns replay their recordings in order.
    pub fn driver(&self) -> Driver {
        let adapter = self.adapter.clone();
        let script: Vec<MessageId> = self.script.iter().map(|c| c.message_id).collect();
        let provider = self.provider.clone();
        let model = self.model.clone();
        let counter = self.calls.clone();
        Box::new(move || {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            let message_id = script[call.min(script.len() - 1)];
            let mut stream = match adapter.stream(CallId(1), &[] as &[Message], &[], None) {
                Ok(s) => s,
                Err(err) => return stop_error(err.code, &err.message),
            };
            // Replay never blocks: poll to completion with a noop waker.
            let mut assembler = BlockAssembler::new();
            let mut failure: Option<ProviderFailure> = None;
            let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
            loop {
                match std::pin::Pin::new(&mut stream).poll_next(&mut cx) {
                    std::task::Poll::Ready(Some(StreamEvent::Frame(frame))) => {
                        assembler.push(&frame)
                    }
                    std::task::Poll::Ready(Some(StreamEvent::Failed(f))) => failure = Some(f),
                    std::task::Poll::Ready(None) => break,
                    std::task::Poll::Pending => {
                        failure = Some(ProviderFailure {
                            code: ErrorCode::StreamTerminated,
                            message: "replay stream blocked".into(),
                        });
                        break;
                    }
                }
            }
            if let Some(f) = failure {
                return stop_error(f.code, &f.message);
            }
            let blocks = assembler.blocks();
            // A tool-call block in the replayed stream: the model asked for
            // a tool, so the step is a tool call, not a final message. The
            // recorded payload travels in `arguments`; the loop logs the
            // call and answers it with the pipeline result.
            if let Some(tool_block) = blocks.iter().find(|b| b.kind == BlockKind::ToolCall) {
                let requested = parse_requested_tool_call(&tool_block.text)
                    .expect("recorded tool-call payload is raw JSON");
                return DriverOutcome::ToolCall(ToolCallRecord {
                    call_id: requested.call_id,
                    tool: requested.tool,
                    arguments: tool_block.text.clone(),
                });
            }
            DriverOutcome::Message(MessageRecord {
                id: message_id,
                blocks: blocks
                    .into_iter()
                    .map(|b| match b.kind {
                        BlockKind::Reasoning => ContentBlock::Reasoning { text: b.text },
                        _ => ContentBlock::Text { text: b.text },
                    })
                    .collect(),
                provider: Some(provider),
                model: Some(model),
            })
        })
    }

    /// Drive one turn with the replay driver.
    pub fn run_turn(&self) -> DerivedTurn {
        self.loop_.run_turn(self.driver())
    }
    /// Run the guarded pipeline for a requested call directly and log the
    /// resulting [`SessionEvent::ToolResult`] — for tests that drive the
    /// pipeline stage-by-stage instead of through a turn.
    pub fn execute_tool(
        &self,
        requested: &RequestedToolCall,
    ) -> harnless_seams::Result<FrozenResult> {
        let frozen = self.tools.execute(
            requested.call_id,
            &requested.tool,
            requested.arguments.as_bytes(),
        )?;
        self.log_tool_result(requested.call_id, frozen.value.to_string());
        Ok(frozen)
    }

    /// Drive one **full tool turn**: the replayed stream requests a tool,
    /// the loop logs the call and answers it through the guarded pipeline
    /// (the frozen result lands in the log), then a follow-up turn commits
    /// the model's final answer. The event sequence is the assertion target.
    ///
    /// [`Harness::allow_all`] (or an equivalent pre-execute listener) must
    /// be registered or the pipeline denies — fail-closed is the contract,
    /// and the denial is visible in the log as an error tool result.
    ///
    /// # Panics
    /// Panics when the first scripted call did not request a tool — misuse
    /// of the fixture, not a stream outcome.
    pub fn run_tool_turn(&self) -> (DerivedTurn, DerivedTurn) {
        let first = self.run_turn();
        last_tool_call(&self.log)
            .expect("first turn must log a tool call; script it with a tool-call recording");
        let second = self.run_turn();
        (first, second)
    }

    /// The stable event-kind sequence of the whole log.
    pub fn event_kinds(&self) -> Vec<String> {
        self.log
            .snapshot()
            .records
            .iter()
            .map(|r| event_kind(&r.event))
            .collect()
    }

    /// The derived message list projected from the full log.
    pub fn derived(&self) -> Vec<SurfaceNode> {
        let mut history = History::default();
        for record in self.log.snapshot().records {
            history.apply(&record.event);
        }
        history.nodes().to_vec()
    }

    /// Assemble the system prompt over the log.
    pub fn prompt(&self, base: &str) -> SystemPrompt {
        assemble(base, &self.log)
    }

    /// Checkpoint the whole log through `backend` and load it back.
    ///
    /// The batch is snapshotted eagerly (the persistence seam's save future
    /// borrows it), then the log is saved and loaded from `backend`.
    pub async fn checkpoint_load<B: SessionPersistence>(&self, backend: &mut B) -> LoadedLog {
        let events: Vec<SessionEvent> = self
            .log
            .snapshot()
            .records
            .into_iter()
            .map(|r| r.event)
            .collect();
        let session = self.log.session();
        backend.save(&session, &events).await.unwrap();
        backend.load(&session).await.unwrap().expect("saved")
    }
}

/// The most recent tool call logged, as a requested call.
pub fn last_tool_call(log: &SessionLog) -> Option<RequestedToolCall> {
    log.snapshot().records.iter().rev().find_map(|r| {
        if let SessionEvent::ToolCall(c) = &r.event {
            Some(RequestedToolCall {
                call_id: c.call_id,
                tool: c.tool.clone(),
                arguments: c.arguments.clone(),
            })
        } else {
            None
        }
    })
}

/// A driver stop carrying a normalized failure as the turn error.
fn stop_error(code: ErrorCode, message: &str) -> DriverOutcome {
    DriverOutcome::Stop(TurnEndReason::Error {
        code: code.to_string(),
        message: message.to_string(),
    })
}

/// The stable kind spelling of one session event.
pub fn event_kind(event: &SessionEvent) -> String {
    match event {
        SessionEvent::TurnOpen => "turn_open",
        SessionEvent::TurnClose { .. } => "turn_close",
        SessionEvent::StepOpen => "step_open",
        SessionEvent::StepClose => "step_close",
        SessionEvent::UserMessage(_) => "user_message",
        SessionEvent::AssistantChunk(_) => "assistant_chunk",
        SessionEvent::AssistantMessage(_) => "assistant_message",
        SessionEvent::ToolCall(_) => "tool_call",
        SessionEvent::ToolResult(_) => "tool_result",
        SessionEvent::SeedBoundary => "seed_boundary",
    }
    .to_string()
}

/// Render one content block to its display line (same rendering the prompt
/// assembly uses).
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

/// Build a golden [`Recording`] from seam frames plus replay state.
pub fn recording(frames: &[StreamFrame], replay: &ReplayState) -> Recording {
    Recording::capture(frames, replay)
}

/// The replay state a golden recording carries: provider-stamped response
/// metadata (the ownership marker the adapter claims back).
pub fn golden_replay_state(provider: &str) -> ReplayState {
    ReplayState {
        response: Some(serde_json::json!({ "id": "resp-golden", "__harnless_provider": provider })),
        blocks: vec![serde_json::json!({ "i": 0 })],
    }
}

/// A minimal text-answer recording: one text block streamed in `deltas`,
/// usage, finish.
pub fn text_recording(deltas: &[&str]) -> Recording {
    let mut frames = vec![StreamFrame::BlockStart {
        index: 0,
        kind: BlockKind::Text,
    }];
    for text in deltas {
        frames.push(StreamFrame::TextDelta {
            index: 0,
            text: (*text).into(),
        });
    }
    let assembled: String = deltas.concat();
    frames.push(StreamFrame::BlockEnd {
        index: 0,
        assembled: harnless_seams::ContentBlock {
            kind: BlockKind::Text,
            text: assembled,
        },
    });
    frames.push(StreamFrame::Usage(harnless_seams::Usage {
        uncached_input: deltas.len() as u64,
        cached_reads: 0,
        cached_writes: 0,
        output: 1,
        reasoning: 0,
    }));
    frames.push(StreamFrame::Finish);
    Recording::capture(&frames, &golden_replay_state("replay"))
}

/// A tool-request recording: the model streams a short text lead-in then one
/// tool-call block for `call_id`/`name`/`arguments`.
pub fn tool_recording(call_id: u64, name: &str, arguments: &str) -> Recording {
    let payload = serde_json::json!({
        "id": call_id,
        "name": name,
        "arguments": arguments,
    })
    .to_string();
    let frames = vec![
        StreamFrame::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        },
        StreamFrame::TextDelta {
            index: 0,
            text: "working".into(),
        },
        StreamFrame::BlockEnd {
            index: 0,
            assembled: harnless_seams::ContentBlock {
                kind: BlockKind::Text,
                text: "working".into(),
            },
        },
        StreamFrame::BlockStart {
            index: 1,
            kind: BlockKind::ToolCall,
        },
        StreamFrame::ToolCallDelta {
            index: 1,
            call_id: CallId(call_id),
            json: payload.clone(),
        },
        StreamFrame::BlockEnd {
            index: 1,
            assembled: harnless_seams::ContentBlock {
                kind: BlockKind::ToolCall,
                text: payload,
            },
        },
        StreamFrame::Usage(harnless_seams::Usage {
            uncached_input: 2,
            cached_reads: 0,
            cached_writes: 0,
            output: 1,
            reasoning: 0,
        }),
        StreamFrame::Finish,
    ];
    Recording::capture(&frames, &golden_replay_state("replay"))
}
