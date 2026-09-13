//! The config-driven boot: a [`harnless_config`] composition behind the CLI's
//! [`BootComposer`] seam.
//!
//! This is the wire point the previous `PARENT-WIRE` comment marked. The
//! [`ConfigComposer`] replaces the built-in [`DefaultComposer`](crate::boot::DefaultComposer)
//! in the binary: composition, dumping, and mounting all flow through one
//! [`harnless_config::Composer`] plus a [`harnless_config::PluginRegistry`],
//! so `hrls --dump-config` prints the *same* [`ConfigDoc`] that `mount`
//! consumes — same parser, same patch fold, same substitution pass.
//! Dump-equals-mount is a shared code path, not two implementations that tests
//! hope agree.
//!
//! # The two document shapes
//!
//! [`ConfigDoc`] is the general composition: plugin rows with opaque config,
//! which is what bundles, patches, and overlays edit.
//! [`ProfileDoc`](crate::profile::ProfileDoc) is the CLI's *mount plan*: the
//! seams, model, tools, and prompt a composed configuration actually mounts.
//! [`ConfigComposer::plan`] translates one to the other, and the translation
//! is driven by the rows' **seam kind** ([`Seam`]) rather than by row id, so
//! renaming a row never changes what it mounts.
//!
//! # Boot failure discipline
//!
//! A failure names the plugin row and the stage it happened in
//! ([`harnless_config::Stage`]), and a partially-mounted composition is
//! disposed before the error surfaces. The codes are stable strings a shell
//! routes on: `unknown-profile`, `unknown-bundle`, `unknown-plugin`,
//! `plugin-build-failed`, `bad-patch`, `unknown-substitution`.

use std::sync::Arc;

use harnless_agent::spine::Spine;
use harnless_config::compose::{Composer, Composition, ProfileStore, Warning};
use harnless_config::doc::{BundleDoc, ConfigDoc, Layer, ProfileSpec, Row};
use harnless_config::error::ConfigError;
use harnless_config::subst::Subst;
use harnless_config::{MountedResource, PluginRegistry};
use harnless_runtime::context::Context;
use harnless_runtime::plugin::Registry;
use harnless_seams::SessionId;

use crate::boot::{BootComposer, Mounted};
use crate::model::{build_adapter, ModelHandle};
use crate::profile::{ModelSpec, ProfileDoc};
use crate::CliError;

/// The config crate, re-exported so callers (and tests) name composition
/// types without taking a direct dependency on `harnless-config`.
pub use harnless_config as config;

/// The plugin implementing the agent spine row.
pub const SPINE_PLUGIN: &str = "spine";
/// The plugin implementing the network-free replay model.
pub const REPLAY_PLUGIN: &str = "llm-replay";
/// The plugin for a profile that wires no model provider.
pub const NO_MODEL_PLUGIN: &str = "none";
/// The plugin implementing a tool-pipeline row.
pub const TOOL_PLUGIN: &str = "tool";
/// The row id the built-in model composition lives under.
pub const MODEL_ROW: &str = "model";

/// The services the spine plugin provides, in mount order.
///
/// The plan's `seams` list names mounted services, and one spine row provides
/// the whole core chain — so the row projects to the chain it installs, which
/// is what keeps the composed default plan equal to the shipped plan.
pub const SPINE_SEAMS: [&str; 4] = ["spine", "session-log", "tool-pipeline", "agent-loop"];

/// What a plugin row contributes to the composition.
///
/// The projection to the CLI's mount plan keys on this, not on row ids: a user
/// who renames their model row from `model` to `brain` still gets a model.
/// A plugin declares its kind by name in [`PluginSpecs`]; an unregistered
/// plugin is `Unknown`, which the mount reports rather than drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seam {
    /// The agent spine: session log, event registry, tool pipeline, loop.
    Spine,
    /// The model provider row.
    Model,
    /// A tool registered on the pipeline.
    Tool,
    /// A plugin the CLI mount does not interpret yet.
    Unknown,
}

/// The plugin-name → [`Seam`] table.
#[derive(Clone, Default)]
pub struct PluginSpecs(std::collections::BTreeMap<String, Seam>);

impl PluginSpecs {
    /// The built-in table.
    pub fn built_in() -> Self {
        let mut specs = Self::default();
        specs.register(SPINE_PLUGIN, Seam::Spine);
        specs.register(REPLAY_PLUGIN, Seam::Model);
        specs.register(NO_MODEL_PLUGIN, Seam::Model);
        specs.register(TOOL_PLUGIN, Seam::Tool);
        specs
    }

    /// Declare which seam `plugin` fills.
    pub fn register(&mut self, plugin: &str, seam: Seam) -> &mut Self {
        self.0.insert(plugin.to_string(), seam);
        self
    }

    /// The seam `plugin` fills.
    pub fn seam(&self, plugin: &str) -> Seam {
        self.0.get(plugin).copied().unwrap_or(Seam::Unknown)
    }
}

/// A composed model provider, mounted as a resource so a later row's failure
/// disposes it.
pub struct ModelResource {
    /// The row this model was composed for.
    pub id: String,
    /// The composed adapter, or `None` for a providerless row.
    pub model: Option<ModelHandle>,
}

impl MountedResource for ModelResource {
    fn id(&self) -> &str {
        &self.id
    }

    fn dispose(&mut self) {
        self.model = None;
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A live composition of the built-in rows: the mounted context, the plugins
/// that mounted it, and the composed model.
///
/// Disposal is reverse mount order: dropping the spine's registry unwinds the
/// services the loop was built from, and the model handle goes with it.
pub struct CompositionMount {
    /// The service context the rows mounted onto.
    pub ctx: Context,
    /// The model composed by the model row, if any.
    pub model: Option<ModelHandle>,
    /// The spine's registry — dropping it unwinds the spine fiber.
    _registry: Registry,
    /// The mount guard that disposes every row resource.
    _guard: harnless_config::MountGuard,
}

/// The plugin registry the config boot mounts through.
///
/// Registered out of the box: [`SPINE_PLUGIN`] (mounts the agent spine),
/// [`REPLAY_PLUGIN`] (composes the network-free replay adapter), and
/// [`NO_MODEL_PLUGIN`] (a providerless composition). An unregistered plugin
/// name is a named `unknown-plugin` mount failure, never a skipped row.
pub fn default_plugins() -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    registry.register_fn(REPLAY_PLUGIN, |id: &str, config| {
        let model = build_adapter(&model_plan(id, config)?).map_err(|e| e.message)?;
        Ok(ModelResource {
            id: id.to_string(),
            model,
        })
    });
    registry.register_fn(NO_MODEL_PLUGIN, |id: &str, _config| {
        Ok(ModelResource {
            id: id.to_string(),
            model: None,
        })
    });
    registry
}

/// A mount plan carrying only a model composition.
fn model_plan(id: &str, config: &serde_yaml::Value) -> Result<ProfileDoc, String> {
    Ok(ProfileDoc {
        name: id.to_string(),
        seams: Vec::new(),
        model: model_spec_from_config(config)?,
        tools: Vec::new(),
        system_prompt: String::new(),
    })
}

/// Read a [`ModelSpec`] out of a model row's opaque config.
///
/// The row's `config` *is* the model composition in profile-document shape
/// (`{kind: replay, provider: openai, script: path}` or `{kind: none}`), so a
/// patch that swaps the model restates this object whole — the same
/// whole-config replacement rule that governs every other row.
pub fn model_spec_from_config(config: &serde_yaml::Value) -> Result<ModelSpec, String> {
    if config.is_null() {
        return Ok(ModelSpec::Replay {
            provider: "openai".to_string(),
            script: None,
        });
    }
    serde_yaml::from_value(config.clone()).map_err(|e| format!("invalid model config: {e}"))
}

/// The home directory `${home}` expands to.
fn default_home() -> Option<String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| h.to_string_lossy().into_owned())
}

/// Translate a [`ConfigError`] into the CLI's machine-routable error.
///
/// The stage is folded into the message so a shell sees
/// `unknown-plugin: mount: … (plugin model)` and routes on the code alone.
pub fn cli_error(err: ConfigError) -> CliError {
    let message = if err.plugin.is_empty() {
        format!("{}: {}", err.stage, err.message)
    } else {
        format!("{}: {} (plugin {})", err.stage, err.message, err.plugin)
    };
    CliError::new(err.code, message)
}

/// Report composition warnings on `out`.
///
/// Warnings never fail a composition — a patch naming an absent id is a
/// portable patch, not a mistake — but they are never silent either.
pub fn report_warnings(warnings: &[Warning], mut out: impl std::io::Write) {
    for warning in warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
}

/// Parse per-run overlay text into layers.
///
/// The text may hold several YAML documents (one `--patch` file each, or one
/// multi-document file); each becomes a layer, in order, so repeated overlays
/// fold in the order they were named. A malformed document is a `bad-patch`
/// failure at the patch stage — the caller keeps the last good document, so a
/// typo in `--patch` never bricks a run that was working a moment ago.
pub fn parse_overlays(patch: Option<&str>) -> Result<Vec<Layer>, CliError> {
    let Some(text) = patch else {
        return Ok(Vec::new());
    };
    let mut layers = Vec::new();
    for (index, doc) in serde_yaml::Deserializer::from_str(text).enumerate() {
        let value = <serde_yaml::Value as serde::Deserialize>::deserialize(doc).map_err(|e| {
            CliError::new(
                "bad-patch",
                format!("patch: patch document {} is not valid YAML: {e}", index + 1),
            )
        })?;
        let layer = Layer::from_value(value).map_err(cli_error)?;
        if !layer.rows.is_empty() || !layer.patch.is_empty() {
            layers.push(layer);
        }
    }
    Ok(layers)
}

/// The built-in bundle and profile set, compiled in.
///
/// The built-in is expressed in the same vocabulary a user edits — a bundle
/// declaring rows, a profile naming it — which is what lets the default
/// profile go through the same fold as a user profile instead of a special
/// case that `--dump-config` could drift from.
pub struct BuiltInStore;

impl BuiltInStore {
    /// The `core` bundle: the agent spine plus the replay model.
    pub fn core_bundle() -> BundleDoc {
        BundleDoc {
            name: "core".to_string(),
            rows: vec![
                Row::new("spine", SPINE_PLUGIN),
                Row {
                    id: MODEL_ROW.to_string(),
                    plugin: REPLAY_PLUGIN.to_string(),
                    config: serde_yaml::Value::Null,
                },
            ],
        }
    }

    /// The built-in `default` profile.
    pub fn default_profile() -> ProfileSpec {
        ProfileSpec {
            name: "default".to_string(),
            bundles: vec!["core".to_string()],
            rows: Vec::new(),
            patch: Vec::new(),
            system_prompt: Some(ProfileDoc::default_profile().system_prompt),
            model: None,
        }
    }

    /// Every built-in profile.
    pub fn profiles() -> Vec<ProfileSpec> {
        vec![Self::default_profile()]
    }

    /// Every built-in bundle.
    pub fn bundles() -> Vec<BundleDoc> {
        vec![Self::core_bundle()]
    }
}

impl ProfileStore for BuiltInStore {
    fn load(&self, name: &str) -> harnless_config::error::Result<Option<ProfileSpec>> {
        Ok(Self::profiles().into_iter().find(|p| p.name == name))
    }

    fn names(&self) -> Vec<String> {
        Self::profiles().iter().map(|p| p.name.clone()).collect()
    }
}

impl harnless_config::compose::BundleStore for BuiltInStore {
    fn load(&self, name: &str) -> harnless_config::error::Result<Option<BundleDoc>> {
        Ok(Self::bundles().into_iter().find(|b| b.name == name))
    }

    fn names(&self) -> Vec<String> {
        Self::bundles().iter().map(|b| b.name.clone()).collect()
    }
}

/// A store that consults a directory first and the built-in set second.
///
/// Drop `profiles/power.yml` into the config directory and it shadows only a
/// same-named built-in; every other built-in profile stays available.
pub struct LayeredStore {
    /// The user directory (`profiles/` and `bundles/` inside it).
    dir: Option<std::path::PathBuf>,
    /// The compiled-in fallback.
    built_in: BuiltInStore,
}

impl LayeredStore {
    /// A layered store over `dir`, falling back to the built-ins.
    pub fn new(dir: Option<std::path::PathBuf>) -> Self {
        Self {
            dir,
            built_in: BuiltInStore,
        }
    }

    /// The directory store, when a directory is configured.
    fn disk(&self) -> Option<harnless_config::compose::DirStore> {
        self.dir
            .as_ref()
            .map(|d| harnless_config::compose::DirStore::new(d))
    }

    /// Merge `disk` names with the built-in names, de-duplicated and sorted.
    fn merge(&self, disk: Vec<String>, built_in: Vec<String>) -> Vec<String> {
        let mut names = disk;
        for name in built_in {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names.sort();
        names
    }
}

impl ProfileStore for LayeredStore {
    fn load(&self, name: &str) -> harnless_config::error::Result<Option<ProfileSpec>> {
        if let Some(disk) = self.disk() {
            if let Some(spec) = ProfileStore::load(&disk, name)? {
                return Ok(Some(spec));
            }
        }
        ProfileStore::load(&self.built_in, name)
    }

    fn names(&self) -> Vec<String> {
        let disk = self
            .disk()
            .map(|d| ProfileStore::names(&d))
            .unwrap_or_default();
        self.merge(disk, ProfileStore::names(&self.built_in))
    }
}

impl harnless_config::compose::BundleStore for LayeredStore {
    fn load(&self, name: &str) -> harnless_config::error::Result<Option<BundleDoc>> {
        if let Some(disk) = self.disk() {
            if let Some(bundle) = harnless_config::compose::BundleStore::load(&disk, name)? {
                return Ok(Some(bundle));
            }
        }
        harnless_config::compose::BundleStore::load(&self.built_in, name)
    }

    fn names(&self) -> Vec<String> {
        let disk = self
            .disk()
            .map(|d| harnless_config::compose::BundleStore::names(&d))
            .unwrap_or_default();
        self.merge(
            disk,
            harnless_config::compose::BundleStore::names(&self.built_in),
        )
    }
}

/// The config-driven composer: the boot seam implemented over a
/// [`harnless_config::Composer`].
///
/// One value serves both sides of the contract: [`compose`](BootComposer::compose)
/// and [`dump`](BootComposer::dump) are the offline half, [`mount`](BootComposer::mount)
/// the live half, and both read the same fold.
pub struct ConfigComposer {
    composer: Composer,
    plugins: PluginRegistry,
    specs: PluginSpecs,
    /// Warnings from the most recent composition, so the binary reports them
    /// once instead of threading them through every call.
    last_warnings: parking_lot::Mutex<Vec<Warning>>,
}

impl ConfigComposer {
    /// A composer over `store` with the given substitution context.
    pub fn new(store: Arc<LayeredStore>, subst: Subst) -> Self {
        Self::with_home_patch(store, subst, None)
    }

    /// A composer with a home-level patch document applied.
    ///
    /// A malformed home patch is *reported*, not fatal: the last good
    /// configuration (no home layer) keeps booting, which is the difference
    /// between a typo in `~/.config/harnless/patch.yml` and a bricked harness.
    pub fn with_home_patch(
        store: Arc<LayeredStore>,
        subst: Subst,
        home_patch: Option<&str>,
    ) -> Self {
        let composer = match home_patch {
            Some(text) => match Composer::new(store.clone(), store.clone(), subst.clone())
                .with_home_patch_text("home", text)
            {
                Ok(composer) => composer,
                Err(err) => {
                    eprintln!("warning: home patch ignored — {}", cli_error(err));
                    Composer::new(store.clone(), store, subst)
                }
            },
            None => Composer::new(store.clone(), store, subst),
        };
        Self {
            composer,
            plugins: default_plugins(),
            specs: PluginSpecs::built_in(),
            last_warnings: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// The built-in composition with no config directory.
    pub fn built_in() -> Self {
        Self::new(
            Arc::new(LayeredStore::new(None)),
            Subst::from_env(default_home()),
        )
    }

    /// The built-in composition plus a home patch document.
    pub fn built_in_with_home_patch(home_patch: Option<&str>) -> Self {
        Self::with_home_patch(
            Arc::new(LayeredStore::new(None)),
            Subst::from_env(default_home()),
            home_patch,
        )
    }

    /// A composer reading profiles and bundles from `dir`, falling back to the
    /// built-ins.
    pub fn with_dir(dir: impl Into<std::path::PathBuf>, home_patch: Option<&str>) -> Self {
        Self::with_home_patch(
            Arc::new(LayeredStore::new(Some(dir.into()))),
            Subst::from_env(default_home()),
            home_patch,
        )
    }

    /// A composer with an explicit substitution context and config directory.
    pub fn with_dir_and_subst(
        dir: Option<std::path::PathBuf>,
        subst: Subst,
        home_patch: Option<&str>,
    ) -> Self {
        Self::with_home_patch(Arc::new(LayeredStore::new(dir)), subst, home_patch)
    }

    /// Override the plugin-seam table (embedders registering new plugins).
    pub fn with_specs(mut self, specs: PluginSpecs) -> Self {
        self.specs = specs;
        self
    }

    /// Override the plugin registry.
    pub fn with_plugins(mut self, plugins: PluginRegistry) -> Self {
        self.plugins = plugins;
        self
    }

    /// Warnings from the most recent composition.
    pub fn warnings(&self) -> Vec<Warning> {
        self.last_warnings.lock().clone()
    }

    /// Compose the full configuration document, with warnings and trace.
    pub fn compose_full(&self, name: &str, overlays: &[Layer]) -> Result<Composition, CliError> {
        let out = self.composer.compose(name, overlays).map_err(cli_error)?;
        *self.last_warnings.lock() = out.warnings.clone();
        Ok(out)
    }

    /// Compose the full configuration document for `name`.
    ///
    /// This is what `--dump-config` prints in full-configuration mode; the
    /// mount plan is its [`plan`](Self::plan) projection.
    pub fn compose_config(&self, name: &str, overlays: &[Layer]) -> Result<ConfigDoc, CliError> {
        Ok(self.compose_full(name, overlays)?.doc)
    }

    /// Project a composed configuration onto the CLI's mount plan.
    ///
    /// Rows are classified by [`PluginSpecs`], so the plan is a function of
    /// what the rows *are*, not what they are called.
    pub fn plan(&self, doc: &ConfigDoc) -> ProfileDoc {
        let mut seams = Vec::new();
        let mut tools = Vec::new();
        let mut model = ModelSpec::None;
        for row in &doc.rows {
            match self.specs.seam(&row.plugin) {
                Seam::Spine => {
                    // The spine plugin provides the whole core chain, so its
                    // row stands for the services it mounts.
                    seams.extend(SPINE_SEAMS.iter().map(|s| s.to_string()));
                }
                Seam::Model => {
                    model = model_spec_from_config(&row.config).unwrap_or(ModelSpec::None);
                }
                Seam::Tool => tools.push(tool_name(row)),
                Seam::Unknown => {}
            }
        }
        ProfileDoc {
            name: doc.name.clone(),
            seams,
            model,
            tools,
            system_prompt: doc.system_prompt.clone().unwrap_or_default(),
        }
    }

    /// The plugin registry this composer mounts through.
    pub fn plugins(&self) -> &PluginRegistry {
        &self.plugins
    }

    /// The seam table this composer projects with.
    pub fn specs(&self) -> &PluginSpecs {
        &self.specs
    }

    /// The underlying config composer, for embedders that want the raw fold.
    pub fn config(&self) -> &Composer {
        &self.composer
    }

    /// Mount a composed configuration: every row through the plugin registry,
    /// then the spine onto a fresh context.
    ///
    /// Rows mount in composition order. If a row fails, the resources built
    /// for earlier rows are disposed (reverse order) before the error is
    /// returned, so a failed boot leaves no half-mounted service holding a
    /// context.
    pub fn mount_config(&self, doc: &ConfigDoc) -> Result<CompositionMount, CliError> {
        let ctx = Context::root();
        let mut guard = harnless_config::MountGuard::new();
        let mut spine: Option<Registry> = None;
        let mut model: Option<ModelHandle> = None;
        for row in &doc.rows {
            match self.specs.seam(&row.plugin) {
                Seam::Spine => {
                    let registry = Registry::new();
                    registry.mount(&ctx, Arc::new(Spine::new(SessionId(1)))).map_err(|e| {
                        guard.dispose();
                        CliError::new("mount-failed", format!("{}: {}", e.code, e.message))
                    })?;
                    spine = Some(registry);
                }
                Seam::Model | Seam::Tool | Seam::Unknown => {
                    match harnless_config::mount(&one_row(doc, row), &self.plugins) {
                        Ok(mut row_guard) => {
                            // A model row's resource carries the composed
                            // adapter out; everything else rides in the guard.
                            if self.specs.seam(&row.plugin) == Seam::Model {
                                if let Some(resource) = row_guard.find::<ModelResource>() {
                                    model = resource.model.clone();
                                }
                            }
                            guard.absorb(&mut row_guard);
                        }
                        Err(err) => {
                            guard.dispose();
                            return Err(cli_error(err));
                        }
                    }
                }
            }
        }
        let registry = spine.unwrap_or_default();
        Ok(CompositionMount {
            ctx,
            model,
            _registry: registry,
            _guard: guard,
        })
    }
}

/// The tool name a tool row contributes: its config string, else its id.
fn tool_name(row: &Row) -> String {
    match row.config.as_str() {
        Some(name) => name.to_string(),
        None => row.id.clone(),
    }
}

/// A single-row view of a document, so per-row mounting reuses `mount`.
fn one_row(doc: &ConfigDoc, row: &Row) -> ConfigDoc {
    ConfigDoc {
        name: doc.name.clone(),
        rows: vec![row.clone()],
        system_prompt: None,
        model: None,
    }
}

impl BootComposer for ConfigComposer {
    fn profiles(&self) -> Vec<String> {
        self.composer.profiles()
    }

    fn compose(&self, name: &str, patch: Option<&str>) -> Result<ProfileDoc, CliError> {
        let overlays = parse_overlays(patch)?;
        let doc = self.compose_config(name, &overlays)?;
        Ok(self.plan(&doc))
    }

    fn dump(&self, doc: &ProfileDoc) -> String {
        doc.dump()
    }

    fn mount(&self, doc: &ProfileDoc) -> Result<Mounted, CliError> {
        // The mount plan is the projection of a composed configuration, so
        // this is mounting what the dump printed. The spine mounts in its own
        // fiber, and the composed model handle rides along.
        let ctx = Context::root();
        let registry = Registry::new();
        registry
            .mount(&ctx, Arc::new(Spine::new(SessionId(1))))
            .map_err(|e| CliError::new("mount-failed", format!("{}: {}", e.code, e.message)))?;
        let model = build_adapter(doc)?;
        Ok(Mounted {
            ctx,
            _registry: registry,
            model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer() -> ConfigComposer {
        ConfigComposer::built_in()
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hrls-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::create_dir_all(dir.join("bundles")).unwrap();
        dir
    }

    #[test]
    fn the_built_in_profile_composes_to_the_shipped_plan() {
        let composer = composer();
        let doc = composer.compose("default", None).unwrap();
        assert_eq!(doc, ProfileDoc::default_profile());
    }

    #[test]
    fn a_config_patch_overlay_swaps_the_model_row() {
        let composer = composer();
        let doc = composer
            .compose("default", Some("op: set\nid: model\nconfig:\n  kind: none\n"))
            .unwrap();
        assert!(!doc.has_model());
        assert_eq!(
            doc.system_prompt,
            ProfileDoc::default_profile().system_prompt
        );
    }

    #[test]
    fn a_patch_naming_an_absent_row_warns_without_failing() {
        let composer = composer();
        let out = composer
            .compose_full(
                "default",
                &[Layer::load("op: set\nid: ghost\nconfig: 1\n").unwrap()],
            )
            .unwrap();
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(out.warnings[0].code, "patch-target-missing");
        assert!(out.warnings[0].message.contains("ghost"));
    }

    #[test]
    fn an_unknown_plugin_row_names_the_row_at_the_mount_stage() {
        let doc = ConfigDoc {
            name: "t".to_string(),
            rows: vec![Row::new("weird", "not-installed")],
            system_prompt: None,
            model: None,
        };
        let err = harnless_config::mount(&doc, composer().plugins()).unwrap_err();
        assert_eq!(err.code, "unknown-plugin");
        assert_eq!(err.stage, harnless_config::Stage::Mount);
        assert!(err.names_plugin("weird"));
    }

    #[test]
    fn a_directory_profile_overrides_and_adds_to_the_built_in() {
        let dir = temp("dir");
        std::fs::write(
            dir.join("bundles/tiny.yml"),
            "name: tiny\nrows:\n- id: spine\n  plugin: spine\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("profiles/tiny.yml"),
            "name: tiny\nbundles:\n- tiny\nsystem_prompt: be terse\n",
        )
        .unwrap();
        let composer = ConfigComposer::with_dir(&dir, None);
        assert!(composer.profiles().contains(&"tiny".to_string()));
        assert!(composer.profiles().contains(&"default".to_string()));
        let doc = composer.compose("tiny", None).unwrap();
        assert_eq!(doc.system_prompt, "be terse");
        assert!(!doc.has_model());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn substitution_expands_home_in_a_composed_document() {
        let dir = temp("home");
        std::fs::write(
            dir.join("bundles/store.yml"),
            "name: store\nrows:\n- id: store\n  plugin: storage-jsonl\n  config:\n    dir: ${home}/state\n",
        )
        .unwrap();
        std::fs::write(dir.join("profiles/store.yml"), "name: store\nbundles:\n- store\n").unwrap();
        let composer = ConfigComposer::with_dir_and_subst(
            Some(dir.clone()),
            Subst::new().with_home("/home/tester"),
            None,
        );
        let doc = composer.compose_config("store", &[]).unwrap();
        assert_eq!(doc.row("store").unwrap().config["dir"], "/home/tester/state");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_expression_is_a_substitute_stage_error() {
        let dir = temp("badsub");
        std::fs::write(
            dir.join("bundles/b.yml"),
            "name: b\nrows:\n- id: x\n  plugin: spine\n  config:\n    k: ${eval:1}\n",
        )
        .unwrap();
        std::fs::write(dir.join("profiles/b.yml"), "name: b\nbundles:\n- b\n").unwrap();
        let composer =
            ConfigComposer::with_dir_and_subst(Some(dir.clone()), Subst::new(), None);
        let err = composer.compose_config("b", &[]).unwrap_err();
        assert_eq!(err.code, "unknown-substitution");
        assert!(err.message.starts_with("substitute:"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_overlay_reports_bad_patch_and_the_last_good_plan_stands() {
        let composer = composer();
        let good = composer.compose("default", None).unwrap();
        let err = composer
            .compose("default", Some("op: set\nid: model\nbad: ["))
            .unwrap_err();
        assert_eq!(err.code, "bad-patch");
        assert_eq!(composer.compose("default", None).unwrap(), good);
    }

    #[test]
    fn mounting_a_composed_configuration_yields_a_live_spine_and_model() {
        let composer = composer();
        let doc = composer.compose_config("default", &[]).unwrap();
        let mounted = composer.mount_config(&doc).expect("mount");
        assert!(mounted.ctx.get::<harnless_agent::AgentLoop>().is_some());
        assert!(mounted.model.is_some());
    }

    #[test]
    fn a_model_row_config_reaches_the_composed_adapter() {
        let composer = composer();
        let doc = ConfigDoc {
            name: "t".to_string(),
            rows: vec![Row {
                id: "brain".to_string(),
                plugin: REPLAY_PLUGIN.to_string(),
                config: serde_yaml::from_str("kind: replay\nprovider: myprov\n").unwrap(),
            }],
            system_prompt: None,
            model: None,
        };
        let mounted = composer.mount_config(&doc).unwrap();
        let adapter = mounted.model.expect("model composed");
        assert_eq!(adapter.provider(), "myprov");
    }

    #[test]
    fn a_failing_row_disposes_earlier_rows() {
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut plugins = PluginRegistry::new();
        let recorder = log.clone();
        plugins.register_fn("probe", move |id: &str, _c: &serde_yaml::Value| {
            Ok(Recorder {
                id: id.to_string(),
                log: recorder.clone(),
            })
        });
        plugins.register_fn("boom", |_id: &str, _c: &serde_yaml::Value| {
            let result: Result<Recorder, String> = Err("configured to fail".to_string());
            result
        });
        let doc = ConfigDoc {
            name: "t".to_string(),
            rows: vec![
                Row::new("a", "probe"),
                Row::new("b", "probe"),
                Row::new("c", "boom"),
            ],
            system_prompt: None,
            model: None,
        };
        let err = harnless_config::mount(&doc, &plugins).unwrap_err();
        assert_eq!(err.code, "plugin-build-failed");
        assert!(err.names_plugin("c"));
        assert_eq!(*log.lock(), vec!["dispose:b", "dispose:a"]);
    }

    struct Recorder {
        id: String,
        log: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    impl MountedResource for Recorder {
        fn id(&self) -> &str {
            &self.id
        }
        fn dispose(&mut self) {
            self.log.lock().push(format!("dispose:{}", self.id));
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn a_malformed_home_patch_is_reported_and_the_profile_still_boots() {
        let composer = ConfigComposer::built_in_with_home_patch(Some("op: set\nid: model\nbad: ["));
        let doc = composer.compose("default", None).expect("last good config boots");
        assert!(doc.has_model());
    }

    #[test]
    fn a_home_patch_outranks_the_profile_but_loses_to_an_overlay() {
        let composer = ConfigComposer::built_in_with_home_patch(Some(
            "op: set\nid: model\nconfig:\n  kind: replay\n  provider: from-home\n",
        ));
        let doc = composer.compose("default", None).unwrap();
        assert_eq!(
            doc.model,
            ModelSpec::Replay {
                provider: "from-home".to_string(),
                script: None
            }
        );
        let doc = composer
            .compose(
                "default",
                Some("op: set\nid: model\nconfig:\n  kind: replay\n  provider: from-overlay\n"),
            )
            .unwrap();
        assert_eq!(
            doc.model,
            ModelSpec::Replay {
                provider: "from-overlay".to_string(),
                script: None
            }
        );
    }

    #[test]
    fn the_plan_projection_keys_on_seam_kind_not_row_id() {
        let composer = composer();
        let doc = ConfigDoc {
            name: "t".to_string(),
            rows: vec![
                Row::new("core", SPINE_PLUGIN),
                Row {
                    id: "brain".to_string(),
                    plugin: REPLAY_PLUGIN.to_string(),
                    config: serde_yaml::from_str("kind: replay\nprovider: p\n").unwrap(),
                },
                Row {
                    id: "t1".to_string(),
                    plugin: TOOL_PLUGIN.to_string(),
                    config: serde_yaml::Value::String("bash".to_string()),
                },
            ],
            system_prompt: Some("hi".to_string()),
            model: None,
        };
        let plan = composer.plan(&doc);
        assert_eq!(plan.seams, SPINE_SEAMS);
        assert_eq!(plan.tools, vec!["bash"]);
        assert_eq!(plan.system_prompt, "hi");
        assert_eq!(
            plan.model,
            ModelSpec::Replay {
                provider: "p".to_string(),
                script: None
            }
        );
    }
}
