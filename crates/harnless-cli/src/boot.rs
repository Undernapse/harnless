//! Boot composition: the seam between the CLI and the config-boot crate.
//!
//! The contract is deliberately thin: a [`BootComposer`] turns a profile
//! name into a [`ProfileDoc`] and mounts that document onto a live runtime
//! [`Context`]. The dump and the mount read the *same* document —
//! `--dump-config` prints exactly what `mount` would compose — so the
//! dump-equals-mount property is structural rather than aspirational.
//!
//! [`crate::config_boot::ConfigComposer`] is the wired implementation: it
//! composes through `harnless-config`'s layered fold (bundles → profile patch
//! → home patch → per-run overlays), expands `${env:}` / `${home}`
//! expressions, and mounts rows through a plugin registry that disposes
//! partial state on failure. [`DefaultComposer`] remains as the minimal
//! reference composer — the shape a composer must satisfy, and the fixture
//! the seam's own tests pin.

use std::sync::Arc;

use harnless_agent::spine::Spine;
use harnless_runtime::context::Context;
use harnless_runtime::plugin::Registry;
use harnless_seams::SessionId;
use std::sync::atomic::{AtomicU64, Ordering};

use harnless_agent::events::CommittedRecord;
use crate::model::{build_adapter, ModelHandle};
use crate::profile::ProfileDoc;
use crate::CliError;

/// The boot seam: compose a profile document, dump it, and mount it.
///
/// One trait so the CLI never depends on a config crate: the future
/// `harnless-config` boot will implement this same shape, and the parent
/// swaps the implementation at the binary's composition root.
pub trait BootComposer: Send + Sync + 'static {
    /// Names of profiles this composer knows, in display order.
    fn profiles(&self) -> Vec<String>;

    /// Compose the profile named `name`, or a named `unknown-profile` error.
    ///
    /// `patch` is an optional profile-patch document (YAML) merged over the
    /// composed profile before it is dumped or mounted.
    fn compose(&self, name: &str, patch: Option<&str>) -> Result<ProfileDoc, CliError>;

    /// Serialize a composed document through the boot serializer.
    ///
    /// The output reparses to an equal document (dump-equals-mount).
    fn dump(&self, doc: &ProfileDoc) -> String;

    /// The tool wiring a profile's declared tools require at boot.
    ///
    /// A profile with no tools needs nothing — the spine's tool-less loop is
    /// today's composition. A profile that declares tools needs the loop
    /// rebuilt with the mounted registry (`AgentLoop::with_tools`) plus an
    /// auto-allow pre-execute listener, because the pipeline is fail-closed
    /// and the CLI exposes no approval surface yet. The service `mount`
    /// installs is the one this hook returns, so the loop every consumer
    /// gets from the context is the wired one.
    ///
    /// The default is the tool-less shape. Implementations that own richer
    /// composition (the config boot) override it.
    fn tools_wiring(&self, doc: &ProfileDoc) -> Result<ToolsWiring, CliError> {
        let _ = doc;
        Ok(ToolsWiring::None)
    }

    /// Mount a composed document onto a fresh context, returning the live
    /// composition.
    fn mount(&self, doc: &ProfileDoc) -> Result<Mounted, CliError>;

    /// Mount a composed document with a session seed (#67 §5 / #68 §2).
    ///
    /// The resume/fork route: the log is seeded from the store's records,
    /// the id allocator floors at the store's max, and a writer mirrors
    /// every append. The default is [`mount`](Self::mount) — a composer
    /// that cannot seed says so through the seed's own refusal, never a
    /// silently unseeded mount: the seam's test harness overrides this.
    fn mount_seeded(&self, doc: &ProfileDoc, seed: MountSeed) -> Result<Mounted, CliError> {
        let _ = seed;
        self.mount(doc)
    }
}

/// What a profile's tool declarations require of the mounted loop.
///
/// The spine mounts a tool-less loop — a tool-call step answers with the
/// empty object, which is correct for a profile that never declared tools.
/// A profile that *does* declare tools gets the registry-backed loop: the
/// call is executed through the guarded pipeline and the frozen result is
/// what the log records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolsWiring {
    /// No tools declared; keep the spine's tool-less loop.
    None,
    /// Tools declared: rebuild the loop with the mounted registry, grant
    /// pre-execute allow to the registry's own tools (the CLI has no approval
    /// surface; the pipeline's fail-closed default stays the agent crate's
    /// tested interior), and register the built-ins named by `declared`.
    AutoAllow { declared: Vec<String> },
}

/// A live composition: the mounted context plus the model handle the runner
/// drives turns through.
pub struct Mounted {
    /// The service context with the profile's seams mounted.
    pub ctx: Context,
    /// Keeps the plugin registry (and thus mounted fibers) alive.
    pub _registry: Arc<Registry>,
    /// The composition's disposal owner. A config-boot mount hands the live
    /// spine here, so dropping (or `shutdown`) the `Mounted` unwinds the row
    /// resources and the spine's fiber; the reference mount's spine is owned
    /// by its registry and needs no separate handle.
    pub _spine: Option<Arc<dyn std::any::Any + Send + Sync>>,
    /// The composed model provider, if the profile names one.
    pub model: Option<ModelHandle>,
    /// The composition's id allocator: every `MessageId` and adapter-request
    /// `CallId` this session mints comes off it, so ids are unique across
    /// turns. A fresh composition starts a fresh counter; a session never
    /// reuses an id.
    pub ids: Ids,
    /// The mounted tool registry, when the profile declares tools. The runner
    /// projects its schemas onto every adapter request and its presence gates
    /// the tool-call driver shape.
    pub tools: Option<Arc<harnless_agent::tools::ToolRegistry>>,
    /// The mounted session store, when the profile's plan names a `store`
    /// row (#69 §3). Concrete, like `tools` — no seam trait for one
    /// provider. `None` is sessionless mode: resume/fork/list fail
    /// `storage-not-mounted`, observable at the field, never inferred.
    pub store: Option<Arc<harnless_storage_jsonl::SessionStore>>,
}

impl Mounted {
    /// Tear the composition down before this handle is dropped: dispose the
    /// disposal owner (row resources and the spine's fiber unwind, in
    /// reverse mount order). Idempotent; a composition whose spine is owned
    /// by the plain registry has nothing extra to unwind here.
    pub fn shutdown(&mut self) {
        self._spine = None;
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        // Releasing the last strong handle on the spine disposes the
        // composition: `SpineMount::Drop` runs the guard's row-resource
        // disposal and the spine fiber's unwind. A registry-owned spine
        // (`_spine: None`) has nothing extra to unwind here.
        self._spine = None;
    }
}

/// The per-composition id allocator.
///
/// Positions and times are the log's; *identity* is the caller's — the loop
/// never mints or validates ids. Sharing one counter across user messages,
/// assistant messages, and adapter requests is what keeps a multi-turn
/// session's ids distinct, which is the documented id semantics (a message id
/// names a single authored message, never a stream segment) and what keeps
/// cited retirement unambiguous.
#[derive(Debug, Clone)]
pub struct Ids {
    next: Arc<AtomicU64>,
}

impl Ids {
    /// A fresh allocator: the first id minted is 1 (never the zero value).
    pub fn new() -> Self {
        Self::seeded(0)
    }

    /// An allocator whose first mint is `start + 1` (#68 §2).
    ///
    /// The resume/fork route seeds from the store's max id (`max_seed + 1`
    /// start), which matches `new()`'s "first id is 1" rule when the store
    /// is empty. Seeding is a mount input, never a post-mount mutation.
    pub fn seeded(start: u64) -> Self {
        Self {
            next: Arc::new(AtomicU64::new(start + 1)),
        }
    }

    /// The next message id.
    pub fn message(&self) -> harnless_seams::MessageId {
        harnless_seams::MessageId(self.next.fetch_add(1, Ordering::SeqCst))
    }

    /// The next adapter-request call id.
    pub fn call(&self) -> harnless_seams::CallId {
        harnless_seams::CallId(self.next.fetch_add(1, Ordering::SeqCst))
    }
}

/// The built-in composer: knows the shipped profiles and mounts what is
/// constructible today.
#[derive(Default)]
pub struct DefaultComposer;

impl BootComposer for DefaultComposer {
    fn profiles(&self) -> Vec<String> {
        vec!["default".to_string()]
    }

    fn compose(&self, name: &str, patch: Option<&str>) -> Result<ProfileDoc, CliError> {
        let mut doc = match name {
            "default" => ProfileDoc::default_profile(),
            other => {
                return Err(CliError::new(
                    "unknown-profile",
                    format!(
                        "no profile named {other:?}; available: {}",
                        self.profiles().join(", ")
                    ),
                ))
            }
        };
        if let Some(patch) = patch {
            merge_patch(&mut doc, patch)?;
        }
        Ok(doc)
    }

    fn dump(&self, doc: &ProfileDoc) -> String {
        doc.dump()
    }

    fn tools_wiring(&self, doc: &ProfileDoc) -> Result<ToolsWiring, CliError> {
        // The reference composer wires the echo tool for a profile that
        // declares tools — the minimal in-tree tool (#61). A profile with no
        // tools keeps today's tool-less loop.
        if doc.tools.is_empty() {
            Ok(ToolsWiring::None)
        } else {
            Ok(ToolsWiring::AutoAllow {
                declared: doc.tools.clone(),
            })
        }
    }

    fn mount(&self, doc: &ProfileDoc) -> Result<Mounted, CliError> {
        self.mount_seeded(doc, MountSeed::default())
    }

    fn mount_seeded(&self, doc: &ProfileDoc, seed: MountSeed) -> Result<Mounted, CliError> {
        // The binary's composition root wires ConfigComposer (see
        // `crate::config_boot`); this body is the minimal reference mount the
        // seam's tests pin — and with the default seed it is exactly the
        // sessionless, unseeded shape (#69 §2): the reference plan carries no
        // store, so nothing here touches the filesystem unless a caller
        // hands a seed (the durability seam's store-mounted harness does).
        let fresh = seed.records.is_none() && seed.id_seed == 0 && seed.is_empty();
        let ctx = Context::root();
        let registry = Arc::new(Registry::new());
        let wiring = self.tools_wiring(doc)?;
        let id_seed = seed.id_seed;
        let (tools, _fiber) = mount_spine(&ctx, &registry, wiring, seed)?;
        let model = build_adapter(doc)?;
        Ok(Mounted {
            ctx,
            _registry: registry,
            _spine: None,
            model,
            ids: if fresh { Ids::new() } else { Ids::seeded(id_seed) },
            tools,
            store: None,
        })
    }
}

/// The seed inputs a spine mount takes (#67 §5 / #68 §2): the resume/fork
/// route hands the mount the store's records, the max id, and the mirroring
/// writer; every other route mounts fresh. A plain data input, not a
/// post-mount mutation.
pub struct MountSeed {
    /// The session id the log is built for (the minted/resolved id).
    pub session: SessionId,
    /// The stored committed records to seed the log with, when resuming or
    /// forking. `None` mounts a fresh log.
    pub records: Option<Vec<CommittedRecord>>,
    /// The id allocator starts at `id_seed + 1` (store max; 0 when fresh).
    pub id_seed: u64,
    /// The session writer the log mirrors appends to, when the composition
    /// is store-mounted. The writer holds the session lock for the mount's
    /// life; the mirror owns it and releases it when the composition drops.
    /// `Mutex` because `Plugin::apply` runs on `&self` (the plugin must be
    /// `Sync`) and the writer is not `Clone` — the mount takes it once.
    pub writer: std::sync::Mutex<Option<harnless_storage_jsonl::SessionWriter>>,
}

impl Default for MountSeed {
    /// The fresh route: the stock spine's placeholder session id, no
    /// records, no seed, no writer.
    fn default() -> Self {
        Self {
            session: SessionId(1),
            records: None,
            id_seed: 0,
            writer: std::sync::Mutex::new(None),
        }
    }
}

impl MountSeed {
    /// Whether this seed is the fresh shape (nothing to seed).
    pub fn is_empty(&self) -> bool {
        self.records.is_none()
            && self.id_seed == 0
            && self.writer.lock().expect("seed lock").is_none()
    }

    /// Take the writer out of the seed (once). A second take yields `None`,
    /// which is the sessionless shape, never a silent loss: `apply` runs
    /// once per mount.
    fn take_writer(&self) -> Option<harnless_storage_jsonl::SessionWriter> {
        self.writer.lock().expect("mount seed lock").take()
    }
}

/// Mount the agent spine, wired per `wiring` and seeded per `seed`, and
/// return the mounted tool registry when one was wired.
///
/// This is the single place the CLI's boot composes the loop service. The
/// spine mounts the tool-less shape; a profile that declared tools gets the
/// spine's own services re-provided through `AgentLoop::with_tools`, so every
/// consumer that takes the loop from the context drives the registry-backed
/// loop, and the registry handle rides along on [`Mounted`] for the runner's
/// schema projection.
pub(crate) fn mount_spine(
    ctx: &Context,
    registry: &Arc<Registry>,
    wiring: ToolsWiring,
    seed: MountSeed,
) -> Result<
    (
        Option<Arc<harnless_agent::tools::ToolRegistry>>,
        Arc<harnless_runtime::fiber::Fiber>,
    ),
    CliError,
> {
    // A spine owns the `AgentLoop` service key. Mounting a second spine over
    // one context would leave two fibers' disposers racing over that key —
    // unsupported, and loud rather than racy.
    if ctx.has::<harnless_agent::loop_::AgentLoop>() {
        return Err(CliError::new(
            "mount-failed",
            "a spine is already mounted on this context",
        ));
    }
    // `Registry::mount` runs the plugin body on a *fresh* fiber of the
    // registry's own. If the caller's context already carries a fiber, this
    // is a re-entrant mount from inside another plugin's body, and the
    // wiring's effects would be booked onto that outer fiber — unwinding
    // with it while this composition stayed live. That is a silent effect
    // loss, so refuse it loudly at boot.
    if ctx.fiber().is_some() {
        return Err(CliError::new(
            "mount-failed",
            "mount_spine called on a context already under a fiber",
        ));
    }
    let fiber = registry
        .mount(
            ctx,
            Arc::new(SpineWired {
                wiring: wiring.clone(),
                seed,
            }),
        )
        .map_err(|e| CliError::new("mount-failed", format!("{}: {}", e.code, e.message)))?;
    let tools = match wiring {
        // The registry-backed loop is live: the handle rides on `Mounted` so
        // the runner projects its schemas onto every adapter request.
        ToolsWiring::AutoAllow { .. } => ctx.get::<harnless_agent::tools::ToolRegistry>(),
        // No tools declared: the spine's registry stays empty, no schemas are
        // projected, and the loop keeps today's tool-less shape.
        ToolsWiring::None => None,
    };
    Ok((tools, fiber))
}

/// The mirror half of a store-mounted log (#67 §3): the writer every append
/// of the mounted `SessionLog` mirrors to, *before* the commit. One append
/// path (the log's own `append`/`append_with` mirror), so no event kind can
/// be "missed by the filter". Seeded records are never re-mirrored — they
/// already are the file.
///
/// The append order is deliberate: durable file first, memory second. A
/// mirrored file can hold a record the process died before committing
/// (crash recovery's torn-tail story); the reverse order could lose an
/// acknowledged event, which is the worse failure. The mirror runs inside
/// `SessionLog::append_with`, under the log's lock, so position, mirrored
/// line, and commit can never interleave.
///
/// The writer holds the session lock for the composition's life; dropping
/// the mirror (the last handle of the mounted log) releases it.
pub(crate) struct MirroringLog {
    writer: std::sync::Mutex<harnless_storage_jsonl::SessionWriter>,
}

impl MirroringLog {
    /// A mirror that writes each committed record to `writer`.
    pub fn new(writer: harnless_storage_jsonl::SessionWriter) -> Self {
        Self {
            writer: std::sync::Mutex::new(writer),
        }
    }

    /// The mirror function installed on the mounted log. A mirror failure is
    /// loud — the append fails and the log stays unchanged, never a silently
    /// unpersisted event.
    pub fn mirror_fn(self: &Arc<Self>) -> Arc<harnless_agent::session::Mirror> {
        let this = Arc::clone(self);
        Arc::new(move |record| {
            let mut writer = this.writer.lock().expect("mirror writer lock");
            writer.append(record).map_err(|e| e.message)
        })
    }
}

/// The spine plugin as the CLI's boot mounts it: the stock [`Spine`]'s
/// service chain rebuilt from the mount seed (#67 §5), plus — for a profile
/// that declared tools — the built-in tools, the auto-allow listener, and
/// the registry-backed loop.
///
/// The log is the only service the seed changes: a resume/fork mount hands
/// it stored records (`SessionLog::seeded`), and a store-mounted composition
/// mirrors every append into the session file. The composition never learns
/// whether its log is seeded — every #64 mount route stays byte-identical.
///
/// The wiring runs inside the plugin's `apply`, on the spine's own fiber, so
/// every registration is owned by the same fiber as the spine's own and
/// unwinds with it. Mounting the wiring from outside `apply` would attach it
/// to the wrong fiber (or none).
struct SpineWired {
    wiring: ToolsWiring,
    /// The mount's seed inputs. The fresh route's defaults keep the mount
    /// exactly the #64-pinned shape.
    seed: MountSeed,
}

impl harnless_runtime::plugin::Plugin for SpineWired {
    fn name(&self) -> &str {
        "spine"
    }

    fn apply(&self, ctx: &Context) -> harnless_runtime::Result<()> {
        use harnless_agent::session::SessionLog;
        // `Registry::mount` sets the fresh fiber on the plugin context
        // before calling `apply`, so the fiber here is *this mount's* fiber.
        let fiber = ctx
            .fiber()
            .ok_or_else(|| harnless_runtime::RuntimeError::new("MOUNT", "no owning fiber"))?;
        // The spine's chain, built from the mount seed: the log is seeded
        // when records ride the mount, and a store-mounted composition
        // installs the mirror so every append lands in the session file.
        // The stock `Spine::apply` is this with a fresh, unmirrored log.
        let log = match &self.seed.records {
            Some(records) => SessionLog::seeded(self.seed.session, records.clone()).map_err(
                |_| harnless_runtime::RuntimeError::new("MOUNT", "session seed is not contiguous"),
            )?,
            None => SessionLog::new(self.seed.session),
        };
        // The store-mounted route installs the mirror on the log *before*
        // the spine provides it, so the service the context hands out is the
        // mirroring log and every append path — the loop's, the runner's —
        // crosses the mirror. `SessionWriter` is not `Clone`; the mount
        // hands ownership over by taking it out of the seed. The seed is
        // built per mount and consumed here exactly once, so the interior
        // mutability is never observed.
        let writer = self.seed.take_writer();
        let log = match writer {
            Some(writer) => log.with_mirror(Arc::new(MirroringLog::new(writer)).mirror_fn()),
            None => log,
        };
        Spine::new(self.seed.session).apply_with_log(ctx, log)?;
        let ToolsWiring::AutoAllow { declared } = &self.wiring else {
            return Ok(());
        };
        let tools = ctx
            .get::<harnless_agent::tools::ToolRegistry>()
            .ok_or_else(|| harnless_runtime::RuntimeError::new("MOUNT", "tool pipeline missing"))?;
        use harnless_seams::Tools as _;
        let registered: Vec<String> = {
            let all = register_builtins(&tools, declared)?;
            tools
                .names()
                .into_iter()
                .filter(|n| all.contains(n))
                .collect()
        };
        // Auto-allow, scoped: the pipeline is fail-closed, and the CLI grants
        // allowance only to the tools it actually registered from the plan's
        // declarations. A later row that registers a body under some other
        // name still hits the fail-closed default, not a blanket grant.
        // Approval UX and policy modes stay out of scope (#61); the denial
        // path stays the agent crate's tested interior.
        let allowed = registered;
        tools.on_pre_execute(
            move |e: &mut harnless_agent::tools::PreExecute,
                  _next: &mut harnless_runtime::events::Next<
                '_,
                harnless_agent::tools::PreExecute,
                harnless_seams::PreDecision,
            >| {
                if allowed.iter().any(|name| *name == e.0) {
                    harnless_seams::PreDecision::Allow
                } else {
                    harnless_seams::PreDecision::Deny(
                        "tool is not registered by the CLI boot".to_string(),
                    )
                }
            },
        )?;
        let loop_ = harnless_agent::loop_::AgentLoop::with_tools(
            ctx.get::<harnless_agent::session::SessionLog>()
                .expect("spine provides the log"),
            ctx.get::<harnless_runtime::events::EventRegistry>()
                .expect("spine provides the event registry")
                .as_ref()
                .clone(),
            fiber.clone(),
            tools.clone(),
        );
        // Re-provide the loop under its service key so `ctx.get::<AgentLoop>()`
        // hands back the wired loop. The spine's tool-less provide is owned by
        // this same fiber and unwinds LIFO with it.
        //
        // The remove-then-provide is deliberate, not a stale-disposer race:
        // both registrations live on one fiber, so at teardown the wired
        // loop's disposer runs first (LIFO) and the spine's older disposer
        // then runs its unconditional remove on an already-absent key — a
        // no-op. A *second* spine mount over one context is unsupported:
        // two fibers would race over the one `AgentLoop` key.
        ctx.remove::<harnless_agent::loop_::AgentLoop>();
        ctx.provide_shared(&fiber, Arc::new(loop_))?;
        Ok(())
    }
}

/// The CLI's minimal in-tree tool (#61): echoes its arguments back.
///
/// Deliberately trivial — its job is to prove the composed loop executes a
/// model-requested call through the guarded pipeline and logs the frozen
/// result, not to be a useful tool. The exec-world tools (bash, sandbox) stay
/// out of this map's scope.
pub struct EchoTool;

impl harnless_seams::ToolBody for EchoTool {
    fn run(
        &self,
        _call_id: harnless_seams::CallId,
        args: &[u8],
    ) -> harnless_seams::Result<serde_json::Value> {
        let value: serde_json::Value = serde_json::from_slice(args).map_err(|e| {
            harnless_seams::SeamError::new(
                harnless_seams::ErrorCode::ToolDenied,
                format!("echo arguments are not valid JSON: {e}"),
            )
        })?;
        Ok(value)
    }
}

/// Register the CLI's built-in tools on a freshly wired registry.
///
/// The registry starts empty at boot; the profile's declared tool names pick
/// which built-ins ride. Today the only built-in is [`EchoTool`]. A declared
/// name with no built-in body is a named mount failure — the plan advertised
/// a tool the composition cannot run, and a silently unregistered tool (no
/// schema on the wire, no guard) is exactly the quiet divergence this seam
/// exists to prevent. Returns the names actually registered.
pub(crate) fn register_builtins(
    tools: &harnless_agent::tools::ToolRegistry,
    declared: &[String],
) -> harnless_runtime::Result<Vec<String>> {
    use harnless_seams::Tools as _;
    let mut registered = Vec::new();
    for name in declared {
        match name.as_str() {
            "echo" => {
                tools
                    .register(
                        harnless_seams::ToolDefinition {
                            name: "echo".to_string(),
                            description: "Echo the arguments back as the tool result.".to_string(),
                            schema: serde_json::json!({"type": "object"}),
                            serialized: false,
                        },
                        Arc::new(EchoTool),
                    )
                    .map_err(|_| {
                        harnless_runtime::RuntimeError::new("MOUNT", "echo registration failed")
                    })?;
                registered.push("echo".to_string());
            }
            other => {
                // `RuntimeError` carries a static message; the declared name
                // is in the mount-failed text the CLI maps onto its error.
                let _ = other;
                return Err(harnless_runtime::RuntimeError::new(
                    "MOUNT",
                    "profile declares a tool this build has no body for (built-ins: echo)",
                ));
            }
        }
    }
    Ok(registered)
}

/// Merge a YAML patch document over a composed profile.
///
/// The patch is a partial profile: any subset of the document's fields,
/// applied field-wise. Unknown fields are rejected so a typo'd patch fails
/// loudly instead of silently mounting the unpatched profile.
fn merge_patch(doc: &mut ProfileDoc, patch: &str) -> Result<(), CliError> {
    let value: serde_yaml::Value = serde_yaml::from_str(patch)
        .map_err(|e| CliError::new("bad-patch", format!("patch is not valid YAML: {e}")))?;
    let mapping = value
        .as_mapping()
        .ok_or_else(|| CliError::new("bad-patch", "patch must be a YAML mapping"))?;
    let mut merged = serde_yaml::to_value(&*doc).expect("document is plain YAML data");
    let base = merged
        .as_mapping_mut()
        .expect("document serializes to a mapping");
    for (key, val) in mapping {
        let key_str = key
            .as_str()
            .ok_or_else(|| CliError::new("bad-patch", "patch keys must be strings"))?;
        match key_str {
            "name" | "seams" | "model" | "tools" | "system_prompt" => {}
            other => {
                return Err(CliError::new(
                    "bad-patch",
                    format!("unknown profile field {other:?}"),
                ))
            }
        }
        base.insert(key.clone(), val.clone());
    }
    *doc = serde_yaml::from_value(merged)
        .map_err(|e| CliError::new("bad-patch", format!("patch does not compose: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_profile_is_a_named_error() {
        let err = DefaultComposer.compose("nope", None).unwrap_err();
        assert_eq!(err.code, "unknown-profile");
        assert!(err.message.contains("default"));
    }

    #[test]
    fn patch_overrides_model_and_prompt() {
        let patch = "system_prompt: be terse\nmodel:\n  kind: none\n";
        let doc = DefaultComposer.compose("default", Some(patch)).unwrap();
        assert_eq!(doc.system_prompt, "be terse");
        assert!(!doc.has_model());
    }

    #[test]
    fn bad_patch_field_is_a_named_error() {
        let err = DefaultComposer
            .compose("default", Some("nope: 1\n"))
            .unwrap_err();
        assert_eq!(err.code, "bad-patch");
    }

    #[test]
    fn mount_composes_a_model_handle() {
        let doc = DefaultComposer.compose("default", None).unwrap();
        let mounted = DefaultComposer.mount(&doc).expect("mount");
        assert!(mounted.ctx.get::<harnless_agent::AgentLoop>().is_some());
        assert!(mounted.model.is_some());
    }

    #[test]
    fn mount_of_a_none_model_succeeds_without_a_provider() {
        let doc = ProfileDoc {
            model: crate::profile::ModelSpec::None,
            ..ProfileDoc::default_profile()
        };
        let mounted = DefaultComposer.mount(&doc).expect("mount");
        assert!(mounted.model.is_none());
    }
}
