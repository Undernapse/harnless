//! Boot composition: the seam between the CLI and the (not-yet-landed)
//! config-boot crate.
//!
//! The contract is deliberately thin: a [`BootComposer`] turns a profile
//! name into a [`ProfileDoc`] and mounts that document onto a live runtime
//! [`Context`]. The dump and the mount read the *same* document —
//! `--dump-config` prints exactly what `mount` would compose — so the
//! dump-equals-mount property holds structurally, before real composition
//! lands.
//!
//! `DefaultComposer` is the only composer today: it knows the built-in
//! profiles and mounts what is constructible on main (the agent spine plus a
//! network-free replay model). When `harnless-config` lands, the parent
//! replaces the composer at the single wire point below; nothing else in the
//! CLI changes.

use std::sync::Arc;

use harnless_agent::spine::Spine;
use harnless_runtime::context::Context;
use harnless_runtime::plugin::Registry;
use harnless_seams::SessionId;

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

    /// Mount a composed document onto a fresh context, returning the live
    /// composition.
    fn mount(&self, doc: &ProfileDoc) -> Result<Mounted, CliError>;
}

/// A live composition: the mounted context plus the model handle the runner
/// drives turns through.
pub struct Mounted {
    /// The service context with the profile's seams mounted.
    pub ctx: Context,
    /// Keeps the plugin registry (and thus mounted fibers) alive.
    pub _registry: Registry,
    /// The composed model provider, if the profile names one.
    pub model: Option<ModelHandle>,
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

    fn mount(&self, doc: &ProfileDoc) -> Result<Mounted, CliError> {
        // PARENT-WIRE: replace DefaultComposer with the harnless-config boot
        // (compose/dump from ConfigTree) once that crate lands; this mount
        // body then becomes the config-driven plugin loader.
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
