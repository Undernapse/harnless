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

use harnless_config::compose::{Composer, Composition, ProfileStore, Warning};
use harnless_config::doc::{BundleDoc, ConfigDoc, Layer, ProfileSpec, Row};
use harnless_config::error::ConfigError;
use harnless_config::subst::Subst;
use harnless_config::{MountedResource, PluginRegistry};
use harnless_runtime::context::Context;
use harnless_runtime::plugin::Registry;

use crate::boot::{BootComposer, Mounted, ToolsWiring};
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
    /// The mounted tool registry, when the profile's plan declared tools.
    pub tools: Option<Arc<harnless_agent::tools::ToolRegistry>>,
    /// The spine's registry — kept alive so its services stay mounted.
    _registry: Arc<Registry>,
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
        let plan = model_plan(id, config)?;
        let model = build_adapter(&plan).map_err(|e| e.message)?;
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
    // A `tool` row is plan content the spine's wiring acts on, not a service
    // this registry builds — but the row must still *mount*, so the name
    // resolves to an inert resource. The tool bodies themselves are
    // registered by `crate::boot::register_builtins` on the spine's
    // pipeline; a declared name with no built-in body is a named mount
    // failure there, never a silently unregistered tool.
    registry.register_fn(TOOL_PLUGIN, |id: &str, _config| {
        Ok(InertResource { id: id.to_string() })
    });
    registry
}

/// A mounted row that contributes no service: the placeholder a `tool` row
/// resolves to, so the row validates and mounts while the spine's wiring
/// owns the actual tool registration.
struct InertResource {
    id: String,
}

impl MountedResource for InertResource {
    fn id(&self) -> &str {
        &self.id
    }

    fn dispose(&mut self) {}

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
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
            if let Some(doc) = harnless_config::compose::BundleStore::load(&disk, name)? {
                return Ok(Some(doc));
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

/// A composed configuration with the profile name it was composed for and
/// the spine mount it produced, if any.
///
/// `mount` checks the name against the plan it was handed before trusting
/// the stored document, so a `mount` can never silently mount a *different
/// profile's* rows. The spine rides along because mounting it is not
/// idempotent: a second `mount_config` over the same plan would re-mount a
/// mounted one reuses that composition instead of mounting another.
#[derive(Clone)]
struct Entry {
    name: String,
    doc: ConfigDoc,
    spine: Option<Arc<SpineMount>>,
}

/// A live spine composition: the mounted context, its registry, the model
/// and tool registry the rows composed, and the fiber that owns the spine's
/// registrations.
///
/// Disposal is explicit and idempotent: dropping the last handle (or
/// `Mounted::shutdown`) disposes the guard chain — row resources and the
/// spine's fiber unwind — in reverse mount order. Until then the
/// composition stays live for whoever holds it.
pub struct SpineMount {
    ctx: Context,
    registry: Arc<Registry>,
    fiber: Arc<harnless_runtime::Fiber>,
    model: Option<ModelHandle>,
    tools: Option<Arc<harnless_agent::tools::ToolRegistry>>,
    guard: std::sync::Mutex<harnless_config::MountGuard>,
}

impl SpineMount {
    /// Tear the composition down: dispose every row resource and unwind the
    /// spine's fiber. Idempotent.
    fn dispose(&self) {
        self.guard.lock().expect("guard lock").dispose();
        self.registry.unmount(&self.fiber);
    }
}

impl Drop for SpineMount {
    fn drop(&mut self) {
        self.dispose();
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
    /// The most recent composition's full document and the profile name it
    /// was composed for, so `mount` — the boot half of the seam — mounts the
    /// *rows* the dump printed rather than a spine-only re-composition of
    /// the plan, and refuses to mount a document composed for a different
    /// profile. `compose` writes it; `mount` reads it and checks the name.
    last_config: parking_lot::Mutex<Option<Entry>>,
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
        let mut home_warning = None;
        let composer = match home_patch {
            Some(text) => match Composer::new(store.clone(), store.clone(), subst.clone())
                .with_home_patch_text("home", text)
            {
                Ok(composer) => composer,
                Err(err) => {
                    // Routed through the crate's own warning channel — a
                    // stable code, not prose a shell must pattern-match.
                    let warning = Warning {
                        code: "home-patch-ignored",
                        message: format!(
                            "home patch ignored; the profile boots without it — {}",
                            cli_error(err).message
                        ),
                    };
                    eprintln!("warning: {warning}");
                    home_warning = Some(warning);
                    Composer::new(store.clone(), store, subst)
                }
            },
            None => Composer::new(store.clone(), store, subst),
        };
        let mut last_warnings = Vec::new();
        if let Some(warning) = home_warning {
            last_warnings.push(warning);
        }
        Self {
            composer,
            plugins: default_plugins(),
            specs: PluginSpecs::built_in(),
            last_warnings: parking_lot::Mutex::new(last_warnings),
            last_config: parking_lot::Mutex::new(None),
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
        // A new composition replaces the old entry. If the old entry's spine
        // was handed to a live `Mounted`, that handle owns its disposal; the
        // entry drops its own reference, and a later `mount` composes a
        // fresh spine for the new document.
        *self.last_config.lock() = Some(Entry {
            name: out.doc.name.clone(),
            doc: out.doc.clone(),
            spine: None,
        });
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
    /// what the rows *are*, not what they are called. A malformed model
    /// row is a typed failure here too — `mount_config` refuses the same
    /// row, and a dump that hid the defect would diverge from what boot
    /// does with it (dump-equals-mount covers error semantics as well as
    /// bytes).
    pub fn plan(&self, doc: &ConfigDoc) -> Result<ProfileDoc, CliError> {
        use harnless_config::doc::{DOC_PLUGIN_PREFIX, SYSTEM_PROMPT_ROW_KEY, TOOLS_ROW_KEY};
        let mut seams = Vec::new();
        let mut tools = Vec::new();
        let mut model = ModelSpec::None;
        // A field-wise patch's `system_prompt:` outranks the profile's own
        // document field, exactly as a later row outranks an earlier one.
        let mut system_prompt = doc.system_prompt.clone();
        for row in &doc.rows {
            // A field-wise patch's document rows are content, not services:
            // they project onto the plan's fields and never mount.
            if row.plugin.starts_with(DOC_PLUGIN_PREFIX) {
                match row.id.as_str() {
                    SYSTEM_PROMPT_ROW_KEY => {
                        // A null value is YAML's "no value": the field
                        // stays unset, exactly as a null-valued key did in
                        // the legacy field-wise merge. A non-string is the
                        // authoring error.
                        match &row.config {
                            serde_yaml::Value::Null => {}
                            serde_yaml::Value::String(value) => {
                                system_prompt = Some(value.clone());
                            }
                            other => {
                                return Err(CliError::new(
                                    "plugin-build-failed",
                                    format!("system_prompt row config must be a string: {other:?}"),
                                ));
                            }
                        }
                    }
                    TOOLS_ROW_KEY => match &row.config {
                        serde_yaml::Value::Null => {}
                        serde_yaml::Value::Sequence(items) => {
                            for item in items {
                                let name = item.as_str().ok_or_else(|| {
                                    CliError::new(
                                        "plugin-build-failed",
                                        format!("tools entries must be strings: {item:?}"),
                                    )
                                })?;
                                tools.push(name.to_string());
                            }
                        }
                        other => {
                            return Err(CliError::new(
                                "plugin-build-failed",
                                format!("tools row config must be a sequence: {other:?}"),
                            ));
                        }
                    },
                    other => {
                        return Err(CliError::new(
                            "unknown-plugin",
                            format!("document row {other:?} is not a known document field"),
                        ));
                    }
                }
                continue;
            }
            match self.specs.seam(&row.plugin) {
                Seam::Spine => {
                    // The spine plugin provides the whole core chain, so its
                    // row stands for the services it mounts.
                    seams.extend(SPINE_SEAMS.iter().map(|s| s.to_string()));
                }
                Seam::Model => {
                    model = model_spec_from_config(&row.config).map_err(|e| {
                        CliError::new(
                            "plugin-build-failed",
                            format!("model row {:?}: {e}", row.id),
                        )
                    })?;
                }
                Seam::Tool => tools.push(tool_name(row)),
                Seam::Unknown => {}
            }
        }
        Ok(ProfileDoc {
            name: doc.name.clone(),
            seams,
            model,
            tools,
            system_prompt: system_prompt.unwrap_or_default(),
        })
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
    /// returned — including the spine's fiber, which an explicit unmount
    /// unwinds (dropping the registry alone does not dispose fibers) — so a
    /// failed boot leaves no half-mounted service holding a context.
    pub fn mount_config(&self, doc: &ConfigDoc) -> Result<CompositionMount, CliError> {
        use harnless_config::doc::{DOC_PLUGIN_PREFIX, SYSTEM_PROMPT_ROW_KEY, TOOLS_ROW_KEY};
        let ctx = Context::root();
        let mut guard = harnless_config::MountGuard::new();
        let mut spine: Option<Arc<Registry>> = None;
        let mut model: Option<ModelHandle> = None;
        let mut spine_tools: Option<Arc<harnless_agent::tools::ToolRegistry>> = None;
        // The wiring is a property of the plan, computed once — the same
        // projection `plan()` publishes, so the mounted loop and the dumped
        // plan cannot disagree about whether tools were declared.
        let wiring = self.tools_wiring(&self.plan(doc)?)?;
        for row in &doc.rows {
            // A field-wise patch's document rows are plan content, not
            // services — but they are validated here exactly as `plan`
            // validates them, so a document that mounts is a document the
            // plan projection accepts (dump-equals-mount's error clause).
            if row.plugin.starts_with(DOC_PLUGIN_PREFIX) {
                if !matches!(row.id.as_str(), SYSTEM_PROMPT_ROW_KEY | TOOLS_ROW_KEY) {
                    guard.dispose();
                    return Err(CliError::new(
                        "unknown-plugin",
                        format!("document row {:?} is not a known document field", row.id),
                    ));
                }
                continue;
            }
            match self.specs.seam(&row.plugin) {
                Seam::Spine => {
                    // The CLI's boot owns the loop composition: the spine
                    // mounts wired per the plan's tool declarations, so a
                    // profile that declared tools gets the registry-backed
                    // loop as its `AgentLoop` service.
                    let registry = Arc::new(Registry::new());
                    let (tools, fiber) =
                        match crate::boot::mount_spine(&ctx, &registry, wiring.clone()) {
                            Ok(mounted) => mounted,
                            Err(err) => {
                                guard.dispose();
                                return Err(err);
                            }
                        };
                    // The spine's fiber needs an explicit unwind: dropping
                    // the registry does not dispose mounted fibers, so a
                    // later row failure must unmount it or the spine's
                    // services stay live on a context nobody owns.
                    guard.push(Box::new(SpineUnwind {
                        registry: Arc::clone(&registry),
                        fiber: Arc::clone(&fiber),
                    }));
                    spine = Some(Arc::clone(&registry));
                    spine_tools = tools;
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
        // A plan that declared tools and a mount that wired none is a named
        // failure, never an absent Option: `Mounted.tools == None` is the
        // runner's signal to project no schemas, which must mean "no tools
        // declared", not "the spine row was missing".
        if !matches!(wiring, crate::boot::ToolsWiring::None) && spine_tools.is_none() {
            guard.dispose();
            return Err(CliError::new(
                "mount-failed",
                "the plan declared tools but no spine row mounted a tool pipeline",
            ));
        }
        Ok(CompositionMount {
            ctx,
            model,
            tools: spine_tools,
            _registry: registry,
            // The caller owns the guard: dropping it (or `dispose`) unwinds
            // the row resources. A composition that never hands the guard
            // to a live owner disposes here, not never.
            _guard: guard,
        })
    }

    /// Mount a composition and capture its spine as a reusable handle.
    ///
    /// The returned `SpineMount` owns the disposal chain: dropping the last
    /// handle (or `Mounted::shutdown`) unwinds the row resources and the
    /// spine's fiber. A `mount` through the seam reuses the handle stored on
    /// the entry rather than mounting a second spine.
    pub(crate) fn mount_spine_for(&self, doc: &ConfigDoc) -> Result<SpineMount, CliError> {
        let CompositionMount {
            ctx,
            model,
            tools,
            _registry,
            _guard,
        } = self.mount_config(doc)?;
        // The spine's fiber is the registry's mount; a composition without
        // one mounted no spine row, which the seam cannot boot.
        let fiber =
            _registry.fibers().into_iter().last().ok_or_else(|| {
                CliError::new("mount-failed", "composition mounted no spine fiber")
            })?;
        Ok(SpineMount {
            ctx,
            registry: _registry,
            fiber,
            model,
            tools,
            guard: std::sync::Mutex::new(_guard),
        })
    }
}

/// A mounted spine's unwind handle, held as a guard resource.
///
/// Dropping a [`Registry`] does not dispose its mounted fibers, so the
/// composition records the unmount explicitly: a failed boot disposes
/// this like any other row's resource, and a successful boot leaves it
/// inert until the mount itself ends.
struct SpineUnwind {
    registry: Arc<Registry>,
    fiber: Arc<harnless_runtime::Fiber>,
}

impl harnless_config::MountedResource for SpineUnwind {
    fn id(&self) -> &str {
        "spine"
    }

    fn dispose(&mut self) {
        // Idempotent: unmounting an unknown fiber is a no-op.
        self.registry.unmount(&self.fiber);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
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
        Ok(self.plan(&doc)?)
    }

    fn dump(&self, doc: &ProfileDoc) -> String {
        doc.dump()
    }

    fn tools_wiring(&self, doc: &ProfileDoc) -> Result<ToolsWiring, CliError> {
        if doc.tools.is_empty() {
            Ok(ToolsWiring::None)
        } else {
            Ok(ToolsWiring::AutoAllow {
                declared: doc.tools.clone(),
            })
        }
    }

    fn mount(&self, doc: &ProfileDoc) -> Result<Mounted, CliError> {
        // The boot half mounts the rows the dump printed *when the stored
        // composition is the one this plan projects* — identity is checked,
        // not assumed. A stale or foreign `last_config` re-composes the
        // plan's own name; a composition that fails is a named failure,
        // never a silent reference fallback. The stored composition's spine
        // is *reused*, not re-mounted: mounting is not idempotent, and the
        // composition `compose` produced is the one the dump describes.
        let stored = self.last_config.lock().clone();
        let entry = match stored {
            Some(entry) if entry.name == doc.name => Some(entry),
            // The overlays `compose` applied are not recoverable from the
            // plan alone, so a foreign entry re-composes the named profile
            // from the stored layers. A composition that fails here fails
            // the mount — dump-equals-mount's error clause says a profile
            // that cannot compose cannot boot either.
            _ => {
                let composition = self.compose_full(&doc.name, &[])?;
                Some(Entry {
                    name: composition.doc.name.clone(),
                    doc: composition.doc,
                    spine: None,
                })
            }
        };
        let entry = entry.expect("compose_full returns or errors");
        let spine = match entry.spine {
            Some(spine) => spine,
            None => {
                let spine = Arc::new(self.mount_spine_for(&entry.doc)?);
                // The composition's spine is now live; the entry hands it to
                // every later `mount` of the same plan instead of mounting a
                // second spine onto the service map.
                *self.last_config.lock() = Some(Entry {
                    name: entry.name.clone(),
                    doc: entry.doc.clone(),
                    spine: Some(Arc::clone(&spine)),
                });
                spine
            }
        };
        // A plan whose model differs from the rows' model (a field-wise
        // patch swapped it) mounts the plan's model, never the rows'.
        let model = if model_matches(doc, spine.model.as_ref()) {
            spine.model.clone()
        } else {
            build_adapter(doc)?
        };
        Ok(Mounted {
            ctx: spine.ctx.clone(),
            _registry: Arc::clone(&spine.registry),
            tools: spine.tools.clone(),
            _spine: Some(spine as Arc<dyn std::any::Any + Send + Sync>),
            model,
            ids: crate::boot::Ids::new(),
        })
    }
}

/// Whether a composed model handle is the one a plan names.
///
/// The identity that matters at this boundary is the plan's `ModelSpec` in
/// full: a plan that names no provider must not mount a row-composed
/// adapter; a plan whose replay provider *or golden script* differs from the
/// rows' must mount its own. A patch that swaps only `model.script` swaps
/// the mounted adapter, never silently keeps the rows' golden.
fn model_matches(doc: &ProfileDoc, mounted: Option<&ModelHandle>) -> bool {
    match (&doc.model, mounted) {
        (ModelSpec::None, None) => true,
        // A plan with no `script:` names the built-in demo corpus, which an
        // adapter built without a golden path reports as `""`.
        (ModelSpec::Replay { provider, script }, Some(handle)) => {
            handle.provider() == provider && handle.script_id() == script.as_deref().unwrap_or("")
        }
        _ => false,
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
            .compose(
                "default",
                Some("op: set\nid: model\nconfig:\n  kind: none\n"),
            )
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
        std::fs::write(
            dir.join("profiles/store.yml"),
            "name: store\nbundles:\n- store\n",
        )
        .unwrap();
        let composer = ConfigComposer::with_dir_and_subst(
            Some(dir.clone()),
            Subst::new().with_home("/home/tester"),
            None,
        );
        let doc = composer.compose_config("store", &[]).unwrap();
        assert_eq!(
            doc.row("store").unwrap().config["dir"],
            "/home/tester/state"
        );
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
        let composer = ConfigComposer::with_dir_and_subst(Some(dir.clone()), Subst::new(), None);
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
        let doc = composer
            .compose("default", None)
            .expect("last good config boots");
        assert!(doc.has_model());
    }

    #[test]
    fn a_home_patch_outranks_the_profile_but_loses_to_an_overlay() {
        let composer = ConfigComposer::built_in_with_home_patch(Some(
            "op: set\nid: model\nconfig:\n  kind: replay\n  provider: from-home\n",
        ));
        let doc = composer.compose_config("default", &[]).unwrap();
        assert_eq!(
            doc.row("model").unwrap().config["provider"],
            serde_yaml::Value::String("from-home".to_string())
        );
        let overlays = parse_overlays(Some(
            "op: set\nid: model\nconfig:\n  kind: replay\n  provider: from-overlay\n",
        ))
        .unwrap();
        let doc = composer.compose_config("default", &overlays).unwrap();
        assert_eq!(
            doc.row("model").unwrap().config["provider"],
            serde_yaml::Value::String("from-overlay".to_string())
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
        let plan = composer.plan(&doc).expect("valid plan");
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

    /// A field-wise `--patch` overlay's document rows must project onto the
    /// plan's `system_prompt`/`tools` fields — the legacy `--patch tools:…`
    /// contract — and never reach the mount as services.
    #[test]
    fn a_field_wise_overlay_projects_onto_the_plan() {
        let composer = composer();
        let overlays = parse_overlays(Some("system_prompt: be terse\ntools:\n- echo\n")).unwrap();
        let doc = composer.compose_config("default", &overlays).unwrap();
        let plan = composer.plan(&doc).expect("plan projects doc rows");
        assert_eq!(plan.system_prompt, "be terse");
        assert_eq!(plan.tools, vec!["echo"]);
        // The doc rows never appear as seams and never mount.
        assert_eq!(plan.seams, SPINE_SEAMS);
        let mounted = composer
            .mount_config(&doc)
            .expect("doc rows are inert at mount");
        assert!(mounted.ctx.get::<harnless_agent::AgentLoop>().is_some());
    }

    /// Dump-equals-mount's error-semantics clause: plan and `mount_config`
    /// must agree about which documents are valid. A null-valued doc field
    /// (`--patch system_prompt:`) means "unset" — the profile's own value
    /// survives — and an unknown `doc:`-namespaced row is an
    /// `unknown-plugin` failure in both halves, never plan-only.
    #[test]
    fn plan_and_mount_agree_on_document_rows() {
        let composer = composer();
        let default_prompt = composer
            .compose_config("default", &[])
            .and_then(|doc| composer.plan(&doc))
            .map(|plan| plan.system_prompt)
            .unwrap();
        let nulls = parse_overlays(Some("system_prompt:\ntools:\n")).unwrap();
        let doc = composer.compose_config("default", &nulls).unwrap();
        let plan = composer.plan(&doc).expect("a null doc field is unset");
        assert_eq!(plan.system_prompt, default_prompt);
        assert!(composer.mount_config(&doc).is_ok());

        let mystery = vec![harnless_config::doc::Layer::rows(
            "mystery",
            vec![Row::new("mystery", "doc:mystery")],
        )];
        let doc = composer.compose_config("default", &mystery).unwrap();
        let plan_err = composer.plan(&doc).unwrap_err();
        assert_eq!(plan_err.code, "unknown-plugin");
        let mount_err = composer
            .mount_config(&doc)
            .err()
            .expect("an unknown doc: row fails mount too");
        assert_eq!(mount_err.code, "unknown-plugin");
    }
}
