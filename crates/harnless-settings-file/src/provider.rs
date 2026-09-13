//! The file-backed settings provider: ordered layers over pluggable
//! document sources.
//!
//! Design points that carry the contract:
//!
//! * **Sources are read through per operation.** A layer holds a
//!   [`LayerSource`], not a parsed document. File sources are read, parsed,
//!   and resolved on every `get`, so editing a layer file on disk reaches
//!   the next call with no restart and no cache invalidation. The parse is
//!   cheap and the documents are small; correctness (rotation, live edits)
//!   beats a cache with a staleness bug.
//! * **Declaration vs. value.** A namespace/key exists when *some* layer
//!   declares it — either implicitly (the document carries a value at
//!   `ns.key`) or explicitly (a `declare` entry with no value). Resolution
//!   of a declared-but-valueless key is `Ok(None)`: the schema is real, the
//!   value is simply unset. Undeclared coordinates are also `Ok(None)` —
//!   absent is absent either way — but `has_namespace` distinguishes them.
//! * **Precedence is positional.** The layer list is precedence: index 0 is
//!   the shipped composition base, the last layer is the user layer. `get`
//!   scans from the top (last) downward and the first layer that both
//!   declares the key and carries a value wins.

use std::path::{Path, PathBuf};

use harnless_seams::error::{ErrorCode, SeamError};
use harnless_seams::settings::{Namespace, RedactedDescriptor, Settings};
use serde_json::Value;

/// Where one settings layer's raw document comes from.
///
/// This is the provider-swap decision point: replacing a source changes
/// *storage* (a file becomes an embedded base, a test fixture becomes a
/// real path) while the layer list — and with it the resolution order —
/// stays exactly as constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerSource {
    /// A raw YAML or JSON document file. YAML is a superset of JSON here:
    /// the file is parsed as YAML, which accepts both spellings.
    File(PathBuf),
    /// An in-memory raw document (the shipped composition base, a test
    /// fixture, or anything a swap wants to hand in directly).
    Doc(Value),
}

impl LayerSource {
    /// A layer backed by a document file at `path`.
    pub fn file(path: impl AsRef<Path>) -> Self {
        LayerSource::File(path.as_ref().to_path_buf())
    }

    /// A layer backed by an in-memory document.
    pub fn doc(document: Value) -> Self {
        LayerSource::Doc(document)
    }

    /// Read and parse the layer's raw document.
    ///
    /// A missing file is *not* an error: it yields an empty document, so an
    /// absent user layer leaves the base layers fully in effect (the usual
    /// "user has no settings file yet" state). An unreadable-but-present
    /// file or an unparseable document is `io-error` — a corrupt settings
    /// file must surface, not silently resolve against nothing.
    pub fn read(&self) -> harnless_seams::error::Result<Value> {
        match self {
            LayerSource::Doc(doc) => Ok(doc.clone()),
            LayerSource::File(path) => match std::fs::read_to_string(path) {
                Ok(text) => serde_yaml::from_str::<Value>(&text).map_err(|e| {
                    SeamError::new(
                        ErrorCode::IoError,
                        format!("settings layer {} is not valid YAML/JSON: {e}", path.display()),
                    )
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
                Err(e) => Err(SeamError::new(
                    ErrorCode::IoError,
                    format!("reading settings layer {}: {e}", path.display()),
                )),
            },
        }
    }
}

/// A file-backed [`Settings`] provider over ordered layers.
///
/// Construct with [`SettingsFile::new`], passing layers lowest-precedence
/// first (shipped base beneath user layer). Extra key declarations the
/// documents do not carry can be added with [`SettingsFile::declare`].
#[derive(Debug)]
pub struct SettingsFile {
    /// Precedence order: index 0 is the lowest layer (shipped base).
    layers: Vec<LayerSource>,
    /// Explicit declarations: namespace → declared keys (with or without a
    /// value anywhere). Supplements the implicit declarations documents
    /// carry.
    declared: parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>,
}

impl SettingsFile {
    /// Build a provider over `layers`, lowest-precedence first.
    ///
    /// The last layer outranks all others; with a shipped composition base
    /// first and a user file last, the user layer wins key-by-key while
    /// base-only keys stay visible.
    pub fn new(layers: Vec<LayerSource>) -> Self {
        Self {
            layers,
            declared: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// A provider over a single in-memory document (simplest composition).
    pub fn from_doc(document: Value) -> Self {
        Self::new(vec![LayerSource::doc(document)])
    }

    /// Declare `key` as part of namespace `ns`'s schema even when no layer
    /// carries a value for it. Declared keys resolve `Ok(None)` until some
    /// layer supplies a value; the namespace counts as present.
    pub fn declare(&self, ns: &Namespace, key: &str) {
        let mut declared = self.declared.lock();
        let keys = declared.entry(ns.0.clone()).or_default();
        if !keys.iter().any(|k| k == key) {
            keys.push(key.to_string());
        }
    }

    /// The effective document of a layer, or the resolution error it
    /// produced. Errors propagate to the caller — a corrupt layer is a
    /// typed failure, never a silent skip.
    fn layer_doc(&self, layer: &LayerSource) -> harnless_seams::error::Result<Value> {
        layer.read()
    }

    /// Whether `ns`/`key` is declared by any layer (explicitly or by
    /// carrying a value at that coordinate).
    fn is_declared(
        &self,
        docs: &[Value],
        ns: &Namespace,
        key: &str,
    ) -> harnless_seams::error::Result<bool> {
        {
            let declared = self.declared.lock();
            if let Some(keys) = declared.get(&ns.0) {
                if keys.iter().any(|k| k == key) {
                    return Ok(true);
                }
            }
        }
        Ok(docs.iter().any(|doc| lookup(doc, &ns.0, key).is_some()))
    }

    /// Whether any layer declares namespace `ns` at all.
    fn namespace_declared(&self, docs: &[Value], ns: &Namespace) -> bool {
        if let Some(keys) = self.declared.lock().get(&ns.0) {
            if !keys.is_empty() {
                return true;
            }
        }
        docs.iter()
            .any(|doc| doc.get(&ns.0).map_or(false, |v| v.is_object()))
    }
}

/// `ns.key` lookup in a raw document: the namespace must be an object and
/// carry `key`.
fn lookup<'a>(doc: &'a Value, ns: &str, key: &str) -> Option<&'a Value> {
    doc.get(ns)?.get(key)
}

/// The value-free summary of a resolved value: its JSON type only.
///
/// Never derived from content — `"string"`, `"integer"`, `"float"`,
/// `"boolean"`, `"array"`, `"object"`, or `"present"` for `null`.
fn summarize(value: &Value) -> String {
    match value {
        Value::Null => "present".to_string(),
        Value::Bool(_) => "boolean".to_string(),
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer".to_string(),
        Value::Number(_) => "float".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Array(_) => "array".to_string(),
        Value::Object(_) => "object".to_string(),
    }
}

impl Settings for SettingsFile {
    fn get(&self, ns: &Namespace, key: &str) -> harnless_seams::error::Result<Option<Value>> {
        let docs: Vec<Value> = self
            .layers
            .iter()
            .map(|l| self.layer_doc(l))
            .collect::<harnless_seams::error::Result<_>>()?;
        if !self.is_declared(&docs, ns, key)? {
            return Ok(None);
        }
        // Top-down: the last (highest) layer wins.
        for doc in docs.iter().rev() {
            if let Some(value) = lookup(doc, &ns.0, key) {
                return Ok(Some(value.clone()));
            }
        }
        Ok(None)
    }

    fn describe(
        &self,
        ns: &Namespace,
        key: &str,
    ) -> harnless_seams::error::Result<Option<RedactedDescriptor>> {
        // The descriptor mirrors resolution exactly: same declaration gate,
        // same value, summarized by type only.
        match self.get(ns, key)? {
            None => Ok(None),
            Some(value) => Ok(Some(RedactedDescriptor {
                key: key.to_string(),
                present: true,
                summary: summarize(&value),
            })),
        }
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        let Ok(docs) = self
            .layers
            .iter()
            .map(|l| self.layer_doc(l))
            .collect::<harnless_seams::error::Result<Vec<Value>>>()
        else {
            // Fail closed: a corrupt layer cannot prove a namespace
            // ABSENT, and reporting absent would claim an absence the
            // damage cannot rule out. Report present (the real error
            // surfaces through `get`/`describe`, which propagate it).
            return true;
        };
        self.namespace_declared(&docs, ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ns(name: &str) -> Namespace {
        Namespace(name.to_string())
    }

    fn doc(pairs: &[(&str, serde_json::Map<String, Value>)]) -> Value {
        let mut root = serde_json::Map::new();
        for (n, keys) in pairs {
            root.insert((*n).to_string(), Value::Object(keys.clone()));
        }
        Value::Object(root)
    }

    fn keys(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn later_layer_outranks_earlier() {
        // Layered order: shipped base beneath the user layer.
        let base = doc(&[("app", keys(&[("theme", json!("dark")), ("locale", json!("en"))]))]);
        let user = doc(&[("app", keys(&[("theme", json!("light"))]))]);
        let settings = SettingsFile::new(vec![LayerSource::doc(base), LayerSource::doc(user)]);
        assert_eq!(
            settings.get(&ns("app"), "theme").unwrap(),
            Some(json!("light")),
            "user layer must outrank the shipped base"
        );
        assert_eq!(
            settings.get(&ns("app"), "locale").unwrap(),
            Some(json!("en")),
            "base-only keys stay visible beneath the user layer"
        );
    }

    #[test]
    fn file_layer_reads_through_per_operation() {
        // A provider swap changes storage: a file layer is read per get,
        // so editing the file reaches the next call with no restart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.yml");
        std::fs::write(&path, "app:\n  theme: light\n").unwrap();
        let base = doc(&[("app", keys(&[("theme", json!("dark"))]))]);
        let settings =
            SettingsFile::new(vec![LayerSource::doc(base), LayerSource::file(&path)]);
        assert_eq!(settings.get(&ns("app"), "theme").unwrap(), Some(json!("light")));
        std::fs::write(&path, "app:\n  theme: solarized\n").unwrap();
        assert_eq!(settings.get(&ns("app"), "theme").unwrap(), Some(json!("solarized")));
    }

    #[test]
    fn json_document_parses_as_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.json");
        std::fs::write(&path, r#"{"app":{"theme":"dark"}}"#).unwrap();
        let settings = SettingsFile::new(vec![LayerSource::file(&path)]);
        assert_eq!(settings.get(&ns("app"), "theme").unwrap(), Some(json!("dark")));
    }

    #[test]
    fn missing_user_file_leaves_base_in_effect() {
        let base = doc(&[("app", keys(&[("theme", json!("dark"))]))]);
        let settings = SettingsFile::new(vec![
            LayerSource::doc(base),
            LayerSource::file("/nonexistent/harnless-user-settings.yml"),
        ]);
        assert_eq!(settings.get(&ns("app"), "theme").unwrap(), Some(json!("dark")));
    }

    #[test]
    fn corrupt_layer_is_typed_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yml");
        std::fs::write(&path, "\t: : [unclosed").unwrap();
        let settings = SettingsFile::new(vec![LayerSource::file(&path)]);
        let err = settings.get(&ns("app"), "theme").unwrap_err();
        assert_eq!(err.code, ErrorCode::IoError);
    }

    #[test]
    fn corrupt_layer_fails_has_namespace_closed() {
        // A damaged layer cannot prove a namespace absent: has_namespace
        // must report present (fail closed) rather than claim an absence
        // the damage cannot rule out. The real error surfaces through get.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yml");
        std::fs::write(&path, "\t: : [unclosed").unwrap();
        let settings = SettingsFile::new(vec![LayerSource::file(&path)]);
        assert!(
            settings.has_namespace(&ns("anything")),
            "a corrupt layer must not report any namespace absent"
        );
    }

    #[test]
    fn unknown_namespace_and_key_are_ok_none() {
        let settings = SettingsFile::from_doc(doc(&[("app", keys(&[("theme", json!("dark"))]))]));
        assert_eq!(settings.get(&ns("nope"), "theme").unwrap(), None);
        assert_eq!(settings.get(&ns("app"), "nope").unwrap(), None);
        assert!(!settings.has_namespace(&ns("nope")));
        assert!(settings.has_namespace(&ns("app")));
    }

    #[test]
    fn declared_key_without_value_resolves_none_but_namespace_exists() {
        let settings = SettingsFile::from_doc(Value::Object(serde_json::Map::new()));
        settings.declare(&ns("app"), "theme");
        assert_eq!(settings.get(&ns("app"), "theme").unwrap(), None);
        assert!(settings.has_namespace(&ns("app")));
        // Once a layer carries the value, resolution picks it up.
        let filled = SettingsFile::new(vec![LayerSource::doc(doc(&[(
            "app",
            keys(&[("theme", json!("42"))]),
        )]))]);
        filled.declare(&ns("app"), "theme");
        assert_eq!(filled.get(&ns("app"), "theme").unwrap(), Some(json!("42")));
    }

    #[test]
    fn describe_is_redacted_by_type_only() {
        // Redaction: summary derives from the JSON type, never content.
        let settings = SettingsFile::from_doc(doc(&[(
            "app",
            keys(&[
                ("token", json!("hunter2-super-secret")),
                ("retries", json!(3)),
                ("ratio", json!(0.5)),
                ("verbose", json!(true)),
                ("nothing", Value::Null),
            ]),
        )]));
        let d = settings.describe(&ns("app"), "token").unwrap().unwrap();
        assert_eq!((d.key.as_str(), d.present), ("token", true));
        assert_eq!(d.summary, "string");
        assert!(!d.summary.contains("hunter2"));
        assert_eq!(settings.describe(&ns("app"), "retries").unwrap().unwrap().summary, "integer");
        assert_eq!(settings.describe(&ns("app"), "ratio").unwrap().unwrap().summary, "float");
        assert_eq!(settings.describe(&ns("app"), "verbose").unwrap().unwrap().summary, "boolean");
        assert_eq!(settings.describe(&ns("app"), "nothing").unwrap().unwrap().summary, "present");
        // describe mirrors resolution for absent coordinates.
        assert!(settings.describe(&ns("app"), "ghost").unwrap().is_none());
    }

    #[test]
    fn describe_mirrors_layered_resolution() {
        let base = doc(&[("app", keys(&[("secret", json!("base-value"))]))]);
        let user = doc(&[("app", keys(&[("secret", json!("user-value"))]))]);
        let settings = SettingsFile::new(vec![LayerSource::doc(base), LayerSource::doc(user)]);
        let d = settings.describe(&ns("app"), "secret").unwrap().unwrap();
        assert!(d.present);
        assert_eq!(d.summary, "string");
        assert!(!d.summary.contains("base-value") && !d.summary.contains("user-value"));
    }

}
