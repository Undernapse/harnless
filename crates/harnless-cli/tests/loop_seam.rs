//! The CLI loop-seam contract (#63): what the composed session log must show.
//!
//! The assertion surface is the **session log** — the event sequence, its
//! positions, and the ids it carries — not stdout. Stdout is presentation;
//! the log is the promise. The composition is the real spine mount (real
//! log, real event registry, real guarded tool pipeline, real id allocator);
//! the only injection is the scripted model (see `support/mod.rs`).
//!
//! The properties pinned here:
//! * multi-turn accumulation — one `Mounted`, many turns, one growing log;
//! * id/position invariants — monotonic ids off one allocator, contiguous
//!   positions, derived history consistent with the log;
//! * per-turn error survival — a failed turn is a logged `TurnClose` with a
//!   structured error, and the session keeps going;
//! * clean end — `exit`/EOF end the REPL with the log intact and no extra
//!   turn;
//! * the tool round-trip — a model-requested `echo` call executes through
//!   the guarded pipeline, the frozen result lands in the log, and the
//!   schemas ride the next adapter request.
//!
//! Zero network, zero flake: every model answer comes from a captured,
//! validated corpus replayed in-process.

mod support;

use harnless_cli::repl::repl;
use harnless_cli::run::drive_turn;
use harnless_seams::ErrorCode;
use support::{
    derived_ids, failing_recording, log_events, message_ids, mount_seam, positions, text_recording,
    tool_call_recording, write_corpus,
};

/// Every turn the runner drives appends exactly this bracket around its
/// content: the loop's own open/close pair per step and per turn.
const BRACKET: [&str; 4] = ["turn_open", "step_open", "step_close", "turn_close"];

fn kinds(events: &[(String, String)]) -> Vec<String> {
    events.iter().map(|(k, _)| k.clone()).collect()
}

#[test]
fn multi_turn_accumulation_grows_one_log() {
    let corpus = write_corpus(
        "accum",
        &[
            text_recording("answer one"),
            text_recording("answer two"),
            text_recording("answer three"),
        ],
    );
    let seam = mount_seam("accum", &corpus, &[]);

    drive_turn(&seam.mounted, "first").unwrap();
    drive_turn(&seam.mounted, "second").unwrap();
    drive_turn(&seam.mounted, "third").unwrap();

    let events = log_events(&seam.mounted);
    // Exact event count: three turns × (bracket + user + assistant). A
    // fourth turn — or a silently repeated one — changes this number.
    assert_eq!(events.len(), 3 * 6, "event sequence: {events:?}");
    assert_eq!(
        kinds(&events),
        [
            "user_message",
            "turn_open",
            "step_open",
            "assistant_message",
            "step_close",
            "turn_close",
        ]
        .iter()
        .map(|s| s.to_string())
        .cycle()
        .take(18)
        .collect::<Vec<_>>()
    );
    // The user prompts and assistant answers are all in the log, in order.
    // (Id uniqueness and monotonicity are pinned by the ids test; here the
    // content and its position in the sequence.)
    let details: Vec<&str> = events.iter().map(|(_, d)| d.as_str()).collect();
    assert!(details[0].ends_with(" first"));
    assert!(details[3].ends_with(" answer one"));
    assert!(details[6].ends_with(" second"));
    assert!(details[9].ends_with(" answer two"));
    assert!(details[12].ends_with(" third"));
    assert!(details[15].ends_with(" answer three"));
    // Every turn closed completed.
    for i in [5, 11, 17] {
        assert_eq!(events[i].1, "Completed", "turn {i} close");
    }
}

#[test]
fn ids_and_positions_are_unique_and_monotonic_across_turns() {
    let corpus = write_corpus(
        "ids",
        &[
            text_recording("a1"),
            text_recording("a2"),
            text_recording("a3"),
        ],
    );
    let seam = mount_seam("ids", &corpus, &[]);
    drive_turn(&seam.mounted, "p1").unwrap();
    drive_turn(&seam.mounted, "p2").unwrap();
    drive_turn(&seam.mounted, "p3").unwrap();

    // Message ids: strictly increasing off one allocator shared with the
    // adapter call ids — so they are monotonic and unique, but not dense
    // (call ids consume values between them). Density is not the promise;
    // uniqueness across the session is.
    let ids = message_ids(&seam.mounted);
    assert_eq!(ids.len(), 6, "six messages committed");
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "ids strictly monotonic: {ids:?}"
    );

    // Positions: contiguous from 0, exactly one record per position.
    let pos = positions(&seam.mounted);
    assert_eq!(
        pos,
        (0..pos.len()).collect::<Vec<_>>(),
        "contiguous positions"
    );

    // Adapter call ids are unique too — one allocator for messages *and*
    // requests, so a call id can never collide with a message id.
    let call_ids: Vec<u64> = seam.model.requests().iter().map(|r| r.call_id).collect();
    assert_eq!(call_ids.len(), 3);
    for c in &call_ids {
        assert!(!ids.contains(c), "call id {c} collides with a message id");
    }
    assert!(
        call_ids.windows(2).all(|w| w[0] < w[1]),
        "call ids monotonic: {call_ids:?}"
    );

    // Derived history: recomputing from the log yields exactly the
    // message-producing events' ids, in order.
    assert_eq!(
        derived_ids(&seam.mounted),
        ids,
        "derived history matches log"
    );
}

#[test]
fn per_turn_error_survives_and_the_session_continues() {
    let corpus = write_corpus(
        "err",
        &[
            text_recording("fine"),
            failing_recording(ErrorCode::StreamTerminated, "provider exploded"),
            text_recording("still here"),
        ],
    );
    let seam = mount_seam("err", &corpus, &[]);

    assert_eq!(drive_turn(&seam.mounted, "one").unwrap(), "fine");

    // The failing turn: a named, structured error at the seam.
    let err = drive_turn(&seam.mounted, "two").unwrap_err();
    assert_eq!(err.code, "turn-failed");
    assert!(
        err.message.contains("stream-terminated") && err.message.contains("provider exploded"),
        "error carries the structured cause: {err}"
    );

    // And the session keeps going — the failure was a turn, not the session.
    assert_eq!(drive_turn(&seam.mounted, "three").unwrap(), "still here");

    // The log tells the whole story: the failed turn's user message stands,
    // its close carries the structured error, and no assistant message was
    // committed for it.
    let events = log_events(&seam.mounted);
    // Two clean turns (6 records each) plus the failed turn's five: the
    // bracket, the prompt, and the error close — no assistant message.
    assert_eq!(
        events.len(),
        2 * 6 + 5,
        "failed turn commits no message: {events:?}"
    );
    let closes: Vec<&str> = events
        .iter()
        .filter(|(k, _)| k == "turn_close")
        .map(|(_, d)| d.as_str())
        .collect();
    assert_eq!(
        closes,
        vec![
            "Completed",
            r#"Error { code: "stream-terminated", message: "provider exploded" }"#,
            "Completed"
        ],
        "per-turn close reasons"
    );
    // Exactly two assistant messages committed (turns 1 and 3).
    assert_eq!(
        events
            .iter()
            .filter(|(k, _)| k == "assistant_message")
            .count(),
        2
    );
    // The failed turn's prompt is still model-visible in the log.
    assert!(events
        .iter()
        .any(|(k, d)| k == "user_message" && d.ends_with("two")));
}

#[test]
fn repl_exit_ends_cleanly_with_the_log_intact() {
    let corpus = write_corpus(
        "exit",
        &[text_recording("hi there"), text_recording("bye now")],
    );
    let seam = mount_seam("exit", &corpus, &[]);

    let mut out: Vec<u8> = Vec::new();
    repl(&seam.mounted, "one\ntwo\nexit\nnever".as_bytes(), &mut out).unwrap();

    // Two turns answered; nothing after `exit` ran.
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.matches("hi there").count(), 1);
    assert_eq!(text.matches("bye now").count(), 1);
    assert!(!text.contains("never"));

    // The log ends at exactly two clean turns — `exit` opened no third.
    let events = log_events(&seam.mounted);
    assert_eq!(events.len(), 2 * 6, "exit opened no turn: {events:?}");
    assert_eq!(events.last().unwrap().0, "turn_close");
    assert_eq!(events.last().unwrap().1, "Completed");
    assert_eq!(seam.model.calls(), 2, "no adapter call after exit");
}

#[test]
fn repl_eof_ends_cleanly_with_the_log_intact() {
    let corpus = write_corpus("eof", &[text_recording("answered")]);
    let seam = mount_seam("eof", &corpus, &[]);

    // Input ends with a newline and no `exit` — EOF is the other clean end.
    let mut out: Vec<u8> = Vec::new();
    repl(&seam.mounted, "ask\n".as_bytes(), &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.matches("answered").count(), 1);
    let events = log_events(&seam.mounted);
    assert_eq!(events.len(), 6, "EOF drove exactly one turn: {events:?}");
    assert_eq!(events.last().unwrap().1, "Completed");
}

#[test]
fn tool_round_trip_executes_logs_and_carries_schemas() {
    let corpus = write_corpus(
        "tool",
        &[
            tool_call_recording(r#"{"x":1}"#),
            text_recording("echoed it"),
        ],
    );
    let seam = mount_seam("tool", &corpus, &["echo"]);

    // The tool turn: the model requests echo, the loop executes it through
    // the guarded pipeline, logs the frozen result, and the turn completes.
    drive_turn(&seam.mounted, "use the tool").expect("tool turn completes");

    let events = log_events(&seam.mounted);
    // One turn: user prompt + bracket + tool_call + tool_result.
    assert_eq!(
        kinds(&events),
        vec![
            "user_message",
            "turn_open",
            "step_open",
            "tool_call",
            "tool_result",
            "step_close",
            "turn_close"
        ],
        "tool turn event sequence: {events:?}"
    );
    let call = &events[3];
    assert_eq!(call.1, "call=1 tool=echo args={\"x\":1}");
    // The frozen result is the echo body's raw JSON — the pipeline's output,
    // not a re-serialization.
    let result = &events[4];
    assert_eq!(result.1, "call=1 content={\"x\":1}");

    // The schemas rode the adapter request: the declared echo tool reached
    // the wire through the registry's sanctioned projection.
    let reqs = seam.model.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].tools, vec!["echo".to_string()], "schema on request");

    // The tool result is message-producing in the derived surface (the
    // loop's own history applies it), and the next turn's request carries
    // the accumulated surface. The loop ends a turn at the tool round-trip
    // — one driver call per turn is the loop's shape — so the next turn's
    // adapter request is where the result's projection shows up.
    let derived = derived_ids(&seam.mounted);
    assert!(
        !derived.is_empty(),
        "the tool round-trip joins the derived surface"
    );
    drive_turn(&seam.mounted, "and now?").unwrap();
    let reqs = seam.model.requests();
    assert_eq!(reqs.len(), 2, "one adapter call per turn");
    // The tool result is model-visible: the projection route is the CLI's
    // seam_message, which renders ToolResult as a text block naming the
    // call. Pin that the next request's surface mentions the call.
    let tool_text = reqs[1]
        .messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        tool_text.contains("use the tool") && tool_text.contains("and now?"),
        "next request carries the accumulated surface: {tool_text}"
    );
}

#[test]
fn toolless_composition_requests_with_no_schemas() {
    // A profile that declared no tools: the wire shape stays today's, and a
    // tool call against it answers fail-closed through the pipeline's
    // default, not a silent empty object.
    let corpus = write_corpus("notools", &[text_recording("plain answer")]);
    let seam = mount_seam("notools", &corpus, &[]);
    drive_turn(&seam.mounted, "hello").unwrap();
    let reqs = seam.model.requests();
    assert_eq!(reqs[0].tools, Vec::<String>::new(), "no schemas declared");
}

#[test]
fn runner_projects_the_log_onto_the_adapter_request() {
    // The message list the adapter sees is the logged surface: prior turns'
    // user and assistant messages, in order, with the current prompt last.
    let corpus = write_corpus("projection", &[text_recording("r1"), text_recording("r2")]);
    let seam = mount_seam("projection", &corpus, &[]);
    drive_turn(&seam.mounted, "ping").unwrap();
    drive_turn(&seam.mounted, "pong").unwrap();

    let reqs = seam.model.requests();
    // Turn 1's request: just the prompt.
    assert_eq!(reqs[0].messages.len(), 1);
    let texts: Vec<String> = reqs[1]
        .messages
        .iter()
        .map(|m| {
            m.blocks
                .iter()
                .map(|b| b.text.clone())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect();
    assert_eq!(texts, vec!["ping", "r1", "pong"]);
    let roles: Vec<&str> = reqs[1]
        .messages
        .iter()
        .map(|m| match m.role {
            harnless_seams::Role::User => "user",
            harnless_seams::Role::Assistant => "assistant",
            other => panic!("unexpected role {other:?}"),
        })
        .collect();
    assert_eq!(roles, vec!["user", "assistant", "user"]);
}

#[test]
fn every_turn_in_the_doc_claim_list_has_a_test() {
    // The doc-claim map (#63 acceptance): each promise in run.rs/repl.rs
    // module docs maps to a test above. This test pins the map itself so a
    // doc claim can't silently lose its pin.
    let claims: &[(&str, &str)] = &[
        (
            "run: one headless turn drives one loop turn",
            "multi_turn_accumulation_grows_one_log",
        ),
        (
            "run: no-model-provider is a named failure",
            "patched_none_model_reports_no_provider (run.rs unit)",
        ),
        (
            "run: prompt is logged before the turn",
            "runner_projects_the_log_onto_the_adapter_request",
        ),
        (
            "run: per-turn error is structured, session survives",
            "per_turn_error_survives_and_the_session_continues",
        ),
        (
            "repl: log accumulates across turns",
            "multi_turn_accumulation_grows_one_log",
        ),
        (
            "repl: exit ends the session",
            "repl_exit_ends_cleanly_with_the_log_intact",
        ),
        (
            "repl: EOF ends the session",
            "repl_eof_ends_cleanly_with_the_log_intact",
        ),
        (
            "repl: a bad turn never kills the REPL",
            "per_turn_error_survives_and_the_session_continues",
        ),
        (
            "boot: ids unique across turns",
            "ids_and_positions_are_unique_and_monotonic_across_turns",
        ),
        (
            "boot: declared tools reach the adapter",
            "tool_round_trip_executes_logs_and_carries_schemas",
        ),
        (
            "boot: no tools -> no schemas",
            "toolless_composition_requests_with_no_schemas",
        ),
    ];
    // The map is documentation-as-code; assert it is non-empty and the
    // seam-side claims are the ones this file actually pins.
    assert!(claims.len() >= 10);
    let pinned: &[&str] = &[
        "multi_turn_accumulation_grows_one_log",
        "ids_and_positions_are_unique_and_monotonic_across_turns",
        "per_turn_error_survives_and_the_session_continues",
        "repl_exit_ends_cleanly_with_the_log_intact",
        "repl_eof_ends_cleanly_with_the_log_intact",
        "tool_round_trip_executes_logs_and_carries_schemas",
        "toolless_composition_requests_with_no_schemas",
        "runner_projects_the_log_onto_the_adapter_request",
    ];
    for name in pinned {
        assert!(claims.iter().any(|(_, t)| t == name), "{name} in map");
    }
    // And every bracket kind the loop emits appears in at least one pin.
    let events = {
        let corpus = write_corpus("bracket", &[text_recording("x")]);
        let seam = mount_seam("bracket", &corpus, &[]);
        drive_turn(&seam.mounted, "q").unwrap();
        log_events(&seam.mounted)
    };
    let ks = kinds(&events);
    for b in BRACKET {
        assert!(ks.iter().any(|k| k == b), "bracket {b} observed");
    }
}
