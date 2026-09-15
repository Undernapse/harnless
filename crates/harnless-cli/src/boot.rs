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
        Self {
            next: Arc::new(AtomicU64::new(1)),
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
        // The binary's composition root wires ConfigComposer (see
        // `crate::config_boot`); this body is the minimal reference mount the
        // seam's tests pin.
        let ctx = Context::root();
        let registry = Arc::new(Registry::new());
        let wiring = self.tools_wiring(doc)?;
        let (tools, _fiber) = mount_spine(&ctx, &registry, wiring)?;
        let model = build_adapter(doc)?;
        Ok(Mounted {
            ctx,
            _registry: registry,
            model,
            ids: Ids::new(),
            tools,
        })
    }
}

/// Mount the agent spine, wired per `wiring`, and return the mounted tool
/// registry when one was wired.
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
) -> Result<
    (
        Option<Arc<harnless_agent::tools::ToolRegistry>>,
        Arc<harnless_runtime::fiber::Fiber>,
    ),
    CliError,
> {
    let fiber = registry
        .mount(
            ctx,
            Arc::new(SpineWired {
                wiring: wiring.clone(),
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

/// The spine plugin as the CLI's boot mounts it: the stock [`Spine`], plus —
/// for a profile that declared tools — the built-in tools, the auto-allow
/// listener, and the registry-backed loop.
///
/// The wiring runs inside the plugin's `apply`, on the spine's own fiber, so
/// every registration is owned by the same fiber as the spine's own and
/// unwinds with it. Mounting the wiring from outside `apply` would attach it
/// to the wrong fiber (or none).
struct SpineWired {
    wiring: ToolsWiring,
}

impl harnless_runtime::plugin::Plugin for SpineWired {
    fn name(&self) -> &str {
        "spine"
    }

    fn apply(&self, ctx: &Context) -> harnless_runtime::Result<()> {
        Spine::new(SessionId(1)).apply(ctx)?;
        let ToolsWiring::AutoAllow { declared } = &self.wiring else {
            return Ok(());
        };
        let tools = ctx
            .get::<harnless_agent::tools::ToolRegistry>()
            .ok_or_else(|| harnless_runtime::RuntimeError::new("MOUNT", "tool pipeline missing"))?;
        register_builtins(&tools, declared)?;
        // Auto-allow: the pipeline is fail-closed, and the CLI composes only
        // its own registered tools. Approval UX and policy modes stay out of
        // scope (#61); the denial path stays the agent crate's tested interior.
        tools.on_pre_execute(
            |_: &mut harnless_agent::tools::PreExecute,
             _next: &mut harnless_runtime::events::Next<
                '_,
                harnless_agent::tools::PreExecute,
                harnless_seams::PreDecision,
            >| harnless_seams::PreDecision::Allow,
        )?;
        let fiber = ctx.fiber().expect("spine must be mounted within a fiber");
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
/// which built-ins ride. Today the only built-in is [`EchoTool`]; a declared
/// name with no built-in body mounts as no-op (the name still reaches the
/// plan and the dump), which is the honest shape until exec-world tools land.
pub(crate) fn register_builtins(
    tools: &harnless_agent::tools::ToolRegistry,
    declared: &[String],
) -> harnless_runtime::Result<()> {
    use harnless_seams::Tools as _;
    if declared.iter().any(|name| name == "echo") {
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
    }
    Ok(())
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
