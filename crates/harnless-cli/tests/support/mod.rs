//! The scripted-model harness for the CLI's loop-seam tests (#63).
//!
//! The contract this serves: the session log is the assertion surface, and
//! the composition under test is the *real* spine mount — real log, real
//! event registry, real guarded tool pipeline, real id allocator. The only
//! thing a test injects is the *script*: which recordings the model serves,
//! in call order.
//!
//! Injection rides the existing `ModelSpec::Replay { script: Some(path) }`
//! shape. A test writes its multi-recording corpus to a process-scoped file
//! and names it in the profile; the harness loads it as a `Script` and
//! serves it through a spy adapter. The spy delegates every call to the real
//! `ReplayAdapter` — frame semantics (usage-before-finish, in-band failure,
//! assembled blocks) stay production, never a test re-implementation — and
//! records what the runner sent so tests can pin the wire shape too.
//!
//! The spy serves each script recording *exactly once* (`recording_at`), so
//! `Script`'s past-the-end repeat can never silently mask an extra turn: a
//! sixth adapter call against a five-recording script panics loudly.
//!
//! Every corpus is *captured* with `Recording::capture` and `validate()`d
//! before it is written, so an invalid corpus fails the test that authored
//! it rather than silently replaying wrong.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use harnless_cli::boot::Mounted;
use harnless_llm_replay::{Recording, ReplayAdapter, Script};
use harnless_seams::{
    BlockAssembler, CallId, ErrorCode, Message, ProviderFailure, ReplayState, ToolSchema,
};

/// A process-scoped corpus path, pre-cleaned.
pub fn corpus_path(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("hrls-seam-{tag}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// Capture `recordings` (in call order) to a corpus file and return its path.
///
/// The file is a JSON array of recording documents. Each recording is
/// validated, and the array is round-tripped through `Script::from_json`
/// before writing — a corpus that fails to reload fails here, loudly.
pub fn write_corpus(tag: &str, recordings: &[Recording]) -> PathBuf {
    let docs: Vec<serde_json::Value> = recordings
        .iter()
        .map(|r| {
            r.validate().expect("fixture corpus validates");
            let json = r.to_json().expect("fixture corpus serializes");
            serde_json::from_str(&json).expect("recording doc is JSON")
        })
        .collect();
    let array = serde_json::Value::Array(docs.clone());
    let texts: Vec<String> = docs.iter().map(|d| d.to_string()).collect();
    let refs_: Vec<&str> = texts.iter().map(String::as_str).collect();
    Script::from_json(&refs_).expect("corpus array reloads as a script");
    let text = serde_json::to_string_pretty(&array).expect("corpus array serializes");
    let path = corpus_path(tag);
    std::fs::write(&path, text).expect("write corpus");
    path
}

/// Load a corpus file as the script it encodes.
pub fn load_script(path: &std::path::Path) -> Script {
    let text = std::fs::read_to_string(path).expect("read corpus");
    let value: serde_json::Value = serde_json::from_str(&text).expect("corpus is JSON");
    let docs: Vec<String> = value
        .as_array()
        .expect("corpus is an array of recording documents")
        .iter()
        .map(|d| d.to_string())
        .collect();
    let refs_: Vec<&str> = docs.iter().map(String::as_str).collect();
    Script::from_json(&refs_).expect("corpus builds a script")
}

/// One recorded adapter request — what the composed runner put on the wire.
#[derive(Debug, Clone)]
pub struct Request {
    /// The call id the runner minted for this request.
    pub call_id: u64,
    /// The message list the runner projected from the log.
    pub messages: Vec<Message>,
    /// The tool-schema names the runner declared.
    pub tools: Vec<String>,
}

/// A scripted model adapter: serves its script's recordings in call order,
/// exactly once each, and records every request.
pub struct ScriptedModel {
    script: Script,
    calls: Mutex<Vec<Request>>,
}

impl ScriptedModel {
    pub fn new(script: Script) -> Self {
        Self {
            script,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The requests observed so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.calls.lock().unwrap().clone()
    }

    /// The number of adapter calls so far.
    pub fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl harnless_seams::ModelAdapter for ScriptedModel {
    fn provider(&self) -> &str {
        "scripted"
    }

    fn owns(&self, _replay_state: &ReplayState) -> bool {
        false
    }

    fn stream(
        &self,
        call_id: CallId,
        messages: &[Message],
        tools: &[ToolSchema],
        _replay_state: Option<ReplayState>,
    ) -> harnless_seams::Result<harnless_seams::BoxStream> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(Request {
                call_id: call_id.0,
                messages: messages.to_vec(),
                tools: tools.iter().map(|t| t.name.clone()).collect(),
            });
            calls.len() - 1
        };
        // Serve the exact recording at this index — never the past-the-end
        // repeat — so an unexpected extra adapter call panics instead of
        // silently replaying the last recording again.
        let recording = self.script.recording_at(n).clone();
        ReplayAdapter::new("scripted", Script::one(recording))
            .stream(call_id, messages, tools, None)
    }
}

/// A profile document for the seam tests: spine seams, scripted model,
/// declared tools.
pub fn seam_profile(
    name: &str,
    script: Option<&std::path::Path>,
    tools: &[&str],
) -> harnless_cli::profile::ProfileDoc {
    harnless_cli::profile::ProfileDoc {
        name: name.to_string(),
        seams: vec![
            "spine".to_string(),
            "session-log".to_string(),
            "tool-pipeline".to_string(),
            "agent-loop".to_string(),
        ],
        model: harnless_cli::profile::ModelSpec::Replay {
            provider: "scripted".to_string(),
            script: script.map(|p| p.display().to_string()),
        },
        tools: tools.iter().map(|t| t.to_string()).collect(),
        system_prompt: "seam test".to_string(),
    }
}

/// A live seam composition: the mounted spine plus its scripted model spy.
pub struct Seam {
    /// The live composition under test.
    pub mounted: Mounted,
    /// The scripted model serving the composition.
    pub model: Arc<ScriptedModel>,
}

/// Compose-and-mount a seam profile with a scripted model.
///
/// The spine, log, registry, tool wiring, and id allocator are the mounted
/// production ones (the reference composer's wired mount, the shape the
/// seam's boot pins). Only the model handle is the harness's spy — and it
/// serves production replay frames.
pub fn mount_seam(name: &str, script: &std::path::Path, tools: &[&str]) -> Seam {
    mount_seam_doc(&seam_profile(name, Some(script), tools))
}

/// As [`mount_seam`], from an explicit document.
pub fn mount_seam_doc(doc: &harnless_cli::profile::ProfileDoc) -> Seam {
    use harnless_cli::boot::{BootComposer, DefaultComposer};
    // The scripted model is built lazily from the corpus the document names.
    // `DefaultComposer::mount` validates that file through the production
    // `build_adapter` route — a corpus the *production* loader rejects fails
    // here, loudly — and then the harness's spy (same corpus, same
    // `Script::from_json` route) takes the mounted model slot. The spy
    // delegates every call to the real `ReplayAdapter`, so the frames the
    // runner folds are production frames.
    let mounted = DefaultComposer.mount(doc).expect("seam profile mounts");
    let path = match &doc.model {
        harnless_cli::profile::ModelSpec::Replay { script, .. } => {
            script.as_ref().map(PathBuf::from)
        }
        _ => None,
    };
    let model = Arc::new(ScriptedModel::new(match path {
        Some(p) => load_script(&p),
        None => Script::one(text_recording("(no script named)")),
    }));
    let mut mounted = mounted;
    mounted.model = Some(model.clone());
    Seam { mounted, model }
}

/// A text-only recording answering with `text`.
pub fn text_recording(text: &str) -> Recording {
    use harnless_seams::{BlockKind, ContentBlock, StreamFrame, Usage};
    let frames = vec![
        StreamFrame::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        },
        StreamFrame::TextDelta {
            index: 0,
            text: text.to_string(),
        },
        StreamFrame::BlockEnd {
            index: 0,
            assembled: ContentBlock {
                kind: BlockKind::Text,
                text: text.to_string(),
            },
        },
        StreamFrame::Usage(Usage::default()),
        StreamFrame::Finish,
    ];
    let rec = Recording::capture(&frames, &ReplayState::default());
    rec.validate().expect("text recording validates");
    rec
}

/// A recording whose stream fails in-band with `code`/`message`.
pub fn failing_recording(code: ErrorCode, message: &str) -> Recording {
    let failure = ProviderFailure {
        code,
        message: message.to_string(),
    };
    let rec = Recording::capture_failed(&[], &failure);
    rec.validate().expect("failure recording validates");
    rec
}

/// A recording that requests the `echo` tool with raw-JSON arguments `args`.
pub fn tool_call_recording(args: &str) -> Recording {
    use harnless_seams::{BlockKind, ContentBlock, StreamFrame, Usage};
    let json = format!(
        "{{\"id\":1,\"name\":\"echo\",\"arguments\":{}}}",
        serde_json::Value::String(args.to_string())
    );
    let frames = vec![
        StreamFrame::BlockStart {
            index: 0,
            kind: BlockKind::ToolCall,
        },
        StreamFrame::ToolCallDelta {
            index: 0,
            call_id: CallId(1),
            json: json.clone(),
        },
        StreamFrame::BlockEnd {
            index: 0,
            assembled: ContentBlock {
                kind: BlockKind::ToolCall,
                text: json,
            },
        },
        StreamFrame::Usage(Usage::default()),
        StreamFrame::Finish,
    ];
    let rec = Recording::capture(&frames, &ReplayState::default());
    rec.validate().expect("tool-call recording validates");
    rec
}

/// Fold a recording's frames the way the production driver folds a stream.
pub fn fold(recording: &Recording) -> Vec<harnless_seams::ContentBlock> {
    let mut assembler = BlockAssembler::new();
    for frame in recording.to_frames().expect("valid corpus") {
        assembler.push(&frame);
    }
    assembler.blocks()
}

/// The session log's committed events, as `(kind, detail)` pairs in position
/// order — the assertion surface the contract names.
pub fn log_events(mounted: &Mounted) -> Vec<(String, String)> {
    use harnless_agent::events::SessionEvent;
    let log = mounted
        .ctx
        .get::<harnless_agent::session::SessionLog>()
        .expect("spine provides the log");
    log.snapshot()
        .records
        .iter()
        .map(|r| match &r.event {
            SessionEvent::TurnOpen => ("turn_open".into(), String::new()),
            SessionEvent::TurnClose { reason } => ("turn_close".into(), format!("{reason:?}")),
            SessionEvent::StepOpen => ("step_open".into(), String::new()),
            SessionEvent::StepClose => ("step_close".into(), String::new()),
            SessionEvent::UserMessage(m) => (
                "user_message".into(),
                format!("id={} {}", m.id.0, text_of(&m.blocks)),
            ),
            SessionEvent::AssistantMessage(m) => (
                "assistant_message".into(),
                format!("id={} {}", m.id.0, text_of(&m.blocks)),
            ),
            SessionEvent::ToolCall(c) => (
                "tool_call".into(),
                format!("call={} tool={} args={}", c.call_id.0, c.tool, c.arguments),
            ),
            SessionEvent::ToolResult(r) => (
                "tool_result".into(),
                format!("call={} content={}", r.call_id.0, r.content),
            ),
            SessionEvent::AssistantChunk(_) => ("assistant_chunk".into(), String::new()),
            SessionEvent::SeedBoundary => ("seed_boundary".into(), String::new()),
        })
        .collect()
}

fn text_of(blocks: &[harnless_agent::events::ContentBlock]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            harnless_agent::events::ContentBlock::Text { text } => text.clone(),
            harnless_agent::events::ContentBlock::Reasoning { text } => format!("<r>{text}"),
            harnless_agent::events::ContentBlock::ToolCall { call_id, arguments } => {
                format!("<call {call_id}:{arguments}>")
            }
            harnless_agent::events::ContentBlock::ToolResult { call_id, content } => {
                format!("<result {call_id}:{content}>")
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// The message ids committed to the log, in append order.
pub fn message_ids(mounted: &Mounted) -> Vec<u64> {
    use harnless_agent::events::SessionEvent;
    let log = mounted
        .ctx
        .get::<harnless_agent::session::SessionLog>()
        .expect("spine provides the log");
    log.snapshot()
        .records
        .iter()
        .filter_map(|r| match &r.event {
            SessionEvent::UserMessage(m) | SessionEvent::AssistantMessage(m) => Some(m.id.0),
            _ => None,
        })
        .collect()
}

/// The log positions of the committed records.
pub fn positions(mounted: &Mounted) -> Vec<usize> {
    let log = mounted
        .ctx
        .get::<harnless_agent::session::SessionLog>()
        .expect("spine provides the log");
    log.snapshot().records.iter().map(|r| r.position).collect()
}

/// The derived history's node ids, recomputed from the whole log.
pub fn derived_ids(mounted: &Mounted) -> Vec<u64> {
    use harnless_agent::history::History;
    let log = mounted
        .ctx
        .get::<harnless_agent::session::SessionLog>()
        .expect("spine provides the log");
    let mut history = History::default();
    for r in log.snapshot().records {
        let _ = history.apply(&r.event);
    }
    history.nodes().iter().map(|n| n.message_id.0).collect()
}
