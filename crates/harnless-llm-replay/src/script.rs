use crate::recording::Recording;

/// A scripted corpus: the recordings a replay adapter serves, in order.
/// One recording answers one adapter call; the sequence replays a whole
/// multi-step conversation. When the sequence is exhausted the adapter
/// replays its final recording again — a demo replayed against a longer
/// conversation still behaves deterministically rather than failing.
pub struct Script {
    recordings: Vec<Recording>,
}

impl Script {
    /// Build a script from recordings, in call order.
    ///
    /// # Panics
    /// Panics when given no recordings: an adapter with nothing to replay
    /// would fail every call with an empty-completion error, which is a
    /// construction mistake, not a stream outcome.
    pub fn new(recordings: Vec<Recording>) -> Self {
        assert!(
            !recordings.is_empty(),
            "a replay script needs at least one recording"
        );
        Self { recordings }
    }

    /// Build a script from one recording (the common single-turn case).
    pub fn one(recording: Recording) -> Self {
        Self::new(vec![recording])
    }

    /// Build a script from golden-file JSON documents, in call order.
    pub fn from_json(docs: &[&str]) -> Result<Self, String> {
        if docs.is_empty() {
            return Err("a replay script needs at least one recording".into());
        }
        let recordings = docs
            .iter()
            .map(|d| Recording::from_json(d))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { recordings })
    }

    /// Build a script from a single golden-file JSON document.
    pub fn from_json_str(doc: &str) -> Result<Self, String> {
        Self::from_json(std::slice::from_ref(&doc))
    }

    /// Read a corpus file: a JSON array of recording documents, in call
    /// order.
    ///
    /// This is the multi-recording golden-file shape. A single recording's
    /// golden JSON is an object; a script that serves several turns is the
    /// array of those objects. The array round-trips through
    /// [`Script::from_json`], so a corpus written by
    /// [`Script::write_corpus_json`] always loads.
    ///
    /// [`Script::write_corpus_json`]: Script::write_corpus_json
    pub fn from_json_file(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read script {}: {e}", path.display()))?;
        Self::from_json_file_text(&text)
    }

    /// As [`Script::from_json_file`], from already-read text.
    ///
    /// Accepts either golden shape: a single recording object (today's
    /// single-turn golden file) or a JSON array of recording documents (a
    /// multi-turn corpus).
    pub fn from_json_file_text(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("corpus is not JSON: {e}"))?;
        match value {
            serde_json::Value::Array(items) => {
                let docs: Vec<String> = items.iter().map(|d| d.to_string()).collect();
                let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
                Self::from_json(&refs)
            }
            serde_json::Value::Object(_) => Recording::from_json(text).map(Self::one),
            _ => Err("corpus must be a recording object or an array of them".to_string()),
        }
    }

    /// Write this script's recordings to `path` as a corpus file.
    ///
    /// Each recording is validated first — a corpus that would not replay
    /// fails here, at the authoring site, not at some later mount.
    pub fn write_corpus_json(&self, path: &std::path::Path) -> Result<(), String> {
        let docs: Vec<serde_json::Value> = self
            .recordings
            .iter()
            .map(|r| {
                r.validate()?;
                let json = r.to_json()?;
                serde_json::from_str(&json).map_err(|e| format!("recording doc: {e}"))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let text = serde_json::to_string_pretty(&serde_json::Value::Array(docs))
            .map_err(|e| format!("cannot serialize corpus: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// The recording for call `n`, without the past-the-end repeat.
    ///
    /// A test spy that serves a script one call at a time needs the exact
    /// recording at each index — the past-the-end repeat is the adapter's
    /// demo behavior, and a spy that inherits it would silently mask extra
    /// turns. Out-of-range is the caller's bug; this panics on it.
    pub fn recording_at(&self, n: usize) -> &Recording {
        &self.recordings[n]
    }

    /// The number of recordings in the script.
    pub fn len(&self) -> usize {
        self.recordings.len()
    }

    /// Whether the script is empty.
    pub fn is_empty(&self) -> bool {
        self.recordings.is_empty()
    }

    /// The recording for call `n`: past the end, the last one repeats.
    pub(crate) fn get(&self, n: usize) -> &Recording {
        &self.recordings[n.min(self.recordings.len() - 1)]
    }
}
