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
        let recordings = docs
            .iter()
            .map(|d| Recording::from_json(d))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(recordings))
    }

    /// Build a script from a single golden-file JSON document.
    pub fn from_json_str(doc: &str) -> Result<Self, String> {
        Self::from_json(std::slice::from_ref(&doc))
    }

    /// The recording for call `n`: past the end, the last one repeats.
    pub(crate) fn get(&self, n: usize) -> &Recording {
        &self.recordings[n.min(self.recordings.len() - 1)]
    }
}
