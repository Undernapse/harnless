//! The primary seam test (#19): the agent loop observed through the session
//! log, driven by the replay adapter over a recorded model stream.
//!
//! Because model-visible means logged, the log *is* the observable behavior:
//! every test here drives turns with the [`Harness`] from
//! `harnless_agent::testsupport` (no network, no GPU, no provider
//! credentials) and asserts the **emitted event sequence** and the
//! **derived message list** through the log.
//!
//! The exercises pinned here (issue #19):
//!
//! * the event system + all five dispatch modes (`emit`, `parallel`,
//!   `serial`, `bail`, `waterfall`);
//! * effects and fibers (registrations unwind with their fiber);
//! * tool registration + the full guarded pipeline (pre-execute → monotonic
//!   guards → execute → post-execute → finalize → result);
//! * approval, failing closed;
//! * prompt assembly;
//! * persistence.
//!
//! The replay corpus is the golden-file directory `tests/corpus/`: recorded
//! [`Recording`] JSON documents, pinned byte-for-byte, replayed turn for
//! turn.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use harnless_agent::events::{ContentBlock, MessageRecord, SessionEvent, TurnEndReason};
use harnless_agent::testsupport::{
    event_kind, last_tool_call, parse_requested_tool_call, recording, render_block, text_recording,
    tool_recording, CountingTool, Harness, ScriptedCall,
};
use harnless_agent::tools::{PostExecute, PreExecute};
use harnless_agent::{History, SessionPersistence};
use harnless_runtime::events::{EventOptions, EventRegistry, Next};
use harnless_runtime::fiber::Fiber;
use harnless_seams::tools::{GuardVerdict, PostDecision, PreDecision};
use harnless_seams::{BlockKind, CallId, ErrorCode, MessageId, SessionId, StreamFrame};

use harnless_llm_replay::Recording;

/// Load a golden recording from the corpus directory.
fn corpus(name: &str) -> Recording {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/").to_string() + name;
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("corpus file {path} must exist: {e}"));
    Recording::from_json(&text).unwrap_or_else(|e| panic!("corpus {name} must parse: {e}"))
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Golden-file replay corpus
// ---------------------------------------------------------------------------

/// Pin the corpus itself: each golden file, re-serialized, is byte-for-byte
/// the file on disk (mirrors dsh's snapshot discipline — the recorded
/// document is the contract).
#[test]
fn corpus_golden_files_are_byte_for_byte_stable() {
    for name in ["answer_basic.json", "tool_echo.json", "answer_final.json"] {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/").to_string() + name;
        let on_disk = std::fs::read_to_string(&path).unwrap();
        let recording = Recording::from_json(&on_disk).unwrap();
        assert_eq!(
            recording.to_json().unwrap(),
            on_disk.trim_end_matches('\n'),
            "corpus file {name} drifted from its capture"
        );
    }
}

/// A corpus recording replays frame-for-frame through the adapter: capture
/// again from the replayed frames and the golden document is identical.
#[test]
fn corpus_recording_replays_frame_for_frame() {
    use futures::stream::StreamExt as _;
    let recording = corpus("answer_basic.json");
    let adapter = harnless_llm_replay::ReplayAdapter::new(
        "replay",
        harnless_llm_replay::Script::one(recording.clone()),
    );
    use harnless_seams::ModelAdapter as _;
    let runtime = rt();
    let frames = runtime.block_on(async {
        let mut stream = adapter
            .stream(CallId(1), &[], &[], None)
            .expect("valid corpus");
        let mut frames = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                harnless_seams::StreamEvent::Frame(f) => frames.push(f),
                harnless_seams::StreamEvent::Failed(f) => panic!("replay failed: {f:?}"),
            }
        }
        frames
    });
    let again = Recording::capture(&frames, &recording.to_replay_state());
    assert_eq!(again.to_json().unwrap(), recording.to_json().unwrap());
}

// ---------------------------------------------------------------------------
// The turn through the seam: event sequence + derived message list
// ---------------------------------------------------------------------------

/// Acceptance: a turn driven by the replay adapter yields the expected event
/// sequence and derived message list.
#[test]
fn replayed_turn_emits_the_pinned_event_sequence_and_history() {
    let h = Harness::with_script(
        SessionId(1901),
        vec![ScriptedCall::new(corpus("answer_basic.json"), MessageId(2))],
    );
    h.user(MessageId(1), "say hi");
    let turn = h.run_turn();

    assert_eq!(
        h.event_kinds(),
        vec![
            "user_message",
            "turn_open",
            "step_open",
            "assistant_message",
            "step_close",
            "turn_close",
        ]
    );
    assert_eq!(turn.reason, TurnEndReason::Completed);

    // Derived message list: exactly the two model-visible messages.
    let derived = h.derived();
    assert_eq!(derived.len(), 2);
    assert_eq!(
        derived[0].blocks,
        vec![ContentBlock::Text {
            text: "say hi".into()
        }]
    );
    assert_eq!(
        derived[1].blocks,
        vec![ContentBlock::Text {
            text: "Hello there".into()
        }]
    );
    assert_eq!(derived[1].message_id, MessageId(2));
}

/// The committed assistant message carries the replay adapter's provenance.
#[test]
fn replayed_turn_commits_provider_and_model_provenance() {
    let h = Harness::with_script(
        SessionId(1902),
        vec![ScriptedCall::new(corpus("answer_basic.json"), MessageId(2))],
    );
    let _ = h.run_turn();
    let snap = h.log.snapshot();
    let msg = snap
        .records
        .iter()
        .find_map(|r| match &r.event {
            SessionEvent::AssistantMessage(m) => Some(m),
            _ => None,
        })
        .expect("assistant message");
    assert_eq!(msg.provider.as_deref(), Some("replay"));
    assert_eq!(msg.model.as_deref(), Some("golden"));
}

/// Multiple turns replay their scripted recordings in call order, and the
/// derived history accumulates across turns — the log is the single source
/// of truth a resumed step reads.
#[test]
fn successive_turns_replay_their_script_in_order() {
    let h = Harness::with_script(
        SessionId(1903),
        vec![
            ScriptedCall::new(corpus("answer_basic.json"), MessageId(2)),
            ScriptedCall::new(corpus("answer_final.json"), MessageId(3)),
        ],
    );
    h.user(MessageId(1), "first");
    let t1 = h.run_turn();
    h.user(MessageId(4), "second");
    let t2 = h.run_turn();
    assert_eq!(t1.reason, TurnEndReason::Completed);
    assert_eq!(t2.reason, TurnEndReason::Completed);
    let derived = h.derived();
    let texts: Vec<String> = derived
        .iter()
        .map(|n| {
            n.blocks
                .iter()
                .map(render_block)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    assert_eq!(texts, vec!["first", "Hello there", "second", "done"]);
    // Event sequence: two full turn brackets, in order.
    assert_eq!(
        h.event_kinds(),
        vec![
            "user_message",
            "turn_open",
            "step_open",
            "assistant_message",
            "step_close",
            "turn_close",
            "user_message",
            "turn_open",
            "step_open",
            "assistant_message",
            "step_close",
            "turn_close",
        ]
    );
}

/// A scripted provider failure replays in-band and surfaces as a structured
/// turn error in the log — the sanctioned failure path observed at the seam.
#[test]
fn scripted_provider_failure_ends_the_turn_with_a_structured_error() {
    let mut recording = corpus("answer_final.json");
    // Drop the terminal Finish and stamp the scripted failure instead.
    recording
        .frames
        .retain(|f| !matches!(f, harnless_llm_replay::RecordedFrame::Finish));
    recording.failure = Some(harnless_seams::ProviderFailure {
        code: ErrorCode::StreamTerminated,
        message: "provider stalled".into(),
    });
    let h = Harness::with_script(
        SessionId(1904),
        vec![ScriptedCall::new(recording, MessageId(2))],
    );
    let turn = h.run_turn();
    assert!(matches!(
        turn.reason,
        TurnEndReason::Error { ref code, .. } if code == "stream-terminated"
    ));
    // No assistant message was committed; the bracket still closed.
    assert_eq!(
        h.event_kinds(),
        vec!["turn_open", "step_open", "step_close", "turn_close"]
    );
    let snap = h.log.snapshot();
    let last = snap.records.last().expect("turn close");
    match &last.event {
        SessionEvent::TurnClose {
            reason: TurnEndReason::Error { code, message },
        } => {
            assert_eq!(code, "stream-terminated");
            assert_eq!(message, "provider stalled");
        }
        other => panic!("expected error turn close, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The event system: all five dispatch modes at the seam
// ---------------------------------------------------------------------------

/// `emit`: fire-and-forget, registration order, no result. The frozen
/// tool-result notification the pipeline sends is exactly this mode — a
/// listener observes the finalized result without being able to change it.
#[test]
fn emit_dispatches_fire_and_forget_in_registration_order() {
    let h = Harness::with_script(
        SessionId(1910),
        vec![ScriptedCall::new(corpus("tool_echo.json"), MessageId(2))],
    );
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    for tag in ["first", "second"] {
        let order = order.clone();
        let _d = h
            .events
            .on(
                &h.fiber,
                move |n: &mut harnless_seams::tools::FrozenResult| {
                    order
                        .lock()
                        .unwrap()
                        .push((tag.to_string(), n.value.to_string()));
                },
                EventOptions::new(),
            )
            .unwrap();
    }
    h.allow_all().unwrap();
    h.register_tool(
        "echo",
        "test tool",
        serde_json::json!({}),
        CountingTool::new(r#"{"ok":true}"#),
    )
    .unwrap();
    let _ = h.run_turn();
    let seen = order.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            ("first".to_string(), r#"{"ok":true}"#.to_string()),
            ("second".to_string(), r#"{"ok":true}"#.to_string()),
        ],
        "emit runs every listener in registration order"
    );
}

/// `parallel`: all listeners run, all settle, one failure never cancels
/// siblings (failure containment, decision 03/04).
#[test]
fn parallel_dispatch_settles_every_listener_despite_failures() {
    let events = EventRegistry::new();
    let fiber = Fiber::active();
    let hits = Arc::new(AtomicUsize::new(0));
    for i in 0..3 {
        let hits = hits.clone();
        let _d = events
            .on(
                &fiber,
                move |_e: &mut String| {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if i == 1 {
                        panic!("contained listener failure");
                    }
                },
                EventOptions::new(),
            )
            .unwrap();
    }
    rt().block_on(events.parallel("payload".to_string()));
    assert_eq!(hits.load(Ordering::SeqCst), 3, "all listeners settled");
}

/// `serial`: registration order, stopping at the first bail.
#[test]
fn serial_dispatch_stops_at_the_first_bail() {
    let events = EventRegistry::new();
    let fiber = Fiber::active();
    let visited = Arc::new(std::sync::Mutex::new(Vec::new()));
    for i in 0..3 {
        let visited = visited.clone();
        let _d = events
            .on(
                &fiber,
                move |e: &mut String| {
                    visited.lock().unwrap().push(i);
                    if i == 1 {
                        *e = "bailed".into();
                    }
                },
                EventOptions::new(),
            )
            .unwrap();
    }
    let mut payload = String::new();
    let bail = events.serial(&mut payload, |e, slot| {
        slot.lock()(e);
        if *e == "bailed" {
            Some(e.clone())
        } else {
            None
        }
    });
    assert_eq!(bail.as_deref(), Some("bailed"));
    assert_eq!(
        *visited.lock().unwrap(),
        vec![0, 1],
        "walk stopped at the bail"
    );
}

/// `bail`: same ordered walk, inspecting synchronous returns.
#[test]
fn bail_dispatch_stops_on_the_first_synchronous_bail() {
    let events = EventRegistry::new();
    let fiber = Fiber::active();
    let visited = Arc::new(std::sync::Mutex::new(Vec::new()));
    for i in 0..3 {
        let visited = visited.clone();
        let _d = events
            .on(
                &fiber,
                move |e: &mut u32| {
                    visited.lock().unwrap().push(i);
                    if i == 0 {
                        *e = 42;
                    }
                },
                EventOptions::new(),
            )
            .unwrap();
    }
    let mut code = 0u32;
    let first = events.bail(&mut code, |e, slot| {
        slot.lock()(e);
        if *e != 0 {
            Some(*e)
        } else {
            None
        }
    });
    assert_eq!(first, Some(42));
    assert_eq!(*visited.lock().unwrap(), vec![0]);
}

/// `waterfall`: compose around the built-in; delegation propagates values,
/// absence of delegation vetoes. The loop's model-streaming commit and the
/// pipeline's pre/post stages are all this mode.
#[test]
fn waterfall_composes_delegation_and_veto() {
    let events = EventRegistry::new();
    let fiber = Fiber::active();
    // Outer listener transforms and delegates; inner listener sees the
    // transformed value.
    let _d1 = events
        .on_waterfall(
            &fiber,
            |e: &mut String, next: &mut Next<'_, String, String>| {
                *e = format!("[{e}]");
                next.call(e.clone())
            },
            EventOptions::new(),
        )
        .unwrap();
    let _d2 = events
        .on_waterfall(
            &fiber,
            |e: &mut String, next: &mut Next<'_, String, String>| {
                e.push('!');
                next.call(e.clone())
            },
            EventOptions::new(),
        )
        .unwrap();
    let out = events.waterfall("x".to_string(), |e| format!("built-in({e})"));
    assert_eq!(out, "built-in([x]!)");
    // A listener that never calls next() vetoes everything downstream.
    let _d3 = events
        .on_waterfall(
            &fiber,
            |e: &mut String, _next: &mut Next<'_, String, String>| format!("vetoed({e})"),
            EventOptions::prepend(),
        )
        .unwrap();
    let out = events.waterfall("y".to_string(), |e| format!("built-in({e})"));
    assert_eq!(out, "vetoed(y)", "the outermost veto owns the result");
}

// ---------------------------------------------------------------------------
// Effects and fibers
// ---------------------------------------------------------------------------

/// A registered `on_stream` waterfall listener transforms the assistant
/// message before commit — and its effect unwinds when its fiber unloads,
/// so the next turn over the same log commits the replayed message verbatim.
#[test]
fn stream_listener_transforms_then_unwinds_with_its_fiber() {
    let h = Harness::with_script(
        SessionId(1920),
        vec![
            ScriptedCall::new(corpus("answer_basic.json"), MessageId(2)),
            ScriptedCall::new(corpus("answer_basic.json"), MessageId(3)),
        ],
    );
    let plugin_fiber = Fiber::active();
    let disposer = h
        .loop_
        .on_stream(
            |msg: &mut MessageRecord, next: &mut Next<'_, MessageRecord, MessageRecord>| {
                if let Some(ContentBlock::Text { text }) = msg.blocks.first_mut() {
                    text.push_str(" [annotated]");
                }
                next.call(msg.clone())
            },
        )
        .unwrap();
    // The effect is owned by the harness fiber; hand teardown to a plugin
    // fiber by disposing explicitly when the plugin unloads.
    let _ = h.fiber.effect(move || {
        let mut disposer = Some(disposer);
        Some(Box::new(move || {
            if let Some(d) = disposer.take() {
                d.dispose();
            }
        }) as harnless_runtime::fiber::DisposeFn)
    });
    let _ = h.run_turn();
    let committed = |log: &harnless_agent::SessionLog, nth: usize| -> String {
        log.snapshot()
            .records
            .iter()
            .filter_map(|r| match &r.event {
                SessionEvent::AssistantMessage(m) => Some(
                    m.blocks
                        .iter()
                        .map(render_block)
                        .collect::<Vec<_>>()
                        .join("|"),
                ),
                _ => None,
            })
            .nth(nth)
            .expect("assistant message")
    };
    assert_eq!(committed(&h.log, 0), "Hello there [annotated]");
    // Unload the plugin: its fiber unwinds the listener effect in LIFO.
    plugin_fiber.dispose();
    h.fiber.dispose();
    assert_eq!(
        h.fiber.state(),
        harnless_runtime::fiber::FiberState::Disposed
    );
    // The loop still drives the log; the listener is gone.
    let text = {
        let turn = h.run_turn();
        turn.reason
    };
    assert_eq!(text, TurnEndReason::Completed);
    assert_eq!(committed(&h.log, 1), "Hello there");
}

// ---------------------------------------------------------------------------
// Tool registration + the full guarded pipeline
// ---------------------------------------------------------------------------

/// The locked-order pipeline runs end to end and every stage is observable:
/// pre-execute (allow) → monotonic guard (abstain) → execute (body runs) →
/// post-execute (replace) → finalize (frozen result) → notify (emit). The
/// tool exchange lands in the log as `tool_call` + `tool_result`, and the
/// derived history gains the tool-result node.
#[test]
fn guarded_pipeline_runs_every_stage_in_locked_order() {
    let trace = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let h = Harness::with_script(
        SessionId(1930),
        vec![
            ScriptedCall::new(corpus("tool_echo.json"), MessageId(2)),
            ScriptedCall::new(corpus("answer_final.json"), MessageId(3)),
        ],
    );
    let tool = CountingTool::new(r#"{"echo":"1"}"#);
    h.register_tool(
        "echo",
        "test tool",
        serde_json::json!({"type":"object"}),
        tool.clone(),
    )
    .unwrap();

    // 1. Pre-execute: allow (and record the stage).
    {
        let trace = trace.clone();
        h.tools
            .on_pre_execute(
                move |e: &mut PreExecute, _next: &mut Next<'_, PreExecute, PreDecision>| {
                    trace.lock().unwrap().push(format!("pre:{}", e.0));
                    PreDecision::Allow
                },
            )
            .unwrap();
    }
    // 2. Monotonic guard: abstain (registered by identity, not reorderable).
    {
        let trace = trace.clone();
        h.tools.add_guard(
            "never-denies",
            Arc::new(move |name: &str, _args: &[u8]| {
                trace.lock().unwrap().push(format!("guard:{name}"));
                GuardVerdict::Abstain
            }),
        );
    }
    // 4. Post-execute: observe the body's value and delegate it onward
    //    (the accept path of the post waterfall). The listener really runs:
    //    the stage dispatches under the key the registrar registers under.
    {
        let trace = trace.clone();
        h.tools
            .on_post_execute(
                move |e: &mut PostExecute, next: &mut harnless_agent::BridgeNext| {
                    trace.lock().unwrap().push(format!("post:{}", e.1));
                    next.call((e.0, e.1.clone()))
                },
            )
            .unwrap();
    }
    // 6. Notify: the frozen-result emission.
    {
        let trace = trace.clone();
        h.events
            .on(
                &h.fiber,
                move |f: &mut harnless_seams::tools::FrozenResult| {
                    trace.lock().unwrap().push(format!("notify:{}", f.value));
                },
                EventOptions::new(),
            )
            .unwrap();
    }

    let (_first, _second) = h.run_tool_turn();

    assert_eq!(
        trace.lock().unwrap().clone(),
        vec![
            "pre:echo",
            "guard:echo",
            "post:{\"echo\":\"1\"}",
            "notify:{\"echo\":\"1\"}",
        ],
        "stages ran in the locked order: pre → guard → body → post → notify",
    );
    assert_eq!(tool.invocations(), 1, "the body executed exactly once");
    // The frozen (post-replaced) value is what the loop logged as the result.
    let snap0 = h.log.snapshot();
    let logged_result = snap0
        .records
        .iter()
        .find_map(|r| match &r.event {
            SessionEvent::ToolResult(t) => Some(t.clone()),
            _ => None,
        })
        .expect("tool result");
    assert_eq!(logged_result.content, r#"{"echo":"1"}"#);
    assert_eq!(
        logged_result.call_id,
        CallId(8),
        "the minted call id correlates"
    );

    // The exchange is visible at the seam: call + result events, in order.
    let kinds = h.event_kinds();
    let call_at = kinds.iter().position(|k| k == "tool_call").expect("call");
    let result_at = kinds
        .iter()
        .position(|k| k == "tool_result")
        .expect("result");
    assert!(call_at < result_at);
    let derived = h.derived();
    // user? none. assistant? the tool turn commits none (tool-call step);
    // nodes: [tool result, final answer].
    assert_eq!(derived.len(), 2);
    assert_eq!(
        derived[0].blocks,
        vec![ContentBlock::ToolResult {
            call_id: CallId(8),
            content: r#"{"echo":"1"}"#.into(),
        }]
    );
    assert_eq!(
        derived[1].blocks,
        vec![ContentBlock::Text {
            text: "done".into()
        }]
    );
    // The logged call carries the recorded raw-JSON payload untouched.
    let snap = h.log.snapshot();
    let logged = snap
        .records
        .iter()
        .find_map(|r| match &r.event {
            SessionEvent::ToolCall(c) => Some(c.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(logged.tool, "echo");
    assert_eq!(logged.call_id, CallId(8));
    let requested = parse_requested_tool_call(&logged.arguments).expect("raw JSON payload");
    assert_eq!(requested.arguments, r#"{"x":1}"#);
}

// ---------------------------------------------------------------------------
// Approval: fail-closed
// ---------------------------------------------------------------------------

/// With no pre-execute listener at all, the pipeline denies — absence of an
/// answer is refusal, and the body never runs.
#[test]
fn approval_fails_closed_with_no_pre_execute_handler() {
    let h = Harness::with_script(
        SessionId(1940),
        vec![ScriptedCall::new(corpus("tool_echo.json"), MessageId(2))],
    );
    let tool = CountingTool::new(r#"{"ok":true}"#);
    h.register_tool("echo", "test tool", serde_json::json!({}), tool.clone())
        .unwrap();
    let _ = h.run_turn();
    assert_eq!(tool.invocations(), 0, "a denied call never runs the body");
    // The refusal is observable at the seam: the logged result is the
    // structured denial, never a body value.
    let snap = h.log.snapshot();
    let logged = snap
        .records
        .iter()
        .find_map(|r| match &r.event {
            SessionEvent::ToolResult(t) => Some(t.content.clone()),
            _ => None,
        })
        .expect("denial is logged as a result");
    assert_eq!(logged, r#"{"error":"tool-denied"}"#);
}

/// A pre-execute listener answering `Ask` (approval required, nobody
/// grants it in-process) is also a denial — the ask is not a yes.
#[test]
fn approval_ask_is_denied_not_deferred() {
    let h = Harness::with_script(
        SessionId(1941),
        vec![ScriptedCall::new(corpus("tool_echo.json"), MessageId(2))],
    );
    let tool = CountingTool::new(r#"{"ok":true}"#);
    h.register_tool("echo", "test tool", serde_json::json!({}), tool.clone())
        .unwrap();
    h.tools
        .on_pre_execute(
            |_: &mut PreExecute, _next: &mut Next<'_, PreExecute, PreDecision>| PreDecision::Ask,
        )
        .unwrap();
    let _ = h.run_turn();
    assert_eq!(tool.invocations(), 0);
    let snap = h.log.snapshot();
    let logged = snap
        .records
        .iter()
        .find_map(|r| match &r.event {
            SessionEvent::ToolResult(t) => Some(t.content.clone()),
            _ => None,
        })
        .expect("ask-without-answer is logged as a denial");
    assert_eq!(logged, r#"{"error":"tool-denied"}"#);
}

/// A monotonic guard's denial cannot be reordered around by the pre-execute
/// waterfall: even with allow registered, the protected guard refuses.
#[test]
fn monotonic_guard_denial_beats_pre_execute_allow() {
    let h = Harness::with_script(
        SessionId(1942),
        vec![ScriptedCall::new(corpus("tool_echo.json"), MessageId(2))],
    );
    let tool = CountingTool::new(r#"{"ok":true}"#);
    h.register_tool("echo", "test tool", serde_json::json!({}), tool.clone())
        .unwrap();
    h.allow_all().unwrap();
    h.tools.add_guard(
        "policy",
        Arc::new(|_name: &str, args: &[u8]| {
            if args.contains(&b'1') {
                GuardVerdict::Deny("one is not allowed".into())
            } else {
                GuardVerdict::Abstain
            }
        }),
    );
    let _ = h.run_turn();
    assert_eq!(tool.invocations(), 0);
    let snap = h.log.snapshot();
    let logged = snap
        .records
        .iter()
        .find_map(|r| match &r.event {
            SessionEvent::ToolResult(t) => Some(t.content.clone()),
            _ => None,
        })
        .expect("guard denial is logged as a result");
    assert_eq!(logged, r#"{"error":"tool-denied"}"#);
}

// ---------------------------------------------------------------------------
// Prompt assembly
// ---------------------------------------------------------------------------

/// The assembled prompt is derived fresh from the log: exactly the
/// message-producing events, in surface order, rendered the same way the
/// surface renders — structural records never appear.
#[test]
fn prompt_assembly_mirrors_the_derived_surface() {
    let h = Harness::with_script(
        SessionId(1950),
        vec![
            ScriptedCall::new(corpus("tool_echo.json"), MessageId(2)),
            ScriptedCall::new(corpus("answer_final.json"), MessageId(3)),
        ],
    );
    h.user(MessageId(1), "run echo");
    h.allow_all().unwrap();
    h.register_tool(
        "echo",
        "test tool",
        serde_json::json!({}),
        CountingTool::new(r#"{"ok":true}"#),
    )
    .unwrap();
    let _ = h.run_tool_turn();
    let p = h.prompt("You are helpful.");
    assert_eq!(p.base, "You are helpful.");
    assert_eq!(
        p.conversation,
        vec![
            "run echo".to_string(),
            "[tool result callid-8] {\"ok\":true}".to_string(),
            "done".to_string(),
        ],
        "prompt lines are exactly the derived surface, in order"
    );
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// File-backed JSONL persistence backend (test-side; the lib owns no I/O).
struct JsonlFileBackend {
    dir: std::path::PathBuf,
}

impl SessionPersistence for JsonlFileBackend {
    fn save(
        &mut self,
        session: &SessionId,
        batch: &[SessionEvent],
    ) -> impl Future<Output = std::result::Result<(), String>> + Send {
        let path = self.dir.join(format!("session-{}.jsonl", session.0));
        async move {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| e.to_string())?;
            for event in batch {
                writeln!(file, "{}", serde_json::to_string(event).unwrap())
                    .map_err(|e| e.to_string())?;
            }
            file.sync_all().map_err(|e| e.to_string())
        }
    }

    fn load(
        &mut self,
        session: &SessionId,
    ) -> impl Future<Output = std::result::Result<Option<harnless_agent::LoadedLog>, String>> + Send
    {
        let path = self.dir.join(format!("session-{}.jsonl", session.0));
        async move {
            if !path.exists() {
                return Ok(None);
            }
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let mut events = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let event: SessionEvent =
                    serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
                events.push(event);
            }
            Ok(Some(harnless_agent::LoadedLog { events }))
        }
    }
}

/// Persistence at the seam: the replay-driven log checkpoints through the
/// backend and loads back exactly — and a log rebuilt from the loaded
/// events derives the same message list the live log derived.
#[test]
fn persisted_log_round_trips_and_rederives_the_same_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut backend = JsonlFileBackend {
        dir: dir.path().to_path_buf(),
    };
    let h = Harness::with_script(
        SessionId(1960),
        vec![
            ScriptedCall::new(corpus("tool_echo.json"), MessageId(2)),
            ScriptedCall::new(corpus("answer_final.json"), MessageId(3)),
        ],
    );
    h.user(MessageId(1), "run echo");
    h.allow_all().unwrap();
    h.register_tool(
        "echo",
        "test tool",
        serde_json::json!({}),
        CountingTool::new(r#"{"ok":true}"#),
    )
    .unwrap();
    let _ = h.run_tool_turn();
    let live = h.derived();
    let loaded = rt().block_on(h.checkpoint_load(&mut backend));
    // Exact events, byte-for-byte after JSON.
    let live_events: Vec<SessionEvent> = h
        .log
        .snapshot()
        .records
        .into_iter()
        .map(|r| r.event)
        .collect();
    assert_eq!(loaded.events.len(), live_events.len());
    for (got, want) in loaded.events.iter().zip(&live_events) {
        assert_eq!(
            serde_json::to_string(got).unwrap(),
            serde_json::to_string(want).unwrap()
        );
    }
    // Re-derivation from the loaded events yields the same message list.
    let mut rebuilt = History::default();
    for event in &loaded.events {
        rebuilt.apply(event);
    }
    assert_eq!(rebuilt.nodes(), live.as_slice());
}

/// The post-execute stage of the locked pipeline dispatches every registered
/// [`harnless_agent::ToolRegistry::on_post_execute`] listener around the
/// body's value (accept, block, replace, add context).
///
/// The registry and the registrar must agree on one dispatch key: the runtime
/// keys waterfall slots by `(TypeId<E>, TypeId<R>)`, so a stage that
/// dispatches under a result type nobody registers under composes an empty
/// chain and silently freezes the body's value. This test is the pin: the
/// listener observes the body's value and delegates it onward.
#[test]
fn post_execute_listener_runs_in_the_locked_order() {
    let h = Harness::with_script(
        SessionId(1931),
        vec![
            ScriptedCall::new(corpus("tool_echo.json"), MessageId(2)),
            ScriptedCall::new(corpus("answer_final.json"), MessageId(3)),
        ],
    );
    let tool = CountingTool::new(r#"{"echo":"1"}"#);
    h.register_tool(
        "echo",
        "test tool",
        serde_json::json!({"type":"object"}),
        tool.clone(),
    )
    .unwrap();
    h.allow_all().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let seen = seen.clone();
        h.tools
            .on_post_execute(
                move |e: &mut PostExecute, next: &mut harnless_agent::BridgeNext| {
                    seen.lock().unwrap().push(format!("post:{}", e.1));
                    next.call((e.0, e.1.clone()))
                },
            )
            .unwrap();
    }
    let _ = h.run_tool_turn();
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["post:{\"echo\":\"1\"}"],
        "the post-execute listener must observe the body's value"
    );
}

// ---------------------------------------------------------------------------
// Post-execute decisions, observed through the log
// ---------------------------------------------------------------------------

/// A harness scripted for one full tool exchange (`echo` body → final
/// answer), with the tool registered and approval granted — the shared
/// setup for the `PostDecision` outcome tests below.
fn tool_exchange_harness(session: u64) -> (Harness, Arc<CountingTool>) {
    let h = Harness::with_script(
        SessionId(session),
        vec![
            ScriptedCall::new(corpus("tool_echo.json"), MessageId(2)),
            ScriptedCall::new(corpus("answer_final.json"), MessageId(3)),
        ],
    );
    let tool = CountingTool::new(r#"{"echo":"1"}"#);
    h.register_tool(
        "echo",
        "test tool",
        serde_json::json!({"type":"object"}),
        tool.clone(),
    )
    .unwrap();
    h.allow_all().unwrap();
    (h, tool)
}

/// The content of the first logged tool result.
fn first_logged_tool_result(h: &Harness) -> String {
    h.log
        .snapshot()
        .records
        .iter()
        .find_map(|r| match &r.event {
            SessionEvent::ToolResult(t) => Some(t.content.clone()),
            _ => None,
        })
        .expect("a tool exchange logs a result")
}

/// A `Replace` decision changes what the loop logs as the tool result: the
/// model sees the listener's value, not the body's.
#[test]
fn post_execute_replace_changes_the_logged_tool_result() {
    let (h, tool) = tool_exchange_harness(1932);
    let _post = h
        .tools
        .on_post_execute(
            |_: &mut PostExecute, _next: &mut harnless_agent::BridgeNext| {
                PostDecision::Replace(serde_json::json!({"redacted": true}))
            },
        )
        .unwrap();
    let _ = h.run_tool_turn();
    assert_eq!(tool.invocations(), 1, "the body still ran");
    assert_eq!(
        first_logged_tool_result(&h),
        r#"{"redacted":true}"#,
        "the replaced value is what the log — and therefore the model — sees"
    );
    // The derived surface projects the replaced content too.
    let derived = h.derived();
    assert!(matches!(
        &derived[0].blocks[0],
        ContentBlock::ToolResult { content, .. } if content == r#"{"redacted":true}"#
    ));
}

/// A `Block` decision makes the logged tool result the structured denial:
/// the `tool-denied` code, never the body's value.
#[test]
fn post_execute_block_logs_the_denial_code() {
    let (h, tool) = tool_exchange_harness(1933);
    let _post = h
        .tools
        .on_post_execute(
            |_: &mut PostExecute, _next: &mut harnless_agent::BridgeNext| {
                PostDecision::Block("policy says no".into())
            },
        )
        .unwrap();
    let _ = h.run_tool_turn();
    assert_eq!(tool.invocations(), 1, "the body ran before the post stage");
    // The loop renders an errored pipeline as a structured error result;
    // consumers route on the code, so that is what the log must carry.
    assert_eq!(first_logged_tool_result(&h), r#"{"error":"tool-denied"}"#);
}

/// An `AddContext` decision leaves the logged tool result byte-for-byte the
/// body's value and surfaces the extra context as its own model-visible
/// record.
#[test]
fn post_execute_add_context_keeps_the_result_and_surfaces_context() {
    let (h, _tool) = tool_exchange_harness(1934);
    let _post = h
        .tools
        .on_post_execute(
            |_: &mut PostExecute, _next: &mut harnless_agent::BridgeNext| {
                PostDecision::AddContext(vec![serde_json::json!({"note": "extra"})])
            },
        )
        .unwrap();
    let _ = h.run_tool_turn();
    // Both records are in the log, in order: the sink's context record is
    // delivered inside `execute`, the loop's result record lands after.
    let contents: Vec<String> = h
        .log
        .snapshot()
        .records
        .iter()
        .filter_map(|r| match &r.event {
            SessionEvent::ToolResult(t) => Some(t.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        contents,
        vec![
            r#"[context] {"note":"extra"}"#.to_string(),
            r#"{"echo":"1"}"#.to_string()
        ],
        "the context is surfaced without corrupting the result"
    );
    // The derived surface carries the intact result — the model's view of
    // the call is exactly what the accepted stage produced.
    let derived = h.derived();
    assert!(matches!(
        &derived[1].blocks[0],
        ContentBlock::ToolResult { content, .. } if content == r#"{"echo":"1"}"#
    ));
}

/// A post listener that returns without calling `next` vetoes the remainder:
/// later listeners never run, and neither does the built-in accept — the
/// vetoing decision alone is what the log shows.
#[test]
fn post_execute_veto_hides_later_listeners_and_the_built_in() {
    let (h, _tool) = tool_exchange_harness(1935);
    let later_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _outer = h
        .tools
        .on_post_execute(
            |_: &mut PostExecute, _next: &mut harnless_agent::BridgeNext| {
                PostDecision::Replace(serde_json::json!({"vetoed": true}))
            },
        )
        .unwrap();
    let ran = later_ran.clone();
    let _inner = h
        .tools
        .on_post_execute(
            move |_: &mut PostExecute, _next: &mut harnless_agent::BridgeNext| {
                ran.store(true, std::sync::atomic::Ordering::SeqCst);
                PostDecision::Replace(serde_json::json!({"inner": true}))
            },
        )
        .unwrap();
    let _ = h.run_tool_turn();
    assert!(
        !later_ran.load(std::sync::atomic::Ordering::SeqCst),
        "a veto must not run later listeners"
    );
    assert_eq!(
        first_logged_tool_result(&h),
        r#"{"vetoed":true}"#,
        "only the vetoing decision's value reaches the log"
    );
}

/// Listeners compose in registration order, outermost first: the outer
/// listener sees (and may amend) what its `next` answered, and the
/// outermost value is what the log carries.
#[test]
fn post_execute_listeners_compose_outermost_first_through_the_log() {
    let (h, _tool) = tool_exchange_harness(1936);
    let order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let trace = order.clone();
    let _outer = h
        .tools
        .on_post_execute(
            move |e: &mut PostExecute, next: &mut harnless_agent::BridgeNext| {
                trace.lock().unwrap().push("outer".into());
                match next.call((e.0, e.1.clone())) {
                    PostDecision::Replace(v) => {
                        let mut obj = v.as_object().cloned().unwrap_or_default();
                        obj.insert("outer".into(), serde_json::json!(true));
                        PostDecision::Replace(serde_json::Value::Object(obj))
                    }
                    other => other,
                }
            },
        )
        .unwrap();
    let trace = order.clone();
    let _inner = h
        .tools
        .on_post_execute(
            move |_: &mut PostExecute, _next: &mut harnless_agent::BridgeNext| {
                trace.lock().unwrap().push("inner".into());
                PostDecision::Replace(serde_json::json!({"inner": true}))
            },
        )
        .unwrap();
    let _ = h.run_tool_turn();
    assert_eq!(
        *order.lock().unwrap(),
        vec!["outer", "inner"],
        "registration order is outermost-first"
    );
    assert_eq!(
        first_logged_tool_result(&h),
        r#"{"inner":true,"outer":true}"#,
        "the outer listener amended the inner listener's replacement"
    );
}

// ---------------------------------------------------------------------------
// Test-support surface: helpers other crates reuse at this seam
// ---------------------------------------------------------------------------

/// The corpus builders round-trip: a recording built with
/// [`text_recording`]/[`tool_recording`] parses back to itself, so a test
/// that scripts a call inline still pins a stable golden document.
#[test]
fn corpus_builders_round_trip() {
    let t = text_recording(&["a", "b"]);
    assert_eq!(Recording::from_json(&t.to_json().unwrap()).unwrap(), t);
    let u = tool_recording(9, "bash", r#"{"cmd":"ls"}"#);
    assert_eq!(Recording::from_json(&u.to_json().unwrap()).unwrap(), u);
    assert_eq!(u.tool_calls.len(), 1);
    assert_eq!(u.tool_calls[0].name, "bash");
    // The frames builder produces the same document from the same frames.
    let built = recording(&u.to_frames().unwrap(), &u.to_replay_state());
    assert_eq!(built.to_json().unwrap(), u.to_json().unwrap());
    // Kind spellings are stable and total over the vocabulary.
    assert_eq!(event_kind(&SessionEvent::TurnOpen), "turn_open");
    assert_eq!(
        event_kind(&SessionEvent::TurnClose {
            reason: TurnEndReason::Blocked,
        }),
        "turn_close"
    );
    // A requested call parses out of its recorded payload.
    let r = parse_requested_tool_call(r#"{"id":9,"name":"bash","arguments":"{\"cmd\":\"ls\"}"}"#)
        .expect("payload");
    assert_eq!(r.call_id, CallId(9));
    assert_eq!(r.tool, "bash");
    assert_eq!(r.arguments, r#"{"cmd":"ls"}"#);
    // The display renderer matches the prompt assembly's rendering.
    assert_eq!(
        render_block(&ContentBlock::ToolResult {
            call_id: CallId(3),
            content: "x".into(),
        }),
        "[tool result callid-3] x"
    );
    // The most recent logged call is discoverable.
    let h = Harness::with_script(
        SessionId(1970),
        vec![ScriptedCall::new(corpus("tool_echo.json"), MessageId(2))],
    );
    assert!(last_tool_call(&h.log).is_none());
    let _ = h.run_turn();
    assert_eq!(last_tool_call(&h.log).expect("call").tool, "echo");
    // BlockKind spellings used by the builders are the seam's own.
    assert_eq!(BlockKind::Text.as_str(), "text");
    assert!(matches!(
        u.frames.last(),
        Some(harnless_llm_replay::RecordedFrame::Finish)
    ));
    let _ = StreamFrame::Finish; // the builder vocabulary is the seam's frames
}

/// An unscripted harness still drives turns: the placeholder recording
/// replays (repeat-last semantics) and commits deterministically, so helper
/// tests that only exercise the pipeline never panic on an empty script.
#[test]
fn unscripted_harness_drives_turns_without_panicking() {
    let h = Harness::new(SessionId(1980));
    let turn = h.run_turn();
    assert_eq!(turn.reason, TurnEndReason::Completed);
    // Past the (placeholder) script's end the last recording repeats.
    let _ = h.run_turn();
    assert_eq!(
        h.event_kinds(),
        vec![
            "turn_open",
            "step_open",
            "assistant_message",
            "step_close",
            "turn_close",
            "turn_open",
            "step_open",
            "assistant_message",
            "step_close",
            "turn_close",
        ]
    );
}
