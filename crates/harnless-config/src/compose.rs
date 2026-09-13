//! Layered composition: bundles → profile patch → home patch → overlays.
//!
//! [`Composer`] turns a profile name into a [`ConfigDoc`] by folding layers
//! in precedence order and then expanding substitutions. The fold is the
//! whole model, and it is deliberately boring:
//!
//! ```text
//! lowest  ──►  bundle layers (in profile order)
//!              profile patch
//!              home patch
//! highest ──►  per-run `--patch` overlays
//! ```
//!
//! Each layer is applied by [`apply_layer`], which is also what a live reload
//! uses, so the algorithm the dump runs is literally the algorithm boot runs
//! — dump-equals-mount is a shared code path, not a test that hopes two
//! implementations agree.
//!
//! # Precedence rules
//!
//! * A layer that declares an **existing** id replaces that row **in place**
//!   (mount order is preserved, so a swap never reorders the pipeline).
//! * A layer that declares a **new** id appends it.
//! * A patch `set` on an existing id replaces that row's **whole** `config`
//!   (never a deep merge — see [`crate::doc::PatchOp`]).
//! * A patch `set` on an **absent** id is a [`Warning`], not an error: the
//!   composition succeeds and reports the skipped target.

use std::collections::BTreeMap;

use serde_yaml::Value;

use crate::doc::{BundleDoc, ConfigDoc, Layer, PatchOp, ProfileSpec, Row};
use crate::error::{ConfigError, Result, Stage};
use crate::subst::Subst;

/// A non-fatal composition observation.
///
/// Warnings are how the model stays usable across profile variation: a patch
/// written against a superset of profiles, or a home patch that targets a row
/// a particular profile dropped, composes fine and says what it skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Stable machine-readable code, e.g. `patch-target-missing`.
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// The result of composing a profile: the document plus what to report.
#[derive(Debug, Clone)]
pub struct Composition {
    /// The composed document — exactly what `--dump-config` prints and what
    /// mount consumes.
    pub doc: ConfigDoc,
    /// Non-fatal observations, in the order the layers produced them.
    pub warnings: Vec<Warning>,
    /// The layer names that contributed, lowest precedence first — the trace
    /// that makes a composed document explainable.
    pub layers: Vec<String>,
}

impl Composition {
    /// The composed document.
    pub fn into_doc(self) -> ConfigDoc {
        self.doc
    }
}

/// A bundle store: name → bundle document.
///
/// A [`DirStore`] reads the filesystem; this trait is the seam tests and
/// embedders use to compose from fixtures without touching disk.
pub trait BundleStore: Send + Sync {
    /// Load the bundle named `name`, or `Ok(None)` when it does not exist.
    fn load(&self, name: &str) -> Result<Option<BundleDoc>>;

    /// Known bundle names, in display order.
    fn names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A profile store: name → profile document.
pub trait ProfileStore: Send + Sync {
    /// Load the profile named `name`, or `Ok(None)` when it does not exist.
    fn load(&self, name: &str) -> Result<Option<ProfileSpec>>;

    /// Known profile names, in display order.
    fn names(&self) -> Vec<String>;
}

/// An in-memory store for both profiles and bundles — the test and embedder
/// fixture, and the shape a compiled-in default set takes.
#[derive(Default)]
pub struct MemoryStore {
    /// Profiles by name.
    pub profiles: BTreeMap<String, ProfileSpec>,
    /// Bundles by name.
    pub bundles: BTreeMap<String, BundleDoc>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a profile.
    pub fn with_profile(mut self, profile: ProfileSpec) -> Self {
        self.profiles.insert(profile.name.clone(), profile);
        self
    }

    /// Add a bundle.
    pub fn with_bundle(mut self, bundle: BundleDoc) -> Self {
        self.bundles.insert(bundle.name.clone(), bundle);
        self
    }
}

impl BundleStore for MemoryStore {
    fn load(&self, name: &str) -> Result<Option<BundleDoc>> {
        Ok(self.bundles.get(name).cloned())
    }

    fn names(&self) -> Vec<String> {
        self.bundles.keys().cloned().collect()
    }
}

impl ProfileStore for MemoryStore {
    fn load(&self, name: &str) -> Result<Option<ProfileSpec>> {
        Ok(self.profiles.get(name).cloned())
    }

    fn names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }
}

/// A filesystem store: `<dir>/profiles/<name>.yml` and
/// `<dir>/bundles/<name>.yml`.
pub struct DirStore {
    /// The config directory holding `profiles/` and `bundles/`.
    pub dir: std::path::PathBuf,
}

impl DirStore {
    /// A store rooted at `dir`.
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory profiles live in.
    pub fn profiles_dir(&self) -> std::path::PathBuf {
        self.dir.join("profiles")
    }

    /// The directory bundles live in.
    pub fn bundles_dir(&self) -> std::path::PathBuf {
        self.dir.join("bundles")
    }

    /// Read `<dir>/<sub>/<name>.yml`, mapping a missing file to `Ok(None)`.
    fn read(&self, sub: &str, name: &str, kind: &'static str) -> Result<Option<String>> {
        let path = self.dir.join(sub).join(format!("{name}.yml"));
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ConfigError::for_plugin(
                Stage::Compose,
                "io-error",
                name.to_string(),
                format!("cannot read {kind} {}: {e}", path.display()),
            )),
        }
    }

    /// Names of the `<sub>` documents available.
    fn list(&self, sub: &str) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.dir.join(sub)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("yml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        names
    }
}

impl BundleStore for DirStore {
    fn load(&self, name: &str) -> Result<Option<BundleDoc>> {
        match self.read("bundles", name, "bundle")? {
            None => Ok(None),
            Some(text) => {
                let doc = BundleDoc::load(&text)?;
                if doc.name != name {
                    return Err(ConfigError::new(
                        Stage::Compose,
                        "bad-bundle",
                        format!(
                            "bundle file {name:?} declares name {:?}; a document's name must \
                             match the file it lives in",
                            doc.name
                        ),
                    ));
                }
                Ok(Some(doc))
            }
        }
    }

    fn names(&self) -> Vec<String> {
        self.list("bundles")
    }
}

impl ProfileStore for DirStore {
    fn load(&self, name: &str) -> Result<Option<ProfileSpec>> {
        match self.read("profiles", name, "profile")? {
            None => Ok(None),
            Some(text) => {
                let doc = ProfileSpec::load(&text)?;
                if doc.name != name {
                    return Err(ConfigError::new(
                        Stage::Compose,
                        "bad-profile",
                        format!(
                            "profile file {name:?} declares name {:?}; a document's name must \
                             match the file it lives in",
                            doc.name
                        ),
                    ));
                }
                Ok(Some(doc))
            }
        }
    }

    fn names(&self) -> Vec<String> {
        self.list("profiles")
    }
}

/// The layered composer: a profile store, a bundle store, a home-level patch,
/// and the substitution context.
///
/// One `Composer` serves both sides of the boot contract: [`compose`](Composer::compose)
/// builds the document, [`dump`](Composer::dump) serializes it, and the CLI's
/// mount consumes it. Because the document is produced by the same fold in
/// both cases, the printed bytes and the mounted state cannot drift.
pub struct Composer {
    profiles: std::sync::Arc<dyn ProfileStore>,
    bundles: std::sync::Arc<dyn BundleStore>,
    home_patch: Vec<Layer>,
    subst: Subst,
}

impl Composer {
    /// A composer over `profiles`/`bundles` with no home patch and the given
    /// substitution context.
    pub fn new(
        profiles: std::sync::Arc<dyn ProfileStore>,
        bundles: std::sync::Arc<dyn BundleStore>,
        subst: Subst,
    ) -> Self {
        Self {
            profiles,
            bundles,
            home_patch: Vec::new(),
            subst,
        }
    }

    /// Set the home-level patch layers (applied above the profile patch,
    /// below per-run overlays).
    pub fn with_home_patch(mut self, layers: Vec<Layer>) -> Self {
        self.home_patch = layers;
        self
    }

    /// Parse a home-level patch from YAML text.
    ///
    /// A malformed home patch is reported here — at load time — rather than
    /// bricking every later composition.
    pub fn with_home_patch_text(self, name: impl Into<String>, yaml: &str) -> Result<Self> {
        let name = name.into();
        let mut layer = Layer::load(yaml)?;
        layer.name = name;
        Ok(self.with_home_patch(vec![layer]))
    }

    /// Profile names this composer knows.
    pub fn profiles(&self) -> Vec<String> {
        self.profiles.names()
    }

    /// Compose the profile named `name`, with per-run `overlays` on top.
    ///
    /// `overlays` are the parsed `--patch` layers, applied in the order
    /// given, above the home patch. Composition never writes to the profile
    /// store: an overlay is a fold input, so a per-run swap leaves the stored
    /// profile bytes untouched.
    pub fn compose(&self, name: &str, overlays: &[Layer]) -> Result<Composition> {
        let spec = self.profiles.load(name)?.ok_or_else(|| {
            ConfigError::for_plugin(
                Stage::Compose,
                "unknown-profile",
                name.to_string(),
                format!(
                    "no profile named {name:?}; available: {}",
                    if self.profiles().is_empty() {
                        "(none)".to_string()
                    } else {
                        self.profiles().join(", ")
                    }
                ),
            )
        })?;
        let mut layers: Vec<Layer> = Vec::new();
        for bundle in &spec.bundles {
            let doc = self.bundles.load(bundle)?.ok_or_else(|| {
                ConfigError::for_plugin(
                    Stage::Compose,
                    "unknown-bundle",
                    bundle.clone(),
                    format!(
                        "profile {name:?} requires bundle {bundle:?}, which is not installed \
                         (known: {})",
                        if self.bundles.names().is_empty() {
                            "(none)".to_string()
                        } else {
                            self.bundles.names().join(", ")
                        }
                    ),
                )
            })?;
            layers.push(doc.as_layer());
        }
        if !spec.rows.is_empty() {
            layers.push(Layer::rows(format!("profile:{name}"), spec.rows.clone()));
        }
        if !spec.patch.is_empty() {
            layers.push(Layer::patch(
                format!("profile:{name}:patch"),
                spec.patch.clone(),
            ));
        }
        layers.extend(self.home_patch.iter().cloned());
        layers.extend(overlays.iter().cloned());
        compose_layers(name, &spec, &layers, &self.subst)
    }

    /// Compose from an explicit profile spec (no profile-store lookup).
    pub fn compose_spec(&self, spec: &ProfileSpec, overlays: &[Layer]) -> Result<Composition> {
        let mut layers: Vec<Layer> = Vec::new();
        for bundle in &spec.bundles {
            let doc = self.bundles.load(bundle)?.ok_or_else(|| {
                ConfigError::for_plugin(
                    Stage::Compose,
                    "unknown-bundle",
                    bundle.clone(),
                    format!("profile {:?} requires bundle {bundle:?}", spec.name),
                )
            })?;
            layers.push(doc.as_layer());
        }
        if !spec.rows.is_empty() {
            layers.push(Layer::rows(
                format!("profile:{}", spec.name),
                spec.rows.clone(),
            ));
        }
        if !spec.patch.is_empty() {
            layers.push(Layer::patch(
                format!("profile:{}:patch", spec.name),
                spec.patch.clone(),
            ));
        }
        layers.extend(self.home_patch.iter().cloned());
        layers.extend(overlays.iter().cloned());
        compose_layers(&spec.name, spec, &layers, &self.subst)
    }

    /// Serialize a composed document through the boot serializer.
    ///
    /// The output reparses to an equal document.
    pub fn dump(&self, doc: &ConfigDoc) -> String {
        doc.dump()
    }

    /// The substitution context this composer expands with.
    pub fn subst(&self) -> &Subst {
        &self.subst
    }
}

/// Fold `layers` (lowest precedence first) into a row set and expand
/// substitutions.
///
/// This is the single patch algorithm: `Composer::compose` and any live
/// reload both call it, which is what makes the dump and the mount agree by
/// construction.
pub fn compose_layers(
    name: &str,
    spec: &ProfileSpec,
    layers: &[Layer],
    subst: &Subst,
) -> Result<Composition> {
    let mut rows: Vec<Row> = Vec::new();
    let mut warnings: Vec<Warning> = Vec::new();
    let mut trace: Vec<String> = Vec::new();
    for layer in layers {
        let label = if layer.name.is_empty() {
            "layer".to_string()
        } else {
            layer.name.clone()
        };
        trace.push(label.clone());
        apply_layer(&mut rows, layer, &label, &mut warnings)?;
    }
    let mut doc = ConfigDoc {
        name: name.to_string(),
        rows,
        system_prompt: spec.system_prompt.clone(),
        model: spec.model.clone(),
    };
    // Substitution runs once, over the composed document, so an expression in
    // any layer sees the same environment the mount would.
    let expanded = subst.expand_value(&to_value(&doc)?)?;
    doc = from_value(expanded)?;
    Ok(Composition {
        doc,
        warnings,
        layers: trace,
    })
}

/// Apply one layer to an accumulated row set, in place.
///
/// Row declarations replace-by-id-in-place or append; patch ops then rewrite
/// whole configs or append rows. A patch op naming an absent id pushes a
/// warning instead of failing.
pub fn apply_layer(
    rows: &mut Vec<Row>,
    layer: &Layer,
    label: &str,
    warnings: &mut Vec<Warning>,
) -> Result<()> {
    for row in &layer.rows {
        match rows.iter().position(|r| r.id == row.id) {
            Some(pos) => rows[pos] = row.clone(),
            None => rows.push(row.clone()),
        }
    }
    for op in &layer.patch {
        match op {
            PatchOp::Set { id, config } => match rows.iter().position(|r| r.id == *id) {
                Some(pos) => rows[pos].config = config.clone(),
                None => warnings.push(Warning {
                    code: "patch-target-missing",
                    message: format!(
                        "patch in layer {label:?} targets row {id:?}, which no lower layer \
                         declares; the patch was skipped"
                    ),
                }),
            },
            PatchOp::Insert { row } => match rows.iter().position(|r| r.id == row.id) {
                Some(pos) => rows[pos] = row.clone(),
                None => rows.push(row.clone()),
            },
        }
    }
    Ok(())
}

/// The `Value` form of a document, for substitution.
fn to_value(doc: &ConfigDoc) -> Result<Value> {
    serde_yaml::to_value(doc).map_err(|e| {
        ConfigError::new(Stage::Substitute, "bad-config", format!("config is not YAML: {e}"))
    })
}

/// The document form of a substituted `Value`.
fn from_value(value: Value) -> Result<ConfigDoc> {
    serde_yaml::from_value(value).map_err(|e| {
        ConfigError::new(
            Stage::Substitute,
            "bad-config",
            format!("substituted config does not compose: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Row;

    fn row(id: &str, plugin: &str, config: Value) -> Row {
        Row {
            id: id.to_string(),
            plugin: plugin.to_string(),
            config,
        }
    }

    fn cfg(pairs: &[(&str, &str)]) -> Value {
        let mut map = serde_yaml::Mapping::new();
        for (k, v) in pairs {
            map.insert(Value::String((*k).to_string()), Value::String((*v).to_string()));
        }
        Value::Mapping(map)
    }

    fn bundle(name: &str, rows: Vec<Row>) -> BundleDoc {
        BundleDoc {
            name: name.to_string(),
            rows,
        }
    }

    fn spec(name: &str, bundles: &[&str]) -> ProfileSpec {
        ProfileSpec {
            name: name.to_string(),
            bundles: bundles.iter().map(|b| b.to_string()).collect(),
            rows: Vec::new(),
            patch: Vec::new(),
            system_prompt: None,
            model: None,
        }
    }

    #[test]
    fn later_bundle_layers_outrank_earlier_ones() {
        let store = std::sync::Arc::new(
            MemoryStore::new()
                .with_bundle(bundle(
                    "base",
                    vec![row("model", "replay", cfg(&[("provider", "openai")]))],
                ))
                .with_bundle(bundle(
                    "openai",
                    vec![row("model", "openai", cfg(&[("provider", "openai")]))],
                ))
                .with_profile(spec("default", &["base", "openai"])),
        );
        let composer = Composer::new(store.clone(), store, Subst::new());
        let doc = composer.compose("default", &[]).unwrap().doc;
        assert_eq!(doc.row("model").unwrap().plugin, "openai");
        // In-place replacement keeps mount position.
        assert_eq!(doc.rows.len(), 1);
    }

    #[test]
    fn precedence_is_bundles_then_profile_patch_then_home_then_overlay() {
        let store = std::sync::Arc::new(
            MemoryStore::new()
                .with_bundle(bundle("b", vec![row("model", "from-bundle", Value::Null)]))
                .with_profile(ProfileSpec {
                    name: "default".to_string(),
                    bundles: vec!["b".to_string()],
                    patch: vec![PatchOp::Set {
                        id: "model".to_string(),
                        config: cfg(&[("v", "profile-patch")]),
                    }],
                    ..spec("default", &["b"])
                }),
        );
        let composer = Composer::new(store.clone(), store, Subst::new())
            .with_home_patch_text("home", "op: set\nid: model\nconfig:\n  v: home-patch\n")
            .unwrap();
        let doc = composer.compose("default", &[]).unwrap().doc;
        assert_eq!(doc.row("model").unwrap().config["v"], "home-patch");
        let overlay = Layer::load("op: set\nid: model\nconfig:\n  v: overlay\n").unwrap();
        let doc = composer.compose("default", &[overlay]).unwrap().doc;
        assert_eq!(doc.row("model").unwrap().config["v"], "overlay");
    }

    #[test]
    fn a_patch_set_replaces_the_whole_config_not_a_deep_merge() {
        let store = std::sync::Arc::new(
            MemoryStore::new()
                .with_bundle(bundle(
                    "b",
                    vec![row("model", "openai", cfg(&[("a", "1"), ("b", "2")]))],
                ))
                .with_profile(spec("default", &["b"])),
        );
        let composer = Composer::new(store.clone(), store, Subst::new());
        let overlay = Layer::load("op: set\nid: model\nconfig:\n  b: 9\n").unwrap();
        let doc = composer.compose("default", &[overlay]).unwrap().doc;
        let config = &doc.row("model").unwrap().config;
        assert_eq!(config["b"], serde_yaml::Value::Number(9.into()));
        // `a` is gone: whole-config replacement, restating is required.
        assert!(
            config.get("a").is_none(),
            "deep merge would have kept it: {config:?}"
        );
    }

    #[test]
    fn a_patch_on_an_absent_id_warns_and_still_composes() {
        let store = std::sync::Arc::new(
            MemoryStore::new()
                .with_bundle(bundle("b", vec![row("model", "openai", Value::Null)]))
                .with_profile(spec("default", &["b"])),
        );
        let composer = Composer::new(store.clone(), store, Subst::new());
        let overlay = Layer::load("op: set\nid: ghost\nconfig:\n  a: 1\n").unwrap();
        let out = composer.compose("default", &[overlay]).unwrap();
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].code, "patch-target-missing");
        assert!(out.warnings[0].message.contains("ghost"));
        assert_eq!(out.doc.rows.len(), 1);
    }

    #[test]
    fn an_overlay_swaps_one_row_without_touching_the_profile() {
        let store = std::sync::Arc::new(
            MemoryStore::new()
                .with_bundle(bundle("b", vec![row("model", "openai", Value::Null)]))
                .with_profile(spec("default", &["b"])),
        );
        let composer = Composer::new(store.clone(), store.clone(), Subst::new());
        let overlay = Layer::load("op: set\nid: model\nconfig:\n  script: /tmp/x.json\n").unwrap();
        let doc = composer.compose("default", &[overlay]).unwrap().doc;
        assert_eq!(doc.row("model").unwrap().config["script"], "/tmp/x.json");
        // The stored profile is unchanged — an overlay is a fold input.
        let stored = ProfileStore::load(&*store, "default").unwrap().unwrap();
        assert!(stored.patch.is_empty());
    }

    #[test]
    fn a_malformed_overlay_is_reported_and_the_last_good_document_stands() {
        let store = std::sync::Arc::new(
            MemoryStore::new()
                .with_bundle(bundle("b", vec![row("model", "openai", Value::Null)]))
                .with_profile(spec("default", &["b"])),
        );
        let composer = Composer::new(store.clone(), store, Subst::new());
        let good = composer.compose("default", &[]).unwrap().doc;
        let broken = Layer::load("op: set\nid: model\nthis is not: valid yaml: [");
        assert!(broken.is_err());
        // The good document is still mountable — nothing was mutated in place.
        assert_eq!(composer.compose("default", &[]).unwrap().doc, good);
    }

    #[test]
    fn unknown_profile_and_unknown_bundle_are_named_errors() {
        let store = std::sync::Arc::new(MemoryStore::new());
        let composer = Composer::new(store.clone(), store, Subst::new());
        let err = composer.compose("nope", &[]).unwrap_err();
        assert_eq!(err.code, "unknown-profile");
        assert_eq!(err.stage, Stage::Compose);
        assert!(err.names_plugin("nope"));

        let store = std::sync::Arc::new(MemoryStore::new().with_profile(spec("default", &["gone"])));
        let composer = Composer::new(store.clone(), store, Subst::new());
        let err = composer.compose("default", &[]).unwrap_err();
        assert_eq!(err.code, "unknown-bundle");
        assert!(err.names_plugin("gone"));
    }

    #[test]
    fn substitution_runs_over_the_composed_document() {
        let store = std::sync::Arc::new(
            MemoryStore::new()
                .with_bundle(bundle(
                    "b",
                    vec![Row {
                        id: "store".to_string(),
                        plugin: "storage-jsonl".to_string(),
                        config: cfg(&[("dir", "${home}/state")]),
                    }],
                ))
                .with_profile(spec("default", &["b"])),
        );
        let subst = Subst::new().with_home("/home/u");
        let composer = Composer::new(store.clone(), store, subst);
        let doc = composer.compose("default", &[]).unwrap().doc;
        assert_eq!(doc.row("store").unwrap().config["dir"], "/home/u/state");
    }

    #[test]
    fn a_bad_expression_fails_in_the_substitute_stage() {
        let store = std::sync::Arc::new(
            MemoryStore::new()
                .with_bundle(bundle(
                    "b",
                    vec![Row {
                        id: "store".to_string(),
                        plugin: "x".to_string(),
                        config: cfg(&[("dir", "${eval:1}"),]),
                    }],
                ))
                .with_profile(spec("default", &["b"])),
        );
        let composer = Composer::new(store.clone(), store, Subst::new());
        let err = composer.compose("default", &[]).unwrap_err();
        assert_eq!(err.stage, Stage::Substitute);
        assert_eq!(err.code, "unknown-substitution");
    }

    #[test]
    fn the_layer_trace_names_every_contributing_layer() {
        let store = std::sync::Arc::new(
            MemoryStore::new()
                .with_bundle(bundle("a", vec![]))
                .with_bundle(bundle("b", vec![]))
                .with_profile(spec("default", &["a", "b"])),
        );
        let composer = Composer::new(store.clone(), store, Subst::new());
        let out = composer.compose("default", &[]).unwrap();
        assert_eq!(out.layers, vec!["a", "b"]);
    }
}
