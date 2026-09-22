//! The profile document: the composed configuration a boot mounts.
//!
//! A profile is the serializable shape of one composition — which seams the
//! profile mounts, how the model is reached, and what the tool pipeline
//! permits. This type is the single source of truth for both sides of the
//! boot contract: [`crate::boot::BootComposer::compose`] produces one, and
//! [`crate::boot::BootComposer::dump`] serializes the very document that
//! composition consumes, so `--dump-config` output always equals what a boot
//! would mount (dump-equals-mount).
//!
//! The document round-trips through YAML losslessly: `load(dump(p))` yields
//! an equal profile, which is the property the CLI's integration tests pin.

use serde::{Deserialize, Serialize};

/// How a profile reaches a model.
///
/// `Replay` is the network-free scripted composition (the built-in default);
/// `None` is a composed profile with no provider wired yet — running it is a
/// named, machine-routable error, never a silent no-op.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ModelSpec {
    /// No model provider composed; `run` fails with `no-model-provider`.
    #[default]
    None,
    /// Deterministic replay against a built-in or file-backed script.
    Replay {
        /// Provider identity the replay adapter declares.
        #[serde(default = "default_provider")]
        provider: String,
        /// Optional golden-file path; absent means the built-in demo script.
        #[serde(default)]
        script: Option<String>,
    },
}

fn default_provider() -> String {
    "openai".to_string()
}

/// One composed profile document.
///
/// Field order here is the order `dump` emits, keeping the golden shape
/// stable across releases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDoc {
    /// The profile name this document composes.
    pub name: String,
    /// Seam/plugin services the composition mounts, in mount order.
    pub seams: Vec<String>,
    /// The model composition.
    pub model: ModelSpec,
    /// Tools the tool pipeline registers at boot.
    pub tools: Vec<String>,
    /// The base system-prompt directive for assembled prompts.
    pub system_prompt: String,
    /// The session store the composition mounts, as the plan's tail key.
    ///
    /// `None` is sessionless mode (#69 §4): resume/fork/list fail
    /// `storage-not-mounted`. Appended **last** so every existing dump
    /// golden keeps its byte order; `#[serde(default)]` keeps a dump from an
    /// older build loadable (absent = sessionless).
    #[serde(default)]
    pub store: Option<StoreSpec>,
}

/// The plan's projection of a storage row (#69 §1): where the session logs
/// live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreSpec {
    /// The state directory holding `<id>.jsonl` session files.
    pub dir: String,
}

impl ProfileDoc {
    /// The built-in `default` profile: core seams (session log, event
    /// registry, tool pipeline, agent loop via the spine) plus the
    /// network-free replay model.
    pub fn default_profile() -> Self {
        Self {
            name: "default".to_string(),
            seams: vec![
                "spine".to_string(),
                "session-log".to_string(),
                "tool-pipeline".to_string(),
                "agent-loop".to_string(),
            ],
            model: ModelSpec::Replay {
                provider: default_provider(),
                script: None,
            },
            tools: Vec::new(),
            system_prompt: "You are harnless, a helpful agent.".to_string(),
            // The reference plan stays sessionless on purpose (#69 §2): the
            // `DefaultComposer` is the seam-test fixture and must never
            // touch `$HOME`. Only the config-boot built-in bundle ships the
            // store row.
            store: None,
        }
    }

    /// Whether this profile composes any model provider.
    pub fn has_model(&self) -> bool {
        !matches!(self.model, ModelSpec::None)
    }

    /// Serialize to the YAML boot shape — the bytes `--dump-config` prints.
    ///
    /// # Panics
    /// Only if the document contains a value YAML cannot represent; every
    /// field of [`ProfileDoc`] is plain data, so this is unreachable for
    /// documents produced by [`ProfileDoc::default_profile`] or [`load`].
    pub fn dump(&self) -> String {
        serde_yaml::to_string(self).expect("profile document is plain YAML data")
    }

    /// Parse a profile document from YAML, rejecting unknown fields so a
    /// dump from a newer build fails loudly instead of mounting silently.
    pub fn load(yaml: &str) -> Result<Self, String> {
        serde_yaml::from_str(yaml).map_err(|e| format!("invalid profile document: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_reloads_to_an_equal_document() {
        let doc = ProfileDoc::default_profile();
        let text = doc.dump();
        let back = ProfileDoc::load(&text).expect("dump reloads");
        assert_eq!(doc, back);
    }

    #[test]
    fn reload_of_dump_is_stable_under_re_dump() {
        let text = ProfileDoc::default_profile().dump();
        let again = ProfileDoc::load(&text).unwrap().dump();
        assert_eq!(text, again);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut text = ProfileDoc::default_profile().dump();
        text.push_str("bogus: true\n");
        assert!(ProfileDoc::load(&text).is_err());
    }

    #[test]
    fn none_model_reports_no_provider() {
        let mut doc = ProfileDoc::default_profile();
        doc.model = ModelSpec::None;
        assert!(!doc.has_model());
        assert!(doc.dump().contains("kind: none"));
    }
}
