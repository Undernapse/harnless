//! Run the model-adapter conformance suite against the shipped replay adapter.
//!
//! Same discipline as `harnless-fs-local/tests/conformance.rs`: adding a
//! provider means passing the suite unchanged. The replay adapter is the
//! cheapest honest adapter in the workspace — it has no network and no
//! provider SDK to blame — so a violation here is either a real contract break
//! or a scenario the harness described wrongly, and both are worth catching.
//!
//! The suite takes the scripted corpus as a parameter rather than hardcoding a
//! fixture: [`scenario_for`] is this provider's scenario library, and it builds
//! each case's corpus through the *real* recording path
//! ([`Recording::capture`] / [`Recording::capture_failed`]). That matters for
//! what the run proves. A harness that hand-wrote a [`Recording`] would only
//! prove that the replay adapter replays whatever JSON it is handed; capturing
//! from the frames the case expects proves the whole loop — capture, golden
//! serialization, restore, validate, replay — preserves the obligation the case
//! is about. So the corpus here is produced the way a live corpus is produced,
//! round-tripped through golden JSON, and only then handed to the adapter.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use harnless_conformance::adapter_suite::{Scenario, ScenarioKind};
use harnless_llm_replay::{Recording, ReplayAdapter, Script};
use harnless_seams::error::ErrorCode;
use harnless_seams::ids::MessageId;
use harnless_seams::llm::{
    BlockKind, ContentBlock, Message, ProviderFailure, ReplayState, Role, StreamFrame, ToolSchema,
    Usage,
};
use serde_json::{json, Value};

/// The key `ReplayAdapter::owns` reads out of replay state.
///
/// This is the adapter's own private convention (mirroring what the live
/// adapters stamp), which is exactly why the suite cannot assume it and the
/// harness has to declare it.
const REPLAY_OWNER_KEY: &str = "__harnless_provider";

/// The provider identity the suite drives.
///
/// `owns()` is an exact-name match against the marker stamped into replay
/// state, so the ownership case needs a state document stamped with *this*
/// name to claim, and a differently-named one to refuse.
const PROVIDER: &str = "replay-conformance";

// ---------------------------------------------------------------------------
// The corpus library
// ---------------------------------------------------------------------------

/// One scripted turn, in the shape a golden file holds it.
///
/// `frames` is what the adapter must emit; `response`/`blocks` are the replay
/// metadata the recording carries; `failure` is the terminal failure for a turn
/// that ended in one instead of a `Finish`.
struct Corpus {
    frames: Vec<StreamFrame>,
    response: Option<Value>,
    blocks: Vec<Value>,
    failure: Option<ProviderFailure>,
}

impl Corpus {
    /// A plain text turn with disjoint usage, the shape most cases drive.
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
        }
    }

    /// A tool-call turn streaming the suite's awkward argument payload.
    ///
    /// The payload is split into two fragments mid-string so a buffered,
    /// parse-then-reserialize adapter has the chance to take it — which is the
    /// behaviour the raw-JSON case exists to catch.
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
        }
    }

    /// A turn that ends in-band with `code` after emitting one block.
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
                message: "recorded failure".into(),
            }),
        }
    }

    /// A completion with no content at all.
    fn silent_turn() -> Self {
        Self {
            frames: Vec::new(),
            response: None,
            blocks: Vec::new(),
            failure: None,
        }
    }

    /// Blocks emitted in **descending** index order.
    ///
    /// This is the interleaving that tells emission-order alignment apart from
    /// index-order alignment — bug #25's shape. Metadata is appended at each
    /// `BlockEnd`, so the honest recording pairs `[index 1, index 0]`, and an
    /// adapter that sorted by index would store `[index 0, index 1]` instead.
    fn interleaved_turn() -> Self {
        Self {
            frames: vec![
                StreamFrame::BlockStart {
                    index: 1,
                    kind: BlockKind::Text,
                },
                // A block the provider opened but never closed: the recording
                // keeps it, `to_frames` replays it, and the assembler marks the
                // stream truncated at `Finish`. That is the honest shape of a
                // real interleaved capture — providers do drop blocks — and it
                // is what stops the replay adapter's own emission from being
                // hand-matched to the scenario's expectation.
                StreamFrame::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                },
                StreamFrame::TextDelta {
                    index: 0,
                    text: "first".into(),
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
            blocks: vec![json!({"index": 1}), json!({"index": 0})],
            failure: None,
        }
    }

    /// The corpus the suite's `case` drives.
    ///
    /// The case name wins over the kind for the two failure-shape cases that
    /// share a kind but script different corpora: `sanctioned_failure_paths`
    /// drives a provider that *throws* a broken corpus, and
    /// `in_band_failure_is_terminal` drives one that fails in band. A replay
    /// adapter only has the in-band path, so both of its corpora are the same
    /// failed turn — but the ownership case needs naming too, and naming is
    /// cheaper than a second kind per case.
    fn for_case(case: &str) -> Self {
        match case {
            "empty_completion_is_retryable_failure" => Self::silent_turn(),
            "context_overflow_canonical_code" => Self::failed_turn(ErrorCode::ContextOverflow),
            "raw_json_tool_arguments" => Self::tool_turn(),
            "replay_alignment_is_emission_order" => Self::interleaved_turn(),
            "sanctioned_failure_paths" | "in_band_failure_is_terminal" => {
                Self::failed_turn(ErrorCode::ProviderFailure)
            }
            _ => Self::text_turn(),
        }
    }

    /// This corpus as a recording, captured the way a live corpus is captured.
    ///
    /// A failed turn goes through [`Recording::capture_failed`]; everything else
    /// through [`Recording::capture`] with the replay state the recording itself
    /// would hand back. The silent turn has nothing to capture — its recording
    /// is empty by construction, and the adapter derives the empty-completion
    /// failure from that emptiness rather than from a stored failure.
    fn recording(&self) -> Recording {
        match &self.failure {
            Some(failure) => Recording::capture_failed(&self.frames, failure),
            None => {
                let replay = ReplayState {
                    response: self.response.clone(),
                    blocks: self.blocks.clone(),
                };
                Recording::capture(&self.frames, &replay)
            }
        }
    }

    /// The scenario a provider harness builds for this corpus.
    ///
    /// `marker` is the ownership-marker *value* stamped into the scenario:
    /// exactly [`PROVIDER`], which is what [`ReplayAdapter::owns`]
    /// exact-name-matches. The whole-suite harness routes a stream by the
    /// running case's *index* — a process-wide slot the scenario factory
    /// writes and the wrapper reads (see [`WholeSuite`]) — so the scenario
    /// itself never carries harness state.
    fn scenario(&self, kind: ScenarioKind, marker: &str) -> Scenario {
        // The suite cannot know which key this adapter stamps into replay state,
        // so the harness declares it up front — before any ownership document is
        // built, and once, in the single place scenarios are constructed. The key
        // is `__harnless_provider` and the value naming it is this adapter's
        // identity. Without the declaration the ownership case would drive
        // `owns()` with a document this adapter has no reason to recognise, and a
        // pass would prove nothing about the real handoff.
        let scenario = Scenario::new(vec![user_message()])
            .ownership_marker(REPLAY_OWNER_KEY, marker)
            .expected(self.frames.clone())
            // The replay adapter appends exactly what the recording carries, in
            // the order the recording carries it. Declaring this (rather than
            // leaving it unset) is what lets the alignment case bite: the suite
            // then knows what *this* provider publishes and can tell an
            // emission-ordered pairing from an index-ordered one.
            .published_metadata(self.blocks.clone());
        let scenario = match &self.response {
            Some(response) => scenario.response_metadata(response.clone()),
            None => scenario,
        };
        let scenario = match &self.failure {
            Some(failure) => scenario.failure(failure.clone()),
            None => scenario,
        };
        if Corpus::carries_replay(kind) {
            // The ownership case is the one that hands `stream()` state this
            // adapter claims; it drives the plain text corpus. `with_owned_replay`
            // builds the document from the marker declared on this scenario, so
            // the state the case drives `owns()` with and the state handed to
            // `stream()` are built by the same code.
            scenario.with_owned_replay()
        } else {
            scenario
        }
    }

    /// Whether this corpus is the one the ownership case drives.
    ///
    /// The ownership case needs `stream()` to be handed state the adapter
    /// claims, and it drives a plain text turn — so only that corpus carries
    /// replay state. Asserting the shape rather than the case name is what keeps
    /// the harness honest if the suite ever moves the case to another corpus.
    fn carries_replay(kind: ScenarioKind) -> bool {
        kind == ScenarioKind::TextTurn
    }
}

/// A user message; the replay adapter ignores content, but the scenario needs
/// a conversation to hand `stream`.
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

/// Replay state stamped as this adapter's own.
///
/// `ReplayAdapter::owns` matches the `__harnless_provider` marker against the
/// adapter's identity, so this is the document it must claim. Built through the
/// suite's own builder so the state the harness asserts on here is the *same
/// document* the suite hands `owns()` — two hand-written copies of a shape drift
/// silently, and the drift shows up as a case that passes for the wrong reason.
fn owned_state() -> ReplayState {
    Scenario::new(vec![user_message()])
        .ownership_marker(REPLAY_OWNER_KEY, PROVIDER)
        .owned_replay_state()
}

/// Replay state stamped as another adapter's.
fn foreign_state() -> ReplayState {
    Scenario::new(vec![user_message()])
        .ownership_marker(REPLAY_OWNER_KEY, PROVIDER)
        .foreign_replay_state()
}

// ---------------------------------------------------------------------------
// The provider's scenario library
// ---------------------------------------------------------------------------

/// Build the scenario for one case: capture its corpus, round-trip it through
/// golden JSON, and describe what the adapter owes back.
///
/// The JSON round-trip is not decoration. A golden file is the only form a
/// corpus survives in, and the replay adapter restores frames from it — so a
/// case that skipped the round-trip would pass on a capture path that no real
/// corpus ever takes.
fn scenario_for(case: &str, kind: ScenarioKind) -> Scenario {
    let corpus = Corpus::for_case(case);
    let recording = corpus.recording();
    // The suite cannot know which key this adapter stamps into replay state, so
    // the harness declares it before anything builds a state document. This is
    // the whole ownership contract for this provider: the key is
    // `__harnless_provider` and the value naming it is this adapter's identity.
    // Without the declaration the ownership case would drive `owns()` with a
    // document this adapter has no reason to recognise, and a pass would mean
    // nothing.
    let restored = Recording::from_json(&recording.to_json().expect("serialize recording"))
        .expect("restore recording");
    // The restored recording must still be the corpus the case is about; a lossy
    // capture would make the scenario describe a turn the adapter never sees.
    assert_eq!(
        restored.to_frames().expect("restore frames"),
        corpus.frames,
        "the golden round-trip changed the {kind:?} corpus"
    );
    let mut scenario = corpus.scenario(kind, PROVIDER);
    // The suite's stall bound is generous for a local replay, but a provider
    // that never yields would otherwise hang the run for the full window.
    scenario.stall_timeout = Duration::from_secs(5);
    // Register the corpus this scenario drives, keyed by the scenario's own
    // identity, so the whole-suite adapter serves it to the stream this case
    // opens (see [`WholeSuite`]). Registration happens on the test thread
    // before the scenario leaves the factory, and the suite's drive thread only
    // ever looks the key up after that.
    WholeSuite::register(&scenario, restored);
    scenario
}

/// An adapter scripted with one case's captured recording.
///
/// Used by the provider-specific pins below, which name their case rather than
/// going through the suite's factory.
fn make_adapter_for(case: &str) -> ReplayAdapter {
    make_adapter(case)
}

/// The macro's adapter factory: a plain `fn` item, as the macro requires.
///
/// The adapter is scripted with exactly the case's own recording, so a case can
/// only ever replay the corpus its scenario describes. A mis-paired corpus would
/// not just fail the case — it would fail it with a *wrong* explanation ("the
/// provider invented output" for a tool-call turn), which is worse than a
/// failure. Naming the case in the factory is what makes the pairing structural.
fn make_adapter(case: &str) -> ReplayAdapter {
    ReplayAdapter::new(PROVIDER, Script::one(Corpus::for_case(case).recording()))
}

/// The whole-suite adapter: every case's corpus behind one seam object.
///
/// # Why not one script of ten recordings
///
/// `ReplayAdapter` picks its recording with an internal call counter: call *n*
/// answers recording *n*, and the counter belongs to the adapter. The seam gives
/// the suite no way to tell an adapter which case it is driving, so a script in
/// case order is correct only while every case drives exactly one stream.
///
/// That is not safe to bake into a harness. A case is free to open more than one
/// stream — an ownership-style check that hands the adapter two state documents
/// does exactly that — and one extra stream in case *k* shifts every later case
/// onto the wrong corpus. The suite then reports a violation blaming the provider
/// for a mix-up that lives in the harness.
///
/// # Why route by scenario identity
///
/// A process-wide "current case" slot is also unsafe, and was the original
/// flake: the suite's drive thread is released at its next await point rather
/// than killed, so a late `stream()` call from a dead case's worker can read
/// whatever slot the *next* case has written and serve the wrong corpus. This
/// harness instead keys each recording by the *scenario* it belongs to:
/// [`scenario_for`] registers the scenario it builds, and `stream` rebuilds the
/// key from the arguments it was handed — the suite passes the scenario's own
/// messages, tools, and replay state through verbatim, so the key the wrapper
/// computes is the key the factory registered. A late or mis-ordered call can
/// only ever replay the corpus its scenario describes.
///
/// The key is the scenario's observable identity: its messages, tools, and
/// replay state. Those are distinct per case here because the corpora are
/// distinct — the tool case's scenario carries the tool set, the ownership
/// case's carries replay state, the rest carry the plain user message with no
/// tools and no state. (Cases sharing a corpus *and* a scenario — the two
/// failed-turn cases — share a recording too, which is exactly right: their
/// corpora are the same.)
///
/// A scenario this harness never registered (a hand-built scenario from a
/// sibling suite's shared fixture) falls back to the text-turn corpus, which is
/// what such a scenario describes.
struct WholeSuite {
    /// The fallback corpus for foreign scenarios (see [`WholeSuite`]).
    fallback: Recording,
    /// A replay adapter used only for its `owns` contract — ownership is
    /// identity-based, so any adapter under this provider's identity answers
    /// identically for every case.
    owner: ReplayAdapter,
}

/// A scenario's observable identity: what `stream` is handed.
///
/// JSON-serialising the pieces keeps the key total (the seam types are `Debug`
/// and `PartialEq`, not `Hash`) and makes key equality exactly the scenarios'
/// observable equality.
#[derive(PartialEq, Eq, Hash)]
struct ScenarioKey(String);

/// The scenario→recording registry, shared by every [`WholeSuite`].
///
/// Process-wide because the suite's scenario factory is a plain `fn` item — it
/// receives no adapter to register with — so the only object the factory can
/// reach is a static. Entries are keyed by scenario identity, so even the
/// per-case tests' registrations (which the whole-suite run never looks up,
/// since those scenarios drive their own per-case adapters) cannot mis-pair a
/// case: a lookup returns the recording registered for *that scenario*.
static ROUTES: LazyLock<Mutex<std::collections::HashMap<ScenarioKey, Recording>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

impl WholeSuite {
    /// Register the recording a scenario drives, keyed by its identity.
    fn register(scenario: &Scenario, recording: Recording) {
        let key = scenario_key(
            &scenario.messages,
            &scenario.tools,
            scenario.replay.as_ref(),
        );
        ROUTES.lock().expect("route map").insert(key, recording);
    }
}

/// The identity key of a scenario's stream arguments.
fn scenario_key(
    messages: &[Message],
    tools: &[ToolSchema],
    replay: Option<&ReplayState>,
) -> ScenarioKey {
    // `Debug` is the total, deterministic rendering these types carry; the key
    // only ever compares renderings of the same type, so the format is an
    // implementation detail, not a contract.
    ScenarioKey(format!("{messages:?} | {tools:?} | {replay:?}"))
}

impl harnless_seams::llm::ModelAdapter for WholeSuite {
    fn provider(&self) -> &str {
        PROVIDER
    }

    fn owns(&self, replay_state: &ReplayState) -> bool {
        self.owner.owns(replay_state)
    }

    fn stream(
        &self,
        _call_id: harnless_seams::CallId,
        messages: &[Message],
        tools: &[ToolSchema],
        replay: Option<ReplayState>,
    ) -> harnless_seams::error::Result<harnless_seams::llm::BoxStream> {
        let key = scenario_key(messages, tools, replay.as_ref());
        let recording = ROUTES
            .lock()
            .expect("route map")
            .get(&key)
            .cloned()
            .unwrap_or_else(|| self.fallback.clone());
        self.owner.stream_recording(&recording)
    }
}

/// The whole-suite factory.
///
/// The route registry fills as [`scenario_for`] builds each case's scenario,
/// which the suite does immediately before it drives that case — so by the time
/// a stream is opened, its recording is registered. The factory cannot
/// pre-register: scenario identity is what the key names, and only the factory
/// builds scenarios.
fn make_full_suite_adapter(_cases: &[&str]) -> WholeSuite {
    WholeSuite {
        fallback: Corpus::text_turn().recording(),
        owner: ReplayAdapter::new(PROVIDER, Script::one(Corpus::text_turn().recording())),
    }
}

harnless_conformance::conformance_tests_adapter! {
    replay,
    make_adapter,
    scenario_for,
    make_full_suite_adapter,
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

// ---------------------------------------------------------------------------
// The provider-specific pins the suite's cases cannot express
// ---------------------------------------------------------------------------

/// The ownership marker is the adapter's own, not the suite's fixture.
///
/// The suite's ownership case drives `owns()` with state documents built from
/// *its* marker keys. This adapter's real ownership contract is the
/// `__harnless_provider` identity, so the pin that matters for this provider is
/// stated here: it claims state stamped with its own name and refuses state
/// stamped with anyone else's, including bare state.
#[test]
fn owns_only_state_stamped_with_its_own_identity() {
    use harnless_seams::llm::ModelAdapter;

    let adapter = make_adapter_for("replay_state_ownership");
    assert!(
        adapter.owns(&owned_state()),
        "the adapter refused replay state stamped with its own provider identity"
    );
    assert!(
        !adapter.owns(&foreign_state()),
        "the adapter claimed replay state stamped with another adapter's identity; an \
         ownership-gated handoff would then hand a conversation to an adapter that cannot \
         interpret it"
    );
    assert!(
        !adapter.owns(&ReplayState::default()),
        "the adapter claimed bare replay state; there is no owner to interpret it"
    );
}

/// A malformed corpus throws at the stream entry; it is never a stream outcome.
///
/// The suite's failure-path case accepts either sanctioned path, so it cannot
/// pin *this* provider's choice. For a replay adapter the choice is forced: a
/// corpus that cannot be decoded is a broken fixture, and a caller must not be
/// able to retry its way past it.
#[tokio::test(flavor = "multi_thread")]
async fn a_broken_corpus_throws_rather_than_failing_in_band() {
    use futures::StreamExt;
    use harnless_seams::llm::{ModelAdapter, StreamEvent};

    // `Usage` recorded without the block it belongs to is not a protocol
    // violation by itself; what `validate` rejects is a frame after `Finish`,
    // which is the corruption a golden file can actually acquire in review.
    let mut corpus = Corpus::text_turn();
    corpus.frames.push(StreamFrame::TextDelta {
        index: 0,
        text: "after finish".into(),
    });
    let recording = corpus.recording();
    let adapter = ReplayAdapter::new(PROVIDER, Script::one(recording));

    let thrown = adapter.stream(
        harnless_seams::CallId(1),
        &[user_message()],
        &[ToolSchema::new("t", "t", json!({}))],
        None,
    );
    match thrown {
        Ok(mut stream) => {
            // If it did not throw, the corruption must not be replayable as a
            // normal completion.
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                events.push(event);
            }
            let failed = events.iter().any(|e| matches!(e, StreamEvent::Failed(_)));
            assert!(
                failed,
                "a corpus violating the stream protocol replayed as a normal completion: \
                 {events:?}"
            );
        }
        Err(err) => assert_eq!(
            err.code,
            ErrorCode::ProviderFailure,
            "a broken corpus surfaced `{}`; a construction mistake must not be classified \
             as a retryable provider outcome",
            err.code.as_str()
        ),
    }
}

/// An empty recording is the retryable empty-completion failure, in band.
///
/// Pinned separately from the suite's case because this provider derives the
/// failure from emptiness rather than storing it: the classification is the
/// adapter's own decision, and it is the one a caller retries on.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_recording_fails_with_the_retryable_code() {
    use futures::StreamExt;
    use harnless_seams::llm::{ModelAdapter, StreamEvent};

    let adapter = make_adapter_for("empty_completion_is_retryable_failure");
    let mut stream = adapter
        .stream(harnless_seams::CallId(1), &[user_message()], &[], None)
        .expect("empty corpus is a valid corpus");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let failure = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Failed(failure) => Some(failure.clone()),
            _ => None,
        })
        .expect("an empty completion must surface as a failure, not a silent success");
    assert_eq!(
        failure.code,
        ErrorCode::EmptyCompletion,
        "an empty completion surfaced `{}`; the caller decides to retry by reading \
         `empty-completion`",
        failure.code.as_str()
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, StreamEvent::Frame(StreamFrame::Finish))),
        "an empty completion also finished; a stream ends exactly one way"
    );
}
