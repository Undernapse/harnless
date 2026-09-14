//! The model-adapter conformance suite.
//!
//! [`check_model_adapter_contract_all`] runs every case in
//! [`ADAPTER_CONFORMANCE_CASES`] against a provider;
//! [`check_model_adapter_contract`] runs one case by name (what the
//! [`conformance_tests_adapter`](crate::conformance_tests_adapter) macro
//! expands to).
//!
//! # Why the caller supplies the scenario
//!
//! Every obligation in the adapter contract is about a *stream*, and a
//! stream only exists once something is asked of the provider. The suite
//! therefore owns the assertions and the caller owns the corpus: a
//! [`Scenario`] says which messages, tools and replay state to hand the
//! adapter and which frames the adapter must hand back. The suite never
//! hard-codes one adapter family's fixture, which is what lets a replay
//! adapter, a live HTTP adapter and a test double all be checked by the
//! same code.
//!
//! Comparison is deliberately **frame-for-frame identity**, not
//! "compatible": a replaying adapter is expected to reproduce its corpus
//! exactly, and a live adapter pointed at a scripted corpus owes the same.
//! A provider that legitimately cannot reproduce a scenario's frames
//! byte-for-byte declares it through [`Scenario::tolerate_frame_drift`],
//! and the suite downgrades the identity assertions to the structural
//! invariants that still hold (each case's docs say what survives).
//!
//! # Skip, not violation
//!
//! Following the filesystem suite: a case whose scenario the provider
//! *rejects at the stream entry* is skipped rather than failed. Throwing
//! from `stream()` is a sanctioned failure path, so a provider that
//! refuses a scenario (a live adapter with no network, a replay adapter
//! whose script has no such turn) owes nothing on that case's assertions.
//! Skips are reported under `HARNLESS_CONFORMANCE_DEBUG` exactly as the
//! filesystem suite reports its own.
//!
//! # Why driving is synchronous
//!
//! The suite drives the stream from plain synchronous code with a
//! per-event watchdog bound ([`Scenario::stall_timeout`]) instead of
//! assuming an async runtime: a conformance kit that needs tokio is a kit
//! half its providers cannot instantiate. The stream is pulled on a
//! dedicated drive thread and its events arrive over a channel, so the
//! suite thread can bound each wait and report a stall rather than hang.
//! A panic anywhere in the provider — at the stream entry or mid-poll — is
//! caught and reported as a violation, never propagated.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use futures_lite::prelude::*;
use harnless_seams::error::ErrorCode;
use harnless_seams::llm::{
    BlockAssembler, BlockKind, BoxStream, Message, ModelAdapter, ProviderFailure, ReplayState,
    StreamEvent, StreamFrame, ToolSchema, Usage,
};
use serde_json::Value;

use crate::types::Violation;

/// The response-metadata key the ownership cases stamp replay state with.
///
/// Replay state is adapter-private, so the suite cannot know a provider's
/// real marker. The ownership cases instead build state the *harness*
/// declares claimable: [`Scenario::with_owned_replay`] and
/// [`Scenario::foreign_replay_state`] each carry a key/value pair, and the
/// case asserts only the relationship — the adapter must claim the state
/// the harness says is its own and refuse the state the harness says
/// belongs to someone else. Which literal marker means "mine" stays the
/// provider's business, exactly as on the live seam.
const OWNED_MARKER_KEY: &str = "harnless_conformance_owned";

/// Every adapter conformance case, in suite order.
pub const ADAPTER_CONFORMANCE_CASES: &[&str] = &[
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
];

/// The awkward tool-argument payload the raw-JSON case scripts.
///
/// Keys out of alphabetical order, nested arrays, unicode, escaped quotes,
/// an integer past `f64` precision, a negative zero and an empty object —
/// every one of which a parse-then-reserialize round-trip changes. The
/// suite's own tests assert this string does *not* survive a `serde_json`
/// round-trip, so the case cannot quietly go vacuous.
pub const RAW_TOOL_ARGS: &str = r#"{"z":1,"a":{"deep":[3,1,2]},"名":"漢字","q":"say \"hi\"","big":123456789012345678901234567890,"neg":-0.0,"empty":{},"arr":[[1],[2]]}"#;

/// The marker key a harness declares as the adapter's own replay owner.
pub const OWNED_STATE_KEY: &str = "harnless-conformance-self";
/// The value paired with [`OWNED_STATE_KEY`].
pub const OWNED_STATE_VALUE: &str = "adapter-under-test";
/// The marker key a harness declares as another provider's replay owner.
pub const FOREIGN_STATE_KEY: &str = "harnless-conformance-other";
/// The value paired with [`FOREIGN_STATE_KEY`].
pub const FOREIGN_STATE_VALUE: &str = "someone-else";

/// One scripted turn: what to ask the adapter for, and what it must give
/// back.
///
/// Construct with [`Scenario::new`] plus the builder methods; every field
/// is public so a provider's harness can build *and inspect* a scenario
/// without going through the builders.
#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    /// The conversation handed to `stream`.
    pub messages: Vec<Message>,
    /// The request-time tool set handed to `stream`.
    pub tools: Vec<ToolSchema>,
    /// The replay state handed to `stream`, if any.
    pub replay: Option<ReplayState>,
    /// The frames the adapter must emit, in order.
    pub expected: Vec<StreamFrame>,
    /// The response-level metadata the adapter should stamp its reply with.
    pub response_metadata: Option<Value>,
    /// The per-block metadata entries the scenario scripts, in emission order.
    pub block_metadata: Vec<Value>,
    /// Whether [`Scenario::block_metadata`] is what the *provider* publishes
    /// for this scenario (see [`Scenario::published_metadata`]).
    pub provider_publishes_metadata: bool,
    /// The terminal failure this scenario scripts instead of a `Finish`.
    pub failure: Option<ProviderFailure>,
    /// Whether frame-identity assertions may degrade for this provider
    /// (see [`Scenario::tolerate_frame_drift`]).
    pub frame_drift_tolerated: bool,
    /// How long the suite waits for the provider's next event.
    ///
    /// A provider that neither yields a terminal event nor ends inside this
    /// window is reported as a violation — "stalls bounded by a transport
    /// watchdog" is the one contract obligation a conformance run can only
    /// observe as a timeout.
    pub stall_timeout: Duration,
}

impl Scenario {
    /// A scenario asking for `messages`, with no tools and no replay state.
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            replay: None,
            expected: Vec::new(),
            response_metadata: None,
            block_metadata: Vec::new(),
            provider_publishes_metadata: false,
            failure: None,
            frame_drift_tolerated: false,
            stall_timeout: Duration::from_secs(10),
        }
    }

    /// Set the request-time tool set.
    #[must_use]
    pub fn tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.tools = tools;
        self
    }

    /// Set the replay state handed to `stream`.
    #[must_use]
    pub fn replay(mut self, replay: Option<ReplayState>) -> Self {
        self.replay = replay;
        self
    }

    /// Set the frames the adapter must emit, in order.
    #[must_use]
    pub fn expected(mut self, expected: Vec<StreamFrame>) -> Self {
        self.expected = expected;
        self
    }

    /// Set the response-level metadata the adapter should carry on its
    /// reply — the shape replay state is stamped with.
    #[must_use]
    pub fn response_metadata(mut self, response: Value) -> Self {
        self.response_metadata = Some(response);
        self
    }

    /// Set the per-block metadata entries, in emission order.
    ///
    /// These are the entries the *scenario* expects to end up aligned with
    /// the emitted blocks. For the alignment case the suite needs to know
    /// what the **provider** appends, which is not necessarily the same list
    /// — see [`Scenario::published_metadata`].
    #[must_use]
    pub fn block_metadata(mut self, blocks: Vec<Value>) -> Self {
        self.block_metadata = blocks;
        self
    }

    /// Declare that [`Scenario::block_metadata`] is the metadata this provider
    /// itself appends for this scenario, in the order it appends it.
    ///
    /// Replay metadata is provider-produced: the suite cannot invent it. The
    /// alignment case ([`ADAPTER_CONFORMANCE_CASES`]'s
    /// `replay_alignment_is_emission_order`) therefore aligns the entries the
    /// provider publishes and checks the pairing. A harness that cannot say
    /// what its provider appends leaves this unset, and the case falls back to
    /// the scenario's own emission-ordered entries — which a misaligning
    /// provider then passes, so a provider harness that wants the pin to bite
    /// must declare this.
    #[must_use]
    pub fn published_metadata(mut self, blocks: Vec<Value>) -> Self {
        self.block_metadata = blocks;
        self.provider_publishes_metadata = true;
        self
    }

    /// Script this scenario as a *failed* stream ending in `failure`
    /// instead of a `Finish`.
    #[must_use]
    pub fn failure(mut self, failure: ProviderFailure) -> Self {
        self.failure = Some(failure);
        self
    }

    /// Declare that this provider may answer with frames equivalent to, but
    /// not byte-identical to, [`Scenario::expected`].
    ///
    /// Frame-identity assertions then degrade to structural invariants
    /// (ordering, terminal shape, raw-JSON payload preservation). Reserve
    /// this for providers that genuinely re-frame — a replay adapter should
    /// never need it.
    #[must_use]
    pub fn tolerate_frame_drift(mut self) -> Self {
        self.frame_drift_tolerated = true;
        self
    }

    /// Override the per-event stall bound.
    #[must_use]
    pub fn stall_timeout(mut self, stall_timeout: Duration) -> Self {
        self.stall_timeout = stall_timeout;
        self
    }

    /// Response metadata declaring replay state to be this adapter's own.
    #[must_use]
    pub fn owned_response_metadata() -> Value {
        marker_metadata(OWNED_STATE_KEY, OWNED_STATE_VALUE)
    }

    /// Response metadata declaring replay state to be another provider's.
    #[must_use]
    pub fn foreign_response_metadata() -> Value {
        marker_metadata(FOREIGN_STATE_KEY, FOREIGN_STATE_VALUE)
    }

    /// Set [`Scenario::replay`] to the state this suite's ownership case
    /// declares to be the adapter's own.
    #[must_use]
    pub fn with_owned_replay(mut self) -> Self {
        self.replay = Some(Self::owned_replay_state());
        self
    }

    /// The state [`Scenario::with_owned_replay`] installs, exposed so a
    /// harness can build the same value for an `owns` assertion.
    #[must_use]
    pub fn owned_replay_state() -> ReplayState {
        ReplayState {
            response: Some(Self::owned_response_metadata()),
            blocks: Vec::new(),
        }
    }

    /// The foreign-owner state the ownership case asserts against.
    #[must_use]
    pub fn foreign_replay_state() -> ReplayState {
        ReplayState {
            response: Some(Self::foreign_response_metadata()),
            blocks: Vec::new(),
        }
    }
}

/// The marker document the ownership cases key on.
fn marker_metadata(key: &str, value: &str) -> Value {
    serde_json::json!({ OWNED_MARKER_KEY: { "key": key, "value": value } })
}

/// A provider's scenario library: builds the scripted turn one case drives.
///
/// A plain `fn` item is `Send + Sync + 'static`, so the suite entry points
/// take a function pointer. Each case asks for its own scenario, which
/// keeps the suite independent of any one adapter's corpus format.
pub type ScenarioFactory = fn(&str) -> Scenario;

/// Run the whole adapter suite against `adapter` with `scenario_for` as the
/// corpus.
///
/// A provider is conformant when the returned list is empty.
pub fn check_model_adapter_contract_all(
    adapter: &dyn ModelAdapter,
    scenario_for: ScenarioFactory,
) -> Vec<Violation> {
    let mut out = Vec::new();
    for case in ADAPTER_CONFORMANCE_CASES {
        check_case_into(adapter, scenario_for, case, &mut out);
    }
    out
}

/// Run one adapter conformance case by name against `adapter`.
///
/// Unknown case names yield a single violation naming the unknown case.
pub fn check_model_adapter_contract(
    adapter: &dyn ModelAdapter,
    case: &str,
    scenario_for: ScenarioFactory,
) -> Vec<Violation> {
    let mut out = Vec::new();
    check_case_into(adapter, scenario_for, case, &mut out);
    out
}

fn check_case_into(
    adapter: &dyn ModelAdapter,
    scenario_for: ScenarioFactory,
    case: &str,
    out: &mut Vec<Violation>,
) {
    if !ADAPTER_CONFORMANCE_CASES.contains(&case) {
        out.push(Violation::new(case.to_string(), "unknown conformance case"));
        return;
    }
    // The scenario is the harness's own fixture. A factory that panics is
    // the provider misbehaving on the suite's input, and a panic is a
    // contract violation, never a crash.
    let scenario = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (scenario_for)(case)
    })) {
        Ok(scenario) => scenario,
        Err(_) => {
            out.push(Violation::new(
                case.to_string(),
                "the scenario factory panicked while building this case's fixture",
            ));
            return;
        }
    };
    let mut cx = Cx::default();
    match case {
        "usage_before_finish" => usage_before_finish(adapter, &scenario, &mut cx),
        "nothing_after_finish" => nothing_after_finish(adapter, &scenario, &mut cx),
        "raw_json_tool_arguments" => raw_json_tool_arguments(adapter, &scenario, &mut cx),
        "sanctioned_failure_paths" => sanctioned_failure_paths(adapter, &scenario, &mut cx),
        "in_band_failure_is_terminal" => in_band_failure_is_terminal(adapter, &scenario, &mut cx),
        "empty_completion_is_retryable_failure" => {
            empty_completion_is_retryable_failure(adapter, &scenario, &mut cx)
        }
        "context_overflow_canonical_code" => {
            context_overflow_canonical_code(adapter, &scenario, &mut cx)
        }
        "disjoint_usage" => disjoint_usage(adapter, &scenario, &mut cx),
        "replay_state_ownership" => replay_state_ownership(adapter, &mut cx),
        "replay_alignment_is_emission_order" => {
            replay_alignment_is_emission_order(adapter, &scenario, &mut cx)
        }
        _ => unreachable!("case name checked above"),
    }
    out.extend(cx.into_violations(case));
}

/// Per-case outcome collector.
#[derive(Default)]
struct Cx {
    violations: Vec<Violation>,
    skips: Vec<String>,
}

impl Cx {
    fn fail(&mut self, detail: impl Into<String>) {
        self.violations.push(Violation::new(String::new(), detail));
    }

    /// The provider cannot produce this scenario; the dependent assertions
    /// are owed nothing. A skip is not a violation.
    fn skip(&mut self, why: impl Into<String>) {
        self.skips.push(why.into());
    }

    fn into_violations(self, case: &str) -> Vec<Violation> {
        if std::env::var_os("HARNLESS_CONFORMANCE_DEBUG").is_some() && !self.skips.is_empty() {
            eprintln!("[conformance skip] {case}: {}", self.skips.join("; "));
        }
        self.violations
            .into_iter()
            .map(|mut v| {
                v.case = case.to_string();
                v
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Driving the seam
// ---------------------------------------------------------------------------

/// Everything the suite observed from one `stream` call, normalised across
/// the two sanctioned failure paths.
#[derive(Debug)]
struct Collected {
    /// Frames emitted before any terminal event.
    frames: Vec<StreamFrame>,
    /// The in-band terminal failure, when the stream ended in one.
    failure: Option<ProviderFailure>,
    /// Events observed after the first terminal event — the "nothing after
    /// finish" evidence.
    after_terminal: Vec<String>,
    /// The provider threw from `stream()` (the first sanctioned path).
    threw: Option<ErrorCode>,
    /// The stream neither yielded a terminal event nor ended in time.
    stalled: bool,
    /// The provider panicked at the stream entry or mid-poll.
    panicked: bool,
}

impl Collected {
    fn empty() -> Self {
        Self {
            frames: Vec::new(),
            failure: None,
            after_terminal: Vec::new(),
            threw: None,
            stalled: false,
            panicked: false,
        }
    }

    /// How the stream ended, phrased for a violation message.
    fn ending(&self) -> String {
        if self.panicked {
            "panicked while producing the stream".to_string()
        } else if let Some(code) = self.threw {
            format!("threw from `stream()` with `{}`", code.as_str())
        } else if let Some(failure) = &self.failure {
            format!("ended in-band with `{}`", failure.code.as_str())
        } else if self.frames.contains(&StreamFrame::Finish) {
            "ended with a `Finish` frame".to_string()
        } else if self.stalled {
            "stalled: no terminal event before the watchdog bound".to_string()
        } else {
            "ended with no terminal event".to_string()
        }
    }

    /// Whether the provider refused the scenario outright — a skip, not a
    /// finding.
    fn refused(&self, cx: &mut Cx) -> bool {
        match self.threw {
            Some(code) => {
                cx.skip(format!(
                    "provider refused the scenario at `stream()` (`{}`)",
                    code.as_str()
                ));
                true
            }
            None => false,
        }
    }

    /// A provider that panicked has violated the "surface as a violation,
    /// never a crash" floor; record it once and stop interpreting the
    /// (absent) stream.
    fn panic_note(&self, cx: &mut Cx, label: &str) -> bool {
        if self.panicked {
            cx.fail(format!(
                "{label}: the provider panicked on this scenario; a misbehaving input \
                 must surface as a typed failure, never a crash"
            ));
            true
        } else {
            false
        }
    }
}

/// Describe an event for the "nothing after finish" report.
fn describe(event: &StreamEvent) -> String {
    match event {
        StreamEvent::Frame(frame) => match frame {
            StreamFrame::BlockStart { index, kind } => {
                format!("Frame(BlockStart {{ index: {index}, kind: {:?} }})", kind)
            }
            StreamFrame::TextDelta { index, text } => {
                format!("Frame(TextDelta {{ index: {index}, text: {text:?} }})")
            }
            StreamFrame::ReasoningDelta { index, text } => {
                format!("Frame(ReasoningDelta {{ index: {index}, text: {text:?} }})")
            }
            StreamFrame::ToolCallDelta {
                index,
                call_id,
                json,
            } => format!(
                "Frame(ToolCallDelta {{ index: {index}, call_id: {:?}, json: {:?} }})",
                call_id.0, json
            ),
            StreamFrame::BlockEnd { index, assembled } => format!(
                "Frame(BlockEnd {{ index: {index}, kind: {:?} }})",
                assembled.kind
            ),
            StreamFrame::Usage(usage) => format!("Frame(Usage({usage:?}))"),
            StreamFrame::Finish => "Frame(Finish)".to_string(),
        },
        StreamEvent::Failed(failure) => format!("Failed({})", failure.code.as_str()),
    }
}

/// Drive `adapter` through `scenario`, observing only the seam.
///
/// Never propagates a panic: a provider that throws from `stream()` is
/// recorded as a throw, one that panics is recorded as a panic, and one
/// that hangs is recorded as a stall.
fn drive(adapter: &dyn ModelAdapter, scenario: &Scenario) -> Collected {
    let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        adapter.stream(
            harnless_seams::CallId(1),
            &scenario.messages,
            &scenario.tools,
            scenario.replay.clone(),
        )
    }));
    let stream = match started {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => {
            let mut collected = Collected::empty();
            collected.threw = Some(err.code);
            return collected;
        }
        Err(_) => {
            let mut collected = Collected::empty();
            collected.panicked = true;
            return collected;
        }
    };
    collect_stream(stream, scenario.stall_timeout)
}

/// One step of the drive thread's report: an event, or the reason it
/// stopped polling.
enum Step {
    Event(StreamEvent),
    End,
    Panicked,
}

/// Poll a stream on a dedicated thread and bound every wait.
///
/// The stream is `Send` but not `Unpin`, and the caller is synchronous, so the
/// drive thread owns it and ships events over a channel; the suite thread
/// never touches the pinned stream and needs no runtime, no lock and no
/// `unsafe`.
///
/// The bound runs in both directions. On a stall the suite reports it *and*
/// flips the shared stop flag, so the drive thread stops polling at its next
/// await point instead of polling a provider's endless stream for the rest of
/// the process's life. A provider whose `poll` never returns cannot be
/// interrupted by any caller — that is the execution-world suite's
/// cancellation case, not a promise a stream consumer can make.
fn collect_stream(stream: BoxStream, stall_timeout: Duration) -> Collected {
    let (sender, receiver) = mpsc::channel::<Step>();
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let polled = std::sync::Arc::clone(&stop);
    let worker = std::thread::Builder::new()
        .name("harnless-conformance-drive".to_string())
        .spawn(move || {
            let pulled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                futures_lite::future::block_on(pump(stream, sender.clone(), polled))
            }));
            if pulled.is_err() {
                let _ = sender.send(Step::Panicked);
            }
        });
    if worker.is_err() {
        // No thread to drive on: report the stall honestly rather than
        // pretending the provider answered.
        let mut collected = Collected::empty();
        collected.stalled = true;
        return collected;
    }

    let mut collected = Collected::empty();
    let mut terminal_seen = false;
    loop {
        // Past the terminal event the provider owes nothing but may not
        // speak again; a conforming stream ends immediately.
        let bound = if terminal_seen {
            Duration::from_secs(2)
        } else {
            stall_timeout
        };
        match receiver.recv_timeout(bound) {
            Ok(Step::Event(StreamEvent::Frame(frame))) if !terminal_seen => {
                if frame == StreamFrame::Finish {
                    terminal_seen = true;
                }
                collected.frames.push(frame);
            }
            Ok(Step::Event(StreamEvent::Failed(cause))) if !terminal_seen => {
                terminal_seen = true;
                collected.failure = Some(cause);
            }
            // Past the terminal event the provider may not speak again; keep
            // whatever it did say as the "nothing after finish" evidence.
            Ok(Step::Event(event)) => collected.after_terminal.push(describe(&event)),
            Ok(Step::End) => break,
            Ok(Step::Panicked) => {
                collected.panicked = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                collected.stalled = true;
                // Release the drive thread: it checks the flag at its next
                // await point and stops polling.
                stop.store(true, Ordering::SeqCst);
                break;
            }
            // The drive thread dropped its sender without saying `End`,
            // which only happens if its own reporting failed.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                collected.panicked = true;
                break;
            }
        }
    }
    collected
}

/// Forward every event of `stream` to the suite thread.
async fn pump(mut stream: BoxStream, sender: mpsc::Sender<Step>, stop: std::sync::Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // Race the provider's next event against the stop flag so a stream
        // that awaits forever still observes the bound.
        let polled = futures_lite::future::or(
            async { stream.next().await.map(Step::Event) },
            async {
                while !stop.load(Ordering::SeqCst) {
                    futures_lite::future::yield_now().await;
                }
                Some(Step::End)
            },
        )
        .await;
        match polled {
            Some(step) => {
                let terminal = matches!(step, Step::End);
                if sender.send(step).is_err() || terminal {
                    return;
                }
            }
            // The stream ended on its own.
            None => break,
        }
    }
    let _ = sender.send(Step::End);
}

/// The frames a scenario asks for.
fn expected_frames(scenario: &Scenario) -> Vec<StreamFrame> {
    scenario.expected.clone()
}

/// Compare observed frames with expected ones, honouring the drift opt-out.
fn frame_identity(
    cx: &mut Cx,
    scenario: &Scenario,
    observed: &[StreamFrame],
    expected: &[StreamFrame],
    label: &str,
) -> bool {
    if scenario.frame_drift_tolerated {
        cx.skip(format!(
            "{label}: provider declared frame-drift tolerance; frame-identity assertions \
             reduced to structural invariants"
        ));
        return true;
    }
    if observed == expected {
        return true;
    }
    cx.fail(format!(
        "{label}: provider emitted {} frame(s), scenario scripted {}.\n  expected: \
         {}\n  observed: {}",
        observed.len(),
        expected.len(),
        summarize(expected),
        summarize(observed),
    ));
    false
}

/// A compact one-line frame summary for violation messages.
fn summarize(frames: &[StreamFrame]) -> String {
    if frames.is_empty() {
        return "<none>".to_string();
    }
    frames
        .iter()
        .map(|f| match f {
            StreamFrame::BlockStart { index, kind } => {
                format!("start({index},{})", kind.as_str())
            }
            StreamFrame::TextDelta { index, text } => format!("text({index},{text:?})"),
            StreamFrame::ReasoningDelta { index, text } => format!("reason({index},{text:?})"),
            StreamFrame::ToolCallDelta {
                index,
                call_id,
                json,
            } => format!("tool({index},#{},{json:?})", call_id.0),
            StreamFrame::BlockEnd { index, assembled } => format!(
                "end({index},{},{:?})",
                assembled.kind.as_str(),
                assembled.text
            ),
            StreamFrame::Usage(_) => "usage".to_string(),
            StreamFrame::Finish => "finish".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `Usage` a stream reported, with the "reported exactly once" check.
fn single_usage(cx: &mut Cx, collected: &Collected, label: &str) -> Option<Usage> {
    let reported: Vec<Usage> = collected
        .frames
        .iter()
        .filter_map(|f| match f {
            StreamFrame::Usage(usage) => Some(*usage),
            _ => None,
        })
        .collect();
    match reported.as_slice() {
        [] => {
            cx.fail(format!(
                "{label}: stream ended {} without reporting `Usage`",
                collected.ending()
            ));
            None
        }
        [one] => Some(*one),
        many => {
            cx.fail(format!(
                "{label}: stream reported `Usage` {} times; the accounting frame is \
                 emitted once per attempt",
                many.len()
            ));
            None
        }
    }
}

/// The `Usage` a scenario scripts, when it scripts one.
fn scripted_usage(scenario: &Scenario) -> Option<Usage> {
    expected_frames(scenario).iter().find_map(|f| match f {
        StreamFrame::Usage(usage) => Some(*usage),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Stream protocol
// ---------------------------------------------------------------------------

/// Usage before finish.
///
/// The `Usage` frame must appear, exactly once, strictly before the
/// terminal `Finish` frame. Split from "nothing after finish" so a provider
/// that violates one half is diagnosably the provider that violates that
/// half.
fn usage_before_finish(adapter: &dyn ModelAdapter, scenario: &Scenario, cx: &mut Cx) {
    let collected = drive(adapter, scenario);
    if collected.panic_note(cx, "usage before finish") || collected.refused(cx) {
        return;
    }
    let finish = collected.frames.iter().position(|f| *f == StreamFrame::Finish);
    let usage = collected
        .frames
        .iter()
        .position(|f| matches!(f, StreamFrame::Usage(_)));
    match (usage, finish) {
        (Some(u), Some(f)) if u < f => {}
        (Some(_), Some(f)) => cx.fail(format!(
            "`Usage` arrived after the terminal `Finish` frame (frame {f} is `Finish`); \
             usage must precede finish"
        )),
        (None, Some(_)) => cx.fail(
            "stream finished without a `Usage` frame; every successful attempt reports its \
             accounting before the terminal frame",
        ),
        (Some(_), None) => cx.fail(format!(
            "stream reported `Usage` but never emitted the terminal `Finish` frame; it \
             ended {}",
            collected.ending()
        )),
        (None, None) => cx.fail(format!(
            "stream ended {} with neither `Usage` nor `Finish`",
            collected.ending()
        )),
    }
    if !collected.after_terminal.is_empty() {
        cx.fail(format!(
            "{} event(s) followed the terminal frame: {}",
            collected.after_terminal.len(),
            collected.after_terminal.join(", ")
        ));
    }
}

/// Nothing after finish.
///
/// The provider must reproduce the scripted stream exactly, which is the
/// strongest observation available for this half of the obligation: a frame
/// appended after `Finish` shows up as a frame-sequence mismatch even when
/// the drive loop's own "events after terminal" capture cannot see it (a
/// provider that ends its stream only after emitting the extra frames).
fn nothing_after_finish(adapter: &dyn ModelAdapter, scenario: &Scenario, cx: &mut Cx) {
    let collected = drive(adapter, scenario);
    if collected.panic_note(cx, "nothing after finish") {
        return;
    }
    if collected.refused(cx) {
        return;
    }
    if !collected.after_terminal.is_empty() {
        cx.fail(format!(
            "{} event(s) followed the terminal frame: {}",
            collected.after_terminal.len(),
            collected.after_terminal.join(", ")
        ));
    }
    if collected.stalled {
        cx.fail(
            "stream never terminated within the watchdog bound; a stalled stream is the \
             failure path the transport watchdog exists to bound",
        );
        return;
    }
    if scenario.failure.is_some() {
        cx.skip("scenario scripts a failed stream; its terminal shape is the failure case's job");
        return;
    }
    let finish = collected.frames.iter().position(|f| *f == StreamFrame::Finish);
    match finish {
        Some(pos) if pos + 1 == collected.frames.len() => {}
        Some(pos) => cx.fail(format!(
            "frame {} follows the terminal `Finish` frame at {pos}; nothing may follow the \
             terminal frame",
            pos + 2
        )),
        None => cx.fail(format!(
            "successful stream ended without a terminal `Finish` frame; it ended {}",
            collected.ending()
        )),
    }
    frame_identity(
        cx,
        scenario,
        &collected.frames,
        &expected_frames(scenario),
        "stream replay",
    );
}

// ---------------------------------------------------------------------------
// Lossless tool arguments
// ---------------------------------------------------------------------------

/// Tool arguments stay raw JSON strings end to end.
///
/// Strongest observation available through the seam: the assembled
/// tool-call payload must be the scripted bytes, the streamed fragments must
/// be the scripted fragments, and the fragments must *concatenate* to the
/// payload. A provider that parses the arguments and re-serialises them
/// breaks at least one of the three even when the final payload happens to
/// survive — re-serialisation reorders keys, loses precision and rewrites
/// `-0.0`.
fn raw_json_tool_arguments(adapter: &dyn ModelAdapter, scenario: &Scenario, cx: &mut Cx) {
    let collected = drive(adapter, scenario);
    if collected.panic_note(cx, "raw tool arguments") || collected.refused(cx) {
        return;
    }
    let assembled: Vec<(usize, &str)> = collected
        .frames
        .iter()
        .filter_map(|f| match f {
            StreamFrame::BlockEnd { index, assembled } if assembled.kind == BlockKind::ToolCall => {
                Some((*index, assembled.text.as_str()))
            }
            _ => None,
        })
        .collect();
    if assembled.is_empty() {
        cx.fail(
            "scenario scripted a tool call and the stream assembled none; the tool-call \
             block is the payload this case exists to check",
        );
        return;
    }
    for (index, text) in &assembled {
        if *text != RAW_TOOL_ARGS {
            cx.fail(format!(
                "assembled tool-call arguments for block {index} are {text:?}, not the \
                 scripted raw JSON {RAW_TOOL_ARGS:?}; the adapter re-serialised the \
                 provider's bytes"
            ));
        }
    }
    let expected_deltas: Vec<String> = expected_frames(scenario)
        .iter()
        .filter_map(|f| match f {
            StreamFrame::ToolCallDelta { json, .. } => Some(json.clone()),
            _ => None,
        })
        .collect();
    if expected_deltas.is_empty() {
        cx.skip("scenario streams no tool-call deltas; only the assembled payload is checkable");
        return;
    }
    let observed_deltas: Vec<String> = collected
        .frames
        .iter()
        .filter_map(|f| match f {
            StreamFrame::ToolCallDelta { json, .. } => Some(json.clone()),
            _ => None,
        })
        .collect();
    if scenario.frame_drift_tolerated {
        cx.skip("frame drift tolerated; delta identity reduced to concatenation");
    } else if observed_deltas != expected_deltas {
        cx.fail(format!(
            "tool-call deltas are {observed_deltas:?}, scripted as {expected_deltas:?}; a \
             provider that buffers and re-emits whole arguments is re-serialising"
        ));
    }
    let concatenated: String = observed_deltas.iter().map(String::as_str).collect();
    if concatenated != RAW_TOOL_ARGS {
        cx.fail(format!(
            "concatenated tool-call deltas are {concatenated:?}, not the scripted raw JSON \
             {RAW_TOOL_ARGS:?}"
        ));
    }
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

/// Exactly two sanctioned failure paths, normalising to one shape.
///
/// The scenario scripts a stream that ends in-band with a
/// [`ProviderFailure`]. A provider that instead throws from `stream()` is
/// still on a sanctioned path and the case accepts it — but the thrown code
/// must be the scripted code, because both paths normalise to the same
/// provider-neutral shape a caller routes on.
fn sanctioned_failure_paths(adapter: &dyn ModelAdapter, scenario: &Scenario, cx: &mut Cx) {
    let Some(scripted) = scenario.failure.clone() else {
        cx.fail("scenario for this case scripts no failure; the harness must script one");
        return;
    };
    let collected = drive(adapter, scenario);
    if collected.panic_note(cx, "failure paths") {
        return;
    }
    if let Some(code) = collected.threw {
        if code != scripted.code {
            cx.fail(format!(
                "`stream()` threw `{}` for a scenario scripted as `{}`; both sanctioned \
                 paths normalise to the same provider-neutral code",
                code.as_str(),
                scripted.code.as_str()
            ));
        }
        return;
    }
    match &collected.failure {
        Some(failure) if failure.code == scripted.code => {}
        Some(failure) => cx.fail(format!(
            "in-band failure carried `{}`, scenario scripted `{}`; the failure shape must \
             survive the adapter boundary unchanged",
            failure.code.as_str(),
            scripted.code.as_str()
        )),
        None => cx.fail(format!(
            "scenario scripted a `{}` failure and the stream ended {} instead; a failing \
             attempt must surface through one of the two sanctioned paths",
            scripted.code.as_str(),
            collected.ending()
        )),
    }
    if !collected.after_terminal.is_empty() {
        cx.fail(format!(
            "{} event(s) followed the terminal failure: {}",
            collected.after_terminal.len(),
            collected.after_terminal.join(", ")
        ));
    }
}

/// An in-band failure is the *terminal* event.
///
/// The scenario scripts a stream that emits content and then fails, so the
/// assertion is specifically that the failure ends the stream: nothing may
/// follow it, a `Finish` may not also appear, and the frames before it must
/// be the scripted ones.
fn in_band_failure_is_terminal(adapter: &dyn ModelAdapter, scenario: &Scenario, cx: &mut Cx) {
    let collected = drive(adapter, scenario);
    if collected.panic_note(cx, "in-band failure") {
        return;
    }
    if collected.refused(cx) {
        return;
    }
    if collected.failure.is_none() {
        cx.fail(format!(
            "scenario scripted a terminal in-band failure and the stream ended {} instead",
            collected.ending()
        ));
        return;
    }
    if !collected.after_terminal.is_empty() {
        cx.fail(format!(
            "in-band `Failed` is not terminal: {} event(s) followed it: {}",
            collected.after_terminal.len(),
            collected.after_terminal.join(", ")
        ));
    }
    if collected.frames.contains(&StreamFrame::Finish) {
        cx.fail(
            "stream emitted both a `Finish` frame and an in-band `Failed` event; a stream \
             ends exactly one way",
        );
    }
    frame_identity(
        cx,
        scenario,
        &collected.frames,
        &expected_frames(scenario),
        "pre-failure frames",
    );
}

/// An empty completion is a retryable failure, never a success.
fn empty_completion_is_retryable_failure(
    adapter: &dyn ModelAdapter,
    scenario: &Scenario,
    cx: &mut Cx,
) {
    let collected = drive(adapter, scenario);
    if collected.panic_note(cx, "empty completion") {
        return;
    }
    if let Some(code) = collected.threw {
        // Throwing is sanctioned, but only with the empty-completion code:
        // a silent provider is retryable, and the caller decides to retry by
        // reading that code.
        if code != ErrorCode::EmptyCompletion {
            cx.fail(format!(
                "silent completion threw `{}`; an empty completion is the retryable \
                 `empty-completion` failure",
                code.as_str()
            ));
        }
        return;
    }
    let content = collected.frames.iter().any(|f| {
        matches!(
            f,
            StreamFrame::BlockStart { .. }
                | StreamFrame::TextDelta { .. }
                | StreamFrame::ReasoningDelta { .. }
                | StreamFrame::ToolCallDelta { .. }
                | StreamFrame::BlockEnd { .. }
        )
    });
    if content {
        cx.fail(
            "scenario scripted a silent completion and the stream emitted content frames; \
             the provider invented output for an empty response",
        );
        return;
    }
    match &collected.failure {
        Some(failure) if failure.code == ErrorCode::EmptyCompletion => {}
        Some(failure) => cx.fail(format!(
            "silent completion failed with `{}`; the contract classifies it as the retryable \
             `empty-completion` failure",
            failure.code.as_str()
        )),
        None => cx.fail(format!(
            "silent completion was reported as a success ({}); an empty completion is a \
             retryable failure, never a result",
            collected.ending()
        )),
    }
    if collected.stalled {
        cx.fail("silent completion stalled instead of failing within the watchdog bound");
    }
}

/// Context overflow classifies to the single canonical code.
fn context_overflow_canonical_code(adapter: &dyn ModelAdapter, scenario: &Scenario, cx: &mut Cx) {
    let collected = drive(adapter, scenario);
    if collected.panic_note(cx, "context overflow") {
        return;
    }
    if let Some(code) = collected.threw {
        if code != ErrorCode::ContextOverflow {
            cx.fail(format!(
                "context overflow threw `{}`; the seam has exactly one canonical code for \
                 it (`{}`)",
                code.as_str(),
                ErrorCode::ContextOverflow.as_str()
            ));
        }
        return;
    }
    match &collected.failure {
        Some(failure) if failure.code == ErrorCode::ContextOverflow => {}
        Some(failure) => cx.fail(format!(
            "context overflow arrived in-band as `{}`; consumers route overflow on the \
             canonical `context-overflow` code and nothing else",
            failure.code.as_str()
        )),
        None => cx.fail(format!(
            "context overflow ended {} instead of failing; an overflow must surface as the \
             canonical failure, never as a truncated success",
            collected.ending()
        )),
    }
    if !collected.after_terminal.is_empty() {
        cx.fail(format!(
            "{} event(s) followed the terminal overflow failure",
            collected.after_terminal.len()
        ));
    }
}

// ---------------------------------------------------------------------------
// Usage accounting
// ---------------------------------------------------------------------------

/// Disjoint usage: `billed_input()` is the disjoint sum and reasoning is
/// never re-added to output.
///
/// The suite cannot read an adapter's arithmetic, so it checks the
/// accounting the adapter *publishes*: it must be the scripted numbers (a
/// re-derived total is the usual way a provider double-counts) and it must
/// be internally consistent under the seam's own definition. Cached reads
/// folded into uncached input, or reasoning added on top of output, both
/// publish a `Usage` whose parts contradict the totals a consumer derives
/// from it.
fn disjoint_usage(adapter: &dyn ModelAdapter, scenario: &Scenario, cx: &mut Cx) {
    let collected = drive(adapter, scenario);
    if collected.panic_note(cx, "disjoint usage") || collected.refused(cx) {
        return;
    }
    let Some(usage) = single_usage(cx, &collected, "usage accounting") else {
        return;
    };
    let expected = scripted_usage(scenario);
    if let Some(expected) = expected {
        if !scenario.frame_drift_tolerated && usage != expected {
            cx.fail(format!(
                "reported usage {usage:?} differs from the scripted accounting {expected:?}; \
                 the adapter must publish the provider's numbers, not re-derived ones"
            ));
        }
    }
    let recomputed = usage.uncached_input + usage.cached_reads + usage.cached_writes;
    if usage.billed_input() != recomputed {
        cx.fail(format!(
            "billed input {} != uncached {} + cached reads {} + cached writes {}; the input \
             parts must be disjoint",
            usage.billed_input(),
            usage.uncached_input,
            usage.cached_reads,
            usage.cached_writes
        ));
    }
    if usage.reasoning > usage.output {
        cx.fail(format!(
            "reported reasoning {} exceeds output {}; reasoning tokens are already inside \
             output and must never be added again",
            usage.reasoning, usage.output
        ));
    }
    // The specific double-count the contract warns about: reasoning folded
    // into output *and* still reported separately shows up as output equal
    // to the honest output plus reasoning. The scripted pair pins the
    // honest numbers, so compare against them.
    if let Some(expected) = expected {
        if usage.output == expected.output + usage.reasoning
            && usage.reasoning > 0
            && usage.output != expected.output
        {
            cx.fail(format!(
                "reported output {} is the honest output {} plus reasoning {} re-added; \
                 reasoning is already inside output",
                usage.output, expected.output, usage.reasoning
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Replay-state ownership
// ---------------------------------------------------------------------------

/// `owns()` claims only state stamped for this adapter.
///
/// The harness declares which marker is the adapter's own
/// ([`Scenario::owned_replay_state`]) and which belongs to someone else
/// ([`Scenario::foreign_replay_state`]); the case asserts the adapter's
/// `owns` agrees with the declaration. A provider whose `owns` ignores the
/// marker — claiming everything — is the failure mode this case exists for.
fn replay_state_ownership(adapter: &dyn ModelAdapter, cx: &mut Cx) {
    let owned = Scenario::owned_replay_state();
    let foreign = Scenario::foreign_replay_state();
    let bare = ReplayState::default();

    // `owns` is the one seam method the suite calls outside a stream, so it
    // gets the same panic guard the stream path has. The guard returns a
    // verdict plus a panic flag rather than writing to `cx`, so the borrow
    // checker never has to reconcile a live closure with the case body.
    let claims = |state: &ReplayState| -> (Option<bool>, bool) {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| adapter.owns(state))) {
            Ok(claims) => (Some(claims), false),
            Err(_) => (None, true),
        }
    };

    let (claims_owned, panicked) = claims(&owned);
    if panicked {
        cx.fail("`owns()` panicked on state the harness declares as this adapter's own");
        return;
    }
    if claims_owned == Some(false) {
        cx.fail(format!(
            "`owns()` refused state the harness declares as this adapter's own (`{}` = \
             `{}`); an adapter that cannot claim its own state silently drops it on the \
             next turn",
            OWNED_STATE_KEY, OWNED_STATE_VALUE
        ));
    }
    let (claims_foreign, panicked) = claims(&foreign);
    if panicked {
        cx.fail("`owns()` panicked on state the harness declares as another provider's");
        return;
    }
    if claims_foreign == Some(true) {
        cx.fail(format!(
            "`owns()` claimed state the harness declares as another provider's (`{}` = \
             `{}`); claiming foreign replay state is how an ownership-gated handoff \
             corrupts a conversation with metadata it cannot interpret",
            FOREIGN_STATE_KEY, FOREIGN_STATE_VALUE
        ));
    }
    let (claims_bare, panicked) = claims(&bare);
    if panicked {
        cx.fail("`owns()` panicked on replay state with no response metadata");
        return;
    }
    if claims_bare == Some(true) {
        cx.fail(
            "`owns()` claimed replay state carrying no response metadata at all; state with \
             no owner marker belongs to nobody",
        );
    }
}

// ---------------------------------------------------------------------------
// Replay alignment (bug #25)
// ---------------------------------------------------------------------------

/// `BlockAssembler::align_replay` pairs metadata by **emission** order.
///
/// Pinned from bug #25: a provider that interleaves blocks (block 1's
/// metadata appended before block 0's) must still get each entry attached
/// to the block it was appended for. Alignment by *index* order mispairs
/// exactly this stream, so the scenario scripts blocks in descending index
/// order and the case checks the pairing the shipped consumer stores.
///
/// The suite folds the provider's own frames through the seam's assembler —
/// the only way to observe alignment across a provider boundary, and the
/// pairing the real consumer keeps.
fn replay_alignment_is_emission_order(
    adapter: &dyn ModelAdapter,
    scenario: &Scenario,
    cx: &mut Cx,
) {
    let collected = drive(adapter, scenario);
    if collected.panic_note(cx, "replay alignment") || collected.refused(cx) {
        return;
    }
    if !frame_identity(
        cx,
        scenario,
        &collected.frames,
        &expected_frames(scenario),
        "interleaved stream",
    ) {
        cx.skip("frame mismatch: alignment assertions would compare the wrong stream");
        return;
    }
    let emission: Vec<(usize, BlockKind)> = collected
        .frames
        .iter()
        .filter_map(|f| match f {
            StreamFrame::BlockEnd { index, assembled } => Some((*index, assembled.kind)),
            _ => None,
        })
        .collect();
    if emission.is_empty() {
        cx.fail("scenario scripted no completed blocks; alignment has nothing to pair");
        return;
    }
    let distinct: BTreeSet<usize> = emission.iter().map(|(index, _)| *index).collect();
    if distinct.len() != emission.len() {
        cx.fail("scenario scripted the same block index more than once");
        return;
    }
    if !emission.windows(2).any(|w| w[0].0 > w[1].0) {
        cx.fail(
            "scenario does not interleave (emission order equals index order), so it cannot \
             distinguish emission-order alignment from index-order alignment; the harness \
             must script a descending-index stream",
        );
        return;
    }
    if !scenario.provider_publishes_metadata {
        cx.skip(
            "scenario did not declare what metadata the provider appends \
             (Scenario::published_metadata); the suite will not align entries it \
             invented and call the result the provider's pairing",
        );
        return;
    }
    if emission.len() != scenario.block_metadata.len() {
        cx.fail(format!(
            "provider emitted {} completed block(s) for a scenario that appended {} \
             metadata entry(ies); alignment cannot describe content that was not emitted",
            emission.len(),
            scenario.block_metadata.len()
        ));
        return;
    }

    let mut assembler = BlockAssembler::new();
    for frame in &collected.frames {
        assembler.push(frame);
    }
    let replay = ReplayState {
        response: scenario.response_metadata.clone(),
        blocks: scenario.block_metadata.clone(),
    };
    let aligned = assembler.align_replay(replay);
    let blocks = assembler.blocks();
    if aligned.blocks.len() != blocks.len() {
        cx.fail(format!(
            "aligned replay state carries {} metadata entries for {} assembled blocks; \
             stored metadata must describe stored content one-for-one",
            aligned.blocks.len(),
            blocks.len()
        ));
        return;
    }
    // Entry `i` was appended for emission slot `i`, so it must describe the
    // block emitted at slot `i` — not the block with index `i`.
    for (slot, entry) in aligned.blocks.iter().enumerate() {
        let (paired_index, _) = emission[slot];
        let Some(described) = entry.get("index").and_then(|v| v.as_u64()) else {
            cx.fail(format!(
                "metadata entry {slot} carries no `index` marker; the scenario must make \
                 the pairing observable"
            ));
            return;
        };
        if described != paired_index as u64 {
            cx.fail(format!(
                "metadata entry {slot} describes block {described} but was aligned to \
                 emitted block {paired_index}; alignment must consume emission order, never \
                 index order"
            ));
        }
    }
    // Stored metadata must survive the keep-or-drop decision too.
    if assembler.truncated() {
        if blocks
            .iter()
            .any(|block| block.kind == BlockKind::ToolCall)
        {
            cx.fail(
                "truncated stream kept tool-call blocks; a partial call is unsafe to \
                 execute",
            );
        }
        if aligned.blocks.len() != blocks.len() {
            cx.fail(
                "truncated stream kept metadata for dropped blocks; stored metadata must \
                 describe stored content",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_list_is_the_documented_set() {
        assert_eq!(
            ADAPTER_CONFORMANCE_CASES,
            &[
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
            ]
        );
    }

    #[test]
    fn raw_args_does_not_survive_a_json_round_trip() {
        // The raw-JSON case only bites if the scripted payload differs from
        // what parse-then-reserialize would produce. If this ever passes
        // identically the payload needs replacing, not the check.
        let round_trip =
            serde_json::to_string(&serde_json::from_str::<Value>(RAW_TOOL_ARGS).unwrap())
                .expect("serialize");
        assert_ne!(
            round_trip, RAW_TOOL_ARGS,
            "the scripted payload round-trips identically; the case would be vacuous"
        );
    }

    #[test]
    fn ownership_markers_are_distinguishable() {
        assert_ne!(
            Scenario::owned_response_metadata(),
            Scenario::foreign_response_metadata()
        );
        assert_eq!(
            Scenario::owned_replay_state().response,
            Some(Scenario::owned_response_metadata())
        );
    }

    #[test]
    fn summarize_handles_empty() {
        assert_eq!(summarize(&[]), "<none>");
    }

    #[test]
    fn drive_thread_reports_a_stall_instead_of_hanging() {
        // A stream that never yields: the bound must produce a stall verdict.
        let (_keepalive, receiver) = mpsc::channel::<()>();
        let stream = Box::pin(futures_lite::stream::empty::<StreamEvent>());
        // An empty stream ends immediately; use a pending stream instead.
        drop(stream);
        let pending = Box::pin(futures_lite::stream::pending::<StreamEvent>());
        let collected = collect_stream(pending, Duration::from_millis(120));
        assert!(collected.stalled, "stalled stream was not reported: {collected:?}");
        drop(receiver);
    }

    #[test]
    fn drive_catches_a_panicking_stream() {
        struct Panics;
        impl ModelAdapter for Panics {
            fn provider(&self) -> &str {
                "panics"
            }
            fn owns(&self, _r: &ReplayState) -> bool {
                false
            }
            fn stream(
                &self,
                _c: harnless_seams::CallId,
                _m: &[Message],
                _t: &[ToolSchema],
                _r: Option<ReplayState>,
            ) -> harnless_seams::Result<BoxStream> {
                panic!("provider exploded")
            }
        }
        fn scenario_for(_case: &str) -> Scenario {
            Scenario::new(Vec::new())
        }
        let violations =
            check_model_adapter_contract(&Panics, "usage_before_finish", scenario_for);
        assert!(
            violations.iter().any(|v| v.detail.contains("panicked")),
            "panicking provider was not reported as a violation: {violations:?}"
        );
    }

    #[test]
    fn unknown_case_is_a_violation_not_a_panic() {
        struct Noop;
        impl ModelAdapter for Noop {
            fn provider(&self) -> &str {
                "noop"
            }
            fn owns(&self, _r: &ReplayState) -> bool {
                false
            }
            fn stream(
                &self,
                _c: harnless_seams::CallId,
                _m: &[Message],
                _t: &[ToolSchema],
                _r: Option<ReplayState>,
            ) -> harnless_seams::Result<BoxStream> {
                Err(harnless_seams::SeamError::code(ErrorCode::ProviderFailure))
            }
        }
        fn scenario_for(_case: &str) -> Scenario {
            Scenario::new(Vec::new())
        }
        let violations = check_model_adapter_contract(&Noop, "not-a-case", scenario_for);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].case, "not-a-case");
    }
}
