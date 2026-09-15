//! The model boundary: compose a [`ModelAdapter`] from a profile document.
//!
//! The one-shot runner and the REPL both drive turns through the composed
//! adapter. A profile that names no provider yields `None` here and a named
//! `no-model-provider` error at run time — never a silent empty answer.
//!
//! Today the only network-free composition is the replay adapter, which
//! serves a built-in demo script (or a golden file named by the profile), so
//! `hrls run` works end to end with no provider credentials.

use std::sync::Arc;

use futures::stream::Stream as _;
use harnless_llm_replay::{Recording, ReplayAdapter, Script};
use harnless_seams::{BlockKind, CallId, Message, ModelAdapter, StreamEvent, StreamFrame, Usage};

use crate::profile::{ModelSpec, ProfileDoc};
use crate::CliError;

/// A composed model provider behind a shared handle.
pub type ModelHandle = Arc<dyn ModelAdapter>;

/// Build the adapter a profile names, or `None` for a providerless profile.
pub fn build_adapter(doc: &ProfileDoc) -> Result<Option<ModelHandle>, CliError> {
    match &doc.model {
        ModelSpec::None => Ok(None),
        ModelSpec::Replay { provider, script } => {
            let loaded = match script.as_deref() {
                Some(path) => {
                    let text = std::fs::read_to_string(path).map_err(|e| {
                        CliError::new("bad-script", format!("cannot read script {path}: {e}"))
                    })?;
                    (load_script_text(&text)?, path.to_string())
                }
                None => (Script::one(demo_recording()), String::new()),
            };
            Ok(Some(Arc::new(ReplayAdapter::with_script_id(
                provider.clone(),
                loaded.0,
                loaded.1,
            ))))
        }
    }
}

/// Load script text: a single recording document, or a corpus array of
/// them in call order.
///
/// A golden file for one turn is a recording object; a multi-turn script is
/// the JSON array of those objects. Both shapes load here, so a profile's
/// `script:` path names either.
fn load_script_text(text: &str) -> Result<Script, CliError> {
    Script::from_json_file_text(text).map_err(|e| CliError::new("bad-script", e))
}

/// A minimal built-in recording: one text block answering the prompt.
///
/// This is the demo corpus the built-in `default` profile replays, so a
/// fresh checkout runs `hrls run` with no network and no fixtures.
pub fn demo_recording() -> Recording {
    let hello = "Hello from the harnless replay model.";
    let frames = vec![
        StreamFrame::BlockStart {
            index: 0,
            kind: BlockKind::Text,
        },
        StreamFrame::TextDelta {
            index: 0,
            text: hello.into(),
        },
        StreamFrame::BlockEnd {
            index: 0,
            assembled: harnless_seams::ContentBlock {
                kind: BlockKind::Text,
                text: hello.into(),
            },
        },
        StreamFrame::Usage(Usage {
            uncached_input: 8,
            cached_reads: 0,
            cached_writes: 0,
            output: 8,
            reasoning: 0,
        }),
        StreamFrame::Finish,
    ];
    // `capture` is the write side of the golden format; the replay adapter's
    // validator is the read side, so capturing here guarantees a valid corpus.
    let recording = Recording::capture(&frames, &Default::default());
    recording.validate().expect("demo recording is valid");
    recording
}

/// Poll a never-blocking adapter stream to assembled text.
///
/// The replay stream is an in-memory script, so polling with a noop waker
/// inside a synchronous driver is sound — the same shape the replay crate's
/// own seam test uses. A blocked stream is an adapter bug, surfaced as a
/// named error rather than a hang.
pub fn poll_stream_text(adapter: &ModelHandle, messages: &[Message]) -> Result<String, CliError> {
    let mut stream = adapter
        .stream(CallId(1), messages, &[], None)
        .map_err(|e| CliError::new("model-stream-failed", e.to_string()))?;
    let mut text = String::new();
    // The replay stream never blocks, so a noop waker suffices.
    let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
    loop {
        match std::pin::Pin::new(&mut stream).poll_next(&mut cx) {
            std::task::Poll::Ready(Some(StreamEvent::Frame(frame))) => {
                if let StreamFrame::TextDelta { text: delta, .. } = &frame {
                    text.push_str(delta);
                }
            }
            std::task::Poll::Ready(Some(StreamEvent::Failed(f))) => {
                return Err(CliError::new(
                    "model-stream-failed",
                    format!("{}: {}", f.code, f.message),
                ))
            }
            std::task::Poll::Ready(None) => break,
            std::task::Poll::Pending => {
                return Err(CliError::new(
                    "model-stream-blocked",
                    "model stream blocked; adapter bug",
                ))
            }
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_recording_replays_to_text() {
        let mut doc = ProfileDoc::default_profile();
        doc.model = ModelSpec::Replay {
            provider: "openai".into(),
            script: None,
        };
        let adapter = build_adapter(&doc).unwrap().expect("adapter");
        let text = poll_stream_text(&adapter, &[]).expect("replay");
        assert!(text.contains("Hello from the harnless replay model."));
    }

    #[test]
    fn none_model_composes_no_adapter() {
        let mut doc = ProfileDoc::default_profile();
        doc.model = ModelSpec::None;
        assert!(build_adapter(&doc).unwrap().is_none());
    }

    #[test]
    fn missing_script_file_is_a_named_error() {
        let mut doc = ProfileDoc::default_profile();
        doc.model = ModelSpec::Replay {
            provider: "openai".into(),
            script: Some("/nonexistent/recording.json".into()),
        };
        let err = match build_adapter(&doc) {
            Err(err) => err,
            Ok(_) => panic!("missing script must fail"),
        };
        assert_eq!(err.code, "bad-script");
    }
}
