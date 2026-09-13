//! The config document: plugin rows, bundles, profiles, and patches.
//!
//! One composed configuration is a [`ConfigDoc`] — a `profile` name plus an
//! ordered list of plugin [`Row`]s, each `{id, plugin, config}`. Every layer
//! fed into composition (a bundle, a profile's own rows, a home document, a
//! per-run overlay) is a [`Layer`], and a layer is either whole rows or patch
//! operations over row ids. The document is the only shape that crosses the
//! boot boundary: `--dump-config` prints exactly one, and mount consumes
//! exactly one, which is what makes dump-equals-mount structural.
//!
//! # Row identity
//!
//! Rows are keyed by `id`, never by position. A later layer that declares an
//! id already present *replaces* that row in place (the position of the
//! original declaration is kept, so mount order stays stable across layers);
//! an id a layer introduces is appended. This is what lets a one-line overlay
//! swap a single entry without restating the document.

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

/// The row id a field-wise profile patch targets for its `model` field.
///
/// The composition layer does not know plugin semantics, so a `model:` key in
/// a patch document is translated to a whole-config `set` on this id — the
/// convention every profile in this ecosystem follows for its model row.
pub const MODEL_ROW_KEY: &str = "model";

/// One plugin row: an id, the plugin implementation to instantiate, and the
/// opaque config handed to it.
///
/// `config` is deliberately untyped here — the composition layer does not
/// know what any plugin accepts, and validating it would privilege the config
/// crate over the plugins. The mount stage is where a plugin parses its own
/// config and a bad value becomes a named, plugin-attributed failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// Stable row identity — what patches target and what mount reports.
    pub id: String,
    /// The plugin implementation to instantiate for this row.
    pub plugin: String,
    /// The opaque config handed to the plugin.
    #[serde(default)]
    pub config: Value,
}

impl Row {
    /// A row with no config payload.
    pub fn new(id: impl Into<String>, plugin: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            plugin: plugin.into(),
            config: Value::Null,
        }
    }
}

/// One patch operation over the row set.
///
/// Two operations exist, mirroring the reference model:
///
/// * [`PatchOp::Set`] targets a row **by id** and replaces that row's
///   **whole** `config` with the given value. It is *not* a deep merge: the
///   patch must restate every field it wants kept. Deep-merging plugin config
///   produces compositions nobody can read from the patch alone — you cannot
///   tell what a mounted plugin actually sees without replaying the whole
///   layer history — so the model refuses it. The one exception is
///   [`PatchOp::Insert`], which adds a row rather than editing one.
/// * [`PatchOp::Insert`] appends a whole new row.
///
/// A `set` naming an id that no lower layer declares is a **warning**, not an
/// error: a patch written against a superset of profiles (or a profile that
/// later dropped a bundle) stays usable, and the composition reports exactly
/// which target it skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PatchOp {
    /// Replace the whole `config` of the row with this id.
    Set {
        /// The target row id.
        id: String,
        /// The row's new, complete config.
        config: Value,
    },
    /// Append a new row.
    Insert {
        /// The row to append.
        #[serde(flatten)]
        row: Row,
    },
}

/// One layer of a composition: whole rows, patch operations, or both.
///
/// Bundles and profile documents are [`LayerKind::Rows`]; profile patches,
/// the home patch, and per-run `--patch` overlays are [`LayerKind::Patch`].
/// The distinction is load-bearing for the absent-id warning: only a patch
/// layer may name an id that does not exist yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    /// Human-readable layer name (bundle name, patch file path) used in
    /// warnings and diagnostics.
    #[serde(default)]
    pub name: String,
    /// What this layer contributes.
    #[serde(default)]
    pub rows: Vec<Row>,
    /// Operations this layer applies over the accumulated row set.
    #[serde(default)]
    pub patch: Vec<PatchOp>,
}

impl Layer {
    /// A layer of whole rows (a bundle or a profile document).
    pub fn rows(name: impl Into<String>, rows: Vec<Row>) -> Self {
        Self {
            name: name.into(),
            rows,
            patch: Vec::new(),
        }
    }

    /// A layer of patch operations (a profile patch, home patch, or overlay).
    pub fn patch(name: impl Into<String>, patch: Vec<PatchOp>) -> Self {
        Self {
            name: name.into(),
            rows: Vec::new(),
            patch,
        }
    }

    /// Parse a layer from YAML.
    ///
    /// Accepts three shapes: a full mapping (`name`/`rows`/`patch`), a bare
    /// sequence of rows or patch ops, or a bare single mapping row. A bare
    /// sequence of row-shaped maps is read as rows, a sequence of op-shaped
    /// maps as patch ops; a mapping with an `op` key is a single patch op.
    /// This is what lets a `--patch` file be as small as one document.
    pub fn load(yaml: &str) -> crate::error::Result<Self> {
        let value: Value = serde_yaml::from_str(yaml).map_err(|e| {
            crate::error::ConfigError::new(
                crate::error::Stage::Patch,
                "bad-patch",
                format!("layer is not valid YAML: {e}"),
            )
        })?;
        Self::from_value(value)
    }

    /// Build a layer from an already-parsed YAML value.
    pub fn from_value(value: Value) -> crate::error::Result<Self> {
        use crate::error::{ConfigError, Stage};
        match value {
            Value::Null => Ok(Layer {
                name: String::new(),
                rows: Vec::new(),
                patch: Vec::new(),
            }),
            Value::Sequence(items) => {
                let rows = decode::<Vec<Row>>(Value::Sequence(items.clone())).ok();
                let patch = decode::<Vec<PatchOp>>(Value::Sequence(items.clone())).ok();
                match (rows, patch) {
                    (Some(rows), None) => Ok(Layer::rows(String::new(), rows)),
                    (None, Some(patch)) => Ok(Layer::patch(String::new(), patch)),
                    (Some(_), Some(_)) => Err(ConfigError::new(
                        Stage::Patch,
                        "bad-patch",
                        "sequence layer is ambiguous between rows and patch ops",
                    )),
                    (None, None) => Err(ConfigError::new(
                        Stage::Patch,
                        "bad-patch",
                        "sequence layer is neither rows nor patch ops",
                    )),
                }
            }
            Value::Mapping(_) => {
                // The smallest possible overlay, in decreasing generality: a
                // single patch op, a whole layer, a single row, or a
                // field-wise profile patch (see [`field_patch`]).
                if let Some(op) = decode::<PatchOp>(value.clone()).ok() {
                    return Ok(Layer::patch(String::new(), vec![op]));
                }
                if let Some(layer) = decode::<Layer>(value.clone()).ok() {
                    return Ok(layer);
                }
                if let Some(row) = decode::<Row>(value.clone()).ok() {
                    return Ok(Layer::rows(String::new(), vec![row]));
                }
                if let Some(patch) = field_patch(&value) {
                    return Ok(Layer::patch(String::new(), patch));
                }
                Err(ConfigError::new(
                    Stage::Patch,
                    "bad-patch",
                    format!(
                        "layer mapping is not a layer, row, patch op, or profile patch: {}",
                        first_decode_error(&value)
                    ),
                ))
            }
            other => Err(ConfigError::new(
                Stage::Patch,
                "bad-patch",
                format!("layer must be a mapping or sequence, got {}", kind_of(&other)),
            )),
        }
    }

    /// Serialize this layer as YAML.
    ///
    /// # Panics
    /// Only if the layer contains a value YAML cannot represent; every field
    /// is plain data, so this is unreachable for parsed layers.
    pub fn dump(&self) -> String {
        serde_yaml::to_string(self).expect("layer is plain YAML data")
    }
}

/// A bundle: a named YAML layer of rows, referenced by profiles.
///
/// Bundles are the reusable unit — "the openai + filesystem + bash setup" —
/// and declare their insert rows directly. A profile names bundles in order;
/// the bundle name is what warnings and the layer trace report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleDoc {
    /// The bundle's name (the profile's reference into the store).
    pub name: String,
    /// The rows this bundle inserts.
    #[serde(default)]
    pub rows: Vec<Row>,
}

impl BundleDoc {
    /// Parse a bundle from YAML.
    pub fn load(yaml: &str) -> crate::error::Result<Self> {
        decode(parse(yaml)).map_err(|e| {
            crate::error::ConfigError::new(
                crate::error::Stage::Compose,
                "bad-bundle",
                format!("invalid bundle document: {e}"),
            )
        })
    }

    /// The same content as a composition layer, named by this bundle.
    pub fn as_layer(&self) -> Layer {
        Layer::rows(self.name.clone(), self.rows.clone())
    }

    /// Serialize as YAML.
    ///
    /// # Panics
    /// Only if the document holds a value YAML cannot represent.
    pub fn dump(&self) -> String {
        serde_yaml::to_string(self).expect("bundle document is plain YAML data")
    }
}

/// A stored profile: an ordered bundle list plus its own rows and patches.
///
/// The profile is the user-facing document. `bundles` is ordered and its
/// order *is* precedence (later bundles outrank earlier); `patch` is the
/// profile-level patch, which outranks every bundle but loses to the
/// home-level patch and per-run overlays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSpec {
    /// The profile name.
    pub name: String,
    /// Bundles to compose, lowest precedence first.
    #[serde(default)]
    pub bundles: Vec<String>,
    /// Rows the profile itself declares, applied above all bundles.
    #[serde(default)]
    pub rows: Vec<Row>,
    /// The profile-level patch.
    #[serde(default)]
    pub patch: Vec<PatchOp>,
    /// The base system-prompt directive, when the profile carries one.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// The model row id this profile expects, when it names one.
    #[serde(default)]
    pub model: Option<Value>,
}

impl ProfileSpec {
    /// Parse a profile from YAML.
    pub fn load(yaml: &str) -> crate::error::Result<Self> {
        decode(parse(yaml)).map_err(|e| {
            crate::error::ConfigError::new(
                crate::error::Stage::Compose,
                "bad-profile",
                format!("invalid profile document: {e}"),
            )
        })
    }

    /// Serialize as YAML.
    ///
    /// # Panics
    /// Only if the document holds a value YAML cannot represent.
    pub fn dump(&self) -> String {
        serde_yaml::to_string(self).expect("profile spec is plain YAML data")
    }
}

/// The composed configuration document: what boot mounts and what
/// `--dump-config` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDoc {
    /// The composed profile's name.
    pub name: String,
    /// The composed plugin rows, in mount order.
    #[serde(default)]
    pub rows: Vec<Row>,
    /// The base system-prompt directive, if any.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// The model selection carried by the profile, if any.
    #[serde(default)]
    pub model: Option<Value>,
}

impl ConfigDoc {
    /// The row with this id, if present.
    pub fn row(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }

    /// Serialize through the boot serializer — the bytes `--dump-config`
    /// prints.
    ///
    /// # Panics
    /// Only if the document holds a value YAML cannot represent; every field
    /// is plain data, so this is unreachable for composed documents.
    pub fn dump(&self) -> String {
        serde_yaml::to_string(self).expect("config document is plain YAML data")
    }

    /// Parse a composed document from YAML, rejecting unknown fields so a
    /// dump from a newer build fails loudly instead of mounting silently.
    pub fn load(yaml: &str) -> crate::error::Result<Self> {
        decode(parse(yaml)).map_err(|e| {
            crate::error::ConfigError::new(
                crate::error::Stage::Compose,
                "bad-config",
                format!("invalid config document: {e}"),
            )
        })
    }
}

/// Decode plain YAML data into `T`, surfacing serde_yaml's typed error as a
/// human string (the caller attaches the stage and code).
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> serde_yaml::Result<T> {
    serde_yaml::from_value(value)
}

/// Parse a plain-data YAML document, with an empty file mapped to `null` so
/// callers can share one decode path.
fn parse(yaml: &str) -> Value {
    let text = yaml.trim();
    if text.is_empty() {
        return Value::Null;
    }
    serde_yaml::from_str(text).unwrap_or_else(|e| {
        // A document that fails to parse is reported by the caller's typed
        // error; surface it as a null value so the decode error names the
        // shape problem rather than panicking here.
        let _ = e;
        Value::Null
    })
}

/// Recognise a field-wise profile patch and translate it to row operations.
///
/// A patch document whose keys are profile fields (`model`, `system_prompt`,
/// `tools`, `seams`, `name`) is the shape a user writes when they think in
/// profile terms rather than row terms. Translating it here — rather than
/// deep-merging it later — keeps one patch algorithm: each named field becomes
/// a whole-config `set` on the row that carries that field, so restating is
/// still required and the composition stays readable.
///
/// Returns `None` when any key is not a known profile field, so a typo'd key
/// still surfaces as a typed `bad-patch` error instead of a silent no-op.
fn field_patch(value: &Value) -> Option<Vec<PatchOp>> {
    let map = value.as_mapping()?;
    let mut ops = Vec::new();
    for (key, val) in map {
        let field = key.as_str()?;
        match field {
            "model" => ops.push(PatchOp::Set {
                id: MODEL_ROW_KEY.to_string(),
                config: val.clone(),
            }),
            "system_prompt" | "tools" | "seams" | "name" => {
                // Document-level fields ride as a whole-document override:
                // they are not rows, so they cannot be a `set` on one.
                return None;
            }
            _ => return None,
        }
    }
    (!ops.is_empty()).then_some(ops)
}

/// The first decode error for a mapping layer, for the `bad-patch` message.
fn first_decode_error(value: &Value) -> String {
    decode::<PatchOp>(value.clone())
        .err()
        .or_else(|| decode::<Layer>(value.clone()).err())
        .map(|e| e.to_string())
        .unwrap_or_else(|| "unrecognised shape".to_string())
}

/// The YAML node kind name for error messages.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Sequence(_) => "sequence",
        Value::Mapping(_) => "mapping",
        _ => "tagged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_doc_round_trips() {
        let doc = ConfigDoc {
            name: "default".to_string(),
            rows: vec![
                Row {
                    id: "model".to_string(),
                    plugin: "llm-replay".to_string(),
                    config: serde_yaml::from_str("provider: openai").unwrap(),
                },
                Row::new("spine", "spine"),
            ],
            system_prompt: Some("be terse".to_string()),
            model: None,
        };
        let text = doc.dump();
        assert_eq!(ConfigDoc::load(&text).expect("reloads"), doc);
        assert_eq!(ConfigDoc::load(&text).unwrap().dump(), text);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let text = "name: x\nrows: []\nbogus: 1\n";
        assert!(ConfigDoc::load(text).is_err());
    }

    #[test]
    fn a_patch_file_is_a_single_set_op() {
        let layer = Layer::load("op: set\nid: model\nconfig:\n  kind: none\n").unwrap();
        assert_eq!(layer.patch.len(), 1);
        assert!(layer.rows.is_empty());
        assert_eq!(
            layer.patch[0],
            PatchOp::Set {
                id: "model".to_string(),
                config: serde_yaml::from_str("kind: none").unwrap(),
            }
        );
    }

    #[test]
    fn a_patch_file_is_a_single_insert_row() {
        let layer = Layer::load("id: extra\nplugin: bash\n").unwrap();
        assert_eq!(layer.rows.len(), 1);
        assert_eq!(layer.rows[0].id, "extra");
    }

    #[test]
    fn a_patch_file_is_a_sequence_of_ops() {
        let layer =
            Layer::load("- op: insert\n  id: a\n  plugin: bash\n- op: set\n  id: b\n  config: 1\n")
                .unwrap();
        assert_eq!(layer.patch.len(), 2);
        assert!(layer.rows.is_empty());
    }

    #[test]
    fn a_malformed_patch_is_a_typed_error() {
        let err = Layer::load("[1, 2, this is not a row").unwrap_err();
        assert_eq!(err.code, "bad-patch");
        assert_eq!(err.stage, crate::error::Stage::Patch);
    }
}
