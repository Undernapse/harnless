//! Self-tests for the model-adapter conformance suite.
//!
//! The kit is only trustworthy if it passes a conforming reference adapter —
//! and, exactly as `fs_conformance.rs` proves for the filesystem suite, if
//! every case actually *bites* a provider that breaks the obligation that
//! case exists for. The reference here is a small in-memory scripted adapter
//! that replays a fixed corpus; the negative fixtures are that same adapter
//! with one obligation broken, so each violation the suite reports is
//! attributable to one defect.
//!
//! The fixtures also prove the suite's never-panic rule: a provider that
//! panics, throws, or hangs must produce [`Violation`]s, never a crash.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use harnless_conformance::adapter_suite::{
    check_model_adapter_contract, check_model_adapter_contract_all, Scenario,
    ADAPTER_CONFORMANCE_CASES,
};
use harnless_seams::error::{ErrorCode, Result, SeamError};
use harnless_seams::ids::MessageId;
use harnless_seams::llm::{
    BlockKind, BoxStream, ContentBlock, Message, ModelAdapter, ProviderFailure, ReplayState, Role,
    StreamEvent, StreamFrame, ToolSchema, Usage,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// The scripted corpus
// ---------------------------------------------------------------------------

/// One scripted turn the reference adapter replays.
///
/// Mirrors what a provider's own golden file holds: the frames, the response
/// metadata, the per-block metadata, and the terminal failure if the turn
/// ended in one.
#[derive(Clone, Debug)]
struct Corpus {
    frames: Vec<StreamFrame>,
    response: Option<Value>,
    blocks: Vec<Value>,
    failure: Option<ProviderFailure>,
    /// Whether the turn is silent (no content frames at all).
    silent: bool,
}

impl Corpus {
    fn text_turn() -> Self {
        Self {
            frames: vec![
                StreamFrame::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                },
                StreamFrame::TextDelta {
                    index: 0,
                    text: "hello".into(),
                },
                StreamFrame::BlockEnd {
                    index: 0,
                    assembled: ContentBlock {
                        kind: BlockKind::Text,
                        text: "hello".into(),
                    },
                },
                StreamFrame::Usage(Usage {
                    uncached_input: 100,
                    cached_reads: 30,
                    cached_writes: 5,
                    output: 40,
                    reasoning: 12,
                }),
                StreamFrame::Finish,
            ],
            response: Some(json!({"id": "resp-1"})),
            blocks: vec![json!({"index": 0})],
            failure: None,
            silent: false,
        }
    }

    /// A tool turn whose arguments are the suite's awkward raw JSON, streamed
    /// in two fragments.
    fn tool_turn() -> Self {
        let raw = harnless_conformance::adapter_suite::RAW_TOOL_ARGS;
        let (head, tail) = raw.split_at(24);
        Self {
            frames: vec![
                StreamFrame::BlockStart {
                    index: 0,
                    kind: BlockKind::ToolCall,
                },
                StreamFrame::ToolCallDelta {
                    index: 0,
                    call_id: harnless_seams::CallId(7),
                    json: head.to_string(),
                },
                StreamFrame::ToolCallDelta {
                    index: 0,
                    call_id: harnless_seams::CallId(7),
                    json: tail.to_string(),
                },
                StreamFrame::BlockEnd {
                    index: 0,
                    assembled: ContentBlock {
                        kind: BlockKind::ToolCall,
                        text: raw.to_string(),
                    },
                },
                StreamFrame::Usage(Usage::default()),
                StreamFrame::Finish,
            ],
            response: None,
            blocks: vec![json!({"index": 0})],
            failure: None,
            silent: false,
        }
    }

    fn failed_turn(code: ErrorCode) -> Self {
        Self {
            frames: vec![
                StreamFrame::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                },
                StreamFrame::BlockEnd {
                    index: 0,
                    assembled: ContentBlock {
                        kind: BlockKind::Text,
                        text: "partial".into(),
                    },
                },
            ],
            response: None,
            blocks: vec![json!({"index": 0})],
            failure: Some(ProviderFailure {
                code,
                message: "scripted failure".into(),
            }),
            silent: false,
        }
    }

    /// A completion with no content at all.
    fn silent_turn() -> Self {
        Self {
            frames: Vec::new(),
            response: None,
            blocks: Vec::new(),
            failure: None,
            silent: true,
        }
    }

    /// Blocks emitted in **descending** index order: the interleaving that
    /// tells emission-order alignment apart from index-order alignment.
    fn interleaved_turn() -> Self {
        Self {
            frames: vec![
                StreamFrame::BlockStart {
                    index: 1,
                    kind: BlockKind::Text,
                },
                StreamFrame::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                },
                StreamFrame::BlockEnd {
                    index: 1,
                    assembled: ContentBlock {
                        kind: BlockKind::Text,
                        text: "second".into(),
                    },
                },
                StreamFrame::BlockEnd {
                    index: 0,
                    assembled: ContentBlock {
                        kind: BlockKind::Text,
                        text: "first".into(),
                    },
                },
                StreamFrame::Usage(Usage::default()),
                StreamFrame::Finish,
            ],
            response: Some(json!({"id": "resp-2"})),
            // Metadata appended at each `BlockEnd`, i.e. in emission order.
            blocks: vec![json!({"index": 1}), json!({"index": 0})],
            failure: None,
            silent: false,
        }
    }

    /// The corpus the suite's `case` is expected to drive.
    fn for_case(case: &str) -> Self {
        match case {
            "raw_json_tool_arguments" => Self::tool_turn(),
            "sanctioned_failure_paths" | "in_band_failure_is_terminal" => {
                Self::failed_turn(ErrorCode::ProviderFailure)
            }
            "empty_completion_is_retryable_failure" => Self::silent_turn(),
            "context_overflow_canonical_code" => Self::failed_turn(ErrorCode::ContextOverflow),
            "replay_alignment_is_emission_order" => Self::interleaved_turn(),
            _ => Self::text_turn(),
        }
    }

    /// The metadata this corpus's adapter appends, defect applied.
    ///
    /// The `IndexOrderAlignment` fixture is bug #25 in adapter form: it stores
    /// its per-block entries in *index* order even though it emitted the
    /// blocks in descending index order.
    fn published_metadata(&self, defect: Defect) -> Vec<Value> {
        match defect {
            Defect::IndexOrderAlignment => {
                let mut blocks = self.blocks.clone();
                blocks.sort_by_key(|entry| {
                    entry.get("index").and_then(|v| v.as_u64()).unwrap_or(0)
                });
                blocks
            }
            _ => self.blocks.clone(),
        }
    }

    /// The scenario a provider harness would build for this corpus.
    fn scenario(&self, case: &str, defect: Defect) -> Scenario {
        let frames = match defect {
            // The re-serialising adapter buffers its fragments and rewrites
            // the assembled payload; its scenario must describe the honest
            // corpus so the suite compares against what it owes.
            _ => self.frames.clone(),
        };
        let scenario = Scenario::new(vec![user_message()])
            .expected(frames)
            .published_metadata(self.published_metadata(defect));
        let scenario = match &self.response {
            Some(response) => scenario.response_metadata(response.clone()),
            None => scenario,
        };
        let scenario = match &self.failure {
            Some(failure) => scenario.failure(failure.clone()),
            None => scenario,
        };
        if case == "replay_state_ownership" {
            scenario.with_owned_replay()
        } else {
            scenario
        }
    }
}

fn user_message() -> Message {
    Message {
        id: MessageId(1),
        role: Role::User,
        blocks: vec![ContentBlock {
            kind: BlockKind::Text,
            text: "hi".into(),
        }],
        provider: None,
        model: None,
        replay_state: None,
    }
}

// ---------------------------------------------------------------------------
// Defects
// ---------------------------------------------------------------------------

/// One broken obligation per variant.
///
/// One knob per fixture, so "the reference adapter minus one contract" is the
/// only thing separating a passing run from a reported violation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Defect {
    /// Honors every obligation.
    None,
    /// Emits `Finish` before `Usage`.
    UsageAfterFinish,
    /// Emits a frame after `Finish`.
    FrameAfterFinish,
    /// Parses and re-serialises tool arguments.
    ReserialisesArgs,
    /// Ends a scripted failure with the wrong code.
    MiscodedFailure,
    /// Swallows a scripted failure and finishes successfully.
    DropsFailure,
    /// Speaks again after the in-band terminal failure.
    EventAfterFailure,
    /// Reports a silent completion as a success.
    SilentIsSuccess,
    /// Reports a silent completion under the wrong code.
    MiscodedEmpty,
    /// Classifies context overflow as a generic provider failure.
    MiscodedOverflow,
    /// Counts cached reads twice in the published input parts.
    DoubleCountsInput,
    /// Re-adds reasoning on top of an output that already contains it.
    ReAddsReasoning,
    /// Claims every replay state regardless of owner marker.
    ClaimsEverything,
    /// Aligns replay metadata by index order instead of emission order.
    IndexOrderAlignment,
    /// Throws from `stream()` instead of streaming.
    Throws,
    /// Panics from `stream()`.
    Panics,
    /// Never terminates the stream.
    Hangs,
}

/// The scripted reference adapter: replays its corpus with one defect applied.
struct Scripted {
    defect: Defect,
    corpus: Corpus,
}

impl Scripted {
    fn new(defect: Defect, case: &str) -> Self {
        Self {
            defect,
            corpus: Corpus::for_case(case),
        }
    }

    /// The frames this adapter emits for its corpus, defect applied.
    fn frames(&self) -> Vec<StreamFrame> {
        let mut frames = self.corpus.frames.clone();
        match self.defect {
            Defect::UsageAfterFinish => {
                if let Some(pos) = frames
                    .iter()
                    .position(|f| matches!(f, StreamFrame::Usage(_)))
                {
                    let usage = frames.remove(pos);
                    frames.push(usage);
                }
            }
            Defect::FrameAfterFinish => frames.push(StreamFrame::TextDelta {
                index: 0,
                text: "after finish".into(),
            }),
            Defect::ReserialisesArgs => {
                for frame in &mut frames {
                    if let StreamFrame::BlockEnd { assembled, .. } = frame {
                        if assembled.kind == BlockKind::ToolCall {
                            let parsed: Value =
                                serde_json::from_str(&assembled.text).expect("valid json");
                            assembled.text = serde_json::to_string(&parsed).expect("serialize");
                        }
                    }
                }
                // A re-serialiser buffers: it stops streaming fragments.
                frames.retain(|f| !matches!(f, StreamFrame::ToolCallDelta { .. }));
            }
            Defect::IndexOrderAlignment => {
                // The provider stores metadata in *index* order while
                // emitting in emission order: exactly bug #25. Its frames are
                // honest; the mispairing is in the alignment it publishes.
            }
            _ => {}
        }
        frames
    }

    /// The usage this adapter publishes, defect applied.
    fn usage(&self) -> Usage {
        let mut usage = match self.corpus.frames.iter().find_map(|f| match f {
            StreamFrame::Usage(usage) => Some(*usage),
            _ => None,
        }) {
            Some(usage) => usage,
            None => Usage::default(),
        };
        match self.defect {
            Defect::DoubleCountsInput => {
                // Cached reads also reported as uncached input: the published
                // parts no longer sum to what a consumer bills.
                usage.uncached_input += usage.cached_reads;
            }
            Defect::ReAddsReasoning => {
                usage.output += usage.reasoning;
            }
            _ => {}
        }
        usage
    }

    /// Whether the adapter's stream should end in-band.
    fn terminal_failure(&self) -> Option<ProviderFailure> {
        let scripted = self.corpus.failure.clone()?;
        let code = match self.defect {
            Defect::MiscodedFailure => ErrorCode::StreamTerminated,
            Defect::MiscodedOverflow => ErrorCode::ProviderFailure,
            _ => scripted.code,
        };
        Some(ProviderFailure {
            code,
            message: scripted.message,
        })
    }

    fn owns_state(state: &ReplayState, claim_all: bool) -> bool {
        if claim_all {
            return true;
        }
        state
            .response
            .as_ref()
            .and_then(|r| r.get("harnless_conformance_owned"))
            .and_then(|owner| owner.get("key"))
            .and_then(|v| v.as_str())
            == Some("harnless-conformance-self")
    }
}

impl ModelAdapter for Scripted {
    fn provider(&self) -> &str {
        "scripted"
    }

    fn owns(&self, replay_state: &ReplayState) -> bool {
        // The alignment defect stores index-ordered metadata, which is a
        // separate obligation from ownership; ownership stays honest.
        Scripted::owns_state(replay_state, self.defect == Defect::ClaimsEverything)
    }

    fn stream(
        &self,
        _call_id: harnless_seams::CallId,
        _messages: &[Message],
        _tools: &[ToolSchema],
        _replay: Option<ReplayState>,
    ) -> Result<BoxStream> {
        match self.defect {
            Defect::Throws => {
                return Err(SeamError::new(
                    ErrorCode::ProviderFailure,
                    "scripted throw path",
                ))
            }
            Defect::Panics => panic!("scripted provider exploded"),
            Defect::Hangs => {
                let stream = async_stream::stream! {
                    futures_lite::future::pending::<()>().await;
                    #[allow(unreachable_code)]
                    yield StreamEvent::Frame(StreamFrame::Finish);
                };
                return Ok(Box::pin(stream));
            }
            _ => {}
        }

        let frames = self.frames();
        let failure = self.terminal_failure();
        let drops_failure = self.defect == Defect::DropsFailure;
        let silent_is_success = self.defect == Defect::SilentIsSuccess;
        let miscoded_empty = self.defect == Defect::MiscodedEmpty;
        let event_after_failure = self.defect == Defect::EventAfterFailure;
        let silent = self.corpus.silent;
        let usage = self.usage();
        let rewrites_usage = matches!(
            self.defect,
            Defect::DoubleCountsInput | Defect::ReAddsReasoning
        );

        let stream = async_stream::stream! {
            for frame in frames {
                if rewrites_usage && matches!(frame, StreamFrame::Usage(_)) {
                    yield StreamEvent::Frame(StreamFrame::Usage(usage));
                } else {
                    yield StreamEvent::Frame(frame);
                }
            }
            if let Some(failure) = failure {
                if drops_failure {
                    yield StreamEvent::Frame(StreamFrame::Usage(usage));
                    yield StreamEvent::Frame(StreamFrame::Finish);
                } else {
                    yield StreamEvent::Failed(failure);
                    if event_after_failure {
                        yield StreamEvent::Frame(StreamFrame::TextDelta {
                            index: 0,
                            text: "after failure".into(),
                        });
                    }
                }
            } else if silent {
                match silent_is_success {
                    true => {
                        yield StreamEvent::Frame(StreamFrame::Usage(usage));
                        yield StreamEvent::Frame(StreamFrame::Finish);
                    }
                    false => {
                        let code = match miscoded_empty {
                            true => ErrorCode::StreamTerminated,
                            false => ErrorCode::EmptyCompletion,
                        };
                        yield StreamEvent::Failed(ProviderFailure {
                            code,
                            message: "no content".into(),
                        });
                    }
                }
            }
        };
        Ok(Box::pin(stream))
    }
}

// ---------------------------------------------------------------------------
// The scenario factory
// ---------------------------------------------------------------------------

/// The scenario factory the suite is driven with: builds the scenario for the
/// case under test from the same corpus the adapter replays.
///
/// A provider's own harness writes exactly this shape against its real golden
/// files.
fn scenario_for(case: &str) -> Scenario {
    scenario_for_defect(case, Defect::None)
}

/// The scenario factory for one defect: the corpus is the same, and the
/// provider-published metadata is whatever that defect's adapter appends.
fn scenario_for_defect(case: &str, defect: Defect) -> Scenario {
    let scenario = Corpus::for_case(case).scenario(case, defect);
    match defect {
        // A hanging provider is bounded by the scenario's watchdog; a fixture
        // test should not wait out the default ten seconds.
        Defect::Hangs => scenario.stall_timeout(Duration::from_millis(200)),
        _ => scenario,
    }
}

thread_local! {
    // The defect the running case is being driven with, so a single `fn`
    // factory can serve both the reference and the negative fixtures.
    static DEFECT: Cell<Defect> = const { Cell::new(Defect::None) };
}

/// The factory the negative fixtures drive: reads the running defect.
fn scenario_for_negative(case: &str) -> Scenario {
    scenario_for_defect(case, DEFECT.with(|d| d.get()))
}

/// A factory that also records which case is running, so a single adapter
/// instance can serve the whole suite (each case's corpus differs).
fn scenario_for_tracking(case: &str) -> Scenario {
    CURRENT.with(|current| *current.borrow_mut() = Corpus::for_case(case));
    DEFECT.with(|d| d.set(Defect::None));
    scenario_for(case)
}

thread_local! {
    /// The corpus the running case drives, for the whole-suite adapter.
    static CURRENT: RefCell<Corpus> = RefCell::new(Corpus::text_turn());
}

/// The whole-suite reference adapter: answers whichever case is running with
/// that case's corpus, honoring every obligation.
struct Reference;

impl ModelAdapter for Reference {
    fn provider(&self) -> &str {
        "reference"
    }
    fn owns(&self, replay_state: &ReplayState) -> bool {
        Scripted::owns_state(replay_state, false)
    }
    fn stream(
        &self,
        _call_id: harnless_seams::CallId,
        _messages: &[Message],
        _tools: &[ToolSchema],
        _replay: Option<ReplayState>,
    ) -> Result<BoxStream> {
        let corpus = CURRENT.with(|current| current.borrow().clone());
        let frames = corpus.frames.clone();
        let failure = corpus.failure.clone();
        let silent = corpus.silent;
        let stream = async_stream::stream! {
            for frame in frames {
                yield StreamEvent::Frame(frame);
            }
            if let Some(failure) = failure {
                yield StreamEvent::Failed(failure);
            } else if silent {
                yield StreamEvent::Failed(ProviderFailure {
                    code: ErrorCode::EmptyCompletion,
                    message: "no content".into(),
                });
            }
        };
        Ok(Box::pin(stream))
    }
}

/// The alignment case's reference adapter: like [`Reference`], but publishing
/// metadata the way the seam's contract says it was appended.
///
/// The suite aligns metadata itself, so the reference's job is to emit the
/// interleaved frames honestly and hand over emission-ordered entries — which
/// is what `Scenario::block_metadata` carries.
#[cfg(test)]
mod suite_passes_reference {
    use super::*;

    #[test]
    fn every_case_passes_individually() {
        for case in ADAPTER_CONFORMANCE_CASES {
            CURRENT.with(|current| *current.borrow_mut() = Corpus::for_case(case));
            let violations = check_model_adapter_contract(&Reference, case, scenario_for);
            assert!(
                violations.is_empty(),
                "reference adapter violated `{case}`:\n{}",
                violations
                    .iter()
                    .map(|v| format!("  - {v}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    #[test]
    fn full_suite_is_clean() {
        let violations = check_model_adapter_contract_all(&Reference, scenario_for_tracking);
        assert!(
            violations.is_empty(),
            "reference adapter violated the full suite:\n{}",
            violations
                .iter()
                .map(|v| format!("  - {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn unknown_case_is_reported() {
        let violations = check_model_adapter_contract(&Reference, "not-a-real-case", scenario_for);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].case, "not-a-real-case");
    }

}

// ---------------------------------------------------------------------------
// Negative tests: every case bites the provider that breaks it
// ---------------------------------------------------------------------------

#[cfg(test)]
mod negative {
    use super::*;

    /// Drive one case against a defective adapter replaying that case's corpus.
    fn bites(defect: Defect, case: &str) -> Vec<harnless_conformance::types::Violation> {
        CURRENT.with(|current| *current.borrow_mut() = Corpus::for_case(case));
        DEFECT.with(|d| d.set(defect));
        let adapter = Scripted::new(defect, case);
        check_model_adapter_contract(&adapter, case, scenario_for_negative)
    }

    /// Assert a defect is caught by `case`, and attributed to `case`.
    fn assert_bites(defect: Defect, case: &str) {
        let violations = bites(defect, case);
        assert!(
            !violations.is_empty(),
            "defect {defect:?} passed the `{case}` case"
        );
        assert!(
            violations.iter().all(|v| v.case == case),
            "violations from `{case}` were attributed elsewhere: {violations:?}"
        );
    }

    #[test]
    fn bites_usage_after_finish() {
        assert_bites(Defect::UsageAfterFinish, "usage_before_finish");
    }

    #[test]
    fn bites_frame_after_finish() {
        assert_bites(Defect::FrameAfterFinish, "nothing_after_finish");
    }

    #[test]
    fn bites_reserialised_arguments() {
        assert_bites(Defect::ReserialisesArgs, "raw_json_tool_arguments");
    }

    #[test]
    fn bites_miscoded_failure() {
        assert_bites(Defect::MiscodedFailure, "sanctioned_failure_paths");
    }

    #[test]
    fn bites_dropped_failure() {
        assert_bites(Defect::DropsFailure, "sanctioned_failure_paths");
    }

    #[test]
    fn bites_event_after_failure() {
        assert_bites(Defect::EventAfterFailure, "in_band_failure_is_terminal");
    }

    #[test]
    fn bites_silent_completion_as_success() {
        assert_bites(Defect::SilentIsSuccess, "empty_completion_is_retryable_failure");
    }

    #[test]
    fn bites_miscoded_empty_completion() {
        assert_bites(Defect::MiscodedEmpty, "empty_completion_is_retryable_failure");
    }

    #[test]
    fn bites_miscoded_context_overflow() {
        assert_bites(Defect::MiscodedOverflow, "context_overflow_canonical_code");
    }

    #[test]
    fn bites_double_counted_input() {
        assert_bites(Defect::DoubleCountsInput, "disjoint_usage");
    }

    #[test]
    fn bites_readded_reasoning() {
        assert_bites(Defect::ReAddsReasoning, "disjoint_usage");
    }

    #[test]
    fn bites_claiming_every_replay_state() {
        assert_bites(Defect::ClaimsEverything, "replay_state_ownership");
    }

    #[test]
    fn bites_index_order_alignment() {
        assert_bites(Defect::IndexOrderAlignment, "replay_alignment_is_emission_order");
    }

    #[test]
    fn a_provider_that_throws_is_skipped_not_failed() {
        // Throwing from `stream()` is a sanctioned path, so the stream-shape
        // cases skip. The failure-path cases still check the thrown code.
        for case in [
            "usage_before_finish",
            "nothing_after_finish",
            "raw_json_tool_arguments",
            "in_band_failure_is_terminal",
            "disjoint_usage",
        ] {
            let violations = bites(Defect::Throws, case);
            assert!(
                violations.is_empty(),
                "a throwing provider was failed by `{case}`: {violations:?}"
            );
        }
    }

    #[test]
    fn every_defect_produces_no_panic() {
        // The suite's floor: no fixture — not even the panicking or hanging
        // ones — crashes the run. `catch_unwind` here is the assertion: if the
        // suite let a provider panic through, this test panics too.
        let defects = [
            Defect::None,
            Defect::UsageAfterFinish,
            Defect::FrameAfterFinish,
            Defect::ReserialisesArgs,
            Defect::MiscodedFailure,
            Defect::DropsFailure,
            Defect::EventAfterFailure,
            Defect::SilentIsSuccess,
            Defect::MiscodedEmpty,
            Defect::MiscodedOverflow,
            Defect::DoubleCountsInput,
            Defect::ReAddsReasoning,
            Defect::ClaimsEverything,
            Defect::IndexOrderAlignment,
            Defect::Throws,
            Defect::Panics,
            Defect::Hangs,
        ];
        for case in ADAPTER_CONFORMANCE_CASES {
            for defect in defects {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    CURRENT.with(|current| *current.borrow_mut() = Corpus::for_case(case));
                    DEFECT.with(|d| d.set(defect));
                    let adapter = Scripted::new(defect, case);
                    check_model_adapter_contract(&adapter, case, scenario_for_negative)
                }));
                assert!(
                    outcome.is_ok(),
                    "defect {defect:?} crashed the suite on case `{case}`"
                );
            }
        }
    }

    #[test]
    fn a_hanging_provider_is_reported_not_waited_out() {
        // The stall bound must produce a verdict quickly rather than hanging
        // the run for the provider's whole (infinite) stream.
        let started = std::time::Instant::now();
        let violations = bites(Defect::Hangs, "nothing_after_finish");
        let elapsed = started.elapsed();
        // The scenario's own bound is 10s; the point of the pin is that the
        // run ends at the bound instead of hanging on an endless stream.
        assert!(
            elapsed < Duration::from_secs(5),
            "the stall bound was not honoured: {elapsed:?}"
        );
        assert!(
            !violations.is_empty(),
            "a stream that never terminated passed the terminal-frame case"
        );
    }

    #[test]
    fn a_panicking_provider_is_a_violation_not_a_crash() {
        let violations = bites(Defect::Panics, "usage_before_finish");
        assert!(
            violations
                .iter()
                .any(|v| v.detail.contains("panicked")),
            "panicking provider was not reported: {violations:?}"
        );
    }

    #[test]
    fn the_reference_passes_every_case_the_defects_fail() {
        // Guards against a case that bites everything: the reference must pass
        // each case some defect fails, or the case is simply broken.
        let pairs = [
            (Defect::UsageAfterFinish, "usage_before_finish"),
            (Defect::FrameAfterFinish, "nothing_after_finish"),
            (Defect::ReserialisesArgs, "raw_json_tool_arguments"),
            (Defect::MiscodedFailure, "sanctioned_failure_paths"),
            (Defect::EventAfterFailure, "in_band_failure_is_terminal"),
            (
                Defect::SilentIsSuccess,
                "empty_completion_is_retryable_failure",
            ),
            (Defect::MiscodedOverflow, "context_overflow_canonical_code"),
            (Defect::DoubleCountsInput, "disjoint_usage"),
            (Defect::ClaimsEverything, "replay_state_ownership"),
            (
                Defect::IndexOrderAlignment,
                "replay_alignment_is_emission_order",
            ),
        ];
        for (_, case) in pairs {
            CURRENT.with(|current| *current.borrow_mut() = Corpus::for_case(case));
            let violations = check_model_adapter_contract(&Reference, case, scenario_for);
            assert!(
                violations.is_empty(),
                "reference adapter failed `{case}` that a defect is supposed to fail: \
                 {violations:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The instantiation macro, run against the reference
// ---------------------------------------------------------------------------

/// A `Send + Sync + 'static` adapter the instantiation macro can build from a
/// plain `fn` item; delegates to [`Reference`].
struct MacroAdapter;

impl ModelAdapter for MacroAdapter {
    fn provider(&self) -> &str {
        "reference"
    }
    fn owns(&self, replay_state: &ReplayState) -> bool {
        Scripted::owns_state(replay_state, false)
    }
    fn stream(
        &self,
        call_id: harnless_seams::CallId,
        messages: &[Message],
        tools: &[ToolSchema],
        replay: Option<ReplayState>,
    ) -> Result<BoxStream> {
        Reference.stream(call_id, messages, tools, replay)
    }
}

fn make_reference() -> MacroAdapter {
    MacroAdapter
}

harnless_conformance::conformance_tests_adapter! {
    reference,
    make_reference,
    scenario_for_tracking,
    "usage_before_finish",
    "nothing_after_finish",
    "raw_json_tool_arguments",
    "sanctioned_failure_paths",
    "in_band_failure_is_terminal",
    "empty_completion_is_retryable_failure",
    "context_overflow_canonical_code",
    "disjoint_usage",
    "replay_state_ownership",
    "replay_alignment_is_emission_order",
}
