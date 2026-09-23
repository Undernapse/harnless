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

use harnless_config::compose::{Composer, Composition, ProfileStore, Warning};
use harnless_config::doc::{BundleDoc, ConfigDoc, Layer, ProfileSpec, Row};
use harnless_config::error::ConfigError;
use harnless_config::subst::Subst;
use harnless_config::{MountedResource, PluginRegistry};
use harnless_runtime::context::Context;
use harnless_runtime::plugin::Registry;
use std::sync::Arc;

use crate::boot::{BootComposer, Mounted, ToolsWiring};
use crate::model::{build_adapter, ModelHandle};
use crate::profile::{ModelSpec, ProfileDoc, StoreSpec};
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

/// The session-store plugin row's name (#69 §1).
pub const STORAGE_PLUGIN: &str = "storage-jsonl";

/// The plan seam a storage row projects to (#69 §1).
pub const STORAGE_SEAM: &str = "store";

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
    /// The session-store row (#69 §1): mounts the session store, projects
    /// the plan's tail `store:` key.
    Storage,
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
        specs.register(STORAGE_PLUGIN, Seam::Storage);
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
    /// The mounted session store, when a storage row mounted one (#69 §3).
    pub store: Option<Arc<harnless_storage_jsonl::SessionStore>>,
    /// The spine's registry — kept alive so its services stay mounted.
    _registry: Arc<Registry>,
    /// The mount guard that disposes every row resource.
    _guard: harnless_config::MountGuard,
    /// The composition's mirror handle when the spine mounted a store-seeded log;
    /// `Mounted::drop` evicts the service with it (see `Mounted`'s `Drop`).
    mirror: Option<Arc<crate::boot::MirroringLog>>,
}

/// The plugin registry the config boot mounts through.
///
/// Registered out of the box: [`SPINE_PLUGIN`] (mounts the agent spine),
/// [`REPLAY_PLUGIN`] (composes the network-free replay adapter),
/// [`NO_MODEL_PLUGIN`] (a providerless composition), [`TOOL_PLUGIN`] (plan
/// content, inert here), and [`STORAGE_PLUGIN`] (the session store's plan
/// projection; the live *mount* is the CLI's `Seam::Storage` arm, so the
/// registry entry is the inert placeholder that lets a raw
/// `harnless_config::mount` of a store-carrying document validate the row).
/// An unregistered plugin name is a named `unknown-plugin` mount failure,
/// never a skipped row.
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
    registry.register_fn(TOOL_PLUGIN, |id: &str, _config| {
        Ok(InertResource { id: id.to_string() })
    });
    // A `storage-jsonl` row is mounted by the CLI's own `Seam::Storage` arm
    // (#69 §3), not by a resource builder here — but a raw config-crate
    // mount of a store-carrying document must still *validate* the row, so
    // the name resolves to an inert placeholder. The config is validated at
    // build time so a malformed store row fails the same way everywhere.
    registry.register_fn(STORAGE_PLUGIN, |id: &str, config| {
        storage_spec_from_config(config).map_err(|e| format!("invalid store row: {e}"))?;
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
        store: None,
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

/// Parse a storage row's config into the plan's [`StoreSpec`] (#69 §1).
/// A null config is the authoring error — a store row must name its dir.
pub fn storage_spec_from_config(config: &serde_yaml::Value) -> Result<StoreSpec, String> {
    serde_yaml::from_value(config.clone()).map_err(|e| format!("invalid store config: {e}"))
}

/// The home directory `${home}` expands to.
pub(crate) fn default_home() -> Option<String> {
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
///
/// Two built-in bundles: `core` (spine + replay model, sessionless) and
/// `sessions` (#69 §2: the store row the *binary's* default profile names,
/// `${home}/.harnless/sessions`). The reference `ProfileDoc::default_profile`
/// stays sessionless — it is the seam-test fixture and must never touch
/// `$HOME` — while the *composed* default gains durability through the
/// bundle layer.
pub struct BuiltInStore;

impl BuiltInStore {
    /// The `core` bundle: the agent spine plus the replay model, sessionless.
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

    /// The `sessions` bundle (#69 §2): the session-store row a user profile
    /// can name to gain durability — the same row shape the built-in
    /// `default` profile carries as its own row. Row order is spine-then-
    /// store, so the store mounts after the spine and a later row's failure
    /// disposes in reverse mount order with the spine unwinding last.
    pub fn sessions_bundle() -> BundleDoc {
        BundleDoc {
            name: "sessions".to_string(),
            rows: vec![Row {
                id: STORAGE_SEAM.to_string(),
                plugin: STORAGE_PLUGIN.to_string(),
                config: serde_yaml::to_value(StoreSpec {
                    dir: "${home}/.harnless/sessions".to_string(),
                })
                .expect("store spec is plain YAML"),
            }],
        }
    }

    /// The built-in `default` profile: `core` plus the store row — the
    /// binary's default is durable (#69 §2). The *reference* plan
    /// ([`ProfileDoc::default_profile`]) stays sessionless; the composed
    /// plan gains the tail `store:` key through this profile row.
    ///
    /// The store row rides the profile's own rows, not a bundle: a bundle
    /// layer folds as a whole and a profile's rows layer above every bundle,
    /// so the row cannot be shadowed away by a `core`-only fold — and
    /// `LayeredStore`'s same-name shadowing still lets a user directory
    /// replace the whole `default` profile.
    pub fn default_profile() -> ProfileSpec {
        ProfileSpec {
            name: "default".to_string(),
            bundles: vec!["core".to_string()],
            rows: vec![Row {
                id: STORAGE_SEAM.to_string(),
                plugin: STORAGE_PLUGIN.to_string(),
                config: serde_yaml::to_value(StoreSpec {
                    dir: "${home}/.harnless/sessions".to_string(),
                })
                .expect("store spec is plain YAML"),
            }],
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
        vec![Self::core_bundle(), Self::sessions_bundle()]
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
/// a *weak* handle to the spine mount it produced, if any.
///
/// `mount` checks the name against the plan it was handed before trusting
/// the stored document, so a `mount` can never silently mount a *different
/// profile's* rows. The spine rides along weakly because mounting is not
/// idempotent: a live composition is reused, never re-mounted — but the
/// cache must not own it, or disposal could never happen. A cold upgrade
/// re-mounts under the composer's mount lock, so two racing cold mounts
/// cannot each leave a spine mounted on the shared service map.
#[derive(Clone)]
struct Entry {
    name: String,
    doc: ConfigDoc,
    spine: Option<std::sync::Weak<SpineMount>>,
    /// The store dir the cached composition's rows mounted, resolved. The
    /// warm reuse must agree with the plan it is handed (#69 §3): a patch
    /// that moved the store cannot reuse a spine whose storage row names
    /// another dir.
    store_dir: Option<String>,
}

/// A live spine composition: the mounted context, its registry, the model
/// and tool registry the rows composed, and the fiber that owns the spine's
/// registrations.
///
/// Disposal is explicit and idempotent: dropping the last handle disposes
/// the guard chain — row resources and the spine's fiber unwind — in
/// reverse mount order. The composition cache holds only a *weak* reference
/// (see [`ConfigComposer::mount`]), so the last live `Mounted` is the last
/// owner and teardown actually happens.
///
/// The state is **shared** across every `Mounted` of one composition: the
/// log, event registry, tool pipeline, loop, and id allocator are one per
pub(crate) struct SpineMount {
    pub(crate) ctx: Context,
    pub(crate) registry: Arc<Registry>,
    fiber: Arc<harnless_runtime::Fiber>,
    pub(crate) model: Option<ModelHandle>,
    pub(crate) tools: Option<Arc<harnless_agent::tools::ToolRegistry>>,
    /// The wiring the spine was mounted with — the plan's tool declarations
    /// it implements. A warm reuse must agree with the plan on this axis
    /// (see [`ConfigComposer::mount`]); disagreement is a named failure,
    /// never a silently tool-less loop for a plan that declared tools.
    wiring: ToolsWiring,
    /// The composition's id allocator. It rides the *shared* spine, not the
    /// per-`Mounted` copy: every handle of one composition mints off one
    /// counter, so ids stay unique across turns and mounts of the same
    /// session. The log's positions and the loop's ids share no allocator,
    /// so this is the only cross-mount identity source.
    pub(crate) ids: crate::boot::Ids,
    /// The mounted session store, when a storage row mounted one (#69 §3).
    pub(crate) store: Option<Arc<harnless_storage_jsonl::SessionStore>>,
    /// The spine's mirror handle when it mounted a store-seeded log;
    /// every `Mounted` of this spine carries it so the last drop evicts the
    /// service (see `Mounted`'s `Drop`).
    pub(crate) mirror: Option<Arc<crate::boot::MirroringLog>>,
    guard: Arc<std::sync::Mutex<harnless_config::MountGuard>>,
}

impl SpineMount {
    /// Tear the composition down: dispose every row resource, then unwind
    /// the spine's fiber. Idempotent — the guard's latch and the fiber's
    /// state both make a second call a no-op.
    pub(crate) fn dispose(&self) {
        self.guard.lock().expect("guard lock").dispose();
        self.registry.unmount(&self.fiber);
    }

    /// The full failure unwind: dispose the composition *and* close the
    /// session mirror (releasing the flock). The wrapper's Err arm and the
    /// panic guard share this one shape.
    pub(crate) fn unwind_after_failure(&self) {
        self.dispose();
        if let Some(mirror) = self.mirror.as_ref() {
            mirror.close();
        }
    }
}

impl Drop for SpineMount {
    fn drop(&mut self) {
        self.dispose();
    }
}

thread_local! {
    /// The spine the most recent *seeded* mount composed (never a warm
    /// reuse). The `mount_seeded` wrapper disposes it when the body fails
    /// after the spine mounted: the spine's registry entry pins the
    /// plugin, so the body's `Arc<SpineMount>` drop alone never runs
    /// `SpineMount::dispose`, and a live mirror would strand the session
    /// lock for the process's life.
    static LAST_MOUNTED_SPINE: std::sync::Mutex<std::sync::Weak<SpineMount>> =
        const { std::sync::Mutex::new(std::sync::Weak::new()) };
    /// The panic-path unwind slot: a cell the `mount_seeded` wrapper
    /// arms *before* the spine composes (the composition's own row loop
    /// is fallible, and a panic inside it must find the half-published
    /// mirror's flock in the registry slot). A panic past the arm runs
    /// the guard's Drop — the same dispose the wrapper's Err arm performs
    /// — and then the classification: a session file this boot created
    /// abandons, never orphans (#67 §5).
    static SPINE_UNWIND: std::sync::Mutex<UnwindSlot> =
        std::sync::Mutex::new(UnwindSlot::default());
}

/// The panic-path unwind slot's payload: the weak spine handle plus the
/// seed's writer cell and `created_by_mount` flag. The cell classifies a
/// created file when the panic lands *before* the spine's mount consumed
/// it; once the mount consumed the writer, the flag stays set and the
/// guard's Drop classifies through the spine's mirror instead.
#[derive(Default)]
struct UnwindSlot {
    spine: std::sync::Weak<SpineMount>,
    cell: Option<std::sync::Arc<std::sync::Mutex<Option<harnless_storage_jsonl::SessionWriter>>>>,
    created_by_mount: bool,
}

/// Arms the panic-path unwind for a seeded mount. The body takes the
/// spine handle out of the slot when it records a composed spine (the
/// composition's own exits own any unwind inside it); the wrapper's Ok
/// exit clears the whole slot, so a *later* mount's failure can never
/// dispose or classify this mount's healthy spine.
struct SpineUnwindGuard;

impl SpineUnwindGuard {
    /// Take the wrapper's unwind handle for the spine the body just
    /// composed, so the wrapper's exits own the dispose.
    fn disarm() {
        SPINE_UNWIND.with(|g| *g.lock().expect("unwind lock") = UnwindSlot::default());
    }

}

impl Drop for SpineUnwindGuard {
    fn drop(&mut self) {
        // Panic path: dispose the live composition (guard + fiber) and
        // close its mirror, releasing the session lock, then classify the
        // seed's cell — a file this boot created abandons here, never as
        // a phantom the panic leaves behind. The handle is taken *inside*
        // `ManuallyDrop`: a taken `Arc<SpineMount>` whose scope ends
        // normally would run `SpineMount::drop` a second time — on the
        // wrapper's Ok exit that resurrects the healthy spine and disposes
        // it while its `Mounted` still serves. Only the unwind's Drop path
        // gets `ManuallyDrop`'s drop glue.
        let UnwindSlot {
            spine,
            cell,
            created_by_mount,
        } = SPINE_UNWIND.with(|g| std::mem::take(&mut *g.lock().expect("unwind lock")));
        // The writer's home *before* any close decides the route:
        // - still in the shared cell (the mount never consumed it, or
        //   rolled it back): classify through the cell;
        // - consumed into the mirror: the mirror's held id names the
        //   created file. The id is captured at the mirror's construction,
        //   so it survives the close and the dispose below.
        let still_in_cell = cell
            .as_ref()
            .map(|c| c.lock().expect("seed cell").is_some())
            .unwrap_or(false);
        // The strong spine handle: the seam's leaked handle and the
        // wrapper's slot keep the spine alive across the unwind, so the
        // mirror's id and the store's dir are still readable after the
        // dispose.
        let spine_live = spine
            .upgrade()
            .or_else(|| LAST_MOUNTED_SPINE.with(|s| s.lock().expect("probe lock").upgrade()));
        let mirror = spine_live.as_ref().and_then(|s| s.mirror.clone());
        let mirror_id = if !still_in_cell && created_by_mount {
            mirror.as_ref().and_then(|m| m.held_id())
        } else {
            None
        };
        // The store dir the created file lives in, taken *before* the
        // dispose: the abandon names the file by id, so it needs the dir.
        let store_dir = spine_live
            .as_ref()
            .and_then(|s| s.store.as_ref())
            .map(|st| st.dir().to_path_buf());
        if let Some(spine) = spine_live {
            // `ManuallyDrop`'s holder keeps the Arc from running
            // `SpineMount::drop` when this scope ends: only the unwind's
            // Drop path gets drop glue here. The dispose closes the
            // mirror, releasing the flock — every handle to the mirror
            // goes through this one spine, so no writer outlives the
            // abandon below and re-creates the lock sibling.
            let held = std::mem::ManuallyDrop::new(spine);
            held.unwind_after_failure();
        }
        // The classification. The panic unwinds without an error value
        // to return, so a failed abandon is reported, never swallowed —
        // the same shape `unwind_created` prints.
        let outcome = if let Some(cell) = cell.as_ref().filter(|_| still_in_cell) {
            crate::boot::classify_failed_cell(cell, created_by_mount).1
        } else if let Some(id) = mirror_id {
            // The mount consumed the writer into the mirror and the
            // dispose above closed it: the file is this boot's orphan
            // with no writer left to name it. Abandon by id — the by-id
            // shape fences a concurrent opener with the lock probe and
            // is success on a missing file/sibling.
            match store_dir.as_ref() {
                Some(dir) => harnless_storage_jsonl::SessionStore::new(dir).abandon(id),
                // No store row means no file this mount could have named.
                None => Ok(()),
            }
        } else {
            Ok(())
        };
        if let Err(e) = outcome {
            eprintln!(
                "warning: the session file created by this run could not be abandoned: {}: {}",
                e.code, e.message
            );
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The spine the most recent test-path `mount` composed or reused —
    /// a *weak* handle, so the test's own `Mounted` drops remain the last
    /// strong references and the disposal under test actually runs.
    pub(crate) static TEST_SPINES: std::sync::Mutex<Option<std::sync::Weak<SpineMount>>> =
        std::sync::Mutex::new(None);
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
    /// The most recent composition's full document, the profile name it was
    /// composed for, and a weak handle to its live spine, so `mount` — the
    /// boot half of the seam — mounts the *rows* the dump printed, reuses a
    /// live composition instead of re-mounting one, and never owns a spine
    /// the caller has already dropped. `compose` writes it; `mount` reads
    /// and upgrades it under `mount_lock`.
    last_config: parking_lot::Mutex<Option<Entry>>,
    /// Serializes the cold-mount read-upgrade-mount-write sequence so two
    /// racing `mount` calls cannot both mount a spine onto one service map.
    mount_lock: parking_lot::Mutex<()>,
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
            mount_lock: parking_lot::Mutex::new(()),
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
        // Under the mount lock: replacing the entry must never interleave
        // with a cold mount, or a spine mounted for the old document gets
        // published into the new one's entry.
        let _mount_guard = self.mount_lock.lock();
        let out = self.composer.compose(name, overlays).map_err(cli_error)?;
        *self.last_warnings.lock() = out.warnings.clone();
        // A new composition replaces the old entry. The old entry's weak
        // spine dies with the last live `Mounted` that holds it; a later
        // `mount` of the new document composes a fresh spine.
        *self.last_config.lock() = Some(Entry {
            name: out.doc.name.clone(),
            doc: out.doc.clone(),
            spine: None,
            store_dir: None,
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
        let mut store: Option<StoreSpec> = None;
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
                // A storage row projects exactly one plan entry: the plan's
                // tail `store:` key (#69 §1). A second storage row is the
                // authoring error — the plan names one store, and a silent
                // last-row-wins would diverge the dump from what mounts.
                Seam::Storage => {
                    if store.is_some() {
                        return Err(CliError::new(
                            "plugin-build-failed",
                            format!("store row {:?}: the plan names one store", row.id),
                        ));
                    }
                    store = Some(storage_spec_from_config(&row.config).map_err(|e| {
                        CliError::new(
                            "plugin-build-failed",
                            format!("store row {:?}: {e}", row.id),
                        )
                    })?);
                    seams.push(STORAGE_SEAM.to_string());
                }
                Seam::Unknown => {}
            }
        }
        Ok(ProfileDoc {
            name: doc.name.clone(),
            seams,
            model,
            tools,
            system_prompt: system_prompt.unwrap_or_default(),
            store,
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
        self.mount_config_with_seed(doc, crate::boot::MountSeed::default())
    }

    /// [`mount_config`](Self::mount_config) with the spine mounted from
    /// `seed` (#67 §5): the resume/fork route's spine is seeded — stored
    /// records, id floor, mirroring writer — while the model/tool rows stay
    /// sessionless. The spine row's mount consumes the seed exactly once;
    /// the fresh shape (default seed) is byte-identical to the #64 route.
    pub(crate) fn mount_config_with_seed(
        &self,
        doc: &ConfigDoc,
        seed: crate::boot::MountSeed,
    ) -> Result<CompositionMount, CliError> {
        use harnless_config::doc::{DOC_PLUGIN_PREFIX, SYSTEM_PROMPT_ROW_KEY, TOOLS_ROW_KEY};
        let ctx = Context::root();
        let mut guard = harnless_config::MountGuard::new();
        let mut spine: Option<Arc<Registry>> = None;
        let mut spine_unwind: Option<SpineUnwind> = None;
        let mut model: Option<ModelHandle> = None;
        let mut spine_tools: Option<Arc<harnless_agent::tools::ToolRegistry>> = None;
        let mut store: Option<Arc<harnless_storage_jsonl::SessionStore>> = None;
        let mut spine_mirror: Option<Arc<crate::boot::MirroringLog>> = None;
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
                    let (tools, fiber, mirror) =
                        match crate::boot::mount_spine(&ctx, &registry, wiring.clone(), &seed) {
                            Ok(mounted) => mounted,
                            Err(err) => {
                                // Same rule as every other row's failure arm:
                                // rows mounted before the spine dispose in
                                // reverse order, never leak onto a context
                                guard.dispose();
                                return Err(err);
                            }
                        };
                    // The spine's fiber needs an explicit unwind: dropping
                    // the registry does not dispose mounted fibers, so a
                    // later row failure must unmount it or the spine's
                    // services stay live on a context nobody owns. The
                    // unwind handle is held aside and pushed *after* the
                    // row loop, so the guard's teardown order stays
                    // reverse-mount: the spine's fiber unwinds last, after
                    // every row resource it hosts has been disposed.
                    spine_unwind = Some(SpineUnwind {
                        registry: Arc::clone(&registry),
                        fiber: Arc::clone(&fiber),
                    });
                    spine = Some(Arc::clone(&registry));
                    spine_tools = tools;
                    spine_mirror = mirror;
                }
                // A storage row mounts the session store (#69 §3): pure path
                // state, dir created lazily at first write — mounting never
                // touches the filesystem. The row's config rides the same
                // substitution pass every other row's config gets, so
                // `${home}/state` expands here exactly as it does in the
                // dump; an unknown expression is the same named failure.
                Seam::Storage => {
                    let config =
                        match harnless_config::subst::expand(&row.config, self.composer.subst()) {
                            Ok(config) => config,
                            Err(err) => {
                                guard.dispose();
                                if let Some(mirror) = spine_mirror.take() {
                                    // The spine mounted and took the session lock; no
                                    // composition ever owns it. A failed boot unwinds,
                                    // never leaks.
                                    mirror.close();
                                }
                                return Err(cli_error(err));
                            }
                        };
                    match storage_spec_from_config(&config) {
                        Ok(spec) => {
                            store = Some(Arc::new(harnless_storage_jsonl::SessionStore::new(
                                spec.dir,
                            )))
                        }
                        Err(e) => {
                            guard.dispose();
                            if let Some(mirror) = spine_mirror.take() {
                                // The spine mounted and took the session lock; no
                                // composition ever owns it. A failed boot unwinds,
                                // never leaks.
                                mirror.close();
                            }
                            return Err(CliError::new(
                                "plugin-build-failed",
                                format!("store row {:?}: {e}", row.id),
                            ));
                        }
                    }
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
                            if let Some(mirror) = spine_mirror.take() {
                                // The spine mounted and took the session lock; no
                                // composition ever owns it. A failed boot unwinds,
                                // never leaks.
                                mirror.close();
                            }
                            return Err(cli_error(err));
                        }
                    }
                }
            }
        }
        // The spine's unwind handle joins the guard last, so reverse-order
        // teardown disposes every row resource before the spine's fiber
        // unwinds (the documented reverse-mount-order rule). The push is
        // deliberately post-loop: the in-loop error paths already `dispose`
        // belt-and-braces here rather than a live path.
        if let Some(unwind) = spine_unwind {
            guard.push(Box::new(unwind));
        }
        let registry = spine.unwrap_or_default();
        // A plan that declared tools and a mount that wired none is a named
        // failure, never an absent Option: `Mounted.tools == None` is the
        // runner's signal to project no schemas, which must mean "no tools
        // declared", not "the spine row was missing".
        if !matches!(wiring, crate::boot::ToolsWiring::None) && spine_tools.is_none() {
            guard.dispose();
            if let Some(mirror) = spine_mirror.take() {
                // The spine mounted and took the session lock; no
                // composition ever owns it. A failed boot unwinds,
                // never leaks.
                mirror.close();
            }
            return Err(CliError::new(
                "mount-failed",
                "the plan declared tools but no spine row mounted a tool pipeline",
            ));
        }
        Ok(CompositionMount {
            ctx,
            model,
            tools: spine_tools,
            store,
            _registry: registry,
            mirror: spine_mirror,
            // The caller owns the guard: dropping it (or `dispose`) unwinds
            // the row resources. A composition that never hands the guard
            // to a live owner disposes here, not never.
            _guard: guard,
        })
    }

    /// Mount a composition and capture its spine as a reusable handle.
    ///
    /// The seed threads through to the spine row's mount (#67 §5).
    ///
    /// **Unwind contract.** A seeded spine's registry entry pins the
    /// plugin (and through it the seed's writer cell), so dropping the
    /// returned `SpineMount` alone never runs its `Drop` — the session
    /// mirror's flock would outlive the mount. Callers that compose a
    /// spine outside the `mount_seeded` wrapper (the seam tests) must
    /// hold the `Arc<SpineMount>` until the fallible step completes, or
    /// dispose explicitly. The wrapper is the tested production route.
    pub(crate) fn mount_spine_for(
        &self,
        doc: &ConfigDoc,
        wiring: ToolsWiring,
        seed: crate::boot::MountSeed,
    ) -> Result<SpineMount, CliError> {
        // The spine row's mount consumes the seed exactly once (#67 §5): the
        // mirroring writer — while the model/tool rows stay sessionless.
        // The plan's rows must carry a spine row for the seed to reach the
        // spine at all; a seed with no spine row is a named failure, never a
        // silently unseeded mount.
        if !seed.is_empty()
            && !doc
                .rows
                .iter()
                .any(|r| self.specs.seam(&r.plugin) == Seam::Spine)
        {
            return Err(CliError::new(
                "mount-failed",
                "a session-seeded mount requires a document with a spine row",
            ));
        }
        // The id floor is read before the mount consumes the seed.
        let id_seed = seed.id_seed;
        let CompositionMount {
            ctx,
            model,
            tools,
            store,
            mut mirror,
            _registry,
            _guard,
        } = self.mount_config_with_seed(doc, seed)?;
        // The spine is the only plugin this composition mounts through this
        // registry — model/tool rows mount through their own guards, never
        // here — so the registry's last fiber *is* the spine's. No spine
        // fiber means no spine row mounted, which the seam cannot boot.
        let fiber = match _registry.fibers().into_iter().last() {
            Some(fiber) => fiber,
            None => {
                // The composition failed after its rows mounted: the guard
                // is a bound local here, and dropping it without `dispose`
                // would leave the mounted rows' resources — and an Active
                // spine fiber on an orphan context — alive forever. A failed
                // boot unwinds, never leaks.
                let mut guard = _guard;
                guard.dispose();
                if let Some(mirror) = mirror.take() {
                    // The spine mounted and took the session lock; no
                    // composition ever owns it. A failed boot unwinds,
                    // never leaks.
                    mirror.close();
                }
                return Err(CliError::new(
                    "mount-failed",
                    "composition mounted no spine fiber",
                ));
            }
        };
        Ok(SpineMount {
            ctx,
            registry: _registry,
            mirror,
            fiber,
            model,
            tools,
            wiring,
            ids: crate::boot::Ids::seeded(id_seed),
            store,
            guard: Arc::new(std::sync::Mutex::new(_guard)),
        })
    }

    /// The seam tests' window onto [`Self::mount_spine_for`]: an
    /// integration test drives the panic path through the `BootComposer`
    /// wrapper, so the crate keeps the direct-spine call crate-internal
    /// and the test reaches it only under the `test-cfg` feature. The
    /// return is the type-erased `Arc` — the seam only needs the handle to
    /// live until the panic, never the spine's surface.
    #[cfg(any(test, feature = "test-cfg"))]
    pub fn mount_spine_for_for_test(
        &self,
        doc: &ConfigDoc,
        wiring: ToolsWiring,
        seed: crate::boot::MountSeed,
    ) -> Result<Arc<dyn std::any::Any + Send + Sync>, CliError> {
        self.mount_spine_for(doc, wiring, seed)
            .map(|spine| Arc::new(spine) as Arc<dyn std::any::Any + Send + Sync>)
    }
}

/// The seam's window onto the panic path *through the production
/// wrapper*: the wrapper's `mount_seeded` entry arms the panic guard and
/// records the seed's cell; its body then dispatches to the installed
/// [`SEAM_HOOK`], which mounts the real seeded spine and panics at the
/// post-spine step with the composition pinned by the registry (the
/// shape the guard's dispose must reach). The panic unwinds past this
/// call; the test drives it under `catch_unwind`.
#[cfg(any(test, feature = "test-cfg"))]
pub fn mount_seeded_panicking_after_spine_for_test(
    composer: &std::sync::Arc<ConfigComposer>,
    doc: &crate::profile::ProfileDoc,
    seed: crate::boot::MountSeed,
    wiring: ToolsWiring,
) -> Result<crate::boot::Mounted, crate::boot::MountFailure> {
    let inner = composer.clone();
    let cell = seed.writer.clone();
    let hook = move |doc: &crate::profile::ProfileDoc, seed: crate::boot::MountSeed| {
        let config_doc = inner
            .compose_config(&doc.name, &[])
            .expect("the plan re-composes");
        let spine = std::sync::Arc::new(
            inner
                .mount_spine_for(&config_doc, wiring.clone(), seed)
                .expect("the seeded spine mounts"),
        );
        // Leak the seam's spine handle: the guard's Drop must observe the
        // post-mount state through the slot's weak handle, exactly as the
        // production body's window does (there the composition is owned by
        // the `Mounted` that never gets built).
        let _spine = std::mem::ManuallyDrop::new(spine);
        // The seam stands in for the wrapper body's *post-mount* window:
        // record the spine and re-arm the cell exactly as the real body
        // does, so the guard's Drop sees the production post-spine state.
        LAST_MOUNTED_SPINE
            .with(|s| *s.lock().expect("probe lock") = std::sync::Arc::downgrade(&_spine));
        SPINE_UNWIND.with(|g| {
            let mut slot = g.lock().expect("unwind lock");
            slot.spine = std::sync::Arc::downgrade(&_spine);
            slot.cell = Some(cell.clone());
        });
        panic!("the model step panicked after the spine mounted");
    };
    let prev = SEAM_HOOK.with(|h| {
        h.lock()
            .expect("seam hook")
            .replace(std::sync::Arc::new(hook) as SeamHook)
            .map(|_| ())
    });
    assert!(prev.is_none(), "one seam per thread");
    // Restore the hook on every exit — including the panic unwind, which
    // runs this guard's Drop.
    let _restore = SeamHookGuard;
    // The production wrapper's entry arms the panic guard and records the
    // seed's cell *before* its body runs, and the body dispatches back
    // through the seam — so the panic unwinds past the armed guard, the
    // exact window the panic-path unwind is tested against.
    composer.mount_seeded(doc, seed)
}

/// Restores the thread's seam hook on unwind (the seam's panic path).
struct SeamHookGuard;

impl Drop for SeamHookGuard {
    fn drop(&mut self) {
        SEAM_HOOK.with(|h| *h.lock().expect("seam hook") = None);
    }
}

/// The thread's installed panic seam (test-cfg only): `None` outside a
/// seam test. The wrapper's body dispatches here when armed. The hook is
/// an `Arc` closure so the body can call it without taking ownership
/// (the helper's restore guard owns the teardown).
#[cfg(any(test, feature = "test-cfg"))]
pub(crate) type SeamHook =
    std::sync::Arc<dyn Fn(&crate::profile::ProfileDoc, crate::boot::MountSeed) + Send>;

#[cfg(any(test, feature = "test-cfg"))]
thread_local! {
    pub(crate) static SEAM_HOOK: std::sync::Mutex<Option<SeamHook>> =
        std::sync::Mutex::new(None);
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

/// Resolve a configuration document's store dir the way the rows' mount
/// resolves it (#69 §3): the `storage-jsonl` row's config through the
/// composer's substitution pass. `None` when the document has no store row.
fn resolve_store_dir(doc: &ConfigDoc, subst: &harnless_config::subst::Subst) -> Option<String> {
    doc.rows
        .iter()
        .find(|r| r.plugin == STORAGE_PLUGIN)
        .and_then(|row| {
            harnless_config::subst::expand(&row.config, subst)
                .ok()
                .and_then(|v| v.get("dir").and_then(|d| d.as_str()).map(String::from))
        })
}

impl BootComposer for ConfigComposer {
    fn profiles(&self) -> Vec<String> {
        self.composer.profiles()
    }

    fn compose(&self, name: &str, patch: Option<&str>) -> Result<ProfileDoc, CliError> {
        let overlays = parse_overlays(patch)?;
        let doc = self.compose_config(name, &overlays)?;
        self.plan(&doc)
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
        self.mount_seeded(doc, crate::boot::MountSeed::default())
            .map_err(|failure| failure.err)
    }

    fn mount_seeded(
        &self,
        doc: &ProfileDoc,
        seed: crate::boot::MountSeed,
    ) -> Result<Mounted, crate::boot::MountFailure> {
        // The trait's fallible-body hook is this composer's real
        // composition; the wrapper below owns the guard and the
        // classification.
        // The seed's writer cell is *shared* with the body: the wrapper
        // keeps its own `Arc` clone of the cell, so a failure before the
        // spine row mounts — when the body drops its seed copy — still
        // leaves the unconsumed writer in the cell for the classification
        // below (a mount that never reached its spine never took the
        // writer; see `MountFailure`).
        let created_by_mount = seed.created_by_mount;
        let cell = seed.writer.clone();
        // The panic-path unwind: the guard's Drop disposes the spine the
        // body recorded and classifies this seed's cell — a created file
        // abandons on the panic path exactly as on the Err path below.
        SPINE_UNWIND.with(|g| {
            let mut slot = g.lock().expect("unwind lock");
            slot.cell = Some(cell.clone());
            slot.created_by_mount = created_by_mount;
        });
        let _unwind = SpineUnwindGuard;
        match self.mount_seeded_body(doc, seed) {
            Ok(mounted) => {
                // clear the unwind slots so a *later* mount's failure can
                // never dispose this healthy spine.
                LAST_MOUNTED_SPINE.with(|s| {
                    *s.lock().expect("probe lock") = std::sync::Weak::new();
                });
                SpineUnwindGuard::disarm();
                Ok(mounted)
            }
            Err(err) => {
                // A failure *after* the spine mounted must tear the whole
                // composition down — every row resource in the spine's
                // mount guard and the spine fiber. Dropping the body's
                // `Arc<SpineMount>` is not enough: the spine's registry
                // entry pins the plugin (and through it the seed's writer
                // cell and the context's services), so the mount's drop
                // never runs. `SpineMount::unwind_after_failure` is the
                // full unwind (guard dispose + fiber unmount + mirror
                // close, idempotent). A pre-spine failure leaves the slot
                // full and no spine — the unwind is a no-op there.
                if let Some(spine) =
                    LAST_MOUNTED_SPINE.with(|s| s.lock().expect("probe lock").upgrade())
                {
                    spine.unwind_after_failure();
                }
                LAST_MOUNTED_SPINE.with(|s| {
                    *s.lock().expect("probe lock") = std::sync::Weak::new();
                });
                // The classification's abandon (when this boot created the
                // file) runs *before* the failure rides out, while the
                // spine slot is already cleared — the created file never
                // unlinks under a live composition that names its id
                // (#67 §5). A carried-back writer means nothing was
                // abandoned yet: the route's `unwind_created` owns it.
                Err(crate::boot::MountFailure::carried(
                    err,
                    crate::boot::classify_failed_cell(&cell, created_by_mount),
                ))
            }
        }
    }
}

impl ConfigComposer {
    /// The body of [`BootComposer::mount_seeded`], spelled against plain
    /// `CliError`s; the trait impl wraps a failure with the seed's
    /// unconsumed writer (see `MountFailure`).
    fn mount_seeded_body(
        &self,
        doc: &ProfileDoc,
        seed: crate::boot::MountSeed,
    ) -> Result<Mounted, CliError> {
        // The test seam's panic path: when a seam test installs a hook,
        // the body dispatches to it (the seam mounts the real spine and
        // panics at the post-spine step) instead of composing. Production
        // never arms a hook; the dispatch is inert there.
        #[cfg(any(test, feature = "test-cfg"))]
        if let Some(hook) = SEAM_HOOK.with(|h| h.lock().expect("seam hook").clone()) {
            hook(doc, seed);
            unreachable!("the seam hook panics");
        }
        // The plan's wiring is computed from the plan alone — the same
        // projection `plan()` publishes — before any lock is taken.
        let wiring = self.tools_wiring(doc)?;
        // The boot half mounts the rows the dump printed *when the stored
        // composition is the one this plan projects* — identity is checked,
        // not assumed. The read-mount-write sequence runs under the mount
        // lock: two racing cold mounts cannot each mount a spine onto the
        // shared service map, and the cache's weak spine can never be
        // observed half-published. `parking_lot::Mutex` is not reentrant,
        // so the cold path composes through the *inner* fold directly —
        // never through `compose_full`, which takes the same lock.
        let _mount_guard = self.mount_lock.lock();
        let stored = self.last_config.lock().clone();
        // The plan's store dir, resolved from its own rows the way the
        // mount resolves them (#69 §3). The warm reuse compares against it:
        // a patch that moved the store must not reuse a spine whose storage
        // row names another dir — the divergence is loud, never a silent
        // wrong-dir mount.
        let plan_store_dir = doc.store.as_ref().and_then(|spec| {
            harnless_config::subst::expand(
                &serde_yaml::to_value(spec).expect("store spec is plain YAML"),
                self.composer.subst(),
            )
            .ok()
            .and_then(|v| v.get("dir").and_then(|d| d.as_str()).map(String::from))
        });
        let entry = match stored {
            Some(entry)
                if entry.name == doc.name && {
                    // The entry's rows are what mount; its resolved store
                    // dir is the truth the plan must agree with (#69 §3).
                    // A plan that names no store defers to the entry.
                    let entry_dir = entry
                        .store_dir
                        .clone()
                        .or_else(|| resolve_store_dir(&entry.doc, self.composer.subst()));
                    plan_store_dir.is_none() || entry_dir == plan_store_dir
                } =>
            {
                entry
            }
            _ => {
                let out = self.composer.compose(&doc.name, &[]).map_err(cli_error)?;
                *self.last_warnings.lock() = out.warnings.clone();
                Entry {
                    name: out.doc.name.clone(),
                    doc: out.doc.clone(),
                    spine: None,
                    store_dir: resolve_store_dir(&out.doc, self.composer.subst()),
                }
            }
        };
        // A session-seeded mount (#67 §5) is its *own* composition: a
        // resume's mirroring log and seeded ids cannot share a spine with a
        // fresh or other-session mount (one spine, one log). It therefore
        // bypasses the warm-spine cache entirely and never populates it.
        // The id allocator's scope is deliberately per-session (#68 §2): a
        // fresh session's ids legitimately start at 1 even when another
        // session file already holds 1..n — ids are message identity within
        // one log, and the store's `max + 1` floor keeps them distinct
        // *within* the session that continues here.
        let fresh = seed.is_empty();
        // A seeded mount is its own composition (#67 §5): its log mirrors
        // one session file and its ids floor at that session's max, so it
        // can never share the warm spine — and it never populates the cache.
        let spine = if !fresh {
            // The unwind slot's spine handle arms once the spine exists:
            // `unwind_after_failure` needs the `SpineMount`, and a panic
            // *inside* the composition is owned by the row loop's own
            // rollback (the plugin's WriterGuard/MirrorGuard release the
            // mirror through the registry-pinned plugin even on unwind).
            //
            // The composition returning Ok consumes the writer into the
            // mirror (or there was none): clear the slot's cell half so a
            // later panic never touches this mount's writer — the mirror
            // path owns a created file from here. A composition that
            // failed *inside* its own row loop keeps the cell armed: if
            // the spine row never consumed the writer it rode back with
            // the dropped seed, and only the guard's classification can
            // abandon a created file then. (A failure after the row
            // consumed the writer left the cell empty; the
            // classification is inert.)
            //
            // Same-thread contract: the slot is a `thread_local`, and the
            // wrapper's failure arm reads it back on the *calling*
            // thread. `mount_seeded` must therefore run start-to-finish
            // on one thread (true for the binary's route and every caller
            // in tree); a cross-thread entry would make the dispose a
            // silent no-op and strand the session lock.
            let spine = Arc::new(self.mount_spine_for(&entry.doc, wiring, seed)?);
            // The composition returned Ok: the spine row consumed the
            // writer into the mirror (or there was none). The shared
            // cell is empty now, so the guard's classification is inert
            // from here — the mirror (closed by the dispose or the
            // plugin's unwind guards) owns the writer.
            // The cell's writer moved into the mirror during the
            // composition; keep the *cell* (the same Arc the wrapper
            // holds) and let the guard's Drop read the consumed state
            // through it. Dropping the slot's handle here would leave
            // the guard blind to the mirror path's created file.

            // Record the freshly-composed seeded spine *now*, before any
            // later fallible step: the registry pins the plugin (and
            // through it the seed's writer cell), so the body's Arc drop
            // alone never runs `SpineMount::dispose` — a failure after
            // this point (the model step below) must find the spine in
            // the slot for the wrapper's full unwind. A warm-reused spine
            // is owned by live `Mounted` siblings and is never recorded:
            // disposing it would close a writer a sibling still needs.
            //
            // The wrapper's Err arm disposes first through
            // `LAST_MOUNTED_SPINE`; the guard's Drop re-runs the
            // idempotent dispose, so no exit leaks the lock.
            LAST_MOUNTED_SPINE
                .with(|s| *s.lock().expect("probe lock") = std::sync::Arc::downgrade(&spine));
            SPINE_UNWIND.with(|g| {
                g.lock().expect("unwind lock").spine = std::sync::Arc::downgrade(&spine)
            });
            spine
        } else {
            match entry.spine.as_ref().and_then(std::sync::Weak::upgrade) {
                Some(spine) => {
                    // A warm reuse must implement the *shape* of the plan it is
                    // handed: the spine's loop is either tool-less or
                    // registry-backed, and that axis is frozen at mount time.
                    // The registry-backed loop decides allow/deny by name
                    // membership in what it registered, so a plan whose
                    // `declared` list differs only in content or order is still
                    // served by the live spine — only the None/AutoAllow
                    // mismatch boots the wrong loop shape, and it fails loudly.
                    let same_shape = matches!(
                        (&spine.wiring, &wiring),
                        (ToolsWiring::None, ToolsWiring::None)
                            | (ToolsWiring::AutoAllow { .. }, ToolsWiring::AutoAllow { .. })
                    );
                    if !same_shape {
                        return Err(CliError::new(
                            "mount-failed",
                            "a spine with different tool wiring is already live for this profile; \
                             drop its last Mounted before mounting the other plan",
                        ));
                    }
                    // The invariant that keeps `Mounted::drop`'s mirror-close
                    // safe under warm reuse: a cached spine is always built
                    // from the default seed, so it never owns a mirroring
                    // writer. If a future change ever cached a store-mounted
                    // spine, the first `Mounted` to drop would close a writer
                    // a live sibling still needs — refuse it at boot instead.
                    if spine.mirror.is_some() {
                        return Err(CliError::new(
                            "mount-failed",
                            "a store-mounted spine is never warm-reusable; \
                             this is a bug in the spine cache",
                        ));
                    }
                    spine
                }
                None => {
                    let spine = Arc::new(self.mount_spine_for(
                        &entry.doc,
                        wiring,
                        crate::boot::MountSeed::default(),
                    )?);
                    // The composition's spine is now live; the entry keeps it
                    // weakly, so every later `mount` of the same plan reuses it
                    // while it is alive, and the last `Mounted` to drop disposes
                    // it for real.
                    *self.last_config.lock() = Some(Entry {
                        name: entry.name.clone(),
                        doc: entry.doc.clone(),
                        spine: Some(Arc::downgrade(&spine)),
                        store_dir: entry.store_dir.clone(),
                    });
                    spine
                }
            }
        };
        #[cfg(test)]
        crate::config_boot::TEST_SPINES
            .with(|s| *s.lock().expect("probe lock") = Some(std::sync::Arc::downgrade(&spine)));
        // A plan whose model differs from the rows' model (a field-wise
        // patch swapped it) mounts the plan's model, never the rows'.
        let model = if model_matches(doc, spine.model.as_ref()) {
            spine.model.clone()
        } else {
            build_adapter(doc)?
        };
        let ids = spine.ids.clone();
        let store = spine.store.clone();
        let mirror = spine.mirror.clone();
        Ok(Mounted {
            ctx: spine.ctx.clone(),
            _registry: Arc::clone(&spine.registry),
            tools: spine.tools.clone(),
            _spine: Some(spine as Arc<dyn std::any::Any + Send + Sync>),
            model,
            ids,
            store,
            mirror,
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
        // The shipped plan is the reference plan plus the store row (#69 §2):
        // the binary's default is durable; the reference fixture is not.
        // `${home}` is expanded by the fold's substitution pass, so the
        // golden asserts the row's *shape*, with the expansion checked here.
        let mut expected = ProfileDoc::default_profile();
        expected.seams.push(STORAGE_SEAM.to_string());
        expected.store = Some(StoreSpec {
            dir: doc
                .store
                .as_ref()
                .expect("the shipped plan names a store")
                .dir
                .clone(),
        });
        assert_eq!(doc, expected);
        assert!(doc
            .store
            .as_ref()
            .unwrap()
            .dir
            .ends_with("/.harnless/sessions"));
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
        // The doc rows never appear as seams and never mount. The plan's
        // own store row rides on top of the spine seams (#69 §2).
        let mut expected_seams = SPINE_SEAMS.to_vec();
        expected_seams.push(STORAGE_SEAM);
        assert_eq!(plan.seams, expected_seams);
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

    /// The seam's boot half over the *config* composer (the binary's
    /// composition root, not the reference mount the seam tests drive):
    /// mounting a composed plan reuses the composition's spine — one log,
    /// one id allocator across handles — and disposal happens when the
    /// last handle goes, not never.
    #[test]
    fn mounting_a_plan_reuses_the_composition_spine_and_disposes_last_handle() {
        use crate::boot::BootComposer as _;
        let composer = composer();
        let plan = composer.compose("default", None).unwrap();
        let first = composer.mount(&plan).expect("config mount boots");
        let second = composer.mount(&plan).expect("second mount reuses");
        // The probe is a *weak* handle on the spine: it observes the spine's
        // disposal without preventing it. The raw inner handles — the fiber
        // Arc and the guard Arc — outlive the spine struct and carry the
        // observable state the post-drop assertions read.
        let spine_probe = TEST_SPINES
            .with(|s| s.lock().expect("probe lock").clone())
            .expect("mount recorded its spine");
        let live = spine_probe
            .upgrade()
            .expect("spine alive while handles serve");
        let fiber_probe = Arc::clone(&live.fiber);
        let guard_probe = Arc::clone(&live.guard);
        // `live` is a temporary strong handle: release it before the
        // disposal assertions so `drop(first)` really is the last one.
        drop(live);
        let log1 = first
            .ctx
            .get::<harnless_agent::session::SessionLog>()
            .expect("spine provides the log");
        let id_a = first.ids.message();
        let id_b = second.ids.message();
        assert_ne!(id_a, id_b, "one allocator, monotonic ids");
        assert_eq!(
            fiber_probe.state(),
            harnless_runtime::fiber::FiberState::Active,
            "the live spine's fiber is Active"
        );
        assert!(
            !guard_probe.lock().expect("guard lock").is_empty(),
            "the guard holds the composition's row resources"
        );
        drop(second);
        assert!(
            first.ctx.get::<harnless_agent::AgentLoop>().is_some(),
            "dropping a sibling handle never disposes the live spine"
        );
        assert_eq!(log1.snapshot().records.len(), 0, "untouched log");
        // The last strong handle dropped: observe disposal directly — the
        // fiber reached Disposed (the spine's unwind ran) and the guard's
        // row resources are gone (guard.dispose ran).
        drop(first);
        assert_eq!(
            fiber_probe.state(),
            harnless_runtime::fiber::FiberState::Disposed,
            "the last handle's drop ran SpineMount::dispose"
        );
        assert!(
            guard_probe.lock().expect("guard lock").is_empty(),
            "the guard's row resources were disposed"
        );
        // A fresh mount composes a fresh spine.
        let third = composer.mount(&plan).expect("remount after disposal");
        assert!(third.ctx.get::<harnless_agent::AgentLoop>().is_some());
        assert_eq!(
            third
                .ctx
                .get::<harnless_agent::session::SessionLog>()
                .expect("fresh log")
                .snapshot()
                .records
                .len(),
            0
        );
    }

    /// A warm spine reuse must implement the plan's loop *shape*: a
    /// tool-declaring plan over a live tool-less spine fails loudly, and a
    /// plan that only reorders/extends the tool names is still served by
    /// the live registry-backed spine (name-membership allow, not a
    /// frozen list).
    #[test]
    fn warm_spine_reuse_rejects_only_the_wiring_shape_mismatch() {
        use crate::boot::BootComposer as _;
        let composer = composer();
        let plain = composer.compose("default", None).unwrap();
        let tool_plan = composer
            .compose("default", Some("tools: [echo]"))
            .expect("tools patch composes");
        assert_ne!(plain.tools, tool_plan.tools, "the patch declared tools");
        // A tool-less spine is live for "default".
        let live = composer.mount(&plain).expect("tool-less mount boots");
        // The same shape (tool-less) reuses it.
        let again = composer.mount(&plain).expect("same-shape reuse");
        drop(again);
        // A tool-declaring plan over the tool-less spine: wrong loop shape,
        // named failure — never a silently tool-less loop for a plan that
        let err = match composer.mount(&tool_plan) {
            Ok(_) => panic!("shape mismatch must fail the mount"),
            Err(err) => err,
        };
        assert_eq!(err.code, "mount-failed");
        // The live handle is untouched by the refused mount.
        assert!(live.ctx.get::<harnless_agent::AgentLoop>().is_some());
    }

    /// The positive half of the shape rule: over a live registry-backed
    /// spine, a plan whose declared tool list differs only in content or
    /// order is still served (name-membership allow), so the mount reuses
    /// the spine instead of failing.
    #[test]
    fn warm_spine_reuse_accepts_a_differing_tool_list() {
        use crate::boot::BootComposer as _;
        let composer = composer();
        let echo_plan = composer
            .compose("default", Some("tools: [echo]"))
            .expect("tools patch composes");
        // Same *set*, different declared *list*: the built-in registry only
        // has `echo`, so the second plan re-states the name in a shape the
        // frozen-list comparison would have rejected.
        let reordered_plan = composer
            .compose("default", Some("tools: [echo, echo]"))
            .expect("reordered tools patch composes");
        assert_ne!(
            echo_plan.tools, reordered_plan.tools,
            "the plans' declared lists differ"
        );
        // A registry-backed spine is live for "default" with `echo`.
        let live = composer.mount(&echo_plan).expect("tool mount boots");
        assert!(live.tools.is_some(), "the spine wired a tool registry");
        // The re-stated plan over the same-shape spine: served by name
        // membership, so the mount reuses the live spine.
        let restated = composer
            .mount(&reordered_plan)
            .expect("same-shape reuse serves a differing tool list");
        assert!(Arc::ptr_eq(
            live.tools.as_ref().expect("live registry"),
            restated.tools.as_ref().expect("reused registry")
        ));
    }

    /// `model_matches`' golden clause: a plan that keeps the provider but
    /// swaps `model.script` must NOT match the rows' mounted adapter —
    /// the mount swaps in the plan's own golden, never silently keeps the
    /// rows'. A plan naming no script matches the built-in demo corpus
    /// (script_id ""), and a different provider never matches.
    #[test]
    fn model_matches_compares_provider_and_golden() {
        let rows = build_adapter(&ProfileDoc::default_profile())
            .unwrap()
            .expect("default composes an adapter");
        // Same provider, no script named: matches the demo corpus.
        assert!(model_matches(&ProfileDoc::default_profile(), Some(&rows)));
        // Same provider, a golden swapped in: must not match.
        let mut swapped = ProfileDoc::default_profile();
        swapped.model = ModelSpec::Replay {
            provider: "openai".into(),
            script: Some("/golden/other.json".into()),
        };
        assert!(
            !model_matches(&swapped, Some(&rows)),
            "a script-swapped plan never matches the rows' adapter"
        );
        // A different provider never matches, script or not.
        let mut other_provider = ProfileDoc::default_profile();
        other_provider.model = ModelSpec::Replay {
            provider: "otherprov".into(),
            script: None,
        };
        assert!(!model_matches(&other_provider, Some(&rows)));
        // No plan model and no mounted adapter match; a plan model over no
        // adapter never does.
        let mut none = ProfileDoc::default_profile();
        none.model = ModelSpec::None;
        assert!(model_matches(&none, None));
        assert!(!model_matches(&ProfileDoc::default_profile(), None));
    }
}
